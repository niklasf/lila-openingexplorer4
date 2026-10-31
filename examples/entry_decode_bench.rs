// Times the stages of serving a large entry: the RocksDB get, iterating
// cells, iterating games, building a DecodedEntry, and aggregating from the
// packed and the decoded entry.
//
// usage: entry_decode_bench <db> <play moves, comma separated>...

use std::{env, path::Path, time::Instant};

use lila_openingexplorer4::{
    packed::EntryView,
    server::{
        Database, DbConfig,
        explore::{DecodedEntry, prepare_lichess},
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
        },
    )
    .expect("open db");
    let openings = Openings::default();
    for play in &args[1..] {
        let query = LichessQuery::parse(&RawQuery {
            play: Some(play.clone()),
            ..RawQuery::default()
        })
        .unwrap();
        let pos = query.play.position(&openings).unwrap();
        let hash = position_hash(&pos.pos);

        let t = Instant::now();
        let entry = db.positions_entry(&hash, true).unwrap().expect("entry");
        let get1 = t.elapsed();
        let t = Instant::now();
        let entry2 = db.positions_entry(&hash, true).unwrap().expect("entry");
        let get2 = t.elapsed();
        drop(entry2);
        let bytes: &[u8] = &entry;

        let t = Instant::now();
        let cells = EntryView::unpack_entire(bytes).cells().count();
        let iter_cells = t.elapsed();

        let t = Instant::now();
        let games: usize = EntryView::unpack_entire(bytes)
            .cells()
            .map(|c| c.games.count())
            .sum();
        let iter_games = t.elapsed();

        let t = Instant::now();
        let copy = bytes.to_vec();
        let memcpy = t.elapsed();
        drop(copy);

        let t = Instant::now();
        let decoded = DecodedEntry::new(bytes);
        let decode = t.elapsed();

        let t = Instant::now();
        let packed = prepare_lichess(
            &EntryView::unpack_entire(bytes),
            &query.filter,
            &query.limits,
            false,
        );
        let agg_packed = t.elapsed();

        let t = Instant::now();
        let dec = prepare_lichess(&decoded, &query.filter, &query.limits, false);
        let agg_decoded = t.elapsed();
        assert_eq!(packed.total, dec.total);

        println!(
            "{play}: {} bytes, {cells} cells, {games} games, weight {}\n  get {get1:.1?} (again {get2:.1?}), cells {iter_cells:.1?}, games {iter_games:.1?}, memcpy {memcpy:.1?}, decode {decode:.1?}, aggregate packed {agg_packed:.1?} / decoded {agg_decoded:.1?}",
            bytes.len(),
            decoded.weight(),
        );
    }
}
