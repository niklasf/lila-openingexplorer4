use std::{
    convert::Infallible,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, Query, State, rejection::QueryRejection},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use shakmaty::{Color, EnPassantMode, Position, fen::Fen, san::SanPlus, variant::VariantPosition};
use tokio::{sync::Semaphore, task};

use crate::{
    packed::SubStats,
    player::{
        InvalidUserName, PlayerDb, UserId,
        db::WARM_CF,
        explore::{PlayerGameResponse, PlayerGameWithUci, PlayerMove, PlayerResponse, prepare},
        indexer::Indexer,
        keys::PlayerGameIndex,
        query::PlayerQuery,
        queue::{Progress, Ticket},
    },
    server::{
        listen,
        openings::{self, Opening, Openings},
        query::PlayError,
        response::Stats,
    },
    util::{Counter, Histogram, push_counters, push_field},
};

#[derive(Debug, Clone)]
pub struct PlayerServerConfig {
    pub bind: SocketAddr,
    pub admin_bind: Option<SocketAddr>,
    pub cors: bool,
    /// Concurrent database reads.
    pub blocking_threads: usize,
    pub openings_dir: Option<PathBuf>,
    pub no_openings: bool,
    /// Snapshots at most this often while indexing.
    pub debounce: Duration,
    /// An empty line after this long without a snapshot.
    pub keep_alive: Duration,
}

