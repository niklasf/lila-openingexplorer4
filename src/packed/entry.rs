use std::collections::BTreeMap;

use arbitrary::{Arbitrary, Unstructured};
use bytes::{Buf, BufMut};
use shakmaty::{Color, KnownOutcome, packed::PackedUciMove};

use crate::packed::{GameId, Month, Rating, Speed, pack_int, pack_uint, unpack_int, unpack_uint};

/// Each cell (move, group and month) keeps only its most recent games, up to
/// this many.
pub const GAMES_PER_CELL: usize = 8;

#[derive(Arbitrary, Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RatingGroup {
    Low,
    Group1000,
    Group1200,
    Group1400,
    Group1600,
    Group1800,
    Group2000,
    Group2200,
    Group2500,
    Group2800,
    Group3200,
}

impl RatingGroup {
    pub fn select(rating: Rating) -> RatingGroup {
        let rating = i32::from(rating);

        if rating < 1000 {
            RatingGroup::Low
        } else if rating < 1200 {
            RatingGroup::Group1000
        } else if rating < 1400 {
            RatingGroup::Group1200
        } else if rating < 1600 {
            RatingGroup::Group1400
        } else if rating < 1800 {
            RatingGroup::Group1600
        } else if rating < 2000 {
            RatingGroup::Group1800
        } else if rating < 2200 {
            RatingGroup::Group2000
        } else if rating < 2500 {
            RatingGroup::Group2200
        } else if rating < 2800 {
            RatingGroup::Group2500
        } else if rating < 3200 {
            RatingGroup::Group2800
        } else {
            RatingGroup::Group3200
        }
    }

    fn typical_rating(self) -> Rating {
        Rating::try_from(match self {
            RatingGroup::Low => 884,
            RatingGroup::Group1000 => 1111,
            RatingGroup::Group1200 => 1307,
            RatingGroup::Group1400 => 1504,
            RatingGroup::Group1600 => 1701,
            RatingGroup::Group1800 => 1896,
            RatingGroup::Group2000 => 2089,
            RatingGroup::Group2200 => 2310,
            RatingGroup::Group2500 => 2600,
            RatingGroup::Group2800 => 2925,
            RatingGroup::Group3200 => 3269,
        })
        .expect("typical rating")
    }
}

/// Orders games of the same month within a `LichessEntry`: games are ranked
/// by month, then by this number. Numbers are only comparable within one
/// entry. Packing condenses them into ranks, and merging continues after the
/// largest number of the entry.
#[derive(Arbitrary, Default, Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GameSeq(pub u64);

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

    /// Packs the counts, then the rating sum relative to the typical rating
    /// of the group. The total is not stored, but derived from the counts.
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

/// A cell to be packed: its statistics and games, ordered by `GameSeq`.
/// At least the most recent `min(GAMES_PER_CELL, games of the cell)` games
/// must be given.
#[derive(Debug, Copy, Clone)]
pub struct EntryCell<'a> {
    pub key: CellKey,
    pub stats: SubStats,
    pub games: &'a [(GameSeq, GameId)],
}

impl<'a> EntryCell<'a> {
    /// The games that are packed: the most recent ones, exactly
    /// `min(GAMES_PER_CELL, games of the cell)` of them.
    fn packed_games(&self) -> &'a [(GameSeq, GameId)] {
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
    games: Vec<(GameSeq, GameId)>,
}

impl Cell {
    fn push_game(&mut self, seq: GameSeq, game_id: GameId) {
        self.games.push((seq, game_id));
        if self.games.len() >= 4 * GAMES_PER_CELL {
            self.truncate_games();
        }
    }

    /// Sorts the games and keeps only the most recent ones.
    fn truncate_games(&mut self) {
        self.games.sort_unstable_by_key(|(seq, _)| *seq);
        self.games
            .drain(..self.games.len().saturating_sub(GAMES_PER_CELL));
    }
}

/// Statistics and recent games of the moves played from a position, by
/// speed, rating group and month.
///
/// Packed format, in canonical order (moves by packed bytes, groups by
/// header byte, months ascending):
///
/// ```text
/// entry     := (move group+)*
/// move      := PackedUciMove (2 bytes)
/// group     := header month_rec+
/// header    := GroupHeader::to_byte() | LAST_GROUP_OF_MOVE if last group of the move
/// month_rec := uint(month delta << 1 | 1 if another month_rec follows)
///              SubStats
///              game*
/// game      := uint(rank) GameId (6 bytes)
/// ```
///
/// Each month_rec is a cell: the games of a month for a move and group. The
/// first month delta of a group is relative to 1900-01. Each cell has
/// exactly `min(GAMES_PER_CELL, games of the cell)` games, the most recent
/// ones. The rank of a game is its chronological position (by month, then
/// `GameSeq`) among all packed games of the entry.
///
/// Merging is associative: within a month, the games of a merged entry are
/// considered more recent than the games of the entry it is merged into.
#[derive(Default, Debug, Clone)]
pub struct LichessEntry {
    cells: BTreeMap<CellKey, Cell>,
}

