use std::{num::NonZeroUsize, path::PathBuf, process, thread, time::Instant};

use clap::Parser;
use indicatif::{MultiProgress, ProgressDrawTarget};
use lila_openingexplorer4::{
    parquet_to_positions::{Config, Plan, convert},
    util::{ByteSize, free_space, total_memory, zstd_level},
};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// Builds the position column families from parquet files, as
/// non-overlapping SST files in `<out>/cold` (hash -> game id, for
/// positions reached by a single game) and `<out>/positions` (hash and
/// month -> entry, for all other positions).
///
/// Replays all games in passes over the parquet files, sorting the
/// positions of a slice of the key space in each pass. Uses disk scratch
/// space to reduce the number of passes.
#[derive(Parser)]
struct Args {
    #[clap(required = true)]
    parquet: Vec<PathBuf>,
    /// Directory for temporary partition files.
    #[clap(long)]
    scratch: PathBuf,
    /// Maximum bytes to use for scratch space. Determines the number of
    /// passes over the input. Defaults to 90% of the free space of the
    /// scratch directory. Accepts units, like 500g.
    #[clap(long)]
    scratch_budget: Option<ByteSize>,
    /// Maximum bytes of memory to use. Determines the number of memory
    /// partitions. Defaults to 60% of the total memory. Accepts units, like
    /// 32g.
    #[clap(long)]
    memory_budget: Option<ByteSize>,
    /// Threads to use in total. Defaults to the available parallelism minus
    /// two, to leave one physical core idle.
    #[clap(long)]
    threads: Option<usize>,
    /// Only handle the k-th of n equal slices of the key space, given as
    /// k/n with n a power of two. The output is complete for that slice, so
    /// the slices can be built in separate runs.
    #[clap(long, default_value = "0/1", value_parser = parse_subset)]
    subset: (usize, usize),
    /// Disable O_DIRECT for reading input, reading/writing scratch files,
    /// and writing SST files.
    #[clap(long)]
    no_direct_io: bool,
    /// Zstd level for scratch chunks, or 0 for no compression.
    #[clap(long, default_value = "1", value_parser = zstd_level())]
    scratch_zstd_level: i32,
    /// Directory for the final SST files.
    #[clap(long)]
    out: PathBuf,
    /// Only process the first row groups of the inputs.
    #[clap(long, help_heading = "Benchmarking")]
    row_groups: Option<usize>,
    /// Stop after the scatter of the first pass.
    #[clap(long, help_heading = "Benchmarking")]
    scatter_only: bool,
    /// Only print the estimated sizes and the chosen layout.
    #[clap(long, help_heading = "Benchmarking")]
    plan_only: bool,
    /// Minimum estimated number of games reaching a position for it to be
    /// aggregated in memory instead of partitioned. Defaults to a fraction
    /// of the partition size.
    #[clap(long, help_heading = "Benchmarking")]
    hot_threshold: Option<u64>,
    /// Finish writing all partitions of a pass before scanning the input for
    /// the next pass. Use when the input is on the output disk, so that the
    /// scan does not compete with the SST writes.
    #[clap(long)]
    no_overlap: bool,
    /// Block size of the SST files.
    #[clap(long, default_value = "4096")]
    sst_block_size: usize,
    /// Zstd level of the cold SST files, or 0 for no compression (random
    /// keys and values do not compress).
    #[clap(long, default_value = "0", value_parser = zstd_level())]
    cold_zstd_level: i32,
    /// Zstd level of the positions SST files, or 0 for no compression.
    #[clap(long, default_value = "3", value_parser = zstd_level())]
    positions_zstd_level: i32,
    /// Threads for decoding and replaying the input (default: three
    /// quarters of the threads with a single pass or --no-overlap, otherwise
    /// half).
    #[clap(long, help_heading = "Advanced")]
    read_threads: Option<usize>,
    /// Concurrent reads of the input, ahead of the decoding threads.
    #[clap(long, default_value = "8")]
    io_depth: usize,
    /// Threads for compressing and writing scratch chunks (default: a
    /// quarter of the threads with a single pass or --no-overlap, otherwise
    /// a fifth).
    #[clap(long, help_heading = "Advanced")]
    write_threads: Option<usize>,
    /// Threads for loading and sorting scratch files (default: a third of
    /// the threads with a single pass or --no-overlap, otherwise a fifth).
    #[clap(long, help_heading = "Advanced")]
    load_threads: Option<usize>,
    /// Threads for writing SST files (default: two thirds of the threads
    /// with a single pass or --no-overlap, otherwise a quarter).
    #[clap(long, help_heading = "Advanced")]
    sst_threads: Option<usize>,
}

