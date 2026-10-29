use std::fmt;

use bytes::{Buf, BufMut};
use shakmaty::{KnownOutcome, packed::PackedUciMove};

use crate::{
    intermediate::GlobalGameSeq,
    packed::{
        CellKey, GameId, GroupHeader, Month, Rating, RatingGroup, Speed, Zobrist96, outcome_bits,
        outcome_from_bits,
    },
};

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct IntermediateEntry(pub [u8; IntermediateEntry::SIZE]);

impl IntermediateEntry {
    pub const SIZE: usize = 30;
    pub const KEY_SIZE: usize = Zobrist96::SIZE;

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hash: Zobrist96,
        month: Month,
        m: PackedUciMove,
        speed: Option<Speed>,
        rating: Rating,
        outcome: KnownOutcome,
        seq: GlobalGameSeq,
        game_id: GameId,
    ) -> IntermediateEntry {
        let header = GroupHeader {
            speed,
            rating_group: RatingGroup::select(rating),
        };
        let mut bytes = [0; Self::SIZE];
        let mut buf = &mut bytes[..];
        hash.pack(&mut buf);
        buf.put_slice(&m.to_bytes());
        buf.put_u8(header.to_byte());
        month.pack(&mut buf);
        buf.put_uint(seq.get() << 2 | u64::from(outcome_bits(outcome)), 5);
        rating.pack_fixed(&mut buf);
        game_id.pack(&mut buf);
        debug_assert!(buf.is_empty());
        IntermediateEntry(bytes)
    }

    #[inline]
    pub fn hash(&self) -> Zobrist96 {
        Zobrist96::unpack(&mut &self.0[..Zobrist96::SIZE])
    }

    #[inline]
    pub fn hash_prefix(&self) -> u64 {
        u64::from_be_bytes(self.0[..8].try_into().expect("8 bytes"))
    }

    #[inline]
    pub fn sort_key(&self) -> (u128, u64) {
        // key() + cell_key() + seq_and_outcome()
        (
            u128::from_be_bytes(self.0[..16].try_into().expect("16 bytes")),
            u64::from_be_bytes(self.0[14..22].try_into().expect("8 bytes")),
        )
    }

    #[inline]
    pub fn key(&self) -> &[u8; Self::KEY_SIZE] {
        self.0[..Self::KEY_SIZE].try_into().expect("key")
    }

    #[inline]
    pub fn month(&self) -> Month {
        Month::unpack(&mut &self.0[15..17])
    }

    #[inline]
    pub fn cell_key(&self) -> CellKey {
        CellKey::from_packed([self.0[12], self.0[13]], self.0[14], self.month())
    }

    #[inline]
    pub fn uci_move(&self) -> PackedUciMove {
        self.cell_key().uci_move()
    }

    #[inline]
    pub fn group_header(&self) -> GroupHeader {
        self.cell_key().group_header()
    }

    #[inline]
    fn seq_and_outcome(&self) -> u64 {
        (&self.0[17..22]).get_uint(5)
    }

    #[inline]
    pub fn seq(&self) -> GlobalGameSeq {
        GlobalGameSeq::new(self.seq_and_outcome() >> 2).expect("38 bits")
    }

    #[inline]
    pub fn outcome(&self) -> KnownOutcome {
        outcome_from_bits((self.seq_and_outcome() & 0b11) as u8).expect("valid outcome")
    }

    #[inline]
    pub fn rating(&self) -> Rating {
        Rating::unpack_fixed(&mut &self.0[22..24])
    }

    #[inline]
    pub fn game_id(&self) -> GameId {
        GameId::try_from(*self.0[24..30].as_array().expect("6 bytes")).expect("valid game id")
    }
}

impl fmt::Debug for IntermediateEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IntermediateEntry")
            .field("hash", &self.hash())
            .field("uci_move", &format_args!("{}", self.uci_move().unpack()))
            .field("group_header", &self.group_header())
            .field("month", &self.month())
            .field("seq", &self.seq())
            .field("outcome", &self.outcome())
            .field("rating", &self.rating())
            .field("game_id", &self.game_id())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;

    #[test]
    fn test_intermediate_entry_roundtrip() {
        arbtest(|u| {
            let hash: Zobrist96 = u.arbitrary()?;
            let month: Month = u.arbitrary()?;
            let m = PackedUciMove::pack(u.arbitrary()?);
            let speed: Option<Speed> = u.arbitrary()?;
            let rating: Rating = u.arbitrary()?;
            let outcome: KnownOutcome = u.arbitrary()?;
            let seq: GlobalGameSeq = u.arbitrary()?;
            let game_id: GameId = u.arbitrary()?;

            let entry =
                IntermediateEntry::new(hash, month, m, speed, rating, outcome, seq, game_id);

            assert_eq!(entry.hash(), hash);
            assert_eq!(entry.month(), month);
            assert_eq!(entry.uci_move(), m);
            assert_eq!(entry.group_header().speed, speed);
            assert_eq!(
                entry.group_header().rating_group,
                RatingGroup::select(rating)
            );
            assert_eq!(entry.seq(), seq);
            assert_eq!(entry.outcome(), outcome);
            assert_eq!(entry.rating(), rating);
            assert_eq!(entry.game_id(), game_id);
            Ok(())
        });
    }
}
