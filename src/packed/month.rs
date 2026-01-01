use std::{fmt, str::FromStr};

use arbitrary::Arbitrary;
use btoi::btou;
use bytes::{Buf, BufMut};
use thiserror::Error;

#[derive(Arbitrary, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Month {
    since_jan_1900: u16,
}

impl Month {
    pub fn from_months_since_jan_1900(since_jan_1900: u16) -> Self {
        Self { since_jan_1900 }
    }

    pub fn pack<B: BufMut>(&self, buf: &mut B) {
        buf.put_u16(self.since_jan_1900);
    }

    pub fn unpack<B: Buf>(buf: &mut B) -> Self {
        Self {
            since_jan_1900: buf.get_u16(),
        }
    }

    pub fn from_ascii(bytes: &[u8]) -> Result<Self, InvalidMonth> {
        let mut parts = bytes.splitn(2, |&b| b == b'-');
        let year = parts
            .next()
            .and_then(|part| btou::<u16>(part).ok())
            .ok_or(InvalidMonth)?;
        let month = parts
            .next()
            .and_then(|part| btou::<u16>(part).ok())
            .filter(|&month| 1 <= month && month <= 12)
            .ok_or(InvalidMonth)?;
        Ok(Month {
            since_jan_1900: year
                .checked_sub(1900)
                .and_then(|year_since_1900| year_since_1900.checked_mul(12))
                .and_then(|months_since_1900| months_since_1900.checked_add(month - 1))
                .ok_or(InvalidMonth)?,
        })
    }
}

impl fmt::Display for Month {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let year = self.since_jan_1900 / 12 + 1900;
        let month = self.since_jan_1900 % 12 + 1;
        write!(f, "{}-{:02}", year, month)
    }
}

impl fmt::Debug for Month {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Month({})", self)
    }
}

impl FromStr for Month {
    type Err = InvalidMonth;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_ascii(s.as_bytes())
    }
}

#[derive(Debug, Error)]
#[error("invalid month")]
pub struct InvalidMonth;

#[cfg(test)]
mod test {
    use arbtest::arbtest;

    use super::*;

    #[test]
    fn test_str() {
        assert_eq!(Month::from_months_since_jan_1900(0).to_string(), "1900-01");
    }

    #[test]
    fn test_str_roundtrip() {
        arbtest(|u| {
            let month: Month = u.arbitrary()?;

            assert_eq!(Month::from_str(&month.to_string()).unwrap(), month);
            Ok(())
        });
    }

    #[test]
    fn test_pack_roundtrip() {
        arbtest(|u| {
            let month: Month = u.arbitrary()?;

            let mut buf = Vec::new();
            month.pack(&mut buf);
            let unpacked = Month::unpack(&mut &buf[..]);
            assert_eq!(month, unpacked);
            Ok(())
        });
    }

    #[test]
    fn test_packed_ord() {
        arbtest(|u| {
            let left: Month = u.arbitrary()?;
            let right: Month = u.arbitrary()?;

            let mut left_buf = Vec::new();
            left.pack(&mut left_buf);

            let mut right_buf = Vec::new();
            right.pack(&mut right_buf);

            assert_eq!(left_buf.cmp(&right_buf), left.cmp(&right));
            Ok(())
        });
    }
}
