use std::{
    fs::File,
    io,
    io::{BufWriter, Write},
    path::Path,
};

use arbitrary::Arbitrary;
use bytes::{Buf, BufMut};
use shakmaty::{KnownOutcome, Outcome, packed::PackedUciMove};

use crate::packed::{
    GameId, Month, Rating, Speed, Zobrist96, outcome_bits, outcome_from_bits, unpack_uci_move,
};

#[derive(Arbitrary, Debug, Clone, Eq, PartialEq)]
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
    pub const KEY_SIZE: usize = 12 + 2;
    pub const SIZE: usize = Self::KEY_SIZE + 2 + 6 + 1 + 2;

    pub fn pack<B: BufMut>(&self, buf: &mut B) {
        self.hash.pack(buf); // 12 bytes
        self.month.pack(buf); // 2 bytes
        buf.put_slice(&self.m.to_bytes()); // 2 bytes
        self.game_id.pack(buf); // 6 bytes
        buf.put_u8(self.speed.bits() | outcome_bits(Outcome::Known(self.outcome)) << 3); // 1 byte
        self.rating.pack_fixed(buf); // 2 bytes
    }

    pub fn unpack<B: Buf>(buf: &mut B) -> Self {
        let hash = Zobrist96::unpack(buf);
        let month = Month::unpack(buf);
        let m = unpack_uci_move(buf);
        let game_id = GameId::unpack(buf);
        let speed_and_outcome = buf.get_u8();
        let speed = Speed::from_bits(speed_and_outcome & 0b111).expect("valid speed");
        let outcome = outcome_from_bits(speed_and_outcome >> 3)
            .expect("valid outcome")
            .known()
            .expect("known outcome");
        let rating = Rating::unpack_fixed(buf);

        IntermediateEntry {
            hash,
            month,
            m,
            game_id,
            speed,
            rating,
            outcome,
        }
    }

    pub fn partition(&self) -> usize {
        (self.hash.prefix() >> (64 - 10)) as usize
    }
}

pub struct IntermediateSink {
    partitions: Vec<BufWriter<File>>,
}

impl IntermediateSink {
    pub fn new(workspace: &Path) -> io::Result<Self> {
        Ok(IntermediateSink {
            partitions: (0..1024)
                .map(|i| {
                    File::create(workspace.join(format!("expanded_{:04}.bin", i)))
                        .map(BufWriter::new)
                })
                .collect::<Result<_, _>>()?,
        })
    }

    pub fn write(&mut self, entry: &IntermediateEntry) -> io::Result<()> {
        let mut bytes = [0; IntermediateEntry::SIZE];
        entry.pack(&mut &mut bytes[..]);
        self.partitions[entry.partition()].write_all(&bytes)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        for partition in &mut self.partitions {
            partition.flush()?;
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

    #[test]
    fn test_pack_roundtrip() {
        arbtest(|u| {
            let entry: IntermediateEntry = u.arbitrary()?;

            let mut buf = [0; IntermediateEntry::SIZE];
            entry.pack(&mut &mut buf[..]);
            let unpacked = IntermediateEntry::unpack(&mut &buf[..]);
            assert_eq!(entry, unpacked);
            Ok(())
        });
    }
}
