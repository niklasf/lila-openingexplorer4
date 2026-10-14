use std::{
    fmt,
    fs::{self, File},
    io, mem,
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crossbeam::channel;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rocksdb::{BlockBasedOptions, DBCompressionType, EnvOptions, Options, SstFileWriter};

use crate::{
    packed::GameId,
    parquet::{GameBatch, GameReader, GameRow, PositionalFile, RowGroupBytes},
};

/// Memory partitions per pass that are kept in memory while reading the
/// input, rather than being written to scratch space and read back.
const RESIDENT_PARTITIONS_PER_PASS: usize = 1;

/// Partitions in memory at the same time: one being written to SST files,
/// one being loaded and sorted, and one resident partition of the next pass.
const PARTITIONS_IN_MEMORY: u64 = 3;

/// Assumed ratio of compressed scratch chunks to record bytes (measured
/// about 0.81 with zstd level 1).
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

/// The row groups of all input files, in order.
struct Input {
    readers: Vec<GameReader<PositionalFile>>,
    /// `O_DIRECT` handles of the input files, for fetching row groups
    /// without the page cache.
    direct_files: Option<Vec<File>>,
    /// Pairs of reader index and row group index within that reader.
    row_groups: Vec<(usize, usize)>,
}

impl Input {
    fn open(paths: &[PathBuf], limit: Option<usize>, direct: bool) -> io::Result<Input> {
        let readers = paths
            .iter()
            .map(|path| Ok(GameReader::try_new(PositionalFile::open(path)?)?))
            .collect::<io::Result<Vec<_>>>()?;
        let direct_files = direct
            .then(|| {
                paths
                    .iter()
                    .map(|path| {
                        File::options()
                            .read(true)
                            .custom_flags(libc::O_DIRECT)
                            .open(path)
                    })
                    .collect::<io::Result<Vec<_>>>()
            })
            .transpose()?;
        let mut row_groups: Vec<(usize, usize)> = readers
            .iter()
            .enumerate()
            .flat_map(|(r, reader)| (0..reader.num_row_groups()).map(move |i| (r, i)))
            .collect();
        if let Some(limit) = limit {
            row_groups.truncate(limit);
        }
        Ok(Input {
            readers,
            direct_files,
            row_groups,
        })
    }

    fn num_row_groups(&self) -> usize {
        self.row_groups.len()
    }

    fn num_rows(&self, row_group: usize) -> u64 {
        let (r, i) = self.row_groups[row_group];
        self.readers[r].metadata().row_group(i).num_rows() as u64
    }

    fn read_games(&self, row_group: usize) -> parquet::errors::Result<GameBatch> {
        let (r, i) = self.row_groups[row_group];
        self.readers[r].read_games(i)
    }

    fn fetch(&self, row_group: usize) -> parquet::errors::Result<RowGroupBytes> {
        let (r, i) = self.row_groups[row_group];
        let Some(direct_files) = &self.direct_files else {
            return self.readers[r].fetch_row_group(i);
        };
        // One aligned read covering the row group.
        let reader = &self.readers[r];
        let (start, end) = reader.row_group_byte_range(i);
        let file_len = parquet::file::reader::Length::len(reader.input());
        let aligned_start = start / DIRECT_ALIGN as u64 * DIRECT_ALIGN as u64;
        let aligned_end = end.next_multiple_of(DIRECT_ALIGN as u64).min(file_len);
        let mut buf = AlignedBuf::new();
        let dst = buf.resize((aligned_end - aligned_start) as usize);
        // Reads past the end of the file are short, so read until the row
        // group is covered rather than until the aligned length is.
        let needed = (end - aligned_start) as usize;
        let mut filled = 0;
        while filled < needed {
            match direct_files[r].read_at(&mut dst[filled..], aligned_start + filled as u64)? {
                0 => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
                n => filled += n,
            }
        }
        let offset = buf.offset;
        let bytes = bytes::Bytes::from(buf.vec).slice(
            offset + (start - aligned_start) as usize..offset + (end - aligned_start) as usize,
        );
        Ok(RowGroupBytes::new(start, file_len, bytes))
    }

    fn read_games_from(
        &self,
        row_group: usize,
        bytes: RowGroupBytes,
    ) -> parquet::errors::Result<GameBatch> {
        let (r, i) = self.row_groups[row_group];
        self.readers[r].read_games_from(i, bytes)
    }
}

/// Estimated data sizes, from a sample of row groups.
struct Estimate {
    rows: u64,
    /// Record bytes (game id, length prefix and value) of all games.
    record_bytes: u64,
}

impl Estimate {
    fn sample(input: &Input) -> Estimate {
        let num_row_groups = input.num_row_groups();
        let rows: u64 = (0..num_row_groups).map(|i| input.num_rows(i)).sum();
        let samples = 256.min(num_row_groups);
        let mut value = Vec::new();
        let (mut sampled_rows, mut sampled_bytes) = (0u64, 0u64);
        for k in 0..samples {
            let i = k * num_row_groups / samples;
            let batch = input.read_games(i).expect("read row group");
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
/// until measured (about 1.1 with `PackedMoves` and 4 KB zstd blocks).
const INITIAL_SST_RATIO: f64 = 1.1;

/// Sub-buckets for the parallel sort of a partition.
const SORT_BUCKET_BITS: u32 = 3;

/// Counters shared by the threads of one run.
#[derive(Default)]
struct Counters {
    /// Games that could not be represented as `LichessGame`.
    skipped_games: AtomicU64,
    /// Bytes currently occupied by scratch files.
    scratch_usage: Gauge,
    /// Partitions written to SST files so far.
    written_partitions: Gauge,
}

/// A counter that threads can wait on.
#[derive(Default)]
struct Gauge {
    value: Mutex<u64>,
    changed: Condvar,
}

impl Gauge {
    fn add(&self, n: u64) {
        *self.value.lock().expect("lock gauge") += n;
        self.changed.notify_all();
    }

    fn sub(&self, n: u64) {
        *self.value.lock().expect("lock gauge") -= n;
        self.changed.notify_all();
    }

    /// Blocks until the value satisfies `ready`, and returns the time
    /// waited.
    fn wait_until(&self, ready: impl Fn(u64) -> bool) -> Duration {
        let timer = Instant::now();
        let mut value = self.value.lock().expect("lock gauge");
        while !ready(*value) {
            value = self.changed.wait(value).expect("wait gauge");
        }
        timer.elapsed()
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
    /// Override the number of passes over the input.
    pub passes: Option<usize>,
    /// Override the number of memory partitions.
    pub memory_partitions: Option<usize>,
    /// Threads for decoding the input.
    pub read_threads: Option<usize>,
    /// Threads for compressing and writing scratch chunks.
    pub write_threads: Option<usize>,
    /// Threads for loading scratch files.
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
    /// Zstd level of the SST files, or 0 for no compression.
    pub sst_zstd_level: i32,
    /// Maximum size of the zstd dictionary trained for each SST file (0 to
    /// disable dictionary compression).
    pub sst_zstd_dict: i32,
}

/// Bytes of the fixed length prefix of each record.
const LEN_PREFIX: usize = 2;

// Every value of a record within the size limit fits the length prefix.
const _: () = assert!(MAX_RECORD_SIZE - GameId::SIZE - LEN_PREFIX <= u16::MAX as usize);

/// Appends a record to a buffer of records: `game_id` (6 bytes), a 2 byte
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
            assert!(
                buf.len() - start <= MAX_RECORD_SIZE,
                "record of {} too large",
                game_id
            );
            let len = (buf.len() - len_offset - LEN_PREFIX) as u16;
            buf[len_offset..len_offset + LEN_PREFIX].copy_from_slice(&len.to_le_bytes());
            Ok(len_offset as u64)
        }
        Err(err) => {
            buf.truncate(start);
            Err(err)
        }
    }
}

/// Reads the length prefix at the given offset of a buffer of records.
fn value_len(records: &[u8], len_offset: usize) -> usize {
    u16::from_le_bytes(
        records[len_offset..len_offset + LEN_PREFIX]
            .try_into()
            .expect("length prefix"),
    ) as usize
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

    fn record_bytes(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum()
    }

    /// The packed value at the given offset.
    fn value_at(&self, offset: u64) -> &[u8] {
        let chunk = &self.chunks[(offset / CHUNK_STRIDE) as usize];
        let offset = (offset % CHUNK_STRIDE) as usize;
        let len = value_len(chunk, offset);
        &chunk[offset + LEN_PREFIX..offset + LEN_PREFIX + len]
    }

    /// Builds index entries for a chunk of records starting at `base`.
    fn index_records(records: &[u8], base: u64, index: &mut Vec<(GameId, u64)>) {
        let mut pos = 0;
        while pos < records.len() {
            let game_id = GameId::unpack(&mut &records[pos..]);
            let len_offset = pos + GameId::SIZE;
            let len = value_len(records, len_offset);
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

/// Alignment of buffers, offsets and lengths for `O_DIRECT` scratch writes.
const DIRECT_ALIGN: usize = 4096;

/// A zero filled buffer aligned to `DIRECT_ALIGN`.
struct AlignedBuf {
    vec: Vec<u8>,
    offset: usize,
}

impl AlignedBuf {
    fn new() -> AlignedBuf {
        AlignedBuf {
            vec: Vec::new(),
            offset: 0,
        }
    }

    /// Resizes to `len` bytes (rounded up to the alignment) and returns
    /// them, zero filled.
    fn resize(&mut self, len: usize) -> &mut [u8] {
        let len = len.next_multiple_of(DIRECT_ALIGN);
        if self.vec.capacity() < len + DIRECT_ALIGN {
            self.vec = Vec::with_capacity(len + DIRECT_ALIGN);
        }
        self.offset = self.vec.as_ptr().align_offset(DIRECT_ALIGN);
        self.vec.clear();
        self.vec.resize(self.offset + len, 0);
        &mut self.vec[self.offset..]
    }

    fn as_slice(&self) -> &[u8] {
        &self.vec[self.offset..]
    }
}

/// A compressed chunk: `len` bytes, followed by zero padding to
/// `DIRECT_ALIGN`.
struct Compressed<'a> {
    len: usize,
    padded: &'a [u8],
}

struct ChunkCompressor {
    /// `None` for no compression.
    zstd: Option<zstd::bulk::Compressor<'static>>,
    buf: AlignedBuf,
}

impl ChunkCompressor {
    fn new(zstd_level: i32) -> ChunkCompressor {
        ChunkCompressor {
            zstd: (zstd_level != 0)
                .then(|| zstd::bulk::Compressor::new(zstd_level).expect("zstd compressor")),
            buf: AlignedBuf::new(),
        }
    }

    fn compress(&mut self, raw: &[u8]) -> Compressed<'_> {
        let len = match &mut self.zstd {
            None => {
                self.buf.resize(raw.len())[..raw.len()].copy_from_slice(raw);
                raw.len()
            }
            Some(zstd) => {
                let dst = self.buf.resize(zstd::zstd_safe::compress_bound(raw.len()));
                zstd.compress_to_buffer(raw, dst).expect("compress")
            }
        };
        // The padding is zero filled by `resize`.
        let padded = &self.buf.as_slice()[..len.next_multiple_of(DIRECT_ALIGN)];
        Compressed { len, padded }
    }
}

struct ChunkDecompressor {
    /// `None` for no compression.
    zstd: Option<zstd::bulk::Decompressor<'static>>,
}

impl ChunkDecompressor {
    fn new(zstd_level: i32) -> ChunkDecompressor {
        ChunkDecompressor {
            zstd: (zstd_level != 0)
                .then(|| zstd::bulk::Decompressor::new().expect("zstd decompressor")),
        }
    }

    fn decompress(&mut self, compressed: &[u8], dst: &mut [u8]) {
        let n = match &mut self.zstd {
            None => {
                dst.copy_from_slice(compressed);
                compressed.len()
            }
            Some(zstd) => zstd
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

/// A scratch file holding compressed chunks of records.
struct ScratchFile {
    path: PathBuf,
    /// Buffered handle for reading.
    file: File,
    /// Handle for writing, and for reading back if `direct`: then with
    /// `O_DIRECT`, and chunks padded to `DIRECT_ALIGN`.
    write_file: File,
    direct: bool,
    zstd_level: i32,
    len: u64,
    chunks: Vec<ChunkMeta>,
}

impl ScratchFile {
    fn create(path: PathBuf, zstd_level: i32, direct: bool) -> io::Result<ScratchFile> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        let write_file = if direct {
            File::options()
                .read(true)
                .write(true)
                .custom_flags(libc::O_DIRECT)
                .open(&path)?
        } else {
            file.try_clone()?
        };
        Ok(ScratchFile {
            path,
            file,
            write_file,
            direct,
            zstd_level,
            len: 0,
            chunks: Vec::new(),
        })
    }

    fn append(
        &mut self,
        raw_len: usize,
        compressed: Compressed<'_>,
        usage: &Gauge,
    ) -> io::Result<()> {
        let bytes = if self.direct {
            compressed.padded
        } else {
            &compressed.padded[..compressed.len]
        };
        self.write_file.write_all_at(bytes, self.len)?;
        self.chunks.push(ChunkMeta {
            offset: self.len,
            raw_len: raw_len as u32,
            compressed_len: compressed.len as u32,
        });
        self.len += bytes.len() as u64;
        usage.add(bytes.len() as u64);
        Ok(())
    }

    /// Loads, decompresses and indexes all chunks in parallel.
    fn load(&self, threads: usize) -> MemoryPartition {
        type Loaded = (Vec<u8>, Vec<(GameId, u64)>);
        let next = AtomicUsize::new(0);
        let mut slots: Vec<Option<Loaded>> = (0..self.chunks.len()).map(|_| None).collect();
        let slots_ref = Mutex::new(&mut slots);

        thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut decompressor = ChunkDecompressor::new(self.zstd_level);
                    let mut compressed = AlignedBuf::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(chunk) = self.chunks.get(i) else {
                            break;
                        };
                        let len = chunk.compressed_len as usize;
                        let dst = compressed.resize(len);
                        if self.direct {
                            self.write_file
                                .read_exact_at(dst, chunk.offset)
                                .expect("read chunk");
                        } else {
                            self.file
                                .read_exact_at(&mut dst[..len], chunk.offset)
                                .expect("read chunk");
                        }
                        let mut raw = new_chunk();
                        raw.resize(chunk.raw_len as usize, 0);
                        decompressor.decompress(&dst[..len], &mut raw);
                        let mut index = Vec::new();
                        MemoryPartition::index_records(&raw, i as u64 * CHUNK_STRIDE, &mut index);
                        slots_ref.lock().expect("lock slots")[i] = Some((raw, index));
                    }
                });
            }
        });

        let mut partition = MemoryPartition::default();
        partition.chunks.reserve_exact(slots.len());
        partition.index.reserve_exact(
            slots
                .iter()
                .map(|slot| slot.as_ref().map_or(0, |(_, index)| index.len()))
                .sum(),
        );
        for slot in slots {
            let (chunk, index) = slot.expect("loaded chunk");
            partition.chunks.push(chunk);
            partition.index.extend(index);
        }
        partition
    }

    fn remove(self, usage: &Gauge) -> io::Result<()> {
        drop(self.file);
        drop(self.write_file);
        fs::remove_file(&self.path)?;
        usage.sub(self.len);
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
    env_opts: &'a EnvOptions,
    out: &'a Path,
    stats: Mutex<Stats>,
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub games: u64,
    pub files: u64,
    pub bytes: u64,
    /// Uncompressed value bytes of the games in the SST files.
    pub raw_bytes: u64,
    pub largest_file: u64,
    /// Games that could not be represented as `LichessGame`.
    pub skipped: u64,
}

