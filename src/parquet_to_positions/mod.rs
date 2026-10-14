//! Builds the position column families from parquet files.
//!
//! Every game is replayed into one record per position (see `record`),
//! keyed by the `Zobrist96` hash of the position and the month of the
//! game. Records are sorted by key with the same pass and partition scheme
//! as `parquet_to_games`, and written as:
//!
//! * `cold`: hash -> game id, for positions reached by a single game;
//! * `positions`: hash and month -> `LichessEntry`, for all other
//!   positions.
//!
//! A handful of positions (the starting position and other shallow ones)
//! are reached by a large share of all games. Their records would make
//! memory partitions overweight, so they are aggregated in memory during
//! the first pass (see `hot`) and merged into the output.

pub mod hot;
pub mod partition;
pub mod record;
pub mod replay;
pub mod write;

use std::{
    fmt, fs, io, mem,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crossbeam::channel;
use indicatif::{MultiProgress, ProgressBar};
use nohash_hasher::IntMap;

use crate::{
    packed::Zobrist96,
    parquet::{Input, PrefetchTimes, Prefetcher},
    parquet_to_positions::{
        hot::{HotMap, HotPacked, HotSet, LocalHot},
        partition::MemoryPartition,
        record::{CHUNK_BYTES, CHUNK_RECORDS, RECORD_SIZE, Record, encode_chunk},
        replay::{Replayer, Skip},
        write::{Output, write_partition},
    },
    scatter::{ChunkCompressor, Gauge, Layout, ScratchFile, seconds, timed},
    sst::{SstConfig, SstContext, SstStats},
    util::{log, rows_style},
};

/// Memory partitions per pass that are kept in memory while reading the
/// input, rather than being written to scratch space and read back.
const RESIDENT_PARTITIONS_PER_PASS: usize = 1;

/// Partitions in memory at the same time: one being written to SST files,
/// one being loaded and sorted, and one resident partition of the next pass.
const PARTITIONS_IN_MEMORY: u64 = 3;

/// Assumed ratio of compressed scratch chunks to record bytes (measured
/// about 0.70 with zstd level 1 and the bucketed, transposed chunk layout).
const SCRATCH_RATIO: f64 = 0.75;

/// Fixed memory overhead for the parquet reader, rocksdb and allocator slack.
const BASE_MEMORY: u64 = 4 << 30;

/// A position is hot if its records would take more than this fraction of
/// a memory partition.
const HOT_PARTITION_FRACTION: f64 = 1.0 / 16.0;

/// Hot positions are only looked for among the first plies of a game.
/// Deeper records of hot positions (transpositions) are merged when
/// writing.
const MAX_HOT_PLY: usize = 24;

/// Row groups sampled for the size estimate.
const SAMPLED_ROW_GROUPS: usize = 256;

/// Row groups among the sampled ones whose positions are counted to find
/// hot positions.
const HOT_SAMPLED_ROW_GROUPS: usize = 32;

/// The part of the key space handled by a run: hashes whose top
/// `subset_bits` are `subset`, split into memory partitions by the
/// following `memory_bits`.
#[derive(Debug, Copy, Clone)]
pub struct KeySpace {
    pub subset_bits: u32,
    pub subset: usize,
    pub memory_bits: u32,
}

impl KeySpace {
    /// Whether the hash is in the subset.
    #[inline]
    pub fn contains(&self, prefix: u64) -> bool {
        self.subset_bits == 0 || (prefix >> (64 - self.subset_bits)) as usize == self.subset
    }

    /// Memory partition of a hash in the subset.
    #[inline]
    pub fn memory_partition(&self, prefix: u64) -> usize {
        let bits = self.subset_bits + self.memory_bits;
        if bits == 0 {
            0
        } else {
            (prefix >> (64 - bits)) as usize & ((1 << self.memory_bits) - 1)
        }
    }

    /// Hash range of a memory partition.
    pub fn range(&self, memory_partition: usize) -> (Zobrist96, Option<Zobrist96>) {
        let bits = self.subset_bits + self.memory_bits;
        let index = (self.subset << self.memory_bits) | memory_partition;
        let lo = Zobrist96::partition_start(index, bits);
        let hi = (index + 1 < 1 << bits).then(|| Zobrist96::partition_start(index + 1, bits));
        (lo, hi)
    }
}

/// Estimated data sizes and hot positions, from a sample of row groups.
pub struct Estimate {
    /// Rows of the whole input.
    rows: u64,
    sampled_rows: u64,
    sampled_games: u64,
    /// Records in the subset of the sampled games.
    sampled_records: u64,
    /// Rows and records of the hot sample.
    hot_sampled_rows: u64,
    hot_sampled_records: u64,
    /// Sampled counts of positions seen at least twice, by hash prefix.
    counts: Vec<(u64, u32)>,
    skipped: [u64; Skip::ALL.len()],
}

impl Estimate {
    fn sample(input: &Input, key_space: KeySpace, threads: usize) -> Estimate {
        let num_row_groups = input.num_row_groups();
        let rows = input.total_rows();
        let samples = SAMPLED_ROW_GROUPS.min(num_row_groups);
        let sampled: Vec<usize> = (0..samples).map(|k| k * num_row_groups / samples).collect();
        let hot_every = samples.div_ceil(HOT_SAMPLED_ROW_GROUPS).max(1);

        let next = AtomicUsize::new(0);
        let sampled_rows = AtomicU64::new(0);
        let sampled_games = AtomicU64::new(0);
        let sampled_records = AtomicU64::new(0);
        let hot_sampled_rows = AtomicU64::new(0);
        let hot_sampled_records = AtomicU64::new(0);
        let skipped: Vec<AtomicU64> = Skip::ALL.iter().map(|_| AtomicU64::new(0)).collect();

        let counts: Vec<IntMap<u64, u32>> = thread::scope(|s| {
            let handles: Vec<_> = (0..threads.max(1))
                .map(|_| {
                    s.spawn(|| {
                        let mut replayer = Replayer::new();
                        let mut counts: IntMap<u64, u32> = IntMap::default();
                        loop {
                            let k = next.fetch_add(1, Ordering::Relaxed);
                            let Some(&row_group) = sampled.get(k) else {
                                break;
                            };
                            let count_hot = k % hot_every == 0;
                            let batch = input.read_games(row_group).expect("read row group");
                            sampled_rows.fetch_add(batch.len() as u64, Ordering::Relaxed);
                            if count_hot {
                                hot_sampled_rows.fetch_add(batch.len() as u64, Ordering::Relaxed);
                            }
                            for row in batch.rows() {
                                let records = match replayer.replay(&row) {
                                    Ok(records) => records,
                                    Err(skip) => {
                                        skipped[skip as usize].fetch_add(1, Ordering::Relaxed);
                                        continue;
                                    }
                                };
                                sampled_games.fetch_add(1, Ordering::Relaxed);
                                let in_subset = records
                                    .iter()
                                    .filter(|r| key_space.contains(r.hash_prefix()))
                                    .count() as u64;
                                sampled_records.fetch_add(in_subset, Ordering::Relaxed);
                                if count_hot {
                                    hot_sampled_records.fetch_add(in_subset, Ordering::Relaxed);
                                    for r in records.iter().take(MAX_HOT_PLY) {
                                        if key_space.contains(r.hash_prefix()) {
                                            *counts.entry(r.hash_prefix()).or_insert(0) += 1;
                                        }
                                    }
                                }
                            }
                        }
                        counts
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("sample thread"))
                .collect()
        });

        let mut merged: IntMap<u64, u32> = IntMap::default();
        for counts in counts {
            for (prefix, count) in counts {
                *merged.entry(prefix).or_insert(0) += count;
            }
        }
        let mut counts: Vec<(u64, u32)> = merged
            .into_iter()
            .filter(|&(_, count)| count >= 2)
            .collect();
        counts.sort_unstable_by_key(|&(prefix, count)| (std::cmp::Reverse(count), prefix));

        Estimate {
            rows,
            sampled_rows: sampled_rows.into_inner(),
            sampled_games: sampled_games.into_inner(),
            sampled_records: sampled_records.into_inner(),
            hot_sampled_rows: hot_sampled_rows.into_inner(),
            hot_sampled_records: hot_sampled_records.into_inner(),
            counts,
            skipped: skipped
                .iter()
                .map(|c| c.load(Ordering::Relaxed))
                .collect::<Vec<_>>()
                .try_into()
                .expect("skip counts"),
        }
    }

    /// Estimated records of the whole input in the subset, with a safety
    /// margin for sampling error.
    fn records(&self) -> u64 {
        if self.sampled_rows == 0 {
            return 0;
        }
        (self.sampled_records as f64 / self.sampled_rows as f64 * self.rows as f64 * 1.05) as u64
    }

    /// Estimated games of the whole input reaching a position with the
    /// given sampled count.
    fn scale_hot_count(&self, count: u32) -> u64 {
        if self.hot_sampled_rows == 0 {
            return 0;
        }
        (u64::from(count) as f64 * self.rows as f64 / self.hot_sampled_rows as f64) as u64
    }

    /// Hot positions: those estimated to have at least `threshold` games.
    /// Returns hash prefixes with estimated games.
    fn hot(&self, threshold: u64) -> Vec<(u64, u64)> {
        self.counts
            .iter()
            .map(|&(prefix, count)| (prefix, self.scale_hot_count(count)))
            .take_while(|&(_, games)| games >= threshold)
            .collect()
    }

    /// Fraction of all records belonging to the given hot positions.
    fn hot_fraction(&self, hot: &[(u64, u64)]) -> f64 {
        if self.hot_sampled_records == 0 {
            return 0.0;
        }
        let hot_prefixes: Vec<u64> = hot.iter().map(|&(prefix, _)| prefix).collect();
        let sampled: u64 = self
            .counts
            .iter()
            .filter(|(prefix, _)| hot_prefixes.contains(prefix))
            .map(|&(_, count)| u64::from(count))
            .sum();
        sampled as f64 / self.hot_sampled_records as f64
    }
}

/// Sizes derived from the estimate for a layout.
struct Sizing {
    /// Records that go through partitions (not hot).
    records: u64,
}

impl Sizing {
    fn partition_memory(&self, layout: Layout) -> u64 {
        (self.records * RECORD_SIZE as u64 * 21 / 20) / layout.memory_partitions() as u64
    }

    /// Peak memory for the given layout and read threads.
    fn memory(&self, layout: Layout, read_threads: u64) -> u64 {
        let buffers =
            read_threads * (layout.scratch_partitions_per_pass() as u64 + 1) * CHUNK_BYTES as u64;
        BASE_MEMORY + PARTITIONS_IN_MEMORY * self.partition_memory(layout) + buffers
    }

    /// Scratch space needed by one pass.
    fn scratch(&self, layout: Layout) -> u64 {
        let per_partition = self.records as f64 * RECORD_SIZE as f64 * SCRATCH_RATIO
            / layout.memory_partitions() as f64;
        (per_partition * layout.scratch_partitions_per_pass() as f64) as u64
    }

    /// Picks the fewest passes that fit the scratch budget, and for them the
    /// fewest memory partitions that fit the memory budget.
    fn layout(&self, memory_budget: u64, scratch_budget: u64, read_threads: u64) -> Layout {
        for disk_bits in 0..=16 {
            for memory_bits in disk_bits..=disk_bits + 12 {
                let layout = Layout {
                    memory_bits,
                    disk_bits,
                    resident: RESIDENT_PARTITIONS_PER_PASS,
                };
                if self.scratch(layout) <= scratch_budget
                    && self.memory(layout, read_threads) <= memory_budget
                {
                    return layout;
                }
            }
        }
        panic!("no layout fits the budgets");
    }
}

pub struct Config {
    /// Threads to use in total, unless overridden for a stage.
    pub threads: usize,
    /// Maximum bytes of memory to use. Determines the number of memory
    /// partitions.
    pub memory_budget: u64,
    /// Maximum bytes to use in scratch space. Determines the number of
    /// passes over the input.
    pub scratch_budget: u64,
    /// Handle only the hashes whose top `log2(n)` bits are `k`, given as
    /// `(k, n)` with `n` a power of two.
    pub subset: (usize, usize),
    /// Minimum estimated number of games for a position to be hot, instead
    /// of deriving it from the partition size.
    pub hot_threshold: Option<u64>,
    /// Threads for decoding and replaying the input.
    pub read_threads: Option<usize>,
    /// Threads for compressing and writing scratch chunks.
    pub write_threads: Option<usize>,
    /// Threads for loading and sorting scratch files.
    pub load_threads: Option<usize>,
    /// Threads for writing SST files.
    pub sst_threads: Option<usize>,
    /// Concurrent reads of the input, ahead of the decoding threads.
    pub io_depth: usize,
    /// Use O_DIRECT for the input, scratch files and SST files.
    pub direct_io: bool,
    /// Zstd level for scratch chunks, or 0 for no compression.
    pub scratch_zstd_level: i32,
    /// Only process the first row groups of the inputs.
    pub row_groups: Option<usize>,
    /// Stop after the scatter of the first pass.
    pub scatter_only: bool,
    /// Scan the input for the next pass while partitions of the previous
    /// pass are still being written.
    pub overlap: bool,
    /// Block size of the SST files.
    pub sst_block_size: usize,
    /// Zstd level of the cold SST files, or 0 for no compression.
    pub cold_zstd_level: i32,
    /// Zstd level of the positions SST files, or 0 for no compression.
    pub positions_zstd_level: i32,
}

/// Counters shared by the threads of one run.
#[derive(Default)]
struct Counters {
    games: AtomicU64,
    skipped: [AtomicU64; Skip::ALL.len()],
    /// Records produced by the scatter, including hot ones.
    records: AtomicU64,
    hot_records: AtomicU64,
    loops: AtomicU64,
    /// Bytes currently occupied by scratch files.
    scratch_usage: Gauge,
    /// Partitions written to SST files so far.
    written_partitions: Gauge,
}

/// The input, its estimated size, and the layout and threads chosen to
/// convert it.
pub struct Plan {
    input: Input,
    estimate: Estimate,
    sizing: Sizing,
    key_space: KeySpace,
    layout: Layout,
    /// Hot positions with their estimated games.
    hot: Vec<(u64, u64)>,
    hot_threshold: u64,
    read_threads: usize,
    write_threads: usize,
    load_threads: usize,
    sst_threads: usize,
}

impl Plan {
    pub fn new(parquet: &[PathBuf], config: &Config) -> io::Result<Plan> {
        let threads = config.threads;
        let (subset, subsets) = config.subset;
        if !subsets.is_power_of_two() || subset >= subsets {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid subset {subset}/{subsets}"),
            ));
        }
        let mut key_space = KeySpace {
            subset_bits: subsets.ilog2(),
            subset,
            memory_bits: 0,
        };
        let input = Input::open(parquet, config.row_groups, config.direct_io)?;
        let estimate = Estimate::sample(&input, key_space, threads);

        let provisional_read_threads = config.read_threads.unwrap_or(threads / 2).max(1);
        let layout_for = |sizing: &Sizing, read_threads: usize| {
            sizing.layout(
                config.memory_budget,
                config.scratch_budget,
                read_threads as u64,
            )
        };

        // Hot positions, from a layout with all records.
        let mut sizing = Sizing {
            records: estimate.records(),
        };
        let mut layout = layout_for(&sizing, provisional_read_threads);
        let hot_threshold = config.hot_threshold.unwrap_or_else(|| {
            let partition_records = sizing.records / layout.memory_partitions() as u64;
            ((partition_records as f64 * HOT_PARTITION_FRACTION) as u64).max(2)
        });
        let hot = estimate.hot(hot_threshold);
        sizing.records = (estimate.records() as f64 * (1.0 - estimate.hot_fraction(&hot))) as u64;
        layout = layout_for(&sizing, provisional_read_threads);

        let single_pass = layout.passes() == 1 || !config.overlap;
        let (read_default, write_default, load_default, sst_default) = if single_pass {
            (threads * 3 / 4, threads / 4, threads / 3, threads * 2 / 3)
        } else {
            (threads / 2, threads / 5, threads / 5, threads / 4)
        };
        let read_threads = config.read_threads.unwrap_or(read_default).max(1);
        let write_threads = config.write_threads.unwrap_or(write_default).max(1);
        let load_threads = config.load_threads.unwrap_or(load_default).max(1);
        let sst_threads = config.sst_threads.unwrap_or(sst_default).max(2);
        if read_threads > provisional_read_threads {
            layout = layout_for(&sizing, read_threads);
        }
        key_space.memory_bits = layout.memory_bits;

        Ok(Plan {
            input,
            estimate,
            sizing,
            key_space,
            layout,
            hot,
            hot_threshold,
            read_threads,
            write_threads,
            load_threads,
            sst_threads,
        })
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let e = &self.estimate;
        let records_per_game = if e.sampled_games == 0 {
            0.0
        } else {
            e.sampled_records as f64 / e.sampled_games as f64
        };
        let sampled_skipped: u64 = e.skipped.iter().sum();
        write!(
            f,
            "{} rows, subset {}/{}: about {} records ({:.1} per game, {} of {} sampled games skipped), {} hot positions with at least {} games holding {:.1}% of records; {} passes with {} memory partitions each, about {} GB scratch per pass and {} GB memory; threads: {} read, {} write, {} load, {} sst",
            e.rows,
            self.key_space.subset,
            1usize << self.key_space.subset_bits,
            e.records(),
            records_per_game,
            sampled_skipped,
            e.sampled_games + sampled_skipped,
            self.hot.len(),
            self.hot_threshold,
            100.0 * e.hot_fraction(&self.hot),
            self.layout.passes(),
            self.layout.memory_partitions_per_pass(),
            self.sizing.scratch(self.layout) >> 30,
            self.sizing.memory(self.layout, self.read_threads as u64) >> 30,
            self.read_threads,
            self.write_threads,
            self.load_threads,
            self.sst_threads,
        )?;
        for (i, &(_, games)) in self.hot.iter().enumerate().take(5) {
            write!(f, "{}{games}", if i == 0 { "; top hot: " } else { ", " })?;
        }
        Ok(())
    }
}

/// Result of a conversion.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub games: u64,
    pub skipped: Vec<(&'static str, u64)>,
    pub records: u64,
    pub hot_records: u64,
    pub loops: u64,
    pub cold_keys: u64,
    pub position_keys: u64,
    pub hot_merged_keys: u64,
    pub cold: SstStats,
    pub positions: SstStats,
}

/// A memory partition to process, either resident or in scratch space.
enum PartitionSource {
    Resident(MemoryPartition),
    Scratch(ScratchFile),
}

struct PartitionJob {
    memory_partition: usize,
    source: PartitionSource,
}

/// Converts the planned input into SST files in `out/cold` and
/// `out/positions`, using `scratch_dir` for temporary partition files.
pub fn convert(
    plan: &Plan,
    scratch_dir: &Path,
    out: &Path,
    config: &Config,
    progress: &MultiProgress,
) -> io::Result<Stats> {
    let Plan {
        ref input,
        ref estimate,
        key_space,
        layout,
        ref hot,
        read_threads,
        write_threads,
        load_threads,
        sst_threads,
        ..
    } = *plan;
    let num_rows = estimate.rows;

    // Fail early rather than after the first scatter pass.
    if !fs::metadata(out)
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", out.display())))?
        .is_dir()
    {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{}: not a directory", out.display()),
        ));
    }
    let cold_dir = out.join("cold");
    let positions_dir = out.join("positions");
    fs::create_dir_all(&cold_dir)?;
    fs::create_dir_all(&positions_dir)?;

    let sst_config = |zstd_level| SstConfig {
        block_size: config.sst_block_size,
        zstd_level,
        zstd_dict: 0,
        direct_io: config.direct_io,
    };
    let output = Output {
        cold: SstContext::new(&sst_config(config.cold_zstd_level), &cold_dir),
        positions: SstContext::new(&sst_config(config.positions_zstd_level), &positions_dir),
        records: AtomicU64::new(0),
        cold_keys: AtomicU64::new(0),
        position_keys: AtomicU64::new(0),
        hot_merged_keys: AtomicU64::new(0),
    };
    let counters = Counters::default();
    let hot_set = HotSet::new(&hot.iter().map(|&(prefix, _)| prefix).collect::<Vec<_>>());
    let hot_map = HotMap::new();
    // Hot entries, packed after the first scatter pass.
    let hot_packed: OnceLock<Vec<HotPacked>> = OnceLock::new();

    let progress =
        progress.add(ProgressBar::new(num_rows * layout.passes() as u64).with_style(rows_style()));

    thread::scope(|s| {
        let progress = &progress;
        let output = &output;
        let input = &input;
        let counters = &counters;
        let hot_set = &hot_set;
        let hot_map = &hot_map;
        let hot_packed = &hot_packed;

        // Loader: loads and sorts partitions in order, one ahead of the
        // writer.
        let (job_tx, job_rx) = channel::unbounded::<PartitionJob>();
        let (sorted_tx, sorted_rx) = channel::bounded::<(usize, MemoryPartition)>(0);
        s.spawn(move || {
            for job in job_rx {
                let memory_partition = job.memory_partition;
                let timer = Instant::now();
                let partition = match job.source {
                    PartitionSource::Resident(mut partition) => {
                        partition.sort_runs(load_threads);
                        partition
                    }
                    PartitionSource::Scratch(scratch) => {
                        let partition = MemoryPartition::load(&scratch, load_threads);
                        scratch
                            .remove(&counters.scratch_usage)
                            .expect("remove scratch");
                        partition
                    }
                };
                log(
                    progress,
                    format!(
                        "partition {memory_partition}: load and sort {:.1?} ({} records in {} runs)",
                        timer.elapsed(),
                        partition.len(),
                        partition.runs.len()
                    ),
                );
                sorted_tx
                    .send((memory_partition, partition))
                    .expect("send sorted");
            }
        });

        // Writer: writes sorted partitions to SST files.
        s.spawn(move || {
            for (memory_partition, partition) in sorted_rx {
                let timer = Instant::now();
                let (lo, hi) = key_space.range(memory_partition);
                write_partition(
                    &partition,
                    hot_packed.get().expect("hot entries"),
                    hot_set,
                    memory_partition,
                    lo,
                    hi,
                    output,
                    sst_threads,
                );
                counters.written_partitions.add(1);
                let cold = output.cold.stats.get();
                let positions = output.positions.stats.get();
                log(
                    progress,
                    format!(
                        "partition {memory_partition}: write {:.1?} ({} cold and {} positions files so far, {:.1} and {:.1} bytes per key)",
                        timer.elapsed(),
                        cold.files,
                        positions.files,
                        cold.bytes as f64 / cold.keys.max(1) as f64,
                        positions.bytes as f64 / positions.keys.max(1) as f64,
                    ),
                );
            }
        });

        for pass in 0..layout.passes() {
            progress.set_prefix(format!("pass {}/{}", pass + 1, layout.passes()));
            if !config.overlap && pass > 0 {
                progress.set_message("waiting for previous pass");
                let waited = counters
                    .written_partitions
                    .wait_until(|n| n >= (pass * layout.memory_partitions_per_pass()) as u64);
                log(
                    progress,
                    format!("pass {pass}: waited {waited:.1?} for previous pass"),
                );
            }
            progress.set_message("scattering");
            let timer = Instant::now();
            let (resident, scratch, throttled) = scatter_pass(
                input,
                pass,
                &ScatterConfig {
                    scratch: scratch_dir,
                    zstd_level: config.scratch_zstd_level,
                    // The first pass has nothing to wait for.
                    budget: (pass > 0).then_some(config.scratch_budget),
                    key_space,
                    layout,
                    hot_set,
                    // Hot entries of all passes are collected in the first.
                    hot_map: (pass == 0).then_some(hot_map),
                    read_threads,
                    write_threads,
                    io_depth: config.io_depth.max(1),
                    direct: config.direct_io,
                    counters,
                },
                progress,
            )
            .expect("scatter");
            let scratch_bytes: u64 = scratch.iter().map(|f| f.len()).sum();
            let scratch_raw: u64 = scratch.iter().map(|f| f.raw_len()).sum();
            log(
                progress,
                format!(
                    "pass {pass}: scatter {:.1?} ({} GB scratch, ratio {:.3}, throttled {:.1?})",
                    timer.elapsed(),
                    scratch_bytes >> 30,
                    scratch_bytes as f64 / scratch_raw.max(1) as f64,
                    throttled
                ),
            );
            if pass == 0 {
                let timer = Instant::now();
                let packed = hot_map.take_packed();
                let hot_bytes: usize = packed.iter().map(|h| h.value.len()).sum();
                log(
                    progress,
                    format!(
                        "pass 0: packed {} hot entries ({} records, {} MB) in {:.1?}",
                        packed.len(),
                        counters.hot_records.load(Ordering::Relaxed),
                        hot_bytes >> 20,
                        timer.elapsed()
                    ),
                );
                hot_packed.set(packed).ok().expect("hot entries set once");
            }
            if config.scatter_only {
                for file in scratch {
                    file.remove(&counters.scratch_usage)
                        .expect("remove scratch");
                }
                break;
            }

            // Process resident partitions first, to free their memory.
            for (memory_partition, partition) in resident {
                job_tx
                    .send(PartitionJob {
                        memory_partition,
                        source: PartitionSource::Resident(partition),
                    })
                    .expect("send job");
            }
            for (memory_partition, file) in scratch.into_iter().enumerate() {
                job_tx
                    .send(PartitionJob {
                        memory_partition: pass * layout.memory_partitions_per_pass()
                            + memory_partition,
                        source: PartitionSource::Scratch(file),
                    })
                    .expect("send job");
            }
        }

        drop(job_tx);
    });

    progress.finish();

    Ok(Stats {
        games: counters.games.load(Ordering::Relaxed),
        skipped: Skip::ALL
            .iter()
            .zip(&counters.skipped)
            .map(|(skip, count)| (skip.name(), count.load(Ordering::Relaxed)))
            .collect(),
        records: counters.records.load(Ordering::Relaxed),
        hot_records: counters.hot_records.load(Ordering::Relaxed),
        loops: counters.loops.load(Ordering::Relaxed),
        cold_keys: output.cold_keys.load(Ordering::Relaxed),
        position_keys: output.position_keys.load(Ordering::Relaxed),
        hot_merged_keys: output.hot_merged_keys.load(Ordering::Relaxed),
        cold: output.cold.stats.get(),
        positions: output.positions.stats.get(),
    })
}

