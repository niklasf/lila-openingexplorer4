// Measures the experimental entry format variants (server::entry_v3) on
// real data, without rebuilding anything:
//
// 1. Size: repacks a sample of entries from the positions column family
//    with each variant and reports bytes per original size bucket.
// 2. Serving: replays the requests of a log slice (each with its own
//    filters) through each variant, timing the aggregation and checking that
//    all variants produce the same response parts.
//
// usage: entry_format_bench <db> <log.zst> <prefix> <limit> <skip> [sample keys]

use std::{
    collections::HashMap,
    env,
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    time::{Duration, Instant},
};

use lila_openingexplorer4::{
    packed::{EntryView, LichessEntry, Zobrist96},
    server::{
        Database, DbConfig,
        entry_v3::{Flags, pack, prepare_lichess_v3},
        explore::{GameRef, LichessPrepared, prepare_lichess},
        openings::Openings,
        query::{LichessQuery, RawQuery},
        replay::position_hash,
    },
};

const BUCKETS: [(usize, &str); 5] = [
    (1 << 10, "<1K"),
    (16 << 10, "<16K"),
    (256 << 10, "<256K"),
    (4 << 20, "<4M"),
    (usize::MAX, ">=4M"),
];

fn bucket(size: usize) -> usize {
    BUCKETS.iter().position(|(max, _)| size < *max).unwrap()
}

fn variants() -> Vec<Flags> {
    vec![
        Flags::PACKED,
        Flags {
            lengths: true,
            ..Flags::PACKED
        },
        Flags {
            lengths: true,
            subtotals: true,
            descending: true,
            ..Flags::PACKED
        },
        Flags {
            lengths: true,
            subtotals: true,
            subtotal_min_months: 4,
            descending: true,
            ..Flags::PACKED
        },
        Flags {
            lengths: true,
            subtotals: true,
            subtotal_min_months: 8,
            descending: true,
            ..Flags::PACKED
        },
        Flags {
            lengths: true,
            subtotals: true,
            subtotal_min_months: 16,
            descending: true,
            ..Flags::PACKED
        },
    ]
}

fn repack_all(bytes: &[u8], variants: &[Flags]) -> Vec<Vec<u8>> {
    let mut entry = LichessEntry::new();
    entry.merge_unpack_entire(bytes);
    variants
        .iter()
        .map(|flags| {
            let mut buf = Vec::new();
            pack(&mut buf, entry.cells(), *flags);
            buf
        })
        .collect()
}

fn ids(games: &[GameRef]) -> Vec<String> {
    games.iter().map(|g| g.id.to_string()).collect()
}

