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
    packed::GameId,
    parquet::{GameColumns, PositionalFile},
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

const DISK_PARTITION_BITS: u32 = 3;

const MEMORY_PARTITION_BITS: u32 = 6;

const _: () = assert!(MEMORY_PARTITION_BITS >= DISK_PARTITION_BITS);

const MEMORY_PARTITIONS_PER_PASS: usize = 1 << (MEMORY_PARTITION_BITS - DISK_PARTITION_BITS);

const ROW_GROUP_LEN: usize = 100_000;

#[derive(Parser)]
struct Args {
    parquet: PathBuf,
    /// Directory for temporary partition files.
    #[clap(long)]
    scratch: PathBuf,
}

struct MemoryPartitionWriter {
    path: PathBuf,
    buffer: GameColumns,
    writer: SerializedFileWriter<File>,
}

impl MemoryPartitionWriter {
    pub fn create(path: PathBuf) -> io::Result<MemoryPartitionWriter> {
        let file = File::create(&path)?;
        Ok(MemoryPartitionWriter {
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
        let paths = scatter_pass(&reader, pass, &args.scratch, &progress)?;

        for path in paths {
            progress.set_message(format!("processing {}", path.display()));
            process_memory_partition(&path)?;
            fs::remove_file(&path)?;
        }
    }

    progress.finish();
    Ok(())
}

fn scatter_pass(
    reader: &SerializedFileReader<PositionalFile>,
    pass: usize,
    scratch: &Path,
    progress: &ProgressBar,
) -> io::Result<Vec<PathBuf>> {
    let writers = (0..MEMORY_PARTITIONS_PER_PASS)
        .map(|i| {
            let memory_partition = pass * MEMORY_PARTITIONS_PER_PASS + i;
            MemoryPartitionWriter::create(
                scratch.join(format!("games_{memory_partition:05}.parquet")),
            )
        })
        .collect::<io::Result<Vec<_>>>()?;

    Ok(thread::scope(|s| {
        // One writer thread per memory partition, collecting pieces into
        // full row groups.
        let (txs, rxs): (Vec<_>, Vec<_>) = (0..MEMORY_PARTITIONS_PER_PASS)
            .map(|_| channel::bounded::<GameColumns>(CHANNEL_SIZE))
            .unzip();

        let writer_threads: Vec<_> = writers
            .into_iter()
            .zip(rxs)
            .map(|(mut writer, rx)| {
                s.spawn(move || {
                    for mut piece in rx {
                        writer.buffer.append(&mut piece);
                        if writer.buffer.len() >= ROW_GROUP_LEN {
                            writer.flush();
                        }
                    }
                    writer.close()
                })
            })
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

        // Only the reader threads hold senders now, so that writer threads
        // finish when all reader threads are done.
        drop(txs);

        writer_threads
            .into_iter()
            .map(|writer_thread| writer_thread.join().expect("writer thread"))
            .collect()
    }))
}

/// Index of the memory partition within the given pass, or `None` if the
/// game belongs to a different pass.
fn pass_memory_partition(game_id: GameId, pass: usize) -> Option<usize> {
    let memory_partition = game_id.partition(MEMORY_PARTITION_BITS);
    let disk_partition = memory_partition >> (MEMORY_PARTITION_BITS - DISK_PARTITION_BITS);
    (disk_partition == pass).then_some(memory_partition & (MEMORY_PARTITIONS_PER_PASS - 1))
}

fn process_memory_partition(_path: &Path) -> io::Result<()> {
    // TODO: Load entirely into memory, sort by game id, and write SST files.
    Ok(())
}
