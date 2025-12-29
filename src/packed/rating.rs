use std::{fmt, str::FromStr};

use btoi::btoi;
use bytes::{Buf, BufMut};
use thiserror::Error;

use crate::packed::{pack_int, unpack_int};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rating {
    offset: i16,
}

impl Rating {
    const BASE: i32 = 1500;

    pub fn pack<B: BufMut>(&self, buf: &mut B) {
        pack_int(buf, self.offset as i64);
    }

    pub fn unpack<B: Buf>(buf: &mut B) -> Self {
        Rating {
            offset: unpack_int(buf) as i16,
        }
    }

    pub fn from_ascii(bytes: &[u8]) -> Result<Self, InvalidRating> {
        btoi::<i32>(bytes)
            .map_err(|_| InvalidRating)
            .and_then(Rating::try_from)
    }
}

#[derive(Debug, Error)]
#[error("invalid rating")]
pub struct InvalidRating;

impl FromStr for Rating {
    type Err = InvalidRating;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_ascii(s.as_bytes())
    }
}

impl From<Rating> for i32 {
    fn from(rating: Rating) -> Self {
        i32::from(rating.offset) + Rating::BASE
    }
}

impl TryFrom<i32> for Rating {
    type Error = InvalidRating;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        Ok(Rating {
            offset: value
                .saturating_sub(Rating::BASE)
                .try_into()
                .map_err(|_| InvalidRating)?,
        })
    }
}

impl fmt::Debug for Rating {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Rating({})", i32::from(*self))
    }
}

#[cfg(test)]
mod tests {
    use arbitrary::Arbitrary;
    use arbitrary::Unstructured;
    use arbtest::arbtest;

    use super::*;

    impl Arbitrary<'_> for Rating {
        fn arbitrary(u: &mut Unstructured) -> arbitrary::Result<Self> {
            Ok(Rating {
                offset: u.arbitrary()?,
            })
        }
    }

    #[test]
    fn test_i32_roundtrip() {
        arbtest(|u| {
            let rating: Rating = u.arbitrary()?;

            assert_eq!(Rating::try_from(i32::from(rating)).unwrap(), rating);
            Ok(())
        });
    }

    #[test]
    fn test_pack_roundtrip() {
        arbtest(|u| {
            let rating: Rating = u.arbitrary()?;

            let mut buf = Vec::new();
            rating.pack(&mut buf);
            assert_eq!(Rating::unpack(&mut &buf[..]), rating);
            Ok(())
        });
    }
}
