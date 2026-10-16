use std::{io::Write, iter, sync::Arc};

use bytes::{BufMut, Bytes};
use parquet::{
    basic::{Compression, LogicalType, Repetition, Type as PhysicalType},
    data_type::{ByteArray, ByteArrayType, FixedLenByteArray, FixedLenByteArrayType, Int32Type},
    file::{
        properties::{EnabledStatistics, WriterProperties, WriterPropertiesBuilder},
        reader::RowGroupReader,
        writer::{SerializedFileWriter, SerializedRowGroupWriter},
    },
    schema::types::{ColumnPath, Type, TypePtr},
};
use shakmaty::{ByColor, Outcome};
use thiserror::Error;

use crate::{
    packed::{
        GameId, LaxDate, Rating, RatingMode, Speed, outcome_bits, pack_bytes,
        pack_speed_and_rating_mode,
    },
    parquet::{OptionalColumn, RequiredColumn},
    pgn_to_parquet::{FlatGame, FlatPlayer},
};

#[derive(Default)]
pub struct PlayerColumns {
    title: OptionalColumn<ByteArrayType>,
    name: RequiredColumn<ByteArrayType>,
    rating: OptionalColumn<Int32Type>,
}

impl PlayerColumns {
    pub fn clear(&mut self) {
        self.title.clear();
        self.name.clear();
        self.rating.clear();
    }

    pub fn reserve(&mut self, additional: usize) {
        self.title.reserve(additional);
        self.name.reserve(additional);
        self.rating.reserve(additional);
    }

    pub fn append(&mut self, other: &mut PlayerColumns) {
        self.title.append(&mut other.title);
        self.name.append(&mut other.name);
        self.rating.append(&mut other.rating);
    }

    pub fn push(&mut self, player: FlatPlayer) {
        self.title.push(
            player
                .title
                .map(|title| ByteArray::from(title.into_bytes())),
        );
        self.name.push(ByteArray::from(player.name.into_bytes()));
        self.rating.push(player.rating.map(i32::from));
    }

    pub fn write_columns<W>(
        &self,
        row_group: &mut SerializedRowGroupWriter<'_, W>,
    ) -> parquet::errors::Result<()>
    where
        W: Write + Send,
    {
        self.title.write_column(row_group)?;
        self.name.write_column(row_group)?;
        self.rating.write_column(row_group)?;
        Ok(())
    }

    pub fn scatter(&self, partitions: &[Option<usize>], targets: &mut [&mut PlayerColumns]) {
        self.title
            .scatter(partitions, &mut project(targets, |t| &mut t.title));
        self.name
            .scatter(partitions, &mut project(targets, |t| &mut t.name));
        self.rating
            .scatter(partitions, &mut project(targets, |t| &mut t.rating));
    }

    pub fn read_columns(
        &mut self,
        reader: &dyn RowGroupReader,
        base_column_index: usize,
    ) -> parquet::errors::Result<()> {
        self.title.read_column(reader, base_column_index)?;
        self.name.read_column(reader, base_column_index + 1)?;
        self.rating.read_column(reader, base_column_index + 2)?;
        Ok(())
    }

    pub fn rows(&self) -> impl Iterator<Item = PlayerRow<'_>> {
        let mut title = self.title.iter();
        let mut name = self.name.iter();
        let mut rating = self.rating.iter();
        iter::from_fn(move || {
            Some(PlayerRow {
                name: name.next()?.data(),
                title: title.next().expect("title").map(ByteArray::data),
                rating: rating.next().expect("rating").copied(),
            })
        })
    }
}

pub struct PlayerRow<'a> {
    pub title: Option<&'a [u8]>,
    pub name: &'a [u8],
    pub rating: Option<i32>,
}

