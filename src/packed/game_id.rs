use std::{fmt, fmt::Write, str::FromStr};

use bytes::{Buf, BufMut};
use thiserror::Error;

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GameId(u64);

impl GameId {
    pub const SIZE: usize = 6;

    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut bytes = [0u8; Self::SIZE];
        self.pack(&mut &mut bytes[..]);
        bytes
    }

    pub fn pack<B: BufMut>(&self, buf: &mut B) {
        buf.put_uint(self.0, Self::SIZE);
    }

    pub fn unpack<B: Buf>(buf: &mut B) -> Self {
        let n = buf.get_uint(Self::SIZE);
        assert!(n < 62_u64.pow(8), "invalid game id");
        GameId(n)
    }

    pub fn from_ascii(bytes: &[u8]) -> Result<Self, InvalidGameId> {
        if bytes.len() != 8 {
            return Err(InvalidGameId);
        }

        let mut base62 = 0;
        for c in bytes.iter().rev() {
            base62 = u64::from(match c {
                b'0'..=b'9' => c - b'0',
                b'A'..=b'Z' => c - b'A' + 10,
                b'a'..=b'z' => c - b'a' + 10 + 26,
                _ => return Err(InvalidGameId),
            }) + base62 * 62;
        }

        Ok(GameId(base62))
    }
}

#[derive(Error, Debug)]
#[error("invalid game id")]
pub struct InvalidGameId;

impl FromStr for GameId {
    type Err = InvalidGameId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_ascii(s.as_bytes())
    }
}

impl fmt::Display for GameId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut n = self.0;
        for _ in 0..8 {
            let rem = n % 62;
            f.write_char(char::from(if rem >= 10 + 26 {
                (rem - (10 + 26)) as u8 + b'a'
            } else if rem >= 10 {
                (rem - 10) as u8 + b'A'
            } else {
                rem as u8 + b'0'
            }))?;
            n /= 62;
        }
        Ok(())
    }
}

impl fmt::Debug for GameId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GameId({self})")
    }
}

#[cfg(test)]
mod tests {
    use quickcheck::{Arbitrary, Gen, quickcheck};

    use super::*;

    impl Arbitrary for GameId {
        fn arbitrary(g: &mut Gen) -> Self {
            GameId(u64::arbitrary(g) % 62u64.pow(8))
        }
    }

    quickcheck! {
        fn str_roundtrip(game_id: GameId) -> bool {
            GameId::from_str(&game_id.to_string()).unwrap() == game_id
        }

        fn pack_roundtrip(game_id: GameId) -> bool {
            let mut buf = Vec::new();
            game_id.pack(&mut buf);
            GameId::unpack(&mut &buf[..]) == game_id
        }

        fn packed_ord(left: GameId, right: GameId) -> bool {
            let mut left_buf = Vec::new();
            left.pack(&mut left_buf);

            let mut right_buf = Vec::new();
            right.pack(&mut right_buf);

            left_buf.cmp(&right_buf) == left.cmp(&right)
        }
    }
}
