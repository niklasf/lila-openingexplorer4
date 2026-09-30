use std::{
    fs::{self, File},
    io, mem,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::Instant,
};

use clap::Parser;
use crossbeam::channel;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use lila_openingexplorer4::{
    packed::{GameId, pack_bytes, unpack_uint},
    parquet::{GameColumns, PositionalFile},
};
use parquet::{
    basic::Compression,
    file::{
        reader::{FileReader, SerializedFileReader},
        writer::SerializedFileWriter,
    },
};
use rocksdb::{BlockBasedOptions, DBCompressionType, Options, SstFileWriter};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// One pass over the input for each disk partition. All memory partitions of
/// a disk partition, except the resident ones, must fit into scratch space at
/// the same time.
const DISK_PARTITION_BITS: u32 = 3;

/// Each memory partition must fit into memory at once.
const MEMORY_PARTITION_BITS: u32 = 4;

const _: () = assert!(MEMORY_PARTITION_BITS >= DISK_PARTITION_BITS);

const MEMORY_PARTITIONS_PER_PASS: usize = 1 << (MEMORY_PARTITION_BITS - DISK_PARTITION_BITS);

/// Memory partitions per pass that are kept in memory while reading the
/// input, rather than being written to scratch space and read back. Each
/// resident partition takes about as much memory as processing a memory
/// partition.
const RESIDENT_PARTITIONS_PER_PASS: usize = 1;

const _: () = assert!(RESIDENT_PARTITIONS_PER_PASS <= MEMORY_PARTITIONS_PER_PASS);

/// Memory partitions per pass that are written to scratch space.
const SCRATCH_PARTITIONS_PER_PASS: usize =
    MEMORY_PARTITIONS_PER_PASS - RESIDENT_PARTITIONS_PER_PASS;

const ROW_GROUP_LEN: usize = 100_000;

/// Hard limit for the size of each SST file.
const SST_MAX_SIZE: u64 = 512 * 1024 * 1024;

/// Start a new SST file once the current one reaches this size. The margin
/// leaves room for the index and footer written when finishing the file.
const SST_ROLL_SIZE: u64 = SST_MAX_SIZE - 16 * 1024 * 1024;

/// Games that could not be represented as `LichessGame`.
static SKIPPED_GAMES: AtomicU64 = AtomicU64::new(0);

#[derive(Parser)]
struct Args {
    parquet: PathBuf,
    /// Directory for temporary partition files.
    #[clap(long)]
    scratch: PathBuf,
    /// Directory for the final SST files.
    #[clap(long)]
    out: PathBuf,
    /// Threads for reading the input and loading scratch files. Defaults to
    /// half of the usable parallelism.
    #[clap(long)]
    read_threads: Option<usize>,
    /// Threads for writing scratch files. Defaults to half of the usable
    /// parallelism.
    #[clap(long)]
    write_threads: Option<usize>,
    /// Threads for writing SST files. Defaults to the usable parallelism.
    #[clap(long)]
    sst_threads: Option<usize>,
    /// Only process the first row groups of the input (for benchmarking).
    #[clap(long)]
    row_groups: Option<usize>,
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

#[derive(Copy, Clone)]
struct Threads {
    read: usize,
    write: usize,
    sst: usize,
}

impl Threads {
    /// Each scratch partition is split into this many files, so that all
    /// write threads can be busy, even with few scratch partitions per pass.
    fn shards(self) -> usize {
        self.write
            .div_ceil(SCRATCH_PARTITIONS_PER_PASS.max(1))
            .max(1)
    }
}

/// Writes row groups to a file in scratch space.
struct ScratchWriter {
    path: PathBuf,
    writer: SerializedFileWriter<File>,
}

impl ScratchWriter {
    pub fn create(path: PathBuf) -> io::Result<ScratchWriter> {
        let file = File::create(&path)?;
        Ok(ScratchWriter {
            path,
            writer: SerializedFileWriter::new(
                file,
                GameColumns::schema(),
                Arc::new(
                    GameColumns::properties()
                        .set_compression(Compression::LZ4)
                        .build(),
                ),
            )?,
        })
    }

    pub fn write(&mut self, columns: &GameColumns) {
        columns
            .write_row_group(&mut self.writer)
            .expect("write row group");
    }

