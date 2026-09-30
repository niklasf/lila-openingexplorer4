use std::{
    fs, io,
    path::PathBuf,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    thread,
};

use clap::Parser;
use lila_openingexplorer4::{
    packed::{GameId, LichessGame},
    parquet::{GameColumns, PositionalFile},
};
use parquet::file::reader::{FileReader, SerializedFileReader};
use rocksdb::{DB, IngestExternalFileOptions, IteratorMode, Options};

/// Ingests SST files into a temporary database and checks them against the
/// parquet input they were created from.
#[derive(Parser)]
struct Args {
    parquet: PathBuf,
    /// Directory with SST files.
    #[clap(long)]
    sst: PathBuf,
    /// Path for the temporary database (will be destroyed).
    #[clap(long)]
    db: PathBuf,
    /// Only check against the first row groups of the input.
    #[clap(long)]
    row_groups: Option<usize>,
    #[clap(long, default_value = "16")]
    threads: usize,
    /// Only verify games of the given memory partition (SST files named
    /// `games_{partition:03}_*`).
    #[clap(long)]
    partition: Option<usize>,
    /// Number of memory partition bits used by parquet_to_sst.
    #[clap(long, default_value = "6")]
    partition_bits: u32,
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let mut opts = Options::default();
    opts.create_if_missing(true);
    let _ = DB::destroy(&opts, &args.db);
    let db = DB::open(&opts, &args.db).expect("open db");

    let mut paths: Vec<PathBuf> = fs::read_dir(&args.sst)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "sst"))
        .filter(|path| {
            args.partition.is_none_or(|p| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&format!("games_{p:03}_")))
            })
        })
        .collect();
    paths.sort();
    println!("ingesting {} sst files", paths.len());
    let mut ingest_opts = IngestExternalFileOptions::default();
    ingest_opts.set_move_files(false);
    db.ingest_external_file_opts(&ingest_opts, paths)
        .expect("ingest");

    println!("scanning database");
    let mut count = 0u64;
    let mut prev: Option<Vec<u8>> = None;
    let mut smallest_value = usize::MAX;
    let mut largest_value = 0;
    for item in db.iterator(IteratorMode::Start) {
        let (key, value) = item.expect("iterate");
        assert_eq!(key.len(), GameId::SIZE, "key size");
        if let Some(prev) = &prev {
            assert!(prev[..] < key[..], "keys not strictly increasing");
        }
        GameId::unpack(&mut &key[..]);
        let game = LichessGame::unpack_entire(&mut &value[..]);
        assert!(game.index_status.is_empty(), "index status must be empty");
        smallest_value = smallest_value.min(value.len());
        largest_value = largest_value.max(value.len());
        prev = Some(key.into_vec());
        count += 1;
    }
    println!("{count} games in database, value size {smallest_value}..={largest_value}");

    println!("checking against parquet");
    let reader = SerializedFileReader::new(PositionalFile::open(&args.parquet)?)?;
    let num_row_groups = args.row_groups.map_or(reader.num_row_groups(), |limit| {
        limit.min(reader.num_row_groups())
    });
    let next_row_group = AtomicUsize::new(0);
    let found = AtomicU64::new(0);
    let skipped = AtomicU64::new(0);

    thread::scope(|s| {
        for _ in 0..args.threads {
            s.spawn(|| {
                let mut columns = GameColumns::default();
                let mut expected = Vec::new();
                loop {
                    let i = next_row_group.fetch_add(1, Ordering::Relaxed);
                    if i >= num_row_groups {
                        break;
                    }
                    columns
                        .read_columns(&*reader.get_row_group(i).expect("row group"))
                        .expect("read columns");
                    for row in columns.rows() {
                        if args
                            .partition
                            .is_some_and(|p| row.game_id.partition(args.partition_bits) != p)
                        {
                            continue;
                        }
                        expected.clear();
                        let value = db.get_pinned(row.game_id.to_bytes()).expect("get");
                        match row.pack_lichess_game(&mut expected) {
                            Ok(()) => {
                                let value =
                                    value.unwrap_or_else(|| panic!("missing game {}", row.game_id));
                                assert!(
                                    &value[..] == &expected[..],
                                    "value mismatch for {}",
                                    row.game_id
                                );
                                found.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(err) => {
                                assert!(value.is_none(), "invalid game {} present", row.game_id);
                                eprintln!("skipped {}: {err}", row.game_id);
                                skipped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            });
        }
    });

    let found = found.load(Ordering::Relaxed);
    let skipped = skipped.load(Ordering::Relaxed);
    println!("{found} games verified, {skipped} skipped rows");
    assert_eq!(
        found, count,
        "database has games not in checked parquet rows"
    );

    drop(db);
    DB::destroy(&opts, &args.db).expect("destroy db");
    println!("OK");
    Ok(())
}
