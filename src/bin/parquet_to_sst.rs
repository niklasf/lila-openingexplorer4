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
    packed::GameId,
    parquet::{GameReader, GameRow, PositionalFile},
};
use rocksdb::{BlockBasedOptions, DBCompressionType, Options, SstFileWriter};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// Memory partitions per pass that are kept in memory while reading the
/// input, rather than being written to scratch space and read back.
const RESIDENT_PARTITIONS_PER_PASS: usize = 1;

/// Partitions in memory at the same time: one being written to SST files,
/// one being loaded and sorted, and one resident partition of the next pass.
const PARTITIONS_IN_MEMORY: u64 = 3;

/// Assumed ratio of compressed scratch chunks to record bytes (measured
/// about 0.79 with zstd level 1 and about 0.95 with LZ4).
const SCRATCH_RATIO: f64 = 1.0;

/// Fixed memory overhead for the parquet reader, rocksdb and allocator slack.
const BASE_MEMORY: u64 = 4 << 30;

/// How the key space is split: `1 << disk_bits` passes over the input, each
/// covering `1 << (memory_bits - disk_bits)` memory partitions.
#[derive(Debug, Copy, Clone)]
struct Layout {
    memory_bits: u32,
    disk_bits: u32,
}

impl Layout {
    fn passes(self) -> usize {
        1 << self.disk_bits
    }

    fn memory_partitions(self) -> usize {
        1 << self.memory_bits
    }

    fn memory_partitions_per_pass(self) -> usize {
        1 << (self.memory_bits - self.disk_bits)
    }

    fn scratch_partitions_per_pass(self) -> usize {
        self.memory_partitions_per_pass() - RESIDENT_PARTITIONS_PER_PASS
    }

    /// Index of the memory partition within the given pass, or `None` if the
    /// game belongs to a different pass.
    fn pass_memory_partition(self, game_id: GameId, pass: usize) -> Option<usize> {
        let memory_partition = game_id.partition(self.memory_bits);
        let disk_partition = memory_partition >> (self.memory_bits - self.disk_bits);
        (disk_partition == pass)
            .then_some(memory_partition & (self.memory_partitions_per_pass() - 1))
    }
}

/// Estimated data sizes, from a sample of row groups.
struct Estimate {
    rows: u64,
    /// Record bytes (game id, length prefix and value) of all games.
    record_bytes: u64,
}

impl Estimate {
    fn sample(reader: &GameReader<PositionalFile>, num_row_groups: usize) -> Estimate {
        let rows: u64 = (0..num_row_groups)
            .map(|i| reader.metadata().row_group(i).num_rows() as u64)
            .sum();
        let samples = 256.min(num_row_groups);
        let mut value = Vec::new();
        let (mut sampled_rows, mut sampled_bytes) = (0u64, 0u64);
        for k in 0..samples {
            let i = k * num_row_groups / samples;
            let batch = reader.read_games(i).expect("read row group");
            for row in batch.rows() {
                value.clear();
                sampled_rows += 1;
                if row.pack_lichess_game(&mut value).is_ok() {
                    sampled_bytes += (GameId::SIZE + LEN_PREFIX + value.len()) as u64;
                }
            }
        }
        let per_row = if sampled_rows == 0 {
            0.0
        } else {
            sampled_bytes as f64 / sampled_rows as f64
        };
        Estimate {
            rows,
            // With a safety margin for sampling error.
            record_bytes: (per_row * rows as f64 * 1.05) as u64,
        }
    }

    /// Bytes of one memory partition in memory, including its index and
    /// chunk slack.
    fn partition_memory(&self, layout: Layout) -> u64 {
        (self.record_bytes + 16 * self.rows) * 21 / 20 / layout.memory_partitions() as u64
    }

    /// Peak memory for the given layout and read threads.
    fn memory(&self, layout: Layout, read_threads: u64) -> u64 {
        let buffers = read_threads * layout.scratch_partitions_per_pass() as u64 * CHUNK_STRIDE;
        BASE_MEMORY + PARTITIONS_IN_MEMORY * self.partition_memory(layout) + buffers
    }

