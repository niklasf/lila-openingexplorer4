use std::{io, path::PathBuf};

use clap::Parser;
use lila_openingexplorer4::parquet::{OptionalColumn, PositionalFile};
use parquet::{
    column::reader::get_typed_column_reader,
    data_type::{
        ByteArray, ByteArrayType, DataType, FixedLenByteArray, FixedLenByteArrayType, Int32Type,
    },
    file::reader::{FileReader, RowGroupReader, SerializedFileReader},
};
use shakmaty::ByColor;
use std::fs::File;

#[derive(Parser)]
struct Args {
    parquet: PathBuf,
}

#[derive(Default)]
struct PlayerColumns {
    title: OptionalColumn<ByteArrayType>,
    name: Vec<ByteArray>,
    rating: OptionalColumn<Int32Type>,
}

#[derive(Default)]
struct Columns {
    game_id: Vec<FixedLenByteArray>,
    players: ByColor<PlayerColumns>,
    date_year: OptionalColumn<Int32Type>,
    date_month: OptionalColumn<Int32Type>,
    date_day: OptionalColumn<Int32Type>,
    outcome: Vec<ByteArray>,
    speed: OptionalColumn<ByteArrayType>,
    rating_mode: Vec<ByteArray>,
    setup: OptionalColumn<ByteArrayType>,
    moves: Vec<ByteArray>,
}

impl Columns {
    fn fill_rows(
        &mut self,
        reader: &dyn RowGroupReader,
        num_rows: usize,
    ) -> parquet::errors::Result<()> {
        read_required::<FixedLenByteArrayType>(reader, 0, num_rows, &mut self.game_id)?;
        Ok(())
    }
}

fn read_required<T: DataType>(
    reader: &dyn RowGroupReader,
    column: usize,
    num_rows: usize,
    values: &mut Vec<T::T>,
) -> parquet::errors::Result<()> {
    values.clear();
    values.reserve(num_rows);

    let mut column_reader = get_typed_column_reader::<T>(reader.get_column_reader(column)?);

    let mut records = 0;
    while records < num_rows {
        let (read, _, _) = column_reader.read_records(num_rows - records, None, None, values)?;
        if read == 0 {
            break;
        }
        records += read;
    }

    Ok(())
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let mut rows = 0;

    //let file = PositionalFile::open(args.parquet)?;
    let file = File::open(args.parquet)?;

    let reader = SerializedFileReader::new(file)?;

    let num_row_groups = reader.num_row_groups();
    let mut columns = Columns::default();
    for i in 0..num_row_groups {
        let row_group_reader = reader.get_row_group(i)?;
        columns.fill_rows(
            &*row_group_reader,
            row_group_reader.metadata().num_rows() as usize,
        )?;

        rows += columns.game_id.len();
    }

    println!("rows = {}", rows);

    Ok(())
}
