use std::{
    ffi::OsStr,
    fs::File,
    io::{self, BufWriter, Write},
    path::PathBuf,
    sync::Arc,
    thread,
};

use bytes::Bytes;
use clap::Parser;
use crossbeam::channel;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use lila_openingexplorer4::{
    indexing::{FlatGame, FlatPlayer, GameVisitor, ParsedGame, RecentGameIds},
    packed::GameId,
};
use parquet::{
    basic::{Compression, LogicalType, Repetition, Type as PhysicalType, ZstdLevel},
    data_type::{
        ByteArray, ByteArrayType, DataType, FixedLenByteArray, FixedLenByteArrayType, Int32Type,
    },
    file::{
        properties::{EnabledStatistics, WriterProperties},
        writer::{SerializedFileWriter, SerializedRowGroupWriter},
    },
    schema::types::{ColumnPath, Type, TypePtr},
};
use pgn_reader::Reader;
use shakmaty::packed::PackedUciMove;
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

const ROW_GROUP_LEN: usize = 100_000;

const FLATTEN_THREADS: usize = 2; // Enough to keep up with parsed games

const CHANNEL_LEN: usize = 32768; // Enough to buffer while previous row group is being written

#[derive(Parser)]
struct Args {
    pgns: Vec<PathBuf>,
    #[clap(long)]
    out: PathBuf,
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

fn schema() -> TypePtr {
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

fn properties() -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(9).expect("zstd level"),
        ))
        .set_data_page_size_limit(8 * 1024 * 1024)
        .set_data_page_row_count_limit(usize::MAX)
        .set_column_dictionary_enabled(ColumnPath::from("game_id"), false)
        .set_column_statistics_enabled(ColumnPath::from("game_id"), EnabledStatistics::None)
        .set_column_dictionary_enabled(ColumnPath::from("moves"), false)
        .set_column_statistics_enabled(ColumnPath::from("moves"), EnabledStatistics::None)
        .set_column_dictionary_enabled(ColumnPath::from("setup"), false)
        .set_column_statistics_enabled(ColumnPath::from("setup"), EnabledStatistics::None)
        .build()
}

fn static_byte_array(value: &'static str) -> ByteArray {
    ByteArray::from(Bytes::from_static(value.as_bytes()))
}

struct OptionalColumn<T> {
    values: Vec<T>,
    def_levels: Vec<i16>,
}

impl<T> Default for OptionalColumn<T> {
    fn default() -> Self {
        OptionalColumn {
            values: Vec::new(),
            def_levels: Vec::new(),
        }
    }
}

impl<T> OptionalColumn<T> {
    fn push(&mut self, value: Option<T>) {
        match value {
            Some(value) => {
                self.values.push(value);
                self.def_levels.push(1);
            }
            None => self.def_levels.push(0),
        }
    }

    fn clear(&mut self) {
        self.values.clear();
        self.def_levels.clear();
    }
}

fn write_optional<T: DataType, W: Write + Send>(
    row_group: &mut SerializedRowGroupWriter<'_, W>,
    column: &OptionalColumn<T::T>,
) -> parquet::errors::Result<()> {
    let mut writer = row_group.next_column()?.expect("column");
    writer
        .typed::<T>()
        .write_batch(&column.values, Some(&column.def_levels), None)?;
    writer.close()
}

fn write_required<T: DataType, W: Write + Send>(
    row_group: &mut SerializedRowGroupWriter<'_, W>,
    values: &[T::T],
) -> parquet::errors::Result<()> {
    let mut writer = row_group.next_column()?.expect("column");
    writer.typed::<T>().write_batch(values, None, None)?;
    writer.close()
}

#[derive(Default)]
struct GameColumns {
    len: usize,
    game_id: Vec<FixedLenByteArray>,
    white_title: OptionalColumn<ByteArray>,
    white_name: Vec<ByteArray>,
    white_rating: OptionalColumn<i32>,
    black_title: OptionalColumn<ByteArray>,
    black_name: Vec<ByteArray>,
    black_rating: OptionalColumn<i32>,
    date_year: OptionalColumn<i32>,
    date_month: OptionalColumn<i32>,
    date_day: OptionalColumn<i32>,
    outcome: Vec<ByteArray>,
    speed: OptionalColumn<ByteArray>,
    rating_mode: Vec<ByteArray>,
    setup: OptionalColumn<ByteArray>,
    moves: Vec<ByteArray>,
}

impl GameColumns {
    fn push(&mut self, game: FlatGame) {
        self.game_id
            .push(FixedLenByteArray::from(game.game_id.to_bytes().to_vec()));

        push_player(
            &mut self.white_title,
            &mut self.white_name,
            &mut self.white_rating,
            game.players.white,
        );
        push_player(
            &mut self.black_title,
            &mut self.black_name,
            &mut self.black_rating,
            game.players.black,
        );

        self.date_year
            .push(game.date.as_ref().map(|d| d.year() as i32));
        self.date_month
            .push(game.date.as_ref().and_then(|d| d.month()).map(i32::from));
        self.date_day
            .push(game.date.as_ref().and_then(|d| d.day()).map(i32::from));

        self.outcome.push(static_byte_array(game.outcome.as_str()));
        self.speed
            .push(game.speed.map(|speed| static_byte_array(speed.as_str())));
        self.rating_mode
            .push(static_byte_array(game.rating_mode.as_str()));

        self.setup
            .push(game.setup.map(|setup| ByteArray::from(setup.as_bytes())));

        let mut moves = Vec::with_capacity(game.moves.len() * PackedUciMove::BYTES);
        for m in game.moves {
            moves.extend_from_slice(&m.to_bytes());
        }
        self.moves.push(ByteArray::from(moves));

        self.len += 1;
    }

