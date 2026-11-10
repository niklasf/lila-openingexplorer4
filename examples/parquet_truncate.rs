// Converts a parquet file of pgn_to_parquet (with all moves) into one with
// only the first moves of each game, as `pgn_to_parquet --max-ply` would
// have written it (except that games with illegal moves after the cut were
// already skipped). Row groups and row order are kept, so game indexes stay
// the same.
//
// usage: parquet_truncate <in.parquet> <out.parquet> [max_ply=50] [threads=28]
//        [zstd_level=9] [moves_zstd_level=0]

use std::{
    collections::BTreeMap,
    fs::File,
    io::BufWriter,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Instant,
};

use bytes::Bytes;
use crossbeam::channel;
use lila_openingexplorer4::{
    packed::PackedMoves,
    parquet::{GameColumns, Input},
    pgn_to_parquet::{append_row_group, writer_properties},
    util::progress_bar,
};
use parquet::file::writer::SerializedFileWriter;
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

fn main() {
    let usage = "usage: parquet_truncate <in.parquet> <out.parquet> [max_ply=50] [threads=28] [zstd_level=9] [moves_zstd_level=0]";
    let mut args = std::env::args().skip(1);
    let input_path = PathBuf::from(args.next().expect(usage));
    let out_path = PathBuf::from(args.next().expect(usage));
    let max_ply: usize = args.next().map_or(50, |s| s.parse().expect("max_ply"));
    let threads: usize = args.next().map_or(16, |s| s.parse().expect("threads"));
    let zstd_level: i32 = args.next().map_or(9, |s| s.parse().expect("zstd_level"));
    let moves_zstd_level: i32 = args
        .next()
        .map_or(0, |s| s.parse().expect("moves_zstd_level"));
    assert!(max_ply >= 2, "max_ply must be at least 2");
    assert_ne!(input_path, out_path);

    let input = Input::open(&[input_path], 0, None, true).expect("open input");
    let properties = writer_properties(zstd_level, moves_zstd_level);
    let mut writer = SerializedFileWriter::new(
        BufWriter::with_capacity(4 << 20, File::create(&out_path).expect("create output")),
        GameColumns::schema(),
        Arc::clone(&properties),
    )
    .expect("writer");

    let started = Instant::now();
    let num_row_groups = input.num_row_groups();
    let progress = progress_bar(input.total_rows());
    let next = AtomicUsize::new(0);
    // Bounds the encoded row groups waiting to be appended in order.
    let window = 4 * threads;
    let (token_tx, token_rx) = channel::bounded::<()>(window);
    for _ in 0..window {
        token_tx.send(()).expect("token");
    }
    let (tx, rx) = channel::unbounded::<(usize, Bytes, usize)>();

    let (games, truncated) = thread::scope(|s| {
        for _ in 0..threads.max(1) {
            let tx = tx.clone();
            let token_rx = token_rx.clone();
            let (input, properties, next) = (&input, &properties, &next);
            s.spawn(move || {
                let mut columns = GameColumns::default();
                while token_rx.recv().is_ok() {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= num_row_groups {
                        break;
                    }
                    let batch = input
                        .fetch(i)
                        .and_then(|bytes| input.read_games_from(i, bytes))
                        .expect("read row group");
                    columns.clear();
                    columns.reserve(batch.len());
                    let mut truncated = 0;
                    for (id, game) in batch.rows() {
                        let moves =
                            PackedMoves::truncated(game.moves, game.initial_position(), max_ply)
                                .expect("valid moves");
                        if moves.0.len() < game.moves.len() {
                            truncated += 1;
                        }
                        let mut game = game.into_owned();
                        game.moves = moves;
                        columns.push(id, game);
                    }
                    let mut w = SerializedFileWriter::new(
                        Vec::new(),
                        GameColumns::schema(),
                        Arc::clone(properties),
                    )
                    .expect("writer");
                    if !columns.is_empty() {
                        columns.write_row_group(&mut w).expect("write row group");
                    }
                    let encoded = Bytes::from(w.into_inner().expect("encode"));
                    if tx.send((i, encoded, truncated)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);

        let mut pending = BTreeMap::new();
        let mut written = 0;
        let (mut games, mut truncated) = (0u64, 0u64);
        while written < num_row_groups {
            let (i, encoded, t) = rx.recv().expect("workers alive");
            pending.insert(i, (encoded, t));
            while let Some((encoded, t)) = pending.remove(&written) {
                let rows = input.num_rows(written) as usize;
                append_row_group(&mut writer, &encoded, &vec![true; rows]).expect("append");
                games += rows as u64;
                truncated += t as u64;
                progress.inc(rows as u64);
                written += 1;
                let _ = token_tx.send(());
            }
        }
        drop(token_tx);
        (games, truncated)
    });

    writer.close().expect("close output");
    progress.finish_and_clear();
    let in_len = std::fs::metadata(std::env::args().nth(1).unwrap())
        .unwrap()
        .len();
    let out_len = std::fs::metadata(&out_path).unwrap().len();
    println!(
        "{games} games in {num_row_groups} row groups, {truncated} truncated to {max_ply} plies, {in_len} -> {out_len} bytes ({:.1}%), {:.1?}",
        100.0 * out_len as f64 / in_len as f64,
        started.elapsed()
    );
}
