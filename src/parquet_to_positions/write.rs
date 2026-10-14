//! Writing a memory partition to SST files of the two column families.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rocksdb::SstFileWriter;

use crate::{
    packed::{GameId, LichessEntry, SubStats, Zobrist96, pack_sorted_entry},
    parquet_to_positions::{
        hot::{GAMES_PER_SUB_ENTRY, HotPacked, HotSet},
        partition::{MemoryPartition, RUN_BUCKET_BITS, RUN_BUCKETS},
        record::{Record, SubKey},
    },
    sst::{SST_MAX_SIZE, SST_ROLL_SIZE, SstContext},
};

/// Initial guess for the ratio of SST bytes to raw key and value bytes of
/// the cold column family (uncompressed, with index and restart overhead).
const INITIAL_COLD_RATIO: f64 = 1.15;

/// Raw bytes of a cold record: hash and game id.
const COLD_RECORD_BYTES: usize = Zobrist96::SIZE + GameId::SIZE;

/// Output of a run: the two column families and their counters.
pub struct Output {
    pub cold: SstContext,
    pub positions: SstContext,
    /// Records written, including those of hot entries.
    pub records: AtomicU64,
    /// Keys of the cold column family.
    pub cold_keys: AtomicU64,
    /// Keys of the positions column family.
    pub position_keys: AtomicU64,
    /// Keys of hot entries that also had partitioned records, merged with
    /// the slow path.
    pub hot_merged_keys: AtomicU64,
}

/// A sequence of SST files of bounded size.
struct SstSequence<'a> {
    ctx: &'a SstContext,
    prefix: String,
    seq: usize,
    writer: Option<SstFileWriter<'a>>,
    keys_in_file: u64,
    raw_in_file: u64,
}

impl<'a> SstSequence<'a> {
    fn new(ctx: &'a SstContext, prefix: String) -> SstSequence<'a> {
        SstSequence {
            ctx,
            prefix,
            seq: 0,
            writer: None,
            keys_in_file: 0,
            raw_in_file: 0,
        }
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), rocksdb::Error> {
        let w = match &mut self.writer {
            Some(w) => w,
            None => {
                let w = self
                    .ctx
                    .create_writer(&format!("{}_{}.sst", self.prefix, self.seq))?;
                self.seq += 1;
                self.writer.insert(w)
            }
        };
        w.put(key, value)?;
        self.keys_in_file += 1;
        self.raw_in_file += (key.len() + value.len()) as u64;
        if w.file_size() >= SST_ROLL_SIZE {
            self.finish()?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), rocksdb::Error> {
        if let Some(mut w) = self.writer.take() {
            w.finish()?;
            let bytes = w.file_size();
            assert!(bytes <= SST_MAX_SIZE, "sst file too large: {bytes}");
            self.ctx
                .stats
                .add_file(self.keys_in_file, bytes, self.raw_in_file);
            self.keys_in_file = 0;
            self.raw_in_file = 0;
        }
        Ok(())
    }
}

/// Writes a memory partition covering the hashes `lo..hi`, together with
/// the hot entries of that range, to SST files of both column families.
///
/// The range is split into sub-ranges of whole buckets, of about one full
/// cold SST file each, written in parallel. File names sort in key order.
#[allow(clippy::too_many_arguments)]
pub fn write_partition(
    partition: &MemoryPartition,
    hot: &[HotPacked],
    hot_set: &HotSet,
    memory_partition: usize,
    lo: Zobrist96,
    hi: Option<Zobrist96>,
    partition_bits: u32,
    output: &Output,
    threads: usize,
) {
    let records = partition.len();
    let hot_lo = hot.partition_point(|h| h.hash() < lo);
    let hot_hi = match hi {
        Some(hi) => hot.partition_point(|h| h.hash() < hi),
        None => hot.len(),
    };
    let hot = &hot[hot_lo..hot_hi];
    if records == 0 && hot.is_empty() {
        return;
    }

    let ratio = output.cold.stats.get().ratio(INITIAL_COLD_RATIO);
    let records_per_range = (SST_ROLL_SIZE as f64 * 0.97 / ratio) as usize / COLD_RECORD_BYTES;
    let ranges = records
        .div_ceil(records_per_range.max(1))
        .max(threads)
        .clamp(1, RUN_BUCKETS);

    // Hash of the start of a bucket.
    let bucket_shift = 64 - partition_bits - RUN_BUCKET_BITS;
    let bucket_start = |bucket: usize| -> Option<Zobrist96> {
        if bucket >= RUN_BUCKETS {
            hi
        } else {
            Some(Zobrist96::from_prefix(
                lo.prefix() + ((bucket as u64) << bucket_shift),
            ))
        }
    };
    let next_range = AtomicUsize::new(0);

    std::thread::scope(|s| {
        for _ in 0..threads.min(ranges) {
            s.spawn(|| {
                let mut writer = RangeWriter::new(output, hot_set);
                loop {
                    let i = next_range.fetch_add(1, Ordering::Relaxed);
                    if i >= ranges {
                        break;
                    }
                    let buckets = i * RUN_BUCKETS / ranges..(i + 1) * RUN_BUCKETS / ranges;
                    let range_lo = bucket_start(buckets.start).expect("range start");
                    let range_hi = bucket_start(buckets.end);
                    let hot_range = &hot[hot.partition_point(|h| h.hash() < range_lo)
                        ..match range_hi {
                            Some(hi) => hot.partition_point(|h| h.hash() < hi),
                            None => hot.len(),
                        }];
                    writer
                        .write_range(
                            partition,
                            buckets,
                            hot_range,
                            &format!("{memory_partition:04}_{i:05}"),
                        )
                        .expect("write sst files");
                }
            });
        }
    });
}

/// Writes key ranges, reusing buffers.
struct RangeWriter<'a> {
    output: &'a Output,
    hot_set: &'a HotSet,
    group: Vec<Record>,
    subs: Vec<(SubKey, SubStats, Vec<GameId>)>,
    value: Vec<u8>,
}

impl<'a> RangeWriter<'a> {
    fn new(output: &'a Output, hot_set: &'a HotSet) -> RangeWriter<'a> {
        RangeWriter {
            output,
            hot_set,
            group: Vec::new(),
            subs: Vec::new(),
            value: Vec::new(),
        }
    }

