// Replays games from a parquet file and reports statistics relevant to
// parquet_to_positions: entries per game after loop removal, replay cost, and
// the frequency distribution of positions (how many positions are "hot").

use std::{
    env,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    thread,
    time::Instant,
};

use lila_openingexplorer4::parquet::{GameReader, PositionalFile};
use nohash_hasher::IntMap;
use rustc_hash::FxHashMap;
use shakmaty::{
    CastlingMode, EnPassantMode, Position,
    packed::PackedSetup,
    variant::{Variant, VariantPosition},
    zobrist::Zobrist128,
};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

const MAX_COUNTED_PLY: usize = 40;

fn main() {
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .expect("usage: position_stats <parquet> [row_groups] [stride] [threads] [count]");
    let row_groups: usize = args.next().map_or(32, |s| s.parse().unwrap());
    let stride: usize = args.next().map_or(1, |s| s.parse().unwrap());
    let threads: usize = args.next().map_or(16, |s| s.parse().unwrap());
    let count = args.next().is_some_and(|s| s == "count");

    let reader = GameReader::try_new(PositionalFile::open(&path).unwrap()).unwrap();
    let num_row_groups = reader.num_row_groups();
    let selected: Vec<usize> = (0..num_row_groups)
        .step_by(stride)
        .take(row_groups)
        .collect();

    let next = AtomicUsize::new(0);
    let games = AtomicU64::new(0);
    let skipped = AtomicU64::new(0);
    let plies = AtomicU64::new(0);
    let entries = AtomicU64::new(0);
    let loops = AtomicU64::new(0);
    let incremental = AtomicU64::new(0);
    let replay_nanos = AtomicU64::new(0);
    let decode_nanos = AtomicU64::new(0);
    let started = Instant::now();

    let counts: Vec<IntMap<u64, u32>> = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut counts: IntMap<u64, u32> = IntMap::default();
                    let mut seen: IntMap<u64, usize> = IntMap::default();
                    let mut stack: Vec<u64> = Vec::new();
                    let (mut local_games, mut local_skipped) = (0u64, 0u64);
                    let (mut local_plies, mut local_entries, mut local_loops) = (0u64, 0u64, 0u64);
                    let mut local_incremental = 0u64;
                    let mut local_replay = 0u64;
                    let mut local_decode = 0u64;
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&rg) = selected.get(i) else { break };
                        let timer = Instant::now();
                        let batch = reader.read_games(rg).unwrap();
                        local_decode += timer.elapsed().as_nanos() as u64;
                        let timer = Instant::now();
                        for row in batch.rows() {
                            let mut pos = match row.setup {
                                None => VariantPosition::new(Variant::Chess),
                                Some(bytes) => {
                                    let Ok(setup) = PackedSetup::try_from_bytes(bytes) else {
                                        local_skipped += 1;
                                        continue;
                                    };
                                    let Ok((setup, variant)) = setup.unpack_variant() else {
                                        local_skipped += 1;
                                        continue;
                                    };
                                    match VariantPosition::from_setup(
                                        variant,
                                        setup,
                                        CastlingMode::Chess960,
                                    ) {
                                        Ok(pos) => pos,
                                        Err(_) => {
                                            local_skipped += 1;
                                            continue;
                                        }
                                    }
                                }
                            };
                            let packed =
                                lila_openingexplorer4::packed::PackedMoves(row.moves.to_vec());
                            let Ok(mut decoder) = packed.decode() else {
                                local_skipped += 1;
                                continue;
                            };
                            local_games += 1;
                            seen.clear();
                            stack.clear();
                            let mut hash: Zobrist128 = pos.zobrist_hash(EnPassantMode::Legal);
                            while let Some(m) = decoder.decode_next(&pos).unwrap() {
                                local_plies += 1;
                                let key = (u128::from(hash) >> 64) as u64;
                                if let Some(&i) = seen.get(&key) {
                                    local_loops += 1;
                                    for h in stack.drain(i..) {
                                        seen.remove(&h);
                                    }
                                }
                                seen.insert(key, stack.len());
                                stack.push(key);
                                hash = match pos.update_zobrist_hash(hash, m, EnPassantMode::Legal)
                                {
                                    Some(h) => {
                                        local_incremental += 1;
                                        pos.play_unchecked(m);
                                        h
                                    }
                                    None => {
                                        pos.play_unchecked(m);
                                        pos.zobrist_hash(EnPassantMode::Legal)
                                    }
                                };
                            }
                            local_entries += stack.len() as u64;
                            if count {
                                for &key in stack.iter().take(MAX_COUNTED_PLY) {
                                    *counts.entry(key).or_insert(0) += 1;
                                }
                            }
                        }
                        local_replay += timer.elapsed().as_nanos() as u64;
                    }
                    games.fetch_add(local_games, Ordering::Relaxed);
                    skipped.fetch_add(local_skipped, Ordering::Relaxed);
                    plies.fetch_add(local_plies, Ordering::Relaxed);
                    entries.fetch_add(local_entries, Ordering::Relaxed);
                    loops.fetch_add(local_loops, Ordering::Relaxed);
                    incremental.fetch_add(local_incremental, Ordering::Relaxed);
                    replay_nanos.fetch_add(local_replay, Ordering::Relaxed);
                    decode_nanos.fetch_add(local_decode, Ordering::Relaxed);
                    counts
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let elapsed = started.elapsed();
    let games = games.into_inner();
    let plies = plies.into_inner();
    let entries = entries.into_inner();
    println!(
        "{} row groups, {games} games ({} skipped), {plies} plies, {entries} entries ({:.2} per game), {} loops, {:.1}% incremental hashes",
        selected.len(),
        skipped.into_inner(),
        entries as f64 / games as f64,
        loops.into_inner(),
        100.0 * incremental.into_inner() as f64 / plies as f64,
    );
    println!(
        "wall {elapsed:.1?}; per thread: decode {:.0} ns/game, replay {:.1} ns/ply (incl. loop removal and counting)",
        decode_nanos.into_inner() as f64 / games as f64,
        replay_nanos.into_inner() as f64 / plies as f64,
    );

    // Merge the per thread counts. Positions are counted at most up to
    // MAX_COUNTED_PLY plies deep.
    let timer = Instant::now();
    let mut merged: FxHashMap<u64, u32> = FxHashMap::default();
    for counts in counts {
        for (key, count) in counts {
            *merged.entry(key).or_insert(0) += count;
        }
    }
    println!(
        "{} distinct positions within {MAX_COUNTED_PLY} plies (merged in {:.1?})",
        merged.len(),
        timer.elapsed()
    );
    let mut counts: Vec<u32> = merged.into_values().collect();
    counts.sort_unstable_by(|a, b| b.cmp(a));
    let total_counted: u64 = counts.iter().map(|&c| c as u64).sum();
    println!(
        "{} entries counted; count of the top positions: {:?}",
        total_counted,
        &counts[..counts.len().min(16)]
    );
    for &fraction in &[0.1, 0.03, 0.01, 0.003, 0.001, 0.0003, 0.0001, 0.00001] {
        let threshold = (games as f64 * fraction) as u32;
        let hot = counts.partition_point(|&c| c >= threshold.max(2));
        let hot_entries: u64 = counts[..hot].iter().map(|&c| c as u64).sum();
        println!(
            "positions with >= {fraction:.5} of games ({threshold} games): {hot} ({:.2}% of entries)",
            100.0 * hot_entries as f64 / entries as f64
        );
    }
    let singles = counts.iter().rev().take_while(|&&c| c == 1).count();
    println!(
        "positions seen once within {MAX_COUNTED_PLY} plies: {singles} ({:.1}% of counted entries)",
        100.0 * singles as f64 / total_counted as f64
    );
}
