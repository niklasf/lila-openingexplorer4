use crate::packed::{GameId, Speed, pack_int, pack_uint, unpack_int, unpack_uint};
use arbitrary::Arbitrary;
use bytes::Buf;
use bytes::BufMut;
use rustc_hash::FxHashMap;
use shakmaty::packed::PackedUciMove;

enum RatingGroup {}

#[derive(Arbitrary, Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GameIdx(pub u64);

struct Header {
    speed: Speed,
    rating_group: RatingGroup,
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

struct LichessEntry {
    moves: FxHashMap<PackedUciMove, FxHashMap<Header, SubEntry>>,
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use crate::packed::entry::SubEntry;

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
}
