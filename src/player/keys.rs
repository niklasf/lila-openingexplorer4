//! Keys of the column families of the personal explorer. Every key of
//! player data starts with the `UserHash`, so that all data of a player is
//! one contiguous key range.
//!
//! ```text
//! status    uh                                  -> PlayerStatus
//! games     uh | seq                            -> PlayerGame
//! game_ids  uh | GameId                         -> seq
//! warm      uh | gen | color | PositionKey      -> PlayerEntry
//! cold      uh | gen | color | bucket           -> PlayerColdGroup
//! meta      name                                -> value (no player)
//! ```
//!
//! Integers are big endian, so that keys sort numerically.

use std::fmt;

use shakmaty::Color;

use crate::{
    packed::{GameId, PositionKey},
    player::UserHash,
};

/// Number of a game of a player, in indexing order.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct GameSeq(pub u32);

impl GameSeq {
    pub const SIZE: usize = 4;

    pub fn next(self) -> GameSeq {
        GameSeq(self.0.checked_add(1).expect("game numbers exhausted"))
    }

    pub fn to_bytes(self) -> [u8; GameSeq::SIZE] {
        self.0.to_be_bytes()
    }

    pub fn from_bytes(bytes: [u8; GameSeq::SIZE]) -> GameSeq {
        GameSeq(u32::from_be_bytes(bytes))
    }
}

impl fmt::Display for GameSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Generation of the warm and cold index of a player. Rebuilds write the
/// next generation and then switch to it.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Generation(pub u32);

impl Generation {
    pub const SIZE: usize = 4;

    pub fn next(self) -> Generation {
        Generation(self.0.checked_add(1).expect("generations exhausted"))
    }

    pub fn to_bytes(self) -> [u8; Generation::SIZE] {
        self.0.to_be_bytes()
    }
}

/// Byte of a color in keys.
pub fn color_byte(color: Color) -> u8 {
    match color {
        Color::White => 0,
        Color::Black => 1,
    }
}

