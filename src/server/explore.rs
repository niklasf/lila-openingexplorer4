use std::{
    cmp::{Reverse, min},
    collections::BTreeMap,
};

use shakmaty::packed::PackedUciMove;

use crate::{
    packed::{EntryView, GameId, Month, RatingGroup, Speed, SubStats},
    server::query::{LichessFilter, Limits},
};

/// Most games referenced by a lichess response (top and recent games).
pub const MAX_LICHESS_GAMES: usize = 8;
/// Most top games of a lichess response.
pub const MAX_TOP_GAMES: usize = 4;
/// Most top games of a masters response.
pub const MAX_MASTERS_GAMES: usize = 15;

/// Statistics of a move, aggregated over the matching cells.
#[derive(Debug, Clone)]
pub struct PreparedMove {
    pub m: PackedUciMove,
    pub stats: SubStats,
    /// The only game, if the move was played exactly once.
    pub game: Option<GameId>,
    latest: Option<(u64, GameId)>,
}

/// A game referenced by an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameRef {
    pub group: RatingGroup,
    pub speed: Option<Speed>,
    /// Chronological rank within the entry.
    pub rank: u64,
    pub m: PackedUciMove,
    pub id: GameId,
}

#[derive(Debug, Clone)]
pub struct HistorySegment {
    pub month: Month,
    pub stats: SubStats,
}

#[derive(Debug, Default)]
struct Aggregate {
    total: SubStats,
    moves: Vec<PreparedMove>,
    games: Vec<GameRef>,
    months: BTreeMap<Month, SubStats>,
}

/// Aggregates the cells of an entry that match the filter, in canonical
/// order.
fn aggregate(
    entry: EntryView<'_>,
    filter: &LichessFilter,
    collect_games: bool,
    collect_months: bool,
) -> Aggregate {
    let mut agg = Aggregate::default();
    for cell in entry.cells() {
        let header = cell.key.group_header();
        let month = cell.key.month();
        if !filter.contains_speed(header.speed)
            || !filter.contains_rating_group(header.rating_group)
            || !filter.contains_month(month)
        {
            continue;
        }

        let m = cell.key.uci_move();
        let current = match agg.moves.last_mut() {
            Some(current) if current.m == m => current,
            _ => {
                agg.moves.push(PreparedMove {
                    m,
                    stats: SubStats::default(),
                    game: None,
                    latest: None,
                });
                agg.moves.last_mut().expect("just pushed")
            }
        };
        current.stats.merge(&cell.stats);
        agg.total.merge(&cell.stats);
        if collect_months {
            agg.months.entry(month).or_default().merge(&cell.stats);
        }
        if collect_games {
            for (rank, id) in cell.games {
                if current.latest.is_none_or(|(latest, _)| latest < rank) {
                    current.latest = Some((rank, id));
                }
                agg.games.push(GameRef {
                    group: header.rating_group,
                    speed: header.speed,
                    rank,
                    m,
                    id,
                });
            }
        }
    }
    for pm in &mut agg.moves {
        pm.game = pm
            .latest
            .filter(|_| pm.stats.games() == 1)
            .map(|(_, id)| id);
    }
    agg
}

fn sort_by_key_and_truncate<T, K: Ord>(vec: &mut Vec<T>, num: usize, f: impl FnMut(&T) -> K) {
    vec.sort_by_cached_key(f);
    vec.truncate(num);
}

/// Highest rating ever reached at the speed (as of 2024-07-27, like the
/// previous explorer), to rank top games across speeds.
fn highscore(speed: Option<Speed>) -> i32 {
    match speed {
        Some(Speed::UltraBullet) => 2644,
        Some(Speed::Bullet) => 3352,
        Some(Speed::Blitz) => 2974,
        Some(Speed::Rapid) => 2949,
        Some(Speed::Classical) => 2533,
        Some(Speed::Correspondence) | None => 4000,
    }
}

fn lower_bound(group: RatingGroup) -> i32 {
    match group {
        RatingGroup::Low => 0,
        RatingGroup::Group1000 => 1000,
        RatingGroup::Group1200 => 1200,
        RatingGroup::Group1400 => 1400,
        RatingGroup::Group1600 => 1600,
        RatingGroup::Group1800 => 1800,
        RatingGroup::Group2000 => 2000,
        RatingGroup::Group2200 => 2200,
        RatingGroup::Group2500 => 2500,
        RatingGroup::Group2800 => 2800,
        RatingGroup::Group3200 => 3200,
    }
}

