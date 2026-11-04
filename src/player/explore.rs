//! Responses of `/player` from the entry of a position.

use std::cmp::Reverse;

use serde::Serialize;
use serde_with::{DisplayFromStr, serde_as};
use shakmaty::{Color, packed::PackedUciMove, san::SanPlus, uci::UciMove};

use crate::{
    packed::{EntryView, GameId, Month, PlayerGroupHeader, Rating, Speed, SubStats},
    player::{
        keys::GameSeq,
        query::{Mode, PlayerFilter, PlayerLimits},
        record::PlayerGame,
    },
    server::{openings::Opening, response::Stats},
};

/// Statistics of a move, aggregated over the matching cells.
#[derive(Debug, Clone)]
pub struct PreparedMove {
    pub m: PackedUciMove,
    pub stats: SubStats,
    /// The only game, if the move was played exactly once.
    pub game: Option<GameSeq>,
}

#[derive(Debug, Clone, Default)]
pub struct Prepared {
    pub total: SubStats,
    /// Most played first.
    pub moves: Vec<PreparedMove>,
    /// Newest first.
    pub recent_games: Vec<(PackedUciMove, GameSeq)>,
}

/// Aggregates a packed `PlayerEntry` with the filter.
pub fn prepare(entry: &[u8], filter: &PlayerFilter, limits: &PlayerLimits) -> Prepared {
    let mut prepared = Prepared::default();
    let mut recent: Vec<(GameSeq, PackedUciMove)> = Vec::new();
    let mut current: Option<(PreparedMove, Option<GameSeq>)> = None;
    let finish = |current: Option<(PreparedMove, Option<GameSeq>)>, prepared: &mut Prepared| {
        if let Some((mut m, latest)) = current
            && m.stats.games() > 0
        {
            m.game = latest.filter(|_| m.stats.games() == 1);
            prepared.total.merge(&m.stats);
            prepared.moves.push(m);
        }
    };
    for group in EntryView::<PlayerGroupHeader>::unpack_entire(entry).groups() {
        if current
            .as_ref()
            .is_none_or(|(m, _)| m.m != group.uci_move())
        {
            finish(current.take(), &mut prepared);
            current = Some((
                PreparedMove {
                    m: group.uci_move(),
                    stats: SubStats::default(),
                    game: None,
                },
                None,
            ));
        }
        if !filter.contains_speed(group.header.speed) || !filter.contains_mode(group.header.mode) {
            continue;
        }
        let (m, latest) = current.as_mut().expect("current move");
        let mut cells = group.cells();
        while let Some(cell) = cells.next_cell() {
            let month = cell.key.month();
            if filter.since.is_some_and(|since| month < since) {
                // Months are newest first.
                break;
            }
            if !filter.contains_month(month) {
                continue;
            }
            m.stats.merge(&cell.stats);
            while let Some(game) = cells.next_game() {
                let seq = GameSeq(game.get() as u32);
                *latest = (*latest).max(Some(seq));
                recent.push((seq, m.m));
            }
        }
    }
    finish(current.take(), &mut prepared);

    prepared.moves.sort_by_key(|m| Reverse(m.stats.games()));
    prepared.moves.truncate(limits.moves);
    recent.sort_unstable_by_key(|&(seq, _)| Reverse(seq));
    recent.truncate(limits.recent_games);
    prepared.recent_games = recent.into_iter().map(|(seq, m)| (m, seq)).collect();
    prepared
}

/// Performance rating from the point of view of `color`, after
/// <https://handbook.fide.com/chapter/B022017>, as before.
pub fn performance(stats: &SubStats, color: Color) -> Option<i32> {
    const DELTAS: [f64; 101] = [
        -800.0, -677.0, -589.0, -538.0, -501.0, -470.0, -444.0, -422.0, -401.0, -383.0, -366.0,
        -351.0, -336.0, -322.0, -309.0, -296.0, -284.0, -273.0, -262.0, -251.0, -240.0, -230.0,
        -220.0, -211.0, -202.0, -193.0, -184.0, -175.0, -166.0, -158.0, -149.0, -141.0, -133.0,
        -125.0, -117.0, -110.0, -102.0, -95.0, -87.0, -80.0, -72.0, -65.0, -57.0, -50.0, -43.0,
        -36.0, -29.0, -21.0, -14.0, -7.0, 0.0, 7.0, 14.0, 21.0, 29.0, 36.0, 43.0, 50.0, 57.0, 65.0,
        72.0, 80.0, 87.0, 95.0, 102.0, 110.0, 117.0, 125.0, 133.0, 141.0, 149.0, 158.0, 166.0,
        175.0, 184.0, 193.0, 202.0, 211.0, 220.0, 230.0, 240.0, 251.0, 262.0, 273.0, 284.0, 296.0,
        309.0, 322.0, 336.0, 351.0, 366.0, 383.0, 401.0, 422.0, 444.0, 470.0, 501.0, 538.0, 589.0,
        677.0, 800.0,
    ];
    let games = stats.games();
    if games == 0 {
        return None;
    }
    // Exactly as before (sum of absolute ratings over games), so that
    // rounding at .5 agrees.
    let rating_sum = stats.rating_offset_sum + 1500 * games as i64;
    let average = rating_sum as f64 / games as f64;
    let wins = match color {
        Color::White => stats.white,
        Color::Black => stats.black,
    };
    let score = 100 * wins + 50 * stats.draws;
    let p = score as f64 / games as f64;
    let idx = p.trunc() as usize;
    Some(
        (average
            + DELTAS[idx] * (1.0 - p.fract())
            + DELTAS.get(idx + 1).copied().unwrap_or(800.0) * p.fract())
        .round() as i32,
    )
}

