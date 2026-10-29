use std::{fmt, str::FromStr};

use arbitrary::Arbitrary;
use btoi::btoi;
use bytes::{Buf, BufMut};
use thiserror::Error;

use crate::packed::{pack_int, unpack_int};

#[derive(Arbitrary, Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rating {
    offset: i16,
}

impl Rating {
    const BASE: i32 = 1500;

    /// Packed in place of the offset of a missing rating. Outside the range
    /// of offsets.
    const MISSING: i64 = i16::MAX as i64 + 1;

    pub fn offset(self) -> i16 {
        self.offset
    }

    pub fn from_offset(offset: i16) -> Self {
        Rating { offset }
    }

    pub fn pack_fixed<B: BufMut>(&self, buf: &mut B) {
        buf.put_i16(self.offset);
    }

    pub fn unpack_fixed<B: Buf>(buf: &mut B) -> Self {
        Rating::from_offset(buf.get_i16())
    }

    pub fn pack_optional<B: BufMut>(rating: Option<Rating>, buf: &mut B) {
        pack_int(
            buf,
            rating.map_or(Rating::MISSING, |rating| i64::from(rating.offset)),
        );
    }

    pub fn unpack_optional<B: Buf>(buf: &mut B) -> Option<Rating> {
        match unpack_int(buf) {
            Rating::MISSING => None,
            offset => Some(Rating::from_offset(
                i16::try_from(offset).expect("valid rating"),
            )),
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
    use arbtest::arbtest;

    use super::*;

    #[test]
    fn test_i32_roundtrip() {
        arbtest(|u| {
            let rating: Rating = u.arbitrary()?;

            assert_eq!(Rating::try_from(i32::from(rating)).unwrap(), rating);
            Ok(())
        });
    }

    #[test]
    fn test_pack_optional_roundtrip() {
        arbtest(|u| {
            let rating: Option<Rating> = u.arbitrary()?;

            let mut buf = Vec::new();
            Rating::pack_optional(rating, &mut buf);
            assert_eq!(Rating::unpack_optional(&mut &buf[..]), rating);
            Ok(())
        });
    }

    #[test]
    fn test_pack_fixed_roundtrip() {
        arbtest(|u| {
            let rating: Rating = u.arbitrary()?;

            let mut buf = [0; 2];
            rating.pack_fixed(&mut &mut buf[..]);
            assert_eq!(Rating::unpack_fixed(&mut &buf[..]), rating);
            Ok(())
        });
    }
}
