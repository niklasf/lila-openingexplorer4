use std::{
    ffi::OsStr,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Instant,
};

use clap::Parser;
use crossbeam::channel;
use lila_openingexplorer4::{
    indexing::{FlatGame, GameVisitor, ParsedGame, RecentGameIds},
    packed::GameId,
    parquet::{FixedColumn, GameColumns, PositionalFile},
};
use parquet::{
    basic::{Compression, ZstdLevel},
    column::writer::ColumnCloseResult,
    file::{
        metadata::{PageIndexPolicy, ParquetMetaDataReader},
        reader::{FileReader, SerializedFileReader},
        writer::SerializedFileWriter,
    },
};
use pgn_reader::Reader;
use rustc_hash::FxHashSet;
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

const ROW_GROUP_LEN: usize = 100_000;

const FLATTEN_THREADS: usize = 2; // Enough to keep up with parsed games

const CHANNEL_LEN: usize = 32768; // Enough to buffer while previous row group is being written

/// Games with fewer plies are considered unplayed by Lichess and eventually
/// deleted, after which their ids can be reused. They are skipped, so that
/// game ids in the output are unique.
const MIN_PLIES: usize = 2;

/// Converts PGN files to a single parquet file.
///
/// Files are converted in parallel to parts in scratch space, which are then
/// concatenated in the given order. Games that appear in consecutive files
/// are only kept once.
#[derive(Parser)]
struct Args {
    pgns: Vec<PathBuf>,
    #[clap(long)]
    out: PathBuf,
    /// Directory for temporary part files.
    #[clap(long)]
    scratch: PathBuf,
    /// Files converted in parallel. Each takes about three threads.
    #[clap(long)]
    jobs: Option<usize>,
}

#[derive(Default, Debug, Clone, Copy)]
struct FileStats {
    games: u64,
    unplayed: u64,
    duplicates: u64,
    invalid: u64,
}

fn writer_properties() -> Arc<parquet::file::properties::WriterProperties> {
    Arc::new(
        GameColumns::properties()
            .set_compression(Compression::ZSTD(ZstdLevel::try_new(9).unwrap()))
            .build(),
    )
}

/// Converts one PGN file to a parquet part.
fn convert_file(pgn: &Path, part: &Path) -> io::Result<FileStats> {
    let mut stats = FileStats::default();

    thread::scope(|s| -> io::Result<()> {
        // Flatten in round robin, so that games can be collected in the
        // original order.
        let mut parsed_txs = Vec::with_capacity(FLATTEN_THREADS);
        let mut flat_rxs = Vec::with_capacity(FLATTEN_THREADS);

        for _ in 0..FLATTEN_THREADS {
            let (parsed_tx, parsed_rx) = channel::bounded::<ParsedGame>(CHANNEL_LEN);
            let (flat_tx, flat_rx) = channel::bounded::<io::Result<FlatGame>>(CHANNEL_LEN);
            parsed_txs.push(parsed_tx);
            flat_rxs.push(flat_rx);

            s.spawn(move || {
                for parsed in parsed_rx {
                    flat_tx
                        .send(FlatGame::flatten(parsed))
                        .expect("send flat game");
                }
            });
        }

        let writer = s.spawn(move || {
            let file = File::create(part).expect("create parquet part");
            let mut writer =
                SerializedFileWriter::new(file, GameColumns::schema(), writer_properties())
                    .expect("parquet writer");

            let mut columns = GameColumns::default();
            let mut games = 0;
            let mut invalid = 0;

            'collect: loop {
                for flat_rx in &flat_rxs {
                    match flat_rx.recv() {
                        Ok(Ok(flat)) => {
                            columns.push(flat);
                            games += 1;
                            if columns.len() >= ROW_GROUP_LEN {
                                columns
                                    .write_row_group(&mut writer)
                                    .expect("write row group");
                                columns.clear();
                            }
                        }
                        Ok(Err(err)) => {
                            eprintln!("skipping game: {err}");
                            invalid += 1;
                        }
                        Err(channel::RecvError) => break 'collect,
                    }
                }
            }

            if columns.len() > 0 {
                columns
                    .write_row_group(&mut writer)
                    .expect("write final row group");
            }
            writer.close().expect("close parquet file");
            (games, invalid)
        });

        let mut next_thread = 0;

        // Files can contain games twice, close to each other.
        let mut recent_game_ids = RecentGameIds::new();

        let file = File::open(pgn)?;
        let uncompressed: Box<dyn io::Read> = if pgn.extension() == Some(OsStr::new("zst")) {
            Box::new(zstd::Decoder::new(file)?)
        } else {
            Box::new(file)
        };

        let mut reader = Reader::new(uncompressed);

        for parsed in reader.read_games(&mut GameVisitor) {
            let parsed = match parsed? {
                Ok(parsed) => parsed,
                Err(err) => {
                    eprintln!("skipping game: {err}");
                    stats.invalid += 1;
                    continue;
                }
            };

            if parsed.num_plies() < MIN_PLIES {
                stats.unplayed += 1;
                continue;
            }

            if parsed
                .game_id()
                .is_some_and(|game_id| !recent_game_ids.insert(game_id))
            {
                continue;
            }

            parsed_txs[next_thread].send(parsed).expect("send parsed");
            next_thread = (next_thread + 1) % FLATTEN_THREADS;
        }

        stats.duplicates = recent_game_ids.duplicates();
        drop(parsed_txs);

        let (games, invalid) = writer.join().expect("writer thread");
        stats.games = games;
        stats.invalid += invalid;
        Ok(())
    })?;

    Ok(stats)
}