#[derive(Debug)]
pub struct LichessPrepared {
    pub total: SubStats,
    pub moves: Vec<PreparedMove>,
    pub top_games: Vec<GameRef>,
    pub recent_games: Vec<GameRef>,
    pub history: Option<Vec<HistorySegment>>,
}

pub fn prepare_lichess(
    entry: EntryView<'_>,
    filter: &LichessFilter,
    limits: &Limits,
    history: bool,
) -> LichessPrepared {
    let agg = aggregate(entry, filter, limits.games_wanted(), history);
    let mut moves = agg.moves;
    sort_by_key_and_truncate(&mut moves, limits.moves, |pm| Reverse(pm.stats.games()));

    let games = agg.games;
    let (mut top_games, mut recent_games) = match filter.top_group() {
        Some(top_group) => {
            let mut top_games: Vec<GameRef> = games
                .iter()
                .copied()
                .filter(|g| g.group >= top_group)
                .collect();
            sort_by_key_and_truncate(&mut top_games, MAX_TOP_GAMES, |g| {
                (highscore(g.speed) - lower_bound(g.group), Reverse(g.rank))
            });
            let recent_games = games
                .into_iter()
                .filter(|g| !top_games.iter().any(|t| t.id == g.id))
                .collect();
            (top_games, recent_games)
        }
        None => (Vec::new(), games),
    };
    let valid_recent_games = MAX_LICHESS_GAMES - top_games.len();
    top_games.truncate(limits.top_games);
    sort_by_key_and_truncate(
        &mut recent_games,
        min(valid_recent_games, limits.recent_games),
        |g| Reverse(g.rank),
    );

    LichessPrepared {
        total: agg.total,
        moves,
        top_games,
        recent_games,
        history: history.then(|| build_history(agg.months, filter)),
    }
}

/// Monthly statistics from the first month (or `since`) to the last month,
/// with gaps filled. Without `until` the last month is omitted, because it
/// may not be completely indexed.
fn build_history(months: BTreeMap<Month, SubStats>, filter: &LichessFilter) -> Vec<HistorySegment> {
    let mut segments = Vec::with_capacity(months.len());
    let mut next = filter.since;
    for (month, stats) in months {
        if let Some(mut gap) = next {
            while gap < month {
                segments.push(HistorySegment {
                    month: gap,
                    stats: SubStats::default(),
                });
                gap = Month::from_months_since_jan_1900(gap.months_since_jan_1900() + 1);
            }
        }
        segments.push(HistorySegment { month, stats });
        next = Some(Month::from_months_since_jan_1900(
            month.months_since_jan_1900() + 1,
        ));
    }
    if filter.until.is_none() {
        segments.pop();
    }
    segments
}

#[derive(Debug)]
pub struct MastersPrepared {
    pub total: SubStats,
    pub moves: Vec<PreparedMove>,
    /// Candidates for the top games, best groups and most recent first.
    pub candidates: Vec<GameRef>,
}

/// Aggregates a masters entry. Top games are the highest rated ones, but
/// entries only keep the most recent games of each cell, so the candidates
/// are the most recent games of the highest rating groups, up to
/// `max_candidates`, to be ranked by their actual ratings.
pub fn prepare_masters(
    entry: EntryView<'_>,
    filter: &LichessFilter,
    limits: &Limits,
    max_candidates: usize,
) -> MastersPrepared {
    let agg = aggregate(entry, filter, limits.top_games > 0, false);
    let mut moves = agg.moves;
    sort_by_key_and_truncate(&mut moves, limits.moves, |pm| Reverse(pm.stats.games()));
    let mut candidates = agg.games;
    sort_by_key_and_truncate(&mut candidates, max_candidates, |g| {
        (Reverse(g.group), Reverse(g.rank))
    });
    MastersPrepared {
        total: agg.total,
        moves,
        candidates,
    }
}

#[cfg(test)]
mod tests {
    use shakmaty::{Color, KnownOutcome, uci::UciMove};