pub struct GameRow<'a> {
    pub game_id: GameId,
    pub players: ByColor<PlayerRow<'a>>,
    pub date_year: Option<i32>,
    pub date_month: Option<i32>,
    pub date_day: Option<i32>,
    pub outcome: &'a [u8],
    pub speed: Option<&'a [u8]>,
    pub rating_mode: &'a [u8],
    pub setup: Option<&'a [u8]>,
    pub moves: &'a [u8],
}

#[derive(Debug, Error, Copy, Clone, PartialEq, Eq)]
pub enum InvalidGameRow {
    #[error("missing rating")]
    MissingRating,
    #[error("invalid rating")]
    InvalidRating,
    #[error("missing date")]
    MissingDate,
    #[error("invalid date")]
    InvalidDate,
    #[error("invalid outcome")]
    InvalidOutcome,
    #[error("missing speed")]
    MissingSpeed,
    #[error("invalid speed")]
    InvalidSpeed,
    #[error("invalid rating mode")]
    InvalidRatingMode,
}

impl GameRow<'_> {
    pub fn pack_lichess_game<B: BufMut>(&self, buf: &mut B) -> Result<(), InvalidGameRow> {
        let ratings = ByColor {
            white: rating_from_row(self.players.white.rating)?,
            black: rating_from_row(self.players.black.rating)?,
        };

        let date = LaxDate::from_ymd(
            u32::try_from(self.date_year.ok_or(InvalidGameRow::MissingDate)?)
                .map_err(|_| InvalidGameRow::InvalidDate)?,
            optional_u8(self.date_month)?,
            optional_u8(self.date_day)?,
        )
        .map_err(|_| InvalidGameRow::InvalidDate)?;

        let outcome =
            Outcome::from_ascii(self.outcome).map_err(|_| InvalidGameRow::InvalidOutcome)?;

        let speed = Speed::from_ascii(self.speed.ok_or(InvalidGameRow::MissingSpeed)?)
            .map_err(|_| InvalidGameRow::InvalidSpeed)?;

        let rating_mode = RatingMode::from_ascii(self.rating_mode)
            .map_err(|_| InvalidGameRow::InvalidRatingMode)?;

        for (player, rating) in [
            (&self.players.white, ratings.white),
            (&self.players.black, ratings.black),
        ] {
            pack_bytes(buf, player.title.unwrap_or(b""));
            pack_bytes(buf, player.name);
            rating.pack(buf);
        }
        date.pack(buf);
        buf.put_u8(outcome_bits(outcome));
        pack_speed_and_rating_mode(buf, speed, rating_mode);
        pack_bytes(buf, self.setup.unwrap_or(b""));
        buf.put_slice(self.moves);
        Ok(())
    }
}

fn rating_from_row(rating: Option<i32>) -> Result<Rating, InvalidGameRow> {
    Rating::try_from(rating.ok_or(InvalidGameRow::MissingRating)?)
        .map_err(|_| InvalidGameRow::InvalidRating)
}

fn optional_u8(n: Option<i32>) -> Result<Option<u8>, InvalidGameRow> {
    n.map(|n| u8::try_from(n).map_err(|_| InvalidGameRow::InvalidDate))
        .transpose()
}

#[derive(Default)]
pub struct GameColumns {
    game_id: RequiredColumn<FixedLenByteArrayType>, // 0
    players: ByColor<PlayerColumns>,                // 1,2,3 / 4,5,6
    date_year: OptionalColumn<Int32Type>,           // 7
    date_month: OptionalColumn<Int32Type>,          // 8
    date_day: OptionalColumn<Int32Type>,            // 9
    outcome: RequiredColumn<ByteArrayType>,         // 10
    speed: OptionalColumn<ByteArrayType>,           // 11
    rating_mode: RequiredColumn<ByteArrayType>,     // 12
    setup: OptionalColumn<ByteArrayType>,           // 13
    moves: RequiredColumn<ByteArrayType>,           // 14
}

impl GameColumns {
    pub fn len(&self) -> usize {
        self.game_id.len()
    }

