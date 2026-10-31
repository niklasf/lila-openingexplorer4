use std::{
    cmp::{Ordering, Reverse, min},
    collections::{BTreeMap, BinaryHeap},
};

use shakmaty::packed::PackedUciMove;

use crate::{
    packed::{CellGames, CellKey, EntryView, GameId, Month, RatingGroup, Speed, SubStats},
    server::query::{LichessFilter, Limits},
};

/// Most games referenced by a lichess response (top and recent games).
pub const MAX_LICHESS_GAMES: usize = 8;
/// Most top games of a lichess response.
pub const MAX_TOP_GAMES: usize = 4;
/// Most top games of a masters response.
pub const MAX_MASTERS_GAMES: usize = 15;

/// A cell of an entry, from a packed or a decoded entry.
pub struct CellRef<'a> {
    pub key: CellKey,
    pub stats: SubStats,
    pub games: CellGames<'a>,
    /// Highest rank among the games and its game, if known in advance.
    pub last: Option<(u64, GameId)>,
}

/// Source of the cells of an entry, in canonical order.
pub trait CellSource {
    fn cells(&self) -> impl Iterator<Item = CellRef<'_>>;
}

impl CellSource for EntryView<'_> {
    fn cells(&self) -> impl Iterator<Item = CellRef<'_>> {
        EntryView::cells(self).map(|cell| CellRef {
            key: cell.key,
            stats: cell.stats,
            games: cell.games,
            last: None,
        })
    }
}

/// A cell header, compact (48 bytes).
#[derive(Debug, Clone)]
struct DecodedCell {
    key: CellKey,
    white: u32,
    draws: u32,
    black: u32,
    rating_offset_sum: i64,
    /// Offset and length of the packed games in the entry bytes.
    games_start: u32,
    games_len: u16,
    /// Rank and id of the most recent game.
    last_rank: u32,
    last_id: [u8; GameId::SIZE],
}

impl DecodedCell {
    fn stats(&self) -> SubStats {
        SubStats {
            rating_offset_sum: self.rating_offset_sum,
            white: u64::from(self.white),
            black: u64::from(self.black),
            draws: u64::from(self.draws),
        }
    }
}

/// An entry with the cell headers decoded, for fast repeated aggregation
/// of large entries. Games are only decoded when a cell is selected.
#[derive(Debug)]
pub struct DecodedEntry {
    bytes: Vec<u8>,
    cells: Vec<DecodedCell>,
}

impl DecodedEntry {
    pub fn new(bytes: &[u8]) -> DecodedEntry {
        let started = std::time::Instant::now();
        let base = bytes.as_ptr() as usize;
        let cells = EntryView::unpack_entire(bytes)
            .cells()
            .map(|cell| {
                let games = cell.games.as_bytes();
                // Games of a cell are packed in chronological order.
                let (last_rank, last_id) = cell.games.last().expect("cell has games");
                let stats = cell.stats;
                DecodedCell {
                    key: cell.key,
                    white: u32::try_from(stats.white).expect("cell count"),
                    draws: u32::try_from(stats.draws).expect("cell count"),
                    black: u32::try_from(stats.black).expect("cell count"),
                    rating_offset_sum: stats.rating_offset_sum,
                    games_start: (games.as_ptr() as usize - base) as u32,
                    games_len: u16::try_from(games.len()).expect("games of a cell"),
                    last_rank: u32::try_from(last_rank).expect("rank"),
                    last_id: last_id.to_bytes(),
                }
            })
            .collect();
        let decoded = DecodedEntry {
            bytes: bytes.to_vec(),
            cells,
        };
        log::debug!(
            "decoded entry of {} bytes with {} cells ({} bytes) in {:.1?}",
            bytes.len(),
            decoded.cells.len(),
            decoded.weight(),
            started.elapsed()
        );
        decoded
    }