fn parse_subset(s: &str) -> Result<(usize, usize), String> {
    let (k, n) = s.split_once('/').ok_or("expected k/n")?;
    let k: usize = k.trim().parse().map_err(|_| "invalid k")?;
    let n: usize = n.trim().parse().map_err(|_| "invalid n")?;
    if !n.is_power_of_two() {
        return Err("n must be a power of two".to_owned());
    }
    if k >= n {
        return Err("k must be less than n".to_owned());
    }
    Ok((k, n))
}

fn main() {
    let args = Args::parse();

    let config = Config {
        threads: args.threads.unwrap_or_else(|| {
            thread::available_parallelism()
                .map_or(1, NonZeroUsize::get)
                .saturating_sub(2)
                .max(2)
        }),
        memory_budget: args.memory_budget.map_or_else(
            || total_memory().expect("total memory, pass --memory-budget") / 10 * 6,
            u64::from,
        ),
        scratch_budget: args.scratch_budget.map_or_else(
            || {
                free_space(&args.scratch).expect("free scratch space, pass --scratch-budget") / 10
                    * 9
            },
            u64::from,
        ),
        subset: args.subset,
        hot_threshold: args.hot_threshold,
        read_threads: args.read_threads,
        write_threads: args.write_threads,
        load_threads: args.load_threads,
        sst_threads: args.sst_threads,
        io_depth: args.io_depth,
        direct_io: !args.no_direct_io,
        scratch_zstd_level: args.scratch_zstd_level,
        row_groups: args.row_groups,
        scatter_only: args.scatter_only,
        overlap: !args.no_overlap,
        sst_block_size: args.sst_block_size,
        cold_zstd_level: args.cold_zstd_level,
        positions_zstd_level: args.positions_zstd_level,
    };

    let started = Instant::now();
    let plan = match Plan::new(&args.parquet, &config) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("error: {err}");
            process::exit(1);
        }
    };
    eprintln!("plan ({:.1?}): {plan}", started.elapsed());
    if args.plan_only {
        return;
    }

    let started = Instant::now();
    let progress = MultiProgress::with_draw_target(ProgressDrawTarget::stdout_with_hz(4));
    let stats = match convert(&plan, &args.scratch, &args.out, &config, &progress) {
        Ok(stats) => stats,
        Err(err) => {
            eprintln!("error: {err}");
            process::exit(1);
        }
    };

    println!(
        "done in {:.1?}: {} games ({} loops removed), {} records ({} hot); {} cold keys in {} files ({} bytes, largest {}, {:.1} bytes per key); {} position keys in {} files ({} bytes, largest {}, {:.1} bytes per key, ratio {:.3}); {} hot keys merged with partitioned records",
        started.elapsed(),
        stats.games,
        stats.loops,
        stats.records,
        stats.hot_records,
        stats.cold_keys,
        stats.cold.files,
        stats.cold.bytes,
        stats.cold.largest_file,
        stats.cold.bytes as f64 / stats.cold_keys.max(1) as f64,
        stats.position_keys,
        stats.positions.files,
        stats.positions.bytes,
        stats.positions.largest_file,
        stats.positions.bytes as f64 / stats.position_keys.max(1) as f64,
        stats.positions.ratio(1.0),
        stats.hot_merged_keys,
    );
    let skipped: Vec<String> = stats
        .skipped
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(name, count)| format!("{count} {name}"))
        .collect();
    if !skipped.is_empty() {
        println!("skipped games: {}", skipped.join(", "));
    }
}
