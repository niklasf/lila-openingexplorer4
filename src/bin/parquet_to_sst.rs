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

const DISK_PARTITION_BITS: u32 = 3;

const ROW_GROUP_LEN: usize = 100_000;

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

    pub fn flush(&mut self) {
        if self.buffer.len() > 0 {
            self.buffer
                .write_row_group(&mut self.writer)
                .expect("write row group");
            self.buffer.clear();
        }
    }

    pub fn close(mut self) {
        self.flush();
        self.writer.close().expect("close partition file");
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

        drop(tx);

        let mut partitions: Vec<_> = (0..(1 << DISK_PARTITION_BITS))
            .map(DiskPartition::new)
            .collect();

        for columns in rx {
            columns.scatter(
                |game_id| game_id.partition(DISK_PARTITION_BITS),
                &mut partitions
                    .iter_mut()
                    .map(|p| &mut p.buffer)
                    .collect::<Vec<_>>(),
            );

            for partition in &mut partitions {
                if partition.buffer.len() >= ROW_GROUP_LEN {
                    partition.flush();
                }
            }
        }

        for partition in partitions {
            partition.close();
        }

        Ok(())
    })
}