    /// Scratch space needed by one pass.
    fn scratch(&self, layout: Layout) -> u64 {
        let per_partition =
            self.record_bytes as f64 * SCRATCH_RATIO / layout.memory_partitions() as f64;
        (per_partition * layout.scratch_partitions_per_pass() as f64) as u64
    }

    /// Picks the fewest passes that fit the scratch budget, and the fewest
    /// memory partitions that fit the memory budget.
    fn layout(
        &self,
        memory_budget: u64,
        scratch_budget: u64,
        read_threads: u64,
        passes: Option<usize>,
        memory_partitions: Option<usize>,
    ) -> Layout {
        let mut layout = Layout {
            memory_bits: 0,
            disk_bits: 0,
        };
        if let Some(passes) = passes {
            layout.disk_bits = passes.max(1).next_power_of_two().trailing_zeros();
        }
        if let Some(partitions) = memory_partitions {
            layout.memory_bits = partitions
                .max(1)
                .next_power_of_two()
                .trailing_zeros()
                .max(layout.disk_bits);
        }
        loop {
            let fits_memory = self.memory(layout, read_threads) <= memory_budget;
            let fits_scratch = self.scratch(layout) <= scratch_budget;
            if (fits_memory || memory_partitions.is_some()) && (fits_scratch || passes.is_some()) {
                return layout;
            }
            if !fits_scratch && passes.is_none() && layout.disk_bits < layout.memory_bits {
                layout.disk_bits += 1;
            } else if memory_partitions.is_none() {
                layout.memory_bits += 1;
            } else {
                layout.disk_bits += 1;
            }
            assert!(layout.memory_bits <= 13, "no layout fits the budgets");
        }
    }
}

fn total_memory() -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