    pub fn clear(&mut self) {
        self.game_id.clear();
        self.players.white.clear();
        self.players.black.clear();
        self.date_year.clear();
        self.date_month.clear();
        self.date_day.clear();
        self.outcome.clear();
        self.speed.clear();
        self.rating_mode.clear();
        self.setup.clear();
        self.moves.clear();
    }

    pub fn reserve(&mut self, additional: usize) {
        self.game_id.reserve(additional);
        self.players.white.reserve(additional);
        self.players.black.reserve(additional);
        self.date_year.reserve(additional);
        self.date_month.reserve(additional);
        self.date_day.reserve(additional);
        self.outcome.reserve(additional);
        self.speed.reserve(additional);
        self.rating_mode.reserve(additional);
        self.setup.reserve(additional);
        self.moves.reserve(additional);
    }

    pub fn append(&mut self, other: &mut GameColumns) {
        self.game_id.append(&mut other.game_id);
        self.players.white.append(&mut other.players.white);
        self.players.black.append(&mut other.players.black);
        self.date_year.append(&mut other.date_year);
        self.date_month.append(&mut other.date_month);
        self.date_day.append(&mut other.date_day);
        self.outcome.append(&mut other.outcome);
        self.speed.append(&mut other.speed);
        self.rating_mode.append(&mut other.rating_mode);
        self.setup.append(&mut other.setup);
        self.moves.append(&mut other.moves);
    }