impl Stats {
    fn add_file(&mut self, games: u64, bytes: u64, raw_bytes: u64) {
        self.games += games;
        self.files += 1;
        self.bytes += bytes;
        self.raw_bytes += raw_bytes;
        self.largest_file = self.largest_file.max(bytes);
    }

    /// Ratio of SST bytes to uncompressed value bytes.
    pub fn ratio(&self) -> f64 {
        if self.raw_bytes == 0 {
            INITIAL_SST_RATIO
        } else {
            self.bytes as f64 / self.raw_bytes as f64
        }
    }
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
}

impl Plan {
    pub fn new(parquet: &[PathBuf], config: &Config) -> io::Result<Plan> {
        let threads = config.threads;
        let input = Input::open(parquet, config.row_groups, config.direct_io)?;

        let estimate = Estimate::sample(&input);
        // The layout depends on the read threads (buffers), which depend on
        // the layout (overlap). Resolve with the smaller read thread count
        // first.
        let provisional_read_threads = config.read_threads.unwrap_or(threads / 2).max(1);
        let mut layout = estimate.layout(
            config.memory_budget,
            config.scratch_budget,
            provisional_read_threads as u64,
            config.passes,
            config.memory_partitions,
        );
        // With a single pass the scan and the partition writes never overlap,
        // so each phase can use all threads. With several passes the scan of
        // the next pass runs while partitions are written.
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
            layout = estimate.layout(
                config.memory_budget,
                config.scratch_budget,
                read_threads as u64,
                config.passes,
                config.memory_partitions,
            );
        }

