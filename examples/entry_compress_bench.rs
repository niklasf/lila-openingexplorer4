// Measures how well scatter records of parquet_to_positions compress with
// zstd, in a few layouts: as produced, sorted by hash within a chunk, and
// transposed (column major within a chunk).

use std::{env, time::Instant};

use lila_openingexplorer4::{
    packed::{GameId, PackedMoves, Rating, Speed},
    parquet::{GameReader, PositionalFile},
};
use shakmaty::{
    CastlingMode, EnPassantMode, Outcome, Position,
    packed::{PackedSetup, PackedUciMove},
    variant::{Variant, VariantPosition},
    zobrist::Zobrist128,
};

const RECORD: usize = 25;
const CHUNK_RECORDS: usize = (4 << 20) / RECORD;

fn main() {
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .expect("usage: entry_compress_bench <parquet> [row_groups] [partition_bits]");
    let row_groups: usize = args.next().map_or(64, |s| s.parse().unwrap());
    let partition_bits: u32 = args.next().map_or(7, |s| s.parse().unwrap());

    let reader = GameReader::try_new(PositionalFile::open(&path).unwrap()).unwrap();
    let mut records: Vec<[u8; RECORD]> = Vec::new();
    for rg in 0..row_groups.min(reader.num_row_groups()) {
        let batch = reader.read_games(rg).unwrap();
        for row in batch.rows() {
            let mut pos = match row.setup {
                None => VariantPosition::new(Variant::Chess),
                Some(bytes) => {
                    let (setup, variant) = PackedSetup::try_from_bytes(bytes)
                        .unwrap()
                        .unpack_variant()
                        .unwrap();
                    VariantPosition::from_setup(variant, setup, CastlingMode::Chess960).unwrap()
                }
            };
            let (Some(year), Some(month)) = (row.date_year, row.date_month) else {
                continue;
            };
            let month = ((year - 1900) * 12 + month - 1) as u16;
            let (Some(wr), Some(br)) = (row.players.white.rating, row.players.black.rating) else {
                continue;
            };
            let rating = Rating::try_from((wr + br) / 2).unwrap();
            let Ok(speed) = Speed::from_ascii(row.speed.unwrap_or(b"")) else {
                continue;
            };
            let Ok(Outcome::Known(outcome)) = Outcome::from_ascii(row.outcome) else {
                continue;
            };
            let packed = PackedMoves(row.moves.to_vec());
            let mut decoder = packed.decode().unwrap();
            let mut hash: Zobrist128 = pos.zobrist_hash(EnPassantMode::Legal);
            while let Some(m) = decoder.decode_next(&pos).unwrap() {
                let h = u128::from(hash);
                if (h >> (128 - partition_bits)) == 0 {
                    let mut rec = [0u8; RECORD];
                    rec[..12].copy_from_slice(&h.to_be_bytes()[..12]);
                    rec[12..14].copy_from_slice(&month.to_be_bytes());
                    rec[14..16].copy_from_slice(
                        &PackedUciMove::pack(m.to_uci(CastlingMode::Chess960)).to_bytes(),
                    );
                    rec[16] = speed.bits()
                        | lila_openingexplorer4::packed::RatingGroup::select(rating) as u8;
                    let ro = (i32::from(rating) - 1500) as i16;
                    let packed_ro = ((ro as u16) << 2)
                        | lila_openingexplorer4::packed::outcome_bits(Outcome::Known(outcome))
                            as u16;
                    rec[17..19].copy_from_slice(&packed_ro.to_be_bytes());
                    rec[19..25].copy_from_slice(&GameId::to_bytes(row.game_id));
                    records.push(rec);
                }
                hash = match pos.update_zobrist_hash(hash, m, EnPassantMode::Legal) {
                    Some(h) => {
                        pos.play_unchecked(m);
                        h
                    }
                    None => {
                        pos.play_unchecked(m);
                        pos.zobrist_hash(EnPassantMode::Legal)
                    }
                };
            }
        }
    }
    println!(
        "{} records ({} MB)",
        records.len(),
        records.len() * RECORD >> 20
    );

    for level in [1, 3] {
        for layout in [
            "raw",
            "transposed",
            "sorted+transposed",
            "radix16+transposed",
            "radix8+transposed",
        ] {
            let mut compressor = zstd::bulk::Compressor::new(level).unwrap();
            let mut out = vec![0u8; zstd::zstd_safe::compress_bound(CHUNK_RECORDS * RECORD)];
            let mut total = 0usize;
            let mut raw = 0usize;
            let timer = Instant::now();
            let mut prep = 0u128;
            for chunk in records.chunks(CHUNK_RECORDS) {
                let t = Instant::now();
                let mut buf: Vec<[u8; RECORD]> = chunk.to_vec();
                if layout.starts_with("sorted") {
                    buf.sort_unstable();
                } else if layout.starts_with("radix16") {
                    let mut hist = vec![0u32; 1 << 16];
                    for r in &buf {
                        hist[usize::from(u16::from_be_bytes([r[1], r[2]]))] += 1;
                    }
                    let mut sum = 0;
                    for h in &mut hist {
                        let c = *h;
                        *h = sum;
                        sum += c;
                    }
                    let mut sorted = vec![[0u8; RECORD]; buf.len()];
                    for r in &buf {
                        let b = usize::from(u16::from_be_bytes([r[1], r[2]]));
                        sorted[hist[b] as usize] = *r;
                        hist[b] += 1;
                    }
                    buf = sorted;
                } else if layout.starts_with("radix8") {
                    let mut hist = vec![0u32; 1 << 8];
                    for r in &buf {
                        hist[usize::from(r[1])] += 1;
                    }
                    let mut sum = 0;
                    for h in &mut hist {
                        let c = *h;
                        *h = sum;
                        sum += c;
                    }
                    let mut sorted = vec![[0u8; RECORD]; buf.len()];
                    for r in &buf {
                        let b = usize::from(r[1]);
                        sorted[hist[b] as usize] = *r;
                        hist[b] += 1;
                    }
                    buf = sorted;
                }
                let bytes: Vec<u8> = if layout.ends_with("transposed") {
                    let mut t = Vec::with_capacity(buf.len() * RECORD);
                    for i in 0..RECORD {
                        t.extend(buf.iter().map(|r| r[i]));
                    }
                    t
                } else {
                    buf.iter().flatten().copied().collect()
                };
                prep += t.elapsed().as_nanos();
                total += compressor.compress_to_buffer(&bytes, &mut out).unwrap();
                raw += bytes.len();
            }
            let elapsed = timer.elapsed();
            println!(
                "zstd {level} {layout:>18}: {:.2} bytes/record (ratio {:.3}), {:.1} ns/record total, {:.1} ns/record prep",
                total as f64 / records.len() as f64,
                total as f64 / raw as f64,
                elapsed.as_nanos() as f64 / records.len() as f64,
                prep as f64 / records.len() as f64,
            );
        }
    }
}
