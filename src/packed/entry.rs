use std::collections::BTreeMap;

use arbitrary::{Arbitrary, Unstructured};
use bitflags::bitflags;
use bytes::{Buf, BufMut};
use partial_sort::PartialSort;
use shakmaty::{Color, KnownOutcome, packed::PackedUciMove};

use crate::packed::{
    GameIndex, Month, Rating, RatingGroup, Speed, pack_int, pack_uint, unpack_int, unpack_uint,
};

/// Each cell (move, group and month) keeps only its most recent games, up to
/// this many.
pub const GAMES_PER_CELL: usize = 8;

#[derive(Arbitrary, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct GroupHeader {
    pub speed: Option<Speed>,
    pub rating_group: RatingGroup,
}

impl GroupHeader {
    /// Packs the header into the low 7 bits of a byte: the speed (0 if
    /// missing) into the low 3 bits, and the rating group above. Ordering
    /// groups by this byte is the canonical order of groups within a move.
    pub fn to_byte(&self) -> u8 {
        let packed_speed = self.speed.map_or(0, Speed::bits);
        let packed_rating_group = match self.rating_group {
            RatingGroup::Low => 0,
            RatingGroup::Group1000 => 1,
            RatingGroup::Group1200 => 2,
            RatingGroup::Group1400 => 3,
            RatingGroup::Group1600 => 4,
            RatingGroup::Group1800 => 5,
            RatingGroup::Group2000 => 6,
            RatingGroup::Group2200 => 7,
            RatingGroup::Group2500 => 8,
            RatingGroup::Group2800 => 9,
            RatingGroup::Group3200 => 10,
        };
        packed_speed | (packed_rating_group << 3)
    }

    pub fn from_byte(packed: u8) -> Option<GroupHeader> {
        let speed = match packed & 0b111 {
            0 => None,
            bits => Some(Speed::from_bits(bits)?),
        };
        let rating_group = match packed >> 3 {
            0 => RatingGroup::Low,
            1 => RatingGroup::Group1000,
            2 => RatingGroup::Group1200,
            3 => RatingGroup::Group1400,
            4 => RatingGroup::Group1600,
            5 => RatingGroup::Group1800,
            6 => RatingGroup::Group2000,
            7 => RatingGroup::Group2200,
            8 => RatingGroup::Group2500,
            9 => RatingGroup::Group2800,
            10 => RatingGroup::Group3200,
            _ => return None,
        };
        Some(GroupHeader {
            speed,
            rating_group,
        })
    }
}

/// Set in the packed header of the last group of a move.
const LAST_GROUP_OF_MOVE: u8 = 0x80;

/// Groups with at least this many months store the statistics of the whole
/// group before their cells.
pub const SUBTOTAL_MIN_MONTHS: usize = 16;

bitflags! {
    /// Low bits of the tag of a group.
    #[derive(Debug, Clone, Copy)]
    struct GroupFlags: u64 {
        /// More than one month, the body is length prefixed.
        const MULTI_MONTH = 1;
        /// The body starts with a subtotal.
        const HAS_SUBTOTAL = 2;
    }
}

/// Tag in the low bits of the first count of `SubStats`: the stats of more
/// (or less) than one game follow. Tags 0, 1 and 2 are a single game won by
/// white, drawn, or won by black.
const MULTI: u64 = 3;

#[derive(Arbitrary, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SubStats {
    pub rating_offset_sum: i64,
    pub white: u64,
    pub black: u64,
    pub draws: u64,
}

impl SubStats {
    pub fn add(&mut self, rating: Rating, outcome: KnownOutcome) {
        self.rating_offset_sum += i64::from(rating.offset());
        match outcome {
            KnownOutcome::Decisive {
                winner: Color::White,
            } => self.white += 1,
            KnownOutcome::Decisive {
                winner: Color::Black,
            } => self.black += 1,
            KnownOutcome::Draw => self.draws += 1,
        }
    }

    pub fn merge(&mut self, other: &SubStats) {
        self.rating_offset_sum += other.rating_offset_sum;
        self.white += other.white;
        self.black += other.black;
        self.draws += other.draws;
    }