        Ok(Plan {
            input,
            estimate,
            layout,
            read_threads,
            write_threads,
            load_threads,
            sst_threads,
        })
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} games, about {} GB of records; {} passes with {} memory partitions each, about {} GB scratch per pass and {} GB memory; threads: {} read, {} write, {} load, {} sst",
            self.estimate.rows,
            self.estimate.record_bytes >> 30,
            self.layout.passes(),
            self.layout.memory_partitions_per_pass(),
            self.estimate.scratch(self.layout) >> 30,
            self.estimate.memory(self.layout, self.read_threads as u64) >> 30,
            self.read_threads,
            self.write_threads,
            self.load_threads,
            self.sst_threads,
        )
    }
}

/// Sorts the games of the planned input into non-overlapping SST files in
/// `out`, using `scratch_dir` for temporary partition files.
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
    } = *plan;
    let num_rows = estimate.rows;

    let mut sst_opts = Options::default();
    if config.sst_zstd_level == 0 {
        sst_opts.set_compression_type(DBCompressionType::None);
    } else {
        sst_opts.set_compression_type(DBCompressionType::Zstd);
        // Window bits and strategy as rocksdb defaults.
        sst_opts.set_compression_options(-14, config.sst_zstd_level, 0, config.sst_zstd_dict);
        if config.sst_zstd_dict != 0 {
            sst_opts.set_zstd_max_train_bytes(config.sst_zstd_dict * 100);
        }
    }
    let mut table_opts = BlockBasedOptions::default();
    table_opts.set_block_size(config.sst_block_size);
    sst_opts.set_block_based_table_factory(&table_opts);
    let mut sst_env_opts = EnvOptions::default();
    sst_env_opts.set_use_direct_writes(config.direct_io);
    let sst = SstContext {
        opts: &sst_opts,
        env_opts: &sst_env_opts,
        out,
        stats: Mutex::new(Stats::default()),
    };
    let counters = Counters::default();

    let progress = progress.add(
        ProgressBar::new(num_rows * layout.passes() as u64).with_style(
            ProgressStyle::with_template(
                "{spinner} {prefix} {msg} {wide_bar} {human_pos}/{human_len} {per_sec} {eta:>7}",
            )
            .expect("progress template"),
        ),
    );

    thread::scope(|s| {
        let progress = &progress;
        let sst = &sst;
        let input = &input;
        let counters = &counters;

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
                        scratch.remove(&counters.scratch_usage).expect("remove scratch");
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
                counters.written_partitions.add(1);
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
                    layout,
                    read_threads,
                    write_threads,
                    io_depth: config.io_depth.max(1),
                    direct: config.direct_io,
                    counters,
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

    let mut stats = sst.stats.into_inner().expect("stats");
    stats.skipped = counters.skipped_games.load(Ordering::Relaxed);
    Ok(stats)
}

type Resident = Vec<(usize, MemoryPartition)>;

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
}

