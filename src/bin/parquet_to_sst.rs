use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use clap::Parser;
use crossbeam::channel;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use lila_openingexplorer4::{
    packed::{GameId, pack_bytes, pack_int},
    parquet::{GameColumns, GameRow, PositionalFile},
};
use parquet::{
    basic::Compression,
    file::{
        reader::{FileReader, SerializedFileReader},
        writer::SerializedFileWriter,
    },
};

const READ_THREADS: usize = 8;

const CHANNEL_SIZE: usize = 2 * READ_THREADS;

/// One pass over the input for each disk partition. All memory partitions of
/// a disk partition, except the resident ones, must fit into scratch space at
/// the same time.
const DISK_PARTITION_BITS: u32 = 3;

/// Each memory partition must fit into memory at once.
const MEMORY_PARTITION_BITS: u32 = 6;

const _: () = assert!(MEMORY_PARTITION_BITS >= DISK_PARTITION_BITS);

const MEMORY_PARTITIONS_PER_PASS: usize = 1 << (MEMORY_PARTITION_BITS - DISK_PARTITION_BITS);

/// Memory partitions per pass that are kept in memory while reading the
/// input, rather than being written to scratch space and read back. Each
/// resident partition takes about as much memory as processing a memory
/// partition.
const RESIDENT_PARTITIONS_PER_PASS: usize = 1;

const _: () = assert!(RESIDENT_PARTITIONS_PER_PASS <= MEMORY_PARTITIONS_PER_PASS);

const ROW_GROUP_LEN: usize = 100_000;

#[derive(Parser)]
struct Args {
    parquet: PathBuf,
    /// Directory for temporary partition files.
    #[clap(long)]
    scratch: PathBuf,
}

/// Writes a memory partition to a file in scratch space.
struct ScratchWriter {
    path: PathBuf,
    buffer: GameColumns,
    writer: SerializedFileWriter<File>,
}

impl ScratchWriter {
    pub fn create(path: PathBuf) -> io::Result<ScratchWriter> {
        let file = File::create(&path)?;
        Ok(ScratchWriter {
            path,
            buffer: GameColumns::default(),
            writer: SerializedFileWriter::new(
                file,
                GameColumns::schema(),
                Arc::new(
                    GameColumns::properties()
                        .set_compression(Compression::LZ4)
                        .build(),
                ),
            )?,
        })
    }

    pub fn flush(&mut self) {
        if self.buffer.len() > 0 {
            self.buffer
                .write_row_group(&mut self.writer)
                .expect("write row group");
            self.buffer.clear();
        }
    }

    pub fn close(mut self) -> PathBuf {
        self.flush();
        self.writer.close().expect("close partition file");
        self.path
    }
}

/// A memory partition, loaded in a compact form ready to be sorted.
#[derive(Default)]
struct MemoryPartition {
    /// Length-prefixed packed values, back to back.
    values: Vec<u8>,
    /// Game ids and the offsets of their values.
    index: Vec<(GameId, u64)>,
}

impl MemoryPartition {
    pub fn push(&mut self, columns: &GameColumns) {
        self.index.reserve(columns.len());
        let mut value = Vec::new();
        for row in columns.rows() {
            self.index.push((row.game_id, self.values.len() as u64));
            value.clear();
            pack_value(&row, &mut value);
            pack_bytes(&mut self.values, &value);
        }
    }

    pub fn load(path: &Path) -> io::Result<MemoryPartition> {
        let reader = SerializedFileReader::new(PositionalFile::open(path)?)?;
        let mut partition = MemoryPartition::default();
        let mut columns = GameColumns::default();
        for i in 0..reader.num_row_groups() {
            columns.read_columns(&*reader.get_row_group(i)?)?;
            partition.push(&columns);
        }
        Ok(partition)
    }
}

/// Placeholder encoding, lossless with respect to the parquet columns.
///
/// TODO: Replace with the final SST value encoding.
fn pack_value(row: &GameRow<'_>, buf: &mut Vec<u8>) {
    for player in [&row.players.white, &row.players.black] {
        pack_optional_bytes(buf, player.title);
        pack_bytes(buf, player.name);
        pack_optional_int(buf, player.rating);
    }
    pack_optional_int(buf, row.date_year);
    pack_optional_int(buf, row.date_month);
    pack_optional_int(buf, row.date_day);
    pack_bytes(buf, row.outcome);
    pack_optional_bytes(buf, row.speed);
    pack_bytes(buf, row.rating_mode);
    pack_optional_bytes(buf, row.setup);
    pack_bytes(buf, row.moves);
}

fn pack_optional_bytes(buf: &mut Vec<u8>, bytes: Option<&[u8]>) {
    match bytes {
        Some(bytes) => {
            buf.push(1);
            pack_bytes(buf, bytes);
        }
        None => buf.push(0),
    }
}

fn pack_optional_int(buf: &mut Vec<u8>, n: Option<i32>) {
    match n {
        Some(n) => {
            buf.push(1);
            pack_int(buf, i64::from(n));
        }
        None => buf.push(0),
    }
}

/// Destination of the games of a memory partition during a pass.
enum Sink {
    Scratch(ScratchWriter),
    Resident(MemoryPartition),
}

enum Filled {
    Scratch(PathBuf),
    Resident(MemoryPartition),
}

