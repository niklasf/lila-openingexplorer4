use std::{fs, net::SocketAddr, num::NonZeroUsize, path::PathBuf, process, thread, time::Duration};

use clap::{Args, Parser, Subcommand};
use lila_openingexplorer4::{
    player::{
        PlayerDb, PlayerDbConfig,
        indexer::{Indexer, IndexerConfig},
        lila::{LilaClient, LilaClientConfig},
        policy::RunPolicy,
        server::PlayerServerConfig,
    },
    server::{AppConfig, Database, DbConfig, Kind, LilaConfig, ingest, serve},
    util::ByteSize,
};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

/// Opening explorer servers.
///
/// `lichess` and `masters` serve the read-only SST files produced by
/// parquet_to_games and parquet_to_positions: first create a database
/// directory with `ingest`, which hard-links the SST files (they must be on
/// the same file system), then serve it, strictly read-only.
///
/// `player` serves the personal opening explorer (/player) from its own
/// live database, indexing players' games from lila on demand.
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
    /// Serves /player (and /personal) from a writable database of indexed
    /// players, created if missing.
    Player(PlayerArgs),
}

#[derive(Args)]
struct PlayerArgs {
    /// Database directory (created if missing). Only one process may open
    /// it.
    #[arg(long)]
    db: PathBuf,
    /// Binding address of the public endpoints.
    #[arg(long, default_value = "127.0.0.1:9003")]
    bind: SocketAddr,
    /// Binding address of administration and monitoring (/monitor and
    /// DELETE /player/{id}), to be kept private. Without it, these are not
    /// served at all.
    #[arg(long)]
    admin_bind: Option<SocketAddr>,
    /// Allow access from all origins.
    #[arg(long)]
    cors: bool,
    /// Size of the RocksDB block cache.
    #[arg(long, default_value = "4g")]
    cache: ByteSize,
    /// Plies indexed per game. Must not change on an existing database.
    #[arg(long, default_value = "50")]
    max_ply: usize,
    /// Directory of the write ahead log (default: in the database).
    #[arg(long)]
    wal_dir: Option<PathBuf>,
    /// Directory for SST files with a target size, like
    /// /nvme/player:200g. Repeat for more: newer data (upper levels) goes
    /// to the first ones, older data to later ones (for example the last
    /// level on HDDs).
    #[arg(long = "db-path", value_parser = parse_db_path)]
    db_paths: Vec<(PathBuf, u64)>,
    /// Limit flushes and compactions (on all paths) to this many bytes
    /// per second (0 for no limit). Bounds the HDD writes when compactions
    /// rewrite the last level, which are rare; indexing bursts stay in the
    /// upper levels and wrote far less than this in the benchmarks.
    #[arg(long, default_value = "16m")]
    rate_limit: ByteSize,
    /// With several --db-path: the last level holds this many times the
    /// level above (RocksDB's dynamic level sizes are not available with
    /// several paths, so the server sets the level targets itself).
    #[arg(long, default_value = "10")]
    level_ratio: f64,
    /// Collect RocksDB statistics (tickers in /monitor).
    #[arg(long)]
    db_statistics: bool,
    /// Parallel index runs, which is also the most parallel streams from
    /// lila (every request counts, retries included).
    #[arg(long, default_value = "8", help_heading = "Indexer")]
    indexers: usize,
    /// Players waiting for indexing at most (503 when full).
    #[arg(long, default_value = "2000", help_heading = "Indexer")]
    queue_capacity: usize,
    /// Queue players strictly in order of arrival, instead of players that
    /// already have an index (short incremental runs) before first runs.
    #[arg(long, help_heading = "Indexer")]
    fifo: bool,
    /// No new index run of a player within this many seconds.
    #[arg(long, default_value = "120", help_heading = "Indexer")]
    cooldown: u64,
    /// Revisit ongoing games at most this often (seconds).
    #[arg(long, default_value = "86400", help_heading = "Indexer")]
    revisit_interval: u64,
    /// Games per chunk (one atomic write) at most.
    #[arg(long, default_value = "2000", help_heading = "Indexer")]
    chunk_games: usize,
    /// Write a chunk at least this often while indexing (milliseconds).
    #[arg(long, default_value = "1000", help_heading = "Indexer")]
    chunk_interval_ms: u64,
    /// Give up the stream of a run after this many seconds and continue at
    /// the end of the queue (0 to run until done). 30 s with --fifo off was
    /// best in the queue simulation of the launch (empty database).
    #[arg(long, default_value = "30", help_heading = "Indexer")]
    max_run: u64,
    /// Rebuild a player after at least this many new games ...
    #[arg(long, default_value = "1000", help_heading = "Indexer")]
    rebuild_min_games: u32,
    /// ... and this share of the games at the last rebuild.
    #[arg(long, default_value = "0.5", help_heading = "Indexer")]
    rebuild_factor: f64,
    /// Parallel rebuilds at most.
    #[arg(long, default_value = "1", help_heading = "Indexer")]
    rebuilds: usize,
    /// Seconds to wait for lila's response and between chunks of it.
    #[arg(long, default_value = "60", help_heading = "Indexer")]
    lila_timeout: u64,
    /// Base url of the lila instance to index games from.
    #[arg(
        long,
        env = "LILA_URL",
        default_value = "https://lichess.org",
        help_heading = "Indexer"
    )]
    lila: String,
    #[command(flatten, next_help_heading = "Indexer")]
    bearer: LilaBearerArgs,
    /// Concurrent database reads of requests.
    #[arg(long, default_value = "4", help_heading = "Server")]
    blocking_threads: usize,
    /// Directory with a.tsv to e.tsv of lichess-org/chess-openings.
    #[arg(long, help_heading = "Server", conflicts_with = "no_openings")]
    openings_dir: Option<PathBuf>,
    /// Do not load opening names at all.
    #[arg(long, help_heading = "Server")]
    no_openings: bool,
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
    /// Binding address of the public endpoints.
    #[arg(long, default_value = "127.0.0.1:9002")]
    bind: SocketAddr,
    /// Binding address for optional administration and monitoring endpoints.
    /// Should not be exposed publicly.
    #[arg(long)]
    admin_bind: Option<SocketAddr>,
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