    fn write<W: Write + Send>(
        &mut self,
        writer: &mut SerializedFileWriter<W>,
    ) -> parquet::errors::Result<()> {
        if self.len == 0 {
            return Ok(());
        }

        let mut row_group = writer.next_row_group()?;
        write_required::<FixedLenByteArrayType, _>(&mut row_group, &self.game_id)?;
        write_optional::<ByteArrayType, _>(&mut row_group, &self.white_title)?;
        write_required::<ByteArrayType, _>(&mut row_group, &self.white_name)?;
        write_optional::<Int32Type, _>(&mut row_group, &self.white_rating)?;
        write_optional::<ByteArrayType, _>(&mut row_group, &self.black_title)?;
        write_required::<ByteArrayType, _>(&mut row_group, &self.black_name)?;
        write_optional::<Int32Type, _>(&mut row_group, &self.black_rating)?;
        write_optional::<Int32Type, _>(&mut row_group, &self.date_year)?;
        write_optional::<Int32Type, _>(&mut row_group, &self.date_month)?;
        write_optional::<Int32Type, _>(&mut row_group, &self.date_day)?;
        write_required::<ByteArrayType, _>(&mut row_group, &self.outcome)?;
        write_optional::<ByteArrayType, _>(&mut row_group, &self.speed)?;
        write_required::<ByteArrayType, _>(&mut row_group, &self.rating_mode)?;
        write_optional::<ByteArrayType, _>(&mut row_group, &self.setup)?;
        write_required::<ByteArrayType, _>(&mut row_group, &self.moves)?;
        row_group.close()?;

        self.clear();
        Ok(())
    }

    fn clear(&mut self) {
        self.len = 0;
        self.game_id.clear();
        self.white_title.clear();
        self.white_name.clear();
        self.white_rating.clear();
        self.black_title.clear();
        self.black_name.clear();
        self.black_rating.clear();
        self.date_year.clear();
        self.date_month.clear();
        self.date_day.clear();
        self.outcome.clear();
        self.speed.clear();
        self.rating_mode.clear();
        self.setup.clear();
        self.moves.clear();
    }
}

fn push_player(
    titles: &mut OptionalColumn<ByteArray>,
    names: &mut Vec<ByteArray>,
    ratings: &mut OptionalColumn<i32>,
    player: FlatPlayer,
) {
    titles.push(
        player
            .title
            .map(|title| ByteArray::from(title.into_bytes())),
    );
    names.push(ByteArray::from(player.name.into_bytes()));
    ratings.push(player.rating.map(i32::from));
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    thread::scope(|s| {
        // Flatten in round robin, so that games can be collected in the
        // original order.
        let mut parsed_txs = Vec::with_capacity(FLATTEN_THREADS);
        let mut flat_rxs = Vec::with_capacity(FLATTEN_THREADS);

        for _ in 0..FLATTEN_THREADS {
            let (parsed_tx, parsed_rx) = channel::bounded::<ParsedGame>(CHANNEL_LEN);
            let (flat_tx, flat_rx) = channel::bounded::<io::Result<FlatGame>>(CHANNEL_LEN);
            parsed_txs.push(parsed_tx);
            flat_rxs.push(flat_rx);

            s.spawn(move || {
                for parsed in parsed_rx {
                    flat_tx
                        .send(FlatGame::flatten(parsed))
                        .expect("send flat game");
                }
            });
        }

        s.spawn(move || {
            let file = BufWriter::new(File::create(&args.out).expect("create parquet file"));
            let mut writer = SerializedFileWriter::new(file, schema(), Arc::new(properties()))
                .expect("parquet writer");

            let mut columns = GameColumns::default();

            'collect: loop {
                for flat_rx in &flat_rxs {
                    match flat_rx.recv() {
                        Ok(Ok(flat)) => {
                            columns.push(flat);
                            if columns.len >= ROW_GROUP_LEN {
                                columns.write(&mut writer).expect("write row group");
                            }
                        }
                        Ok(Err(err)) => eprintln!("skipping game: {}", err),
                        Err(channel::RecvError) => break 'collect,
                    }
                }
            }

            columns.write(&mut writer).expect("write final row group");
            writer.close().expect("close parquet file");
        });

        let mut next_thread = 0;

        // Consecutive database files can overlap, so that some games appear
        // twice.
        let mut recent_game_ids = RecentGameIds::new();

        for arg in args.pgns {
            let file = File::open(&arg)?;
            let progress = ProgressBar::with_draw_target(
                Some(file.metadata()?.len()),
                ProgressDrawTarget::stdout_with_hz(4),
            )
            .with_style(
                ProgressStyle::with_template(
                    "{spinner} {prefix} {msg} {wide_bar} {bytes_per_sec:>14} {eta:>7}",
                )
                .expect("progress template"),
            )
            .with_prefix(format!("{arg:?}"));

            let file = progress.wrap_read(file);

            let uncompressed: Box<dyn io::Read> = if arg.extension() == Some(OsStr::new("zst")) {
                Box::new(zstd::Decoder::new(file)?)
            } else {
                Box::new(file)
            };

            let mut reader = Reader::new(uncompressed);

            for parsed in reader.read_games(&mut GameVisitor) {
                let parsed = parsed??;

                if parsed
                    .game_id()
                    .is_some_and(|game_id| !recent_game_ids.insert(game_id))
                {
                    continue;
                }

                parsed_txs[next_thread].send(parsed).expect("send parsed");
                next_thread = (next_thread + 1) % FLATTEN_THREADS;
            }

            progress.finish();
        }

        if recent_game_ids.duplicates() > 0 {
            eprintln!("skipped {} duplicate games", recent_game_ids.duplicates());
        }

        Ok(())
    })
}