    pub fn close(self) -> PathBuf {
        self.writer.close().expect("close partition file");
        self.path
    }
}

/// A scratch file, with a buffer collecting pieces into a full row group.
struct ScratchFile {
    buffer: Mutex<GameColumns>,
    writer: Mutex<ScratchWriter>,
}

/// A memory partition, loaded in a compact form ready to be sorted.
#[derive(Default)]
struct MemoryPartition {
    /// Length-prefixed packed values, back to back.
    values: Vec<u8>,
    /// Game ids and the offsets of their values.
    index: Vec<(GameId, u64)>,
}

impl MemoryPartition {
    pub fn push(&mut self, columns: &GameColumns) {
        self.index.reserve(columns.len());
        let mut value = Vec::new();
        let mut skipped = 0;
        for row in columns.rows() {
            value.clear();
            if row.pack_lichess_game(&mut value).is_err() {
                skipped += 1;
                continue;
            }
            self.index.push((row.game_id, self.values.len() as u64));
            pack_bytes(&mut self.values, &value);
        }
        if skipped > 0 {
            SKIPPED_GAMES.fetch_add(skipped, Ordering::Relaxed);
        }
    }

    /// The packed value at the given offset in `values`.
    pub fn value_at(&self, offset: u64) -> &[u8] {
        let mut buf = &self.values[offset as usize..];
        let len = unpack_uint(&mut buf) as usize;
        &buf[..len]
    }

    pub fn append(&mut self, other: MemoryPartition) {
        if self.index.is_empty() {
            *self = other;
            return;
        }
        let base = self.values.len() as u64;
        self.values.extend_from_slice(&other.values);
        self.index.extend(
            other
                .index
                .into_iter()
                .map(|(game_id, offset)| (game_id, base + offset)),
        );
    }

    /// Loads all row groups of the given scratch files, in parallel.
    pub fn load(paths: &[PathBuf], threads: usize) -> io::Result<MemoryPartition> {
        let readers = paths
            .iter()
            .map(|path| Ok::<_, io::Error>(SerializedFileReader::new(PositionalFile::open(path)?)?))
            .collect::<io::Result<Vec<_>>>()?;

        let jobs: Vec<(usize, usize)> = readers
            .iter()
            .enumerate()
            .flat_map(|(file, reader)| (0..reader.num_row_groups()).map(move |i| (file, i)))
            .collect();
        let next_job = AtomicUsize::new(0);

        let partition = Mutex::new(MemoryPartition::default());

        thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut local = MemoryPartition::default();
                    let mut columns = GameColumns::default();
                    while let Some(&(file, i)) = jobs.get(next_job.fetch_add(1, Ordering::Relaxed))
                    {
                        columns
                            .read_columns(&*readers[file].get_row_group(i).expect("row group"))
                            .expect("read columns");
                        local.push(&columns);
                    }
                    partition.lock().expect("lock partition").append(local);
                });
            }
        });

        Ok(partition.into_inner().expect("partition"))
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    // Leave one physical core (two hardware threads) idle.
    let parallelism = thread::available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .saturating_sub(2)
        .max(2);
    let threads = Threads {
        read: args.read_threads.unwrap_or(parallelism / 2),
        write: args.write_threads.unwrap_or(parallelism / 2),
        sst: args.sst_threads.unwrap_or(parallelism),
    };

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

    let passes = 1 << DISK_PARTITION_BITS;
    let started = Instant::now();

    let progress = ProgressBar::with_draw_target(
        Some(num_rows * passes as u64),
        ProgressDrawTarget::stdout_with_hz(4),
    )
    .with_style(
        ProgressStyle::with_template(
            "{spinner} {prefix} {msg} {wide_bar} {human_pos}/{human_len} {per_sec} {eta:>7}",
        )
        .expect("progress template"),
    );

    for pass in 0..passes {
        progress.set_prefix(format!("pass {}/{}", pass + 1, passes));

        progress.set_message("scattering");
        let timer = Instant::now();
        let (resident, scratch) = scatter_pass(
            &reader,
            num_row_groups,
            pass,
            &args.scratch,
            threads,
            &progress,
        )?;
        log(&progress, format!("pass {pass}: scatter {:.1?}", timer.elapsed()));

        // Process resident partitions first, to free their memory.
        for (memory_partition, partition) in resident {
            progress.set_message(format!("processing {memory_partition} (resident)"));
            process_memory_partition(partition, memory_partition, &sst, threads.sst, &progress)?;
        }

        for (memory_partition, paths) in scratch {
            progress.set_message(format!("loading {memory_partition}"));
            let timer = Instant::now();
            let partition = MemoryPartition::load(&paths, threads.read)?;
            for path in &paths {
                fs::remove_file(path)?;
            }
            log(
                &progress,
                format!(
                    "partition {memory_partition}: load {:.1?} ({} games, {} value bytes)",
                    timer.elapsed(),
                    partition.index.len(),
                    partition.values.len()
                ),
            );

            progress.set_message(format!("processing {memory_partition}"));
            process_memory_partition(partition, memory_partition, &sst, threads.sst, &progress)?;
        }
    }

    progress.finish();

    let stats = sst.stats.into_inner().expect("stats");
    println!(
        "done in {:.1?}: {} games in {} sst files ({} bytes, largest {} bytes), {} skipped games",
        started.elapsed(),
        stats.games,
        stats.files,
        stats.bytes,
        stats.largest_file,
        SKIPPED_GAMES.load(Ordering::Relaxed),
    );
    Ok(())
}

