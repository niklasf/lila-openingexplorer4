mod hot;
mod partition;
mod replay;
mod write;

use std::{
    fmt, fs, io, mem,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crossbeam::channel;
use indicatif::{MultiProgress, ProgressBar};
pub use write::{COLD_DIR, OutputStats, POSITIONS_DIR};

use crate::{
    intermediate::{GlobalGameSeq, IntermediateEntry},
    packed::Zobrist96,
    parquet::{Input, PrefetchTimes, Prefetcher},
    parquet_to_positions::{
        hot::{HOT_BATCH, HOT_KEY_MEMORY, HotBuffer, HotMap, HotSet, Sample},
        partition::{Partition, memory_partition},
        replay::{GameInfo, Replayer},
        write::{HotEntries, SstContext, write_partition},
    },
    scatter::{
        ChunkWriterConfig, ChunkWriterTimes, Layout, RESIDENT_PARTITIONS_PER_PASS, ScratchFile,
        spawn_chunk_writers,
    },
    sst::SstConfig,
    util::{Gauge, log_above, progress_bar, timed, timed_seconds},
};

/// Partitions in memory at the same time: one being written to SST files,
/// one being loaded and sorted, and the resident partition of the next pass,
/// which is temporarily held twice while it is distributed to buckets.
const PARTITIONS_IN_MEMORY: u64 = 4;

/// Assumed ratio of compressed scratch chunks to record bytes (measured
/// about 0.59 with zstd level 1).
const SCRATCH_RATIO: f64 = 0.7;

/// Fixed memory overhead for the parquet reader, rocksdb and allocator slack.
const BASE_MEMORY: u64 = 4 << 30;

/// A position is hot if it is estimated to have at least this fraction of
/// the records of a memory partition.
const HOT_FRACTION: u64 = 32;

/// Records per scratch chunk.
const CHUNK_RECORDS: usize = 32 * 1024;

const CHUNK_BYTES: usize = CHUNK_RECORDS * IntermediateEntry::SIZE;

fn new_chunk() -> Vec<u8> {
    Vec::with_capacity(CHUNK_BYTES)
}

/// Estimated data sizes, from replaying a sample of row groups.
struct Estimate {
    rows: u64,
    /// Records of all games, excluding records of hot positions.
    records: u64,
    /// Hot positions and their estimated records.
    hot: Vec<(Zobrist96, u64)>,
    months: u64,
}

impl Estimate {
    fn new(input: &Input, config: &Config, read_threads: u64) -> Estimate {
        let rows = input.total_rows();
        let sample = Sample::replay(input, config.threads);
        let per_row = if sample.sampled_rows == 0 {
            0.0
        } else {
            sample.records as f64 / sample.sampled_rows as f64
        };
        let mut estimate = Estimate {
            rows,
            // With a safety margin for sampling error.
            records: (per_row * rows as f64 * 1.05) as u64,
            hot: Vec::new(),
            months: sample.months,
        };
        // Choose hot positions for the layout without them. Then the
        // remaining records may fit into fewer partitions.
        let layout = estimate.layout(config, read_threads);
        let threshold = estimate.records / layout.memory_partitions() as u64 / HOT_FRACTION;
        estimate.hot = sample.hot(rows, threshold.max(1));
        let hot_records: u64 = estimate.hot.iter().map(|(_, records)| records).sum();
        estimate.records = estimate.records.saturating_sub(hot_records);
        estimate
    }

    fn record_bytes(&self) -> u64 {
        self.records * IntermediateEntry::SIZE as u64
    }

    fn hot_records(&self) -> u64 {
        self.hot.iter().map(|(_, records)| records).sum()
    }

    /// Bytes of one memory partition in memory, including bucket slack.
    fn partition_memory(&self, layout: Layout) -> u64 {
        self.record_bytes() * 17 / 16 / layout.memory_partitions() as u64
    }

    fn hot_memory(&self, read_threads: u64) -> u64 {
        self.hot.len() as u64 * self.months.max(1) * HOT_KEY_MEMORY
            + read_threads * (HOT_BATCH * IntermediateEntry::SIZE) as u64
    }

    /// Peak memory for the given layout and read threads.
    fn memory(&self, layout: Layout, read_threads: u64) -> u64 {
        let buffers =
            read_threads * layout.scratch_partitions_per_pass() as u64 * CHUNK_BYTES as u64;
        BASE_MEMORY
            + PARTITIONS_IN_MEMORY * self.partition_memory(layout)
            + buffers
            + self.hot_memory(read_threads)
    }

    /// Scratch space needed by one pass.
    fn scratch(&self, layout: Layout) -> u64 {
        let per_partition =
            self.record_bytes() as f64 * SCRATCH_RATIO / layout.memory_partitions() as f64;
        (per_partition * layout.scratch_partitions_per_pass() as f64) as u64
    }

    fn layout(&self, config: &Config, read_threads: u64) -> Layout {
        Layout::choose(|layout| {
            (
                self.memory(layout, read_threads) <= config.memory_budget,
                self.scratch(layout) <= config.scratch_budget,
            )
        })
    }
}

/// Counters shared by the threads of one run.
#[derive(Default)]
struct Counters {
    /// Valid games (counted in the first pass).
    games: AtomicU64,
    /// Games that could not be indexed (counted in the first pass).
    skipped_games: AtomicU64,
    /// Plies that repeated a position of the same game (counted in the first
    /// pass).
    repetitions: AtomicU64,
    /// Records routed to memory partitions.
    records: AtomicU64,
    /// Records of hot positions.
    hot_records: AtomicU64,
    /// Bytes currently occupied by scratch files.
    scratch_usage: Gauge,
    /// Partitions written to SST files so far.
    written_partitions: Gauge,
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
    /// Threads for replaying the input.
    pub read_threads: Option<usize>,
    /// Threads for compressing and writing scratch chunks.
    pub write_threads: Option<usize>,
    /// Threads for loading and sorting partitions.
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
    /// Options of the SST files.
    pub sst: SstConfig,
    /// Start a new SST file once the current one reaches this size.
    pub sst_target_size: u64,
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub games: u64,
    /// Games that could not be indexed.
    pub skipped: u64,
    /// Plies that repeated a position of the same game.
    pub repetitions: u64,
    /// Records of all games, including hot records.
    pub records: u64,
    pub hot_positions: usize,
    pub hot_records: u64,
    pub output: OutputStats,
}

/// The input, its estimated size, and the layout and threads chosen to
/// convert it.
pub struct Plan {
    input: Input,
    estimate: Estimate,
    layout: Layout,
    read_threads: usize,
    write_threads: usize,
    load_threads: usize,
    sst_threads: usize,
    /// Threads for loading and writing SST files once the input has been
    /// scanned for the last time.
    final_load_threads: usize,
    final_sst_threads: usize,
}

impl Plan {
    pub fn new(parquet: &[PathBuf], config: &Config) -> io::Result<Plan> {
        let threads = config.threads;
        let input = Input::open(parquet, config.row_groups, config.direct_io)?;

        // The layout depends on the read threads (buffers), which depend on
        // the layout (overlap). Resolve with the larger read thread count.
        let max_read_threads = config.read_threads.unwrap_or(threads * 2 / 3).max(1);
        let estimate = Estimate::new(&input, config, max_read_threads as u64);
        let layout = estimate.layout(config, max_read_threads as u64);
        // With a single pass the scan and the partition writes never overlap,
        // so each phase can use all threads. With several passes the scan of
        // the next pass runs while partitions are written.
        // Replaying is about 2.5 times the work of compressing chunks, and
        // loading and sorting about 2/3 of the work of writing SST files.
        let single_pass = layout.passes() == 1 || !config.overlap;
        let (final_load_default, final_sst_default) = (threads * 2 / 5, threads * 3 / 5);
        let (read_default, write_default, load_default, sst_default) = if single_pass {
            (
                threads * 2 / 3,
                threads / 3,
                final_load_default,
                final_sst_default,
            )
        } else {
            (threads / 2, threads / 5, threads / 5, threads / 4)
        };

        Ok(Plan {
            input,
            estimate,
            layout,
            read_threads: config.read_threads.unwrap_or(read_default).max(1),
            write_threads: config.write_threads.unwrap_or(write_default).max(1),
            load_threads: config.load_threads.unwrap_or(load_default).max(1),
            sst_threads: config.sst_threads.unwrap_or(sst_default).max(1),
            final_load_threads: config.load_threads.unwrap_or(final_load_default).max(1),
            final_sst_threads: config.sst_threads.unwrap_or(final_sst_default).max(1),
        })
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} games, about {} records ({} GB) and {} records of {} hot positions; {} passes with {} memory partitions each, about {} GB scratch per pass and {} GB memory; threads: {} read, {} write, {} load ({} after the last scan), {} sst ({} after the last scan)",
            self.estimate.rows,
            self.estimate.records,
            self.estimate.record_bytes() >> 30,
            self.estimate.hot_records(),
            self.estimate.hot.len(),
            self.layout.passes(),
            self.layout.memory_partitions_per_pass(),
            self.estimate.scratch(self.layout) >> 30,
            self.estimate.memory(self.layout, self.read_threads as u64) >> 30,
            self.read_threads,
            self.write_threads,
            self.load_threads,
            self.final_load_threads,
            self.sst_threads,
            self.final_sst_threads,
        )
    }
}

