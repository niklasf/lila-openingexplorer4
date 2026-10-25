use std::{
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use rocksdb::{EnvOptions, Options};

use crate::{
    intermediate::{EntryPacker, IntermediateEntry},
    packed::Zobrist96,
    parquet_to_positions::{
        hot::HotEntry,
        partition::{BUCKETS, Partition, bucket},
    },
    sst::{RollingSstWriter, SstStats},
};

/// Subdirectory of the output for the cold positions column family.
pub const COLD_DIR: &str = "cold_positions";

/// Subdirectory of the output for the positions column family.
pub const POSITIONS_DIR: &str = "positions";

/// Ranges per SST thread and column family, to balance the load.
const RANGES_PER_THREAD: usize = 4;

/// Ranges are at least this many target file sizes, so that only a small
/// fraction of the files are smaller than the target, as the last file of
/// each range.
const MIN_FILES_PER_RANGE: u64 = 4;

/// Initial guesses for SST bytes per record, used until measured.
const INITIAL_COLD_BYTES_PER_RECORD: f64 = 20.0;
const INITIAL_POSITIONS_BYTES_PER_RECORD: f64 = 2.0;

pub struct SstContext<'a> {
    pub opts: &'a Options,
    pub env_opts: &'a EnvOptions,
    pub out: &'a Path,
    /// Start a new SST file once the current one reaches this size.
    pub target_size: u64,
    pub stats: Mutex<OutputStats>,
}

#[derive(Debug, Default, Clone)]
pub struct OutputStats {
    pub cold: SstStats,
    pub positions: SstStats,
    /// Records written as part of a positions entry, excluding hot records.
    pub position_records: u64,
}

impl OutputStats {
    /// SST bytes of each column family per record written so far
    /// (excluding hot records).
    fn bytes_per_record(&self) -> (f64, f64) {
        let records = self.cold.keys + self.position_records;
        if records == 0 {
            (
                INITIAL_COLD_BYTES_PER_RECORD,
                INITIAL_POSITIONS_BYTES_PER_RECORD,
            )
        } else {
            (
                self.cold.bytes as f64 / records as f64,
                self.positions.bytes as f64 / records as f64,
            )
        }
    }

    fn merge(&mut self, other: &OutputStats) {
        self.cold.merge(&other.cold);
        self.positions.merge(&other.positions);
        self.position_records += other.position_records;
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Family {
    Cold,
    Positions,
}

/// Writes a sorted partition and the packed hot entries of its key range
/// to SST files for both column families.
///
/// For each column family, the partition is split into contiguous ranges of
/// buckets, sized to produce a few full files each. All ranges are written
/// in parallel. File names sort in key order.
pub fn write_partition(
    partition: &Partition,
    hot: &[HotEntry],
    memory_partition: usize,
    memory_partitions: usize,
    sst: &SstContext<'_>,
    threads: usize,
) {
    let (cold_per_record, positions_per_record) =
        sst.stats.lock().expect("lock stats").bytes_per_record();
    let records = partition.records() as f64;
    let hot_bytes: usize = hot.iter().map(|(key, value)| key.len() + value.len()).sum();
    let ranges = |estimated_bytes: f64| {
        ((estimated_bytes / (MIN_FILES_PER_RANGE * sst.target_size) as f64) as usize)
            .clamp(1, (threads * RANGES_PER_THREAD).min(BUCKETS))
    };
    let positions_ranges = ranges(records * positions_per_record + hot_bytes as f64);
    let cold_ranges = ranges(records * cold_per_record);
    // The larger positions tasks first.
    let tasks: Vec<(Family, usize, usize)> = (0..positions_ranges)
        .map(|range| (Family::Positions, range, positions_ranges))
        .chain((0..cold_ranges).map(|range| (Family::Cold, range, cold_ranges)))
        .collect();
    let next_task = AtomicUsize::new(0);

    thread::scope(|s| {
        for _ in 0..threads.clamp(1, tasks.len()) {
            s.spawn(|| {
                let mut packer = EntryPacker::default();
                let mut buf = Vec::new();
                while let Some(&(family, range, ranges)) =
                    tasks.get(next_task.fetch_add(1, Ordering::Relaxed))
                {
                    let buckets = range * BUCKETS / ranges..(range + 1) * BUCKETS / ranges;
                    let hot_start = hot
                        .partition_point(|(key, _)| bucket(key, memory_partitions) < buckets.start);
                    let hot_end = hot
                        .partition_point(|(key, _)| bucket(key, memory_partitions) < buckets.end);
                    let stats = write_range(
                        family,
                        &partition.buckets[buckets],
                        &hot[hot_start..hot_end],
                        sst,
                        &format!("{memory_partition:05}_{range:05}"),
                        &mut packer,
                        &mut buf,
                    )
                    .expect("write sst files");
                    sst.stats.lock().expect("lock stats").merge(&stats);
                }
            });
        }
    });
}

fn hash_bytes(entry: &IntermediateEntry) -> &[u8] {
    &entry.0[..Zobrist96::SIZE]
}

/// Writes the keys of one column family for sorted buckets and sorted hot
/// entries of one key range.
fn write_range(
    family: Family,
    buckets: &[Vec<IntermediateEntry>],
    hot: &[HotEntry],
    sst: &SstContext<'_>,
    name: &str,
    packer: &mut EntryPacker,
    buf: &mut Vec<u8>,
) -> Result<OutputStats, rocksdb::Error> {
    let dir = match family {
        Family::Cold => COLD_DIR,
        Family::Positions => POSITIONS_DIR,
    };
    let mut writer = RollingSstWriter::new(
        sst.opts,
        sst.env_opts,
        sst.out.join(dir).join(name),
        sst.target_size,
    );
    let mut position_records = 0;
    let mut hot = hot.iter().peekable();

    for run in buckets
        .iter()
        .flat_map(|bucket| bucket.chunk_by(|a, b| hash_bytes(a) == hash_bytes(b)))
    {
        let hash = hash_bytes(&run[0]);
        match (family, run) {
            (Family::Cold, [single]) => writer.put(hash, &single.game_id().to_bytes())?,
            (Family::Cold, _) => (),
            (Family::Positions, [_]) => (),
            (Family::Positions, _) => {
                while let Some((key, value)) =
                    hot.next_if(|(key, _)| key[..Zobrist96::SIZE] < *hash)
                {
                    writer.put(key, value)?;
                }
                debug_assert!(
                    hot.peek()
                        .is_none_or(|(key, _)| key[..Zobrist96::SIZE] != *hash),
                    "hot hash in partition records"
                );
                for records in run.chunk_by(|a, b| a.key() == b.key()) {
                    packer.pack_sorted(records, buf);
                    writer.put(records[0].key(), buf)?;
                }
                position_records += run.len() as u64;
            }
        }
    }
    if family == Family::Positions {
        for (key, value) in hot {
            writer.put(key, value)?;
        }
    }

    let written = writer.finish()?;
    Ok(match family {
        Family::Cold => OutputStats {
            cold: written,
            ..OutputStats::default()
        },
        Family::Positions => OutputStats {
            positions: written,
            position_records,
            ..OutputStats::default()
        },
    })
}
