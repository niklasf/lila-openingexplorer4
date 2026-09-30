use std::{
    fs::{self, File},
    io, mem,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
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
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// One pass over the input for each disk partition. All memory partitions of
/// a disk partition, except the resident ones, must fit into scratch space at
/// the same time.
const DISK_PARTITION_BITS: u32 = 3;

/// Each memory partition must fit into memory at once.
const MEMORY_PARTITION_BITS: u32 = 4;

const _: () = assert!(MEMORY_PARTITION_BITS >= DISK_PARTITION_BITS);

const MEMORY_PARTITIONS_PER_PASS: usize = 1 << (MEMORY_PARTITION_BITS - DISK_PARTITION_BITS);

/// Memory partitions per pass that are kept in memory while reading the
/// input, rather than being written to scratch space and read back. Each
/// resident partition takes about as much memory as processing a memory
/// partition.
const RESIDENT_PARTITIONS_PER_PASS: usize = 1;

const _: () = assert!(RESIDENT_PARTITIONS_PER_PASS <= MEMORY_PARTITIONS_PER_PASS);

/// Memory partitions per pass that are written to scratch space.
const SCRATCH_PARTITIONS_PER_PASS: usize =
    MEMORY_PARTITIONS_PER_PASS - RESIDENT_PARTITIONS_PER_PASS;

const ROW_GROUP_LEN: usize = 100_000;

#[derive(Parser)]
struct Args {
    parquet: PathBuf,
    /// Directory for temporary partition files.
    #[clap(long)]
    scratch: PathBuf,
    /// Threads for reading the input and loading scratch files. Defaults to
    /// half of available parallelism.
    #[clap(long)]
    read_threads: Option<usize>,
    /// Threads for writing scratch files. Defaults to half of available
    /// parallelism.
    #[clap(long)]
    write_threads: Option<usize>,
}

#[derive(Copy, Clone)]
struct Threads {
    read: usize,
    write: usize,
}

impl Threads {
    /// Each scratch partition is split into this many files, so that all
    /// write threads can be busy, even with few scratch partitions per pass.
    fn shards(self) -> usize {
        self.write
            .div_ceil(SCRATCH_PARTITIONS_PER_PASS.max(1))
            .max(1)
    }
}

/// Writes row groups to a file in scratch space.
struct ScratchWriter {
    path: PathBuf,
    writer: SerializedFileWriter<File>,
}

impl ScratchWriter {
    pub fn create(path: PathBuf) -> io::Result<ScratchWriter> {
        let file = File::create(&path)?;
        Ok(ScratchWriter {
            path,
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

    pub fn write(&mut self, columns: &GameColumns) {
        columns
            .write_row_group(&mut self.writer)
            .expect("write row group");
    }

    pub fn close(self) -> PathBuf {
        self.writer.close().expect("close partition file");
        self.path
    }
}

/// A scratch file, with a buffer collecting pieces into a full row group.
struct ScratchFile {
    buffer: Mutex<GameColumns>,
    writer: Mutex<ScratchWriter>,
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

    pub fn append(&mut self, other: MemoryPartition) {
        if self.index.is_empty() {
            *self = other;
            return;
        }
        let base = self.values.len() as u64;
        self.values.extend_from_slice(&other.values);
        self.index.extend(
            other
                .index
                .into_iter()
                .map(|(game_id, offset)| (game_id, base + offset)),
        );
    }

    /// Loads all row groups of the given scratch files, in parallel.
    pub fn load(paths: &[PathBuf], threads: usize) -> io::Result<MemoryPartition> {
        let readers = paths
            .iter()
            .map(|path| Ok::<_, io::Error>(SerializedFileReader::new(PositionalFile::open(path)?)?))
            .collect::<io::Result<Vec<_>>>()?;

        let jobs: Vec<(usize, usize)> = readers
            .iter()
            .enumerate()
            .flat_map(|(file, reader)| (0..reader.num_row_groups()).map(move |i| (file, i)))
            .collect();
        let next_job = AtomicUsize::new(0);

        let partition = Mutex::new(MemoryPartition::default());

        thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut local = MemoryPartition::default();
                    let mut columns = GameColumns::default();
                    while let Some(&(file, i)) = jobs.get(next_job.fetch_add(1, Ordering::Relaxed))
                    {
                        columns
                            .read_columns(&*readers[file].get_row_group(i).expect("row group"))
                            .expect("read columns");
                        local.push(&columns);
                    }
                    partition.lock().expect("lock partition").append(local);
                });
            }
        });

        Ok(partition.into_inner().expect("partition"))
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

fn main() -> io::Result<()> {
    let args = Args::parse();

    let parallelism = thread::available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .div_ceil(2);
    let threads = Threads {
        read: args.read_threads.unwrap_or(parallelism),
        write: args.write_threads.unwrap_or(parallelism),
    };

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
        let (resident, scratch) = scatter_pass(&reader, pass, &args.scratch, threads, &progress)?;

        // Process resident partitions first, to free their memory.
        for (memory_partition, partition) in resident {
            progress.set_message(format!("processing {memory_partition} (resident)"));
            process_memory_partition(partition)?;
        }

        for (memory_partition, paths) in scratch {
            progress.set_message(format!("loading {memory_partition}"));
            let partition = MemoryPartition::load(&paths, threads.read)?;
            for path in &paths {
                fs::remove_file(path)?;
            }

            progress.set_message(format!("processing {memory_partition}"));
            process_memory_partition(partition)?;
        }
    }

    progress.finish();
    Ok(())
}