    /// Approximate memory use in bytes.
    pub fn weight(&self) -> usize {
        self.bytes.len() + self.cells.len() * std::mem::size_of::<DecodedCell>()
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

impl CellSource for DecodedEntry {
    fn cells(&self) -> impl Iterator<Item = CellRef<'_>> {
        self.cells.iter().map(|cell| CellRef {
            key: cell.key,
            stats: cell.stats(),
            games: CellGames::from_bytes(
                &self.bytes[cell.games_start as usize..][..usize::from(cell.games_len)],
            ),
            last: Some((
                u64::from(cell.last_rank),
                GameId::try_from(cell.last_id).expect("valid game id"),
            )),
        })
    }
}

/// Statistics of a move, aggregated over the matching cells.
#[derive(Debug, Clone)]
pub struct PreparedMove {
    pub m: PackedUciMove,
    pub stats: SubStats,
    /// The only game, if the move was played exactly once.
    pub game: Option<GameId>,
    pub(crate) latest: Option<(u64, GameId)>,
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

pub(crate) struct Ranked<K, T> {
    key: K,
    item: T,
}

impl<K: Ord, T> PartialEq for Ranked<K, T> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<K: Ord, T> Eq for Ranked<K, T> {}

impl<K: Ord, T> PartialOrd for Ranked<K, T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<K: Ord, T> Ord for Ranked<K, T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

/// Keeps the `k` items with the largest keys (ties broken arbitrarily).
pub(crate) struct TopK<K: Ord, T> {
    k: usize,
    heap: BinaryHeap<Reverse<Ranked<K, T>>>,
}

impl<K: Ord, T> TopK<K, T> {
    pub(crate) fn new(k: usize) -> TopK<K, T> {
        TopK {
            k,
            heap: BinaryHeap::with_capacity(k + 1),
        }
    }

    /// Whether an item with the key would be kept.
    pub(crate) fn accepts(&self, key: &K) -> bool {
        self.k > 0
            && (self.heap.len() < self.k
                || self
                    .heap
                    .peek()
                    .is_some_and(|Reverse(worst)| worst.key < *key))
    }

    pub(crate) fn push(&mut self, key: K, item: T) {
        if !self.accepts(&key) {
            return;
        }
        if self.heap.len() >= self.k {
            self.heap.pop();
        }
        self.heap.push(Reverse(Ranked { key, item }));
    }

    /// The items, best first.
    pub(crate) fn into_sorted(self) -> Vec<T> {
        let mut items: Vec<Ranked<K, T>> = self.heap.into_iter().map(|Reverse(r)| r).collect();
        items.sort_by(|a, b| b.key.cmp(&a.key));
        items.into_iter().map(|r| r.item).collect()
    }
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

/// Key of a top game: closest to the highscore of its speed, then most
/// recent.
pub(crate) fn top_key(speed: Option<Speed>, group: RatingGroup, rank: u64) -> (Reverse<i32>, u64) {
    (Reverse(highscore(speed) - lower_bound(group)), rank)
}

/// Which games of the matching cells to keep while aggregating.
pub(crate) struct GameSelection {
    /// Most recent games, enough to fill the recent games after removing
    /// the top games.
    pub(crate) recent: TopK<u64, GameRef>,
    /// Best games among the top rating groups.
    pub(crate) top: TopK<(Reverse<i32>, u64), GameRef>,
    pub(crate) top_group: Option<RatingGroup>,
    /// Masters: most recent games of the highest groups.
    pub(crate) candidates: TopK<(RatingGroup, u64), GameRef>,
}

impl GameSelection {
    /// Whether any game of a cell whose best game has the given rank could
    /// be selected.
    pub(crate) fn accepts_cell(
        &self,
        speed: Option<Speed>,
        group: RatingGroup,
        max_rank: u64,
    ) -> bool {
        self.recent.accepts(&max_rank)
            || (self.top_group.is_some_and(|top_group| group >= top_group)
                && self.top.accepts(&top_key(speed, group, max_rank)))
            || self.candidates.accepts(&(group, max_rank))
    }

