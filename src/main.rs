use clap::Parser;
use fjall::PartitionCreateOptions;
use fjall::Slice;
use indicatif::ProgressBar;
use indicatif::ProgressDrawTarget;
use indicatif::ProgressStyle;
use shakmaty::zobrist::Zobrist32;
use shakmaty::zobrist::ZobristHash;
use std::error::Error;

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::ops::ControlFlow;
use std::path::PathBuf;

use lila_openingexplorer4::packed::{
    GameId, GamePlayer, LaxDate, LichessGame, Rating, RatingMode, Speed,
};
use pgn_reader::{RawTag, Reader, Skip, Visitor};
use shakmaty::packed::PackedSetup;
use shakmaty::san::SanPlus;
use shakmaty::uci::UciMove;
use shakmaty::variant::Variant;
use shakmaty::{ByColor, Position};
use shakmaty::{Outcome, fen::Fen, packed::PackedUciMove, variant::VariantPosition};

struct ToLichessGame;

const MAX_PLIES: usize = 50;

#[derive(Default)]
struct Tags {
    game_id: Option<GameId>,
    white: Option<String>,
    black: Option<String>,
    white_elo: Option<Rating>,
    black_elo: Option<Rating>,
    white_title: Option<String>,
    black_title: Option<String>,
    speed: Option<Speed>,
    date: Option<LaxDate>,
    variant: Option<Variant>,
    fen: Option<Fen>,
}

impl Tags {
    fn insert(&mut self, name: &[u8], value: RawTag<'_>) -> io::Result<()> {
        match name {
            b"Site" => {
                self.game_id = value
                    .as_bytes()
                    .strip_prefix(b"https://lichess.org/")
                    .map(|id| GameId::from_ascii(id))
                    .transpose()
                    .map_err(|err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "invalid game id in Site tag {:?}: {}",
                                value.decode_utf8_lossy(),
                                err
                            ),
                        )
                    })?;
            }
            b"White" => {
                self.white = Some(String::from_utf8(value.as_bytes().to_owned()).map_err(
                    |err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid White tag: {} @ {:?}", err, self.game_id),
                        )
                    },
                )?);
            }
            b"Black" => {
                self.black = Some(String::from_utf8(value.as_bytes().to_owned()).map_err(
                    |err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid Black tag: {} @ {:?}", err, self.game_id),
                        )
                    },
                )?);
            }
            b"WhiteElo" => {
                self.white_elo = Some(Rating::from_ascii(value.as_bytes()).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "invalid WhiteElo tag {:?}: {} @ {:?}",
                            value.decode_utf8_lossy(),
                            err,
                            self.game_id
                        ),
                    )
                })?);
            }
            b"BlackElo" => {
                self.black_elo = Some(Rating::from_ascii(value.as_bytes()).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "invalid BlackElo tag {:?}: {} @ {:?}",
                            value.decode_utf8_lossy(),
                            err,
                            self.game_id
                        ),
                    )
                })?);
            }
            b"WhiteTitle" => {
                self.white_title = Some(String::from_utf8(value.as_bytes().to_owned()).map_err(
                    |err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid WhiteTitle tag: {} @ {:?}", err, self.game_id),
                        )
                    },
                )?);
            }
            b"BlackTitle" => {
                self.black_title = Some(String::from_utf8(value.as_bytes().to_owned()).map_err(
                    |err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid BlackTitle tag: {} @ {:?}", err, self.game_id),
                        )
                    },
                )?);
            }
            b"Date" => {
                self.date = Some(LaxDate::from_ascii(value.as_bytes()).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "invalid Date tag {:?}: {} @ {:?}",
                            value.decode_utf8_lossy(),
                            err,
                            self.game_id
                        ),
                    )
                })?);
            }
            b"TimeControl" => {
                self.speed = Some(Speed::from_time_control_tag(value.as_bytes()).map_err(
                    |err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "invalid TimeControl tag {:?}: {} @ {:?}",
                                value.decode_utf8_lossy(),
                                err,
                                self.game_id
                            ),
                        )
                    },
                )?);
            }
            b"Variant" => {
                self.variant = Some(Variant::from_ascii(value.as_bytes()).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "invalid Variant tag {:?}: {} @ {:?}",
                            value.decode_utf8_lossy(),
                            err,
                            self.game_id
                        ),
                    )
                })?);
            }
            b"FEN" => {
                self.fen = Some(Fen::from_ascii(value.as_bytes()).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "invalid FEN tag {:?}: {} @ {:?}",
                            value.decode_utf8_lossy(),
                            err,
                            self.game_id
                        ),
                    )
                })?);
            }
            _ => {}
        }
        Ok(())
    }
}

struct Movetext {
    game_id: GameId,
    players: ByColor<GamePlayer>,
    date: LaxDate,
    speed: Speed,
    setup: Option<PackedSetup>,

    pos: VariantPosition,
    moves: Vec<PackedUciMove>,
    hashes: Vec<Zobrist32>,
    outcome: Outcome,
}

