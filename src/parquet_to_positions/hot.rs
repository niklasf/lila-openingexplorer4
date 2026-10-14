//! Hot positions: the few positions reached by a large share of all games.
//! Their records would make the memory partitions containing them
//! overweight, so they are aggregated in memory during the first pass
//! instead of being partitioned.

use std::{collections::BTreeMap, sync::Mutex};

use rustc_hash::FxHashMap;

use crate::{
    packed::{GameId, SubStats, Zobrist96, pack_sorted_entry},
    parquet_to_positions::record::{KEY_SIZE, Record, SubKey},
};

/// The set of hot positions, by hash prefix. A small open addressing table
/// that stays in cache.
pub struct HotSet {
    slots: Vec<u64>,
    len: usize,
}

/// Marks an empty slot. A position with this hash prefix is never hot,
/// which only costs performance.
const EMPTY: u64 = u64::MAX;

impl HotSet {
    pub fn new(prefixes: &[u64]) -> HotSet {
        let mut set = HotSet {
            slots: vec![EMPTY; (prefixes.len() * 4).next_power_of_two().max(64)],
            len: 0,
        };
        for &prefix in prefixes {
            if prefix != EMPTY && !set.contains(prefix) {
                let mask = set.slots.len() - 1;
                let mut slot = prefix as usize & mask;
                while set.slots[slot] != EMPTY {
                    slot = (slot + 1) & mask;
                }
                set.slots[slot] = prefix;
                set.len += 1;
            }
        }
        set
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn contains(&self, prefix: u64) -> bool {
        let mask = self.slots.len() - 1;
        let mut slot = prefix as usize & mask;
        loop {
            match self.slots[slot] {
                EMPTY => return false,
                p if p == prefix => return true,
                _ => slot = (slot + 1) & mask,
            }
        }
    }
}

/// Games kept per sub-entry.
pub const GAMES_PER_SUB_ENTRY: usize = 8;

/// Accumulated sub-entry of a hot position.
#[derive(Default, Clone)]
struct HotSub {
    stats: SubStats,
    /// Games in canonical order, truncated to the last `GAMES_PER_SUB_ENTRY`
    /// once in a while.
    games: Vec<Record>,
}

impl HotSub {
    fn add(&mut self, record: &Record) {
        self.stats.add(record.rating(), record.outcome());
        self.games.push(*record);
        if self.games.len() >= 4 * GAMES_PER_SUB_ENTRY {
            self.truncate();
        }
    }

    fn truncate(&mut self) {
        self.games.sort_unstable();
        self.games
            .drain(..self.games.len().saturating_sub(GAMES_PER_SUB_ENTRY));
    }

    fn merge(&mut self, other: HotSub) {
        self.stats.merge(&other.stats);
        self.games.extend(other.games);
        self.truncate();
    }
}

/// Accumulated entry of one hot position in one month.
#[derive(Default)]
struct HotEntry {
    subs: FxHashMap<SubKey, HotSub>,
    records: u64,
}

impl HotEntry {
    fn add(&mut self, record: &Record) {
        self.subs.entry(record.sub_key()).or_default().add(record);
        self.records += 1;
    }

    fn merge(&mut self, other: HotEntry) {
        for (key, sub) in other.subs {
            self.subs.entry(key).or_default().merge(sub);
        }
        self.records += other.records;
    }

    /// Packs the entry as a `LichessEntry` value in canonical order.
    fn pack(mut self) -> Vec<u8> {
        let mut subs: Vec<(SubKey, HotSub)> = self.subs.drain().collect();
        subs.sort_unstable_by_key(|(key, _)| *key);
        let games: Vec<Vec<GameId>> = subs
            .iter_mut()
            .map(|(_, sub)| {
                sub.truncate();
                sub.games.iter().map(Record::game_id).collect()
            })
            .collect();
        let mut buf = Vec::new();
        pack_sorted_entry(
            &mut buf,
            subs.iter().zip(&games).map(|((key, sub), games)| {
                (
                    key.uci_move(),
                    key.group_header(),
                    sub.stats,
                    games.as_slice(),
                )
            }),
        );
        buf
    }
}

/// Entries of hot positions, keyed by hash and month, sharded by hash.
pub struct HotMap {
    shards: Vec<Mutex<FxHashMap<[u8; KEY_SIZE], HotEntry>>>,
}

impl HotMap {
    pub fn new() -> HotMap {
        HotMap {
            shards: (0..64).map(|_| Mutex::new(FxHashMap::default())).collect(),
        }
    }

    fn shard(&self, key: &[u8; KEY_SIZE]) -> usize {
        usize::from(key[0] >> 2) & (self.shards.len() - 1)
    }