    pub(crate) fn push(&mut self, g: GameRef) {
        self.recent.push(g.rank, g);
        if self.top_group.is_some_and(|top_group| g.group >= top_group) {
            self.top.push(top_key(g.speed, g.group, g.rank), g);
        }
        self.candidates.push((g.group, g.rank), g);
    }
}

#[derive(Default)]
pub(crate) struct Aggregate {
    pub(crate) total: SubStats,
    pub(crate) moves: Vec<PreparedMove>,
    pub(crate) months: BTreeMap<Month, SubStats>,
}

/// Aggregates the cells of an entry that match the filter, in canonical
/// order.
fn aggregate<S: CellSource>(
    entry: &S,
    filter: &LichessFilter,
    games: Option<&mut GameSelection>,
    collect_months: bool,
) -> Aggregate {
    let mut agg = Aggregate::default();
    let mut games = games;
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
        let Some(games) = games.as_deref_mut() else {
            continue;
        };
        if let Some((max_rank, last_id)) = cell.last {
            if current.latest.is_none_or(|(latest, _)| latest < max_rank) {
                current.latest = Some((max_rank, last_id));
            }
            if !games.accepts_cell(header.speed, header.rating_group, max_rank) {
                continue;
            }
        }
        for (rank, id) in cell.games {
            if current.latest.is_none_or(|(latest, _)| latest < rank) {
                current.latest = Some((rank, id));
            }
            games.push(GameRef {
                group: header.rating_group,
                speed: header.speed,
                rank,
                m,
                id,
            });
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

pub(crate) fn sort_moves_and_truncate(moves: &mut Vec<PreparedMove>, num: usize) {
    moves.sort_by_key(|pm| Reverse(pm.stats.games()));
    moves.truncate(num);
}

#[derive(Debug)]
pub struct LichessPrepared {
    pub total: SubStats,
    pub moves: Vec<PreparedMove>,
    pub top_games: Vec<GameRef>,
    pub recent_games: Vec<GameRef>,
    pub history: Option<Vec<HistorySegment>>,
}

pub fn prepare_lichess<S: CellSource>(
    entry: &S,
    filter: &LichessFilter,
    limits: &Limits,
    history: bool,
) -> LichessPrepared {
    let mut games = GameSelection {
        recent: TopK::new(MAX_LICHESS_GAMES + MAX_TOP_GAMES),
        top: TopK::new(MAX_TOP_GAMES),
        top_group: filter.top_group(),
        candidates: TopK::new(0),
    };
    let agg = aggregate(
        entry,
        filter,
        limits.games_wanted().then_some(&mut games),
        history,
    );
    finish_lichess(agg, games, filter, limits, history)
}

/// Builds the response parts from the aggregate and the selected games.
pub(crate) fn finish_lichess(
    agg: Aggregate,
    games: GameSelection,
    filter: &LichessFilter,
    limits: &Limits,
    history: bool,
) -> LichessPrepared {
    let mut moves = agg.moves;
    sort_moves_and_truncate(&mut moves, limits.moves);

    let mut top_games = games.top.into_sorted();
    let valid_recent_games = MAX_LICHESS_GAMES - top_games.len();
    let mut recent_games: Vec<GameRef> = games
        .recent
        .into_sorted()
        .into_iter()
        .filter(|g| !top_games.iter().any(|t| t.id == g.id))
        .collect();
    top_games.truncate(limits.top_games);
    recent_games.truncate(min(valid_recent_games, limits.recent_games));

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
pub(crate) fn build_history(
    months: BTreeMap<Month, SubStats>,
    filter: &LichessFilter,
) -> Vec<HistorySegment> {
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
pub fn prepare_masters<S: CellSource>(
    entry: &S,
    filter: &LichessFilter,
    limits: &Limits,
    max_candidates: usize,
) -> MastersPrepared {
    let mut games = GameSelection {
        recent: TopK::new(0),
        top: TopK::new(0),
        top_group: None,
        candidates: TopK::new(max_candidates),
    };
    let agg = aggregate(
        entry,
        filter,
        (limits.top_games > 0).then_some(&mut games),
        false,
    );
    let mut moves = agg.moves;
    sort_moves_and_truncate(&mut moves, limits.moves);
    MastersPrepared {
        total: agg.total,
        moves,
        candidates: games.candidates.into_sorted(),
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;
    use shakmaty::{Color, KnownOutcome, uci::UciMove};

    use super::*;
    use crate::packed::{GameSeq, GroupHeader, LichessEntry, Rating};

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
    fn test_top_k() {
        let mut top = TopK::new(2);
        for (key, item) in [(3, "c"), (1, "a"), (5, "e"), (4, "d"), (2, "b")] {
            top.push(key, item);
        }
        assert!(top.accepts(&5) && !top.accepts(&4));
        assert_eq!(top.into_sorted(), vec!["e", "d"]);
        let mut none: TopK<i32, i32> = TopK::new(0);
        none.push(1, 1);
        assert!(none.into_sorted().is_empty());
    }

    #[test]
    fn test_prepare_lichess() {
        let buf = entry();
        let view = EntryView::unpack_entire(&buf);
        let prepared = prepare_lichess(&view, &all(), &limits(), true);
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
        let prepared = prepare_lichess(&view, &filter, &limits(), false);
        assert_eq!(prepared.total.games(), 2);
        assert_eq!(prepared.moves.len(), 1);
        assert_eq!(prepared.recent_games.len(), 2);

        let filter = LichessFilter {
            ratings: Some(1 << 6 | 1 << 7 | 1 << 8),
            since: Some(Month::from_year_and_month(2020, 3).unwrap()),
            until: Some(Month::from_year_and_month(2020, 3).unwrap()),
            ..all()
        };
        let prepared = prepare_lichess(&view, &filter, &limits(), true);
        assert_eq!(prepared.total.games(), 2);
        assert_eq!(prepared.top_games.len(), 2);
        assert!(prepared.recent_games.is_empty());
        assert_eq!(prepared.history.unwrap().len(), 1);

        let no_games = Limits {
            top_games: 0,
            recent_games: 0,
            moves: 1,
        };
        let prepared = prepare_lichess(&view, &all(), &no_games, false);
        assert_eq!(prepared.moves.len(), 1);
        assert!(prepared.top_games.is_empty() && prepared.recent_games.is_empty());
    }

    #[test]
    fn test_prepare_masters() {
        let buf = entry();
        let prepared = prepare_masters(&EntryView::unpack_entire(&buf), &all(), &limits(), 2);
        assert_eq!(prepared.total.games(), 4);
        let candidates: Vec<GameId> = prepared.candidates.iter().map(|g| g.id).collect();
        assert_eq!(candidates, vec![id(3), id(4)]);
    }

    fn game_ids(games: &[GameRef]) -> Vec<GameId> {
        games.iter().map(|g| g.id).collect()
    }

    /// Decoded entries with pruning give the same results as packed entries.
    #[test]
    fn test_decoded_matches_packed() {
        arbtest(|u| {
            let mut entry: LichessEntry = u.arbitrary()?;
            let mut buf = Vec::new();
            entry.pack_entire(&mut buf);
            let view = EntryView::unpack_entire(&buf);
            let decoded = DecodedEntry::new(&buf);
            let filter = LichessFilter {
                speeds: u.arbitrary()?,
                ratings: u.arbitrary()?,
                since: None,
                until: None,
            };
            let limits = Limits {
                moves: 12,
                top_games: u.int_in_range(0..=4)?,
                recent_games: u.int_in_range(0..=8)?,
            };
            let a = prepare_lichess(&view, &filter, &limits, true);
            let b = prepare_lichess(&decoded, &filter, &limits, true);
            assert_eq!(a.total, b.total);
            assert_eq!(a.moves.len(), b.moves.len());
            for (x, y) in a.moves.iter().zip(&b.moves) {
                assert_eq!((x.m, x.stats, x.game), (y.m, y.stats, y.game));
            }
            // Ties in rank do not occur: ranks are unique within an entry.
            assert_eq!(game_ids(&a.top_games), game_ids(&b.top_games));
            assert_eq!(game_ids(&a.recent_games), game_ids(&b.recent_games));
            let a = prepare_masters(&view, &filter, &limits, 5);
            let b = prepare_masters(&decoded, &filter, &limits, 5);
            assert_eq!(game_ids(&a.candidates), game_ids(&b.candidates));
            Ok(())
        });
    }
}
