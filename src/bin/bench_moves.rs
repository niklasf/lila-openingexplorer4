use std::{
    fs, io,
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
    packed::{BitReader, BitWriter, GameId, decode_moves, encode_uci_moves, pack_uint},
    parquet::{FastGameColumns, GameRow, PositionalFile},
};
use parquet::file::reader::{FileReader, SerializedFileReader};
use rocksdb::{BlockBasedOptions, DBCompressionType, Options, SstFileWriter};
use shakmaty::{
    CastlingMode,
    packed::{PackedSetup, PackedUciMove},
    uci::UciMove,
    variant::{Variant, VariantPosition},
};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// Measures Huffman move compression: encode speed, and SST size compared
/// to the current value encoding.
#[derive(Parser)]
struct Args {
    parquet: PathBuf,
    #[clap(long, default_value = "200")]
    row_groups: usize,
    #[clap(long, default_value = "0")]
    start: usize,
    #[clap(long, default_value = "28")]
    threads: usize,
    /// Directory for the comparison SST files.
    #[clap(long)]
    scratch: PathBuf,
}

#[derive(Default)]
struct Stats {
    games: u64,
    plies: u64,
    skipped: u64,
    old_bytes: u64,
    new_bytes: u64,
    encode_time: Duration,
    decode_time: Duration,
}

/// Encodes the moves of a row. Returns the new value, or `None` if the game
/// can not be replayed.
fn new_value(
    row: &GameRow<'_>,
    old: &[u8],
    plies: usize,
    stats: &mut Stats,
) -> Result<Vec<u8>, String> {
    let mut pos = match row.setup {
        None => VariantPosition::new(Variant::Chess),
        Some(bytes) => {
            let (setup, variant) = PackedSetup::try_from_bytes(bytes)
                .map_err(|e| format!("packed setup: {e}"))?
                .unpack_variant()
                .map_err(|e| format!("unpack variant: {e}"))?;
            VariantPosition::from_setup(variant, setup, CastlingMode::Chess960)
                .map_err(|e| format!("from setup: {e}"))?
        }
    };

    let start_pos = pos.clone();
    let timer = Instant::now();
    let mut writer = BitWriter::default();
    let ucis = row.moves.chunks_exact(PackedUciMove::BYTES).map(|bytes| {
        let uci: UciMove = PackedUciMove::from_bytes(bytes.try_into().expect("2 bytes")).unpack();
        uci
    });
    encode_uci_moves(&mut pos, ucis, &mut writer).map_err(|e| format!("encode: {e}"))?;
    let huffman = writer.finish();
    stats.encode_time += timer.elapsed();

    // Decode again, checking the roundtrip.
    let timer = Instant::now();
    let mut pos = start_pos;
    let mut decoded = Vec::with_capacity(plies);
    decode_moves(&mut pos, &mut BitReader::new(&huffman), plies, &mut decoded)
        .map_err(|e| format!("decode: {e}"))?;
    stats.decode_time += timer.elapsed();
    let mut packed = Vec::with_capacity(row.moves.len());
    for m in decoded {
        packed.extend_from_slice(&PackedUciMove::pack(m.to_uci(CastlingMode::Chess960)).to_bytes());
    }
    if packed != row.moves {
        return Err("roundtrip mismatch".to_owned());
    }

    let header = &old[..old.len() - row.moves.len()];
    let mut value = Vec::with_capacity(header.len() + 2 + huffman.len());
    value.extend_from_slice(header);
    pack_uint(&mut value, plies as u64);
    value.extend_from_slice(&huffman);
    Ok(value)
}

