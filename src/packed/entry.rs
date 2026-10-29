use std::{cmp::max, collections::BTreeMap, ops::Add};

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

/// Used to preserve chronological order across the groups of a
/// `LichessEntry`.
#[derive(Arbitrary, Default, Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GameSeq(pub usize);

impl Add<usize> for GameSeq {
    type Output = GameSeq;

    fn add(self, rhs: usize) -> Self::Output {
        GameSeq(self.0 + rhs)
    }
}

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

/// The games of a cell that are packed: the most recent ones, exactly
/// `min(GAMES_PER_CELL, games of the cell)` of them. Panics if
/// fewer are given.
fn packed_games<'a, K>(stats: &SubStats, games: &'a [(K, GameId)]) -> &'a [(K, GameId)] {
    let n = stats.games().min(GAMES_PER_CELL as u64) as usize;
    assert!(games.len() >= n, "cell is missing games");
    &games[games.len() - n..]
}

/// Move, group header and month, ordered canonically.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CellKey {
    m: [u8; 2],
    header: u8,
    month: Month,
}

/// Statistics and most recent games of a month of a group.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Cell {
    stats: SubStats,
    /// In chronological order.
    games: Vec<(GameSeq, GameId)>,
}

impl Cell {
    fn truncate_games(&mut self) {
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
/// ones. The rank of a game is its chronological position among all packed
/// games of the entry.
///
/// Merging is associative: each merged entry is considered more recent than
/// the entry it is merged into.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct LichessEntry {
    cells: BTreeMap<CellKey, Cell>,
    next_game_seq: GameSeq,
}

impl Arbitrary<'_> for LichessEntry {
    fn arbitrary(u: &mut Unstructured<'_>) -> arbitrary::Result<Self> {
        let mut entry = LichessEntry::new();
        for part in u.arbitrary_iter()? {
            let (m, speed, rating, outcome, month, game) = part?;
            entry.add_game(PackedUciMove::pack(m), speed, rating, outcome, month, game);
        }
        Ok(entry)
    }
}

impl LichessEntry {
    pub fn new() -> Self {
        LichessEntry::default()
    }

    /// Adds a game, more recent than all games added before.
    pub fn add_game(
        &mut self,
        m: PackedUciMove,
        speed: Option<Speed>,
        rating: Rating,
        outcome: KnownOutcome,
        month: Month,
        game: GameId,
    ) {
        let header = GroupHeader {
            speed,
            rating_group: RatingGroup::select(rating),
        };
        let cell = self
            .cells
            .entry(CellKey {
                m: m.to_bytes(),
                header: header.to_byte(),
                month,
            })
            .or_default();

        cell.stats.add(rating, outcome);
        cell.games.push((self.next_game_seq, game));
        cell.truncate_games();
        self.next_game_seq = self.next_game_seq.add(1);
    }

    pub fn pack_entire(&self, buf: &mut Vec<u8>) {
        Self::pack_entire_external(
            buf,
            self.cells.iter().map(|(key, cell)| {
                (
                    PackedUciMove::from_bytes(key.m),
                    GroupHeader::from_byte(key.header).expect("valid group header"),
                    key.month,
                    &cell.stats,
                    &cell.games[..],
                )
            }),
        );
    }

    /// Merges a packed entry into this entry. Its games are considered more
    /// recent than all games of this entry.
    pub fn merge_unpack_entire<B: Buf>(&mut self, buf: &mut B) {
        let base_game_seq = self.next_game_seq;

        while buf.has_remaining() {
            let mut m = [0; 2];
            buf.copy_to_slice(&mut m);

            loop {
                let byte = buf.get_u8();
                let header =
                    GroupHeader::from_byte(byte & !LAST_GROUP_OF_MOVE).expect("valid group header");

                let mut month = 0;
                loop {
                    let tagged = unpack_uint(buf);
                    month += tagged >> 1;
                    let stats = SubStats::unpack(buf, header.rating_group);
                    let cell = self
                        .cells
                        .entry(CellKey {
                            m,
                            header: header.to_byte(),
                            month: Month::from_months_since_jan_1900(
                                u16::try_from(month).expect("valid month"),
                            ),
                        })
                        .or_default();
                    cell.stats.merge(&stats);
                    for _ in 0..stats.games().min(GAMES_PER_CELL as u64) {
                        let seq = base_game_seq.add(unpack_uint(buf) as usize);
                        self.next_game_seq = max(self.next_game_seq, seq.add(1));
                        cell.games.push((seq, GameId::unpack(buf)));
                    }
                    cell.truncate_games();

                    if tagged & 1 == 0 {
                        break;
                    }
                }

                if byte & LAST_GROUP_OF_MOVE != 0 {
                    break;
                }
            }
        }
    }