/// Appends a part to the output, dropping games that also appear in
/// `previous_ids`. Returns the ids of the appended games and the number of
/// dropped games.
fn append_part(
    writer: &mut SerializedFileWriter<File>,
    part: &Path,
    previous_ids: &FxHashSet<GameId>,
) -> io::Result<(FxHashSet<GameId>, u64)> {
    let positional = PositionalFile::open(part)?;
    let reader = SerializedFileReader::new(PositionalFile::open(part)?)?;
    let metadata = ParquetMetaDataReader::new()
        .with_page_index_policy(PageIndexPolicy::Optional)
        .parse_and_finish(&positional)?;
    let page_index = metadata.page_index();

    let mut ids = FxHashSet::default();
    let mut dropped = 0;
    let mut id_column = FixedColumn::default();
    let mut columns = GameColumns::default();
    let mut filtered = GameColumns::default();

    for (rg_idx, rg) in metadata.row_groups().iter().enumerate() {
        let row_group = reader.get_row_group(rg_idx)?;
        id_column.read(&*row_group, 0, GameId::SIZE)?;
        let keep: Vec<Option<usize>> = (0..id_column.len())
            .map(|i| {
                let game_id = GameId::unpack(&mut id_column.get(i));
                (!previous_ids.contains(&game_id)).then_some(0)
            })
            .collect();
        let kept = keep.iter().filter(|k| k.is_some()).count();
        dropped += (keep.len() - kept) as u64;

        if kept == 0 {
            continue;
        }

        if kept == keep.len() {
            // Copy the row group byte for byte.
            let mut rg_out = writer.next_row_group()?;
            for (col_idx, column) in rg.columns().iter().enumerate() {
                rg_out.append_column(
                    &positional,
                    ColumnCloseResult {
                        bytes_written: column.compressed_size() as u64,
                        rows_written: rg.num_rows() as u64,
                        metadata: column.clone(),
                        bloom_filter: None,
                        column_index: page_index
                            .and_then(|pi| pi.column_index(rg_idx, col_idx))
                            .cloned(),
                        offset_index: page_index
                            .and_then(|pi| pi.offset_index(rg_idx, col_idx))
                            .cloned(),
                    },
                )?;
            }
            rg_out.close()?;
        } else {
            // Rewrite the row group without the duplicates.
            columns.read_columns(&*row_group)?;
            filtered.clear();
            columns.scatter(
                |game_id| (!previous_ids.contains(&game_id)).then_some(0),
                &mut [&mut filtered],
            );
            assert_eq!(filtered.len(), kept);
            filtered.write_row_group(writer)?;
        }

        for (i, k) in keep.iter().enumerate() {
            if k.is_some() {
                ids.insert(GameId::unpack(&mut id_column.get(i)));
            }
        }
    }

    Ok((ids, dropped))
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let started = Instant::now();

    let parallelism = thread::available_parallelism().map_or(1, |n| n.get());
    let jobs = args
        .jobs
        .unwrap_or((parallelism.saturating_sub(2) / 3).max(1));

    let n = args.pgns.len();
    let parts: Vec<PathBuf> = (0..n)
        .map(|i| args.scratch.join(format!("part_{i:05}.parquet")))
        .collect();

    // Index of the next file to convert, and how many parts have been
    // appended to the output. Workers stay at most 2 * jobs files ahead of
    // the concatenation, to bound scratch space.
    let next_file = AtomicUsize::new(0);
    let appended = (Mutex::new(0usize), Condvar::new());
    let (done_tx, done_rx) = channel::unbounded::<(usize, io::Result<FileStats>)>();

    thread::scope(|s| {
        let args = &args;
        let parts = &parts;
        let next_file = &next_file;
        let appended = &appended;

        for _ in 0..jobs {
            let done_tx = done_tx.clone();
            s.spawn(move || {
                loop {
                    let i = next_file.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    {
                        let (lock, cvar) = appended;
                        let mut done = lock.lock().expect("lock appended");
                        while i >= *done + 2 * jobs {
                            done = cvar.wait(done).expect("wait appended");
                        }
                    }
                    let result = convert_file(&args.pgns[i], &parts[i]);
                    done_tx.send((i, result)).expect("send done");
                }
            });
        }
        drop(done_tx);

        // Concatenate parts in order.
        let file = File::create(&args.out).expect("create output");
        let mut writer =
            SerializedFileWriter::new(file, GameColumns::schema(), writer_properties())
                .expect("parquet writer");
        let mut pending: std::collections::BTreeMap<usize, io::Result<FileStats>> =
            Default::default();
        let mut previous_ids = FxHashSet::default();
        let mut total = FileStats::default();
        let mut boundary_duplicates = 0;

        for next in 0..n {
            let stats = loop {
                if let Some(stats) = pending.remove(&next) {
                    break stats;
                }
                let (i, stats) = done_rx.recv().expect("worker results");
                pending.insert(i, stats);
            };
            let stats = stats.unwrap_or_else(|err| panic!("{:?}: {err}", args.pgns[next]));

            let timer = Instant::now();
            let (ids, dropped) =
                append_part(&mut writer, &parts[next], &previous_ids).expect("append part");
            fs::remove_file(&parts[next]).expect("remove part");
            previous_ids = ids;

            {
                let (lock, cvar) = appended;
                *lock.lock().expect("lock appended") = next + 1;
                cvar.notify_all();
            }

            total.games += stats.games - dropped;
            total.unplayed += stats.unplayed;
            total.duplicates += stats.duplicates + dropped;
            total.invalid += stats.invalid;
            boundary_duplicates += dropped;
            eprintln!(
                "[{:>8.1?}] {}/{n} {}: {} games, {} unplayed, {} duplicates (+{dropped} with previous file), {} invalid, appended in {:.1?}",
                started.elapsed(),
                next + 1,
                args.pgns[next].display(),
                stats.games - dropped,
                stats.unplayed,
                stats.duplicates,
                stats.invalid,
                timer.elapsed()
            );
        }

        writer.close().expect("close output");
        println!(
            "done in {:.1?}: {} games, {} unplayed skipped, {} duplicates skipped ({boundary_duplicates} across files), {} invalid skipped",
            started.elapsed(),
            total.games,
            total.unplayed,
            total.duplicates,
            total.invalid
        );
    });

    Ok(())
}
