use crate::packed::{GameId, Speed, pack_int, pack_uint, unpack_int, unpack_uint};
use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use bytes::Buf;
use bytes::BufMut;
use rustc_hash::FxHashMap;
use shakmaty::packed::PackedUciMove;
use shakmaty::uci::UciMove;

#[derive(Arbitrary, Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum RatingGroup {
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

    pub fn unpack<B: Buf>(buf: &mut B) -> Self {
        let rating_offset_sum = unpack_int(buf);
        let white = unpack_uint(buf);
        let black = unpack_uint(buf);
        let draws = unpack_uint(buf);

        let game_count = unpack_uint(buf) as usize;
        let mut games = Vec::with_capacity(game_count as usize);
        for _ in 0..game_count {
            let idx = GameIdx(unpack_uint(buf));
            let game_id = GameId::unpack(buf);
            games.push((idx, game_id));
        }

        SubEntry {
            white,
            black,
            draws,
            rating_offset_sum,
            games,
        }
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct LichessEntry {
    moves: FxHashMap<PackedUciMove, FxHashMap<Header, SubEntry>>,
}

impl Arbitrary<'_> for LichessEntry {
    fn arbitrary(u: &mut Unstructured<'_>) -> arbitrary::Result<Self> {
        let moves: FxHashMap<UciMove, FxHashMap<Header, SubEntry>> = u.arbitrary()?;
        Ok(LichessEntry {
            moves: moves
                .into_iter()
                .map(|(m, move_entries)| (PackedUciMove::pack(m), move_entries))
                .collect(),
        })
    }
}

impl LichessEntry {
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

    pub fn unpack_entire<B: Buf>(buf: &mut B) -> Self {
        let mut entry = LichessEntry::default();
        while buf.has_remaining() {
            let mut packed_bytes = [0; 2];
            buf.copy_to_slice(&mut packed_bytes);
            let m = PackedUciMove::from_bytes(packed_bytes);

            let sub_entries = entry.moves.entry(m).or_default();

            while buf.has_remaining() {
                match Header::from_byte(buf.get_u8()) {
                    None => break,
                    Some(header) => {
                        let sub_entry = SubEntry::unpack(buf);
                        sub_entries.insert(header, sub_entry);
                    }
                }
            }
        }
        entry
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
            let unpacked = SubEntry::unpack(&mut &buf[..]);
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
            let unpacked = LichessEntry::unpack_entire(&mut &buf[..]);
            assert_eq!(lichess_entry, unpacked);
            Ok(())
        });
    }
}