    /// Packs an entry from the months of all groups, which need to be in
    /// canonical order (by move bytes, then by header byte, then by month),
    /// each with at least its packed games (see `LichessEntry`) in
    /// chronological order.
    ///
    /// `K` is used to maintain the chronological order of games across
    /// groups and months (`GameSeq` or month and `GlobalGameSeq`).
    pub fn pack_entire_external<'a, K, I>(buf: &mut Vec<u8>, cells: I)
    where
        K: Ord + Copy + 'a,
        I: Iterator<
                Item = (
                    PackedUciMove,
                    GroupHeader,
                    Month,
                    &'a SubStats,
                    &'a [(K, GameId)],
                ),
            > + Clone,
    {
        let mut keys: Vec<K> = cells
            .clone()
            .flat_map(|(_, _, _, stats, games)| {
                packed_games(stats, games).iter().map(|(key, _)| *key)
            })
            .collect();
        keys.sort_unstable();

        let mut prev: Option<CellKey> = None;
        let mut header_index = 0;
        let mut cells = cells.peekable();
        while let Some((m, header, month, stats, games)) = cells.next() {
            let key = CellKey {
                m: m.to_bytes(),
                header: header.to_byte(),
                month,
            };
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

            let next = cells
                .peek()
                .map(|(m, header, ..)| (m.to_bytes(), header.to_byte()));
            let more_months = next == Some((key.m, key.header));
            if next.is_none_or(|(next_m, _)| next_m != key.m) {
                buf[header_index] |= LAST_GROUP_OF_MOVE;
            }

            let month = month.months_since_jan_1900();
            pack_uint(
                buf,
                u64::from(month - prev_month) << 1 | u64::from(more_months),
            );
            stats.pack(buf, header.rating_group);

            let games = packed_games(stats, games);
            debug_assert!(games.is_sorted_by(|a, b| a.0 < b.0), "games not in order");
            for (key, game_id) in games {
                pack_uint(buf, keys.partition_point(|k| k < key) as u64);
                game_id.pack(buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;

    fn pack(entry: &LichessEntry) -> Vec<u8> {
        let mut buf = Vec::new();
        entry.pack_entire(&mut buf);
        buf
    }

    fn merge(packed: &[&[u8]]) -> Vec<u8> {
        let mut entry = LichessEntry::new();
        for buf in packed {
            entry.merge_unpack_entire(&mut &buf[..]);
        }
        pack(&entry)
    }

    /// Statistics of all months of all groups, and their packed games in
    /// chronological order.
    #[allow(clippy::type_complexity)]
    fn view(entry: &LichessEntry) -> (Vec<(CellKey, SubStats)>, Vec<(CellKey, GameId)>) {
        let stats = entry
            .cells
            .iter()
            .map(|(key, cell)| (*key, cell.stats))
            .collect();
        let mut games: Vec<_> = entry
            .cells
            .iter()
            .flat_map(|(key, cell)| {
                packed_games(&cell.stats, &cell.games)
                    .iter()
                    .map(|(seq, game_id)| (*seq, *key, *game_id))
            })
            .collect();
        games.sort_unstable();
        (
            stats,
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
            let mut unpacked = LichessEntry::new();
            unpacked.merge_unpack_entire(&mut &buf[..]);
            assert_eq!(view(&unpacked), view(&lichess_entry));
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
            let mut packed_parts = Vec::new();
            for _ in 0..u.int_in_range(0..=4)? {
                let mut part = LichessEntry::new();
                for game in u.arbitrary_iter()? {
                    let (m, speed, rating, outcome, month, game_id) = game?;
                    let m = PackedUciMove::pack(m);
                    all.add_game(m, speed, rating, outcome, month, game_id);
                    part.add_game(m, speed, rating, outcome, month, game_id);
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
