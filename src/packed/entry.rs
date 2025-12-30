use std::{cmp::max, iter};

use arbitrary::{Arbitrary, Unstructured};
use bytes::{Buf, BufMut};
use rustc_hash::FxHashMap;
use shakmaty::{Color, KnownOutcome, packed::PackedUciMove};

use crate::packed::{GameId, Speed, pack_int, pack_uint, unpack_int, unpack_uint};

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

#[derive(Arbitrary, Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GameIdx(pub u64);

impl GameIdx {
    pub fn add(self, offset: u64) -> GameIdx {
        GameIdx(self.0 + offset)
    }
}

#[derive(Arbitrary, Debug, Clone, PartialEq, Eq, Hash)]
struct Header {
    speed: Speed,
    rating_group: RatingGroup,
}

impl Header {
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

    pub fn from_byte(packed: u8) -> Option<Header> {
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
        Some(Header {
            speed,
            rating_group,
        })
    }
}

#[derive(Arbitrary, Debug, Default, Clone, PartialEq, Eq)]
struct SubEntry {
    rating_offset_sum: i64,
    white: u64,
    black: u64,
    draws: u64,
    games: Vec<(GameIdx, GameId)>,
}

impl SubEntry {
    pub fn pack<B: BufMut>(&self, buf: &mut B) {
        pack_int(buf, self.rating_offset_sum);
        pack_uint(buf, self.white);
        pack_uint(buf, self.black);
        pack_uint(buf, self.draws);
        pack_uint(buf, self.games.len() as u64);
        for (GameIdx(idx), game_id) in &self.games {
            pack_uint(buf, *idx);
            game_id.pack(buf);
        }
    }

    pub fn merge_unpack<B: Buf>(&mut self, buf: &mut B, base_game_idx: GameIdx) -> GameIdx {
        self.rating_offset_sum += unpack_int(buf);
        self.white += unpack_uint(buf);
        self.black += unpack_uint(buf);
        self.draws += unpack_uint(buf);

        let mut next_game_idx = base_game_idx;

        let game_count = unpack_uint(buf) as usize;
        self.games.extend(
            iter::repeat_with(|| {
                let idx = base_game_idx.add(unpack_uint(buf));
                next_game_idx = max(next_game_idx, idx.add(1));
                let game_id = GameId::unpack(buf);
                (idx, game_id)
            })
            .take(game_count),
        );

        next_game_idx
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LichessEntry {
    moves: FxHashMap<PackedUciMove, FxHashMap<Header, SubEntry>>,
    next_game_idx: GameIdx,
}

impl Arbitrary<'_> for LichessEntry {
    fn arbitrary(u: &mut Unstructured<'_>) -> arbitrary::Result<Self> {
        let mut entry = LichessEntry::new();
        for part in u.arbitrary_iter()? {
            let (m, speed, rating_group, outcome, game) = part?;
            entry.add_game(PackedUciMove::pack(m), speed, rating_group, outcome, game);
        }
        Ok(entry)
    }
}

impl LichessEntry {
    pub fn new() -> Self {
        LichessEntry {
            moves: FxHashMap::default(),
            next_game_idx: GameIdx(0),
        }
    }

    pub fn add_game(
        &mut self,
        m: PackedUciMove,
        speed: Speed,
        rating_group: RatingGroup,
        outcome: KnownOutcome,
        game: GameId,
    ) {
        let sub_entry = self
            .moves
            .entry(m)
            .or_default()
            .entry(Header {
                speed,
                rating_group,
            })
            .or_default();

        match outcome {
            KnownOutcome::Decisive {
                winner: Color::White,
            } => sub_entry.white += 1,
            KnownOutcome::Decisive {
                winner: Color::Black,
            } => sub_entry.black += 1,
            KnownOutcome::Draw => sub_entry.draws += 1,
        }

        sub_entry.games.push((self.next_game_idx, game));
        self.next_game_idx = self.next_game_idx.add(1);
    }

    pub fn pack_entire<B: BufMut>(&self, buf: &mut B) {
        for (i, (m, move_entries)) in self.moves.iter().enumerate() {
            if i > 0 {
                buf.put_u8(0);
            }
            buf.put_slice(&m.to_bytes());
            for (header, sub_entry) in move_entries {
                buf.put_u8(header.to_byte());
                sub_entry.pack(buf);
            }
        }
    }

    pub fn merge_unpack_entire<B: Buf>(&mut self, buf: &mut B) {
        let base_game_idx = self.next_game_idx;

        while buf.has_remaining() {
            let mut packed_bytes = [0; 2];
            buf.copy_to_slice(&mut packed_bytes);
            let m = PackedUciMove::from_bytes(packed_bytes);

            let sub_entries = self.moves.entry(m).or_default();

            while buf.has_remaining() {
                match Header::from_byte(buf.get_u8()) {
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
                    idx.0 -= deleted_indexes.partition_point(|deleted| deleted < idx) as u64;
                }
            }
        }

        self.next_game_idx.0 -= deleted_indexes.len() as u64;
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;

    #[test]
    fn test_pack_sub_entry_roundtrip() {
        arbtest(|u| {
            let sub_entry: SubEntry = u.arbitrary()?;

            let mut buf = Vec::new();
            sub_entry.pack(&mut buf);
            let mut unpacked = SubEntry::default();
            unpacked.merge_unpack(&mut &buf[..], GameIdx(0));
            assert_eq!(sub_entry, unpacked);
            Ok(())
        });
    }

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
    fn test_merge_unpack() {
        arbtest(|u| {
            let mut lichess_entry = LichessEntry::new();

            for merge_entry in u.arbitrary_iter()? {
                let merge_entry: LichessEntry = merge_entry?;
                let total_games = lichess_entry.next_game_idx.0 + merge_entry.next_game_idx.0;

                let mut buf = Vec::new();
                merge_entry.pack_entire(&mut buf);
                lichess_entry.merge_unpack_entire(&mut &buf[..]);

                assert_eq!(lichess_entry.next_game_idx.0, total_games);
            }

            Ok(())
        });
    }
}
