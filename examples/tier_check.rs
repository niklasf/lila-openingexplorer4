// Checks that block-cache-only game lookups report misses as such (with
// and without pinned metadata), and times the lookup variants.
//
// usage: [PIN=1] [LRU=1] tier_check <db>

use std::{env, path::Path};

use lila_openingexplorer4::{
    packed::EntryView,
    server::{Database, DbConfig},
};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let db = Database::open_read_only(
        Path::new(&args[0]),
        &DbConfig {
            cache_bytes: 1 << 30,
            lru_cache: env::var("LRU").is_ok(),
            direct_reads: false,
            statistics: true,
            file_opening_threads: 16,
            block_size: 4096,
            pin_metadata: env::var("PIN").is_ok(),
        },
    )
    .expect("open db");
    let entries = db.scan_positions(10).expect("scan");
    let mut ids = Vec::new();
    for entry in &entries {
        for cell in EntryView::unpack_entire(entry).cells() {
            if let Some((_, id)) = cell.games.next() {
                ids.push(id);
                break;
            }
        }
    }
    println!("{} ids", ids.len());
    let cached = db.games_cached(&ids, true).expect("cached");
    println!(
        "fresh: {} in cache ({} of them reported absent), {} not",
        cached.iter().filter(|c| c.is_some()).count(),
        cached.iter().filter(|c| matches!(c, Some(None))).count(),
        cached.iter().filter(|c| c.is_none()).count()
    );
    for id in &ids[..ids.len() / 2] {
        db.game(*id, true).expect("get");
    }
    let cached = db.games_cached(&ids, true).expect("cached");
    println!(
        "after loading half: {} in cache, {} not",
        cached.iter().filter(|c| c.is_some()).count(),
        cached.iter().filter(|c| c.is_none()).count()
    );
    for id in &ids {
        db.game(*id, true).expect("get");
    }
    let tick = |label: &str| {
        println!(
            "{label}: data hit {} miss {}, index miss {}",
            db.ticker(rocksdb::statistics::Ticker::BlockCacheDataHit),
            db.ticker(rocksdb::statistics::Ticker::BlockCacheDataMiss),
            db.ticker(rocksdb::statistics::Ticker::BlockCacheIndexMiss)
        );
    };
    tick("before");
    let t = std::time::Instant::now();
    for _ in 0..1000 {
        std::hint::black_box(db.games_cached(&ids, true).expect("cached"));
    }
    println!("games_cached x1000: {:.1?}", t.elapsed());
    tick("after games_cached");
    let t = std::time::Instant::now();
    for _ in 0..1000 {
        std::hint::black_box(db.games(&ids, true).expect("games"));
    }
    println!("games (multi get) x1000: {:.1?}", t.elapsed());
    tick("after games");
    let t = std::time::Instant::now();
    for _ in 0..1000 {
        for id in &ids {
            std::hint::black_box(db.game(*id, true).expect("get"));
        }
    }
    println!("single gets x1000: {:.1?}", t.elapsed());
    tick("after single gets");
    let t = std::time::Instant::now();
    for _ in 0..1000 {
        std::hint::black_box(db.games_cached(&ids, false).expect("cached"));
    }
    println!("games_cached fill_cache=false x1000: {:.1?}", t.elapsed());
    tick("after games_cached fill_cache=false");
    let t = std::time::Instant::now();
    for _ in 0..1000 {
        std::hint::black_box(db.games(&ids, false).expect("games"));
    }
    println!("games fill_cache=false x1000: {:.1?}", t.elapsed());
    tick("after games fill_cache=false");
}