    pub fn games(&self) -> u64 {
        self.white + self.black + self.draws
    }

    pub fn average_rating(&self) -> Option<Rating> {
        let games = i64::try_from(self.games()).expect("games fit i64");
        (games > 0).then(|| {
            let offset = (2 * self.rating_offset_sum + games).div_euclid(2 * games);
            Rating::from_offset(i16::try_from(offset).expect("average within rating range"))
        })
    }

    pub fn pack<B: BufMut>(&self, buf: &mut B, rating_group: RatingGroup) {
        match (self.white, self.draws, self.black) {
            (1, 0, 0) => buf.put_u8(0),
            (0, 1, 0) => buf.put_u8(1),
            (0, 0, 1) => buf.put_u8(2),
            (white, draws, black) => {
                pack_uint(buf, draws << 2 | MULTI);
                pack_uint(buf, white);
                pack_uint(buf, black);
            }
        }
        pack_int(
            buf,
            self.rating_offset_sum
                - self.games() as i64 * i64::from(rating_group.typical_rating().offset()),
        );
    }

    pub fn unpack<B: Buf>(buf: &mut B, rating_group: RatingGroup) -> SubStats {
        let mut stats = SubStats::default();
        match unpack_uint(buf) {
            0 => stats.white = 1,
            1 => stats.draws = 1,
            2 => stats.black = 1,
            tagged => {
                assert_eq!(tagged & 0b11, MULTI, "invalid counts");
                stats.draws = tagged >> 2;
                stats.white = unpack_uint(buf);
                stats.black = unpack_uint(buf);
            }
        }
        stats.rating_offset_sum = unpack_int(buf)
            + stats.games() as i64 * i64::from(rating_group.typical_rating().offset());
        stats
    }
}

/// Move, group header and month of a cell, ordered canonically.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellKey {
    m: [u8; 2],
    header: u8,
    month: Month,
}

impl CellKey {
    pub fn new(m: PackedUciMove, header: GroupHeader, month: Month) -> CellKey {
        CellKey {
            m: m.to_bytes(),
            header: header.to_byte(),
            month,
        }
    }

    /// From the packed move, the `GroupHeader::to_byte()` and the month.
    pub fn from_packed(m: [u8; 2], header: u8, month: Month) -> CellKey {
        CellKey { m, header, month }
    }

    pub fn uci_move(&self) -> PackedUciMove {
        PackedUciMove::from_bytes(self.m)
    }

    pub fn group_header(&self) -> GroupHeader {
        GroupHeader::from_byte(self.header).expect("valid group header")
    }

    pub fn month(&self) -> Month {
        self.month
    }
}

/// A cell to be packed: its statistics and games, in ascending order.
/// At least the most recent `min(GAMES_PER_CELL, games of the cell)` games
/// must be given.
#[derive(Debug, Copy, Clone)]
pub struct EntryCell<'a> {
    pub key: CellKey,
    pub stats: SubStats,
    pub games: &'a [GameIndex],
}

impl<'a> EntryCell<'a> {
    /// The games that are packed: the most recent ones, exactly
    /// `min(GAMES_PER_CELL, games of the cell)` of them.
    fn packed_games(&self) -> &'a [GameIndex] {
        let n = self.stats.games().min(GAMES_PER_CELL as u64) as usize;
        assert!(self.games.len() >= n, "cell is missing games");
        &self.games[self.games.len() - n..]
    }
}

/// Statistics and most recent games of a cell.
#[derive(Debug, Default, Clone)]
struct Cell {
    stats: SubStats,
    /// In any order, truncated to the most recent games once in a while.
    games: Vec<GameIndex>,
}

impl Cell {
    fn push_game(&mut self, game: GameIndex) {
        self.games.push(game);
        if self.games.len() >= 4 * GAMES_PER_CELL {
            self.truncate_games();
        }
    }