fn write_sst(path: &PathBuf, entries: &[(GameId, Vec<u8>, Vec<u8>)], new: bool) -> u64 {
    let mut opts = Options::default();
    opts.set_compression_type(DBCompressionType::Zstd);
    let mut table = BlockBasedOptions::default();
    table.set_block_size(4096);
    opts.set_block_based_table_factory(&table);
    let mut writer = SstFileWriter::create(&opts);
    writer.open(path).expect("open sst");
    for (game_id, old, new_value) in entries {
        writer
            .put(game_id.to_bytes(), if new { new_value } else { old })
            .expect("put");
    }
    writer.finish().expect("finish");
    let size = writer.file_size();
    fs::remove_file(path).expect("remove sst");
    size
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let reader = SerializedFileReader::new(PositionalFile::open(&args.parquet)?)?;
    let end = (args.start + args.row_groups).min(reader.num_row_groups());
    let next = AtomicUsize::new(args.start);
    let total = Mutex::new(Stats::default());
    let entries: Mutex<Vec<(GameId, Vec<u8>, Vec<u8>)>> = Mutex::new(Vec::new());

    let timer = Instant::now();
    thread::scope(|s| {
        for _ in 0..args.threads {
            s.spawn(|| {
                let mut columns = FastGameColumns::default();
                let mut stats = Stats::default();
                let mut local = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= end {
                        break;
                    }
                    columns
                        .read_columns(&*reader.get_row_group(i).expect("row group"))
                        .expect("read");
                    for row in columns.rows() {
                        let mut old = Vec::new();
                        if row.pack_lichess_game(&mut old).is_err() {
                            continue;
                        }
                        let plies = row.moves.len() / PackedUciMove::BYTES;
                        match new_value(&row, &old, plies, &mut stats) {
                            Ok(new) => {
                                stats.games += 1;
                                stats.plies += plies as u64;
                                stats.old_bytes += old.len() as u64;
                                stats.new_bytes += new.len() as u64;
                                local.push((row.game_id, old, new));
                            }
                            Err(reason) => {
                                if stats.skipped < 3 {
                                    eprintln!("skipping {}: {reason}", row.game_id);
                                }
                                stats.skipped += 1;
                            }
                        }
                    }
                }
                let mut total = total.lock().expect("lock");
                total.games += stats.games;
                total.plies += stats.plies;
                total.skipped += stats.skipped;
                total.old_bytes += stats.old_bytes;
                total.new_bytes += stats.new_bytes;
                total.encode_time += stats.encode_time;
                total.decode_time += stats.decode_time;
                entries.lock().expect("lock").append(&mut local);
            });
        }
    });
    let wall = timer.elapsed();
    let stats = total.into_inner().expect("stats");
    let mut entries = entries.into_inner().expect("entries");
    entries.sort_unstable_by_key(|e| e.0);

    println!(
        "{} games, {} plies ({:.1} per game), {} unreplayable; wall {wall:.1?} with {} threads",
        stats.games,
        stats.plies,
        stats.plies as f64 / stats.games as f64,
        stats.skipped,
        args.threads
    );
    println!(
        "encode: {:.0} ns per ply of thread time ({:.1} M plies/s per thread); decode: {:.0} ns per ply",
        stats.encode_time.as_nanos() as f64 / stats.plies as f64,
        stats.plies as f64 / stats.encode_time.as_secs_f64() / 1e6,
        stats.decode_time.as_nanos() as f64 / stats.plies as f64,
    );
    println!(
        "values: old {} bytes ({:.1} per game, {:.2} per ply for moves), new {} bytes ({:.1} per game, {:.2} per ply for moves)",
        stats.old_bytes,
        stats.old_bytes as f64 / stats.games as f64,
        2.0,
        stats.new_bytes,
        stats.new_bytes as f64 / stats.games as f64,
        (stats.new_bytes as f64 - (stats.old_bytes as f64 - 2.0 * stats.plies as f64))
            / stats.plies as f64,
    );

    let old_sst = write_sst(&args.scratch.join("bench_old.sst"), &entries, false);
    let new_sst = write_sst(&args.scratch.join("bench_new.sst"), &entries, true);
    println!(
        "sst (zstd, 4 KB blocks): old {} bytes ({:.1} per game), new {} bytes ({:.1} per game), {:.1}% smaller",
        old_sst,
        old_sst as f64 / stats.games as f64,
        new_sst,
        new_sst as f64 / stats.games as f64,
        100.0 * (1.0 - new_sst as f64 / old_sst as f64)
    );
    Ok(())
}
