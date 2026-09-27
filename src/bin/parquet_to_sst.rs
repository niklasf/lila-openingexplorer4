use std::{io, path::PathBuf};

use clap::Parser;
use lila_openingexplorer4::parquet::{GameColumns, PositionalFile};
use parquet::file::reader::{FileReader, SerializedFileReader};

#[derive(Parser)]
struct Args {
    parquet: PathBuf,
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let mut rows = 0;

    let file = PositionalFile::open(args.parquet)?;
    //let file = File::open(args.parquet)?;

    let reader = SerializedFileReader::new(file)?;

    println!(
        "num rows = {}",
        reader.metadata().file_metadata().num_rows()
    );

    let num_row_groups = reader.num_row_groups();
    let mut columns = GameColumns::default();
    for i in 0..num_row_groups {
        let row_group_reader = reader.get_row_group(i)?;
        columns.read_columns(&*row_group_reader)?;

        rows += columns.len();
    }

    println!("seen rows = {}", rows);

    Ok(())
}