    pub fn rows(&self) -> impl Iterator<Item = GameRow<'_>> {
        let mut game_id = self.game_id.iter();
        let mut white = self.players.white.rows();
        let mut black = self.players.black.rows();
        let mut date_year = self.date_year.iter();
        let mut date_month = self.date_month.iter();
        let mut date_day = self.date_day.iter();
        let mut outcome = self.outcome.iter();
        let mut speed = self.speed.iter();
        let mut rating_mode = self.rating_mode.iter();
        let mut setup = self.setup.iter();
        let mut moves = self.moves.iter();
        iter::from_fn(move || {
            Some(GameRow {
                game_id: GameId::unpack(&mut game_id.next()?.data()),
                players: ByColor {
                    white: white.next().expect("white"),
                    black: black.next().expect("black"),
                },
                date_year: date_year.next().expect("date_year").copied(),
                date_month: date_month.next().expect("date_month").copied(),
                date_day: date_day.next().expect("date_day").copied(),
                outcome: outcome.next().expect("outcome").data(),
                speed: speed.next().expect("speed").map(ByteArray::data),
                rating_mode: rating_mode.next().expect("rating_mode").data(),
                setup: setup.next().expect("setup").map(ByteArray::data),
                moves: moves.next().expect("moves").data(),
            })
        })
    }

    pub fn push(&mut self, game: FlatGame) {
        self.game_id
            .push(FixedLenByteArray::from(game.game_id.to_bytes().to_vec()));

        self.players.white.push(game.players.white);
        self.players.black.push(game.players.black);

        self.date_year
            .push(game.date.as_ref().map(|d| d.year() as i32));
        self.date_month
            .push(game.date.as_ref().and_then(|d| d.month()).map(i32::from));
        self.date_day
            .push(game.date.as_ref().and_then(|d| d.day()).map(i32::from));

        self.outcome.push(ByteArray::from(Bytes::from_static(
            game.outcome.as_str().as_bytes(),
        )));
        self.speed.push(
            game.speed
                .map(|speed| ByteArray::from(Bytes::from_static(speed.as_str().as_bytes()))),
        );
        self.rating_mode.push(ByteArray::from(Bytes::from_static(
            game.rating_mode.as_str().as_bytes(),
        )));

        self.setup
            .push(game.setup.map(|setup| ByteArray::from(setup.as_bytes())));

        self.moves.push(ByteArray::from(game.moves.0));
    }

    pub fn write_row_group<W>(
        &self,
        writer: &mut SerializedFileWriter<W>,
    ) -> parquet::errors::Result<()>
    where
        W: Write + Send,
    {
        let mut row_group = writer.next_row_group()?;
        self.game_id.write_column(&mut row_group)?;
        self.players.white.write_columns(&mut row_group)?;
        self.players.black.write_columns(&mut row_group)?;
        self.date_year.write_column(&mut row_group)?;
        self.date_month.write_column(&mut row_group)?;
        self.date_day.write_column(&mut row_group)?;
        self.outcome.write_column(&mut row_group)?;
        self.speed.write_column(&mut row_group)?;
        self.rating_mode.write_column(&mut row_group)?;
        self.setup.write_column(&mut row_group)?;
        self.moves.write_column(&mut row_group)?;
        row_group.close()?;
        Ok(())
    }

    pub fn read_columns(&mut self, reader: &dyn RowGroupReader) -> parquet::errors::Result<()> {
        self.game_id.read_column(reader, 0)?;
        self.players.white.read_columns(reader, 1)?;
        self.players.black.read_columns(reader, 4)?;
        self.date_year.read_column(reader, 7)?;
        self.date_month.read_column(reader, 8)?;
        self.date_day.read_column(reader, 9)?;
        self.outcome.read_column(reader, 10)?;
        self.speed.read_column(reader, 11)?;
        self.rating_mode.read_column(reader, 12)?;
        self.setup.read_column(reader, 13)?;
        self.moves.read_column(reader, 14)?;
        Ok(())
    }

    pub fn scatter(
        &self,
        mut partition: impl FnMut(GameId) -> Option<usize>,
        targets: &mut [&mut GameColumns],
    ) {
        let partitions: Vec<Option<usize>> = self
            .game_id
            .iter()
            .map(|game_id| partition(GameId::unpack(&mut game_id.data())))
            .collect();

        self.game_id
            .scatter(&partitions, &mut project(targets, |t| &mut t.game_id));
        self.players
            .white
            .scatter(&partitions, &mut project(targets, |t| &mut t.players.white));
        self.players
            .black
            .scatter(&partitions, &mut project(targets, |t| &mut t.players.black));
        self.date_year
            .scatter(&partitions, &mut project(targets, |t| &mut t.date_year));
        self.date_month
            .scatter(&partitions, &mut project(targets, |t| &mut t.date_month));
        self.date_day
            .scatter(&partitions, &mut project(targets, |t| &mut t.date_day));
        self.outcome
            .scatter(&partitions, &mut project(targets, |t| &mut t.outcome));
        self.speed
            .scatter(&partitions, &mut project(targets, |t| &mut t.speed));
        self.rating_mode
            .scatter(&partitions, &mut project(targets, |t| &mut t.rating_mode));
        self.setup
            .scatter(&partitions, &mut project(targets, |t| &mut t.setup));
        self.moves
            .scatter(&partitions, &mut project(targets, |t| &mut t.moves));
    }

    pub fn schema() -> TypePtr {
        Arc::new(
            Type::group_type_builder("game")
                .with_fields(vec![
                    fixed_binary("game_id", GameId::SIZE as i32, Repetition::REQUIRED),
                    string("white_title", Repetition::OPTIONAL),
                    string("white_name", Repetition::REQUIRED),
                    int32("white_rating", Repetition::OPTIONAL),
                    string("black_title", Repetition::OPTIONAL),
                    string("black_name", Repetition::REQUIRED),
                    int32("black_rating", Repetition::OPTIONAL),
                    int32("date_year", Repetition::OPTIONAL),
                    int32("date_month", Repetition::OPTIONAL),
                    int32("date_day", Repetition::OPTIONAL),
                    string("outcome", Repetition::REQUIRED),
                    string("speed", Repetition::OPTIONAL),
                    string("rating_mode", Repetition::REQUIRED),
                    binary("setup", Repetition::OPTIONAL),
                    binary("moves", Repetition::REQUIRED),
                ])
                .build()
                .expect("group type"),
        )
    }

    pub fn properties() -> WriterPropertiesBuilder {
        WriterProperties::builder()
            .set_data_page_size_limit(8 * 1024 * 1024)
            .set_data_page_row_count_limit(usize::MAX)
            .set_column_dictionary_enabled(ColumnPath::from("game_id"), false)
            .set_column_statistics_enabled(ColumnPath::from("game_id"), EnabledStatistics::None)
            .set_column_compression(ColumnPath::from("game_id"), Compression::UNCOMPRESSED) // Random ids
            .set_column_dictionary_enabled(ColumnPath::from("moves"), false)
            .set_column_statistics_enabled(ColumnPath::from("moves"), EnabledStatistics::None)
            .set_column_compression(ColumnPath::from("moves"), Compression::UNCOMPRESSED) // ~%7 for zstd -9 not worth it
            .set_column_dictionary_enabled(ColumnPath::from("setup"), false)
            .set_column_statistics_enabled(ColumnPath::from("setup"), EnabledStatistics::None)
    }
}

