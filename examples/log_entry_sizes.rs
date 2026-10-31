// Distribution of packed entry sizes of the positions requested in a log,
// to size the decoded entry cache: distinct positions and requests per size
// bucket, and how often large positions repeat.
//
// usage: log_entry_sizes <db> <log.zst> <prefix> [limit] [skip]

use std::{
    collections::HashMap,
    env,
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

use lila_openingexplorer4::{
    packed::Zobrist96,
    server::{
        Database, DbConfig,
        openings::Openings,
        query::{LichessQuery, RawQuery},
        replay::position_hash,
    },
};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let db = Database::open_read_only(
        Path::new(&args[0]),
        &DbConfig {
            cache_bytes: 1 << 30,
            lru_cache: true,
            direct_reads: false,
            statistics: false,
            file_opening_threads: 16,
            block_size: 4096,
            pin_metadata: false,
        },
    )
    .expect("open db");
    let prefix = &args[2];
    let limit: usize = args
        .get(3)
        .map_or(usize::MAX, |s| s.parse().expect("limit"));
    let skip: usize = args.get(4).map_or(0, |s| s.parse().expect("skip"));
    let openings = Openings::default();

    let reader = BufReader::new(zstd::Decoder::new(File::open(&args[1]).expect("log")).unwrap());
    let mut sizes: HashMap<Zobrist96, (usize, u64)> = HashMap::new();
    let mut requests = 0;
    let mut distinct_queries: HashMap<String, u64> = HashMap::new();
    for line in reader
        .lines()
        .map(|l| l.unwrap())
        .filter(|l| l.split('?').next() == Some(prefix))
        .skip(skip)
        .take(limit)
    {
        requests += 1;
        *distinct_queries.entry(line.clone()).or_default() += 1;
        let raw: RawQuery = serde_urlencoded::from_str(line.split_once('?').map_or("", |(_, q)| q))
            .unwrap_or_default();
        let Ok(query) = LichessQuery::parse(&raw) else {
            continue;
        };
        let Ok(play) = query.play.position(&openings) else {
            continue;
        };
        let hash = position_hash(&play.pos);
        let entry = sizes.entry(hash).or_insert_with(|| {
            (
                db.positions_entry(&hash, false)
                    .expect("get")
                    .map_or(0, |e| e.len()),
                0,
            )
        });
        entry.1 += 1;
    }
    println!(
        "{requests} requests, {} distinct queries, {} distinct positions",
        distinct_queries.len(),
        sizes.len()
    );
    let buckets = [
        0,
        1 << 10,
        16 << 10,
        64 << 10,
        256 << 10,
        1 << 20,
        4 << 20,
        16 << 20,
        usize::MAX,
    ];
    println!("size bucket        positions  requests  packed bytes");
    for w in buckets.windows(2) {
        let (lo, hi) = (w[0], w[1]);
        let mut positions = 0;
        let mut reqs = 0;
        let mut bytes = 0;
        for (size, count) in sizes.values() {
            if *size >= lo && *size < hi {
                positions += 1;
                reqs += count;
                bytes += size;
            }
        }
        println!(
            "{:>8} - {:<8} {positions:>9} {reqs:>9} {bytes:>13}",
            lo,
            if hi == usize::MAX {
                "inf".to_owned()
            } else {
                hi.to_string()
            }
        );
    }
}
