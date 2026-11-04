use std::{net::SocketAddr, num::NonZeroUsize, path::PathBuf, process, thread, time::Duration};

use clap::{Args, Parser, Subcommand};
use lila_openingexplorer4::{
    server::{AppConfig, Database, DbConfig, Kind, LilaConfig, ingest, serve},
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
    /// Leave out the cold positions (positions reached by a single game),
    /// so that the cold_positions subdirectory is not needed.
    #[arg(long)]
    no_cold: bool,
    /// Directory with the map from game ids to game indexes, written by
    /// parquet_to_games --game-ids. Needed for looking up games by id
    /// (/masters/pgn/{id}).
    #[arg(long)]
    game_ids: Option<PathBuf>,
}

#[derive(Args)]
#[command(next_help_heading = "Database")]
struct DbArgs {
    /// Size of the RocksDB block cache. Accepts units, like 32g. The page
    /// cache serves as a second level unless --direct-reads is given, so
    /// leave memory for it.
    #[arg(long, default_value = "4g")]
    cache: ByteSize,
    /// Read SST files with O_DIRECT, bypassing the page cache.
    #[arg(long)]
    direct_reads: bool,
    /// Collect RocksDB statistics (tickers in /monitor). Costs a few
    /// percent of CPU.
    #[arg(long)]
    db_statistics: bool,
    /// Threads for opening table files at startup.
    #[arg(long, default_value = "16")]
    file_opening_threads: usize,
    /// Load all index and filter blocks when opening (sequentially, file
    /// by file), and pin them in the block cache, instead of reading them
    /// as lookups need them. Budget the block cache accordingly.
    #[arg(long)]
    pin_metadata: bool,
}

impl From<DbArgs> for DbConfig {
    fn from(args: DbArgs) -> DbConfig {
        DbConfig {
            cache_bytes: args.cache.into(),
            direct_reads: args.direct_reads,
            statistics: args.db_statistics,
            file_opening_threads: args.file_opening_threads,
            pin_metadata: args.pin_metadata,
        }
    }
}

#[derive(Args)]
struct ServeArgs {
    /// Database directory created by `ingest`.
    #[arg(long)]
    db: PathBuf,
    /// Binding address. Administrative endpoints (/monitor) must be
    /// protected by a reverse proxy.
    #[arg(long, default_value = "127.0.0.1:9002")]
    bind: SocketAddr,
    /// Allow access from all origins.
    #[arg(long)]
    cors: bool,
    /// Games were indexed up to this ply by parquet_to_positions (0 for all
    /// plies). Must match, or games reaching a position later would be
    /// found (or missed) inconsistently.
    #[arg(long, default_value = "50")]
    max_ply: usize,
    /// Do not look up cold positions: positions reached by a single game
    /// are answered as if no game reached them. Saves a lookup and loading
    /// the candidate games for every position that is not in the positions
    /// column family.
    #[arg(long)]
    no_cold: bool,
    #[command(flatten)]
    db_opts: DbArgs,
    /// Concurrent database requests. Defaults to half the available
    /// parallelism (the physical cores on machines with SMT): more threads
    /// do not add throughput but lengthen tail latencies.
    #[arg(long, help_heading = "Server")]
    blocking_threads: Option<usize>,
    /// Read the games of a response that are not in the block cache in
    /// parallel (one thread each), rather than one after the other. For
    /// disks that serve concurrent reads faster, like hard disk arrays.
    #[arg(long, help_heading = "Server")]
    parallel_fetch: bool,
    /// Entries of the response cache.
    #[arg(long, default_value = "100000", help_heading = "Server")]
    response_cache: u64,
    /// Seconds after which cached responses expire.
    #[arg(long, default_value = "7200", help_heading = "Server")]
    response_cache_ttl: u64,
    /// Directory with a.tsv to e.tsv of lichess-org/chess-openings, to be
    /// loaded instead of downloading the current names from GitHub.
    #[arg(long, help_heading = "Server", conflicts_with = "no_openings")]
    openings_dir: Option<PathBuf>,
    /// Do not load opening names at all.
    #[arg(long, help_heading = "Server")]
    no_openings: bool,
    #[command(flatten)]
    lila: LilaArgs,
}

#[derive(Args)]
#[command(next_help_heading = "Blacklist (lichess only)")]
struct LilaArgs {
    /// Base url of the lila instance to get the blacklist of marked
    /// players from (/api/stream/mod-marked-since).
    #[arg(long, env = "LILA_URL", default_value = "https://lichess.org")]
    lila: String,
    /// Token of https://lichess.org/@/OpeningExplorer, for the blacklist.
    #[arg(long, env = "EXPLORER_BEARER", hide_env_values = true)]
    lila_bearer: Option<String>,
    /// File with the token, read before each update.
    #[arg(long, env = "EXPLORER_BEARER_FILE", conflicts_with = "lila_bearer")]
    lila_bearer_file: Option<PathBuf>,
    /// Seconds between blacklist updates.
    #[arg(long, default_value = "10380")]
    blacklist_refresh: u64,
    /// Do not hide games of blacklisted players.
    #[arg(long)]
    no_blacklist: bool,
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
            if args.db.exists() {
                eprintln!("error: {} already exists", args.db.display());
                process::exit(1);
            }
            match ingest(
                &args.db,
                &args.games,
                &args.positions.join("positions"),
                (!args.no_cold)
                    .then(|| args.positions.join("cold_positions"))
                    .as_deref(),
                args.game_ids.as_deref(),
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
    let db_config = DbConfig::from(args.db_opts);
    let config = AppConfig {
        kind,
        bind: args.bind,
        cors: args.cors,
        cache_entries: args.response_cache,
        cache_ttl: Duration::from_secs(args.response_cache_ttl),
        blocking_threads,
        parallel_fetch: args.parallel_fetch,
        max_ply: (args.max_ply > 0).then_some(args.max_ply),
        cold: !args.no_cold,
        openings_dir: args.openings_dir,
        no_openings: args.no_openings,
        blacklist: (kind == Kind::Lichess && !args.lila.no_blacklist).then(|| LilaConfig {
            url: args.lila.lila.trim_end_matches('/').to_owned(),
            bearer: args.lila.lila_bearer,
            bearer_file: args.lila.lila_bearer_file,
            refresh: Duration::from_secs(args.lila.blacklist_refresh),
        }),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(
            blocking_threads + // enforced by semaphore
            512, // paralell game fetches
        )
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