type Resident = Vec<(usize, MemoryPartition)>;

struct ScatterConfig<'a> {
    scratch: &'a Path,
    zstd_level: i32,
    /// Scratch budget to throttle against, if the previous pass is still
    /// freeing scratch space.
    budget: Option<u64>,
    key_space: KeySpace,
    layout: Layout,
    hot_set: &'a HotSet,
    /// Where to collect hot entries, in the first pass only.
    hot_map: Option<&'a HotMap>,
    read_threads: usize,
    write_threads: usize,
    io_depth: usize,
    direct: bool,
    counters: &'a Counters,
}

/// Accumulated time of each scatter stage, summed over its threads.
#[derive(Default)]
struct ScatterTimes {
    prefetch: PrefetchTimes,
    recv: AtomicU64,
    decode: AtomicU64,
    replay: AtomicU64,
    send: AtomicU64,
    compress_recv: AtomicU64,
    encode: AtomicU64,
    compress: AtomicU64,
    write: AtomicU64,
}

impl ScatterTimes {
    fn summary(&self) -> String {
        let s = seconds;
        format!(
            "prefetch: fetch {:.0}s, send {:.0}s; read: recv {:.0}s, decode {:.0}s, replay and route {:.0}s, send {:.0}s; compress: recv {:.0}s, encode {:.0}s, compress {:.0}s, write {:.0}s",
            s(&self.prefetch.fetch),
            s(&self.prefetch.send),
            s(&self.recv),
            s(&self.decode),
            s(&self.replay),
            s(&self.send),
            s(&self.compress_recv),
            s(&self.encode),
            s(&self.compress),
            s(&self.write),
        )
    }
}

