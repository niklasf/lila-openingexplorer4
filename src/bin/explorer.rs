use std::{net::SocketAddr, num::NonZeroUsize, path::PathBuf, process, thread, time::Duration};

use clap::{Args, Parser, Subcommand};
use lila_openingexplorer4::{
    server::{AppConfig, Database, DbConfig, Kind, ingest, serve},
    util::ByteSize,
};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// Read-only opening explorer server for the SST files produced by
/// parquet_to_games and parquet_to_positions.
///
/// First create a database directory with `ingest`, which hard-links the
/// SST files (they must be on the same file system). Then serve it with
/// `lichess` or `masters`, which open the database strictly read-only.
///
/// Logging is configured with EXPLORER_LOG (like RUST_LOG), default info.
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Creates a database directory from SST files.
    Ingest(IngestArgs),
    /// Serves /lichess from an ingested database of lichess games.
    Lichess(ServeArgs),
    /// Serves /masters and /masters/pgn/{id} from an ingested database of
    /// masters games.
    Masters(ServeArgs),
}

#[derive(Args)]
struct IngestArgs {
    /// Database directory to create (must not exist yet).
    #[arg(long)]
    db: PathBuf,
    /// Output directory of parquet_to_games.
    #[arg(long)]
    games: PathBuf,
    /// Output directory of parquet_to_positions, with the subdirectories
    /// positions and cold_positions.
    #[arg(long)]
    positions: PathBuf,
    #[command(flatten)]
    db_opts: DbArgs,
}

#[derive(Args)]
struct DbArgs {
    /// Block size the SST files were written with.
    #[arg(long, default_value = "4096", help_heading = "Database")]
    block_size: usize,
    /// Threads for opening table files at startup.
    #[arg(long, default_value = "16", help_heading = "Database")]
    file_opening_threads: usize,
}

#[derive(Args)]
struct ServeArgs {
    /// Database directory created by `ingest`.
    #[arg(long)]
    db: PathBuf,
    /// Binding address. Administrative endpoints (/monitor) must be
    /// protected by a reverse proxy.
    #[arg(long, default_value = "0.0.0.0:9002")]
    bind: SocketAddr,
    /// Allow access from all origins.
    #[arg(long)]
    cors: bool,
    /// Games were indexed up to this ply by parquet_to_positions (0 for all
    /// plies). Must match, or games reaching a position later would be
    /// found (or missed) inconsistently.
    #[arg(long, default_value = "50")]
    max_ply: usize,
    /// Size of the RocksDB block cache. Accepts units, like 32g. The page
    /// cache serves as a second level unless --direct-reads is given, so
    /// leave memory for it.
    #[arg(long, default_value = "4g", help_heading = "Database")]
    cache: ByteSize,
    /// Use an LRU block cache instead of the (more scalable) hyper clock
    /// cache.
    #[arg(long, help_heading = "Database")]
    lru_cache: bool,
    /// Read SST files with O_DIRECT, bypassing the page cache.
    #[arg(long, help_heading = "Database")]
    direct_reads: bool,
    /// Collect RocksDB statistics (tickers in /monitor). Costs a few
    /// percent of CPU.
    #[arg(long, help_heading = "Database")]
    db_statistics: bool,
    #[command(flatten)]
    db_opts: DbArgs,
    /// Concurrent database requests. Defaults to half the available
    /// parallelism (the physical cores on machines with SMT): more threads
    /// do not add throughput but lengthen tail latencies.
    #[arg(long, help_heading = "Server")]
    blocking_threads: Option<usize>,
    /// Request log (one path with query string per line, like
    /// sample-log.txt.zst) to replay before listening, so that the block
    /// cache is filled.
    #[arg(long, help_heading = "Server")]
    warm_up: Option<PathBuf>,
    /// Only replay the first lines of the warm-up log.
    #[arg(long, default_value = "1000000", help_heading = "Server")]
    warm_up_limit: usize,
    /// Entries of the response cache.
    #[arg(long, default_value = "100000", help_heading = "Server")]
    response_cache: u64,
    /// Seconds after which cached responses expire.
    #[arg(long, default_value = "7200", help_heading = "Server")]
    response_cache_ttl: u64,
    /// Masters only: most recent games of the highest rating groups that
    /// are loaded and ranked by their actual ratings for the top games.
    #[arg(long, default_value = "128", help_heading = "Server")]
    masters_top_candidates: usize,
    /// Directory with a.tsv to e.tsv of lichess-org/chess-openings, to be
    /// loaded instead of downloading the current names from GitHub.
    #[arg(long, help_heading = "Server")]
    openings_dir: Option<PathBuf>,
    /// Do not load opening names at all.
    #[arg(long, help_heading = "Server")]
    no_openings: bool,
}

