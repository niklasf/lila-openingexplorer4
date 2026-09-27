use std::{io, path::PathBuf};

use clap::Parser;
use crossbeam::channel;
use lila_openingexplorer4::parquet::{GameColumns, PositionalFile};
use parquet::file::reader::{FileReader, SerializedFileReader};
use std::thread;

const CHANNEL_SIZE: usize = 128;

const READ_THREADS: usize = 4;

#[derive(Parser)]
struct Args {
    parquet: PathBuf,
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

        for _columns in rx {}

        Ok(())
    })
}
