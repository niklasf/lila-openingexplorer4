use arbitrary::{Arbitrary, Unstructured};

use crate::packed::GameIndex;

/// The row of a game in the parquet input, used to maintain the order of
/// games as in the input, and as its `GameIndex`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GlobalGameSeq(u64);

impl GlobalGameSeq {
    pub const BITS: u32 = 38;
    pub const MAX: GlobalGameSeq = GlobalGameSeq((1 << GlobalGameSeq::BITS) - 1);

    pub fn new(seq: u64) -> Option<GlobalGameSeq> {
        Some(GlobalGameSeq(seq)).filter(|seq| *seq <= Self::MAX)
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn game_index(self) -> GameIndex {
        GameIndex::new(self.0).expect("game index")
    }
}

impl Arbitrary<'_> for GlobalGameSeq {
    fn arbitrary(u: &mut Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(GlobalGameSeq(u.int_in_range(0..=GlobalGameSeq::MAX.0)?))
    }
}