    /// Keeps only the most recent games, in ascending order.
    fn truncate_games(&mut self) {
        let keep = self.games.len().min(GAMES_PER_CELL);
        self.games.partial_sort(keep, |a, b| b.cmp(a));
        self.games.truncate(keep);
        self.games.reverse();
    }
}

/// Statistics and recent games of the moves played from a position, by
/// speed, rating group and month.
///
/// Packed format, moves by packed bytes and groups by header byte (the
/// canonical order), but months within a group and games within a cell
/// newest first, so that readers can stop early:
///
/// ```text
/// entry  := (move group+)*
/// move   := PackedUciMove (2 bytes)
/// group  := header tag (cell | uint(byte length of multi) multi)
/// header := GroupHeader::to_byte() | LAST_GROUP_OF_MOVE if last group of the move
/// tag    := uint(newest month << 2 | GroupFlags)
/// multi  := [SubStats of all cells of the group] cell (uint(month delta) cell)*
/// cell   := SubStats game*
/// game   := GameIndex (5 bytes)
/// ```
///
/// A cell holds the games of a month for a move and group. Months are
/// counted since 1900-01, and each month delta is subtracted from the
/// month of the previous cell. A group of a single month has neither a
/// length nor a subtotal; a group of `SUBTOTAL_MIN_MONTHS` or more months
/// has a subtotal. Each cell has exactly
/// `min(GAMES_PER_CELL, games of the cell)` games, the most recent ones
/// (highest game indexes), newest first.
///
/// Game indexes are chronological within a variant (the order of the
/// parquet input), so newer months have higher game indexes. Readers rely on
/// this to stop early, and to order games across cells. (Chess960 games are
/// not strictly chronological, which is accepted.)
#[derive(Default, Debug, Clone)]
pub struct LichessEntry {
    cells: BTreeMap<CellKey, Cell>,
}

impl Arbitrary<'_> for LichessEntry {
    fn arbitrary(u: &mut Unstructured<'_>) -> arbitrary::Result<Self> {
        // Chronological game indexes, as in real entries.
        let mut parts: Vec<(
            shakmaty::uci::UciMove,
            Option<Speed>,
            Rating,
            KnownOutcome,
            Month,
        )> = u.arbitrary_iter()?.collect::<arbitrary::Result<_>>()?;
        parts.sort_by_key(|part| part.4);
        let mut entry = LichessEntry::new();
        let mut index = u.int_in_range(0..=1 << 32)?;
        for (m, speed, rating, outcome, month) in parts {
            index += u.int_in_range(1..=1000)?;
            let header = GroupHeader {
                speed,
                rating_group: RatingGroup::select(rating),
            };
            entry.add_game(
                CellKey::new(PackedUciMove::pack(m), header, month),
                rating,
                outcome,
                GameIndex::new(index).expect("game index"),
            );
        }
        Ok(entry)
    }
}

impl LichessEntry {
    pub fn new() -> Self {
        LichessEntry::default()
    }

    /// Adds a game to the cell of the key, which must match the rating.
    pub fn add_game(
        &mut self,
        key: CellKey,
        rating: Rating,
        outcome: KnownOutcome,
        game: GameIndex,
    ) {
        debug_assert_eq!(
            key.group_header().rating_group,
            RatingGroup::select(rating),
            "rating does not match cell"
        );
        let cell = self.cells.entry(key).or_default();
        cell.stats.add(rating, outcome);
        cell.push_game(game);
    }

    pub fn pack_entire(&mut self, buf: &mut Vec<u8>) {
        for cell in self.cells.values_mut() {
            cell.truncate_games();
        }
        Self::pack_cells_entire(
            buf,
            self.cells.iter().map(|(key, cell)| EntryCell {
                key: *key,
                stats: cell.stats,
                games: &cell.games,
            }),
        );
    }

    /// Merges a packed entry into this entry.
    pub fn merge_unpack_entire(&mut self, bytes: &[u8]) {
        for view in EntryView::unpack_entire(bytes).cells() {
            let cell = self.cells.entry(view.key).or_default();
            cell.stats.merge(&view.stats);
            for game in view.games {
                cell.push_game(game);
            }
        }
    }

