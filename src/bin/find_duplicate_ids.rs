use std::{
    collections::HashSet,
    io,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Instant,
};

use clap::Parser;
use lila_openingexplorer4::{
    packed::GameId,
    parquet::{FastGameColumns, FixedColumn, PositionalFile},
};
use parquet::file::reader::{FileReader, SerializedFileReader};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

const BUCKET_BITS: u32 = 6;

/// Finds duplicate game ids in a parquet file and reports where they occur.
#[derive(Parser)]
struct Args {
    parquet: PathBuf,
    #[clap(long, default_value = "28")]
    threads: usize,
    #[clap(long)]
    row_groups: Option<usize>,
    /// Maximum number of duplicates to locate and compare in detail.
    #[clap(long, default_value = "20")]
    examples: usize,
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let reader = SerializedFileReader::new(PositionalFile::open(&args.parquet)?)?;
    let n = args
        .row_groups
        .map_or(reader.num_row_groups(), |l| l.min(reader.num_row_groups()));

    let timer = Instant::now();
    let buckets: Vec<Mutex<Vec<u64>>> = (0..1 << BUCKET_BITS)
        .map(|_| Mutex::new(Vec::new()))
        .collect();
    let next = AtomicUsize::new(0);
    thread::scope(|s| {
        for _ in 0..args.threads {
            s.spawn(|| {
                let mut column = FixedColumn::default();
                let mut local: Vec<Vec<u64>> = (0..1 << BUCKET_BITS).map(|_| Vec::new()).collect();
                let flush = |local: &mut Vec<Vec<u64>>| {
                    for (bucket, ids) in local.iter_mut().enumerate() {
                        if !ids.is_empty() {
                            buckets[bucket].lock().expect("lock").append(ids);
                        }
                    }
                };
                let mut since_flush = 0;
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let rg = reader.get_row_group(i).expect("row group");
                    column.read(&*rg, 0, GameId::SIZE).expect("read ids");
                    for j in 0..column.len() {
                        let id = GameId::unpack(&mut column.get(j));
                        let raw = u64::from_be_bytes({
                            let mut b = [0u8; 8];
                            b[2..].copy_from_slice(&id.to_bytes());
                            b
                        });
                        local[id.partition(BUCKET_BITS)].push(raw);
                    }
                    since_flush += column.len();
                    if since_flush >= 4_000_000 {
                        flush(&mut local);
                        since_flush = 0;
                    }
                }
                flush(&mut local);
            });
        }
    });
    let total: usize = buckets.iter().map(|b| b.lock().expect("lock").len()).sum();
    eprintln!("collected {total} ids in {:.1?}", timer.elapsed());

    let timer = Instant::now();
    let duplicates: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    thread::scope(|s| {
        for _ in 0..args.threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= buckets.len() {
                        break;
                    }
                    let mut ids = std::mem::take(&mut *buckets[i].lock().expect("lock"));
                    ids.sort_unstable();
                    let mut local = Vec::new();
                    for w in ids.windows(2) {
                        if w[0] == w[1] {
                            local.push(w[0]);
                        }
                    }
                    duplicates.lock().expect("lock").append(&mut local);
                }
            });
        }
    });
    let mut duplicates = duplicates.into_inner().expect("duplicates");
    let occurrences = duplicates.len();
    duplicates.dedup();
    eprintln!(
        "{} duplicate ids ({} extra occurrences) found in {:.1?}",
        duplicates.len(),
        occurrences,
        timer.elapsed()
    );
    println!(
        "duplicate_ids {} extra_occurrences {}",
        duplicates.len(),
        occurrences
    );

    if duplicates.is_empty() {
        return Ok(());
    }

    // Locate examples and compare their values.
    let wanted: HashSet<u64> = duplicates.iter().take(args.examples).copied().collect();
    let found: Mutex<Vec<(u64, usize, Vec<u8>)>> = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    thread::scope(|s| {
        for _ in 0..args.threads {
            s.spawn(|| {
                let mut column = FixedColumn::default();
                let mut columns = FastGameColumns::default();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let rg = reader.get_row_group(i).expect("row group");
                    column.read(&*rg, 0, GameId::SIZE).expect("read ids");
                    let mut hits = Vec::new();
                    for j in 0..column.len() {
                        let mut b = [0u8; 8];
                        b[2..].copy_from_slice(column.get(j));
                        if wanted.contains(&u64::from_be_bytes(b)) {
                            hits.push((j, u64::from_be_bytes(b)));
                        }
                    }
                    if hits.is_empty() {
                        continue;
                    }
                    columns.read_columns(&*rg).expect("read columns");
                    for (j, raw) in hits {
                        let row = columns.get(j);
                        let mut value = Vec::new();
                        let packed = row.pack_lichess_game(&mut value).is_ok();
                        let desc = format!(
                            "rg {i} row {j}: {} vs {} {:?}-{:?}-{:?} {} plies, packed {packed}",
                            String::from_utf8_lossy(row.players.white.name),
                            String::from_utf8_lossy(row.players.black.name),
                            row.date_year,
                            row.date_month,
                            row.date_day,
                            row.moves.len() / 2,
                        );
                        found
                            .lock()
                            .expect("lock")
                            .push((raw, i, desc.into_bytes()));
                    }
                }
            });
        }
    });
    let mut found = found.into_inner().expect("found");
    found.sort();
    for (raw, _, desc) in found {
        let id = GameId::unpack(&mut &raw.to_be_bytes()[2..]);
        println!("{id} {}", String::from_utf8_lossy(&desc));
    }
    Ok(())
}
