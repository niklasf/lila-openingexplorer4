use std::{io, ops::ControlFlow};

use pgn_reader::{RawTag, Skip, Visitor};
use shakmaty::{
    ByColor, CastlingMode, EnPassantMode, Outcome, Position,
    fen::Fen,
    packed::PackedSetup,
    san::{San, SanPlus},
    variant::{Variant, VariantPosition},
};

use crate::packed::{
    GameId, GamePlayer, LaxDate, LichessGame, PackedMoves, Rating, RatingMode, Speed,
};

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
            b"WhiteElo" if value.as_bytes() != b"?" => {
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
            b"BlackElo" if value.as_bytes() != b"?" => {
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
            b"WhiteTitle" if !value.as_bytes().is_empty() => {
                self.white_title = Some(String::from_utf8(value.as_bytes().to_owned()).map_err(
                    |err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid WhiteTitle tag: {} @ {:?}", err, self.game_id),
                        )
                    },
                )?);
            }
            b"BlackTitle" if !value.as_bytes().is_empty() => {
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

    pub fn game_id(&self) -> Option<GameId> {
        self.game_id
    }

    pub fn plies(&self) -> usize {
        self.moves.len()
    }

    pub fn outcome(&self) -> Outcome {
        self.outcome.unwrap_or(Outcome::Unknown)
    }

    fn initial_position(&self) -> io::Result<(VariantPosition, Option<PackedSetup>)> {
        let variant = self.variant.unwrap_or_default();

        let (pos, need_setup) = match self.fen.clone() {
            None => (VariantPosition::new(variant), variant != Variant::Chess),
            Some(fen) => (
                VariantPosition::from_setup(variant, fen.into(), CastlingMode::Chess960).map_err(
                    |err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid setup: {} @ {:?}", err, self.game_id),
                        )
                    },
                )?,
                true,
            ),
        };

        let setup = need_setup
            .then(|| {
                // Legal, but can still be unrepresentable, e.g., more than 15
                // pieces of a kind in a crazyhouse pocket.
                PackedSetup::pack_variant_normalized(&pos.to_setup(EnPassantMode::Legal), variant)
                    .map_err(|err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("unsupported setup: {} @ {:?}", err, self.game_id),
                        )
                    })
            })
            .transpose()?;

        Ok((pos, setup))
    }

    pub fn into_game(self) -> io::Result<(GameId, LichessGame)> {
        let game_id = self
            .game_id
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing game id"))?;

        let outcome = self.outcome().known().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown outcome @ {:?}", game_id),
            )
        })?;

        let (mut pos, setup) = self.initial_position()?;

        let mut encoder = PackedMoves::encoder_with_capacity(self.moves.len());

        for san in &self.moves {
            let m = san.to_move(&pos).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("illegal san {}: {} @ {:?}", san, err, game_id),
                )
            })?;

            encoder.encode_next(&pos, m);
            pos.play_unchecked(m);
        }

        let moves = encoder.finish();

        let game = LichessGame {
            players: ByColor {
                white: GamePlayer {
                    title: self.white_title,
                    name: self.white.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("missing white name @ {:?}", game_id),
                        )
                    })?,
                    rating: self.white_elo,
                },
                black: GamePlayer {
                    title: self.black_title,
                    name: self.black.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("missing black name @ {:?}", game_id),
                        )
                    })?,
                    rating: self.black_elo,
                },
            },
            date: self.date.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("missing date @ {:?}", game_id),
                )
            })?,
            outcome,
            speed: self.speed,
            rating_mode: RatingMode::Rated, // always rated for monthly export
            setup,
            event: None,
            moves,
        };

        Ok((game_id, game))
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