    /// Writes the records of a range of buckets and the hot entries of the
    /// same hash range.
    fn write_range(
        &mut self,
        partition: &MemoryPartition,
        buckets: std::ops::Range<usize>,
        hot: &[HotPacked],
        name: &str,
    ) -> Result<(), rocksdb::Error> {
        let mut cold = SstSequence::new(&self.output.cold, format!("cold_{name}"));
        let mut positions = SstSequence::new(&self.output.positions, format!("positions_{name}"));
        let mut merge = partition.merge_buckets(buckets);
        let mut pending = merge.next();
        let mut hot_next = 0;
        let (mut records, mut cold_keys, mut position_keys, mut hot_merged) =
            (0u64, 0u64, 0u64, 0u64);

        loop {
            // The next hash from either source.
            let hash = match (
                pending.map(|r| r.hash()),
                hot.get(hot_next).map(|h| h.hash()),
            ) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => break,
            };
            let hash_words = hash.words();

            // Records of the hash.
            self.group.clear();
            while let Some(record) = pending {
                if record.hash_words() != hash_words {
                    break;
                }
                self.group.push(record);
                pending = merge.next();
            }
            records += self.group.len() as u64;

            // Hot entries of the hash.
            let hot_start = hot_next;
            while hot.get(hot_next).is_some_and(|h| h.hash() == hash) {
                hot_next += 1;
            }
            let hot_entries = &hot[hot_start..hot_next];

            if hot_entries.is_empty()
                && self.group.len() == 1
                && !self.hot_set.contains(hash.prefix())
            {
                cold.put(hash.as_bytes(), &self.group[0].0[18..24])?;
                cold_keys += 1;
                continue;
            }

            // Entries per month, from partitioned records, hot entries or
            // both.
            let mut i = 0;
            let mut h = 0;
            while i < self.group.len() || h < hot_entries.len() {
                let record_key = (i < self.group.len()).then(|| *self.group[i].key());
                let hot_key = hot_entries.get(h).map(|e| e.key);
                let key = match (record_key, hot_key) {
                    (Some(a), Some(b)) => a.min(b),
                    (Some(a), None) => a,
                    (None, Some(b)) => b,
                    (None, None) => unreachable!(),
                };
                let start = i;
                while i < self.group.len() && *self.group[i].key() == key {
                    i += 1;
                }
                let month_records = &self.group[start..i];
                let hot_entry = hot_entries.get(h).filter(|e| e.key == key);
                match hot_entry {
                    Some(hot_entry) => {
                        h += 1;
                        records += hot_entry.records;
                        if month_records.is_empty() {
                            positions.put(&key, &hot_entry.value)?;
                        } else {
                            // A hot position that leaked into the partition:
                            // merge with the slow path.
                            hot_merged += 1;
                            let mut entry = LichessEntry::new();
                            entry.merge_unpack_entire(&mut &hot_entry.value[..]);
                            for r in month_records {
                                entry.add_game(
                                    r.uci_move(),
                                    r.speed(),
                                    r.rating(),
                                    r.outcome(),
                                    r.game_id(),
                                );
                            }
                            entry.truncate_games(GAMES_PER_SUB_ENTRY);
                            self.value.clear();
                            entry.pack_entire(&mut self.value);
                            positions.put(&key, &self.value)?;
                        }
                    }
                    None => {
                        pack_records(month_records, &mut self.subs, &mut self.value);
                        positions.put(&key, &self.value)?;
                    }
                }
                position_keys += 1;
            }
        }