fn free_space(path: &Path) -> Option<u64> {
    let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// Records are collected in chunks. A chunk is full once it reaches this
/// size; a record never spans chunks.
const CHUNK_SIZE: usize = 4 << 20;

/// Maximum record size, so that a full chunk plus one record fits into the
/// chunk stride.
const MAX_RECORD_SIZE: usize = 64 << 10;

/// Offsets of records are `chunk * CHUNK_STRIDE + position`.
const CHUNK_STRIDE: u64 = (CHUNK_SIZE + MAX_RECORD_SIZE) as u64;

fn new_chunk() -> Vec<u8> {
    Vec::with_capacity(CHUNK_STRIDE as usize)
}

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

/// Sub-buckets for the parallel sort of a partition.
const SORT_BUCKET_BITS: u32 = 3;

/// Bytes currently occupied by scratch files.
static SCRATCH_USAGE: AtomicU64 = AtomicU64::new(0);

/// Partitions written to SST files so far.
static WRITTEN_PARTITIONS: AtomicUsize = AtomicUsize::new(0);

#[derive(Parser)]
struct Args {
    parquet: PathBuf,
    /// Directory for temporary partition files.
    #[clap(long)]
    scratch: PathBuf,
    /// Maximum bytes to use in scratch space. Determines the number of
    /// passes over the input. Defaults to 90% of the free space of the
    /// scratch directory.
    #[clap(long)]
    scratch_budget: Option<u64>,
    /// Maximum bytes of memory to use. Determines the number of memory
    /// partitions. Defaults to 60% of the total memory.
    #[clap(long)]
    memory_budget: Option<u64>,
    /// Threads to use in total. Defaults to the available parallelism minus
    /// two, to leave one physical core idle.
    #[clap(long)]
    threads: Option<usize>,
    /// Override the number of passes over the input.
    #[clap(long)]
    passes: Option<usize>,
    /// Override the number of memory partitions.
    #[clap(long)]
    memory_partitions: Option<usize>,
    /// Zstd level for scratch chunks, or 0 for LZ4. Zstd level 1 saves about
    /// 17% scratch space at twice the compression cost.
    #[clap(long, default_value = "0")]
    scratch_zstd_level: i32,
    /// Directory for the final SST files.
    #[clap(long)]
    out: PathBuf,
    /// Threads for reading the input (default: half of the threads).
    #[clap(long)]
    read_threads: Option<usize>,
    /// Threads for compressing and writing scratch chunks (default: a fifth
    /// of the threads).
    #[clap(long)]
    write_threads: Option<usize>,
    /// Threads for loading scratch files (default: a fifth of the threads).
    #[clap(long)]
    load_threads: Option<usize>,
    /// Threads for writing SST files (default: a quarter of the threads).
    #[clap(long)]
    sst_threads: Option<usize>,
    /// Only process the first row groups of the input (for benchmarking).
    #[clap(long)]
    row_groups: Option<usize>,
    /// Stop after the scatter of the first pass (for benchmarking).
    #[clap(long)]
    scatter_only: bool,
    /// Only print the estimated sizes and the chosen layout.
    #[clap(long)]
    plan_only: bool,
    /// Finish writing all partitions of a pass before scanning the input for
    /// the next pass. Use when the input is on the output disk, so that the
    /// scan does not compete with the SST writes.
    #[clap(long)]
    no_overlap: bool,
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

/// Bytes of the fixed length prefix of each record.
const LEN_PREFIX: usize = 4;

/// Appends a record to a buffer of records: `game_id` (6 bytes), a 4 byte
/// little endian value length, and the value produced by `pack`. Nothing is
/// appended if `pack` fails. Returns the offset of the length prefix.
fn push_record<E>(
    buf: &mut Vec<u8>,
    game_id: GameId,
    pack: impl FnOnce(&mut Vec<u8>) -> Result<(), E>,
) -> Result<u64, E> {
    let start = buf.len();
    buf.extend_from_slice(&game_id.to_bytes());
    let len_offset = buf.len();
    buf.extend_from_slice(&[0; LEN_PREFIX]);
    match pack(buf) {
        Ok(()) => {
            let len = (buf.len() - len_offset - LEN_PREFIX) as u32;
            assert!(
                buf.len() - start <= MAX_RECORD_SIZE,
                "record of {} too large",
                game_id
            );
            buf[len_offset..len_offset + LEN_PREFIX].copy_from_slice(&len.to_le_bytes());
            Ok(len_offset as u64)
        }
        Err(err) => {
            buf.truncate(start);
            Err(err)
        }
    }
}

/// A memory partition, loaded in a compact form ready to be sorted.
///
/// Records as produced by `push_record`, in chunks. The index points at the
/// length prefix of each record, as `chunk * CHUNK_STRIDE + position`.
#[derive(Default)]
struct MemoryPartition {
    chunks: Vec<Vec<u8>>,
    index: Vec<(GameId, u64)>,
}

impl MemoryPartition {
    fn push<E>(
        &mut self,
        game_id: GameId,
        pack: impl FnOnce(&mut Vec<u8>) -> Result<(), E>,
    ) -> Result<(), E> {
        if self
            .chunks
            .last()
            .is_none_or(|chunk| chunk.len() >= CHUNK_SIZE)
        {
            self.chunks.push(new_chunk());
        }
        let chunk = self.chunks.len() - 1;
        let offset = push_record(&mut self.chunks[chunk], game_id, pack)?;
        self.index
            .push((game_id, chunk as u64 * CHUNK_STRIDE + offset));
        Ok(())
    }

    /// Appends a chunk of records and indexes them.
    fn push_chunk(&mut self, chunk: Vec<u8>) {
        Self::index_records(
            &chunk,
            self.chunks.len() as u64 * CHUNK_STRIDE,
            &mut self.index,
        );
        self.chunks.push(chunk);
    }

    fn record_bytes(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum()
    }

    /// The packed value at the given offset.
    fn value_at(&self, offset: u64) -> &[u8] {
        let chunk = &self.chunks[(offset / CHUNK_STRIDE) as usize];
        let offset = (offset % CHUNK_STRIDE) as usize;
        let len = u32::from_le_bytes(
            chunk[offset..offset + LEN_PREFIX]
                .try_into()
                .expect("length prefix"),
        ) as usize;
        &chunk[offset + LEN_PREFIX..offset + LEN_PREFIX + len]
    }

    /// Builds index entries for a chunk of records starting at `base`.
    fn index_records(records: &[u8], base: u64, index: &mut Vec<(GameId, u64)>) {
        let mut pos = 0;
        while pos < records.len() {
            let game_id = GameId::unpack(&mut &records[pos..]);
            let len_offset = pos + GameId::SIZE;
            let len = u32::from_le_bytes(
                records[len_offset..len_offset + LEN_PREFIX]
                    .try_into()
                    .expect("length prefix"),
            ) as usize;
            index.push((game_id, base + len_offset as u64));
            pos = len_offset + LEN_PREFIX + len;
        }
        assert_eq!(pos, records.len(), "records truncated");
    }

    /// Sorts the index by game id, in parallel over sub-buckets of the key
    /// range. All entries must belong to one memory partition of `layout`.
    fn sort(&mut self, layout: Layout, threads: usize) {
        let buckets = 1 << SORT_BUCKET_BITS;
        let bits = layout.memory_bits + SORT_BUCKET_BITS;
        let mut bucketed: Vec<Vec<(GameId, u64)>> = (0..buckets).map(|_| Vec::new()).collect();
        for entry in self.index.drain(..) {
            bucketed[entry.0.partition(bits) & (buckets - 1)].push(entry);
        }
        let next = AtomicUsize::new(0);
        let slots: Vec<Mutex<&mut Vec<(GameId, u64)>>> =
            bucketed.iter_mut().map(Mutex::new).collect();
        thread::scope(|s| {
            let next = &next;
            let slots = &slots;
            for _ in 0..threads.clamp(1, buckets) {
                s.spawn(move || {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(slot) = slots.get(i) else {
                            break;
                        };
                        slot.lock()
                            .expect("lock bucket")
                            .sort_unstable_by_key(|&(game_id, _)| game_id);
                    }
                });
            }
        });
        drop(slots);
        for bucket in bucketed {
            self.index.extend(bucket);
        }
    }

    /// Checks that all game ids in the sorted index are unique.
    fn check_unique(&self) -> Result<(), GameId> {
        match self.index.windows(2).find(|w| w[0].0 == w[1].0) {
            Some(w) => Err(w[0].0),
            None => Ok(()),
        }
    }

    /// Merges partitions collected by multiple threads, without copying
    /// records.
    fn merge(parts: Vec<MemoryPartition>) -> MemoryPartition {
        let mut merged = MemoryPartition::default();
        merged
            .chunks
            .reserve_exact(parts.iter().map(|p| p.chunks.len()).sum());
        merged
            .index
            .reserve_exact(parts.iter().map(|p| p.index.len()).sum());
        for part in parts {
            let base = merged.chunks.len() as u64 * CHUNK_STRIDE;
            merged.chunks.extend(part.chunks);
            merged
                .index
                .extend(part.index.into_iter().map(|(id, off)| (id, base + off)));
        }
        merged
    }
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

    /// Loads and decompresses all chunks in parallel.
    fn load(&self, threads: usize) -> MemoryPartition {
        let next = AtomicUsize::new(0);
        let mut slots: Vec<Option<Vec<u8>>> = (0..self.chunks.len()).map(|_| None).collect();
        let slots_ref = Mutex::new(&mut slots);

        thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut decompressor = ChunkDecompressor::new(self.codec);
                    let mut compressed = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(chunk) = self.chunks.get(i) else {
                            break;
                        };
                        compressed.resize(chunk.compressed_len as usize, 0);
                        self.file
                            .read_exact_at(&mut compressed, chunk.offset)
                            .expect("read chunk");
                        let mut raw = new_chunk();
                        raw.resize(chunk.raw_len as usize, 0);
                        decompressor.decompress(&compressed, &mut raw);
                        slots_ref.lock().expect("lock slots")[i] = Some(raw);
                    }
                });
            }
        });

        let mut partition = MemoryPartition::default();
        partition.chunks.reserve_exact(slots.len());
        for slot in slots {
            partition.push_chunk(slot.expect("loaded chunk"));
        }
        partition
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

    // Leave one physical core (two hardware threads) idle by default.
    let threads = args.threads.unwrap_or_else(|| {
        thread::available_parallelism()
            .map_or(1, NonZeroUsize::get)
            .saturating_sub(2)
            .max(2)
    });
    let memory_budget = args
        .memory_budget
        .unwrap_or_else(|| total_memory().expect("total memory; pass --memory-budget") / 10 * 6);
    let scratch_budget = args.scratch_budget.unwrap_or_else(|| {
        free_space(&args.scratch).expect("free scratch space; pass --scratch-budget") / 10 * 9
    });

    let reader = GameReader::try_new(PositionalFile::open(&args.parquet)?)?;
    let num_row_groups = args.row_groups.map_or(reader.num_row_groups(), |limit| {
        limit.min(reader.num_row_groups())
    });

    let estimate = Estimate::sample(&reader, num_row_groups);
    let num_rows = estimate.rows;
    // The layout depends on the read threads (buffers), which depend on the
    // layout (overlap). Resolve with the smaller read thread count first.
    let provisional_read_threads = args.read_threads.unwrap_or(threads / 2).max(1);
    let mut layout = estimate.layout(
        memory_budget,
        scratch_budget,
        provisional_read_threads as u64,
        args.passes,
        args.memory_partitions,
    );
    // With a single pass the scan and the partition writes never overlap, so
    // each phase can use all threads. With several passes the scan of the
    // next pass runs while partitions are written.
    let single_pass = layout.passes() == 1 || args.no_overlap;
    let (read_default, write_default, load_default, sst_default) = if single_pass {
        (threads * 3 / 4, threads / 4, threads / 3, threads * 2 / 3)
    } else {
        (threads / 2, threads / 5, threads / 5, threads / 4)
    };
    let read_threads = args.read_threads.unwrap_or(read_default).max(1);
    let write_threads = args.write_threads.unwrap_or(write_default).max(1);
    let load_threads = args.load_threads.unwrap_or(load_default).max(1);
    let sst_threads = args.sst_threads.unwrap_or(sst_default).max(2);
    if read_threads > provisional_read_threads {
        layout = estimate.layout(
            memory_budget,
            scratch_budget,
            read_threads as u64,
            args.passes,
            args.memory_partitions,
        );
    }
    eprintln!(
        "{} games, about {} GB of records; {} passes with {} memory partitions each, about {} GB scratch per pass and {} GB memory; threads: {read_threads} read, {write_threads} write, {load_threads} load, {sst_threads} sst",
        num_rows,
        estimate.record_bytes >> 30,
        layout.passes(),
        layout.memory_partitions_per_pass(),
        estimate.scratch(layout) >> 30,
        estimate.memory(layout, read_threads as u64) >> 30,
    );
    if args.plan_only {
        return Ok(());
    }

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
        Some(num_rows * layout.passes() as u64),
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
                        let partition = scratch.load(load_threads);
                        scratch.remove().expect("remove scratch");
                        partition
                    }
                };
                let load = timer.elapsed();

                let timer = Instant::now();
                partition.sort(layout, load_threads);
                if let Err(game_id) = partition.check_unique() {
                    // Duplicates must be resolved when creating the parquet
                    // file. Exit instead of panicking, which would leave the
                    // other pipeline stages blocked.
                    eprintln!("duplicate game id {game_id} in partition {memory_partition}");
                    std::process::exit(1);
                }
                log(
                    progress,
                    format!(
                        "partition {memory_partition}: load {load:.1?}, sort {:.1?} ({} games, {} record bytes)",
                        timer.elapsed(),
                        partition.index.len(),
                        partition.record_bytes()
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
                write_partition(&partition, memory_partition, sst, sst_threads);
                WRITTEN_PARTITIONS.fetch_add(1, Ordering::Relaxed);
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

        for pass in 0..layout.passes() {
            progress.set_prefix(format!("pass {}/{}", pass + 1, layout.passes()));
            if args.no_overlap && pass > 0 {
                progress.set_message("waiting for previous pass");
                let timer = Instant::now();
                while WRITTEN_PARTITIONS.load(Ordering::Relaxed)
                    < pass * layout.memory_partitions_per_pass()
                {
                    thread::sleep(Duration::from_millis(100));
                }
                log(
                    progress,
                    format!(
                        "pass {pass}: waited {:.1?} for previous pass",
                        timer.elapsed()
                    ),
                );
            }
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
                    budget: (pass > 0).then_some(scratch_budget),
                    layout,
                    read_threads,
                    write_threads,
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
    layout: Layout,
    read_threads: usize,
    write_threads: usize,
}

/// Reads the entire input and distributes the games of the given disk
/// partition to its memory partitions. The last `RESIDENT_PARTITIONS_PER_PASS`
/// memory partitions are kept in memory, the others are written to scratch
/// files.
fn scatter_pass(
    reader: &GameReader<PositionalFile>,
    num_row_groups: usize,
    pass: usize,
    config: &ScatterConfig<'_>,
    progress: &ProgressBar,
) -> io::Result<(Resident, Vec<ScratchFile>, Duration)> {
    let layout = config.layout;
    let scratch_partitions = layout.scratch_partitions_per_pass();
    let first_memory_partition = pass * layout.memory_partitions_per_pass();

    let files: Vec<Mutex<ScratchFile>> = (0..scratch_partitions)
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
                let mut chunks: Vec<Vec<u8>> =
                    (0..scratch_partitions).map(|_| new_chunk()).collect();
                let mut skipped = 0;

                let mut route = |row: GameRow<'_>,
                                 local_resident: &mut Vec<MemoryPartition>,
                                 chunks: &mut Vec<Vec<u8>>| {
                    let Some(j) = layout.pass_memory_partition(row.game_id, pass) else {
                        return;
                    };
                    let pack = |buf: &mut Vec<u8>| row.pack_lichess_game(buf);
                    if j >= scratch_partitions {
                        if local_resident[j - scratch_partitions]
                            .push(row.game_id, pack)
                            .is_err()
                        {
                            skipped += 1;
                        }
                    } else {
                        let chunk = &mut chunks[j];
                        if push_record(chunk, row.game_id, pack).is_err() {
                            skipped += 1;
                        }
                        if chunk.len() >= CHUNK_SIZE {
                            let full = mem::replace(chunk, new_chunk());
                            tx.send((j, full)).expect("send chunk");
                        }
                    }
                };

                loop {
                    let i = next_row_group.fetch_add(1, Ordering::Relaxed);
                    if i >= num_row_groups {
                        break;
                    }
                    let batch = reader.read_games(i).expect("read row group");
                    for row in batch.rows() {
                        route(row, &mut local_resident, &mut chunks);
                    }

                    progress.inc(batch.len() as u64);
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
                first_memory_partition + scratch_partitions + i,
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
    let raw_per_game = partition.record_bytes() / partition.index.len();
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
                        &format!("games_{memory_partition:04}_{range:05}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> GameId {
        GameId::from_ascii(s.as_bytes()).unwrap()
    }

    #[test]
    fn test_sort_and_check_unique() {
        let mut partition = MemoryPartition::default();
        let games = [
            ("abcdefgh", b"long value".as_slice()),
            ("abcdefgh", b"short".as_slice()),
            ("zzzzzzzz", b"z".as_slice()),
            ("abcdefgh", b"long valux".as_slice()),
            ("00000000", b"first".as_slice()),
            ("zzzzzzzz", b"z".as_slice()),
        ];
        for (game_id, value) in games {
            partition
                .push::<()>(id(game_id), |buf| {
                    buf.extend_from_slice(value);
                    Ok(())
                })
                .unwrap();
        }
        let layout = Layout {
            memory_bits: 0,
            disk_bits: 0,
        };
        partition.sort(layout, 3);
        assert_eq!(partition.check_unique(), Err(id("abcdefgh")));
        let result: Vec<(GameId, Vec<u8>)> = partition
            .index
            .iter()
            .map(|&(game_id, offset)| (game_id, partition.value_at(offset).to_vec()))
            .collect();
        assert_eq!(result[0], (id("00000000"), b"first".to_vec()));
        assert_eq!(result[result.len() - 1], (id("zzzzzzzz"), b"z".to_vec()));
        assert_eq!(result.len(), 6);
    }

    #[test]
    fn test_sort_many() {
        let mut partition = MemoryPartition::default();
        // All ids within the first memory partition, as in production.
        let layout = Layout {
            memory_bits: 6,
            disk_bits: 1,
        };
        let span = 62u64.pow(8) >> layout.memory_bits;
        let mut ids: Vec<GameId> = (0..5000u64)
            .map(|i| {
                let n = (i * 7919 * 1_000_003) % span;
                GameId::unpack(&mut &n.to_be_bytes()[2..])
            })
            .collect();
        for game_id in &ids {
            partition.push::<()>(*game_id, |_| Ok(())).unwrap();
        }
        partition.sort(layout, 4);
        ids.sort();
        assert_eq!(partition.index.iter().map(|e| e.0).collect::<Vec<_>>(), ids);
    }
}