fn same(a: &LichessPrepared, b: &LichessPrepared) -> bool {
    a.total == b.total
        && a.moves.len() == b.moves.len()
        && a.moves
            .iter()
            .zip(&b.moves)
            .all(|(x, y)| (x.m, x.stats, x.game) == (y.m, y.stats, y.game))
        && ids(&a.top_games) == ids(&b.top_games)
        && ids(&a.recent_games) == ids(&b.recent_games)
        && a.history.as_ref().map(|h| h.len()) == b.history.as_ref().map(|h| h.len())
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let db = Database::open_read_only(
        Path::new(&args[0]),
        &DbConfig {
            cache_bytes: 4 << 30,
            lru_cache: true,
            direct_reads: false,
            statistics: false,
            file_opening_threads: 16,
            block_size: 4096,
        },
    )
    .expect("open db");
    let prefix = &args[2];
    let limit: usize = args[3].parse().expect("limit");
    let skip: usize = args[4].parse().expect("skip");
    let sample_keys: usize = args.get(5).map_or(0, |s| s.parse().expect("sample keys"));
    let variants = variants();

    if sample_keys > 0 {
        let started = Instant::now();
        let entries = db.scan_positions(sample_keys).expect("scan");
        let mut sizes = vec![vec![0u64; BUCKETS.len()]; variants.len()];
        let mut counts = vec![0u64; BUCKETS.len()];
        for bytes in &entries {
            let b = bucket(bytes.len());
            counts[b] += 1;
            for (v, repacked) in repack_all(bytes, &variants).iter().enumerate() {
                sizes[v][b] += repacked.len() as u64;
            }
        }
        println!(
            "== size of {} sampled positions entries (repacked in {:.1?}) ==",
            entries.len(),
            started.elapsed()
        );
        print_sizes(&variants, &sizes, &counts);
    }

    // Requests of the log slice.
    let openings = Openings::default();
    let reader = BufReader::new(zstd::Decoder::new(File::open(&args[1]).expect("log")).unwrap());
    let mut requests = Vec::new();
    for line in reader
        .lines()
        .map(|l| l.unwrap())
        .filter(|l| l.split('?').next() == Some(prefix))
        .skip(skip)
        .take(limit)
    {
        let raw: RawQuery = serde_urlencoded::from_str(line.split_once('?').map_or("", |(_, q)| q))
            .unwrap_or_default();
        let Ok(query) = LichessQuery::parse(&raw) else {
            continue;
        };
        let Ok(play) = query.play.position(&openings) else {
            continue;
        };
        requests.push((position_hash(&play.pos), query));
    }

    // Entries of the requested positions, in every variant.
    let started = Instant::now();
    let mut entries: HashMap<Zobrist96, Vec<Vec<u8>>> = HashMap::new();
    let mut sizes = vec![vec![0u64; BUCKETS.len()]; variants.len()];
    let mut counts = vec![0u64; BUCKETS.len()];
    for (hash, _) in &requests {
        if entries.contains_key(hash) {
            continue;
        }
        let bytes = db
            .positions_entry(hash, false)
            .expect("get")
            .map_or_else(Vec::new, |e| e.to_vec());
        let b = bucket(bytes.len());
        counts[b] += 1;
        let repacked: Vec<Vec<u8>> = if bytes.is_empty() {
            vec![Vec::new(); variants.len()]
        } else {
            repack_all(&bytes, &variants)
        };
        for (v, r) in repacked.iter().enumerate() {
            sizes[v][b] += r.len() as u64;
        }
        entries.insert(*hash, repacked);
    }
    println!(
        "\n== size of the {} positions of {} log requests (prepared in {:.1?}) ==",
        entries.len(),
        requests.len(),
        started.elapsed()
    );
    print_sizes(&variants, &sizes, &counts);

    // Serving time per variant and bucket (the first variant is the packed
    // format read by the production reader, then every variant through the
    // experimental reader).
    let mut times = vec![vec![Duration::ZERO; BUCKETS.len()]; variants.len() + 1];
    let mut request_counts = vec![0u64; BUCKETS.len()];
    let mut mismatches = vec![0u64; variants.len()];
    for (hash, query) in &requests {
        let repacked = &entries[hash];
        let b = bucket(repacked[0].len());
        request_counts[b] += 1;
        let t = Instant::now();
        let expected = prepare_lichess(
            &EntryView::unpack_entire(&repacked[0]),
            &query.filter,
            &query.limits,
            query.history,
        );
        times[0][b] += t.elapsed();
        for (v, flags) in variants.iter().enumerate() {
            let t = Instant::now();
            let got = prepare_lichess_v3(
                &repacked[v],
                *flags,
                &query.filter,
                &query.limits,
                query.history,
            );
            times[v + 1][b] += t.elapsed();
            if !same(&got, &expected) {
                mismatches[v] += 1;
            }
        }
    }
    println!("\n== aggregation time per request (us) by original entry size ==");
    print!("{:<10}", "variant");
    for (b, (_, name)) in BUCKETS.iter().enumerate() {
        print!("{:>14}", format!("{name} ({})", request_counts[b]));
    }
    println!("{:>12}", "total ms");
    let mut names = vec!["prod".to_owned()];
    names.extend(variants.iter().map(Flags::name));
    for (v, name) in names.iter().enumerate() {
        print!("{name:<10}");
        for b in 0..BUCKETS.len() {
            let us = times[v][b].as_secs_f64() * 1e6 / request_counts[b].max(1) as f64;
            print!("{us:>14.1}");
        }
        let total: Duration = times[v].iter().sum();
        println!("{:>12.1}", total.as_secs_f64() * 1e3);
    }
    for (v, flags) in variants.iter().enumerate() {
        if mismatches[v] > 0 {
            println!("MISMATCHES {}: {}", flags.name(), mismatches[v]);
        }
    }
}

fn print_sizes(variants: &[Flags], sizes: &[Vec<u64>], counts: &[u64]) {
    print!("{:<10}", "variant");
    for (b, (_, name)) in BUCKETS.iter().enumerate() {
        print!("{:>16}", format!("{name} ({})", counts[b]));
    }
    println!("{:>14}", "total");
    for (v, flags) in variants.iter().enumerate() {
        print!("{:<10}", flags.name());
        let total: u64 = sizes[v].iter().sum();
        for b in 0..BUCKETS.len() {
            let base = sizes[0][b].max(1) as f64;
            print!(
                "{:>16}",
                format!(
                    "{} ({:+.1}%)",
                    sizes[v][b],
                    (sizes[v][b] as f64 / base - 1.0) * 100.0
                )
            );
        }
        println!(
            "{:>14}",
            format!(
                "{total} ({:+.1}%)",
                (total as f64 / sizes[0].iter().sum::<u64>().max(1) as f64 - 1.0) * 100.0
            )
        );
    }
}
