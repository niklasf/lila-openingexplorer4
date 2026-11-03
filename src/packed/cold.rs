use crate::packed::{GameIndex, PositionKey, pack_uint, unpack_uint};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ColdKey {
    prefix: [u8; ColdKey::PREFIX_SIZE],
    suffix: u16,
}

impl ColdKey {
    /// Hash bytes used as RocksDB key.
    pub const PREFIX_SIZE: usize = 4;
    /// Further hash bytes that identify the position within the group.
    pub const SUFFIX_SIZE: usize = 2;

    pub fn new(hash: &PositionKey) -> ColdKey {
        ColdKey::from_hash_bytes(hash.as_bytes())
    }

    pub fn from_hash_bytes(hash: &[u8]) -> ColdKey {
        ColdKey {
            prefix: hash[..ColdKey::PREFIX_SIZE].try_into().expect("prefix"),
            suffix: u16::from_be_bytes(
                hash[ColdKey::PREFIX_SIZE..ColdKey::PREFIX_SIZE + ColdKey::SUFFIX_SIZE]
                    .try_into()
                    .expect("suffix"),
            ),
        }
    }

    pub fn prefix(&self) -> &[u8; ColdKey::PREFIX_SIZE] {
        &self.prefix
    }

    pub fn suffix(&self) -> u16 {
        self.suffix
    }

    /// The games of a group that may contain this position, from the value
    /// of the group.
    pub fn candidates(&self, value: &[u8]) -> Vec<GameIndex> {
        let group = ColdGroupView::new(value);
        let mut gaps = BitReader::new(group.bits, group.count * u64::from(group.width));
        let mut candidates = Vec::new();
        let mut suffix = 0u64;
        for i in 0..group.count {
            suffix += gaps.read_rice(group.k);
            match suffix.cmp(&u64::from(self.suffix)) {
                std::cmp::Ordering::Less => (),
                std::cmp::Ordering::Equal => candidates.push(group.index(i)),
                std::cmp::Ordering::Greater => break,
            }
        }
        candidates
    }
}

/// The games of all positions whose hashes share a prefix, keyed by the
/// prefix. The items are sorted by suffix.
///
/// ```text
/// value := k (u8) width (u8) uint(count) index* gap*   (bit-packed, MSB first)
/// index := GameIndex, `width` bits
/// gap   := suffix - previous suffix (the first from 0), Rice coded with
///          parameter k: (gap >> k) as unary ones, a zero, the low k bits
/// ```
///
/// Indexes come first, at fixed width, so that a lookup can stop decoding
/// gaps once it is past its suffix, and then read the indexes of the
/// matching items directly.
#[derive(Debug)]
pub struct ColdGroup {
    /// Bits of each game index.
    width: u32,
    prefix: Option<[u8; ColdKey::PREFIX_SIZE]>,
    items: Vec<(u16, GameIndex)>,
    value: Vec<u8>,
}

impl ColdGroup {
    /// A group builder for game indexes below `games`.
    pub fn new(games: u64) -> ColdGroup {
        ColdGroup {
            width: (u64::BITS - games.saturating_sub(1).leading_zeros()).max(1),
            prefix: None,
            items: Vec::new(),
            value: Vec::new(),
        }
    }

    /// Adds an item. Must be called in key order. Returns `Err` with the key
    /// if the item does not belong to the current group; the current group
    /// must then be finished first.
    pub fn push(&mut self, key: ColdKey, game: GameIndex) -> Result<(), ColdKey> {
        debug_assert!(game.get() >> self.width == 0, "game index too large");
        match self.prefix {
            Some(prefix) if prefix != key.prefix => return Err(key),
            Some(_) => {
                debug_assert!(
                    self.items
                        .last()
                        .is_none_or(|&(last, _)| last <= key.suffix),
                    "cold items out of order"
                );
            }
            None => self.prefix = Some(key.prefix),
        }
        self.items.push((key.suffix, game));
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.prefix.is_none()
    }

    pub fn items(&self) -> u64 {
        self.items.len() as u64
    }

