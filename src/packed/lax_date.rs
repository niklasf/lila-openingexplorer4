use bytes::Buf;
use bytes::BufMut;
use std::str::FromStr;
use std::{fmt, num::NonZeroU8};
use thiserror::Error;

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct LaxDate {
    year_since_1900: u8,
    month: Option<NonZeroU8>,
    day: Option<NonZeroU8>,
}

impl LaxDate {
    fn year(&self) -> u32 {
        u32::from(self.year_since_1900) + 1900
    }

    pub fn pack<B: BufMut>(&self, buf: &mut B) {
        buf.put_u8(self.year_since_1900);
        buf.put_u8(self.month.map_or(0, |m| m.get()));
        buf.put_u8(self.day.map_or(0, |d| d.get()));
    }

    pub fn unpack<B: Buf>(buf: &mut B) -> Self {
        let year_since_1900 = buf.get_u8();
        let month = NonZeroU8::new(buf.get_u8());
        assert!(month.is_none_or(|m| m.get() <= 12));
        let day = NonZeroU8::new(buf.get_u8());
        assert!(day.is_none_or(|d| d.get() <= 31));

        LaxDate {
            year_since_1900,
            month,
            day,
        }
    }
}

#[derive(Error, Debug)]
#[error("invalid lax date")]
pub struct InvalidLaxDateError;

impl FromStr for LaxDate {
    type Err = InvalidLaxDateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.splitn(3, '-');
        Ok(LaxDate {
            year_since_1900: parts
                .next()
                .and_then(|part| u32::from_str(part).ok())
                .and_then(|year| year.checked_sub(1900))
                .and_then(|year_since_1900| u8::try_from(year_since_1900).ok())
                .ok_or(InvalidLaxDateError)?,
            month: parts
                .next()
                .map(|part| {
                    NonZeroU8::from_str(part)
                        .ok()
                        .filter(|&m| m.get() <= 12)
                        .ok_or(InvalidLaxDateError)
                })
                .transpose()?,
            day: parts
                .next()
                .map(|part| {
                    NonZeroU8::from_str(part)
                        .ok()
                        .filter(|&d| d.get() <= 31)
                        .ok_or(InvalidLaxDateError)
                })
                .transpose()?,
        })
    }
}

impl fmt::Display for LaxDate {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match (self.day, self.month) {
            (Some(day), Some(month)) => {
                write!(f, "{:04}-{:02}-{:02}", self.year(), month.get(), day.get())
            }
            (None, Some(month)) => {
                write!(f, "{:04}-{:02}", self.year(), month.get())
            }
            _ => write!(f, "{:04}", self.year()),
        }
    }
}

impl fmt::Debug for LaxDate {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "LaxDate({self})")
    }
}

#[cfg(test)]
mod tests {
    use quickcheck::{Arbitrary, Gen, quickcheck};

    use super::*;

    impl Arbitrary for LaxDate {
        fn arbitrary(g: &mut Gen) -> Self {
            let year_since_1900 = u8::arbitrary(g);
            let month = NonZeroU8::new(u8::arbitrary(g) % (12 + 1));
            let day = month.and_then(|_| NonZeroU8::new(u8::arbitrary(g) % (31 + 1)));
            LaxDate {
                year_since_1900,
                month,
                day,
            }
        }
    }

    quickcheck! {
        fn str_roundtrip(lax_date: LaxDate) -> bool {
            LaxDate::from_str(&lax_date.to_string()).unwrap() == lax_date
        }

        fn pack_roundtrip(lax_date: LaxDate) -> bool {
            let mut buf = Vec::new();
            lax_date.pack(&mut buf);
            LaxDate::unpack(&mut &buf[..]) == lax_date
        }

        fn packed_partial_ord(left: LaxDate, right: LaxDate) -> bool {
            let mut left_buf = Vec::new();
            left.pack(&mut left_buf);

            let mut right_buf = Vec::new();
            right.pack(&mut right_buf);

            (left < right) == (left_buf < right_buf)
        }
    }
}