        cold.finish()?;
        positions.finish()?;
        self.output.records.fetch_add(records, Ordering::Relaxed);
        self.output
            .cold_keys
            .fetch_add(cold_keys, Ordering::Relaxed);
        self.output
            .position_keys
            .fetch_add(position_keys, Ordering::Relaxed);
        self.output
            .hot_merged_keys
            .fetch_add(hot_merged, Ordering::Relaxed);
        Ok(())
    }
}

/// Packs the entry of the sorted records of one key.
pub fn pack_records(
    records: &[Record],
    subs: &mut Vec<(SubKey, SubStats, Vec<GameId>)>,
    value: &mut Vec<u8>,
) {
    let mut spare: Vec<Vec<GameId>> = subs.drain(..).map(|(_, _, games)| games).collect();
    let mut i = 0;
    while i < records.len() {
        let key = records[i].sub_key();
        let mut stats = SubStats::default();
        let mut games = spare.pop().unwrap_or_default();
        games.clear();
        let start = i;
        while i < records.len() && records[i].sub_key() == key {
            stats.add(records[i].rating(), records[i].outcome());
            i += 1;
        }
        games.extend(
            records[i - (i - start).min(GAMES_PER_SUB_ENTRY)..i]
                .iter()
                .map(Record::game_id),
        );
        subs.push((key, stats, games));
    }
    value.clear();
    pack_sorted_entry(
        value,
        subs.iter().map(|(key, stats, games)| {
            (key.uci_move(), key.group_header(), *stats, games.as_slice())
        }),
    );
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;
    use crate::parquet_to_positions::record::{KEY_SIZE, RECORD_SIZE};

    #[test]
    fn test_pack_records_matches_lichess_entry() {
        arbtest(|u| {
            let key: [u8; KEY_SIZE] = u.arbitrary()?;
            let mut records: Vec<Record> = Vec::new();
            let raw: Vec<[u8; RECORD_SIZE]> = u.arbitrary()?;
            for r in raw {
                let mut r = Record(r);
                r.0[..KEY_SIZE].copy_from_slice(&key);
                r.0[24] &= 0b11;
                r.0[16] = (r.0[16] & 0x1f) | ((r.0[17] % 6 + 1) << 5);
                r.0[18..24].copy_from_slice(&u.arbitrary::<GameId>()?.to_bytes());
                records.push(r);
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

            let mut subs = Vec::new();
            let mut value = Vec::new();
            pack_records(&records, &mut subs, &mut value);
            let mut actual = LichessEntry::new();
            actual.merge_unpack_entire(&mut &value[..]);
            assert_eq!(actual, expected);
            Ok(())
        });
    }
}
