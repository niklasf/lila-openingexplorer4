use arbitrary::Arbitrary;
use bytes::{Buf, BufMut};
use shakmaty::{variant::Variant, zobrist::Zobrist128};

/// The top 96 bits of a position's Zobrist hash, mixed with the variant, in
/// big endian order. The key of the position column families.
#[derive(Arbitrary, Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Zobrist96 {
    inner: [u8; 12],
}

impl Zobrist96 {
    pub const SIZE: usize = 12;

    /// Mixes the variant into the hash. Standard chess (including Chess960)
    /// keeps the plain Zobrist hash, so that the keys of standard positions
    /// match hashes computed elsewhere.
    pub fn new(hash: Zobrist128, variant: Variant) -> Zobrist96 {
        // Fixed random salts.
        let salt: u128 = match variant {
            Variant::Chess => 0,
            Variant::Atomic => 0x8f6f_0a4d_6f0d_bb32_4a7b_4a0f_2f8e_9d11,
            Variant::Antichess => 0x3b7e_c1a0_5d2f_9e4b_8c31_6a2d_f0e5_7b26,
            Variant::KingOfTheHill => 0xd4a2_7c8e_1b5f_3a90_e6d1_9f4c_2b7a_8e35,
            Variant::ThreeCheck => 0x5e91_b3d7_8a2c_4f6e_1d0b_7c5a_9e3f_2a48,
            Variant::Crazyhouse => 0xa7c3_e5f1_2d9b_6a4e_8f0c_3b7d_1e5a_9c62,
            Variant::RacingKings => 0x2c8d_4f7a_9b1e_3d6c_5a0f_8e2b_7d4c_1f93,
            Variant::Horde => 0xe1b5_7a3c_4d8f_2e6a_9c0d_5b1f_8a3e_7d24,
        };
        Zobrist96 {
            inner: (u128::from(hash) ^ salt).to_be_bytes()[..12]
                .try_into()
                .expect("96 bits"),
        }
    }

    pub fn from_bytes(inner: [u8; 12]) -> Zobrist96 {
        Zobrist96 { inner }
    }

    pub fn as_bytes(&self) -> &[u8; 12] {
        &self.inner
    }

    pub fn pack<B: BufMut>(&self, buf: &mut B) {
        buf.put_slice(&self.inner);
    }

    pub fn unpack<B: Buf>(buf: &mut B) -> Self {
        let mut hash = Zobrist96 { inner: [0; 12] };
        buf.copy_to_slice(&mut hash.inner);
        hash
    }

    /// The top 64 bits, for cheap in-memory lookups.
    pub fn prefix(&self) -> u64 {
        u64::from_be_bytes(self.inner[..8].try_into().expect("8 bytes"))
    }

    /// The top `bits` bits (at most 64).
    pub fn partition(&self, bits: u32) -> usize {
        if bits == 0 {
            0
        } else {
            (self.prefix() >> (64 - bits)) as usize
        }
    }

    /// The smallest hash with the given top 64 bits.
    pub fn from_prefix(prefix: u64) -> Zobrist96 {
        let mut inner = [0; 12];
        inner[..8].copy_from_slice(&prefix.to_be_bytes());
        Zobrist96 { inner }
    }

    /// The smallest hash with the given partition index.
    pub fn partition_start(partition: usize, bits: u32) -> Zobrist96 {
        Zobrist96::from_prefix(if bits == 0 {
            0
        } else {
            (partition as u64) << (64 - bits)
        })
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;

    #[test]
    fn test_partition_ord() {
        arbtest(|u| {
            let left: Zobrist96 = u.arbitrary()?;
            let right: Zobrist96 = u.arbitrary()?;
            let bits = u.int_in_range(0..=64)?;
            if left <= right {
                assert!(left.partition(bits) <= right.partition(bits));
            }
            let start = Zobrist96::partition_start(left.partition(bits), bits);
            assert!(start <= left);
            assert_eq!(start.partition(bits), left.partition(bits));
            Ok(())
        });
    }
}