type Resident = Vec<(usize, MemoryPartition)>;
type Scratch = Vec<(usize, Vec<PathBuf>)>;

/// Reads the entire input and distributes the games of the given disk
/// partition to its memory partitions. The last `RESIDENT_PARTITIONS_PER_PASS`
/// memory partitions are kept in memory, the others are written to sharded
/// files in scratch space.
fn scatter_pass(
    reader: &SerializedFileReader<PositionalFile>,
    num_row_groups: usize,
    pass: usize,
    scratch: &Path,
    threads: Threads,
    progress: &ProgressBar,
) -> io::Result<(Resident, Scratch)> {
    let first_memory_partition = pass * MEMORY_PARTITIONS_PER_PASS;
    let shards = threads.shards();

    // Scratch file for shard s of scratch partition i at index i * shards + s.
    let files = (0..SCRATCH_PARTITIONS_PER_PASS)
        .flat_map(|i| (0..shards).map(move |s| (first_memory_partition + i, s)))
        .map(|(memory_partition, shard)| {
            Ok::<_, io::Error>(ScratchFile {
                buffer: Mutex::new(GameColumns::default()),
                writer: Mutex::new(ScratchWriter::create(
                    scratch.join(format!("games_{memory_partition:05}_{shard:03}.parquet")),
                )?),
            })
        })
        .collect::<io::Result<Vec<_>>>()?;

    let resident: Vec<Mutex<MemoryPartition>> = (0..RESIDENT_PARTITIONS_PER_PASS)
        .map(|_| Mutex::new(MemoryPartition::default()))
        .collect();

    thread::scope(|s| {
        let files = &files;
        let resident = &resident;

        // Write threads take full row groups for any scratch file. Writes to
        // the same file are serialized by its lock.
        let (tx, rx) = channel::bounded::<(usize, GameColumns)>(threads.write);

        for _ in 0..threads.write {
            let rx = rx.clone();
            s.spawn(move || {
                for (file, batch) in rx {
                    files[file]
                        .writer
                        .lock()
                        .expect("lock writer")
                        .write(&batch);
                }
            });
        }

        drop(rx);

        // Read threads scatter each row group into pieces, one for each
        // memory partition of this pass. They pack pieces for resident
        // partitions, and collect pieces for scratch files into full row
        // groups.
        let read_threads: Vec<_> = (0..threads.read)
            .map(|thread_idx| {
                let tx = tx.clone();
                s.spawn(move || {
                    // Collect resident partitions locally, to avoid
                    // contention.
                    let mut local_resident: Vec<MemoryPartition> = (0
                        ..RESIDENT_PARTITIONS_PER_PASS)
                        .map(|_| MemoryPartition::default())
                        .collect();

                    for i in (thread_idx..num_row_groups).step_by(threads.read) {
                        let row_group_reader = reader.get_row_group(i).expect("row group");
                        let mut columns = GameColumns::default();
                        columns
                            .read_columns(&*row_group_reader)
                            .expect("read columns");

                        let mut pieces: Vec<GameColumns> = (0..MEMORY_PARTITIONS_PER_PASS)
                            .map(|_| GameColumns::default())
                            .collect();
                        columns.scatter(
                            |game_id| pass_memory_partition(game_id, pass),
                            &mut pieces.iter_mut().collect::<Vec<_>>(),
                        );

                        progress.inc(columns.len() as u64);

                        for (j, mut piece) in pieces.into_iter().enumerate() {
                            if piece.len() == 0 {
                                continue;
                            }

                            if j >= SCRATCH_PARTITIONS_PER_PASS {
                                local_resident[j - SCRATCH_PARTITIONS_PER_PASS].push(&piece);
                            } else {
                                let file = j * shards + i % shards;
                                let batch = {
                                    let mut buffer =
                                        files[file].buffer.lock().expect("lock buffer");
                                    buffer.append(&mut piece);
                                    (buffer.len() >= ROW_GROUP_LEN).then(|| mem::take(&mut *buffer))
                                };
                                if let Some(batch) = batch {
                                    tx.send((file, batch)).expect("send batch");
                                }
                            }
                        }
                    }

                    for (partition, local) in resident.iter().zip(local_resident) {
                        partition.lock().expect("lock resident").append(local);
                    }
                })
            })
            .collect();

        for read_thread in read_threads {
            read_thread.join().expect("read thread");
        }

        // Write remaining partial row groups.
        for (file, scratch_file) in files.iter().enumerate() {
            let batch = mem::take(&mut *scratch_file.buffer.lock().expect("lock buffer"));
            if batch.len() > 0 {
                tx.send((file, batch)).expect("send batch");
            }
        }

        // Write threads finish once all batches are written.
        drop(tx);
    });

    let mut scratch_paths: Scratch = (0..SCRATCH_PARTITIONS_PER_PASS)
        .map(|i| (first_memory_partition + i, Vec::with_capacity(shards)))
        .collect();
    for (file, scratch_file) in files.into_iter().enumerate() {
        let path = scratch_file.writer.into_inner().expect("writer").close();
        scratch_paths[file / shards].1.push(path);
    }

    let resident = resident
        .into_iter()
        .enumerate()
        .map(|(i, partition)| {
            (
                first_memory_partition + SCRATCH_PARTITIONS_PER_PASS + i,
                partition.into_inner().expect("resident"),
            )
        })
        .collect();

    Ok((resident, scratch_paths))
}