fn db_config(
    db_opts: &DbArgs,
    cache_bytes: u64,
    lru_cache: bool,
    direct_reads: bool,
    statistics: bool,
) -> DbConfig {
    DbConfig {
        cache_bytes,
        lru_cache,
        direct_reads,
        statistics,
        file_opening_threads: db_opts.file_opening_threads,
        block_size: db_opts.block_size,
    }
}

fn raise_fd_limit() {
    // SAFETY: getrlimit and setrlimit only read and write the given struct.
    unsafe {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 && limit.rlim_cur < limit.rlim_max
        {
            limit.rlim_cur = limit.rlim_max;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
                log::info!("raised open file limit to {}", limit.rlim_cur);
            }
        }
    }
}

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::new()
            .filter_or("EXPLORER_LOG", "info")
            .write_style("EXPLORER_LOG_STYLE"),
    )
    .format_module_path(false)
    .format_target(false)
    .init();

    match Cli::parse().command {
        Command::Ingest(args) => {
            let config = db_config(&args.db_opts, 0, true, false, false);
            if args.db.exists() {
                eprintln!("error: {} already exists", args.db.display());
                process::exit(1);
            }
            match ingest(
                &args.db,
                &args.games,
                &args.positions.join("positions"),
                &args.positions.join("cold_positions"),
                &config,
            ) {
                Ok(stats) => println!(
                    "ingested {} games files, {} positions files, {} cold_positions files into {}",
                    stats.games_files,
                    stats.positions_files,
                    stats.cold_files,
                    args.db.display()
                ),
                Err(err) => {
                    eprintln!("error: {err}");
                    process::exit(1);
                }
            }
        }
        Command::Lichess(args) => run(Kind::Lichess, args),
        Command::Masters(args) => run(Kind::Masters, args),
    }
}

fn run(kind: Kind, args: ServeArgs) {
    raise_fd_limit();
    let blocking_threads = args.blocking_threads.unwrap_or_else(|| {
        (thread::available_parallelism().map_or(8, NonZeroUsize::get) / 2).max(2)
    });
    let db_config = db_config(
        &args.db_opts,
        args.cache.into(),
        args.lru_cache,
        args.direct_reads,
        args.db_statistics,
    );
    let config = AppConfig {
        kind,
        bind: args.bind,
        cors: args.cors,
        cache_entries: args.response_cache,
        cache_ttl: Duration::from_secs(args.response_cache_ttl),
        blocking_threads,
        max_ply: (args.max_ply > 0).then_some(args.max_ply),
        masters_top_candidates: args.masters_top_candidates,
        openings_dir: args.openings_dir,
        no_openings: args.no_openings,
        warm_up: args.warm_up,
        warm_up_limit: args.warm_up_limit,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(blocking_threads + 16)
        .build()
        .expect("tokio runtime");

    let db = match Database::open_read_only(&args.db, &db_config) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("error: {err}");
            process::exit(1);
        }
    };

    if let Err(err) = runtime.block_on(serve(config, db)) {
        eprintln!("error: {err}");
        process::exit(1);
    }
}