/// Source of the records of a memory partition.
enum PartitionSource {
    /// Collected in memory by the scan threads.
    Resident(Vec<Vec<IntermediateEntry>>),
    Scratch(ScratchFile),
}

struct PartitionJob {
    memory_partition: usize,
    source: PartitionSource,
    hot: HotEntries,
}

/// Creates an empty output directory.
fn create_out_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", dir.display())))?;
    if fs::read_dir(dir)?.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{}: not empty", dir.display()),
        ));
    }
    Ok(())
}

/// Indexes the positions of the planned input into SST files in
/// `out/cold_positions` and `out/positions`, using `scratch_dir` for
/// temporary partition files.
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
        layout,
        read_threads,
        write_threads,
        load_threads,
        sst_threads,
        final_load_threads,
        final_sst_threads,
    } = *plan;

    // Fail early rather than after the first scatter pass.
    create_out_dir(&out.join(COLD_DIR))?;
    create_out_dir(&out.join(POSITIONS_DIR))?;

    let hot_set = HotSet::new(estimate.hot.iter().map(|(hash, _)| *hash));
    let (sst_opts, sst_env_opts) = config.sst.options();
    let sst = SstContext {
        opts: &sst_opts,
        env_opts: &sst_env_opts,
        out,
        target_size: config.sst_target_size,
        stats: Mutex::new(OutputStats::default()),
    };
    let counters = Counters::default();
    // Set once the input has been scanned for the last time, so that loading
    // and writing can use all threads.
    let scanned = AtomicBool::new(false);

    let progress = progress.add(progress_bar(estimate.rows * layout.passes() as u64));

    thread::scope(|s| {
        let progress = &progress;
        let sst = &sst;
        let counters = &counters;
        let hot_set = &hot_set;
        let scanned = &scanned;

        // Loader: loads and sorts partitions in order, one ahead of the
        // writer.
        let (job_tx, job_rx) = channel::unbounded::<PartitionJob>();
        let (sorted_tx, sorted_rx) = channel::bounded::<(usize, Partition, HotEntries)>(0);
        s.spawn(move || {
            for job in job_rx {
                let memory_partition = job.memory_partition;
                let load_threads = if scanned.load(Ordering::Relaxed) {
                    final_load_threads
                } else {
                    load_threads
                };
                let timer = Instant::now();
                let mut partition = match job.source {
                    PartitionSource::Resident(parts) => {
                        partition::from_parts(parts, layout.memory_bits, load_threads)
                    }
                    PartitionSource::Scratch(scratch) => {
                        let partition =
                            partition::load(&scratch, layout.memory_bits, load_threads);
                        scratch
                            .remove(&counters.scratch_usage)
                            .expect("remove scratch");
                        partition
                    }
                };
                let load = timer.elapsed();

                let timer = Instant::now();
                partition.sort(load_threads);
                log_above(
                    progress,
                    format!(
                        "partition {memory_partition}: load {load:.1?}, sort {:.1?} ({} records, {} hot entries)",
                        timer.elapsed(),
                        partition.records(),
                        job.hot.len(),
                    ),
                );
                sorted_tx
                    .send((memory_partition, partition, job.hot))
                    .expect("send sorted");
            }
        });

        // Writer: writes sorted partitions to SST files.
        s.spawn(move || {
            for (memory_partition, partition, hot) in sorted_rx {
                let sst_threads = if scanned.load(Ordering::Relaxed) {
                    final_sst_threads
                } else {
                    sst_threads
                };
                let timer = Instant::now();
                write_partition(
                    &partition,
                    &hot,
                    memory_partition,
                    layout.memory_bits,
                    sst,
                    sst_threads,
                );
                drop(partition);
                counters.written_partitions.add(1);
                let stats = sst.stats.lock().expect("lock stats");
                log_above(
                    progress,
                    format!(
                        "partition {memory_partition}: write {:.1?} ({} cold files with {:.1} B/key, {} positions files with {:.1} B/key so far)",
                        timer.elapsed(),
                        stats.cold.files,
                        stats.cold.bytes_per_key(),
                        stats.positions.files,
                        stats.positions.bytes_per_key(),
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
                log_above(
                    progress,
                    format!("pass {pass}: waited {waited:.1?} for previous pass"),
                );
            }
            progress.set_message("scattering");
            let timer = Instant::now();
            let scattered = scatter_pass(
                input,
                pass,
                &ScatterConfig {
                    scratch: scratch_dir,
                    zstd_level: config.scratch_zstd_level,
                    // The first pass has nothing to wait for.
                    budget: (pass > 0).then_some(config.scratch_budget),
                    layout,
                    read_threads,
                    write_threads,
                    io_depth: config.io_depth.max(1),
                    direct: config.direct_io,
                    counters,
                    hot_set,
                },
                progress,
            )
            .expect("scatter");
            if pass + 1 == layout.passes() || config.scatter_only {
                scanned.store(true, Ordering::Relaxed);
            }
            let scratch_bytes: u64 = scattered.scratch.iter().map(ScratchFile::len).sum();
            let scratch_raw: u64 = scattered.scratch.iter().map(ScratchFile::raw_len).sum();
            log_above(
                progress,
                format!(
                    "pass {pass}: scatter {:.1?} ({} MB scratch, ratio {:.3}, throttled {:.1?}, {} hot entries)",
                    timer.elapsed(),
                    scratch_bytes >> 20,
                    scratch_bytes as f64 / scratch_raw.max(1) as f64,
                    scattered.throttled,
                    scattered.hot.len(),
                ),
            );
            if config.scatter_only {
                for file in scattered.scratch {
                    file.remove(&counters.scratch_usage)
                        .expect("remove scratch");
                }
                break;
            }

            // Hot entries of each memory partition of this pass.
            let first_memory_partition = pass * layout.memory_partitions_per_pass();
            let mut hot_by_partition: Vec<HotEntries> = (0..layout.memory_partitions_per_pass())
                .map(|_| Vec::new())
                .collect();
            for (key, value) in scattered.hot {
                let entry_partition = memory_partition(&key_entry(&key), layout.memory_bits);
                hot_by_partition[entry_partition - first_memory_partition].push((key, value));
            }

            // Process the resident partitions first, to free their memory.
            let scratch_partitions = layout.scratch_partitions_per_pass();
            for (i, parts) in scattered.resident.into_iter().enumerate() {
                let j = scratch_partitions + i;
                job_tx
                    .send(PartitionJob {
                        memory_partition: first_memory_partition + j,
                        source: PartitionSource::Resident(parts),
                        hot: mem::take(&mut hot_by_partition[j]),
                    })
                    .expect("send job");
            }
            for (j, file) in scattered.scratch.into_iter().enumerate() {
                job_tx
                    .send(PartitionJob {
                        memory_partition: first_memory_partition + j,
                        source: PartitionSource::Scratch(file),
                        hot: mem::take(&mut hot_by_partition[j]),
                    })
                    .expect("send job");
            }
        }

        drop(job_tx);
    });

    progress.finish();

    let records = counters.records.load(Ordering::Relaxed);
    let hot_records = counters.hot_records.load(Ordering::Relaxed);
    Ok(Stats {
        games: counters.games.load(Ordering::Relaxed),
        skipped: counters.skipped_games.load(Ordering::Relaxed),
        repetitions: counters.repetitions.load(Ordering::Relaxed),
        records: records + hot_records,
        hot_positions: hot_set.len(),
        hot_records,
        output: sst.stats.into_inner().expect("stats"),
    })
}

/// An entry with the given key, for computing its partition.
fn key_entry(key: &[u8; IntermediateEntry::KEY_SIZE]) -> IntermediateEntry {
    let mut bytes = [0; IntermediateEntry::SIZE];
    bytes[..IntermediateEntry::KEY_SIZE].copy_from_slice(key);
    IntermediateEntry(bytes)
}

struct ScatterConfig<'a> {
    scratch: &'a Path,
    zstd_level: i32,
    /// Scratch budget to throttle against, if the previous pass is still
    /// freeing scratch space.
    budget: Option<u64>,
    layout: Layout,
    read_threads: usize,
    write_threads: usize,
    io_depth: usize,
    direct: bool,
    counters: &'a Counters,
    hot_set: &'a HotSet,
}

/// Accumulated time of each scatter stage, summed over its threads.
#[derive(Default)]
struct ScatterTimes {
    prefetch: PrefetchTimes,
    recv: AtomicU64,
    decode: AtomicU64,
    replay: AtomicU64,
    send: AtomicU64,
    writer: ChunkWriterTimes,
}

impl ScatterTimes {
    fn summary(&self) -> String {
        format!(
            "prefetch: fetch {:.0}s, send {:.0}s; read: recv {:.0}s, decode {:.0}s, replay {:.0}s, send {:.0}s; compress: {}",
            timed_seconds(&self.prefetch.fetch),
            timed_seconds(&self.prefetch.send),
            timed_seconds(&self.recv),
            timed_seconds(&self.decode),
            timed_seconds(&self.replay),
            timed_seconds(&self.send),
            self.writer.summary(),
        )
    }
}

struct Scattered {
    /// Parts of the resident partitions collected by each thread.
    resident: Vec<Vec<Vec<IntermediateEntry>>>,
    scratch: Vec<ScratchFile>,
    /// Packed entries of the hot positions of this pass, sorted by key.
    hot: HotEntries,
    throttled: Duration,
}

/// Replays the entire input and distributes the records of the given disk
/// partition to its memory partitions. The last `RESIDENT_PARTITIONS_PER_PASS`
/// memory partitions are kept in memory, the others are written to scratch
/// files. Records of hot positions are aggregated in memory.
fn scatter_pass(
    input: &Input,
    pass: usize,
    config: &ScatterConfig<'_>,
    progress: &ProgressBar,
) -> io::Result<Scattered> {
    let layout = config.layout;
    let scratch_partitions = layout.scratch_partitions_per_pass();
    let first_memory_partition = pass * layout.memory_partitions_per_pass();
    let counters = config.counters;

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

    let resident: Mutex<Vec<Vec<Vec<IntermediateEntry>>>> = Mutex::new(Vec::new());
    let hot_map = HotMap::default();
    let times = ScatterTimes::default();
    let writer_config = ChunkWriterConfig {
        threads: config.write_threads,
        zstd_level: config.zstd_level,
        budget: config.budget,
        usage: &counters.scratch_usage,
    };

    thread::scope(|s| {
        let files = &files;
        let resident = &resident;
        let hot_map = &hot_map;
        let times = &times;
        let writer_config = &writer_config;
        let prefetcher = Prefetcher::spawn(input, config.io_depth, &times.prefetch, s);

        // Write threads compress full chunks and append them to scratch
        // files, throttled by the scratch budget.
        let (tx, rx) = channel::bounded::<(usize, Vec<u8>)>(config.write_threads * 2);
        spawn_chunk_writers(s, writer_config, files, rx, &times.writer);

        // Read threads replay the games and route the records of this pass
        // to hot positions, resident partitions or scratch chunks.
        for _ in 0..config.read_threads {
            let tx = tx.clone();
            let prefetcher = prefetcher.clone();
            s.spawn(move || {
                let mut replayer = Replayer::default();
                let mut hot = HotBuffer::new(hot_map);
                let mut local_resident: Vec<Vec<IntermediateEntry>> = (0
                    ..RESIDENT_PARTITIONS_PER_PASS)
                    .map(|_| Vec::new())
                    .collect();
                let mut chunks: Vec<Vec<u8>> =
                    (0..scratch_partitions).map(|_| new_chunk()).collect();
                let (mut games, mut skipped, mut repetitions) = (0, 0, 0);
                let (mut records, mut hot_records) = (0, 0);

                while let Some(fetched) = timed(&times.recv, || prefetcher.recv()) {
                    let first_row = input.first_row(fetched.0);
                    let batch = timed(&times.decode, || prefetcher.decode(fetched))
                        .expect("read row group");
                    timed(&times.replay, || {
                        for (k, row) in batch.rows().enumerate() {
                            let seq = GlobalGameSeq::new(first_row + k as u64).expect("seq");
                            let Ok(info) = GameInfo::from_row(&row, seq) else {
                                skipped += 1;
                                continue;
                            };
                            let Ok(pos) = row.initial_position() else {
                                skipped += 1;
                                continue;
                            };
                            let variant = pos.variant();
                            let Ok(replayed) = replayer.replay(pos, row.moves) else {
                                skipped += 1;
                                continue;
                            };
                            games += 1;
                            repetitions += replayed.repetitions as u64;

                            for entry in replayer.entries(variant, &info) {
                                let Some(j) = layout.pass_memory_partition(
                                    memory_partition(&entry, layout.memory_bits),
                                    pass,
                                ) else {
                                    continue;
                                };
                                if config.hot_set.contains(&entry) {
                                    hot.push(entry);
                                    hot_records += 1;
                                } else if j >= scratch_partitions {
                                    local_resident[j - scratch_partitions].push(entry);
                                    records += 1;
                                } else {
                                    let chunk = &mut chunks[j];
                                    chunk.extend_from_slice(&entry.0);
                                    records += 1;
                                    if chunk.len() >= CHUNK_BYTES {
                                        let full = mem::replace(chunk, new_chunk());
                                        timed(&times.send, || tx.send((j, full)))
                                            .expect("send chunk");
                                    }
                                }
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

                if pass == 0 {
                    counters.games.fetch_add(games, Ordering::Relaxed);
                    counters.skipped_games.fetch_add(skipped, Ordering::Relaxed);
                    counters
                        .repetitions
                        .fetch_add(repetitions, Ordering::Relaxed);
                }
                counters.records.fetch_add(records, Ordering::Relaxed);
                counters
                    .hot_records
                    .fetch_add(hot_records, Ordering::Relaxed);
                resident.lock().expect("lock resident").push(local_resident);
            });
        }

        // Write threads finish once all chunks are written.
        drop(tx);
    });

    log_above(progress, format!("pass {pass}: {}", times.summary()));

    let files: Vec<ScratchFile> = files
        .into_iter()
        .map(|file| file.into_inner().expect("scratch file"))
        .collect();

    // Group the per-thread parts by resident partition.
    let mut merged: Vec<Vec<Vec<IntermediateEntry>>> = (0..RESIDENT_PARTITIONS_PER_PASS)
        .map(|_| Vec::new())
        .collect();
    for local in resident.into_inner().expect("resident") {
        for (i, part) in local.into_iter().enumerate() {
            merged[i].push(part);
        }
    }

    Ok(Scattered {
        resident: merged,
        scratch: files,
        hot: hot_map.pack(),
        throttled: Duration::from_nanos(times.writer.throttled.load(Ordering::Relaxed)),
    })
}