/// Implements `AsRef<[u8]>` for a key newtype over a byte array.
macro_rules! key_bytes {
    ($name:ident) => {
        impl AsRef<[u8]> for $name {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }
    };
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StatusKey([u8; StatusKey::SIZE]);

key_bytes!(StatusKey);

impl StatusKey {
    pub const SIZE: usize = UserHash::SIZE;

    pub fn new(uh: UserHash) -> StatusKey {
        StatusKey(*uh.as_bytes())
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GameKey([u8; GameKey::SIZE]);

key_bytes!(GameKey);

impl GameKey {
    pub const SIZE: usize = UserHash::SIZE + GameSeq::SIZE;

    pub fn new(uh: UserHash, seq: GameSeq) -> GameKey {
        let mut key = [0; GameKey::SIZE];
        key[..UserHash::SIZE].copy_from_slice(uh.as_bytes());
        key[UserHash::SIZE..].copy_from_slice(&seq.to_bytes());
        GameKey(key)
    }

    pub fn seq(key: &[u8]) -> GameSeq {
        GameSeq::from_bytes(key[UserHash::SIZE..GameKey::SIZE].try_into().expect("seq"))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GameIdKey([u8; GameIdKey::SIZE]);

key_bytes!(GameIdKey);

impl GameIdKey {
    pub const SIZE: usize = UserHash::SIZE + GameId::SIZE;

    pub fn new(uh: UserHash, id: GameId) -> GameIdKey {
        let mut key = [0; GameIdKey::SIZE];
        key[..UserHash::SIZE].copy_from_slice(uh.as_bytes());
        key[UserHash::SIZE..].copy_from_slice(&id.to_bytes());
        GameIdKey(key)
    }
}

/// Size of the prefix of the warm and cold keys of a generation and color.
const INDEX_PREFIX_SIZE: usize = UserHash::SIZE + Generation::SIZE + 1;

/// Prefix of the warm and cold keys of a generation and color.
fn index_prefix(uh: UserHash, generation: Generation, color: Color) -> [u8; INDEX_PREFIX_SIZE] {
    let mut key = [0; INDEX_PREFIX_SIZE];
    key[..UserHash::SIZE].copy_from_slice(uh.as_bytes());
    key[UserHash::SIZE..UserHash::SIZE + Generation::SIZE].copy_from_slice(&generation.to_bytes());
    key[INDEX_PREFIX_SIZE - 1] = color_byte(color);
    key
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WarmKey([u8; WarmKey::SIZE]);

key_bytes!(WarmKey);

impl WarmKey {
    pub const SIZE: usize = INDEX_PREFIX_SIZE + PositionKey::SIZE;

    pub fn new(uh: UserHash, generation: Generation, color: Color, pos: &PositionKey) -> WarmKey {
        let mut key = [0; WarmKey::SIZE];
        key[..INDEX_PREFIX_SIZE].copy_from_slice(&index_prefix(uh, generation, color));
        key[INDEX_PREFIX_SIZE..].copy_from_slice(pos.as_bytes());
        WarmKey(key)
    }
}

/// Bucket of a cold group: the first `bits` bits of the position key.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColdBucket(pub u32);

/// Most bucket bits of a color of a player.
pub const MAX_COLD_BITS: u8 = 24;

impl ColdBucket {
    /// The bucket and the fingerprint suffix (the 16 bits after the bucket
    /// bits) of a position.
    pub fn of(pos: &PositionKey, bits: u8) -> (ColdBucket, u16) {
        debug_assert!(bits <= MAX_COLD_BITS);
        let prefix = pos.prefix();
        let bucket = if bits == 0 {
            0
        } else {
            (prefix >> (64 - u32::from(bits))) as u32
        };
        let suffix = ((prefix << bits) >> 48) as u16;
        (ColdBucket(bucket), suffix)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColdGroupKey([u8; ColdGroupKey::SIZE]);

key_bytes!(ColdGroupKey);

impl ColdGroupKey {
    pub const SIZE: usize = INDEX_PREFIX_SIZE + 4;

    pub fn new(
        uh: UserHash,
        generation: Generation,
        color: Color,
        bucket: ColdBucket,
    ) -> ColdGroupKey {
        let mut key = [0; ColdGroupKey::SIZE];
        key[..INDEX_PREFIX_SIZE].copy_from_slice(&index_prefix(uh, generation, color));
        key[INDEX_PREFIX_SIZE..].copy_from_slice(&bucket.0.to_be_bytes());
        ColdGroupKey(key)
    }
}

/// Longest key of any column family.
pub const MAX_KEY_SIZE: usize = WarmKey::SIZE;

const _: () = assert!(
    StatusKey::SIZE <= MAX_KEY_SIZE
        && GameKey::SIZE <= MAX_KEY_SIZE
        && GameIdKey::SIZE <= MAX_KEY_SIZE
        && ColdGroupKey::SIZE <= MAX_KEY_SIZE
);

/// Key ranges of a player, for `DeleteRange`.
impl UserHash {
    /// All keys of the player in any column family: `[uh, uh + 1)`, or for
    /// the largest hash up to `uh` and more `0xff` than any key has after
    /// it.
    pub fn range(self) -> (Vec<u8>, Vec<u8>) {
        let start = self.as_bytes().to_vec();
        let end = match self.to_u64().checked_add(1) {
            Some(next) => next.to_be_bytes().to_vec(),
            None => {
                let mut end = start.clone();
                end.resize(MAX_KEY_SIZE + 1, 0xff);
                end
            }
        };
        (start, end)
    }

    /// The warm or cold keys of generations `from..to` (or all from `from`).
    pub fn generations_range(self, from: Generation, to: Option<Generation>) -> (Vec<u8>, Vec<u8>) {
        let mut start = self.as_bytes().to_vec();
        start.extend_from_slice(&from.to_bytes());
        let end = match to {
            Some(to) => {
                let mut end = self.as_bytes().to_vec();
                end.extend_from_slice(&to.to_bytes());
                end
            }
            None => self.range().1,
        };
        (start, end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cold_bucket() {
        let mut bytes = [0; PositionKey::SIZE];
        bytes[..8].copy_from_slice(&0xabcd_ef01_2345_6789u64.to_be_bytes());
        let pos = PositionKey::from_bytes(bytes);
        assert_eq!(ColdBucket::of(&pos, 0), (ColdBucket(0), 0xabcd));
        assert_eq!(ColdBucket::of(&pos, 4), (ColdBucket(0xa), 0xbcde));
        assert_eq!(ColdBucket::of(&pos, 16), (ColdBucket(0xabcd), 0xef01));
        assert_eq!(ColdBucket::of(&pos, 24), (ColdBucket(0xabcdef), 0x0123));
    }

    #[test]
    fn test_ranges() {
        let uh = UserHash::from_bytes(*b"\x12\x34\x56\x78\x9a\xbc\xde\xf0");
        let (start, end) = uh.range();
        let key = WarmKey::new(
            uh,
            Generation(7),
            Color::Black,
            &PositionKey::from_bytes([0xff; 12]),
        );
        assert!(start.as_slice() <= key.as_ref() && key.as_ref() < end.as_slice());
        let (start, end) = uh.generations_range(Generation(7), Some(Generation(8)));
        assert!(start.as_slice() <= key.as_ref() && key.as_ref() < end.as_slice());
        let (start, _) = uh.generations_range(Generation(8), None);
        assert!(key.as_ref() < start.as_slice());
        let max = UserHash::from_bytes([0xff; 8]);
        let (start, end) = max.range();
        let key = GameKey::new(max, GameSeq(u32::MAX));
        assert!(start.as_slice() <= key.as_ref() && key.as_ref() < end.as_slice());
        let key = WarmKey::new(
            max,
            Generation(u32::MAX),
            Color::Black,
            &PositionKey::from_bytes([0xff; 12]),
        );
        assert!(start.as_slice() <= key.as_ref() && key.as_ref() < end.as_slice());
    }
}
