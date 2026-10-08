use std::num::NonZero;

use thiserror::Error;

#[derive(Debug, Error)]
#[error("invalid packed moves")]
struct InvalidPackedMoves;

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    pending: u64,
    pending_len: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter::default()
    }

    fn write_bits(&mut self, value: u32, len: u32) {
        debug_assert!(len <= 32 && u64::from(value) < (1u64 << len));

        self.pending |= u64::from(value) << self.pending_len;
        self.pending_len += len;

        while self.pending_len >= 8 {
            self.bytes.push(self.pending as u8);
            self.pending >>= 8;
            self.pending_len -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.write_bits(1, 1);
        if self.pending_len > 0 {
            self.bytes.push(self.pending as u8);
        }
        self.bytes
    }
}

struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
    len: usize,
}

impl BitReader<'_> {
    fn new(bytes: &[u8]) -> Result<BitReader<'_>, InvalidPackedMoves> {
        Ok(BitReader {
            bytes,
            pos: 0,
            len: bytes.len() * 8
                - bytes
                    .last()
                    .copied()
                    .and_then(NonZero::new)
                    .ok_or(InvalidPackedMoves)?
                    .leading_zeros() as usize
                - 1,
        })
    }

    fn has_remaining(&self) -> bool {
        self.pos < self.len
    }

    fn read_bits(&mut self, len: u32) -> Result<u32, InvalidPackedMoves> {
        debug_assert!(len <= 32);

        if self.pos + len as usize > self.len {
            return Err(InvalidPackedMoves);
        }

        let tail = &self.bytes[self.pos / 8..];
        let n = tail.len().min(8);
        let mut buf = [0; 8];
        buf[..n].copy_from_slice(&tail[..n]);

        let word = u64::from_le_bytes(buf) >> (self.pos % 8);

        self.pos += len as usize;

        Ok((word & ((1u64 << len) - 1)) as u32)
    }
}

#[cfg(test)]
mod tests {
    use arbitrary::Arbitrary;
    use arbtest::arbtest;

    use super::*;

    #[test]
    fn test_bits_roundtrip() {
        struct Group {
            value: u32,
            len: u32,
        }

        impl Arbitrary<'_> for Group {
            fn arbitrary(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
                let len = u.int_in_range(0..=32)?;
                let value = u.int_in_range(0..=(1u64 << len) - 1)? as u32;
                Ok(Group { value, len })
            }
        }

        arbtest(|u| {
            let groups: Vec<Group> = u.arbitrary()?;

            let mut writer = BitWriter::new();
            for group in &groups {
                writer.write_bits(group.value, group.len);
            }
            let bytes = writer.finish();

            let mut reader = BitReader::new(&bytes).unwrap();
            for group in &groups {
                assert!(reader.has_remaining() || group.len == 0);
                assert_eq!(reader.read_bits(group.len).unwrap(), group.value);
            }
            assert!(!reader.has_remaining());

            Ok(())
        });
    }
}
