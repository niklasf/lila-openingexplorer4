use std::sync::Mutex;
use std::{
    fs::File,
    io,
    io::{BufWriter, Write},
    path::Path,
};

use arbitrary::Arbitrary;
use bytes::BufMut;
use shakmaty::{KnownOutcome, Outcome, packed::PackedUciMove, zobrist::Zobrist128};

use crate::packed::{GameId, Month, Rating, Speed, outcome_bits};

#[derive(Arbitrary, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Zobrist96 {
    inner: [u8; 12],
}

impl Zobrist96 {
    pub fn partition(&self) -> usize {
        usize::from(u16::from_be_bytes([self.inner[0], self.inner[1]]) >> (16 - 10))
    }
}

impl From<Zobrist128> for Zobrist96 {
    fn from(zobrist: Zobrist128) -> Self {
        Zobrist96 {
            inner: u128::from(zobrist).to_be_bytes()[..12]
                .try_into()
                .expect("96 bits"),
        }
    }
}

#[derive(Arbitrary)]
pub struct IntermediateEntry {
    pub hash: Zobrist96,
    pub month: Month,
    pub m: PackedUciMove,
    pub game_id: GameId,
    pub speed: Speed,
    pub rating: Rating,
    pub outcome: KnownOutcome,
}

impl IntermediateEntry {
    const SIZE: usize = 25;

    pub fn pack<B: BufMut>(&self, buf: &mut B) {
        buf.put_slice(&self.hash.inner); // 12 bytes
        self.month.pack(buf); // 2 bytes
        buf.put_slice(&self.m.to_bytes()); // 2 bytes
        self.game_id.pack(buf); // 6 bytes
        buf.put_u8(self.speed.bits() | outcome_bits(Outcome::Known(self.outcome)) << 3); // 1 byte
        self.rating.pack_fixed(buf); // 2 bytes
    }

    pub fn partition(&self) -> usize {
        self.hash.partition()
    }
}

pub struct IntermediateSink {
    partitions: Vec<Mutex<BufWriter<File>>>,
}

impl IntermediateSink {
    pub fn new(workspace: &Path) -> io::Result<Self> {
        Ok(IntermediateSink {
            partitions: (0..1024)
                .map(|i| {
                    File::create(workspace.join(format!("partition_{}", i)))
                        .map(BufWriter::new)
                        .map(Mutex::new)
                })
                .collect::<Result<_, _>>()?,
        })
    }

    pub fn write(&self, entry: &IntermediateEntry) -> io::Result<()> {
        let mut bytes = [0; IntermediateEntry::SIZE];
        entry.pack(&mut &mut bytes[..]);
        self.partitions[entry.partition()]
            .lock()
            .expect("poison")
            .write_all(&bytes)
    }

    pub fn flush(&self) -> io::Result<()> {
        for partition in &self.partitions {
            partition.lock().expect("poison").flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;

    #[test]
    fn test_packed_ord() {
        arbtest(|u| {
            let left: IntermediateEntry = u.arbitrary()?;
            let mut left_buf = [0; IntermediateEntry::SIZE];
            left.pack(&mut &mut left_buf[..]);

            let right: IntermediateEntry = u.arbitrary()?;
            let mut right_buf = [0; IntermediateEntry::SIZE];
            right.pack(&mut &mut right_buf[..]);

            if left.partition() < right.partition() {
                assert!(left_buf < right_buf);
            }

            if (left.hash, left.month) < (right.hash, right.month) {
                assert!(left_buf < right_buf);
            }

            Ok(())
        });
    }
}
