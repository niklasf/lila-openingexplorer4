use shakmaty::Color;
use std::{io, ops::ControlFlow};

use nohash_hasher::IntMap;
use pgn_reader::{RawTag, Skip, Visitor};
use shakmaty::{
    ByColor, CastlingMode, Outcome, Position,
    fen::Fen,
    packed::{PackedSetup, PackedUciMove},
    san::{San, SanPlus},
    variant::{Variant, VariantPosition},
    zobrist::{Zobrist128, ZobristHash},
};

use crate::packed::{GameId, GamePlayer, LaxDate, Rating, Speed};

pub struct GameVisitor;

#[derive(Default)]
pub struct ParsedGame {
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
    moves: Vec<San>,
    outcome: Option<Outcome>,
}

impl ParsedGame {
    fn visit_tag(&mut self, name: &[u8], value: RawTag<'_>) -> io::Result<()> {
        match name {
            b"Site" => {
                self.game_id = value
                    .as_bytes()
                    .strip_prefix(b"https://lichess.org/")
                    .map(GameId::from_ascii)
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
            b"UTCDate" => {
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

    fn visit_san(&mut self, san: San) {
        self.moves.push(san);
    }

    fn visit_outcome(&mut self, outcome: Outcome) {
        self.outcome = Some(outcome);
    }
}

pub struct PreparedGameMeta {
    pub game_id: GameId,
    pub players: ByColor<GamePlayer>,
    pub speed: Speed,
    pub date: LaxDate,
    pub outcome: Outcome,
    pub setup: Option<PackedSetup>,
}

pub struct PreparedGame {
    pub meta: PreparedGameMeta,
    pub positions: IntMap<Zobrist128, (Color, PackedUciMove)>,
}

impl PreparedGame {
    pub fn prepare(parsed: ParsedGame) -> io::Result<PreparedGame> {
        let game_id = parsed
            .game_id
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing game id"))?;

        let variant = parsed.variant.unwrap_or_default();
        let (mut pos, need_setup) = match parsed.fen {
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

        let meta = PreparedGameMeta {
            game_id,
            players: ByColor {
                white: GamePlayer {
                    title: parsed.white_title,
                    name: parsed.white.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("missing white name in game {}", game_id),
                        )
                    })?,
                    rating: parsed.white_elo.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("missing white rating in game {}", game_id),
                        )
                    })?,
                },
                black: GamePlayer {
                    title: parsed.black_title,
                    name: parsed.black.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("missing black name in game {}", game_id),
                        )
                    })?,
                    rating: parsed.black_elo.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("missing black rating in game {}", game_id),
                        )
                    })?,
                },
            },
            speed: parsed.speed.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("missing speed in game {}", game_id),
                )
            })?,
            date: parsed.date.ok_or(io::Error::new(
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
            outcome: parsed.outcome.unwrap_or(Outcome::Unknown),
        };

        let mut positions =
            IntMap::with_capacity_and_hasher(parsed.moves.len(), Default::default());

        for san in parsed.moves {
            let m = san.to_move(&pos).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("illegal san {} in game {}: {}", san, game_id, err),
                )
            })?;
            positions.insert(
                pos.zobrist_hash(shakmaty::EnPassantMode::Legal),
                (
                    pos.turn(),
                    PackedUciMove::pack(m.to_uci(CastlingMode::Chess960)),
                ),
            );
            pos.play_unchecked(m);
        }

        Ok(PreparedGame { meta, positions })
    }
}

impl Visitor for GameVisitor {
    type Tags = ParsedGame;
    type Movetext = ParsedGame;
    type Output = io::Result<ParsedGame>;

    fn begin_tags(&mut self) -> ControlFlow<Self::Output, Self::Tags> {
        ControlFlow::Continue(ParsedGame::default())
    }

    fn tag(
        &mut self,
        tags: &mut Self::Tags,
        name: &[u8],
        value: RawTag<'_>,
    ) -> ControlFlow<Self::Output> {
        match tags.visit_tag(name, value) {
            Ok(()) => ControlFlow::Continue(()),
            Err(err) => ControlFlow::Break(Err(err)),
        }
    }

    fn begin_movetext(&mut self, tags: Self::Tags) -> ControlFlow<Self::Output, Self::Movetext> {
        ControlFlow::Continue(tags)
    }

    fn begin_variation(
        &mut self,
        _movetext: &mut Self::Movetext,
    ) -> ControlFlow<Self::Output, Skip> {
        ControlFlow::Continue(Skip(true))
    }

    fn san(
        &mut self,
        movetext: &mut Self::Movetext,
        san_plus: SanPlus,
    ) -> ControlFlow<Self::Output> {
        movetext.visit_san(san_plus.san);
        ControlFlow::Continue(())
    }

    fn outcome(
        &mut self,
        movetext: &mut Self::Movetext,
        outcome: Outcome,
    ) -> ControlFlow<Self::Output> {
        movetext.visit_outcome(outcome);
        ControlFlow::Continue(())
    }

    fn end_game(&mut self, movetext: Self::Movetext) -> Self::Output {
        Ok(movetext)
    }
}
