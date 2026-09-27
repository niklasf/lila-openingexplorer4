use std::{fs::File, io, path::PathBuf, sync::Arc, thread};

use clap::Parser;
use crossbeam::channel;
use lila_openingexplorer4::parquet::{GameColumns, PositionalFile};
use parquet::file::{
    reader::{FileReader, SerializedFileReader},
    writer::SerializedFileWriter,
};

const CHANNEL_SIZE: usize = 128;

const READ_THREADS: usize = 4;

const DISK_PARTITION_BITS: usize = 3;

#[derive(Parser)]
struct Args {
    parquet: PathBuf,
}

struct DiskPartition {
    buffer: GameColumns,
    writer: SerializedFileWriter<File>,
}

impl DiskPartition {
    pub fn new(disk_partition: usize) -> DiskPartition {
        DiskPartition {
            buffer: GameColumns::default(),
            writer: SerializedFileWriter::new(
                File::create(format!("games_{disk_partition}.parquet"))
                    .expect("create partition file"),
                GameColumns::schema(),
                Arc::new(GameColumns::properties()),
            )
            .expect("partition writer"),
        }
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let file = PositionalFile::open(args.parquet)?;

    let reader = SerializedFileReader::new(file)?;
    let reader = &reader;

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

        let partitions: Vec<_> = (0..(1 << DISK_PARTITION_BITS))
            .map(DiskPartition::new)
            .collect();

        for _columns in rx {
            todo!()
        }

        Ok(())
    })
}
