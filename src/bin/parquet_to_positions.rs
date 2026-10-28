use std::{num::NonZeroUsize, path::PathBuf, process, thread, time::Instant};

use clap::Parser;
use indicatif::{MultiProgress, ProgressDrawTarget};
use lila_openingexplorer4::{
    parquet_to_positions::{Config, Plan, convert},
    sst::SstConfig,
    util::{ByteSize, free_space, total_memory, zstd_level},
};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// Indexes the positions of games from parquet files into non-overlapping
/// SST files for the cold_positions and positions column families.
///
/// Scans the parquet files multiple times, replaying all games, and loading
/// and sorting partitions of the key space in memory. Uses disk scratch space
/// to reduce the number of passes over the full input.
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
    /// Directory for the final SST files, in the subdirectories
    /// cold_positions and positions (which must be empty or missing).
    #[clap(long)]
    out: PathBuf,
    /// Only index the positions of the first plies of each game.
    #[clap(long)]
    max_ply: Option<usize>,
    /// Skip the first row groups of the inputs.
    #[clap(long, default_value = "0", help_heading = "Benchmarking")]
    skip_row_groups: usize,
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
    /// Approximate size of the SST files. The target_file_size_base of the
    /// column families should match.
    #[clap(long, default_value = "512m")]
    sst_target_size: ByteSize,
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
    /// Threads for replaying the input (default: two thirds of the threads
    /// with a single pass or --no-overlap, otherwise half).
    #[clap(long, help_heading = "Advanced")]
    read_threads: Option<usize>,
    /// Concurrent reads of the input, ahead of the decoding threads.
    #[clap(long, default_value = "8")]
    io_depth: usize,
    /// Threads for compressing and writing scratch chunks (default: a third
    /// of the threads with a single pass or --no-overlap, otherwise a
    /// fifth).
    #[clap(long, help_heading = "Advanced")]
    write_threads: Option<usize>,
    /// Threads for loading and sorting partitions (default: two fifths of
    /// the threads with a single pass or --no-overlap, otherwise a fifth).
    #[clap(long, help_heading = "Advanced")]
    load_threads: Option<usize>,
    /// Threads for writing SST files (default: three fifths of the threads
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
        max_ply: args.max_ply,
        skip_row_groups: args.skip_row_groups,
        row_groups: args.row_groups,
        scatter_only: args.scatter_only,
        overlap: !args.no_overlap,
        sst: SstConfig {
            block_size: args.sst_block_size,
            zstd_level: args.sst_zstd_level,
            zstd_dict: args.sst_zstd_dict,
            direct_io: !args.no_direct_io,
        },
        sst_target_size: args.sst_target_size.into(),
    };

    let planned = Instant::now();
    let plan = match Plan::new(&args.parquet, &config) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("error: {err}");
            process::exit(1);
        }
    };
    eprintln!("{plan} (planned in {:.1?})", planned.elapsed());
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

    let output = &stats.output;
    println!(
        "done in {:.1?}: {} games ({} skipped), {} records ({} in {} hot positions)",
        started.elapsed(),
        stats.games,
        stats.skipped,
        stats.records,
        stats.hot_records,
        stats.hot_positions,
    );
    println!(
        "cold_positions: {} groups for {} records in {} files ({} bytes, {:.1} B/record, {:.1} B/group, largest {} bytes)",
        output.cold.keys,
        output.cold_records,
        output.cold.files,
        output.cold.bytes,
        output.cold.bytes as f64 / output.cold_records.max(1) as f64,
        output.cold.bytes_per_key(),
        output.cold.largest_file,
    );
    println!(
        "positions: {} keys for {} records in {} files ({} bytes, {:.1} B/key, ratio {:.3}, largest {} bytes)",
        output.positions.keys,
        output.position_records + stats.hot_records,
        output.positions.files,
        output.positions.bytes,
        output.positions.bytes_per_key(),
        output.positions.ratio(),
        output.positions.largest_file,
    );
}