impl Arbitrary<'_> for LichessEntry {
    fn arbitrary(u: &mut Unstructured<'_>) -> arbitrary::Result<Self> {
        let mut entry = LichessEntry::new();
        for (seq, part) in u.arbitrary_iter()?.enumerate() {
            let (m, speed, rating, outcome, month, game_id) = part?;
            let header = GroupHeader {
                speed,
                rating_group: RatingGroup::select(rating),
            };
            entry.add_game(
                CellKey::new(PackedUciMove::pack(m), header, month),
                rating,
                outcome,
                GameSeq(seq as u64),
                game_id,
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
        seq: GameSeq,
        game_id: GameId,
    ) {
        debug_assert_eq!(
            key.group_header().rating_group,
            RatingGroup::select(rating),
            "rating does not match cell"
        );
        let cell = self.cells.entry(key).or_default();
        cell.stats.add(rating, outcome);
        cell.push_game(seq, game_id);
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

    /// Merges a packed entry into this entry. Within a month, its games are
    /// considered more recent than all games of this entry.
    pub fn merge_unpack_entire(&mut self, bytes: &[u8]) {
        let base = self
            .cells
            .values()
            .flat_map(|cell| cell.games.iter().map(|(seq, _)| seq.0 + 1))
            .max()
            .unwrap_or(0);

        for view in EntryView::unpack_entire(bytes).cells() {
            let cell = self.cells.entry(view.key).or_default();
            cell.stats.merge(&view.stats);
            for (rank, game_id) in view.games {
                cell.push_game(GameSeq(base + rank), game_id);
            }
        }
    }

    /// Packs cells, which need to be in canonical order (by move bytes, then
    /// by header byte, then by month).
    pub fn pack_cells_entire<'a, I>(buf: &mut Vec<u8>, cells: I)
    where
        I: Iterator<Item = EntryCell<'a>> + Clone,
    {
        let mut keys: Vec<(Month, GameSeq)> = cells
            .clone()
            .flat_map(|cell| {
                cell.packed_games()
                    .iter()
                    .map(move |(seq, _)| (cell.key.month, *seq))
            })
            .collect();
        keys.sort_unstable();

        let mut prev: Option<CellKey> = None;
        let mut header_index = 0;
        let mut cells = cells.peekable();
        while let Some(cell) = cells.next() {
            let key = cell.key;
            debug_assert!(prev < Some(key), "cells not in canonical order");
            let new_move = prev.is_none_or(|prev| prev.m != key.m);
            let prev_month = match prev {
                Some(prev) if !new_move && prev.header == key.header => {
                    prev.month.months_since_jan_1900()
                }
                _ => {
                    if new_move {
                        buf.put_slice(&key.m);
                    }
                    header_index = buf.len();
                    buf.put_u8(key.header);
                    0
                }
            };
            prev = Some(key);

            let next = cells.peek().map(|next| (next.key.m, next.key.header));
            let more_months = next == Some((key.m, key.header));
            if next.is_none_or(|(next_m, _)| next_m != key.m) {
                buf[header_index] |= LAST_GROUP_OF_MOVE;
            }

            let month = key.month.months_since_jan_1900();
            pack_uint(
                buf,
                u64::from(month - prev_month) << 1 | u64::from(more_months),
            );
            cell.stats.pack(buf, key.group_header().rating_group);

            let games = cell.packed_games();
            debug_assert!(games.is_sorted_by(|a, b| a.0 < b.0), "games not in order");
            for (seq, game_id) in games {
                pack_uint(buf, keys.partition_point(|k| *k < (key.month, *seq)) as u64);
                game_id.pack(buf);
            }
        }
    }
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

    /// The cells, in canonical order.
    pub fn cells(&self) -> CellViews<'a> {
        CellViews {
            buf: self.bytes,
            m: [0; 2],
            header: 0,
            rating_group: RatingGroup::Low,
            month: 0,
            more_groups: false,
            more_months: false,
        }
    }
}

/// A cell of a packed entry, read in place.
#[derive(Debug, Clone)]
pub struct CellView<'a> {
    pub key: CellKey,
    pub stats: SubStats,
    /// The most recent `min(GAMES_PER_CELL, games of the cell)` games, with
    /// their rank in the entry.
    pub games: CellGames<'a>,
}

/// Iterator over the cells of a packed entry.
#[derive(Debug, Clone)]
pub struct CellViews<'a> {
    buf: &'a [u8],
    m: [u8; 2],
    header: u8,
    rating_group: RatingGroup,
    month: u16,
    more_groups: bool,
    more_months: bool,
}