#[derive(Serialize, Debug, Clone)]
pub struct PlayerGamePlayer {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub rating: i32,
}

#[serde_as]
#[derive(Serialize, Debug, Clone)]
pub struct PlayerGameResponse {
    #[serde_as(as = "DisplayFromStr")]
    pub id: GameId,
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub winner: Option<Color>,
    #[serde_as(as = "DisplayFromStr")]
    pub speed: Speed,
    #[serde_as(as = "DisplayFromStr")]
    pub mode: Mode,
    pub white: PlayerGamePlayer,
    pub black: PlayerGamePlayer,
    pub year: u32,
    #[serde_as(as = "DisplayFromStr")]
    pub month: Month,
}

impl PlayerGameResponse {
    /// From a record of the player with the (current) name.
    pub fn new(game: &PlayerGame, name: &str) -> PlayerGameResponse {
        let me = PlayerGamePlayer {
            name: name.to_owned(),
            title: game.player_title.clone(),
            rating: i32::from(game.player_rating),
        };
        let opponent = PlayerGamePlayer {
            name: game.opponent_name.clone(),
            title: game.opponent_title.clone(),
            rating: i32::from(game.opponent_rating),
        };
        let (white, black) = match game.color {
            Color::White => (me, opponent),
            Color::Black => (opponent, me),
        };
        PlayerGameResponse {
            id: game.id,
            winner: game.outcome.winner(),
            speed: game.speed,
            mode: Mode(game.mode),
            white,
            black,
            year: u32::from(game.month.months_since_jan_1900() / 12) + 1900,
            month: game.month,
        }
    }
}

#[serde_as]
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlayerMove {
    #[serde_as(as = "DisplayFromStr")]
    pub uci: UciMove,
    #[serde_as(as = "DisplayFromStr")]
    pub san: SanPlus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_opponent_rating: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub performance: Option<i32>,
    #[serde(flatten)]
    pub stats: Stats,
    pub game: Option<PlayerGameResponse>,
    pub opening: Option<Opening>,
}

#[serde_as]
#[derive(Serialize, Debug, Clone)]
pub struct PlayerGameWithUci {
    #[serde_as(as = "DisplayFromStr")]
    pub uci: UciMove,
    #[serde(flatten)]
    pub game: PlayerGameResponse,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlayerResponse {
    pub opening: Option<Opening>,
    pub queue_position: u64,
    #[serde(flatten)]
    pub total: Stats,
    pub moves: Vec<PlayerMove>,
    pub recent_games: Vec<PlayerGameWithUci>,
}

impl PlayerResponse {
    pub fn total_games(&self) -> u64 {
        self.total.white + self.total.draws + self.total.black
    }
}

/// Average rating of the stats, as JSON number.
pub fn average_rating(stats: &SubStats) -> Option<i32> {
    stats.average_rating().map(|r: Rating| i32::from(r))
}

#[cfg(test)]
mod tests {
    use shakmaty::KnownOutcome;

    use super::*;
    use crate::packed::{CellKey, GameIndex, PlayerEntry, RatingMode};

    #[test]
    fn test_prepare() {
        let mut entry = PlayerEntry::new();
        let e4 = PackedUciMove::pack("e2e4".parse().unwrap());
        let d4 = PackedUciMove::pack("d2d4".parse().unwrap());
        let blitz = PlayerGroupHeader {
            speed: Speed::Blitz,
            mode: RatingMode::Rated,
        };
        let casual = PlayerGroupHeader {
            speed: Speed::Blitz,
            mode: RatingMode::Unrated,
        };
        let month = |m| Month::from_months_since_jan_1900(1400 + m);
        let rating = Rating::try_from(1600).unwrap();
        let win = KnownOutcome::Decisive {
            winner: Color::White,
        };
        for seq in 0..10u64 {
            entry.add_game(
                CellKey::new(e4, blitz, month((seq % 3) as u16)),
                rating,
                win,
                GameIndex::new(seq).unwrap(),
            );
        }
        entry.add_game(
            CellKey::new(d4, casual, month(5)),
            rating,
            KnownOutcome::Draw,
            GameIndex::new(10).unwrap(),
        );
        let mut buf = Vec::new();
        entry.pack_entire(&mut buf);
        let limits = PlayerLimits {
            moves: usize::MAX,
            recent_games: 8,
        };
        let prepared = prepare(&buf, &PlayerFilter::default(), &limits);
        assert_eq!(prepared.total.games(), 11);
        assert_eq!(prepared.moves[0].m, e4);
        assert_eq!(prepared.moves[0].game, None);
        assert_eq!(prepared.moves[1].game, Some(GameSeq(10)));
        assert_eq!(prepared.recent_games.len(), 8);
        assert_eq!(prepared.recent_games[0], (d4, GameSeq(10)));
        assert_eq!(prepared.recent_games[1], (e4, GameSeq(9)));
        assert_eq!(average_rating(&prepared.moves[0].stats), Some(1600));
        assert_eq!(
            performance(&prepared.moves[0].stats, Color::White),
            Some(2400)
        );
        assert_eq!(
            performance(&prepared.moves[0].stats, Color::Black),
            Some(800)
        );

        let rated = PlayerFilter {
            modes: Some((true, false)),
            since: Some(month(1)),
            ..PlayerFilter::default()
        };
        let prepared = prepare(&buf, &rated, &limits);
        // Months 1 and 2 of e4: seqs 1, 2, 4, 5, 7, 8.
        assert_eq!(prepared.total.games(), 6);
        assert_eq!(prepared.moves.len(), 1);
        assert_eq!(prepared.recent_games[0], (e4, GameSeq(8)));
    }
}