type Resident = Vec<(usize, MemoryPartition)>;
type Scratch = Vec<(usize, Vec<PathBuf>)>;

/// Reads the entire input and distributes the games of the given disk
/// partition to its memory partitions. The last `RESIDENT_PARTITIONS_PER_PASS`
/// memory partitions are kept in memory, the others are written to sharded
/// files in scratch space.
fn scatter_pass(
    reader: &SerializedFileReader<PositionalFile>,
    pass: usize,
    scratch: &Path,
    threads: Threads,
    progress: &ProgressBar,
) -> io::Result<(Resident, Scratch)> {
    let first_memory_partition = pass * MEMORY_PARTITIONS_PER_PASS;
    let shards = threads.shards();

    // Scratch file for shard s of scratch partition i at index i * shards + s.
    let files = (0..SCRATCH_PARTITIONS_PER_PASS)
        .flat_map(|i| (0..shards).map(move |s| (first_memory_partition + i, s)))
        .map(|(memory_partition, shard)| {
            Ok::<_, io::Error>(ScratchFile {
                buffer: Mutex::new(GameColumns::default()),
                writer: Mutex::new(ScratchWriter::create(
                    scratch.join(format!("games_{memory_partition:05}_{shard:03}.parquet")),
                )?),
            })
        })
        .collect::<io::Result<Vec<_>>>()?;

    let resident: Vec<Mutex<MemoryPartition>> = (0..RESIDENT_PARTITIONS_PER_PASS)
        .map(|_| Mutex::new(MemoryPartition::default()))
        .collect();

    thread::scope(|s| {
        let files = &files;
        let resident = &resident;

        // Write threads take full row groups for any scratch file. Writes to
        // the same file are serialized by its lock.
        let (tx, rx) = channel::bounded::<(usize, GameColumns)>(threads.write);

        for _ in 0..threads.write {
            let rx = rx.clone();
            s.spawn(move || {
                for (file, batch) in rx {
                    files[file]
                        .writer
                        .lock()
                        .expect("lock writer")
                        .write(&batch);
                }
            });
        }

        drop(rx);

        // Read threads scatter each row group into pieces, one for each
        // memory partition of this pass. They pack pieces for resident
        // partitions, and collect pieces for scratch files into full row
        // groups.
        let read_threads: Vec<_> = (0..threads.read)
            .map(|thread_idx| {
                let tx = tx.clone();
                s.spawn(move || {
                    // Collect resident partitions locally, to avoid
                    // contention.
                    let mut local_resident: Vec<MemoryPartition> = (0
                        ..RESIDENT_PARTITIONS_PER_PASS)
                        .map(|_| MemoryPartition::default())
                        .collect();

                    for i in (thread_idx..reader.num_row_groups()).step_by(threads.read) {
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

                        for (j, mut piece) in pieces.into_iter().enumerate() {
                            if piece.len() == 0 {
                                continue;
                            }

                            if j >= SCRATCH_PARTITIONS_PER_PASS {
                                local_resident[j - SCRATCH_PARTITIONS_PER_PASS].push(&piece);
                            } else {
                                let file = j * shards + i % shards;
                                let batch = {
                                    let mut buffer =
                                        files[file].buffer.lock().expect("lock buffer");
                                    buffer.append(&mut piece);
                                    (buffer.len() >= ROW_GROUP_LEN).then(|| mem::take(&mut *buffer))
                                };
                                if let Some(batch) = batch {
                                    tx.send((file, batch)).expect("send batch");
                                }
                            }
                        }
                    }

                    for (partition, local) in resident.iter().zip(local_resident) {
                        partition.lock().expect("lock resident").append(local);
                    }
                })
            })
            .collect();

        for read_thread in read_threads {
            read_thread.join().expect("read thread");
        }

        // Write remaining partial row groups.
        for (file, scratch_file) in files.iter().enumerate() {
            let batch = mem::take(&mut *scratch_file.buffer.lock().expect("lock buffer"));
            if batch.len() > 0 {
                tx.send((file, batch)).expect("send batch");
            }
        }

        // Write threads finish once all batches are written.
        drop(tx);
    });

    let mut scratch_paths: Scratch = (0..SCRATCH_PARTITIONS_PER_PASS)
        .map(|i| (first_memory_partition + i, Vec::with_capacity(shards)))
        .collect();
    for (file, scratch_file) in files.into_iter().enumerate() {
        let path = scratch_file.writer.into_inner().expect("writer").close();
        scratch_paths[file / shards].1.push(path);
    }

    let resident = resident
        .into_iter()
        .enumerate()
        .map(|(i, partition)| {
            (
                first_memory_partition + SCRATCH_PARTITIONS_PER_PASS + i,
                partition.into_inner().expect("resident"),
            )
        })
        .collect();

    Ok((resident, scratch_paths))
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
