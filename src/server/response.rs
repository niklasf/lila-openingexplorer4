use serde::Serialize;
use shakmaty::Color;

use crate::{
    packed::{BorrowedLichessGame, GameId, Month, SubStats},
    server::openings::Opening,
};

#[derive(Serialize, Debug, Clone)]
pub struct GamePlayer {
    pub name: String,
    pub rating: Option<i32>,
}

#[derive(Serialize, Debug, Clone)]
pub struct ExplorerGame {
    pub id: String,
    pub winner: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<&'static str>,
    pub black: GamePlayer,
    pub white: GamePlayer,
    pub year: u32,
    pub month: Option<String>,
}

impl ExplorerGame {
    pub fn new(id: GameId, game: &BorrowedLichessGame<'_>, masters: bool) -> ExplorerGame {
        let player = |color: Color| {
            let player = game.players[color];
            GamePlayer {
                name: player.name.to_owned(),
                rating: player.rating.map(i32::from),
            }
        };
        ExplorerGame {
            id: id.to_string(),
            winner: match game.outcome.winner() {
                Some(Color::White) => Some("white"),
                Some(Color::Black) => Some("black"),
                None => None,
            },
            speed: if masters {
                None
            } else {
                game.speed.map(|speed| speed.as_str())
            },
            mode: (!masters).then(|| game.rating_mode.as_str()),
            black: player(Color::Black),
            white: player(Color::White),
            year: game.date.year(),
            month: if masters {
                game.date
                    .month()
                    .map(|m| Month::from_year_and_month(game.date.year() as u16, u16::from(m)))
                    .and_then(Result::ok)
                    .map(|m| m.to_string())
            } else {
                Some(game.date.earliest_month().to_string())
            },
        }
    }
}

#[derive(Serialize, Debug, Clone)]
pub struct ExplorerGameWithUci {
    pub uci: String,
    #[serde(flatten)]
    pub game: ExplorerGame,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct Stats {
    pub white: u64,
    pub draws: u64,
    pub black: u64,
}

impl From<&SubStats> for Stats {
    fn from(stats: &SubStats) -> Stats {
        Stats {
            white: stats.white,
            draws: stats.draws,
            black: stats.black,
        }
    }
}

/// Average rating of the games, rounded.
pub fn average_rating(stats: &SubStats) -> Option<i32> {
    let games = stats.games();
    (games > 0).then(|| (1500.0 + stats.rating_offset_sum as f64 / games as f64).round() as i32)
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ExplorerMove {
    pub uci: String,
    pub san: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_rating: Option<i32>,
    #[serde(flatten)]
    pub stats: Stats,
    pub game: Option<ExplorerGame>,
    pub opening: Option<Opening>,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub month: String,
    #[serde(flatten)]
    pub stats: Stats,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ExplorerResponse {
    #[serde(flatten)]
    pub total: Stats,
    pub moves: Vec<ExplorerMove>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_games: Option<Vec<ExplorerGameWithUci>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_games: Option<Vec<ExplorerGameWithUci>>,
    pub opening: Option<Opening>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<HistoryEntry>>,
}
