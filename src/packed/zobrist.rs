use arbitrary::Arbitrary;
use bytes::{Buf, BufMut};
use shakmaty::{variant::Variant, zobrist::Zobrist128};

#[derive(Arbitrary, Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Zobrist96 {
    inner: [u8; Zobrist96::SIZE],
}

impl Zobrist96 {
    pub const SIZE: usize = 12;

    pub fn new(hash: Zobrist128, variant: Variant) -> Zobrist96 {
        let variant_hash = Zobrist128(match variant {
            Variant::Chess => 0,
            Variant::Atomic => 0x448f_8a94_c263_0790_6c4d_732b_52bf_d09c,
            Variant::Antichess => 0xc37c_461f_0c94_0a7a_ed6e_5574_5b53_36a8,
            Variant::KingOfTheHill => 0xcfc7_524f_c49e_0cc8_3424_ce23_54a3_0465,
            Variant::ThreeCheck => 0x125f_3ede_51b1_6a4f_1a83_259a_78ae_c28b,
            Variant::Crazyhouse => 0x123d_7313_ce55_0520_d96a_8f0b_7ff9_a3eb,
            Variant::RacingKings => 0x17d0_49d2_5cdb_93df_97e4_5dc6_26d4_f3ce,
            Variant::Horde => 0x2b0e_f487_527a_f875_8e8e_2bfa_05bd_d376,
        });

        let mut inner = [0; Zobrist96::SIZE];
        inner.copy_from_slice(&u128::from(hash ^ variant_hash).to_be_bytes()[..Zobrist96::SIZE]);
        Zobrist96 { inner }
    }

    pub fn from_bytes(inner: [u8; Zobrist96::SIZE]) -> Zobrist96 {
        Zobrist96 { inner }
    }

    pub fn as_bytes(&self) -> &[u8; Zobrist96::SIZE] {
        &self.inner
    }

    pub fn prefix(&self) -> u64 {
        u64::from_be_bytes(self.inner[..8].try_into().expect("8 bytes"))
    }

    pub fn pack<B: BufMut>(&self, buf: &mut B) {
        buf.put_slice(&self.inner);
    }

    pub fn unpack<B: Buf>(buf: &mut B) -> Self {
        let mut inner = [0; Zobrist96::SIZE];
        buf.copy_to_slice(&mut inner);
        Zobrist96 { inner }
    }
}
