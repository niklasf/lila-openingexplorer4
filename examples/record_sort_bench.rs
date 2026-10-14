// Benchmarks sorting runs of position records and encoding chunks.

use std::{env, time::Instant};

use lila_openingexplorer4::parquet_to_positions::record::{
    CHUNK_RECORDS, ChunkEncoder, RECORD_SIZE, Record,
};

fn random_records(n: usize, partition_bits: u32, seed: u64) -> Vec<Record> {
    let mut state = seed;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    (0..n)
        .map(|i| {
            let mut r = [0u8; RECORD_SIZE];
            let prefix = next() >> partition_bits;
            r[..8].copy_from_slice(&prefix.to_be_bytes());
            r[8..12].copy_from_slice(&(next() as u32).to_be_bytes());
            // Month: mostly one value, slowly changing.
            r[12..14].copy_from_slice(&(1500u16 + (i / (n / 3 + 1)) as u16).to_be_bytes());
            let m = next();
            r[14..16].copy_from_slice(&((m as u16) & 0x0fff).to_be_bytes());
            let speed = 2 + (m >> 16) % 3;
            let rating = 4096 + 1500 + ((next() % 1200) as i32 - 600) as u16 as i32 - 1500;
            r[16..18]
                .copy_from_slice(&((speed as u16) << 13 | (rating as u16 & 0x1fff)).to_be_bytes());
            let game_id = next() % 62u64.pow(8);
            r[18..24].copy_from_slice(&game_id.to_be_bytes()[2..]);
            r[24] = (next() % 3 + 1) as u8;
            Record(r)
        })
        .collect()
}

fn main() {
    let mut args = env::args().skip(1);
    let n: usize = args.next().map_or(24_000_000, |s| s.parse().unwrap());
    let partition_bits: u32 = args.next().map_or(3, |s| s.parse().unwrap());
    let records = random_records(n, partition_bits, 0x9e3779b97f4a7c15);

    // Plain sort with the Record order.
    let mut run = records.clone();
    let timer = Instant::now();
    run.sort_unstable();
    println!(
        "sort_unstable (integer Ord): {:.2?}, {:.1} ns/record",
        timer.elapsed(),
        timer.elapsed().as_nanos() as f64 / n as f64
    );
    let sorted = run;

    // Byte comparison.
    let mut run = records.clone();
    let timer = Instant::now();
    run.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    println!(
        "sort_unstable (byte cmp): {:.2?}, {:.1} ns/record",
        timer.elapsed(),
        timer.elapsed().as_nanos() as f64 / n as f64
    );
    assert_eq!(run, sorted);

    // MSD radix on 16 bits after the partition bits, then sort buckets.
    for bits in [12u32, 16] {
        let mut run = records.clone();
        let timer = Instant::now();
        let shift = 64 - partition_bits - bits;
        let bucket = |r: &Record| (r.hash_prefix() >> shift) as usize & ((1 << bits) - 1);
        let mut counts = vec![0u32; 1 << bits];
        for r in &run {
            counts[bucket(r)] += 1;
        }
        let mut offsets = vec![0u32; (1 << bits) + 1];
        for b in 0..1 << bits {
            offsets[b + 1] = offsets[b] + counts[b];
        }
        let mut pos = offsets.clone();
        let mut tmp: Vec<Record> = Vec::with_capacity(n);
        unsafe { tmp.set_len(n) };
        for r in &run {
            let b = bucket(r);
            tmp[pos[b] as usize] = *r;
            pos[b] += 1;
        }
        let scatter = timer.elapsed();
        for b in 0..1 << bits {
            tmp[offsets[b] as usize..offsets[b + 1] as usize].sort_unstable();
        }
        run = tmp;
        println!(
            "radix {bits} + sort buckets: {:.2?} (scatter {:.2?}), {:.1} ns/record",
            timer.elapsed(),
            scatter,
            timer.elapsed().as_nanos() as f64 / n as f64
        );
        assert_eq!(run, sorted);
    }

    // Chunk encoding.
    let mut compressor = zstd::bulk::Compressor::new(1).unwrap();
    let mut cbuf = vec![0u8; zstd::zstd_safe::compress_bound(CHUNK_RECORDS * RECORD_SIZE)];
    let mut encoder = ChunkEncoder::new(partition_bits);
    let timer = Instant::now();
    let mut total = 0;
    let mut encode_nanos = 0;
    for chunk in records.chunks(CHUNK_RECORDS) {
        let t = Instant::now();
        let out = encoder.encode(chunk);
        encode_nanos += t.elapsed().as_nanos();
        total += compressor.compress_to_buffer(out, &mut cbuf).unwrap();
    }
    println!(
        "encode_chunk + zstd 1: {:.1} ns/record (encode {:.1}), {:.2} bytes/record",
        timer.elapsed().as_nanos() as f64 / n as f64,
        encode_nanos as f64 / n as f64,
        total as f64 / n as f64
    );
}