    /// Packs cells, which need to be in canonical order (by move bytes, then
    /// by header byte, then by month).
    pub fn pack_cells_entire<'a, I>(buf: &mut Vec<u8>, cells: I)
    where
        I: Iterator<Item = EntryCell<'a>> + Clone,
    {
        let mut cells = cells.peekable();
        let mut group: Vec<EntryCell<'a>> = Vec::new();
        let mut multi = Vec::new();
        let mut prev: Option<CellKey> = None;
        let mut new_move = true;
        while let Some(first) = cells.next() {
            group.clear();
            group.push(first);
            while let Some(cell) = cells
                .next_if(|cell| cell.key.m == first.key.m && cell.key.header == first.key.header)
            {
                group.push(cell);
            }
            debug_assert!(
                prev < Some(first.key) && group.is_sorted_by(|a, b| a.key < b.key),
                "cells not in canonical order"
            );
            prev = group.last().map(|cell| cell.key);

            if new_move {
                buf.put_slice(&first.key.m);
            }
            new_move = cells.peek().is_none_or(|next| next.key.m != first.key.m);
            buf.put_u8(first.key.header | if new_move { LAST_GROUP_OF_MOVE } else { 0 });
            pack_group(buf, &mut multi, &group);
        }
    }
}

/// Packs the cells of one group (ascending months), using `multi` as
/// scratch space.
fn pack_group(buf: &mut Vec<u8>, multi: &mut Vec<u8>, group: &[EntryCell<'_>]) {
    let rating_group = group[0].key.group_header().rating_group;
    let newest = group[group.len() - 1].key.month.months_since_jan_1900();
    let is_multi = group.len() > 1;
    let has_subtotal = group.len() >= SUBTOTAL_MIN_MONTHS;
    let mut flags = GroupFlags::empty();
    flags.set(GroupFlags::MULTI_MONTH, is_multi);
    flags.set(GroupFlags::HAS_SUBTOTAL, has_subtotal);
    pack_uint(buf, u64::from(newest) << 2 | flags.bits());
    if !is_multi {
        pack_cell(buf, &group[0], rating_group);
        return;
    }

    multi.clear();
    if has_subtotal {
        let mut subtotal = SubStats::default();
        for cell in group {
            subtotal.merge(&cell.stats);
        }
        subtotal.pack(multi, rating_group);
    }
    let mut prev_month = newest;
    for (i, cell) in group.iter().rev().enumerate() {
        let month = cell.key.month.months_since_jan_1900();
        if i > 0 {
            pack_uint(multi, u64::from(prev_month - month));
        }
        pack_cell(multi, cell, rating_group);
        prev_month = month;
    }
    pack_uint(buf, multi.len() as u64);
    buf.extend_from_slice(multi);
}

/// Packs the statistics of a cell and its games, newest first.
fn pack_cell(buf: &mut Vec<u8>, cell: &EntryCell<'_>, rating_group: RatingGroup) {
    cell.stats.pack(buf, rating_group);
    let games = cell.packed_games();
    debug_assert!(games.is_sorted(), "games not in order");
    for game in games.iter().rev() {
        game.pack(buf);
    }
}

/// Skips a cell (statistics and games), returning its statistics.
fn skip_cell(buf: &mut &[u8], rating_group: RatingGroup) -> SubStats {
    let stats = SubStats::unpack(buf, rating_group);
    buf.advance(stats.games().min(GAMES_PER_CELL as u64) as usize * GameIndex::SIZE);
    stats
}

/// A packed entry (see `LichessEntry`), read in place.
#[derive(Debug, Copy, Clone)]
pub struct EntryView<'a> {
    bytes: &'a [u8],
}

impl<'a> EntryView<'a> {
    /// Views the entire slice as a packed entry.
    pub fn unpack_entire(bytes: &'a [u8]) -> EntryView<'a> {
        EntryView { bytes }
    }

    /// The groups, in canonical order.
    pub fn groups(&self) -> GroupViews<'a> {
        GroupViews {
            buf: self.bytes,
            m: [0; 2],
            more_groups: false,
        }
    }

