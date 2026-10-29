use crate::packed::{GameId, Zobrist96};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ColdKey {
    prefix: [u8; ColdKey::PREFIX_SIZE],
    suffix: [u8; ColdKey::SUFFIX_SIZE],
}

impl ColdKey {
    /// Hash bytes used as RocksDB key.
    pub const PREFIX_SIZE: usize = 4;
    /// Further hash bytes stored in each item of the group.
    pub const SUFFIX_SIZE: usize = 2;

    pub fn new(hash: &Zobrist96) -> ColdKey {
        ColdKey::from_hash_bytes(hash.as_bytes())
    }

    pub fn from_hash_bytes(hash: &[u8]) -> ColdKey {
        ColdKey {
            prefix: hash[..ColdKey::PREFIX_SIZE].try_into().expect("prefix"),
            suffix: hash[ColdKey::PREFIX_SIZE..ColdKey::PREFIX_SIZE + ColdKey::SUFFIX_SIZE]
                .try_into()
                .expect("suffix"),
        }
    }

    pub fn prefix(&self) -> &[u8; ColdKey::PREFIX_SIZE] {
        &self.prefix
    }

    pub fn suffix(&self) -> &[u8; ColdKey::SUFFIX_SIZE] {
        &self.suffix
    }

    /// The games of a group that may contain this position, from the value
    /// of the group.
    pub fn candidates<'a>(&self, value: &'a [u8]) -> impl Iterator<Item = GameId> + 'a {
        let (items, rest) = value.as_chunks::<{ ColdGroup::ITEM_SIZE }>();
        assert!(rest.is_empty(), "cold value truncated");
        let suffix = self.suffix;
        let start = items.partition_point(|item| item[..ColdKey::SUFFIX_SIZE] < suffix[..]);
        items[start..]
            .iter()
            .take_while(move |item| item[..ColdKey::SUFFIX_SIZE] == suffix)
            .map(|item| GameId::unpack(&mut &item[ColdKey::SUFFIX_SIZE..]))
    }
}

/// The games of all positions whose hashes share a prefix, keyed by the
/// prefix. The value is a sequence of items, sorted by suffix.
#[derive(Debug, Default)]
pub struct ColdGroup {
    prefix: Option<[u8; ColdKey::PREFIX_SIZE]>,
    value: Vec<u8>,
    items: u64,
}

impl ColdGroup {
    /// Size of an item of the value: the hash suffix and the game.
    pub const ITEM_SIZE: usize = ColdKey::SUFFIX_SIZE + GameId::SIZE;

    /// Adds an item. Must be called in key order. Returns `Err` with the key
    /// if the item does not belong to the current group; the current group
    /// must then be finished first.
    pub fn push(&mut self, key: ColdKey, game_id: GameId) -> Result<(), ColdKey> {
        match self.prefix {
            Some(prefix) if prefix != key.prefix => return Err(key),
            Some(_) => {
                debug_assert!(
                    self.value[self.value.len() - ColdGroup::ITEM_SIZE..][..ColdKey::SUFFIX_SIZE]
                        <= key.suffix[..],
                    "cold items out of order"
                );
            }
            None => {
                self.value.clear();
                self.prefix = Some(key.prefix);
            }
        }
        self.value.extend_from_slice(&key.suffix);
        self.value.extend_from_slice(&game_id.to_bytes());
        self.items += 1;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.prefix.is_none()
    }

    pub fn items(&self) -> u64 {
        self.items
    }

    /// The RocksDB key and value of the group, if it has items. The group
    /// is empty afterwards.
    pub fn finish(&mut self) -> Option<([u8; ColdKey::PREFIX_SIZE], &[u8])> {
        let prefix = self.prefix.take()?;
        self.items = 0;
        Some((prefix, &self.value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(bytes: [u8; 6]) -> Zobrist96 {
        let mut inner = [0xab; Zobrist96::SIZE];
        inner[..6].copy_from_slice(&bytes);
        Zobrist96::from_bytes(inner)
    }

    fn game(n: u64) -> GameId {
        GameId::unpack(&mut &n.to_be_bytes()[2..])
    }

    #[test]
    fn test_group_lookup() {
        let mut group = ColdGroup::default();
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
        assert_eq!(value.len(), 4 * ColdGroup::ITEM_SIZE);
        let lookup = |key: ColdKey| key.candidates(value).collect::<Vec<_>>();
        assert_eq!(lookup(keys[0]), vec![game(1), game(2)]);
        assert_eq!(lookup(keys[2]), vec![game(3)]);
        assert_eq!(lookup(keys[3]), vec![game(4)]);
        assert_eq!(lookup(ColdKey::new(&hash([1, 2, 3, 4, 0, 0]))), vec![]);
        assert_eq!(lookup(ColdKey::new(&hash([1, 2, 3, 4, 0, 8]))), vec![]);
        assert_eq!(lookup(ColdKey::new(&hash([1, 2, 3, 4, 255, 255]))), vec![]);
        assert!(group.is_empty());
        group.push(keys[0], game(9)).unwrap();
        assert_eq!(group.finish().unwrap().1.len(), ColdGroup::ITEM_SIZE);
    }
}
