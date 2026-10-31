// Reproduces RocksDB (11.8.1) reading data blocks from disk although the
// read tier is kBlockCacheTier, when the read is a batched MultiGet.
// `Get` with the same options returns Incomplete, as documented.
// Fixed upstream by facebook/rocksdb#15321 (2026-10-05, after 11.8.1).
//
// usage: multiget_tier_repro <empty or non-existing dir>

use std::env;

use rocksdb::{
    BlockBasedOptions, Cache, DB, ErrorKind, Options, ReadOptions, ReadTier,
    statistics::{StatsLevel, Ticker},
};

fn main() {
    let path = env::args().nth(1).expect("db dir");
    let n: u32 = 20_000;

    let mut table = BlockBasedOptions::default();
    table.set_block_size(4096);
    table.set_block_cache(&Cache::new_lru_cache(64 << 20));
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.set_block_based_table_factory(&table);
    opts.enable_statistics();
    opts.set_statistics_level(StatsLevel::ExceptDetailedTimers);

    // One flushed SST with values of a few hundred bytes, so that keys land
    // in distinct data blocks.
    {
        let db = DB::open(&opts, &path).expect("open");
        let value = vec![b'x'; 500];
        for i in 0..n {
            db.put(i.to_be_bytes(), &value).expect("put");
        }
        db.flush().expect("flush");
    }

    // Reopen and ask for values without I/O. Nothing has read the data
    // blocks yet, so none of them is in the block cache.
    let db = DB::open_cf_for_read_only(&opts, &path, ["default"], false).expect("reopen");
    let cf = db.cf_handle("default").expect("default cf");
    let keys: Vec<[u8; 4]> = (0..n)
        .step_by(n as usize / 8)
        .map(u32::to_be_bytes)
        .collect();
    let mut cache_only = ReadOptions::default();
    cache_only.set_read_tier(ReadTier::BlockCache);

    let describe =
        |result: Result<Option<rocksdb::DBPinnableSlice<'_>>, rocksdb::Error>| match result {
            Ok(Some(value)) => format!("value of {} bytes", value.len()),
            Ok(None) => "not found".to_owned(),
            Err(err) if err.kind() == ErrorKind::Incomplete => "Incomplete".to_owned(),
            Err(err) => format!("error {err}"),
        };
    // Data blocks inserted into the block cache, i.e. read from the file.
    let added = || opts.get_ticker_count(Ticker::BlockCacheDataAdd);

    let before = added();
    println!("Get, read tier BlockCache:");
    for key in &keys {
        println!("  {}", describe(db.get_pinned_cf_opt(cf, key, &cache_only)));
    }
    println!(
        "  data blocks read and added to the cache: {}",
        added() - before
    );

    let before = added();
    println!("MultiGet, read tier BlockCache:");
    for result in db.batched_multi_get_cf_opt(cf, &keys, true, &cache_only) {
        println!("  {}", describe(result));
    }
    println!(
        "  data blocks read and added to the cache: {}",
        added() - before
    );

    let before = added();
    println!("Get again, read tier BlockCache:");
    for key in &keys {
        println!("  {}", describe(db.get_pinned_cf_opt(cf, key, &cache_only)));
    }
    println!(
        "  data blocks read and added to the cache: {}",
        added() - before
    );
}
