// Times the stages of serving a large entry: the RocksDB get, iterating
// groups, cells and games, and aggregating with the query's filter and
// with all groups selected.
//
// usage: entry_decode_bench <db> <play moves, comma separated>...

use std::{env, path::Path, time::Instant};

use lila_openingexplorer4::{
    packed::EntryView,
    server::{
        Database, DbConfig,
        explore::prepare_lichess,
        openings::Openings,
        query::{LichessFilter, LichessQuery, RawQuery},
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
        let groups = EntryView::unpack_entire(bytes).groups().count();
        let iter_groups = t.elapsed();

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
        let filtered = prepare_lichess(bytes, &query.filter, &query.limits, false);
        let agg_filtered = t.elapsed();

        let t = Instant::now();
        let all = LichessFilter {
            speeds: None,
            ratings: None,
            since: None,
            until: None,
        };
        let unfiltered = prepare_lichess(bytes, &all, &query.limits, false);
        let agg_all = t.elapsed();

        println!(
            "{play}: {} bytes, {groups} groups, {cells} cells, {games} games, {} / {} games selected\n  get {get1:.1?} (again {get2:.1?}), groups {iter_groups:.1?}, cells {iter_cells:.1?}, games {iter_games:.1?}, memcpy {memcpy:.1?}, aggregate filtered {agg_filtered:.1?} / all {agg_all:.1?}",
            bytes.len(),
            filtered.total.games(),
            unfiltered.total.games(),
        );
    }
}