impl Default for PlayerServerConfig {
    fn default() -> PlayerServerConfig {
        PlayerServerConfig {
            bind: "127.0.0.1:9003".parse().expect("address"),
            admin_bind: None,
            cors: false,
            blocking_threads: 4,
            openings_dir: None,
            no_openings: false,
            debounce: Duration::from_secs(1),
            keep_alive: Duration::from_secs(8),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("bad request: {0}")]
    Query(#[from] QueryRejection),
    #[error("bad request: {0}")]
    Play(#[from] PlayError),
    #[error("bad request: {0}")]
    User(#[from] InvalidUserName),
    #[error("indexing queue is full")]
    QueueFull,
    #[error("not found")]
    NotFound,
    #[error("database: {0}")]
    Db(#[from] crate::player::PlayerDbError),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match self {
            Error::Query(_) | Error::Play(_) | Error::User(_) => StatusCode::BAD_REQUEST,
            Error::QueueFull => StatusCode::SERVICE_UNAVAILABLE,
            Error::NotFound => StatusCode::NOT_FOUND,
            Error::Db(_) => {
                log::error!("{self}");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, self.to_string()).into_response()
    }
}

/// Ply buckets of requests, by tens up to 50 and more.
const PLY_BUCKETS: usize = 6;

#[derive(Default)]
pub struct ServerMetrics {
    pub requests: Counter,
    pub bad_requests: Counter,
    pub queue_full: Counter,
    /// Streams that ended (final snapshot sent).
    pub streams_complete: Counter,
    /// Streams that the client closed before the final snapshot.
    pub streams_gone: Counter,
    pub stream_micros: Counter,
    pub snapshots: Counter,
    /// Snapshots that repeated the last read with a new queue position.
    pub snapshots_repeated: Counter,
    pub keep_alives: Counter,
    /// Database reads of snapshots (on the blocking pool).
    pub read: Histogram,
    /// Snapshots by ply of the position: partial, final.
    pub ply: [(Counter, Counter); PLY_BUCKETS],
    pub warm_hits: Counter,
    pub cold_candidates: Counter,
    pub cold_validated: Counter,
    pub players_deleted: Counter,
}

impl ServerMetrics {
    /// The fields for `/monitor`. Takes the maximum of the read histogram.
    fn fields(&self, out: &mut Vec<String>) {
        push_counters(
            out,
            [
                ("requests", &self.requests),
                ("bad_requests", &self.bad_requests),
                ("queue_full", &self.queue_full),
                ("streams_complete", &self.streams_complete),
                ("streams_gone", &self.streams_gone),
                ("stream_us", &self.stream_micros),
                ("snapshots", &self.snapshots),
                ("snapshots_repeated", &self.snapshots_repeated),
                ("keep_alives", &self.keep_alives),
                ("warm_hits", &self.warm_hits),
                ("cold_candidates", &self.cold_candidates),
                ("cold_validated", &self.cold_validated),
                ("players_deleted", &self.players_deleted),
            ],
        );
        for (i, (partial, complete)) in self.ply.iter().enumerate() {
            let bucket = if i + 1 == PLY_BUCKETS {
                format!("ply_ge{}", i * 10)
            } else {
                format!("ply_lt{}", (i + 1) * 10)
            };
            push_field(out, &format!("{bucket}_partial"), partial.get());
            push_field(out, &format!("{bucket}_final"), complete.get());
        }
        self.read.fields("read", out);
    }
}

pub struct PlayerState {
    config: PlayerServerConfig,
    indexer: Arc<Indexer>,
    openings: RwLock<Openings>,
    semaphore: Semaphore,
    pub metrics: ServerMetrics,
    open_streams: AtomicU64,
}

type Shared = Arc<PlayerState>;

fn ply(pos: &VariantPosition) -> usize {
    (pos.fullmoves().get() as usize - 1) * 2 + usize::from(pos.turn() == Color::Black)
}

struct Request {
    query: PlayerQuery,
    pos: VariantPosition,
    opening: Option<Opening>,
}

impl PlayerState {
    fn read(&self, request: &Request) -> Result<PlayerResponse, Error> {
        let started = Instant::now();
        let query = &request.query;
        let db = self.indexer.db();
        let uh = db.user_hash(&query.player);
        let reader = db.reader();
        let empty = || PlayerResponse {
            opening: request.opening.clone(),
            queue_position: 0,
            total: Stats::default(),
            moves: Vec::new(),
            recent_games: Vec::new(),
        };
        let Some(status) = reader.status(uh)? else {
            return Ok(empty());
        };
        let (mut entry, stats) = reader.position(&status, query.color, &request.pos)?;
        let m = &self.metrics;
        if stats.warm_hit {
            m.warm_hits.inc();
        }
        m.cold_candidates.add(stats.cold_candidates);
        m.cold_validated.add(stats.cold_validated);
        let mut buf = Vec::new();
        entry.pack_entire(&mut buf);
        let prepared = prepare(&buf, &query.filter, &query.limits);

        // Load the referenced games.
        let mut indexes: Vec<PlayerGameIndex> =
            prepared.moves.iter().filter_map(|m| m.game).collect();
        indexes.extend(prepared.recent_games.iter().map(|&(_, index)| index));
        indexes.sort_unstable();
        indexes.dedup();
        let games = reader.games(uh, &indexes)?;
        let game = |index: PlayerGameIndex| -> Option<PlayerGameResponse> {
            let i = indexes.binary_search(&index).ok()?;
            games[i]
                .as_ref()
                .map(|game| PlayerGameResponse::new(game, &status.name))
        };

        let openings = self.openings.read().expect("read openings");
        let pos = &request.pos;
        let moves = prepared
            .moves
            .iter()
            .filter_map(|pm| {
                let uci = pm.m.unpack();
                let Ok(m) = uci.to_move(pos) else {
                    log::warn!(
                        "skipping illegal move {uci} in {}",
                        Fen::from_position(pos, EnPassantMode::Legal)
                    );
                    return None;
                };
                let mut after = pos.clone();
                let san = SanPlus::from_move_and_play_unchecked(&mut after, m);
                Some(PlayerMove {
                    uci,
                    san,
                    average_opponent_rating: pm.stats.average_rating().map(i32::from),
                    performance: pm.stats.performance(query.color),
                    stats: Stats::from(&pm.stats),
                    game: pm.game.and_then(game),
                    opening: openings.classify_exact(&after).cloned(),
                })
            })
            .collect();
        let recent_games = prepared
            .recent_games
            .iter()
            .filter_map(|&(m, index)| {
                game(index).map(|game| PlayerGameWithUci {
                    uci: m.unpack(),
                    game,
                })
            })
            .collect();
        let total: &SubStats = &prepared.total;
        let response = PlayerResponse {
            opening: request.opening.clone(),
            queue_position: 0,
            total: Stats::from(total),
            moves,
            recent_games,
        };
        m.read.record(started.elapsed());
        Ok(response)
    }

    fn record_ply(&self, request: &Request, done: bool) {
        let bucket = (ply(&request.pos) / 10).min(PLY_BUCKETS - 1);
        let (partial, complete) = &self.metrics.ply[bucket];
        if done { complete } else { partial }.inc();
    }
}

async fn blocking<T: Send + 'static>(
    state: &Shared,
    f: impl FnOnce(&Shared) -> T + Send + 'static,
) -> T {
    let _permit = state
        .semaphore
        .acquire()
        .await
        .expect("semaphore not closed");
    let state = Arc::clone(state);
    task::spawn_blocking(move || f(&state))
        .await
        .expect("blocking task")
}

fn line(response: &PlayerResponse) -> Bytes {
    let mut buf = serde_json::to_vec(response).expect("serialize");
    buf.push(b'\n');
    Bytes::from(buf)
}

/// Counts an open stream, and how it ended.
struct StreamGuard {
    state: Shared,
    started: Instant,
    complete: bool,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let m = &self.state.metrics;
        if self.complete {
            m.streams_complete.inc();
        } else {
            m.streams_gone.inc();
        }
        m.stream_micros
            .add(u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX));
        self.state.open_streams.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Streams the snapshots of a request (first immediately,
/// then at most one per `debounce` while indexing, each re-reading only
/// after committed batches, and the final one immediately).
async fn snapshots(
    state: Shared,
    request: Arc<Request>,
    mut ticket: Ticket,
    tx: tokio::sync::mpsc::Sender<Result<Bytes, Infallible>>,
) {
    state.open_streams.fetch_add(1, Ordering::Relaxed);
    let mut guard = StreamGuard {
        state: Arc::clone(&state),
        started: Instant::now(),
        complete: false,
    };
    let debounce = state.config.debounce;
    let keep_alive = state.config.keep_alive;
    let mut progress = ticket.progress();
    let mut read_generation = progress.generation;
    let read = {
        let request = Arc::clone(&request);
        blocking(&state, move |state| state.read(&request)).await
    };
    let mut last = match read {
        Ok(response) => response,
        Err(err) => {
            log::error!("player snapshot: {err}");
            return;
        }
    };
    let mut last_key = None;
    let mut last_snapshot = Instant::now();
    let mut last_line = Instant::now();
    loop {
        last.queue_position = progress.queue_position;
        // Consecutive snapshots with the same queue position and total are
        // dropped, as before (also an unchanged final snapshot).
        let key = (last.queue_position, last.total_games());
        if last_key != Some(key) {
            state.metrics.snapshots.inc();
            state.record_ply(&request, progress.done);
            if tx.send(Ok(line(&last))).await.is_err() {
                return;
            }
            last_key = Some(key);
            last_snapshot = Instant::now();
            last_line = last_snapshot;
        }
        if progress.done {
            guard.complete = true;
            return;
        }

        // Wait for a change, with keep-alive lines meanwhile.
        loop {
            tokio::select! {
                changed = ticket.changed() => {
                    progress = changed;
                    break;
                }
                _ = tokio::time::sleep_until((last_line + keep_alive).into()) => {
                    state.metrics.keep_alives.inc();
                    if tx.send(Ok(Bytes::from_static(b"\n"))).await.is_err() {
                        return;
                    }
                    last_line = Instant::now();
                }
                _ = tx.closed() => return,
            }
        }
        // Debounce, except for the final snapshot.
        if !progress.done {
            tokio::select! {
                _ = tokio::time::sleep_until((last_snapshot + debounce).into()) => (),
                _ = tx.closed() => return,
            }
            progress = merge_progress(progress, ticket.progress());
        }
        if progress.generation != read_generation || progress.done {
            read_generation = progress.generation;
            let request = Arc::clone(&request);
            match blocking(&state, move |state| state.read(&request)).await {
                Ok(response) => last = response,
                Err(err) => {
                    log::error!("player snapshot: {err}");
                    return;
                }
            }
        } else {
            state.metrics.snapshots_repeated.inc();
        }
    }
}

/// The latest progress (a dropped ticket reports done).
fn merge_progress(waited: Progress, now: Progress) -> Progress {
    Progress {
        done: waited.done || now.done,
        ..now
    }
}

async fn player(
    State(state): State<Shared>,
    query: Result<Query<PlayerQuery>, QueryRejection>,
) -> Result<Response, Error> {
    let query = match query {
        Ok(Query(query)) => query,
        Err(err) => {
            state.metrics.bad_requests.inc();
            return Err(err.into());
        }
    };
    let play = query
        .play
        .position(&state.openings.read().expect("read openings"))
        .inspect_err(|_| state.metrics.bad_requests.inc())?;
    state.metrics.requests.inc();
    let ticket = state.indexer.request(&query.player).await.map_err(|_| {
        state.metrics.queue_full.inc();
        log::error!("not indexing {} because the queue is full", query.player);
        Error::QueueFull
    })?;
    let request = Arc::new(Request {
        query,
        pos: play.pos,
        opening: play.opening,
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    task::spawn(snapshots(Arc::clone(&state), request, ticket, tx));
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(futures_util::stream::poll_fn(
            move |cx| rx.poll_recv(cx),
        )))
        .expect("response"))
}

async fn monitor(State(state): State<Shared>) -> String {
    let mut fields = Vec::new();
    state.metrics.fields(&mut fields);
    push_field(
        &mut fields,
        "open_streams",
        state.open_streams.load(Ordering::Relaxed),
    );
    let indexer = &state.indexer;
    let queue = indexer.queue().stats();
    push_field(&mut fields, "queue_waiting", queue.waiting as u64);
    push_field(&mut fields, "queue_running", queue.running as u64);
    push_field(&mut fields, "queue_busy", queue.busy as u64);
    push_field(&mut fields, "lila_streams", indexer.lila().busy() as u64);
    indexer.metrics.fields(&mut fields);
    let db = indexer.db();
    db.counters.fields(&mut fields);
    push_field(&mut fields, "block_cache_usage", db.cache_usage() as u64);
    for cf in PlayerDb::column_families() {
        for (prop, name) in [
            ("rocksdb.estimate-num-keys", "keys"),
            ("rocksdb.total-sst-files-size", "sst_bytes"),
            (
                "rocksdb.estimate-pending-compaction-bytes",
                "pending_compaction_bytes",
            ),
        ] {
            if let Ok(Some(value)) = db.cf_int_property(cf, prop) {
                push_field(&mut fields, &format!("{cf}_{name}"), value);
            }
        }
    }
    for (prop, name) in [
        ("rocksdb.actual-delayed-write-rate", "delayed_write_rate"),
        ("rocksdb.is-write-stopped", "write_stopped"),
        ("rocksdb.num-running-compactions", "running_compactions"),
        ("rocksdb.num-running-flushes", "running_flushes"),
    ] {
        if let Ok(Some(value)) = db.cf_int_property(WARM_CF, prop) {
            push_field(&mut fields, name, value);
        }
    }
    for (name, ticker) in [
        ("stall_us", rocksdb::statistics::Ticker::StallMicros),
        (
            "compact_read_bytes",
            rocksdb::statistics::Ticker::CompactReadBytes,
        ),
        (
            "compact_write_bytes",
            rocksdb::statistics::Ticker::CompactWriteBytes,
        ),
        (
            "flush_write_bytes",
            rocksdb::statistics::Ticker::FlushWriteBytes,
        ),
        (
            "block_cache_miss",
            rocksdb::statistics::Ticker::BlockCacheMiss,
        ),
        (
            "block_cache_hit",
            rocksdb::statistics::Ticker::BlockCacheHit,
        ),
    ] {
        if let Some(value) = db.ticker(ticker) {
            push_field(&mut fields, name, value);
        }
    }
    push_field(
        &mut fields,
        "openings",
        state.openings.read().expect("read openings").len() as u64,
    );
    format!("opening_explorer_player {}\n", fields.join(","))
}

async fn delete_player(
    State(state): State<Shared>,
    Path(user): Path<String>,
) -> Result<StatusCode, Error> {
    let user = UserId::from_name(&user)?;
    let existed = state.indexer.delete(&user).await?;
    Ok(if existed {
        state.metrics.players_deleted.inc();
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

async fn db_prop(State(state): State<Shared>, Path(prop): Path<String>) -> Result<String, Error> {
    blocking(&state, move |state| {
        state
            .indexer
            .db()
            .property(&prop)
            .ok()
            .flatten()
            .ok_or(Error::NotFound)
    })
    .await
}

async fn cf_prop(
    State(state): State<Shared>,
    Path((cf, prop)): Path<(String, String)>,
) -> Result<String, Error> {
    blocking(&state, move |state| {
        state
            .indexer
            .db()
            .cf_property(&cf, &prop)
            .ok()
            .flatten()
            .ok_or(Error::NotFound)
    })
    .await
}

pub fn router(
    config: PlayerServerConfig,
    indexer: Arc<Indexer>,
    openings: Openings,
) -> (Router, Router, Shared) {
    let state: Shared = Arc::new(PlayerState {
        semaphore: Semaphore::new(config.blocking_threads),
        openings: RwLock::new(openings),
        metrics: ServerMetrics::default(),
        open_streams: AtomicU64::new(0),
        config,
        indexer,
    });
    let app = Router::new()
        .route("/player", get(player))
        .route("/personal", get(player))
        .with_state(Arc::clone(&state));
    let admin = Router::new()
        .route("/monitor", get(monitor))
        .route("/monitor/db/{prop}", get(db_prop))
        .route("/monitor/cf/{cf}/{prop}", get(cf_prop))
        .route("/player/{id}", delete(delete_player))
        .with_state(Arc::clone(&state));
    let app = if state.config.cors {
        app.layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        ))
    } else {
        app
    };
    (app, admin, state)
}

pub async fn serve(
    config: PlayerServerConfig,
    indexer: Arc<Indexer>,
) -> Result<(), Box<dyn std::error::Error>> {
    let openings = Openings::initial(config.openings_dir.as_deref())?;
    let download = config.openings_dir.is_none() && !config.no_openings;
    let (bind, admin_bind) = (config.bind, config.admin_bind);
    let (app, admin, state) = router(config, indexer, openings);
    if download {
        let state = Arc::clone(&state);
        task::spawn(async move { openings::periodic_refresh(&state.openings, || ()).await });
    }
    listen::serve_until_signal(bind, app, admin_bind.map(|addr| (addr, admin))).await?;
    Ok(())
}
