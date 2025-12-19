use std::io;
use std::ops::ControlFlow;

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

#[derive(Default)]
struct Tags {
    game_id: Option<GameId>,
    white: Option<String>,
    black: Option<String>,
    white_elo: Option<Rating>,
    black_elo: Option<Rating>,
    white_title: Option<String>,
    black_title: Option<String>,
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
                            format!("invalid game id in Site tag: {}", err),
                        )
                    })?;
            }
            b"White" => {
                self.white = Some(String::from_utf8(value.as_bytes().to_owned()).map_err(
                    |err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid White tag: {}", err),
                        )
                    },
                )?);
            }
            b"Black" => {
                self.black = Some(String::from_utf8(value.as_bytes().to_owned()).map_err(
                    |err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid Black tag: {}", err),
                        )
                    },
                )?);
            }
            b"WhiteElo" => {
                self.white_elo = Some(Rating::from_ascii(value.as_bytes()).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid WhiteElo tag: {}", err),
                    )
                })?);
            }
            b"BlackElo" => {
                self.black_elo = Some(Rating::from_ascii(value.as_bytes()).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid BlackElo tag: {}", err),
                    )
                })?);
            }
            b"WhiteTitle" => {
                self.white_title = Some(String::from_utf8(value.as_bytes().to_owned()).map_err(
                    |err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid WhiteTitle tag: {}", err),
                        )
                    },
                )?);
            }
            b"BlackTitle" => {
                self.black_title = Some(String::from_utf8(value.as_bytes().to_owned()).map_err(
                    |err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid BlackTitle tag: {}", err),
                        )
                    },
                )?);
            }
            b"Date" => {
                self.date = Some(LaxDate::from_ascii(value.as_bytes()).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid Date tag: {}", err),
                    )
                })?);
            }
            b"Variant" => {
                self.variant = Some(Variant::from_ascii(value.as_bytes()).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid Variant tag: {}", err),
                    )
                })?);
            }
            b"FEN" => {
                self.fen = Some(Fen::from_ascii(value.as_bytes()).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid FEN tag: {}", err),
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
    setup: Option<PackedSetup>,

    pos: VariantPosition,
    moves: Vec<PackedUciMove>,
    outcome: Outcome,
}

impl Movetext {
    fn begin(tags: Tags) -> io::Result<Movetext> {
        let variant = tags.variant.unwrap_or_default();
        let (pos, need_setup) = match tags.fen {
            None => (VariantPosition::new(variant), variant != Variant::Chess),
            Some(fen) => (
                VariantPosition::from_setup(variant, fen.into(), shakmaty::CastlingMode::Chess960)
                    .map_err(|err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid setup: {}", err),
                        )
                    })?,
                true,
            ),
        };

        Ok(Movetext {
            game_id: tags
                .game_id
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing game id"))?,
            players: ByColor {
                white: GamePlayer {
                    title: tags.white_title,
                    name: tags.white.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "missing white name")
                    })?,
                    rating: tags.white_elo.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "missing white rating")
                    })?,
                },
                black: GamePlayer {
                    title: tags.black_title,
                    name: tags.black.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "missing black name")
                    })?,
                    rating: tags.black_elo.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "missing black rating")
                    })?,
                },
            },
            date: tags
                .date
                .ok_or(io::Error::new(io::ErrorKind::InvalidData, "missing date"))?,
            setup: need_setup.then(|| {
                PackedSetup::pack_variant_normalized(
                    &pos.to_setup(shakmaty::EnPassantMode::Legal),
                    variant,
                )
                .expect("legal setup")
            }),

            pos,
            moves: Vec::new(),
            outcome: Outcome::Unknown,
        })
    }

    fn push(&mut self, san: SanPlus) -> io::Result<()> {
        let m = san.san.to_move(&self.pos).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("illegal san {} in game {}: {}", san, self.game_id, err),
            )
        })?;
        let uci = UciMove::from_chess960(m);
        self.moves.push(PackedUciMove::pack(uci));
        self.pos.play_unchecked(m);
        Ok(())
    }
}

impl Visitor for ToLichessGame {
    type Tags = Tags;
    type Movetext = Movetext;
    type Output = io::Result<(GameId, LichessGame)>;

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
        Ok((
            movetext.game_id,
            LichessGame {
                players: movetext.players,
                date: movetext.date,
                outcome: movetext.outcome,
                speed: Speed::Classical, // todo
                rating_mode: RatingMode::Rated,
                setup: movetext.setup,
                moves: movetext.moves,
            },
        ))
    }
}

fn main() -> io::Result<()> {
    let mut reader = Reader::new(io::stdin());
    for game in reader.read_games(&mut ToLichessGame) {
        let (game_id, _) = game??;
        println!("id: {}", game_id);
    }
    Ok(())
}
