use std::{
    io,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use clap::Parser;
use lila_openingexplorer4::{
    packed::{EncodedMoves, huffman_moves},
    parquet::{FastGameColumns, GameRow, PositionalFile},
};
use parquet::file::reader::{FileReader, SerializedFileReader};
use shakmaty::{
    CastlingMode,
    packed::PackedSetup,
    variant::{Variant, VariantPosition},
};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// Measures decoding and re-encoding of the compact move format in a
/// parquet file, and compares its size to packed UCI moves and to Huffman
/// coded legal move ranks.
#[derive(Parser)]
struct Args {
    parquet: PathBuf,
    #[clap(long, default_value = "200")]
    row_groups: usize,
    #[clap(long, default_value = "0")]
    start: usize,
    #[clap(long, default_value = "28")]
    threads: usize,
    /// Also encode with Huffman coded legal move ranks for comparison.
    #[clap(long)]
    huffman: bool,
}

#[derive(Default)]
struct Stats {
    games: u64,
    plies: u64,
    failed: u64,
    compact_bytes: u64,
    huffman_bytes: u64,
    decode_time: Duration,
    encode_time: Duration,
    huffman_time: Duration,
}

fn initial_position(row: &GameRow<'_>) -> Result<VariantPosition, String> {
    match row.setup {
        None => Ok(VariantPosition::new(Variant::Chess)),
        Some(bytes) => {
            let (setup, variant) = PackedSetup::try_from_bytes(bytes)
                .map_err(|e| format!("packed setup: {e}"))?
                .unpack_variant()
                .map_err(|e| format!("unpack variant: {e}"))?;
            VariantPosition::from_setup(variant, setup, CastlingMode::Chess960)
                .map_err(|e| format!("from setup: {e}"))
        }
    }
}

fn check_row(row: &GameRow<'_>, stats: &mut Stats, huffman: bool) -> Result<(), String> {
    let start = initial_position(row)?;
    let encoded = EncodedMoves::from_bytes(row.moves.to_vec());

    let timer = Instant::now();
    let mut pos = start.clone();
    let moves = encoded
        .decode(&mut pos)
        .map_err(|e| format!("decode: {e}"))?;
    stats.decode_time += timer.elapsed();

    let timer = Instant::now();
    let mut pos = start.clone();
    let reencoded = EncodedMoves::encode(&mut pos, moves.iter().copied())
        .map_err(|e| format!("encode: {e}"))?;
    stats.encode_time += timer.elapsed();
    if reencoded != encoded {
        return Err("re-encoding differs".to_owned());
    }

    if huffman {
        let timer = Instant::now();
        let mut pos = start;
        let mut writer = huffman_moves::BitWriter::default();
        huffman_moves::encode_moves(&mut pos, moves.iter().copied(), &mut writer)
            .map_err(|e| format!("huffman: {e}"))?;
        stats.huffman_bytes += writer.finish().len() as u64;
        stats.huffman_time += timer.elapsed();
    }

    stats.games += 1;
    stats.plies += moves.len() as u64;
    stats.compact_bytes += row.moves.len() as u64;
    Ok(())
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let reader = SerializedFileReader::new(PositionalFile::open(&args.parquet)?)?;
    let end = (args.start + args.row_groups).min(reader.num_row_groups());
    let next = AtomicUsize::new(args.start);
    let total = Mutex::new(Stats::default());

    let timer = Instant::now();
    thread::scope(|s| {
        for _ in 0..args.threads {
            s.spawn(|| {
                let mut columns = FastGameColumns::default();
                let mut stats = Stats::default();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= end {
                        break;
                    }
                    columns
                        .read_columns(&*reader.get_row_group(i).expect("row group"))
                        .expect("read");
                    for row in columns.rows() {
                        if let Err(reason) = check_row(&row, &mut stats, args.huffman) {
                            if stats.failed < 3 {
                                eprintln!("{}: {reason}", row.game_id);
                            }
                            stats.failed += 1;
                        }
                    }
                }
                let mut total = total.lock().expect("lock");
                total.games += stats.games;
                total.plies += stats.plies;
                total.failed += stats.failed;
                total.compact_bytes += stats.compact_bytes;
                total.huffman_bytes += stats.huffman_bytes;
                total.decode_time += stats.decode_time;
                total.encode_time += stats.encode_time;
                total.huffman_time += stats.huffman_time;
            });
        }
    });
    let wall = timer.elapsed();
    let stats = total.into_inner().expect("stats");

    println!(
        "{} games, {} plies ({:.1} per game), {} failed; wall {wall:.1?} with {} threads",
        stats.games,
        stats.plies,
        stats.plies as f64 / stats.games.max(1) as f64,
        stats.failed,
        args.threads
    );
    println!(
        "compact: {:.3} bytes per ply ({:.2} bits), {:.1} bytes per game; packed uci would be 2.000 bytes per ply",
        stats.compact_bytes as f64 / stats.plies as f64,
        8.0 * stats.compact_bytes as f64 / stats.plies as f64,
        stats.compact_bytes as f64 / stats.games as f64,
    );
    println!(
        "compact: decode {:.0} ns per ply, encode {:.0} ns per ply (thread time)",
        stats.decode_time.as_nanos() as f64 / stats.plies as f64,
        stats.encode_time.as_nanos() as f64 / stats.plies as f64,
    );
    if args.huffman {
        println!(
            "huffman ranks: {:.3} bytes per ply, encode {:.0} ns per ply",
            stats.huffman_bytes as f64 / stats.plies as f64,
            stats.huffman_time.as_nanos() as f64 / stats.plies as f64,
        );
    }
    Ok(())
}
