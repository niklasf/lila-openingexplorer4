use crate::packed::{pack_int, unpack_int};
use std::{fmt, num::TryFromIntError};

use bytes::{Buf, BufMut};

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
}

impl From<Rating> for i32 {
    fn from(rating: Rating) -> Self {
        i32::from(rating.offset) + Rating::BASE
    }
}

impl TryFrom<i32> for Rating {
    type Error = TryFromIntError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        Ok(Rating {
            offset: value.saturating_sub(Rating::BASE).try_into()?,
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
    use quickcheck::{Arbitrary, Gen, quickcheck};

    use super::*;

    impl Arbitrary for Rating {
        fn arbitrary(g: &mut Gen) -> Self {
            Rating {
                offset: i16::arbitrary(g),
            }
        }
    }

    quickcheck! {
        fn i32_roundtrip(rating: Rating) -> bool {
            Rating::try_from(i32::from(rating)).unwrap() == rating
        }

        fn pack_roundtrip(rating: Rating) -> bool {
            let mut buf = Vec::new();
            rating.pack(&mut buf);
            Rating::unpack(&mut &buf[..]) == rating
        }
    }
}
