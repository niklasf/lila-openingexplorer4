use std::{
    fs::{self, File},
    io, mem,
    num::NonZeroUsize,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use clap::Parser;
use crossbeam::channel;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use lila_openingexplorer4::{
    packed::{GameId, pack_uint, unpack_uint},
    parquet::{FastGameColumns, GameColumns, GameRow, PositionalFile},
};
use parquet::file::reader::{FileReader, SerializedFileReader};
use rocksdb::{BlockBasedOptions, DBCompressionType, Options, SstFileWriter};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// One pass over the input for each disk partition. All memory partitions of
/// a disk partition, except the resident ones, must fit into scratch space at
/// the same time.
const DISK_PARTITION_BITS: u32 = 1;

/// Each memory partition must fit into memory at once. Up to three memory
/// partitions are in memory at the same time: one being written to SST files,
/// one being loaded and sorted, and one resident partition of the next pass.
const MEMORY_PARTITION_BITS: u32 = 6;

const _: () = assert!(MEMORY_PARTITION_BITS >= DISK_PARTITION_BITS);

const PASSES: usize = 1 << DISK_PARTITION_BITS;

const MEMORY_PARTITIONS_PER_PASS: usize = 1 << (MEMORY_PARTITION_BITS - DISK_PARTITION_BITS);

/// Memory partitions per pass that are kept in memory while reading the
/// input, rather than being written to scratch space and read back.
const RESIDENT_PARTITIONS_PER_PASS: usize = 1;

const _: () = assert!(RESIDENT_PARTITIONS_PER_PASS <= MEMORY_PARTITIONS_PER_PASS);

/// Memory partitions per pass that are written to scratch space.
const SCRATCH_PARTITIONS_PER_PASS: usize =
    MEMORY_PARTITIONS_PER_PASS - RESIDENT_PARTITIONS_PER_PASS;

/// Uncompressed bytes per chunk in scratch files.
const CHUNK_SIZE: usize = 4 << 20;

/// Hard limit for the size of each SST file.
const SST_MAX_SIZE: u64 = 512 * 1024 * 1024;

/// Start a new SST file once the current one reaches this size. The margin
/// leaves room for the index and footer written when finishing the file.
const SST_ROLL_SIZE: u64 = SST_MAX_SIZE - 16 * 1024 * 1024;

/// Initial guess for the ratio of SST bytes to uncompressed value bytes, used
/// until measured.
const INITIAL_SST_RATIO: f64 = 0.9;

/// Games that could not be represented as `LichessGame`.
static SKIPPED_GAMES: AtomicU64 = AtomicU64::new(0);

/// Bytes currently occupied by scratch files.
static SCRATCH_USAGE: AtomicU64 = AtomicU64::new(0);

#[derive(Parser)]
struct Args {
    parquet: PathBuf,
    /// Directory for temporary partition files.
    #[clap(long)]
    scratch: PathBuf,
    /// Maximum bytes to use in scratch space. The scatter of a pass is
    /// throttled while the previous pass is still freeing scratch space.
    #[clap(long, default_value = "690000000000")]
    scratch_budget: u64,
    /// Zstd level for scratch chunks (0 for LZ4).
    #[clap(long, default_value = "1")]
    scratch_zstd_level: i32,
    /// Use the generic parquet column readers instead of the fast decoder
    /// (for benchmarking).
    #[clap(long)]
    generic_reader: bool,
    /// Directory for the final SST files.
    #[clap(long)]
    out: PathBuf,
    /// Threads for reading the input. Defaults to the usable parallelism
    /// minus the write threads.
    #[clap(long)]
    read_threads: Option<usize>,
    /// Threads for compressing and writing scratch chunks.
    #[clap(long, default_value = "4")]
    write_threads: usize,
    /// Threads for loading scratch files.
    #[clap(long, default_value = "6")]
    load_threads: usize,
    /// Threads for writing SST files.
    #[clap(long, default_value = "6")]
    sst_threads: usize,
    /// Only process the first row groups of the input (for benchmarking).
    #[clap(long)]
    row_groups: Option<usize>,
    /// Stop after the scatter of the first pass (for benchmarking).
    #[clap(long)]
    scatter_only: bool,
    /// Block size of the SST files.
    #[clap(long, default_value = "4096")]
    sst_block_size: usize,
    /// Zstd compression level of the SST files (0 for the default).
    #[clap(long, default_value = "0")]
    sst_zstd_level: i32,
    /// Maximum size of the zstd dictionary trained for each SST file
    /// (0 to disable dictionary compression).
    #[clap(long, default_value = "0")]
    sst_zstd_dict: i32,
}

/// A memory partition, loaded in a compact form ready to be sorted.
///
/// Records are `game_id` (6 bytes), followed by the length-prefixed value.
/// The index points at the length prefix.
#[derive(Default)]
struct MemoryPartition {
    records: Vec<u8>,
    index: Vec<(GameId, u64)>,
}

impl MemoryPartition {
    fn push(&mut self, game_id: GameId, value: &[u8]) {
        self.records.extend_from_slice(&game_id.to_bytes());
        self.index.push((game_id, self.records.len() as u64));
        pack_uint(&mut self.records, value.len() as u64);
        self.records.extend_from_slice(value);
    }

    /// The packed value at the given offset in `records`.
    fn value_at(&self, offset: u64) -> &[u8] {
        let mut buf = &self.records[offset as usize..];
        let len = unpack_uint(&mut buf) as usize;
        &buf[..len]
    }

    /// Builds index entries for a range of records starting at `base`.
    fn index_records(records: &[u8], base: u64, index: &mut Vec<(GameId, u64)>) {
        let mut pos = 0;
        while pos < records.len() {
            let mut buf = &records[pos..];
            let game_id = GameId::unpack(&mut buf);
            let offset = pos + GameId::SIZE;
            let len = unpack_uint(&mut buf) as usize;
            let header = records.len() - pos - buf.len();
            index.push((game_id, base + offset as u64));
            pos += header + len;
        }
        assert_eq!(pos, records.len(), "records truncated");
    }

    /// Merges partitions collected by multiple threads.
    fn merge(parts: Vec<MemoryPartition>) -> MemoryPartition {
        let mut parts = parts.into_iter().filter(|p| !p.index.is_empty());
        let Some(mut merged) = parts.next() else {
            return MemoryPartition::default();
        };
        let parts: Vec<_> = parts.collect();
        merged
            .records
            .reserve_exact(parts.iter().map(|p| p.records.len()).sum());
        merged
            .index
            .reserve_exact(parts.iter().map(|p| p.index.len()).sum());
        for part in parts {
            let base = merged.records.len() as u64;
            merged.records.extend_from_slice(&part.records);
            merged
                .index
                .extend(part.index.into_iter().map(|(id, off)| (id, base + off)));
        }
        merged
    }
}

/// Appends a record to a scratch chunk buffer.
fn push_record(buf: &mut Vec<u8>, game_id: GameId, value: &[u8]) {
    buf.extend_from_slice(&game_id.to_bytes());
    pack_uint(buf, value.len() as u64);
    buf.extend_from_slice(value);
}

/// Compression of scratch chunks.
#[derive(Copy, Clone)]
enum ChunkCodec {
    Lz4,
    Zstd(i32),
}

impl ChunkCodec {
    fn from_level(zstd_level: i32) -> ChunkCodec {
        if zstd_level == 0 {
            ChunkCodec::Lz4
        } else {
            ChunkCodec::Zstd(zstd_level)
        }
    }
}

struct ChunkCompressor {
    codec: ChunkCodec,
    zstd: Option<zstd::bulk::Compressor<'static>>,
    buf: Vec<u8>,
}

impl ChunkCompressor {
    fn new(codec: ChunkCodec) -> ChunkCompressor {
        ChunkCompressor {
            codec,
            zstd: match codec {
                ChunkCodec::Lz4 => None,
                ChunkCodec::Zstd(level) => {
                    Some(zstd::bulk::Compressor::new(level).expect("zstd compressor"))
                }
            },
            buf: Vec::new(),
        }
    }

    fn compress(&mut self, raw: &[u8]) -> &[u8] {
        match self.codec {
            ChunkCodec::Lz4 => {
                self.buf
                    .resize(lz4_flex::block::get_maximum_output_size(raw.len()), 0);
                let n = lz4_flex::compress_into(raw, &mut self.buf).expect("compress");
                &self.buf[..n]
            }
            ChunkCodec::Zstd(_) => {
                self.buf.clear();
                self.buf.reserve(zstd::zstd_safe::compress_bound(raw.len()));
                self.zstd
                    .as_mut()
                    .expect("zstd")
                    .compress_to_buffer(raw, &mut self.buf)
                    .expect("compress");
                &self.buf
            }
        }
    }
}

struct ChunkDecompressor {
    codec: ChunkCodec,
    zstd: Option<zstd::bulk::Decompressor<'static>>,
}

impl ChunkDecompressor {
    fn new(codec: ChunkCodec) -> ChunkDecompressor {
        ChunkDecompressor {
            codec,
            zstd: match codec {
                ChunkCodec::Lz4 => None,
                ChunkCodec::Zstd(_) => {
                    Some(zstd::bulk::Decompressor::new().expect("zstd decompressor"))
                }
            },
        }
    }

    fn decompress(&mut self, compressed: &[u8], dst: &mut [u8]) {
        let n = match self.codec {
            ChunkCodec::Lz4 => lz4_flex::decompress_into(compressed, dst).expect("decompress"),
            ChunkCodec::Zstd(_) => self
                .zstd
                .as_mut()
                .expect("zstd")
                .decompress_to_buffer(compressed, dst)
                .expect("decompress"),
        };
        assert_eq!(n, dst.len(), "chunk length");
    }
}

#[derive(Copy, Clone)]
struct ChunkMeta {
    offset: u64,
    raw_len: u32,
    compressed_len: u32,
}

/// A scratch file holding LZ4 compressed chunks of records.
struct ScratchFile {
    path: PathBuf,
    file: File,
    codec: ChunkCodec,
    len: u64,
    chunks: Vec<ChunkMeta>,
}

impl ScratchFile {
    fn create(path: PathBuf, codec: ChunkCodec) -> io::Result<ScratchFile> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(ScratchFile {
            path,
            file,
            codec,
            len: 0,
            chunks: Vec::new(),
        })
    }

    fn append(&mut self, raw_len: usize, compressed: &[u8]) -> io::Result<()> {
        self.file.write_all_at(compressed, self.len)?;
        self.chunks.push(ChunkMeta {
            offset: self.len,
            raw_len: raw_len as u32,
            compressed_len: compressed.len() as u32,
        });
        self.len += compressed.len() as u64;
        SCRATCH_USAGE.fetch_add(compressed.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn raw_len(&self) -> usize {
        self.chunks.iter().map(|c| c.raw_len as usize).sum()
    }

    /// Loads and decompresses all chunks in parallel.
    fn load(&self, threads: usize) -> MemoryPartition {
        let mut records = vec![0u8; self.raw_len()];

        // Disjoint destination slices for each chunk.
        let mut slots: Vec<Option<(ChunkMeta, u64, &mut [u8])>> =
            Vec::with_capacity(self.chunks.len());
        let mut rest: &mut [u8] = &mut records;
        let mut base = 0u64;
        for chunk in &self.chunks {
            let (dst, tail) = rest.split_at_mut(chunk.raw_len as usize);
            rest = tail;
            slots.push(Some((*chunk, base, dst)));
            base += chunk.raw_len as u64;
        }
        let next = AtomicUsize::new(0);
        let slots = Mutex::new(slots);
        let index = Mutex::new(Vec::new());

        thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut decompressor = ChunkDecompressor::new(self.codec);
                    let mut compressed = Vec::new();
                    let mut local_index = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some((chunk, base, dst)) = slots
                            .lock()
                            .expect("lock slots")
                            .get_mut(i)
                            .and_then(Option::take)
                        else {
                            break;
                        };
                        compressed.resize(chunk.compressed_len as usize, 0);
                        self.file
                            .read_exact_at(&mut compressed, chunk.offset)
                            .expect("read chunk");
                        decompressor.decompress(&compressed, dst);
                        MemoryPartition::index_records(dst, base, &mut local_index);
                    }
                    index.lock().expect("lock index").append(&mut local_index);
                });
            }
        });

        MemoryPartition {
            records,
            index: index.into_inner().expect("index"),
        }
    }

    fn remove(self) -> io::Result<()> {
        drop(self.file);
        fs::remove_file(&self.path)?;
        SCRATCH_USAGE.fetch_sub(self.len, Ordering::Relaxed);
        Ok(())
    }
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

