use std::{io, path::PathBuf};

use clap::Parser;
use lila_openingexplorer4::parquet::{OptionalColumn, PositionalFile, RequiredColumn};
use parquet::{
    column::reader::get_typed_column_reader,
    data_type::{
        ByteArray, ByteArrayType, DataType, FixedLenByteArray, FixedLenByteArrayType, Int32Type,
    },
    file::reader::{FileReader, RowGroupReader, SerializedFileReader},
};
use shakmaty::ByColor;

#[derive(Parser)]
struct Args {
    parquet: PathBuf,
}

#[derive(Default)]
struct PlayerColumns {
    title: OptionalColumn<ByteArrayType>,
    name: RequiredColumn<ByteArrayType>,
    rating: OptionalColumn<Int32Type>,
}

#[derive(Default)]
struct GameColumns {
    game_id: RequiredColumn<FixedLenByteArrayType>,
    players: ByColor<PlayerColumns>,
    date_year: OptionalColumn<Int32Type>,
    date_month: OptionalColumn<Int32Type>,
    date_day: OptionalColumn<Int32Type>,
    outcome: RequiredColumn<ByteArrayType>,
    speed: OptionalColumn<ByteArrayType>,
    rating_mode: RequiredColumn<ByteArrayType>,
    setup: OptionalColumn<ByteArrayType>,
    moves: RequiredColumn<ByteArrayType>,
}

impl GameColumns {
    fn read_columns(
        &mut self,
        reader: &dyn RowGroupReader,
        num_rows: usize,
    ) -> parquet::errors::Result<()> {
        self.game_id.read_column(reader, 0, num_rows)?;
        self.players.white.title.read_column(reader, 1, num_rows)?;
        self.players.white.name.read_column(reader, 2, num_rows)?;
        self.players.white.rating.read_column(reader, 3, num_rows)?;
        self.players.black.title.read_column(reader, 4, num_rows)?;
        self.players.black.name.read_column(reader, 5, num_rows)?;
        self.players.black.rating.read_column(reader, 6, num_rows)?;
        self.date_year.read_column(reader, 7, num_rows)?;
        self.date_month.read_column(reader, 8, num_rows)?;
        self.date_day.read_column(reader, 9, num_rows)?;
        self.outcome.read_column(reader, 10, num_rows)?;
        self.speed.read_column(reader, 11, num_rows)?;
        self.rating_mode.read_column(reader, 12, num_rows)?;
        self.setup.read_column(reader, 13, num_rows)?;
        self.moves.read_column(reader, 14, num_rows)?;
        Ok(())
    }
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
        columns.fill_rows(
            &*row_group_reader,
            row_group_reader.metadata().num_rows() as usize,
        )?;

        rows += columns.game_id.len();
    }

    println!("seen rows = {}", rows);

    Ok(())
}