impl<'a> Iterator for CellViews<'a> {
    type Item = CellView<'a>;

    fn next(&mut self) -> Option<CellView<'a>> {
        if !self.more_months {
            if !self.more_groups {
                if self.buf.is_empty() {
                    return None;
                }
                self.buf.copy_to_slice(&mut self.m);
            }
            let byte = self.buf.get_u8();
            self.header = byte & !LAST_GROUP_OF_MOVE;
            self.rating_group = GroupHeader::from_byte(self.header)
                .expect("valid group header")
                .rating_group;
            self.more_groups = byte & LAST_GROUP_OF_MOVE == 0;
            self.month = 0;
        }

        let tagged = unpack_uint(&mut self.buf);
        self.month += u16::try_from(tagged >> 1).expect("valid month");
        self.more_months = tagged & 1 != 0;
        let stats = SubStats::unpack(&mut self.buf, self.rating_group);

        let start = self.buf;
        for _ in 0..stats.games().min(GAMES_PER_CELL as u64) {
            unpack_uint(&mut self.buf);
            self.buf.advance(GameId::SIZE);
        }
        let games = &start[..start.len() - self.buf.len()];

        Some(CellView {
            key: CellKey {
                m: self.m,
                header: self.header,
                month: Month::from_months_since_jan_1900(self.month),
            },
            stats,
            games: CellGames { buf: games },
        })
    }
}

/// Iterator over the packed games of a cell, with their rank in the entry.
#[derive(Debug, Clone)]
pub struct CellGames<'a> {
    buf: &'a [u8],
}

impl Iterator for CellGames<'_> {
    type Item = (u64, GameId);

    fn next(&mut self) -> Option<(u64, GameId)> {
        if self.buf.is_empty() {
            return None;
        }
        let rank = unpack_uint(&mut self.buf);
        Some((rank, GameId::unpack(&mut self.buf)))
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

    type Game = (UciMove, Option<Speed>, Rating, KnownOutcome, Month, GameId);

    fn add(entry: &mut LichessEntry, (m, speed, rating, outcome, month, game_id): Game, seq: u64) {
        let header = GroupHeader {
            speed,
            rating_group: RatingGroup::select(rating),
        };
        entry.add_game(
            CellKey::new(PackedUciMove::pack(m), header, month),
            rating,
            outcome,
            GameSeq(seq),
            game_id,
        );
    }

    /// Statistics of all cells, and their packed games in chronological
    /// order.
    #[allow(clippy::type_complexity)]
    fn view(bytes: &[u8]) -> (Vec<(CellKey, SubStats)>, Vec<(CellKey, GameId)>) {
        let cells: Vec<_> = EntryView::unpack_entire(bytes).cells().collect();
        let mut games: Vec<_> = cells
            .iter()
            .flat_map(|cell| {
                cell.games
                    .clone()
                    .map(|(rank, game_id)| (rank, cell.key, game_id))
            })
            .collect();
        games.sort_unstable_by_key(|(rank, ..)| *rank);
        assert!(
            games
                .iter()
                .enumerate()
                .all(|(i, (rank, ..))| i as u64 == *rank)
        );
        (
            cells.iter().map(|cell| (cell.key, cell.stats)).collect(),
            games
                .into_iter()
                .map(|(_, key, game_id)| (key, game_id))
                .collect(),
        )
    }

    /// Like `view`, but from the model.
    #[allow(clippy::type_complexity)]
    fn expected_view(entry: &LichessEntry) -> (Vec<(CellKey, SubStats)>, Vec<(CellKey, GameId)>) {
        let mut entry = entry.clone();
        for cell in entry.cells.values_mut() {
            cell.truncate_games();
        }
        let mut games: Vec<_> = entry
            .cells
            .iter()
            .flat_map(|(key, cell)| {
                cell.games
                    .iter()
                    .map(|(seq, game_id)| ((key.month, *seq), *key, *game_id))
            })
            .collect();
        games.sort_unstable_by_key(|(order, ..)| *order);
        (
            entry
                .cells
                .iter()
                .map(|(key, cell)| (*key, cell.stats))
                .collect(),
            games
                .into_iter()
                .map(|(_, key, game_id)| (key, game_id))
                .collect(),
        )
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
    fn test_merge_is_adding() {
        arbtest(|u| {
            // Adding all games to one entry is the same as merging entries
            // of consecutive parts.
            let mut all = LichessEntry::new();
            let mut seq = 0;
            let mut packed_parts = Vec::new();
            for _ in 0..u.int_in_range(0..=4)? {
                let mut part = LichessEntry::new();
                for (part_seq, game) in u.arbitrary_iter()?.enumerate() {
                    let game: Game = game?;
                    add(&mut all, game, seq);
                    add(&mut part, game, part_seq as u64);
                    seq += 1;
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