impl Movetext {
    fn begin(tags: Tags) -> io::Result<Movetext> {
        let game_id = tags
            .game_id
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing game id"))?;

        let variant = tags.variant.unwrap_or_default();
        let (pos, need_setup) = match tags.fen {
            None => (VariantPosition::new(variant), variant != Variant::Chess),
            Some(fen) => (
                VariantPosition::from_setup(variant, fen.into(), shakmaty::CastlingMode::Chess960)
                    .map_err(|err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid setup in game {}: {}", game_id, err),
                        )
                    })?,
                true,
            ),
        };

        Ok(Movetext {
            game_id,
            players: ByColor {
                white: GamePlayer {
                    title: tags.white_title,
                    name: tags.white.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("missing white name in game {}", game_id),
                        )
                    })?,
                    rating: tags.white_elo.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("missing white rating in game {}", game_id),
                        )
                    })?,
                },
                black: GamePlayer {
                    title: tags.black_title,
                    name: tags.black.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("missing black name in game {}", game_id),
                        )
                    })?,
                    rating: tags.black_elo.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("missing black rating in game {}", game_id),
                        )
                    })?,
                },
            },
            speed: tags.speed.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("missing speed in game {}", game_id),
                )
            })?,
            date: tags.date.ok_or(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("missing date in game {}", game_id),
            ))?,
            setup: need_setup.then(|| {
                PackedSetup::pack_variant_normalized(
                    &pos.to_setup(shakmaty::EnPassantMode::Legal),
                    variant,
                )
                .expect("legal setup")
            }),

            pos,
            moves: Vec::with_capacity(MAX_PLIES),
            hashes: Vec::with_capacity(MAX_PLIES),
            outcome: Outcome::Unknown,
        })
    }

    fn push(&mut self, san: SanPlus) -> io::Result<()> {
        if self.moves.len() >= MAX_PLIES {
            return Ok(());
        }

        let m = san.san.to_move(&self.pos).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("illegal san {} in game {}: {}", san, self.game_id, err),
            )
        })?;

        let hash = self.pos.zobrist_hash(shakmaty::EnPassantMode::Legal);
        self.hashes.push(hash);

        let uci = UciMove::from_chess960(m);
        self.moves.push(PackedUciMove::pack(uci));

        self.pos.play_unchecked(m);

        Ok(())
    }
}

struct PreparedLichessGame {
    game_id: GameId,
    game: LichessGame,
    hashes: Vec<Zobrist32>,
}

impl Visitor for ToLichessGame {
    type Tags = Tags;
    type Movetext = Movetext;
    type Output = io::Result<PreparedLichessGame>;

    fn begin_tags(&mut self) -> ControlFlow<Self::Output, Self::Tags> {
        ControlFlow::Continue(Tags::default())
    }

    fn tag(
        &mut self,
        tags: &mut Tags,
        name: &[u8],
        value: RawTag<'_>,
    ) -> ControlFlow<Self::Output> {
        match tags.insert(name, value) {
            Ok(()) => ControlFlow::Continue(()),
            Err(err) => ControlFlow::Break(Err(err)),
        }
    }

    fn begin_movetext(&mut self, tags: Self::Tags) -> ControlFlow<Self::Output, Self::Movetext> {
        match Movetext::begin(tags) {
            Ok(movetext) => ControlFlow::Continue(movetext),
            Err(err) => ControlFlow::Break(Err(err)),
        }
    }

    fn begin_variation(
        &mut self,
        _movetext: &mut Self::Movetext,
    ) -> ControlFlow<Self::Output, Skip> {
        ControlFlow::Continue(Skip(true))
    }

    fn san(&mut self, movetext: &mut Movetext, san: SanPlus) -> ControlFlow<Self::Output> {
        match movetext.push(san) {
            Ok(()) => ControlFlow::Continue(()),
            Err(err) => ControlFlow::Break(Err(err)),
        }
    }

    fn outcome(&mut self, movetext: &mut Movetext, outcome: Outcome) -> ControlFlow<Self::Output> {
        movetext.outcome = outcome;
        ControlFlow::Continue(())
    }

    fn end_game(&mut self, movetext: Self::Movetext) -> Self::Output {
        Ok(PreparedLichessGame {
            game_id: movetext.game_id,
            game: LichessGame {
                players: movetext.players,
                date: movetext.date,
                outcome: movetext.outcome,
                speed: movetext.speed,
                rating_mode: RatingMode::Rated,
                setup: movetext.setup,
                moves: movetext.moves,
            },
            hashes: movetext.hashes,
        })
    }
}

#[derive(Parser)]
struct Args {
    pgns: Vec<PathBuf>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    let keyspace = fjall::Config::new("_db")
        .cache_size(32 * 1024 * 1024 * 1024)
        .open_transactional()?;
    let games_partition = keyspace.open_partition(
        "lichess_game",
        PartitionCreateOptions::default().max_memtable_size(64 * 1024 * 1024),
    )?;
    let index_partition = keyspace.open_partition(
        "lichess_index",
        PartitionCreateOptions::default().max_memtable_size(64 * 1024 * 1024),
    )?;

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
        for prepared in reader.read_games(&mut ToLichessGame) {
            let prepared = prepared??;
            progress.set_message(prepared.game.date.to_string());

            let mut packed_game = Vec::new();
            prepared.game.pack_entire(&mut packed_game);

            let mut write_tx = keyspace.write_tx();

            write_tx.insert(&games_partition, prepared.game_id.to_bytes(), packed_game);

            for hash in prepared.hashes {
                write_tx.update_fetch(
                    &index_partition,
                    u32::from(hash).to_be_bytes(),
                    |value| {
                        let value: &[u8] = value.map_or(&[], |v| v);
                        let (chunks, tail) = value.as_chunks();
                        assert!(tail.is_empty());
                        let mut chunks: Vec<[u8; GameId::SIZE]> = chunks.to_owned();
                        if chunks.len() < 20 {
                            match chunks.binary_search(&prepared.game_id.to_bytes()) {
                                Ok(_) => {} // already exists
                                Err(pos) => chunks.insert(pos, prepared.game_id.to_bytes()),
                            }
                        }
                        Some(Slice::new(chunks.as_flattened()))
                    },
                )?;
            }

            write_tx.commit()?;
        }

        progress.finish();
    }
    Ok(())
}