/// The token for lila.
#[derive(Args)]
struct LilaBearerArgs {
    /// Token of https://lichess.org/@/OpeningExplorer.
    #[arg(long, env = "EXPLORER_BEARER", hide_env_values = true)]
    lila_bearer: Option<String>,
    /// File with the token, read once at startup.
    #[arg(long, env = "EXPLORER_BEARER_FILE", conflicts_with = "lila_bearer")]
    lila_bearer_file: Option<PathBuf>,
}

impl LilaBearerArgs {
    /// The token, or the token from the file (exits if it cannot be read).
    fn bearer(self) -> Option<String> {
        self.lila_bearer.or_else(|| {
            self.lila_bearer_file
                .map(|file| match fs::read_to_string(&file) {
                    Ok(bearer) => bearer.trim().to_owned(),
                    Err(err) => {
                        eprintln!("error: reading bearer file {}: {err}", file.display());
                        process::exit(1);
                    }
                })
        })
    }
}

#[derive(Args)]
#[command(next_help_heading = "Blacklist (lichess only)")]
struct LilaArgs {
    /// Base url of the lila instance to get the blacklist of marked
    /// players from (/api/stream/mod-marked-since).
    #[arg(long, env = "LILA_URL", default_value = "https://lichess.org")]
    lila: String,
    #[command(flatten)]
    bearer: LilaBearerArgs,
    /// Seconds between blacklist updates.
    #[arg(long, default_value = "10380")]
    blacklist_refresh: u64,
    /// Do not hide games of blacklisted players.
    #[arg(long)]
    no_blacklist: bool,
}

fn parse_db_path(s: &str) -> Result<(PathBuf, u64), String> {
    let (path, size) = s
        .rsplit_once(':')
        .ok_or_else(|| "expected PATH:SIZE".to_owned())?;
    let size: ByteSize = size.parse().map_err(|err| format!("{err}"))?;
    Ok((PathBuf::from(path), size.into()))
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
        Command::Player(args) => run_player(args),
    }
}

fn run_player(args: PlayerArgs) {
    raise_fd_limit();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(args.blocking_threads + args.indexers + args.rebuilds + 16)
        .build()
        .expect("tokio runtime");
    let db = match PlayerDb::open(
        &args.db,
        PlayerDbConfig {
            cache_bytes: args.cache.into(),
            max_ply: args.max_ply,
            rate_limit: (args.rate_limit.0 > 0).then_some(args.rate_limit.0),
            wal_dir: args.wal_dir,
            db_paths: args.db_paths.clone(),
            statistics: args.db_statistics,
            level_ratio: args.level_ratio,
            ..PlayerDbConfig::default()
        },
    ) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("error: {err}");
            process::exit(1);
        }
    };
    let result = runtime.block_on(async {
        let lila = std::sync::Arc::new(LilaClient::new(LilaClientConfig {
            url: args.lila.trim_end_matches('/').to_owned(),
            bearer: args.bearer.bearer(),
            streams: args.indexers,
            read_timeout: Duration::from_secs(args.lila_timeout),
            ..LilaClientConfig::default()
        }));
        let db = std::sync::Arc::new(db);
        if !args.db_paths.is_empty() {
            // Static level sizes follow the growth of the last level.
            let db = std::sync::Arc::downgrade(&db);
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(600));
                    let Some(db) = db.upgrade() else {
                        break;
                    };
                    if let Err(err) = db.tune_levels() {
                        log::error!("tuning levels: {err}");
                    }
                }
            });
        }
        let indexer = Indexer::start(
            db,
            lila,
            IndexerConfig {
                indexers: args.indexers,
                queue_capacity: args.queue_capacity,
                known_first: !args.fifo,
                policy: RunPolicy {
                    cooldown: Duration::from_secs(args.cooldown),
                    revisit_interval: Duration::from_secs(args.revisit_interval),
                },
                chunk_games: args.chunk_games,
                chunk_interval: Duration::from_millis(args.chunk_interval_ms),
                max_run: (args.max_run > 0).then(|| Duration::from_secs(args.max_run)),
                rebuild_min_games: args.rebuild_min_games,
                rebuild_factor: args.rebuild_factor,
                rebuilds: args.rebuilds,
                ..IndexerConfig::default()
            },
        );
        lila_openingexplorer4::player::server::serve(
            PlayerServerConfig {
                bind: args.bind,
                admin_bind: args.admin_bind,
                cors: args.cors,
                blocking_threads: args.blocking_threads,
                openings_dir: args.openings_dir,
                no_openings: args.no_openings,
                ..PlayerServerConfig::default()
            },
            indexer,
        )
        .await
    });
    if let Err(err) = result {
        eprintln!("error: {err}");
        process::exit(1);
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
        admin_bind: args.admin_bind,
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
            bearer: args.lila.bearer.bearer(),
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
