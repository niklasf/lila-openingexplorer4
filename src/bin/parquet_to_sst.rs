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
use lila_openingexplorer4::parquet::{GameColumns, PositionalFile};
use parquet::{
    basic::Compression,
    file::{
        reader::{FileReader, SerializedFileReader},
        writer::SerializedFileWriter,
    },
};

const READ_THREADS: usize = 4;

const CHANNEL_SIZE: usize = 2 * READ_THREADS;

const DISK_PARTITION_BITS: u32 = 3;

const MEMORY_PARTITION_BITS: u32 = 4;

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
            "{spinner} {prefix} {msg} {wide_bar} {human_pos}/{human_len} {per_sec:>14} {eta:>7}",
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
    let mut writers = (0..MEMORY_PARTITIONS_PER_PASS)
        .map(|i| {
            let memory_partition = pass * MEMORY_PARTITIONS_PER_PASS + i;
            MemoryPartitionWriter::create(
                scratch.join(format!("games_{memory_partition:05}.parquet")),
            )
        })
        .collect::<io::Result<Vec<_>>>()?;

    thread::scope(|s| {
        let (tx, rx) = channel::bounded(CHANNEL_SIZE);

        for thread_idx in 0..READ_THREADS {
            let tx = tx.clone();

            s.spawn(move || {
                for i in (thread_idx..reader.num_row_groups()).step_by(READ_THREADS) {
                    let row_group_reader = reader.get_row_group(i).expect("row group");
                    let mut columns = GameColumns::default();
                    columns
                        .read_columns(&*row_group_reader)
                        .expect("read columns");
                    tx.send(columns).expect("send columns");
                }
            });
        }

        drop(tx);

        for columns in rx {
            columns.scatter(
                |game_id| {
                    let memory_partition = game_id.partition(MEMORY_PARTITION_BITS);
                    let disk_partition =
                        memory_partition >> (MEMORY_PARTITION_BITS - DISK_PARTITION_BITS);
                    (disk_partition == pass)
                        .then_some(memory_partition & (MEMORY_PARTITIONS_PER_PASS - 1))
                },
                &mut writers
                    .iter_mut()
                    .map(|w| &mut w.buffer)
                    .collect::<Vec<_>>(),
            );

            progress.inc(columns.len() as u64);

            for writer in &mut writers {
                if writer.buffer.len() >= ROW_GROUP_LEN {
                    writer.flush();
                }
            }
        }
    });

    Ok(writers
        .into_iter()
        .map(MemoryPartitionWriter::close)
        .collect())
}

fn process_memory_partition(_path: &Path) -> io::Result<()> {
    // TODO: Load entirely into memory, sort by game id, and write SST files.
    Ok(())
}