fn new_chunk() -> Vec<Record> {
    Vec::with_capacity(CHUNK_RECORDS)
}

/// Reads the entire input and distributes the records of the given pass to
/// its memory partitions. The last `RESIDENT_PARTITIONS_PER_PASS` memory
/// partitions are kept in memory, the others are written to scratch files.
fn scatter_pass(
    input: &Input,
    pass: usize,
    config: &ScatterConfig<'_>,
    progress: &ProgressBar,
) -> io::Result<(Resident, Vec<ScratchFile>, Duration)> {
    let layout = config.layout;
    let key_space = config.key_space;
    let scratch_partitions = layout.scratch_partitions_per_pass();
    let first_memory_partition = pass * layout.memory_partitions_per_pass();
    let partition_bits = key_space.subset_bits + key_space.memory_bits;

    let files: Vec<Mutex<ScratchFile>> = (0..scratch_partitions)
        .map(|i| {
            Ok::<_, io::Error>(Mutex::new(ScratchFile::create(
                config.scratch.join(format!(
                    "positions_{:05}.chunks",
                    first_memory_partition + i
                )),
                config.zstd_level,
                config.direct,
            )?))
        })
        .collect::<io::Result<_>>()?;

    let resident: Mutex<Vec<Vec<Vec<Record>>>> = Mutex::new(Vec::new());
    let throttled_nanos = AtomicU64::new(0);
    let times = ScatterTimes::default();

    thread::scope(|s| {
        let files = &files;
        let resident = &resident;
        let throttled_nanos = &throttled_nanos;
        let times = &times;
        let prefetcher = Prefetcher::spawn(input, config.io_depth, &times.prefetch, s);

        // Write threads encode and compress full chunks and append them to
        // scratch files, throttled by the scratch budget.
        let (tx, rx) = channel::bounded::<(usize, Vec<Record>)>(config.write_threads * 2);

        for _ in 0..config.write_threads {
            let rx = rx.clone();
            s.spawn(move || {
                let mut compressor = ChunkCompressor::new(config.zstd_level);
                let mut encoded = Vec::with_capacity(CHUNK_BYTES);
                while let Ok((file, mut records)) = timed(&times.compress_recv, || rx.recv()) {
                    timed(&times.encode, || {
                        encode_chunk(&mut records, partition_bits, &mut encoded)
                    });
                    drop(records);
                    let compressed = timed(&times.compress, || compressor.compress(&encoded));
                    if let Some(budget) = config.budget {
                        let waited = config
                            .counters
                            .scratch_usage
                            .wait_until(|usage| usage + compressed.padded.len() as u64 <= budget);
                        throttled_nanos.fetch_add(waited.as_nanos() as u64, Ordering::Relaxed);
                    }
                    timed(&times.write, || {
                        files[file].lock().expect("lock scratch file").append(
                            encoded.len(),
                            compressed,
                            &config.counters.scratch_usage,
                        )
                    })
                    .expect("append chunk");
                }
            });
        }

        drop(rx);

        // Read threads replay the games and route the records of this pass
        // to resident partitions or scratch chunks.
        for _ in 0..config.read_threads {
            let tx = tx.clone();
            let prefetcher = prefetcher.clone();
            s.spawn(move || {
                let mut replayer = Replayer::new();
                let mut local_hot = LocalHot::new();
                let mut local_resident: Vec<Vec<Vec<Record>>> = (0..RESIDENT_PARTITIONS_PER_PASS)
                    .map(|_| Vec::new())
                    .collect();
                let mut chunks: Vec<Vec<Record>> =
                    (0..scratch_partitions).map(|_| new_chunk()).collect();
                let mut games = 0u64;
                let mut skipped = [0u64; Skip::ALL.len()];
                let mut records = 0u64;
                let mut hot_records = 0u64;
                let hot_set = config.hot_set;
                let check_hot = !hot_set.is_empty();

                while let Some(fetched) = timed(&times.recv, || prefetcher.recv()) {
                    let batch = timed(&times.decode, || prefetcher.decode(fetched))
                        .expect("read row group");
                    timed(&times.replay, || {
                        for row in batch.rows() {
                            let game_records = match replayer.replay(&row) {
                                Ok(records) => records,
                                Err(skip) => {
                                    skipped[skip as usize] += 1;
                                    continue;
                                }
                            };
                            games += 1;
                            for (i, record) in game_records.iter().enumerate() {
                                let prefix = record.hash_prefix();
                                if !key_space.contains(prefix) {
                                    continue;
                                }
                                records += 1;
                                if check_hot && i < MAX_HOT_PLY && hot_set.contains(prefix) {
                                    hot_records += 1;
                                    if config.hot_map.is_some() {
                                        local_hot.add(record);
                                    }
                                    continue;
                                }
                                let Some(j) = layout.pass_memory_partition(
                                    key_space.memory_partition(prefix),
                                    pass,
                                ) else {
                                    continue;
                                };
                                if j >= scratch_partitions {
                                    let runs = &mut local_resident[j - scratch_partitions];
                                    if runs.last().is_none_or(|run| run.len() >= CHUNK_RECORDS) {
                                        runs.push(new_chunk());
                                    }
                                    runs.last_mut().expect("run").push(*record);
                                } else {
                                    let chunk = &mut chunks[j];
                                    chunk.push(*record);
                                    if chunk.len() >= CHUNK_RECORDS {
                                        let full = mem::replace(chunk, new_chunk());
                                        timed(&times.send, || tx.send((j, full)))
                                            .expect("send chunk");
                                    }
                                }
                            }
                            if let Some(hot_map) = config.hot_map
                                && local_hot.needs_flush()
                            {
                                local_hot.flush(hot_map);
                            }
                        }
                    });

                    progress.inc(batch.len() as u64);
                }

                for (j, chunk) in chunks.into_iter().enumerate() {
                    if !chunk.is_empty() {
                        tx.send((j, chunk)).expect("send chunk");
                    }
                }
                if let Some(hot_map) = config.hot_map {
                    local_hot.flush(hot_map);
                }

                let counters = config.counters;
                counters.games.fetch_add(games, Ordering::Relaxed);
                for (count, skipped) in counters.skipped.iter().zip(skipped) {
                    count.fetch_add(skipped, Ordering::Relaxed);
                }
                counters.records.fetch_add(records, Ordering::Relaxed);
                counters
                    .hot_records
                    .fetch_add(hot_records, Ordering::Relaxed);
                counters.loops.fetch_add(replayer.loops, Ordering::Relaxed);
                resident
                    .lock()
                    .expect("lock resident")
                    .append(&mut local_resident);
            });
        }

        // Write threads finish once all chunks are written.
        drop(tx);
    });

    log(progress, format!("pass {pass}: {}", times.summary()));

    let files: Vec<ScratchFile> = files
        .into_iter()
        .map(|file| file.into_inner().expect("scratch file"))
        .collect();

    // Merge per-thread resident runs.
    let mut per_thread = resident.into_inner().expect("resident");
    let mut merged: Vec<MemoryPartition> = (0..RESIDENT_PARTITIONS_PER_PASS)
        .map(|_| MemoryPartition::default())
        .collect();
    while !per_thread.is_empty() {
        for (i, runs) in per_thread.drain(..RESIDENT_PARTITIONS_PER_PASS).enumerate() {
            merged[i]
                .runs
                .extend(runs.into_iter().filter(|run| !run.is_empty()));
        }
    }
    let resident = merged
        .into_iter()
        .enumerate()
        .map(|(i, partition)| (first_memory_partition + scratch_partitions + i, partition))
        .collect();

    Ok((
        resident,
        files,
        Duration::from_nanos(throttled_nanos.load(Ordering::Relaxed)),
    ))
}
