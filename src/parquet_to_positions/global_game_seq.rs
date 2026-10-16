use arbitrary::{Arbitrary, Unstructured};

/// Used to maintain the order of games as in the input.
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
}

impl Arbitrary<'_> for GlobalGameSeq {
    fn arbitrary(u: &mut Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(GlobalGameSeq(u.int_in_range(0..=GlobalGameSeq::MAX.0)?))
    }
}