    /// The cells, groups in canonical order and months newest first.
    pub fn cells(&self) -> CellViews<'a> {
        CellViews {
            groups: self.groups(),
            cells: GroupCells::empty(),
        }
    }
}

/// A group of a packed entry: the cells of a move, speed and rating group.
#[derive(Debug, Clone)]
pub struct GroupView<'a> {
    m: [u8; 2],
    header_byte: u8,
    pub header: GroupHeader,
    /// The month of the first cell.
    pub newest_month: Month,
    /// Statistics of all cells, if the group stores them.
    pub subtotal: Option<SubStats>,
    cells: &'a [u8],
}

impl<'a> GroupView<'a> {
    pub fn uci_move(&self) -> PackedUciMove {
        PackedUciMove::from_bytes(self.m)
    }

    /// The cells, newest month first.
    pub fn cells(&self) -> GroupCells<'a> {
        GroupCells {
            buf: self.cells,
            m: self.m,
            header: self.header_byte,
            rating_group: self.header.rating_group,
            month: self.newest_month.months_since_jan_1900(),
            first: true,
            games_left: 0,
        }
    }
}

/// Iterator over the groups of a packed entry.
#[derive(Debug, Clone)]
pub struct GroupViews<'a> {
    buf: &'a [u8],
    m: [u8; 2],
    more_groups: bool,
}

impl<'a> Iterator for GroupViews<'a> {
    type Item = GroupView<'a>;

    fn next(&mut self) -> Option<GroupView<'a>> {
        if !self.more_groups {
            if self.buf.is_empty() {
                return None;
            }
            self.buf.copy_to_slice(&mut self.m);
        }
        let byte = self.buf.get_u8();
        let header_byte = byte & !LAST_GROUP_OF_MOVE;
        let header = GroupHeader::from_byte(header_byte).expect("valid group header");
        self.more_groups = byte & LAST_GROUP_OF_MOVE == 0;

        let tag = unpack_uint(&mut self.buf);
        let newest_month =
            Month::from_months_since_jan_1900(u16::try_from(tag >> 2).expect("valid month"));
        let flags = GroupFlags::from_bits_truncate(tag);
        let mut subtotal = None;
        let cells = if flags.contains(GroupFlags::MULTI_MONTH) {
            let len = usize::try_from(unpack_uint(&mut self.buf)).expect("group length");
            let (mut multi, rest) = self.buf.split_at(len);
            self.buf = rest;
            if flags.contains(GroupFlags::HAS_SUBTOTAL) {
                subtotal = Some(SubStats::unpack(&mut multi, header.rating_group));
            }
            multi
        } else {
            let start = self.buf;
            skip_cell(&mut self.buf, header.rating_group);
            &start[..start.len() - self.buf.len()]
        };

        Some(GroupView {
            m: self.m,
            header_byte,
            header,
            newest_month,
            subtotal,
            cells,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CellHeader {
    pub key: CellKey,
    pub stats: SubStats,
}

/// Cursor over the cells of a group, newest month first. Cells can be
/// read as a whole (as an iterator of `CellView`), or incrementally with
/// `next_cell()` and `next_game()`.
#[derive(Debug, Clone)]
pub struct GroupCells<'a> {
    buf: &'a [u8],
    m: [u8; 2],
    header: u8,
    rating_group: RatingGroup,
    /// Month of the current cell.
    month: u16,
    /// The next cell is the first of the group, without a month delta.
    first: bool,
    /// Unread games of the current cell.
    games_left: usize,
}

impl<'a> GroupCells<'a> {
    fn empty() -> GroupCells<'static> {
        GroupCells {
            buf: &[],
            m: [0; 2],
            header: 0,
            rating_group: RatingGroup::Low,
            month: 0,
            first: true,
            games_left: 0,
        }
    }

    /// Advances to the next cell, skipping the unread games of the current
    /// one.
    pub fn next_cell(&mut self) -> Option<CellHeader> {
        while self.next_game().is_some() {}
        if self.buf.is_empty() {
            return None;
        }
        if !self.first {
            self.month -= u16::try_from(unpack_uint(&mut self.buf)).expect("valid month delta");
        }
        self.first = false;
        let stats = SubStats::unpack(&mut self.buf, self.rating_group);
        self.games_left = stats.games().min(GAMES_PER_CELL as u64) as usize;
        Some(CellHeader {
            key: CellKey {
                m: self.m,
                header: self.header,
                month: Month::from_months_since_jan_1900(self.month),
            },
            stats,
        })
    }

    /// The next game of the current cell. Games are newest first.
    pub fn next_game(&mut self) -> Option<GameIndex> {
        if self.games_left == 0 {
            return None;
        }
        self.games_left -= 1;
        Some(GameIndex::unpack(&mut self.buf))
    }
}

impl<'a> Iterator for GroupCells<'a> {
    type Item = CellView<'a>;

