use std::{num::NonZeroUsize, path::PathBuf, process, thread, time::Instant};

use clap::Parser;
use indicatif::{MultiProgress, ProgressDrawTarget};
use lila_openingexplorer4::{
    parquet_to_games::{Config, Plan, convert},
    util::{ByteSize, free_space, total_memory, zstd_level},
};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// Sorts games from parquet files into non-overlapping SST files.
///
/// Scans the parquet files multiple times, loading and sorting partitions
/// in memory. Uses disk scratch space to reduce the number of passes over the
/// full input.
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
    /// Finish writing all partitions of a pass before scanning the input for
    /// the next pass. Use when the input is on the output disk, so that the
    /// scan does not compete with the SST writes.
    #[clap(long)]
    no_overlap: bool,
    /// Block size of the SST files.
    #[clap(long, default_value = "4096")]
    sst_block_size: usize,
    /// Zstd level of the SST files, or 0 for no compression.
    #[clap(long, default_value = "3", value_parser = zstd_level())]
    sst_zstd_level: i32,
    /// Maximum size of the zstd dictionary trained for each SST file
    /// (0 to disable dictionary compression).
    #[clap(long, default_value = "0")]
    sst_zstd_dict: i32,
    /// Threads for decoding the input (default: three quarters of the
    /// threads with a single pass or --no-overlap, otherwise half).
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
    /// Threads for loading scratch files (default: a third of the threads
    /// with a single pass or --no-overlap, otherwise a fifth).
    #[clap(long, help_heading = "Advanced")]
    load_threads: Option<usize>,
    /// Threads for writing SST files (default: two thirds of the threads
    /// with a single pass or --no-overlap, otherwise a quarter).
    #[clap(long, help_heading = "Advanced")]
    sst_threads: Option<usize>,
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
        sst_zstd_level: args.sst_zstd_level,
        sst_zstd_dict: args.sst_zstd_dict,
    };

    let plan = match Plan::new(&args.parquet, &config) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("error: {err}");
            process::exit(1);
        }
    };
    eprintln!("{plan}");
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
        "done in {:.1?}: {} games in {} sst files ({} bytes, largest {} bytes, ratio {:.3}), {} skipped games",
        started.elapsed(),
        stats.games,
        stats.files,
        stats.bytes,
        stats.largest_file,
        stats.ratio(),
        stats.skipped,
    );
}