/// Accumulated time of each scatter stage, summed over its threads.
#[derive(Default)]
struct ScatterTimes {
    fetch: AtomicU64,
    fetch_send: AtomicU64,
    recv: AtomicU64,
    decode: AtomicU64,
    route: AtomicU64,
    send: AtomicU64,
    compress_recv: AtomicU64,
    compress: AtomicU64,
    write: AtomicU64,
}

fn timed<T>(counter: &AtomicU64, f: impl FnOnce() -> T) -> T {
    let timer = Instant::now();
    let result = f();
    counter.fetch_add(timer.elapsed().as_nanos() as u64, Ordering::Relaxed);
    result
}

impl ScatterTimes {
    fn summary(&self) -> String {
        let s = |c: &AtomicU64| c.load(Ordering::Relaxed) as f64 / 1e9;
        format!(
            "prefetch: fetch {:.0}s, send {:.0}s; read: recv {:.0}s, decode {:.0}s, route {:.0}s, send {:.0}s; compress: recv {:.0}s, compress {:.0}s, write {:.0}s",
            s(&self.fetch),
            s(&self.fetch_send),
            s(&self.recv),
            s(&self.decode),
            s(&self.route),
            s(&self.send),
            s(&self.compress_recv),
            s(&self.compress),
            s(&self.write),
        )
    }
}

