use bytes::Buf;
use bytes::BufMut;
use std::{fmt, num::NonZeroU8};

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
        //fn str_roundtrip(game_id: GameId) -> bool {
        //    GameId::from_str(&game_id.to_string()).unwrap() == game_id
        //}

        fn pack_roundtrip(lax_date: LaxDate) -> bool {
            let mut buf = Vec::new();
            lax_date.pack(&mut buf);
            LaxDate::unpack(&mut &buf[..]) == lax_date
        }
    }
}