/// Index of the memory partition within the given pass, or `None` if the
/// game belongs to a different pass.
fn pass_memory_partition(game_id: GameId, pass: usize) -> Option<usize> {
    let memory_partition = game_id.partition(MEMORY_PARTITION_BITS);
    let disk_partition = memory_partition >> (MEMORY_PARTITION_BITS - DISK_PARTITION_BITS);
    (disk_partition == pass).then_some(memory_partition & (MEMORY_PARTITIONS_PER_PASS - 1))
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
    largest_file: u64,
}

impl SstStats {
    fn add_file(&mut self, games: u64, bytes: u64) {
        self.games += games;
        self.files += 1;
        self.bytes += bytes;
        self.largest_file = self.largest_file.max(bytes);
    }
}

/// Sorts the partition by game id and writes it to non-overlapping SST files.
///
/// The sorted partition is split into contiguous ranges, each written by its
/// own thread as a sequence of SST files. File names sort in key order.
fn process_memory_partition(
    mut partition: MemoryPartition,
    memory_partition: usize,
    sst: &SstContext<'_>,
    threads: usize,
    progress: &ProgressBar,
) -> io::Result<()> {
    let timer = Instant::now();
    partition
        .index
        .sort_unstable_by_key(|&(game_id, _)| game_id);
    log(
        progress,
        format!(
            "partition {memory_partition}: sort {:.1?} ({} games)",
            timer.elapsed(),
            partition.index.len()
        ),
    );

    let timer = Instant::now();
    let ranges = threads.clamp(1, partition.index.len().max(1));
    let range_len = partition.index.len().div_ceil(ranges);

    thread::scope(|s| {
        let partition = &partition;
        for (range, index) in partition.index.chunks(range_len).enumerate() {
            s.spawn(move || {
                write_sst_range(
                    partition,
                    index,
                    sst,
                    &format!("games_{memory_partition:04}_{range:03}"),
                )
                .expect("write sst files");
            });
        }
    });
    log(
        progress,
        format!("partition {memory_partition}: write {:.1?}", timer.elapsed()),
    );

    Ok(())
}

fn log(progress: &ProgressBar, msg: String) {
    // Must not touch the progress bar inside `suspend` (its state is locked).
    let elapsed = progress.elapsed();
    progress.suspend(|| eprintln!("[{elapsed:>8.1?}] {msg}"));
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

    let finish = |writer: &mut Option<SstFileWriter<'_>>, games_in_file: &mut u64| {
        if let Some(mut w) = writer.take() {
            w.finish()?;
            let bytes = w.file_size();
            assert!(bytes <= SST_MAX_SIZE, "sst file too large: {bytes}");
            sst.stats
                .lock()
                .expect("lock stats")
                .add_file(*games_in_file, bytes);
            *games_in_file = 0;
        }
        Ok::<_, rocksdb::Error>(())
    };

    for &(game_id, offset) in index {
        let w = match &mut writer {
            Some(w) => w,
            None => {
                let w = SstFileWriter::create(sst.opts);
                w.open(sst.out.join(format!("{prefix}_{seq:04}.sst")))?;
                seq += 1;
                writer.insert(w)
            }
        };

        w.put(game_id.to_bytes(), partition.value_at(offset))?;
        games_in_file += 1;

        if w.file_size() >= SST_ROLL_SIZE {
            finish(&mut writer, &mut games_in_file)?;
        }
    }

    finish(&mut writer, &mut games_in_file)
}
