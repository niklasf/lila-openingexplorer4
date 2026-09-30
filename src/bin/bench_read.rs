use std::{
    io,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::Instant,
};

use clap::Parser;
use lila_openingexplorer4::parquet::{FastGameColumns, GameColumns, PositionalFile};
use parquet::file::reader::{FileReader, SerializedFileReader};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// Compares and benchmarks the generic and the fast row group readers.
#[derive(Parser)]
struct Args {
    parquet: PathBuf,
    #[clap(long, default_value = "200")]
    row_groups: usize,
    /// First row group to read.
    #[clap(long, default_value = "0")]
    start: usize,
    #[clap(long, default_value = "16")]
    threads: usize,
    /// Check that both readers produce identical rows.
    #[clap(long)]
    verify: bool,
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let reader = SerializedFileReader::new(PositionalFile::open(&args.parquet)?)?;
    let end = (args.start + args.row_groups).min(reader.num_row_groups());
    let n = end.saturating_sub(args.start);

    for fast in [false, true, false, true] {
        let next = AtomicUsize::new(0);
        let timer = Instant::now();
        let rows = AtomicUsize::new(0);
        thread::scope(|s| {
            for _ in 0..args.threads {
                s.spawn(|| {
                    let mut generic = GameColumns::default();
                    let mut fast_columns = FastGameColumns::default();
                    let mut local = 0;
                    let mut checksum = 0u64;
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        let rg = reader.get_row_group(args.start + i).expect("row group");
                        if fast {
                            fast_columns.read_columns(&*rg).expect("fast read");
                            local += fast_columns.len();
                            for row in fast_columns.rows() {
                                checksum = checksum.wrapping_add(row.moves.len() as u64);
                            }
                        } else {
                            generic.read_columns(&*rg).expect("read");
                            local += generic.len();
                            for row in generic.rows() {
                                checksum = checksum.wrapping_add(row.moves.len() as u64);
                            }
                        }
                        if args.verify && fast {
                            generic.read_columns(&*rg).expect("read");
                            assert_eq!(generic.len(), fast_columns.len());
                            for (a, b) in generic.rows().zip(fast_columns.rows()) {
                                assert_eq!(a.game_id, b.game_id);
                                assert_eq!(a.players.white.title, b.players.white.title);
                                assert_eq!(a.players.white.name, b.players.white.name);
                                assert_eq!(a.players.white.rating, b.players.white.rating);
                                assert_eq!(a.players.black.title, b.players.black.title);
                                assert_eq!(a.players.black.name, b.players.black.name);
                                assert_eq!(a.players.black.rating, b.players.black.rating);
                                assert_eq!(a.date_year, b.date_year);
                                assert_eq!(a.date_month, b.date_month);
                                assert_eq!(a.date_day, b.date_day);
                                assert_eq!(a.outcome, b.outcome);
                                assert_eq!(a.speed, b.speed);
                                assert_eq!(a.rating_mode, b.rating_mode);
                                assert_eq!(a.setup, b.setup);
                                assert_eq!(a.moves, b.moves);
                            }
                        }
                    }
                    rows.fetch_add(local, Ordering::Relaxed);
                    std::hint::black_box(checksum);
                });
            }
        });
        let elapsed = timer.elapsed();
        let rows = rows.load(Ordering::Relaxed);
        println!(
            "{}: {n} row groups, {rows} rows in {elapsed:.2?} ({:.1} M rows/s)",
            if fast { "fast" } else { "generic" },
            rows as f64 / elapsed.as_secs_f64() / 1e6
        );
    }
    Ok(())
}
