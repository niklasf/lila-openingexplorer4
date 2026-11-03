use crate::packed::{
    GameIndex, PositionKey,
    bits::{BitReader, BitWriter},
    pack_uint, unpack_uint,
};

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
        let mut gaps = BitReader::at(
            group.bits,
            (group.count * u64::from(ColdGroup::INDEX_BITS)) as usize,
        );
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
/// value := uint(count) index* gap*   (bit-packed, least significant bit first)
/// index := GameIndex, `ColdGroup::INDEX_BITS` bits
/// gap   := suffix - previous suffix (the first from 0), Rice coded with
///          parameter `rice_parameter(count)`: (gap >> k) as unary ones, a
///          zero, the low k bits
/// ```
///
/// Indexes come first, at fixed width, so that a lookup can stop decoding
/// gaps once it is past its suffix, and then read the indexes of the
/// matching items directly.
#[derive(Debug)]
pub struct ColdGroup {
    prefix: Option<[u8; ColdKey::PREFIX_SIZE]>,
    items: Vec<(u16, GameIndex)>,
    value: Vec<u8>,
}

impl ColdGroup {
    /// Bits of each game index: enough for 17.2 B games. Lichess had 8.34 B
    /// games in 2026-11, growing by about 1.2 B a year, so this lasts until
    /// about 2033.
    pub const INDEX_BITS: u32 = 34;

    /// A group builder for game indexes below `games`.
    ///
    /// # Panics
    ///
    /// Panics if there are too many games for `INDEX_BITS`.
    pub fn new(games: u64) -> ColdGroup {
        assert!(
            games <= 1 << ColdGroup::INDEX_BITS,
            "{games} games do not fit into the {}-bit game indexes of cold positions",
            ColdGroup::INDEX_BITS
        );
        ColdGroup {
            prefix: None,
            items: Vec::new(),
            value: Vec::new(),
        }
    }

    /// Adds an item. Must be called in key order. Returns `Err` with the key
    /// if the item does not belong to the current group; the current group
    /// must then be finished first.
    pub fn push(&mut self, key: ColdKey, game: GameIndex) -> Result<(), ColdKey> {
        debug_assert!(
            game.get() >> ColdGroup::INDEX_BITS == 0,
            "game index too large"
        );
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
        let k = rice_parameter(count);
        self.value.clear();
        pack_uint(&mut self.value, count);
        let mut bits = BitWriter::new(&mut self.value);
        for &(_, game) in &self.items {
            bits.write(game.get(), ColdGroup::INDEX_BITS);
        }
        let mut previous = 0;
        for &(suffix, _) in &self.items {
            bits.write_rice(u64::from(suffix - previous), k);
            previous = suffix;
        }
        bits.finish_padded();
        self.items.clear();
        Some((prefix, &self.value))
    }
}

/// Rice parameter for `count` uniformly distributed suffixes. The gaps are
/// about exponentially distributed with mean `m = 65536 / (count + 1)`, for
/// which `k + 1` beats `k` while `2^k < asinh(1/2) * m`. So this is the
/// smallest `k` with `2^k >= asinh(1/2) * m`, in integer arithmetic. It
/// matches the empirically best parameter for all realistic counts.
fn rice_parameter(count: u64) -> u32 {
    // asinh(1/2) * 65536 = 31536.7...
    (0..16).find(|k| (count + 1) << k >= 31537).unwrap_or(16)
}

/// The header and bits of a packed group.
struct ColdGroupView<'a> {
    k: u32,
    count: u64,
    bits: &'a [u8],
}

impl<'a> ColdGroupView<'a> {
    fn new(value: &'a [u8]) -> ColdGroupView<'a> {
        let mut rest = value;
        let count = unpack_uint(&mut rest);
        ColdGroupView {
            k: rice_parameter(count),
            count,
            bits: rest,
        }
    }

    fn index(&self, i: u64) -> GameIndex {
        GameIndex::new(
            BitReader::at(self.bits, (i * u64::from(ColdGroup::INDEX_BITS)) as usize)
                .read(ColdGroup::INDEX_BITS),
        )
        .expect("valid game index")
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
            let games = u.int_in_range(1..=1u64 << ColdGroup::INDEX_BITS)?;
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

    /// The integer Rice parameter matches its definition.
    #[test]
    fn test_rice_parameter() {
        for count in 1..=100_000u64 {
            let mean_gap = (1 << 16) as f64 / (count + 1) as f64;
            let expected = (0.5f64.asinh() * mean_gap).log2().ceil().max(0.0) as u32;
            assert_eq!(rice_parameter(count), expected, "count {count}");
        }
        // Near the typical group size.
        assert_eq!(rice_parameter(45), 10);
        assert_eq!(rice_parameter(100), 9);
    }

    /// About 12 bits per suffix for groups like those of the full
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
        // 1 header byte, 45 * 34 index bits, about 45 * 12 gap bits.
        assert!(
            len <= 1 + (45usize * 34 + 45 * 25 / 2).div_ceil(8),
            "{len} bytes"
        );
    }

    #[test]
    #[should_panic(expected = "do not fit")]
    fn test_too_many_games() {
        ColdGroup::new((1 << ColdGroup::INDEX_BITS) + 1);
    }
}