struct SstContext<'a> {
    opts: &'a Options,
    out: &'a Path,
    stats: Mutex<SstStats>,
}

#[derive(Default)]
struct SstStats {
    games: u64,
    files: u64,
    bytes: u64,
    raw_bytes: u64,
    largest_file: u64,
}

impl SstStats {
    fn add_file(&mut self, games: u64, bytes: u64, raw_bytes: u64) {
        self.games += games;
        self.files += 1;
        self.bytes += bytes;
        self.raw_bytes += raw_bytes;
        self.largest_file = self.largest_file.max(bytes);
    }

    fn ratio(&self) -> f64 {
        if self.raw_bytes == 0 {
            INITIAL_SST_RATIO
        } else {
            self.bytes as f64 / self.raw_bytes as f64
        }
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    // Leave one physical core (two hardware threads) idle.
    let parallelism = thread::available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .saturating_sub(2)
        .max(2);
    let read_threads = args
        .read_threads
        .unwrap_or(parallelism.saturating_sub(args.write_threads).max(1));

    let reader = SerializedFileReader::new(PositionalFile::open(&args.parquet)?)?;
    let num_row_groups = args.row_groups.map_or(reader.num_row_groups(), |limit| {
        limit.min(reader.num_row_groups())
    });
    let num_rows: u64 = (0..num_row_groups)
        .map(|i| reader.metadata().row_group(i).num_rows() as u64)
        .sum();

    let mut sst_opts = Options::default();
    sst_opts.set_compression_type(DBCompressionType::Zstd);
    if args.sst_zstd_level != 0 || args.sst_zstd_dict != 0 {
        // Window bits and strategy as rocksdb defaults, but note that the
        // default level is a sentinel value for the zstd default.
        let level = if args.sst_zstd_level != 0 {
            args.sst_zstd_level
        } else {
            32767
        };
        sst_opts.set_compression_options(-14, level, 0, args.sst_zstd_dict);
        if args.sst_zstd_dict != 0 {
            sst_opts.set_zstd_max_train_bytes(args.sst_zstd_dict * 100);
        }
    }
    let mut table_opts = BlockBasedOptions::default();
    table_opts.set_block_size(args.sst_block_size);
    sst_opts.set_block_based_table_factory(&table_opts);
    let sst = SstContext {
        opts: &sst_opts,
        out: &args.out,
        stats: Mutex::new(SstStats::default()),
    };

    let started = Instant::now();

    let progress = ProgressBar::with_draw_target(
        Some(num_rows * PASSES as u64),
        ProgressDrawTarget::stdout_with_hz(4),
    )
    .with_style(
        ProgressStyle::with_template(
            "{spinner} {prefix} {msg} {wide_bar} {human_pos}/{human_len} {per_sec} {eta:>7}",
        )
        .expect("progress template"),
    );

    thread::scope(|s| {
        let progress = &progress;
        let sst = &sst;
        let reader = &reader;
        let args = &args;

        // Loader: loads and sorts partitions in order, one ahead of the
        // writer.
        let (job_tx, job_rx) = channel::unbounded::<PartitionJob>();
        let (sorted_tx, sorted_rx) = channel::bounded::<(usize, MemoryPartition)>(0);
        s.spawn(move || {
            for job in job_rx {
                let memory_partition = job.memory_partition;
                let timer = Instant::now();
                let mut partition = match job.source {
                    PartitionSource::Resident(partition) => partition,
                    PartitionSource::Scratch(scratch) => {
                        let partition = scratch.load(args.load_threads);
                        scratch.remove().expect("remove scratch");
                        partition
                    }
                };
                let load = timer.elapsed();

                let timer = Instant::now();
                partition
                    .index
                    .sort_unstable_by_key(|&(game_id, _)| game_id);
                log(
                    progress,
                    format!(
                        "partition {memory_partition}: load {load:.1?}, sort {:.1?} ({} games, {} record bytes)",
                        timer.elapsed(),
                        partition.index.len(),
                        partition.records.len()
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
                write_partition(&partition, memory_partition, sst, args.sst_threads);
                let stats = sst.stats.lock().expect("lock stats");
                log(
                    progress,
                    format!(
                        "partition {memory_partition}: write {:.1?} ({} files so far, ratio {:.3})",
                        timer.elapsed(),
                        stats.files,
                        stats.ratio()
                    ),
                );
            }
        });

        for pass in 0..PASSES {
            progress.set_prefix(format!("pass {}/{}", pass + 1, PASSES));
            progress.set_message("scattering");
            let timer = Instant::now();
            let (resident, scratch, throttled) = scatter_pass(
                reader,
                num_row_groups,
                pass,
                &ScatterConfig {
                    scratch: &args.scratch,
                    codec: ChunkCodec::from_level(args.scratch_zstd_level),
                    // The first pass has nothing to wait for.
                    budget: (pass > 0).then_some(args.scratch_budget),
                    read_threads,
                    write_threads: args.write_threads,
                    generic_reader: args.generic_reader,
                },
                progress,
            )
            .expect("scatter");
            let scratch_bytes: u64 = scratch.iter().map(|f| f.len).sum();
            log(
                progress,
                format!(
                    "pass {pass}: scatter {:.1?} ({} GB scratch, throttled {:.1?})",
                    timer.elapsed(),
                    scratch_bytes >> 30,
                    throttled
                ),
            );
            if args.scatter_only {
                for file in scratch {
                    file.remove().expect("remove scratch");
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
                        memory_partition: pass * MEMORY_PARTITIONS_PER_PASS + memory_partition,
                        source: PartitionSource::Scratch(file),
                    })
                    .expect("send job");
            }
        }

        drop(job_tx);
    });

    progress.finish();

    let stats = sst.stats.into_inner().expect("stats");
    println!(
        "done in {:.1?}: {} games in {} sst files ({} bytes, largest {} bytes, ratio {:.3}), {} skipped games",
        started.elapsed(),
        stats.games,
        stats.files,
        stats.bytes,
        stats.largest_file,
        stats.ratio(),
        SKIPPED_GAMES.load(Ordering::Relaxed),
    );
    Ok(())
}

type Resident = Vec<(usize, MemoryPartition)>;

struct ScatterConfig<'a> {
    scratch: &'a Path,
    codec: ChunkCodec,
    /// Scratch budget to throttle against, if the previous pass is still
    /// freeing scratch space.
    budget: Option<u64>,
    read_threads: usize,
    write_threads: usize,
    generic_reader: bool,
}

/// Reads the entire input and distributes the games of the given disk
/// partition to its memory partitions. The last `RESIDENT_PARTITIONS_PER_PASS`
/// memory partitions are kept in memory, the others are written to scratch
/// files.
fn scatter_pass(
    reader: &SerializedFileReader<PositionalFile>,
    num_row_groups: usize,
    pass: usize,
    config: &ScatterConfig<'_>,
    progress: &ProgressBar,
) -> io::Result<(Resident, Vec<ScratchFile>, Duration)> {
    let first_memory_partition = pass * MEMORY_PARTITIONS_PER_PASS;

    let files: Vec<Mutex<ScratchFile>> = (0..SCRATCH_PARTITIONS_PER_PASS)
        .map(|i| {
            Ok::<_, io::Error>(Mutex::new(ScratchFile::create(
                config
                    .scratch
                    .join(format!("games_{:05}.chunks", first_memory_partition + i)),
                config.codec,
            )?))
        })
        .collect::<io::Result<_>>()?;

    let resident: Mutex<Vec<MemoryPartition>> = Mutex::new(Vec::new());
    let next_row_group = AtomicUsize::new(0);
    let throttled_nanos = AtomicU64::new(0);

    thread::scope(|s| {
        let files = &files;
        let resident = &resident;
        let next_row_group = &next_row_group;
        let throttled_nanos = &throttled_nanos;

        // Write threads compress full chunks and append them to scratch
        // files, throttled by the scratch budget.
        let (tx, rx) = channel::bounded::<(usize, Vec<u8>)>(config.write_threads * 2);

        for _ in 0..config.write_threads {
            let rx = rx.clone();
            s.spawn(move || {
                let mut compressor = ChunkCompressor::new(config.codec);
                for (file, raw) in rx {
                    let compressed = compressor.compress(&raw);
                    if let Some(budget) = config.budget {
                        let timer = Instant::now();
                        while SCRATCH_USAGE.load(Ordering::Relaxed) + compressed.len() as u64
                            > budget
                        {
                            thread::sleep(Duration::from_millis(20));
                        }
                        throttled_nanos
                            .fetch_add(timer.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    }
                    files[file]
                        .lock()
                        .expect("lock scratch file")
                        .append(raw.len(), compressed)
                        .expect("append chunk");
                }
            });
        }

        drop(rx);

        // Read threads pack the games of this pass and route them to
        // resident partitions or scratch chunks.
        for _ in 0..config.read_threads {
            let tx = tx.clone();
            s.spawn(move || {
                let mut local_resident: Vec<MemoryPartition> = (0..RESIDENT_PARTITIONS_PER_PASS)
                    .map(|_| MemoryPartition::default())
                    .collect();
                let mut chunks: Vec<Vec<u8>> = (0..SCRATCH_PARTITIONS_PER_PASS)
                    .map(|_| Vec::with_capacity(CHUNK_SIZE + 4096))
                    .collect();
                let mut generic = GameColumns::default();
                let mut fast = FastGameColumns::default();
                let mut value = Vec::new();
                let mut skipped = 0;

                let mut route = |row: GameRow<'_>,
                                 local_resident: &mut Vec<MemoryPartition>,
                                 chunks: &mut Vec<Vec<u8>>| {
                    let Some(j) = pass_memory_partition(row.game_id, pass) else {
                        return;
                    };
                    value.clear();
                    if row.pack_lichess_game(&mut value).is_err() {
                        skipped += 1;
                        return;
                    }
                    if j >= SCRATCH_PARTITIONS_PER_PASS {
                        local_resident[j - SCRATCH_PARTITIONS_PER_PASS].push(row.game_id, &value);
                    } else {
                        let chunk = &mut chunks[j];
                        push_record(chunk, row.game_id, &value);
                        if chunk.len() >= CHUNK_SIZE {
                            let full = mem::replace(chunk, Vec::with_capacity(CHUNK_SIZE + 4096));
                            tx.send((j, full)).expect("send chunk");
                        }
                    }
                };

                loop {
                    let i = next_row_group.fetch_add(1, Ordering::Relaxed);
                    if i >= num_row_groups {
                        break;
                    }
                    let row_group = reader.get_row_group(i).expect("row group");
                    let rows = if config.generic_reader {
                        generic.read_columns(&*row_group).expect("read columns");
                        for row in generic.rows() {
                            route(row, &mut local_resident, &mut chunks);
                        }
                        generic.len()
                    } else {
                        fast.read_columns(&*row_group).expect("read columns");
                        for row in fast.rows() {
                            route(row, &mut local_resident, &mut chunks);
                        }
                        fast.len()
                    };

                    progress.inc(rows as u64);
                }

                for (j, chunk) in chunks.into_iter().enumerate() {
                    if !chunk.is_empty() {
                        tx.send((j, chunk)).expect("send chunk");
                    }
                }

                SKIPPED_GAMES.fetch_add(skipped, Ordering::Relaxed);
                resident
                    .lock()
                    .expect("lock resident")
                    .append(&mut local_resident);
            });
        }

        // Write threads finish once all chunks are written.
        drop(tx);
    });

    let files: Vec<ScratchFile> = files
        .into_iter()
        .map(|file| file.into_inner().expect("scratch file"))
        .collect();

    // Merge per-thread resident partitions.
    let mut per_thread = resident.into_inner().expect("resident");
    let mut merged: Vec<Vec<MemoryPartition>> = (0..RESIDENT_PARTITIONS_PER_PASS)
        .map(|_| Vec::new())
        .collect();
    while !per_thread.is_empty() {
        for (i, partition) in per_thread.drain(..RESIDENT_PARTITIONS_PER_PASS).enumerate() {
            merged[i].push(partition);
        }
    }
    let resident = merged
        .into_iter()
        .enumerate()
        .map(|(i, parts)| {
            (
                first_memory_partition + SCRATCH_PARTITIONS_PER_PASS + i,
                MemoryPartition::merge(parts),
            )
        })
        .collect();

    Ok((
        resident,
        files,
        Duration::from_nanos(throttled_nanos.load(Ordering::Relaxed)),
    ))
}

/// Index of the memory partition within the given pass, or `None` if the
/// game belongs to a different pass.
fn pass_memory_partition(game_id: GameId, pass: usize) -> Option<usize> {
    let memory_partition = game_id.partition(MEMORY_PARTITION_BITS);
    let disk_partition = memory_partition >> (MEMORY_PARTITION_BITS - DISK_PARTITION_BITS);
    (disk_partition == pass).then_some(memory_partition & (MEMORY_PARTITIONS_PER_PASS - 1))
}

/// Writes a sorted partition to non-overlapping SST files.
///
/// The partition is split into contiguous ranges sized to produce about one
/// full SST file each. Ranges are written in parallel. File names sort in key
/// order.
fn write_partition(
    partition: &MemoryPartition,
    memory_partition: usize,
    sst: &SstContext<'_>,
    threads: usize,
) {
    if partition.index.is_empty() {
        return;
    }

    let ratio = sst.stats.lock().expect("lock stats").ratio();
    let raw_per_range = (SST_ROLL_SIZE as f64 * 0.97 / ratio) as usize;
    let raw_per_game = partition.records.len() / partition.index.len();
    let games_per_range = (raw_per_range / raw_per_game.max(1))
        .min(partition.index.len().div_ceil(threads))
        .max(1);
    let ranges: Vec<&[(GameId, u64)]> = partition.index.chunks(games_per_range).collect();
    let next_range = AtomicUsize::new(0);

    thread::scope(|s| {
        for _ in 0..threads.min(ranges.len()) {
            s.spawn(|| {
                loop {
                    let range = next_range.fetch_add(1, Ordering::Relaxed);
                    let Some(index) = ranges.get(range) else {
                        break;
                    };
                    write_sst_range(
                        partition,
                        index,
                        sst,
                        &format!("games_{memory_partition:03}_{range:05}"),
                    )
                    .expect("write sst files");
                }
            });
        }
    });
}

/// Writes a sorted range of the partition to SST files of bounded size.
fn write_sst_range(
    partition: &MemoryPartition,
    index: &[(GameId, u64)],
    sst: &SstContext<'_>,
    prefix: &str,
) -> Result<(), rocksdb::Error> {
    let mut seq = 0;
    let mut writer: Option<SstFileWriter<'_>> = None;
    let mut games_in_file = 0;
    let mut raw_in_file = 0;

    let finish = |writer: &mut Option<SstFileWriter<'_>>, games: &mut u64, raw: &mut u64| {
        if let Some(mut w) = writer.take() {
            w.finish()?;
            let bytes = w.file_size();
            assert!(bytes <= SST_MAX_SIZE, "sst file too large: {bytes}");
            sst.stats
                .lock()
                .expect("lock stats")
                .add_file(*games, bytes, *raw);
            *games = 0;
            *raw = 0;
        }
        Ok::<_, rocksdb::Error>(())
    };

    for &(game_id, offset) in index {
        let w = match &mut writer {
            Some(w) => w,
            None => {
                let w = SstFileWriter::create(sst.opts);
                w.open(sst.out.join(format!("{prefix}_{seq}.sst")))?;
                seq += 1;
                writer.insert(w)
            }
        };

        let value = partition.value_at(offset);
        w.put(game_id.to_bytes(), value)?;
        games_in_file += 1;
        raw_in_file += (GameId::SIZE + value.len()) as u64;

        if w.file_size() >= SST_ROLL_SIZE {
            finish(&mut writer, &mut games_in_file, &mut raw_in_file)?;
        }
    }

    finish(&mut writer, &mut games_in_file, &mut raw_in_file)
}

fn log(progress: &ProgressBar, msg: String) {
    // Must not touch the progress bar inside `suspend` (its state is locked).
    let elapsed = progress.elapsed();
    progress.suspend(|| eprintln!("[{elapsed:>8.1?}] {msg}"));
}