    fn next(&mut self) -> Option<CellView<'a>> {
        let CellHeader { key, stats } = self.next_cell()?;
        let start = self.buf;
        while self.next_game().is_some() {}
        Some(CellView {
            key,
            stats,
            games: CellGames {
                buf: &start[..start.len() - self.buf.len()],
            },
        })
    }
}

/// A cell of a packed entry, read in place.
#[derive(Debug, Clone)]
pub struct CellView<'a> {
    pub key: CellKey,
    pub stats: SubStats,
    /// The most recent `min(GAMES_PER_CELL, games of the cell)` games,
    /// newest first.
    pub games: CellGames<'a>,
}

/// Iterator over the cells of a packed entry.
#[derive(Debug, Clone)]
pub struct CellViews<'a> {
    groups: GroupViews<'a>,
    cells: GroupCells<'a>,
}

impl<'a> Iterator for CellViews<'a> {
    type Item = CellView<'a>;

    fn next(&mut self) -> Option<CellView<'a>> {
        loop {
            if let Some(cell) = self.cells.next() {
                return Some(cell);
            }
            self.cells = self.groups.next()?.cells();
        }
    }
}

/// Iterator over the packed games of a cell, newest first.
#[derive(Debug, Clone)]
pub struct CellGames<'a> {
    buf: &'a [u8],
}

