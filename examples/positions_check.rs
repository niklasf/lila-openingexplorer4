// Checks the output of parquet_to_positions against an independent replay
// of the same input slice: ingests both column families, verifies that
// keys are strictly increasing across files, and that every position of
// the replayed games is either a cold key with the right game id or a
// positions key whose entries account for all games reaching it.

use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
};

use lila_openingexplorer4::{
    packed::{GameId, LichessEntry, Zobrist96},
    parquet::{GameReader, PositionalFile},
    parquet_to_positions::{KeySpace, record::Record, replay::Replayer},
};
use nohash_hasher::IntMap;
use rocksdb::{ColumnFamilyDescriptor, DB, IngestExternalFileOptions, IteratorMode, Options};

fn sst_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "sst"))
        .collect();
    files.sort();
    files
}

fn main() {
    let mut args = env::args().skip(1);
    let parquet = args
        .next()
        .expect("usage: positions_check <parquet> <out dir> <db dir> <row_groups> [subset k/n]");
    let out = PathBuf::from(args.next().expect("out dir"));
    let db_dir = PathBuf::from(args.next().expect("db dir"));
    let row_groups: usize = args.next().expect("row groups").parse().unwrap();
    let (subset, subsets) = args.next().map_or((0, 1), |s| {
        let (k, n) = s.split_once('/').unwrap();
        (k.parse().unwrap(), n.parse::<usize>().unwrap())
    });
    let key_space = KeySpace {
        subset_bits: subsets.ilog2(),
        subset,
        memory_bits: 0,
    };

    // Ingest.
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.create_missing_column_families(true);
    let db = DB::open_cf_descriptors(
        &opts,
        &db_dir,
        [
            ColumnFamilyDescriptor::new("cold", Options::default()),
            ColumnFamilyDescriptor::new("positions", Options::default()),
        ],
    )
    .unwrap();
    let cold = db.cf_handle("cold").unwrap();
    let positions = db.cf_handle("positions").unwrap();
    let mut ingest = IngestExternalFileOptions::default();
    ingest.set_move_files(false);
    let cold_files = sst_files(&out.join("cold"));
    let positions_files = sst_files(&out.join("positions"));
    db.ingest_external_file_cf_opts(&cold, &ingest, cold_files.clone())
        .unwrap();
    db.ingest_external_file_cf_opts(&positions, &ingest, positions_files.clone())
        .unwrap();
    println!(
        "ingested {} cold and {} positions files",
        cold_files.len(),
        positions_files.len()
    );

    // Full scans: strictly increasing keys, counts.
    let (mut cold_keys, mut position_keys, mut position_games) = (0u64, 0u64, 0u64);
    let mut prev: Option<Box<[u8]>> = None;
    for item in db.iterator_cf(&cold, IteratorMode::Start) {
        let (key, value) = item.unwrap();
        assert_eq!(key.len(), 12);
        assert_eq!(value.len(), 6);
        if let Some(prev) = &prev {
            assert!(prev < &key, "cold keys not strictly increasing");
        }
        prev = Some(key);
        cold_keys += 1;
    }
    let mut prev: Option<Box<[u8]>> = None;
    for item in db.iterator_cf(&positions, IteratorMode::Start) {
        let (key, value) = item.unwrap();
        assert_eq!(key.len(), 14);
        if let Some(prev) = &prev {
            assert!(prev < &key, "positions keys not strictly increasing");
        }
        let mut entry = LichessEntry::new();
        entry.merge_unpack_entire(&mut &value[..]);
        position_games += entry.total_games();
        prev = Some(key);
        position_keys += 1;
    }
    println!("{cold_keys} cold keys, {position_keys} position keys with {position_games} games");

    // Independent replay: count games per position.
    let reader = GameReader::try_new(PositionalFile::open(&parquet).unwrap()).unwrap();
    let mut replayer = Replayer::new();
    let mut counts: IntMap<u64, (u32, Record)> = IntMap::default();
    let mut records = 0u64;
    for rg in 0..row_groups.min(reader.num_row_groups()) {
        let batch = reader.read_games(rg).unwrap();
        for row in batch.rows() {
            let Ok(game_records) = replayer.replay(&row) else {
                continue;
            };
            for r in game_records {
                if !key_space.contains(r.hash_prefix()) {
                    continue;
                }
                records += 1;
                let e = counts.entry(r.hash_prefix()).or_insert((0, *r));
                e.0 += 1;
            }
        }
    }
    println!(
        "replayed {records} records, {} distinct positions",
        counts.len()
    );
    assert_eq!(records, position_games + cold_keys, "records accounted for");

    // Every position must be found in the right column family.
    let (mut checked_cold, mut checked_positions) = (0u64, 0u64);
    let mut months: HashMap<Zobrist96, u64> = HashMap::new();
    for (i, (_, (count, record))) in counts.iter().enumerate() {
        let hash = record.hash();
        if *count == 1 {
            if i % 16 != 0 {
                continue;
            }
            let value = db
                .get_cf(&cold, hash.as_bytes())
                .unwrap()
                .unwrap_or_else(|| panic!("cold key missing for {record:?}"));
            assert_eq!(GameId::unpack(&mut &value[..]), record.game_id());
            // Not also in positions.
            let mut iter = db.raw_iterator_cf(&positions);
            iter.seek(hash.as_bytes());
            assert!(
                !iter.valid() || !iter.key().unwrap().starts_with(hash.as_bytes()),
                "cold position also in positions: {record:?}"
            );
            checked_cold += 1;
        } else {
            assert!(
                db.get_cf(&cold, hash.as_bytes()).unwrap().is_none(),
                "position with {count} games in cold: {record:?}"
            );
            let mut iter = db.raw_iterator_cf(&positions);
            iter.seek(hash.as_bytes());
            let mut games = 0;
            let mut found_move = false;
            while iter.valid() && iter.key().unwrap().starts_with(hash.as_bytes()) {
                let mut entry = LichessEntry::new();
                entry.merge_unpack_entire(&mut &iter.value().unwrap()[..]);
                games += entry.total_games();
                if iter.key().unwrap() == record.key() {
                    found_move = entry.games_for_move(record.uci_move()) > 0;
                }
                *months.entry(hash).or_insert(0) += 1;
                iter.next();
            }
            assert_eq!(games, u64::from(*count), "games of {record:?}");
            assert!(found_move, "move of {record:?} missing");
            checked_positions += 1;
        }
    }
    println!(
        "checked {checked_cold} cold positions (sampled) and {checked_positions} positions with several games: ok"
    );
}
