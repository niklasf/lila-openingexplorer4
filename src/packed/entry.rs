use std::{cmp::max, iter};

use arbitrary::{Arbitrary, Unstructured};
use bytes::{Buf, BufMut};
use rustc_hash::FxHashMap;
use shakmaty::{Color, KnownOutcome, packed::PackedUciMove};

use crate::packed::{GameId, Rating, Speed, pack_int, pack_uint, unpack_int, unpack_uint};

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
}

/// Used to preserve chronological order across the sub-entries of a
/// `LichessEntry`.
#[derive(Arbitrary, Default, Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GameIdx(pub usize);

impl GameIdx {
    pub fn add(self, offset: usize) -> GameIdx {
        GameIdx(self.0 + offset)
    }
}

#[derive(Arbitrary, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct GroupHeader {
    pub speed: Speed,
    pub rating_group: RatingGroup,
}

impl GroupHeader {
    pub fn to_byte(&self) -> u8 {
        let packed_speed = match self.speed {
            Speed::UltraBullet => 1,
            Speed::Bullet => 2,
            Speed::Blitz => 3,
            Speed::Rapid => 4,
            Speed::Classical => 5,
            Speed::Correspondence => 6,
        };
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
            0 => return None,
            1 => Speed::UltraBullet,
            2 => Speed::Bullet,
            3 => Speed::Blitz,
            4 => Speed::Rapid,
            5 => Speed::Classical,
            6 => Speed::Correspondence,
            _ => panic!("invalid speed"),
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
            _ => panic!("invalid rating group"),
        };
        Some(GroupHeader {
            speed,
            rating_group,
        })
    }
}

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

    pub fn pack<B: BufMut>(&self, buf: &mut B) {
        pack_int(buf, self.rating_offset_sum);
        pack_uint(buf, self.white);
        pack_uint(buf, self.black);
        pack_uint(buf, self.draws);
    }

    pub fn merge_unpack<B: Buf>(&mut self, buf: &mut B) {
        self.rating_offset_sum += unpack_int(buf);
        self.white += unpack_uint(buf);
        self.black += unpack_uint(buf);
        self.draws += unpack_uint(buf);
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SubEntry {
    stats: SubStats,
    games: Vec<(GameIdx, GameId)>,
}

impl SubEntry {
    pub fn merge_unpack<B: Buf>(&mut self, buf: &mut B, base_game_idx: GameIdx) -> GameIdx {
        self.stats.merge_unpack(buf);

        let mut next_game_idx = base_game_idx;

        let game_count = unpack_uint(buf) as usize;
        self.games.extend(
            iter::repeat_with(|| {
                let idx = base_game_idx.add(unpack_uint(buf) as usize);
                next_game_idx = max(next_game_idx, idx.add(1));
                let game_id = GameId::unpack(buf);
                (idx, game_id)
            })
            .take(game_count),
        );

        next_game_idx
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct LichessEntry {
    moves: FxHashMap<PackedUciMove, FxHashMap<GroupHeader, SubEntry>>,
    next_game_idx: GameIdx,
}

impl Arbitrary<'_> for LichessEntry {
    fn arbitrary(u: &mut Unstructured<'_>) -> arbitrary::Result<Self> {
        let mut entry = LichessEntry::new();
        for part in u.arbitrary_iter()? {
            let (m, speed, rating, outcome, game) = part?;
            entry.add_game(PackedUciMove::pack(m), speed, rating, outcome, game);
        }
        Ok(entry)
    }
}

impl LichessEntry {
    pub fn new() -> Self {
        LichessEntry::default()
    }

    pub fn add_game(
        &mut self,
        m: PackedUciMove,
        speed: Speed,
        rating: Rating,
        outcome: KnownOutcome,
        game: GameId,
    ) {
        let sub_entry = self
            .moves
            .entry(m)
            .or_default()
            .entry(GroupHeader {
                speed,
                rating_group: RatingGroup::select(rating),
            })
            .or_default();

        sub_entry.stats.add(rating, outcome);
        sub_entry.games.push((self.next_game_idx, game));
        self.next_game_idx = self.next_game_idx.add(1);
    }

    pub fn pack_entire<B: BufMut>(&self, buf: &mut B) {
        Self::pack_entire_external(
            buf,
            self.moves.iter().flat_map(|(m, move_entries)| {
                move_entries.iter().map(|(header, sub_entry)| {
                    (*m, *header, &sub_entry.stats, &sub_entry.games[..])
                })
            }),
        );
    }

    pub fn merge_unpack_entire<B: Buf>(&mut self, buf: &mut B) {
        let base_game_idx = self.next_game_idx;

        while buf.has_remaining() {
            let mut packed_bytes = [0; 2];
            buf.copy_to_slice(&mut packed_bytes);
            let m = PackedUciMove::from_bytes(packed_bytes);

            let sub_entries = self.moves.entry(m).or_default();

            while buf.has_remaining() {
                match GroupHeader::from_byte(buf.get_u8()) {
                    None => break,
                    Some(header) => {
                        self.next_game_idx = max(
                            self.next_game_idx,
                            sub_entries
                                .entry(header)
                                .or_default()
                                .merge_unpack(buf, base_game_idx),
                        );
                    }
                }
            }
        }
    }

    pub fn truncate_games(&mut self, limit: usize) {
        let mut deleted_indexes = Vec::new();
        for move_entries in self.moves.values_mut() {
            for sub_entry in move_entries.values_mut() {
                deleted_indexes.extend(
                    sub_entry
                        .games
                        .drain(..sub_entry.games.len().saturating_sub(limit))
                        .map(|(idx, _)| idx),
                )
            }
        }

        deleted_indexes.sort_unstable();
        for move_entries in self.moves.values_mut() {
            for sub_entry in move_entries.values_mut() {
                for (idx, _) in &mut sub_entry.games {
                    idx.0 -= deleted_indexes.partition_point(|deleted| deleted < idx);
                }
            }
        }

        self.next_game_idx.0 -= deleted_indexes.len();
    }

    /// Packs an entry from sub-entries, which need to be already grouped
    /// like `LichessEntry` does.
    ///
    /// `K` is used to maintain the chronological order of games across
    /// sub-entries (`GameIdx` or `GlobalGameIdx`).
    pub fn pack_entire_external<'a, B: BufMut, K: Ord + Copy + 'a, I>(buf: &mut B, subs: I)
    where
        I: Iterator<Item = (PackedUciMove, GroupHeader, &'a SubStats, &'a [(K, GameId)])> + Clone,
    {
        let mut keys: Vec<K> = subs
            .clone()
            .flat_map(|(_, _, _, games)| games.iter().map(|(key, _)| *key))
            .collect();
        keys.sort_unstable();

        let mut current_move: Option<PackedUciMove> = None;
        for (m, header, stats, games) in subs {
            if current_move != Some(m) {
                if current_move.is_some() {
                    buf.put_u8(0);
                }
                buf.put_slice(&m.to_bytes());
                current_move = Some(m);
            }
            buf.put_u8(header.to_byte());
            stats.pack(buf);
            pack_uint(buf, games.len() as u64);
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

    #[test]
    fn test_lichess_entry_roundtrip() {
        arbtest(|u| {
            let lichess_entry: LichessEntry = u.arbitrary()?;

            let mut buf = Vec::new();
            lichess_entry.pack_entire(&mut buf);
            let mut unpacked = LichessEntry::new();
            unpacked.merge_unpack_entire(&mut &buf[..]);
            assert_eq!(lichess_entry, unpacked);
            Ok(())
        });
    }

    #[test]
    fn test_merge_unpack_truncate() {
        arbtest(|u| {
            let mut lichess_entry = LichessEntry::new();

            for merge_entry in u.arbitrary_iter()? {
                let merge_entry: LichessEntry = merge_entry?;

                // Merge
                let total_games = lichess_entry.next_game_idx.0 + merge_entry.next_game_idx.0;
                let mut buf = Vec::new();
                merge_entry.pack_entire(&mut buf);
                lichess_entry.merge_unpack_entire(&mut &buf[..]);
                assert_eq!(lichess_entry.next_game_idx.0, total_games);

                // Check invariants
                lichess_entry.truncate_games(2);
                let mut indexes: Vec<GameIdx> = Vec::new();
                for move_entries in lichess_entry.moves.values() {
                    for sub_entry in move_entries.values() {
                        assert!(sub_entry.games.is_sorted());
                        indexes.extend(sub_entry.games.iter().map(|(idx, _)| idx));
                    }
                }
                indexes.sort_unstable();
                assert_eq!(indexes, (0..indexes.len()).map(GameIdx).collect::<Vec<_>>());
            }

            Ok(())
        });
    }
}