impl Iterator for CellGames<'_> {
    type Item = GameIndex;

    fn next(&mut self) -> Option<GameIndex> {
        if self.buf.is_empty() {
            return None;
        }
        Some(GameIndex::unpack(&mut self.buf))
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;
    use shakmaty::uci::UciMove;

    use super::*;

    fn pack(entry: &LichessEntry) -> Vec<u8> {
        let mut buf = Vec::new();
        entry.clone().pack_entire(&mut buf);
        buf
    }

    fn merge(packed: &[&[u8]]) -> Vec<u8> {
        let mut entry = LichessEntry::new();
        for buf in packed {
            entry.merge_unpack_entire(buf);
        }
        pack(&entry)
    }

    type Game = (
        UciMove,
        Option<Speed>,
        Rating,
        KnownOutcome,
        Month,
        GameIndex,
    );

    fn add(entry: &mut LichessEntry, (m, speed, rating, outcome, month, game): Game) {
        let header = GroupHeader {
            speed,
            rating_group: RatingGroup::select(rating),
        };
        entry.add_game(
            CellKey::new(PackedUciMove::pack(m), header, month),
            rating,
            outcome,
            game,
        );
    }

    /// Statistics of all cells in canonical order, and their packed games
    /// in ascending order.
    #[allow(clippy::type_complexity)]
    fn view(bytes: &[u8]) -> (Vec<(CellKey, SubStats)>, Vec<(CellKey, GameIndex)>) {
        let view = EntryView::unpack_entire(bytes);
        let cells: Vec<_> = view.cells().collect();
        let mut games: Vec<_> = cells
            .iter()
            .flat_map(|cell| {
                let games: Vec<_> = cell.games.clone().collect();
                assert!(games.is_sorted_by(|a, b| a > b), "games newest first");
                games.into_iter().map(|game| (game, cell.key))
            })
            .collect();
        games.sort_unstable();

        // Groups are in canonical order and consistent with the cells.
        let mut group_cells = Vec::new();
        let mut prev_group = None;
        for group in view.groups() {
            let group_key = (group.uci_move().to_bytes(), group.header.to_byte());
            assert!(prev_group < Some(group_key), "groups in canonical order");
            prev_group = Some(group_key);
            let cells: Vec<_> = group.cells().collect();
            assert!(
                cells.is_sorted_by(|a, b| a.key > b.key),
                "months newest first"
            );
            assert_eq!(cells[0].key.month(), group.newest_month);
            let mut subtotal = SubStats::default();
            for cell in &cells {
                assert_eq!(cell.key.group_header(), group.header);
                subtotal.merge(&cell.stats);
            }
            assert_eq!(
                group.subtotal,
                (cells.len() >= SUBTOTAL_MIN_MONTHS).then_some(subtotal)
            );
            group_cells.extend(cells.iter().map(|cell| (cell.key, cell.stats)));
        }
        let mut cells: Vec<_> = cells.iter().map(|cell| (cell.key, cell.stats)).collect();
        assert_eq!(cells, group_cells);
        cells.sort_unstable_by_key(|(key, _)| *key);

        (
            cells,
            games.into_iter().map(|(game, key)| (key, game)).collect(),
        )
    }

    /// Like `view`, but from the model.
    #[allow(clippy::type_complexity)]
    fn expected_view(
        entry: &LichessEntry,
    ) -> (Vec<(CellKey, SubStats)>, Vec<(CellKey, GameIndex)>) {
        let mut entry = entry.clone();
        for cell in entry.cells.values_mut() {
            cell.truncate_games();
        }
        let mut games: Vec<_> = entry
            .cells
            .iter()
            .flat_map(|(key, cell)| cell.games.iter().map(|game| (*game, *key)))
            .collect();
        games.sort_unstable();
        (
            entry
                .cells
                .iter()
                .map(|(key, cell)| (*key, cell.stats))
                .collect(),
            games.into_iter().map(|(game, key)| (key, game)).collect(),
        )
    }

    #[test]
    fn test_average_rating() {
        let average = |rating_offset_sum, games| {
            SubStats {
                rating_offset_sum,
                white: games,
                black: 0,
                draws: 0,
            }
            .average_rating()
            .map(i32::from)
        };
        assert_eq!(average(0, 0), None);
        assert_eq!(average(-100, 1), Some(1400));
        // Rounded half up, like the absolute ratings.
        assert_eq!(average(1, 2), Some(1501));
        assert_eq!(average(-1, 2), Some(1500));
        assert_eq!(average(-3, 2), Some(1499));
        assert_eq!(average(-2, 3), Some(1499));
    }

    #[test]
    fn test_sub_stats_roundtrip() {
        arbtest(|u| {
            let mut stats = SubStats {
                rating_offset_sum: 0,
                white: u.int_in_range(0..=3)?,
                black: u.int_in_range(0..=3)?,
                draws: u.int_in_range(0..=3)?,
            };
            if u.arbitrary()? {
                stats = u.arbitrary()?;
                stats.white >>= 20;
                stats.black >>= 20;
                stats.draws >>= 20;
            }
            stats.rating_offset_sum = u.int_in_range(-2000..=2000)? * stats.games() as i64;
            let rating_group: RatingGroup = u.arbitrary()?;

            let mut buf = Vec::new();
            stats.pack(&mut buf, rating_group);
            let mut reader = &buf[..];
            assert_eq!(SubStats::unpack(&mut reader, rating_group), stats);
            assert!(reader.is_empty());
            Ok(())
        });
    }

    #[test]
    fn test_lichess_entry_roundtrip() {
        arbtest(|u| {
            let lichess_entry: LichessEntry = u.arbitrary()?;

            let buf = pack(&lichess_entry);
            assert_eq!(view(&buf), expected_view(&lichess_entry));
            let mut unpacked = LichessEntry::new();
            unpacked.merge_unpack_entire(&buf);
            assert_eq!(pack(&unpacked), buf);
            Ok(())
        });
    }

    #[test]
    fn test_subtotal() {
        // One group with many months, and one with a single month.
        let mut entry = LichessEntry::new();
        let rating = Rating::try_from(1500).unwrap();
        for seq in 0..2 * SUBTOTAL_MIN_MONTHS as u64 {
            let game: Game = (
                "e2e4".parse().unwrap(),
                Some(Speed::Blitz),
                rating,
                KnownOutcome::Draw,
                Month::from_months_since_jan_1900(1400 + 3 * seq as u16),
                GameIndex::new(seq).unwrap(),
            );
            add(&mut entry, game);
        }
        let game: Game = (
            "e2e4".parse().unwrap(),
            Some(Speed::Bullet),
            rating,
            KnownOutcome::Draw,
            Month::from_months_since_jan_1900(1400),
            GameIndex::new(256).unwrap(),
        );
        add(&mut entry, game);

        let buf = pack(&entry);
        let groups: Vec<_> = EntryView::unpack_entire(&buf).groups().collect();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].header.speed, Some(Speed::Bullet));
        assert_eq!(groups[0].subtotal, None);
        assert_eq!(groups[0].cells().count(), 1);
        assert_eq!(groups[1].header.speed, Some(Speed::Blitz));
        let subtotal = groups[1].subtotal.unwrap();
        assert_eq!(subtotal.draws, 2 * SUBTOTAL_MIN_MONTHS as u64);
        assert_eq!(
            subtotal.rating_offset_sum,
            2 * SUBTOTAL_MIN_MONTHS as i64 * i64::from(rating.offset())
        );
        assert_eq!(
            groups[1].newest_month,
            Month::from_months_since_jan_1900(1400 + 3 * (2 * SUBTOTAL_MIN_MONTHS as u16 - 1))
        );

        // Incremental reading, stopping early.
        let mut cells = groups[1].cells();
        let first = cells.next_cell().unwrap();
        assert_eq!(first.key.month(), groups[1].newest_month);
        assert_eq!(first.stats.games(), 1);
        assert_eq!(
            cells.next_game(),
            Some(GameIndex::new(2 * SUBTOTAL_MIN_MONTHS as u64 - 1).unwrap())
        );
        assert_eq!(cells.next_game(), None);
        let second = cells.next_cell().unwrap();
        assert_eq!(
            second.key.month(),
            Month::from_months_since_jan_1900(1400 + 3 * (2 * SUBTOTAL_MIN_MONTHS as u16 - 2))
        );
        // Unread games are skipped.
        assert!(cells.next_cell().is_some());
        assert_eq!(view(&buf), expected_view(&entry));
    }

    #[test]
    fn test_merge_is_adding() {
        arbtest(|u| {
            // Adding all games to one entry is the same as merging entries
            // of consecutive parts.
            let mut all = LichessEntry::new();
            let mut packed_parts = Vec::new();
            for _ in 0..u.int_in_range(0..=4)? {
                let mut part = LichessEntry::new();
                for game in u.arbitrary_iter()? {
                    let game: Game = game?;
                    add(&mut all, game);
                    add(&mut part, game);
                }
                packed_parts.push(pack(&part));
            }
            let parts: Vec<&[u8]> = packed_parts.iter().map(Vec::as_slice).collect();
            assert_eq!(merge(&parts), pack(&all));
            Ok(())
        });
    }

    #[test]
    fn test_merge_is_associative() {
        arbtest(|u| {
            let a = pack(&u.arbitrary()?);
            let b = pack(&u.arbitrary()?);
            let c = pack(&u.arbitrary()?);

            let left = merge(&[&merge(&[&a, &b]), &c]);
            let right = merge(&[&a, &merge(&[&b, &c])]);
            assert_eq!(left, right);
            assert_eq!(left, merge(&[&a, &b, &c]));
            Ok(())
        });
    }
}
