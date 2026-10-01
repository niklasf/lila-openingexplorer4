use std::{
    io,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use clap::Parser;
use lila_openingexplorer4::{
    packed::{EncodedMoves, GameId},
    parquet::{FastGameColumns, GameRow, PositionalFile},
};
use parquet::file::reader::{FileReader, SerializedFileReader};
use rustc_hash::FxHashMap;
use shakmaty::{
    CastlingMode,
    packed::{PackedSetup, PackedUciMove},
    variant::{Variant, VariantPosition},
};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// Compares a parquet file with compactly encoded moves against row groups
/// of a parquet file with packed UCI moves.
#[derive(Parser)]
struct Args {
    /// Parquet file with packed UCI moves (old format).
    #[clap(long)]
    old: PathBuf,
    #[clap(long)]
    old_start: usize,
    #[clap(long)]
    old_end: usize,
    /// Parquet file with compactly encoded moves (new format).
    #[clap(long)]
    new: PathBuf,
    #[clap(long, default_value = "28")]
    threads: usize,
}

/// All columns of a row except the moves, for comparison.
fn header(row: &GameRow<'_>) -> Vec<u8> {
    let mut buf = Vec::new();
    row.pack_lichess_game(&mut buf).expect("valid row");
    buf.truncate(buf.len() - row.moves.len());
    buf
}

fn initial_position(row: &GameRow<'_>) -> VariantPosition {
    match row.setup {
        None => VariantPosition::new(Variant::Chess),
        Some(bytes) => {
            let (setup, variant) = PackedSetup::try_from_bytes(bytes)
                .expect("packed setup")
                .unpack_variant()
                .expect("variant");
            VariantPosition::from_setup(variant, setup, CastlingMode::Chess960).expect("position")
        }
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    // Load the old games: header bytes and packed uci moves.
    let old = SerializedFileReader::new(PositionalFile::open(&args.old)?)?;
    let next = AtomicUsize::new(args.old_start);
    let games: Mutex<FxHashMap<GameId, (Vec<u8>, Vec<u8>)>> = Mutex::new(FxHashMap::default());
    thread::scope(|s| {
        for _ in 0..args.threads {
            s.spawn(|| {
                let mut columns = FastGameColumns::default();
                let mut local = FxHashMap::default();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= args.old_end.min(old.num_row_groups()) {
                        break;
                    }
                    columns
                        .read_columns(&*old.get_row_group(i).expect("row group"))
                        .expect("read");
                    for row in columns.rows() {
                        if row.players.white.rating.is_none() || row.players.black.rating.is_none()
                        {
                            continue;
                        }
                        local.insert(row.game_id, (header(&row), row.moves.to_vec()));
                    }
                }
                games.lock().expect("lock").extend(local);
            });
        }
    });
    let games = games.into_inner().expect("games");
    println!("loaded {} old games", games.len());

    let new = SerializedFileReader::new(PositionalFile::open(&args.new)?)?;
    let next = AtomicUsize::new(0);
    let seen: Mutex<Vec<GameId>> = Mutex::new(Vec::new());
    let stats = Mutex::new((0u64, 0u64, 0u64)); // matched, missing in old, mismatched
    thread::scope(|s| {
        for _ in 0..args.threads {
            s.spawn(|| {
                let mut columns = FastGameColumns::default();
                let mut local_seen = Vec::new();
                let (mut matched, mut missing, mut mismatched) = (0, 0, 0);
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= new.num_row_groups() {
                        break;
                    }
                    columns
                        .read_columns(&*new.get_row_group(i).expect("row group"))
                        .expect("read");
                    for row in columns.rows() {
                        let Some((old_header, old_moves)) = games.get(&row.game_id) else {
                            missing += 1;
                            if missing <= 3 {
                                eprintln!("{} missing in old", row.game_id);
                            }
                            continue;
                        };
                        local_seen.push(row.game_id);
                        let mut pos = initial_position(&row);
                        let decoded = EncodedMoves::from_bytes(row.moves.to_vec())
                            .decode(&mut pos)
                            .expect("decode");
                        let mut packed = Vec::with_capacity(decoded.len() * 2);
                        for m in decoded {
                            packed.extend_from_slice(
                                &PackedUciMove::pack(m.to_uci(CastlingMode::Chess960)).to_bytes(),
                            );
                        }
                        if &packed != old_moves || header(&row) != *old_header {
                            mismatched += 1;
                            if mismatched <= 3 {
                                eprintln!("{} mismatch", row.game_id);
                            }
                        } else {
                            matched += 1;
                        }
                    }
                }
                let mut stats = stats.lock().expect("lock");
                stats.0 += matched;
                stats.1 += missing;
                stats.2 += mismatched;
                seen.lock().expect("lock").append(&mut local_seen);
            });
        }
    });
    let (matched, missing, mismatched) = stats.into_inner().expect("stats");
    let seen = seen.into_inner().expect("seen");
    let mut unseen_plies = [0u64; 3]; // old games not in new, by plies (0, 1, >=2)
    let seen_set: std::collections::HashSet<GameId> = seen.into_iter().collect();
    for (id, (_, moves)) in &games {
        if !seen_set.contains(id) {
            unseen_plies[(moves.len() / 2).min(2)] += 1;
        }
    }
    println!(
        "{matched} games identical, {mismatched} mismatched, {missing} new games missing in old; old games not in new: {} with 0 plies, {} with 1 ply, {} with 2+ plies",
        unseen_plies[0], unseen_plies[1], unseen_plies[2]
    );
    // Old games with 2+ plies that are not in the new file are expected when
    // the old row group range is wider than the new file.
    assert_eq!(mismatched, 0);
    assert_eq!(missing, 0);
    println!("OK");
    Ok(())
}