fn project<'a, 'b: 'a, S: 'b, T>(
    targets: &'a mut [&'b mut S],
    f: impl FnMut(&'a mut &'b mut S) -> &'a mut T,
) -> Vec<&'a mut T> {
    targets.iter_mut().map(f).collect()
}

fn primitive(
    name: &str,
    physical_type: PhysicalType,
    logical_type: Option<LogicalType>,
    repetition: Repetition,
) -> TypePtr {
    Arc::new(
        Type::primitive_type_builder(name, physical_type)
            .with_logical_type(logical_type)
            .with_repetition(repetition)
            .build()
            .expect("primitive type"),
    )
}

fn string(name: &str, repetition: Repetition) -> TypePtr {
    primitive(
        name,
        PhysicalType::BYTE_ARRAY,
        Some(LogicalType::String),
        repetition,
    )
}

fn int32(name: &str, repetition: Repetition) -> TypePtr {
    primitive(name, PhysicalType::INT32, None, repetition)
}

fn binary(name: &str, repetition: Repetition) -> TypePtr {
    primitive(name, PhysicalType::BYTE_ARRAY, None, repetition)
}

fn fixed_binary(name: &str, len: i32, repetition: Repetition) -> TypePtr {
    Arc::new(
        Type::primitive_type_builder(name, PhysicalType::FIXED_LEN_BYTE_ARRAY)
            .with_length(len)
            .with_repetition(repetition)
            .build()
            .expect("fixed len byte array type"),
    )
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;
    use crate::packed::LichessGame;

    #[test]
    fn test_pack_lichess_game_matches_pack_entire() {
        arbtest(|u| {
            let game_id: GameId = u.arbitrary()?;
            let game: LichessGame = u.arbitrary()?;

            let flat = FlatGame {
                game_id,
                players: ByColor {
                    white: FlatPlayer {
                        title: game.players.white.title.clone(),
                        name: game.players.white.name.clone(),
                        rating: Some(game.players.white.rating),
                    },
                    black: FlatPlayer {
                        title: game.players.black.title.clone(),
                        name: game.players.black.name.clone(),
                        rating: Some(game.players.black.rating),
                    },
                },
                date: Some(game.date.clone()),
                outcome: game.outcome,
                speed: Some(game.speed),
                rating_mode: game.rating_mode,
                setup: game.setup.clone(),
                moves: game.moves.clone(),
            };

            let mut columns = GameColumns::default();
            columns.push(flat);
            let row = columns.rows().next().expect("row");
            assert_eq!(row.game_id, game_id);

            let mut expected = Vec::new();
            game.pack_entire(&mut expected);

            let mut actual = Vec::new();
            row.pack_lichess_game(&mut actual).expect("valid row");
            assert_eq!(actual, expected);

            Ok(())
        });
    }
}