    use super::*;
    use crate::packed::{CellKey, GameSeq, GroupHeader, LichessEntry, Rating};

    fn id(n: u64) -> GameId {
        GameId::unpack(&mut &n.to_be_bytes()[2..])
    }

    fn entry() -> Vec<u8> {
        let mut entry = LichessEntry::new();
        let mut seq = 0;
        let mut add = |uci: &str, speed: Speed, rating: i32, outcome: KnownOutcome, month: u16| {
            let rating = Rating::try_from(rating).unwrap();
            seq += 1;
            entry.add_game(
                CellKey::new(
                    PackedUciMove::pack(uci.parse::<UciMove>().unwrap()),
                    GroupHeader {
                        speed: Some(speed),
                        rating_group: RatingGroup::select(rating),
                    },
                    Month::from_year_and_month(2020, month).unwrap(),
                ),
                rating,
                outcome,
                GameSeq(seq),
                id(seq),
            );
        };
        let white = KnownOutcome::Decisive {
            winner: Color::White,
        };
        add("e2e4", Speed::Blitz, 1500, white, 1);
        add("e2e4", Speed::Blitz, 1600, KnownOutcome::Draw, 2);
        add("e2e4", Speed::Bullet, 2600, white, 3);
        add("d2d4", Speed::Rapid, 2100, white, 3);
        let mut buf = Vec::new();
        entry.pack_entire(&mut buf);
        buf
    }

    fn all() -> LichessFilter {
        LichessFilter {
            speeds: None,
            ratings: None,
            since: None,
            until: None,
        }
    }

    fn limits() -> Limits {
        Limits {
            moves: 12,
            top_games: usize::MAX,
            recent_games: usize::MAX,
        }
    }

    #[test]
    fn test_prepare_lichess() {
        let buf = entry();
        let view = EntryView::unpack_entire(&buf);
        let prepared = prepare_lichess(view, &all(), &limits(), true);
        assert_eq!(prepared.total.games(), 4);
        assert_eq!(prepared.moves.len(), 2);
        assert_eq!(prepared.moves[0].stats.games(), 3);
        assert_eq!(prepared.moves[0].game, None);
        assert_eq!(prepared.moves[1].game, Some(id(4)));
        // Without a rating filter, the top groups down to 2000 are selected:
        // games 3 (2600 bullet) and 4 (2100 rapid) are top games, with the
        // bullet game closer to the highscore of its speed.
        let top: Vec<GameId> = prepared.top_games.iter().map(|g| g.id).collect();
        assert_eq!(top, vec![id(3), id(4)]);
        let recent: Vec<GameId> = prepared.recent_games.iter().map(|g| g.id).collect();
        assert_eq!(recent, vec![id(2), id(1)]);
        let history = prepared.history.unwrap();
        // Without `until`, the last month is omitted.
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].stats.games(), 1);
        assert_eq!(history[1].stats.games(), 1);

        let filter = LichessFilter {
            speeds: Some(1 << Speed::Blitz.bits()),
            ..all()
        };
        let prepared = prepare_lichess(view, &filter, &limits(), false);
        assert_eq!(prepared.total.games(), 2);
        assert_eq!(prepared.moves.len(), 1);
        assert_eq!(prepared.recent_games.len(), 2);

        let filter = LichessFilter {
            ratings: Some(1 << 6 | 1 << 7 | 1 << 8),
            since: Some(Month::from_year_and_month(2020, 3).unwrap()),
            until: Some(Month::from_year_and_month(2020, 3).unwrap()),
            ..all()
        };
        let prepared = prepare_lichess(view, &filter, &limits(), true);
        assert_eq!(prepared.total.games(), 2);
        assert_eq!(prepared.top_games.len(), 2);
        assert!(prepared.recent_games.is_empty());
        assert_eq!(prepared.history.unwrap().len(), 1);
    }

    #[test]
    fn test_prepare_masters() {
        let buf = entry();
        let prepared = prepare_masters(EntryView::unpack_entire(&buf), &all(), &limits(), 2);
        assert_eq!(prepared.total.games(), 4);
        let candidates: Vec<GameId> = prepared.candidates.iter().map(|g| g.id).collect();
        assert_eq!(candidates, vec![id(3), id(4)]);
    }
}