    /// Takes and packs all entries, sorted by key.
    pub fn take_packed(&self) -> Vec<HotPacked> {
        let mut entries: BTreeMap<[u8; KEY_SIZE], HotEntry> = BTreeMap::new();
        for shard in &self.shards {
            entries.extend(shard.lock().expect("lock hot shard").drain());
        }
        entries
            .into_iter()
            .map(|(key, entry)| HotPacked {
                key,
                records: entry.records,
                value: entry.pack(),
            })
            .collect()
    }
}

impl Default for HotMap {
    fn default() -> Self {
        Self::new()
    }
}

/// A packed entry of a hot position in one month.
pub struct HotPacked {
    pub key: [u8; KEY_SIZE],
    pub records: u64,
    pub value: Vec<u8>,
}

impl HotPacked {
    pub fn hash(&self) -> Zobrist96 {
        Zobrist96::from_bytes(self.key[..12].try_into().expect("12 bytes"))
    }
}

/// Hot entries accumulated by one thread, merged into the shared map once
/// in a while.
pub struct LocalHot {
    entries: FxHashMap<[u8; KEY_SIZE], HotEntry>,
    records_since_flush: u64,
}

/// Records after which the local entries are merged into the shared map.
const FLUSH_RECORDS: u64 = 1 << 20;

impl LocalHot {
    pub fn new() -> LocalHot {
        LocalHot {
            entries: FxHashMap::default(),
            records_since_flush: 0,
        }
    }

    #[inline]
    pub fn add(&mut self, record: &Record) {
        self.entries.entry(*record.key()).or_default().add(record);
        self.records_since_flush += 1;
    }

    pub fn needs_flush(&self) -> bool {
        self.records_since_flush >= FLUSH_RECORDS
    }

    pub fn flush(&mut self, shared: &HotMap) {
        for (key, entry) in self.entries.drain() {
            let mut shard = shared.shards[shared.shard(&key)]
                .lock()
                .expect("lock hot shard");
            match shard.get_mut(&key) {
                Some(existing) => existing.merge(entry),
                None => {
                    shard.insert(key, entry);
                }
            }
        }
        self.records_since_flush = 0;
    }
}

impl Default for LocalHot {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;
    use crate::packed::LichessEntry;

    #[test]
    fn test_hot_set() {
        arbtest(|u| {
            let prefixes: Vec<u64> = u.arbitrary()?;
            let set = HotSet::new(&prefixes);
            for &p in &prefixes {
                assert_eq!(set.contains(p), p != EMPTY);
            }
            let other: u64 = u.arbitrary()?;
            assert_eq!(
                set.contains(other),
                prefixes.contains(&other) && other != EMPTY
            );
            Ok(())
        });
    }

    #[test]
    fn test_hot_entry_matches_lichess_entry() {
        arbtest(|u| {
            // Records of one key, added through several local maps in
            // arbitrary order, must pack like the sorted records.
            let key: [u8; KEY_SIZE] = u.arbitrary()?;
            let mut records: Vec<Record> = Vec::new();
            let raw: Vec<[u8; crate::parquet_to_positions::record::RECORD_SIZE]> = u.arbitrary()?;
            for r in raw {
                let mut r = Record(r);
                r.0[..KEY_SIZE].copy_from_slice(&key);
                r.0[24] &= 0b11;
                // Valid speed.
                r.0[16] = (r.0[16] & 0x1f) | ((r.0[17] % 6 + 1) << 5);
                r.0[18..24].copy_from_slice(&u.arbitrary::<GameId>()?.to_bytes());
                records.push(r);
            }
            let shared = HotMap::new();
            let mut locals: Vec<LocalHot> = (0..3).map(|_| LocalHot::new()).collect();
            for r in &records {
                locals[usize::from(u.arbitrary::<u8>()?) % 3].add(r);
            }
            for local in &mut locals {
                local.flush(&shared);
            }
            let packed = shared.take_packed();
            let mut actual = LichessEntry::new();
            if let Some(packed) = packed.first() {
                assert_eq!(packed.key, key);
                assert_eq!(packed.records, records.len() as u64);
                actual.merge_unpack_entire(&mut &packed.value[..]);
            }

            records.sort_unstable();
            let mut expected = LichessEntry::new();
            for r in &records {
                expected.add_game(
                    r.uci_move(),
                    r.speed(),
                    r.rating(),
                    r.outcome(),
                    r.game_id(),
                );
            }
            expected.truncate_games(GAMES_PER_SUB_ENTRY);
            assert_eq!(actual, expected);
            Ok(())
        });
    }
}
