use std::{fmt, fmt::Write, str::FromStr};

use bincode::{
    Decode, Encode,
    de::Decoder,
    enc::Encoder,
    error::{DecodeError, EncodeError},
};
use thiserror::Error;

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GameId(u64);

impl Encode for GameId {
    fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), EncodeError> {
        let bytes: [u8; 6] = self.0.to_le_bytes()[..6].try_into().expect("6 bytes");
        bytes.encode(encoder)
    }
}

impl<Context> Decode<Context> for GameId {
    fn decode<D: Decoder<Context = Context>>(decoder: &mut D) -> Result<Self, DecodeError> {
        let d: [u8; 6] = Decode::decode(decoder)?;
        let bytes = [d[0], d[1], d[2], d[3], d[4], d[5], 0, 0];
        let base62 = u64::from_le_bytes(bytes);
        if base62 >= 62_u64.pow(8) {
            return Err(DecodeError::Other("invalid game id"));
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
        if s.len() != 8 {
            return Err(InvalidGameId);
        }

        let mut base62 = 0;
        for c in s.bytes().rev() {
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
        fn arbitrary(g: &mut Gen) -> GameId {
            GameId(u64::arbitrary(g) % 62u64.pow(8))
        }
    }

    quickcheck! {
        fn str_roundtrip(game_id: GameId) -> bool {
            GameId::from_str(&game_id.to_string()).unwrap() == game_id
        }

        fn bincode_roundtrip(game_id: GameId) -> bool {
            let config = bincode::config::standard();
            let encoded = bincode::encode_to_vec(&game_id, config).unwrap();
            let (decoded, n): (GameId, _) = bincode::decode_from_slice(&encoded, config).unwrap();
            (decoded, n) == (game_id, 6)
        }
    }
}
