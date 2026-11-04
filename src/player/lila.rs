//! Client of lila's games export. Holds the limit of parallel streams: every
//! request to lila goes through a `Slot`, and at most `streams` slots exist
//! at once.

use std::{fs, io, path::PathBuf, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use serde::Deserialize;
use shakmaty::{
    ByColor, CastlingMode, Color, KnownOutcome, Position, PositionError,
    fen::Fen,
    san::San,
    variant::{Variant, VariantPosition},
};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{
    packed::{GameId, PackedMoves, Rating, RatingMode, Speed},
    player::{UserId, record::PlayerGame},
    util::civil::month_of_ms,
};

#[derive(Debug, Clone)]
pub struct LilaClientConfig {
    /// Base url, like `https://lichess.org`.
    pub url: String,
    pub bearer: Option<String>,
    /// File with the bearer token, read before each request.
    pub bearer_file: Option<PathBuf>,
    /// Parallel streams at most.
    pub streams: usize,
    pub connect_timeout: Duration,
    /// Longest wait for the response headers and between chunks of the body.
    pub read_timeout: Duration,
}

impl Default for LilaClientConfig {
    fn default() -> LilaClientConfig {
        LilaClientConfig {
            url: "https://lichess.org".to_owned(),
            bearer: None,
            bearer_file: None,
            streams: 8,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Error)]
pub enum LilaError {
    #[error("not found")]
    NotFound,
    #[error("request: {0}")]
    Request(#[from] reqwest::Error),
    #[error("status {0}")]
    Status(reqwest::StatusCode),
    #[error("timeout")]
    Timeout,
    #[error("bearer file: {0}")]
    BearerFile(#[from] io::Error),
}

pub struct LilaClient {
    client: reqwest::Client,
    config: LilaClientConfig,
    semaphore: Arc<Semaphore>,
}

impl LilaClient {
    pub fn new(config: LilaClientConfig) -> LilaClient {
        LilaClient {
            client: reqwest::Client::builder()
                .user_agent("lila-openingexplorer/4")
                .connect_timeout(config.connect_timeout)
                .read_timeout(config.read_timeout)
                .build()
                .expect("reqwest client"),
            semaphore: Arc::new(Semaphore::new(config.streams)),
            config,
        }
    }

    /// Waits for a free slot.
    pub async fn slot(self: &Arc<Self>) -> Slot {
        Slot {
            _permit: Arc::clone(&self.semaphore)
                .acquire_owned()
                .await
                .expect("semaphore not closed"),
            client: Arc::clone(self),
        }
    }

    /// Slots in use.
    pub fn busy(&self) -> usize {
        self.config.streams - self.semaphore.available_permits()
    }
}

/// Permission to have one stream from lila open. Requests borrow the slot
/// for the lifetime of their stream, so one slot never has two.
pub struct Slot {
    _permit: OwnedSemaphorePermit,
    client: Arc<LilaClient>,
}

impl Slot {
    /// Requests the games of a user created since a time (ms), oldest
    /// first, including ongoing games.
    pub async fn user_games(
        &mut self,
        user: &UserId,
        since: u64,
    ) -> Result<GameStream<'_>, LilaError> {
        let config = &self.client.config;
        let mut request = self
            .client
            .client
            .get(format!(
                "{}/api/games/user/{}?sort=dateAsc&ongoing=true&since={since}",
                config.url,
                user.as_str()
            ))
            .header("Accept", "application/x-ndjson");
        if let Some(bearer) = &config.bearer {
            request = request.bearer_auth(bearer);
        } else if let Some(file) = &config.bearer_file {
            request = request.bearer_auth(fs::read_to_string(file)?.trim());
        }
        let response = tokio::time::timeout(config.read_timeout, request.send())
            .await
            .map_err(|_| LilaError::Timeout)??;
        match response.status() {
            reqwest::StatusCode::NOT_FOUND => return Err(LilaError::NotFound),
            status if !status.is_success() => return Err(LilaError::Status(status)),
            _ => (),
        }
        Ok(GameStream {
            response,
            buf: BytesMut::new(),
            done: false,
            _slot: self,
        })
    }
}

/// The lines of a games export.
pub struct GameStream<'a> {
    response: reqwest::Response,
    buf: BytesMut,
    done: bool,
    _slot: &'a mut Slot,
}

impl GameStream<'_> {
    /// The next line (without the newline), or `None` at the end.
    pub async fn next_line(&mut self) -> Result<Option<Bytes>, LilaError> {
        loop {
            if let Some(i) = self.buf.iter().position(|&c| c == b'\n') {
                let line = self.buf.split_to(i + 1).freeze();
                return Ok(Some(line.slice(..i)));
            }
            if self.done {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                let line = self.buf.split().freeze();
                return Ok(Some(line));
            }
            match self.response.chunk().await? {
                Some(chunk) => self.buf.extend_from_slice(&chunk),
                None => self.done = true,
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ExportUser {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ExportPlayer {
    #[serde(default)]
    pub user: Option<ExportUser>,
    #[serde(default)]
    pub rating: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct ExportPlayers {
    pub white: ExportPlayer,
    pub black: ExportPlayer,
}

/// A game of lila's export, as far as it is needed.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportGame {
    pub id: String,
    pub rated: bool,
    pub variant: String,
    pub speed: String,
    pub created_at: u64,
    pub last_move_at: u64,
    pub status: String,
    pub players: ExportPlayers,
    #[serde(default)]
    pub winner: Option<String>,
    #[serde(default)]
    pub moves: String,
    #[serde(default)]
    pub initial_fen: Option<String>,
}

/// Why a game of the export is not indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Not finished yet: to be revisited.
    Ongoing,
    /// Aborted, never started, or ended without result.
    Unfinished,
    /// A side has no account or no rating.
    Anonymous,
    /// The player is neither side.
    NotPlayer,
    Invalid,
}

/// A game of the export, classified.
pub enum Classified {
    Game {
        game: Box<PlayerGame>,
        /// The name of the player as in the game.
        name: String,
    },
    Skip(Skip),
}

fn variant(key: &str) -> Option<Variant> {
    Some(match key {
        "standard" | "chess960" | "fromPosition" => Variant::Chess,
        "antichess" => Variant::Antichess,
        "atomic" => Variant::Atomic,
        "crazyhouse" => Variant::Crazyhouse,
        "horde" => Variant::Horde,
        "kingOfTheHill" => Variant::KingOfTheHill,
        "racingKings" => Variant::RacingKings,
        "threeCheck" => Variant::ThreeCheck,
        _ => return None,
    })
}

impl ExportGame {
    pub fn parse(line: &[u8]) -> Result<ExportGame, serde_json::Error> {
        serde_json::from_slice(line)
    }

    /// Classifies the game, replaying its first `max_ply` plies for a
    /// `PlayerGame` of the user.
    pub fn classify(&self, user: &UserId, max_ply: usize) -> Classified {
        match self.status.as_str() {
            "created" | "started" => return Classified::Skip(Skip::Ongoing),
            "aborted" | "noStart" | "unknownFinish" => {
                return Classified::Skip(Skip::Unfinished);
            }
            _ => (),
        }
        let players = ByColor {
            white: &self.players.white,
            black: &self.players.black,
        };
        if players
            .iter()
            .any(|p| p.user.is_none() || p.rating.is_none())
        {
            return Classified::Skip(Skip::Anonymous);
        }
        let Some(color) = Color::ALL.into_iter().find(|&color| {
            players
                .get(color)
                .user
                .as_ref()
                .is_some_and(|u| user.is_name(&u.name))
        }) else {
            return Classified::Skip(Skip::NotPlayer);
        };
        let (Ok(id), Some(variant), Ok(speed)) = (
            GameId::from_ascii(self.id.as_bytes()),
            variant(&self.variant),
            Speed::from_ascii(self.speed.as_bytes()),
        ) else {
            return Classified::Skip(Skip::Invalid);
        };
        let rating = |color: Color| {
            players
                .get(color)
                .rating
                .and_then(|r| Rating::try_from(i32::from(r)).ok())
        };
        let (Some(player_rating), Some(opponent_rating)) = (rating(color), rating(!color)) else {
            return Classified::Skip(Skip::Invalid);
        };
        let mut pos = match &self.initial_fen {
            Some(fen) => {
                let Some(pos) = fen.parse::<Fen>().ok().and_then(|fen| {
                    VariantPosition::from_setup(variant, fen.into_setup(), CastlingMode::Chess960)
                        .or_else(PositionError::ignore_invalid_castling_rights)
                        .or_else(PositionError::ignore_invalid_ep_square)
                        .ok()
                }) else {
                    return Classified::Skip(Skip::Invalid);
                };
                pos
            }
            None => VariantPosition::new(variant),
        };
        let setup = PlayerGame::setup_of(&pos);
        let mut moves = PackedMoves::encoder_with_capacity(max_ply);
        for san in self.moves.split_ascii_whitespace().take(max_ply) {
            // Cut off at the first illegal move, as before.
            let Ok(m) = san
                .parse::<San>()
                .map_err(|_| ())
                .and_then(|san| san.to_move(&pos).map_err(|_| ()))
            else {
                break;
            };
            moves.encode_next(&pos, m);
            pos.play_unchecked(m);
        }
        let opponent = players.get(!color).user.as_ref().expect("opponent account");
        let me = players.get(color).user.as_ref().expect("own account");
        Classified::Game {
            game: Box::new(PlayerGame {
                id,
                speed,
                mode: if self.rated {
                    RatingMode::Rated
                } else {
                    RatingMode::Unrated
                },
                month: month_of_ms(self.last_move_at),
                outcome: match self.winner.as_deref() {
                    Some("white") => KnownOutcome::Decisive {
                        winner: Color::White,
                    },
                    Some("black") => KnownOutcome::Decisive {
                        winner: Color::Black,
                    },
                    _ => KnownOutcome::Draw,
                },
                color,
                player_rating,
                opponent_rating,
                opponent_name: opponent.name.clone(),
                opponent_title: opponent.title.clone(),
                player_title: me.title.clone(),
                setup,
                moves: moves.finish(),
            }),
            name: me.name.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify() {
        let line = br#"{"id":"oV2tflO2","rated":false,"variant":"standard","speed":"rapid","perf":"rapid","createdAt":1651131730149,"lastMoveAt":1651131935030,"status":"mate","players":{"white":{"user":{"name":"revoof","patron":true,"id":"revoof"},"rating":1887,"provisional":true},"black":{"user":{"name":"maia1","title":"BOT","id":"maia1"},"rating":1417}},"winner":"white","moves":"d4 d5 c4 dxc4 Nc3 e6 e4 Nc6 Nf3 Bb4 Bxc4 Bxc3+ bxc3 Nf6 Bd3 O-O O-O b6 Bg5 Bb7 e5 h6 Bh4 g5 Nxg5 hxg5 Bxg5 Qd5 c4 Qxd4 Bxf6 Nxe5 Qh5 Ng6 Bxg6 fxg6 Qxg6#","clock":{"initial":600,"increment":0,"totalTime":600}}"#;
        let game = ExportGame::parse(line).unwrap();
        let Classified::Game { game, name } =
            game.classify(&UserId::from_name("Maia1").unwrap(), 50)
        else {
            panic!("expected game");
        };
        assert_eq!(name, "maia1");
        assert_eq!(game.color, Color::Black);
        assert_eq!(game.mode, RatingMode::Unrated);
        assert_eq!(game.speed, Speed::Rapid);
        assert_eq!(game.month.to_string(), "2022-04");
        assert_eq!(game.opponent_name, "revoof");
        assert_eq!(game.player_title.as_deref(), Some("BOT"));
        assert_eq!(i32::from(game.opponent_rating), 1887);
        assert_eq!(setup_plies(&game), 37);

        let game = ExportGame::parse(line).unwrap();
        assert!(matches!(
            game.classify(&UserId::from_name("someone").unwrap(), 50),
            Classified::Skip(Skip::NotPlayer)
        ));
        let mut game = ExportGame::parse(line).unwrap();
        game.status = "started".to_owned();
        assert!(matches!(
            game.classify(&UserId::from_name("revoof").unwrap(), 50),
            Classified::Skip(Skip::Ongoing)
        ));
        // Cut off at an illegal move.
        let mut game = ExportGame::parse(line).unwrap();
        game.moves = "d4 d5 O-O-O c4".to_owned();
        let Classified::Game { game, .. } =
            game.classify(&UserId::from_name("revoof").unwrap(), 50)
        else {
            panic!("expected game");
        };
        assert_eq!(setup_plies(&game), 2);
    }

    fn setup_plies(game: &PlayerGame) -> usize {
        let pos = game.as_borrowed().initial_position();
        let mut decoder = game.moves.decode().unwrap();
        let mut pos = pos;
        let mut plies = 0;
        while let Some(m) = decoder.decode_next(&pos).unwrap() {
            pos.play_unchecked(m);
            plies += 1;
        }
        plies
    }
}