/// Hands out row groups to decoding threads, prefetching their bytes with
/// `io_depth` concurrent reads. Cloned into each decoding thread.
#[derive(Clone)]
struct Prefetcher<'a> {
    input: &'a Input,
    /// Fetched row groups, in flight to the decoding threads.
    rx: channel::Receiver<(usize, RowGroupBytes)>,
}

impl<'a> Prefetcher<'a> {
    /// Spawns `io_depth` prefetch threads in the given scope. They finish
    /// once the input is exhausted or all decoding threads are done.
    fn spawn<'s>(
        input: &'a Input,
        io_depth: usize,
        times: &'a ScatterTimes,
        s: &'s thread::Scope<'s, '_>,
    ) -> Prefetcher<'a>
    where
        'a: 's,
    {
        let next_row_group = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = channel::bounded(io_depth);
        for _ in 0..io_depth {
            let tx = tx.clone();
            let next_row_group = Arc::clone(&next_row_group);
            s.spawn(move || {
                loop {
                    let i = next_row_group.fetch_add(1, Ordering::Relaxed);
                    if i >= input.num_row_groups() {
                        break;
                    }
                    let bytes = timed(&times.fetch, || input.fetch(i)).expect("fetch row group");
                    if timed(&times.fetch_send, || tx.send((i, bytes))).is_err() {
                        break;
                    }
                }
            });
        }
        Prefetcher { input, rx }
    }

    /// The next row group to decode, or `None` at the end of the input.
    fn recv(&self) -> Option<(usize, RowGroupBytes)> {
        self.rx.recv().ok()
    }

    fn decode(&self, (i, bytes): (usize, RowGroupBytes)) -> parquet::errors::Result<GameBatch> {
        self.input.read_games_from(i, bytes)
    }
}