    /// The RocksDB key and value of the group, if it has items. The group
    /// is empty afterwards.
    pub fn finish(&mut self) -> Option<([u8; ColdKey::PREFIX_SIZE], &[u8])> {
        let prefix = self.prefix.take()?;
        let count = self.items.len() as u64;
        // Near-optimal Rice parameter for uniformly distributed suffixes:
        // log2 of ln(2) times the mean gap.
        let k = ((1 << 16) as f64 / count as f64 * std::f64::consts::LN_2)
            .log2()
            .floor()
            .clamp(0.0, 16.0) as u32;
        self.value.clear();
        self.value.push(k as u8);
        self.value.push(self.width as u8);
        pack_uint(&mut self.value, count);
        let mut bits = BitWriter::new(&mut self.value);
        for &(_, game) in &self.items {
            bits.write(game.get(), self.width);
        }
        let mut previous = 0;
        for &(suffix, _) in &self.items {
            bits.write_rice(u64::from(suffix - previous), k);
            previous = suffix;
        }
        bits.finish();
        self.items.clear();
        Some((prefix, &self.value))
    }
}

/// The header and bits of a packed group.
struct ColdGroupView<'a> {
    k: u32,
    width: u32,
    count: u64,
    bits: &'a [u8],
}

impl<'a> ColdGroupView<'a> {
    fn new(value: &'a [u8]) -> ColdGroupView<'a> {
        let (&[k, width], mut rest) = value.split_first_chunk::<2>().expect("cold group header");
        let count = unpack_uint(&mut rest);
        ColdGroupView {
            k: u32::from(k),
            width: u32::from(width),
            count,
            bits: rest,
        }
    }

    fn index(&self, i: u64) -> GameIndex {
        GameIndex::new(BitReader::new(self.bits, i * u64::from(self.width)).read(self.width))
            .expect("valid game index")
    }
}

/// Appends bits to a buffer, most significant bit first.
struct BitWriter<'a> {
    buf: &'a mut Vec<u8>,
    acc: u64,
    len: u32,
}

impl<'a> BitWriter<'a> {
    fn new(buf: &'a mut Vec<u8>) -> BitWriter<'a> {
        BitWriter {
            buf,
            acc: 0,
            len: 0,
        }
    }

    /// Writes the low `bits` bits of `value` (at most 56 at a time).
    fn write(&mut self, value: u64, bits: u32) {
        debug_assert!(bits <= 56 && (bits == 64 || value >> bits == 0));
        self.acc = self.acc << bits | value;
        self.len += bits;
        while self.len >= 8 {
            self.len -= 8;
            self.buf.push((self.acc >> self.len) as u8);
        }
        self.acc &= (1 << self.len) - 1;
    }

    fn write_rice(&mut self, value: u64, k: u32) {
        let mut quotient = value >> k;
        while quotient >= 32 {
            self.write((1 << 32) - 1, 32);
            quotient -= 32;
        }
        self.write(((1 << quotient) - 1) << 1, quotient as u32 + 1);
        self.write(value & ((1 << k) - 1), k);
    }

    /// Pads the last byte with zeros.
    fn finish(mut self) {
        if self.len > 0 {
            let pad = 8 - self.len;
            self.write(0, pad);
        }
    }
}

/// Reads bits written by `BitWriter`.
struct BitReader<'a> {
    bits: &'a [u8],
    pos: u64,
}

