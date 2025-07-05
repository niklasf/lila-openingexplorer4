use std::fmt;

use bincode::{Decode, Encode};

#[derive(Clone, Encode, Decode, Eq, PartialEq, Ord, PartialOrd)]
pub struct LaxDate {
    year: u16,
    month: u8,
    day: u8,
}

impl fmt::Display for LaxDate {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.day > 0 && self.month > 0 {
            write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
        } else if self.month > 0 {
            write!(f, "{:04}-{:02}", self.year, self.month)
        } else {
            write!(f, "{:04}", self.year)
        }
    }
}

impl fmt::Debug for LaxDate {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "LaxDate({self})")
    }
}