/// Reads the entire input and distributes the games of the given disk
/// partition to its memory partitions. The last `RESIDENT_PARTITIONS_PER_PASS`
/// memory partitions are kept in memory, the others are written to scratch
/// files.
fn scatter_pass(
    input: &Input,
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
                config.zstd_level,
                config.direct,
            )?))
        })
        .collect::<io::Result<_>>()?;

    let resident: Mutex<Vec<MemoryPartition>> = Mutex::new(Vec::new());
    let throttled_nanos = AtomicU64::new(0);
    let times = ScatterTimes::default();

    thread::scope(|s| {
        let files = &files;
        let resident = &resident;
        let throttled_nanos = &throttled_nanos;
        let times = &times;
        let prefetcher = Prefetcher::spawn(input, config.io_depth, times, s);

        // Write threads compress full chunks and append them to scratch
        // files, throttled by the scratch budget.
        let (tx, rx) = channel::bounded::<(usize, Vec<u8>)>(config.write_threads * 2);

        for _ in 0..config.write_threads {
            let rx = rx.clone();
            s.spawn(move || {
                let mut compressor = ChunkCompressor::new(config.zstd_level);
                while let Ok((file, raw)) = timed(&times.compress_recv, || rx.recv()) {
                    let compressed = timed(&times.compress, || compressor.compress(&raw));
                    if let Some(budget) = config.budget {
                        let waited = config
                            .counters
                            .scratch_usage
                            .wait_until(|usage| usage + compressed.padded.len() as u64 <= budget);
                        throttled_nanos.fetch_add(waited.as_nanos() as u64, Ordering::Relaxed);
                    }
                    timed(&times.write, || {
                        files[file].lock().expect("lock scratch file").append(
                            raw.len(),
                            compressed,
                            &config.counters.scratch_usage,
                        )
                    })
                    .expect("append chunk");
                }
            });
        }

        drop(rx);

        // Read threads pack the games of this pass and route them to
        // resident partitions or scratch chunks.
        for _ in 0..config.read_threads {
            let tx = tx.clone();
            let prefetcher = prefetcher.clone();
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
                            timed(&times.send, || tx.send((j, full))).expect("send chunk");
                        }
                    }
                };

                while let Some(fetched) = timed(&times.recv, || prefetcher.recv()) {
                    let batch = timed(&times.decode, || prefetcher.decode(fetched))
                        .expect("read row group");
                    timed(&times.route, || {
                        for row in batch.rows() {
                            route(row, &mut local_resident, &mut chunks);
                        }
                    });

                    progress.inc(batch.len() as u64);
                }

                for (j, chunk) in chunks.into_iter().enumerate() {
                    if !chunk.is_empty() {
                        tx.send((j, chunk)).expect("send chunk");
                    }
                }

                config
                    .counters
                    .skipped_games
                    .fetch_add(skipped, Ordering::Relaxed);
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
                let w = SstFileWriter::create_with_env_options(sst.opts, sst.env_opts);
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
    progress.println(format!("[{:>8.1?}] {msg}", progress.elapsed()));
}
