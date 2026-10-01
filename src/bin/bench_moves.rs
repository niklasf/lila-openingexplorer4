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
    packed::{
        BitReader, BitWriter, CompactSymbols, GameId, decode_compact, decode_moves,
        decode_reachable, encode_compact, encode_reachable, encode_uci_moves, pack_uint,
    },
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
    /// Index destinations among reachable squares instead of fixed codes.
    #[clap(long)]
    reachable: bool,
}

/// Histograms of compact encoding symbols, to estimate entropy coding gains.
#[derive(Clone)]
struct Histograms {
    /// Piece index by field width.
    piece: Vec<Vec<u64>>,
    /// Destination code by role (index 6 for drops).
    dest: Vec<Vec<u64>>,
}

impl Default for Histograms {
    fn default() -> Self {
        Histograms {
            piece: (0..8).map(|bits| vec![0; 1 << bits]).collect(),
            dest: (0..7).map(|_| vec![0; 512]).collect(),
        }
    }
}

impl Histograms {
    fn observe(&mut self, s: CompactSymbols) {
        self.piece[s.piece_bits as usize][s.piece_index as usize] += 1;
        let role = s.role.map_or(6, |r| match r {
            shakmaty::Role::Pawn => 0,
            shakmaty::Role::Knight => 1,
            shakmaty::Role::Bishop => 2,
            shakmaty::Role::Rook => 3,
            shakmaty::Role::Queen => 4,
            shakmaty::Role::King => 5,
        });
        self.dest[role][s.dest_code as usize] += 1;
    }

    fn merge(&mut self, other: &Histograms) {
        for (a, b) in self.piece.iter_mut().zip(&other.piece) {
            for (x, y) in a.iter_mut().zip(b) {
                *x += y;
            }
        }
        for (a, b) in self.dest.iter_mut().zip(&other.dest) {
            for (x, y) in a.iter_mut().zip(b) {
                *x += y;
            }
        }
    }

    /// Total entropy in bits of a histogram, if coded with an ideal static
    /// code conditioned on the context.
    fn entropy(counts: &[u64]) -> f64 {
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return 0.0;
        }
        counts
            .iter()
            .filter(|&&c| c > 0)
            .map(|&c| {
                let p = c as f64 / total as f64;
                -(c as f64) * p.log2()
            })
            .sum()
    }

    fn report(&self, plies: u64) {
        let piece_fixed: u64 = self
            .piece
            .iter()
            .enumerate()
            .map(|(bits, h)| bits as u64 * h.iter().sum::<u64>())
            .sum();
        let piece_entropy: f64 = self.piece.iter().map(|h| Self::entropy(h)).sum();
        let dest_fixed: f64 = self
            .dest
            .iter()
            .enumerate()
            .map(|(role, h)| {
                // Widths as encoded: pawn mixes 2 and 5 bit codes, approximate
                // by counting codes >= 4 as promotions.
                h.iter()
                    .enumerate()
                    .map(|(code, &c)| {
                        let bits = match role {
                            0 => {
                                if code >= 4 {
                                    5
                                } else {
                                    2
                                }
                            }
                            1 | 5 => 3,
                            2 | 3 => 4,
                            4 => 5,
                            _ => 9,
                        };
                        bits as f64 * c as f64
                    })
                    .sum::<f64>()
            })
            .sum();
        let dest_entropy: f64 = self.dest.iter().map(|h| Self::entropy(h)).sum();
        println!(
            "compact fields per ply: piece {:.2} bits fixed / {:.2} entropy; destination {:.2} bits fixed / {:.2} entropy; total {:.2} fixed / {:.2} entropy ({:.3} bytes)",
            piece_fixed as f64 / plies as f64,
            piece_entropy / plies as f64,
            dest_fixed / plies as f64,
            dest_entropy / plies as f64,
            (piece_fixed as f64 + dest_fixed) / plies as f64,
            (piece_entropy + dest_entropy) / plies as f64,
            (piece_entropy + dest_entropy) / plies as f64 / 8.0,
        );
    }
}

#[derive(Default)]
struct Stats {
    games: u64,
    plies: u64,
    skipped: u64,
    old_bytes: u64,
    new_bytes: u64,
    compact_bytes: u64,
    encode_time: Duration,
    decode_time: Duration,
    compact_encode_time: Duration,
    compact_decode_time: Duration,
    histograms: Histograms,
}