impl<'a> BitReader<'a> {
    fn new(bits: &'a [u8], pos: u64) -> BitReader<'a> {
        BitReader { bits, pos }
    }

    fn bit(&mut self) -> bool {
        let byte = self.bits[(self.pos / 8) as usize];
        let bit = byte >> (7 - self.pos % 8) & 1 != 0;
        self.pos += 1;
        bit
    }

    fn read(&mut self, bits: u32) -> u64 {
        let mut value = 0;
        for _ in 0..bits {
            value = value << 1 | u64::from(self.bit());
        }
        value
    }

    fn read_rice(&mut self, k: u32) -> u64 {
        let mut quotient = 0;
        while self.bit() {
            quotient += 1;
        }
        quotient << k | self.read(k)
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;

    fn hash(bytes: [u8; 6]) -> PositionKey {
        let mut inner = [0xab; PositionKey::SIZE];
        inner[..6].copy_from_slice(&bytes);
        PositionKey::from_bytes(inner)
    }

    fn game(n: u64) -> GameIndex {
        GameIndex::new(n).unwrap()
    }

    #[test]
    fn test_group_lookup() {
        let mut group = ColdGroup::new(100);
        let keys = [
            ColdKey::new(&hash([1, 2, 3, 4, 0, 1])),
            ColdKey::new(&hash([1, 2, 3, 4, 0, 1])), // same truncated hash
            ColdKey::new(&hash([1, 2, 3, 4, 0, 7])),
            ColdKey::new(&hash([1, 2, 3, 4, 9, 9])),
        ];
        for (i, key) in keys.iter().enumerate() {
            group.push(*key, game(i as u64 + 1)).unwrap();
        }
        assert_eq!(group.items(), 4);
        assert!(
            group
                .push(ColdKey::new(&hash([1, 2, 3, 5, 0, 0])), game(5))
                .is_err()
        );
        let (prefix, value) = group.finish().unwrap();
        assert_eq!(prefix, [1, 2, 3, 4]);
        let value = value.to_vec();
        let lookup = |key: ColdKey| key.candidates(&value);
        assert_eq!(lookup(keys[0]), vec![game(1), game(2)]);
        assert_eq!(lookup(keys[2]), vec![game(3)]);
        assert_eq!(lookup(keys[3]), vec![game(4)]);
        assert_eq!(lookup(ColdKey::new(&hash([1, 2, 3, 4, 0, 0]))), vec![]);
        assert_eq!(lookup(ColdKey::new(&hash([1, 2, 3, 4, 0, 8]))), vec![]);
        assert_eq!(lookup(ColdKey::new(&hash([1, 2, 3, 4, 255, 255]))), vec![]);
        assert!(group.is_empty());
        group.push(keys[0], game(9)).unwrap();
        assert!(group.finish().is_some());
    }

    /// Lookups find exactly the items with the suffix, for any group.
    #[test]
    fn test_group_roundtrip() {
        arbtest(|u| {
            let games = u.int_in_range(1..=1u64 << 40)?;
            let mut items: Vec<(u16, u64)> = (0..u.int_in_range(1..=300)?)
                .map(|_| {
                    // Few distinct suffixes sometimes, to have duplicates.
                    let suffix = if u.ratio(1, 4)? {
                        u.int_in_range(0..=3)?
                    } else {
                        u.arbitrary()?
                    };
                    Ok((suffix, u.int_in_range(0..=games - 1)?))
                })
                .collect::<arbitrary::Result<_>>()?;
            items.sort_by_key(|&(suffix, _)| suffix);
            let mut group = ColdGroup::new(games);
            for &(suffix, index) in &items {
                let mut bytes = [7, 7, 7, 7, 0, 0];
                bytes[4..].copy_from_slice(&suffix.to_be_bytes());
                group.push(ColdKey::new(&hash(bytes)), game(index)).unwrap();
            }
            let value = group.finish().unwrap().1.to_vec();
            for probe in
                items
                    .iter()
                    .map(|&(suffix, _)| suffix)
                    .chain([u.arbitrary()?, 0, u16::MAX])
            {
                let mut bytes = [7, 7, 7, 7, 0, 0];
                bytes[4..].copy_from_slice(&probe.to_be_bytes());
                let expected: Vec<GameIndex> = items
                    .iter()
                    .filter(|&&(suffix, _)| suffix == probe)
                    .map(|&(_, index)| game(index))
                    .collect();
                assert_eq!(ColdKey::new(&hash(bytes)).candidates(&value), expected);
            }
            Ok(())
        })
        .budget_ms(2000);
    }

    /// About 12.4 bits per suffix for groups like those of the full
    /// database (about 45 items).
    #[test]
    fn test_group_size() {
        let games = 8_337_694_503;
        let mut group = ColdGroup::new(games);
        let mut x = 1u64;
        let mut suffixes: Vec<u16> = (0..45)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (x >> 48) as u16
            })
            .collect();
        suffixes.sort_unstable();
        for (i, &suffix) in suffixes.iter().enumerate() {
            let mut bytes = [1, 1, 1, 1, 0, 0];
            bytes[4..].copy_from_slice(&suffix.to_be_bytes());
            group
                .push(ColdKey::new(&hash(bytes)), game(i as u64 * 1000))
                .unwrap();
        }
        let len = group.finish().unwrap().1.len();
        // 3 header bytes, 45 * 34 index bits, about 45 * 12.4 gap bits.
        assert!(len <= 3 + (45usize * (34 + 13)).div_ceil(8), "{len} bytes");
    }
}