impl Sink {
    fn fill(self, rx: channel::Receiver<GameColumns>) -> Filled {
        match self {
            Sink::Scratch(mut writer) => {
                // Collect pieces into full row groups.
                for mut piece in rx {
                    writer.buffer.append(&mut piece);
                    if writer.buffer.len() >= ROW_GROUP_LEN {
                        writer.flush();
                    }
                }
                Filled::Scratch(writer.close())
            }
            Sink::Resident(mut partition) => {
                for piece in rx {
                    partition.push(&piece);
                }
                Filled::Resident(partition)
            }
        }
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let reader = SerializedFileReader::new(PositionalFile::open(&args.parquet)?)?;
    let num_rows = reader.metadata().file_metadata().num_rows() as u64;

    let passes = 1 << DISK_PARTITION_BITS;

    let progress = ProgressBar::with_draw_target(
        Some(num_rows * passes as u64),
        ProgressDrawTarget::stdout_with_hz(4),
    )
    .with_style(
        ProgressStyle::with_template(
            "{spinner} {prefix} {msg} {wide_bar} {human_pos}/{human_len} {per_sec} {eta:>7}",
        )
        .expect("progress template"),
    );

    for pass in 0..passes {
        progress.set_prefix(format!("pass {}/{}", pass + 1, passes));

        progress.set_message("scattering");
        let filled = scatter_pass(&reader, pass, &args.scratch, &progress)?;

        // Process resident partitions first, to free their memory.
        let mut scratch_files = Vec::new();
        for (memory_partition, filled) in filled {
            match filled {
                Filled::Resident(partition) => {
                    progress.set_message(format!("processing {memory_partition} (resident)"));
                    process_memory_partition(partition)?;
                }
                Filled::Scratch(path) => scratch_files.push((memory_partition, path)),
            }
        }

        for (memory_partition, path) in scratch_files {
            progress.set_message(format!("loading {memory_partition}"));
            let partition = MemoryPartition::load(&path)?;
            fs::remove_file(&path)?;

            progress.set_message(format!("processing {memory_partition}"));
            process_memory_partition(partition)?;
        }
    }

    progress.finish();
    Ok(())
}

/// Reads the entire input and distributes the games of the given disk
/// partition to its memory partitions. The last `RESIDENT_PARTITIONS_PER_PASS`
/// memory partitions are kept in memory, the others are written to scratch
/// space.
fn scatter_pass(
    reader: &SerializedFileReader<PositionalFile>,
    pass: usize,
    scratch: &Path,
    progress: &ProgressBar,
) -> io::Result<Vec<(usize, Filled)>> {
    let first_memory_partition = pass * MEMORY_PARTITIONS_PER_PASS;

    let sinks = (0..MEMORY_PARTITIONS_PER_PASS)
        .map(|i| {
            let memory_partition = first_memory_partition + i;
            Ok::<_, io::Error>(
                if i >= MEMORY_PARTITIONS_PER_PASS - RESIDENT_PARTITIONS_PER_PASS {
                    Sink::Resident(MemoryPartition::default())
                } else {
                    Sink::Scratch(ScratchWriter::create(
                        scratch.join(format!("games_{memory_partition:05}.parquet")),
                    )?)
                },
            )
        })
        .collect::<io::Result<Vec<_>>>()?;

    let filled: Vec<Filled> = thread::scope(|s| {
        // One sink thread per memory partition.
        let (txs, rxs): (Vec<_>, Vec<_>) = (0..MEMORY_PARTITIONS_PER_PASS)
            .map(|_| channel::bounded::<GameColumns>(CHANNEL_SIZE))
            .unzip();

        let sink_threads: Vec<_> = sinks
            .into_iter()
            .zip(rxs)
            .map(|(sink, rx)| s.spawn(move || sink.fill(rx)))
            .collect();

        // Reader threads scatter each row group into pieces, one for each
        // memory partition of this pass.
        for thread_idx in 0..READ_THREADS {
            let txs = txs.clone();

            s.spawn(move || {
                for i in (thread_idx..reader.num_row_groups()).step_by(READ_THREADS) {
                    let row_group_reader = reader.get_row_group(i).expect("row group");
                    let mut columns = GameColumns::default();
                    columns
                        .read_columns(&*row_group_reader)
                        .expect("read columns");

                    let mut pieces: Vec<GameColumns> = (0..MEMORY_PARTITIONS_PER_PASS)
                        .map(|_| GameColumns::default())
                        .collect();
                    columns.scatter(
                        |game_id| pass_memory_partition(game_id, pass),
                        &mut pieces.iter_mut().collect::<Vec<_>>(),
                    );

                    progress.inc(columns.len() as u64);

                    for (tx, piece) in txs.iter().zip(pieces) {
                        if piece.len() > 0 {
                            tx.send(piece).expect("send piece");
                        }
                    }
                }
            });
        }

        // Only the reader threads hold senders now, so that sink threads
        // finish when all reader threads are done.
        drop(txs);

        sink_threads
            .into_iter()
            .map(|sink_thread| sink_thread.join().expect("sink thread"))
            .collect()
    });

    Ok(filled
        .into_iter()
        .enumerate()
        .map(|(i, filled)| (first_memory_partition + i, filled))
        .collect())
}

/// Index of the memory partition within the given pass, or `None` if the
/// game belongs to a different pass.
fn pass_memory_partition(game_id: GameId, pass: usize) -> Option<usize> {
    let memory_partition = game_id.partition(MEMORY_PARTITION_BITS);
    let disk_partition = memory_partition >> (MEMORY_PARTITION_BITS - DISK_PARTITION_BITS);
    (disk_partition == pass).then_some(memory_partition & (MEMORY_PARTITIONS_PER_PASS - 1))
}

fn process_memory_partition(mut partition: MemoryPartition) -> io::Result<()> {
    partition
        .index
        .sort_unstable_by_key(|&(game_id, _)| game_id);

    // TODO: Write values in sorted order into SST files.
    Ok(())
}