/// Encodes the moves of a row. Returns the new value, or `None` if the game
/// can not be replayed.
fn new_value(
    row: &GameRow<'_>,
    old: &[u8],
    plies: usize,
    stats: &mut Stats,
    reachable: bool,
) -> Result<(Vec<u8>, Vec<u8>), String> {
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
    let mut pos = start_pos.clone();
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

    // Compact encoding.
    let timer = Instant::now();
    let mut pos = start_pos.clone();
    let mut writer = BitWriter::default();
    let ucis = row.moves.chunks_exact(PackedUciMove::BYTES).map(|bytes| {
        let uci: UciMove = PackedUciMove::from_bytes(bytes.try_into().expect("2 bytes")).unpack();
        uci
    });
    if reachable {
        encode_reachable(&mut pos, ucis, &mut writer, |symbols| {
            stats.histograms.observe(symbols)
        })
    } else {
        encode_compact(&mut pos, ucis, &mut writer, |symbols| {
            stats.histograms.observe(symbols)
        })
    }
    .map_err(|e| format!("compact encode: {e}"))?;
    let compact = writer.finish();
    stats.compact_encode_time += timer.elapsed();

    let timer = Instant::now();
    let mut pos = start_pos;
    let mut decoded = Vec::with_capacity(plies);
    if reachable {
        decode_reachable(&mut pos, &mut BitReader::new(&compact), plies, &mut decoded)
    } else {
        decode_compact(&mut pos, &mut BitReader::new(&compact), plies, &mut decoded)
    }
    .map_err(|e| format!("compact decode: {e}"))?;
    stats.compact_decode_time += timer.elapsed();
    let mut packed = Vec::with_capacity(row.moves.len());
    for m in decoded {
        packed.extend_from_slice(&PackedUciMove::pack(m.to_uci(CastlingMode::Chess960)).to_bytes());
    }
    if packed != row.moves {
        return Err("compact roundtrip mismatch".to_owned());
    }

    let header = &old[..old.len() - row.moves.len()];
    let mut value = Vec::with_capacity(header.len() + 2 + huffman.len());
    value.extend_from_slice(header);
    pack_uint(&mut value, plies as u64);
    value.extend_from_slice(&huffman);
    let mut compact_value = Vec::with_capacity(header.len() + 2 + compact.len());
    compact_value.extend_from_slice(header);
    pack_uint(&mut compact_value, plies as u64);
    compact_value.extend_from_slice(&compact);
    Ok((value, compact_value))
}

fn write_sst(path: &PathBuf, entries: &[(GameId, Vec<u8>, Vec<u8>, Vec<u8>)], which: usize) -> u64 {
    let mut opts = Options::default();
    opts.set_compression_type(DBCompressionType::Zstd);
    let mut table = BlockBasedOptions::default();
    table.set_block_size(4096);
    opts.set_block_based_table_factory(&table);
    let mut writer = SstFileWriter::create(&opts);
    writer.open(path).expect("open sst");
    for (game_id, old, new_value, compact) in entries {
        writer
            .put(
                game_id.to_bytes(),
                match which {
                    0 => old,
                    1 => new_value,
                    _ => compact,
                },
            )
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
    let entries: Mutex<Vec<(GameId, Vec<u8>, Vec<u8>, Vec<u8>)>> = Mutex::new(Vec::new());

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
                        match new_value(&row, &old, plies, &mut stats, args.reachable) {
                            Ok((new, compact)) => {
                                stats.games += 1;
                                stats.plies += plies as u64;
                                stats.old_bytes += old.len() as u64;
                                stats.new_bytes += new.len() as u64;
                                stats.compact_bytes += compact.len() as u64;
                                local.push((row.game_id, old, new, compact));
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
                total.compact_bytes += stats.compact_bytes;
                total.encode_time += stats.encode_time;
                total.decode_time += stats.decode_time;
                total.compact_encode_time += stats.compact_encode_time;
                total.compact_decode_time += stats.compact_decode_time;
                total.histograms.merge(&stats.histograms);
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
        "huffman ranks: encode {:.0} ns per ply, decode {:.0} ns per ply; compact: encode {:.0} ns per ply, decode {:.0} ns per ply",
        stats.encode_time.as_nanos() as f64 / stats.plies as f64,
        stats.decode_time.as_nanos() as f64 / stats.plies as f64,
        stats.compact_encode_time.as_nanos() as f64 / stats.plies as f64,
        stats.compact_decode_time.as_nanos() as f64 / stats.plies as f64,
    );
    let header_bytes = stats.old_bytes as f64 - 2.0 * stats.plies as f64;
    println!(
        "compact moves: {:.3} bytes per ply ({:.1} bits), value {:.1} bytes per game",
        (stats.compact_bytes as f64 - header_bytes) / stats.plies as f64,
        8.0 * (stats.compact_bytes as f64 - header_bytes) / stats.plies as f64,
        stats.compact_bytes as f64 / stats.games as f64,
    );
    stats.histograms.report(stats.plies);
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

    let old_sst = write_sst(&args.scratch.join("bench_old.sst"), &entries, 0);
    let new_sst = write_sst(&args.scratch.join("bench_new.sst"), &entries, 1);
    let compact_sst = write_sst(&args.scratch.join("bench_compact.sst"), &entries, 2);
    println!(
        "sst (zstd, 4 KB blocks): current {:.1} bytes per game; huffman ranks {:.1} ({:.1}% smaller); compact {:.1} ({:.1}% smaller)",
        old_sst as f64 / stats.games as f64,
        new_sst as f64 / stats.games as f64,
        100.0 * (1.0 - new_sst as f64 / old_sst as f64),
        compact_sst as f64 / stats.games as f64,
        100.0 * (1.0 - compact_sst as f64 / old_sst as f64)
    );
    Ok(())
}
