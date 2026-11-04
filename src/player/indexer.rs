//! Indexers: take players from the queue, stream their new games from lila,
//! and write them in chunks.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    sync::{Semaphore, mpsc},
    task,
};

use crate::{
    player::{
        PlayerDb, UserId,
        lila::{Classified, ExportGame, LilaClient, LilaError, Skip},
        queue::{ProgressHandle, Queue, QueueFull, QueueItem, Ticket},
        record::PlayerGame,
        status::{IndexRun, PlayerStatus, RunPolicy},
    },
    util::civil::now_ms,
};

#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// Parallel index runs (each with one stream from lila).
    pub indexers: usize,
    /// Players waiting at most.
    pub queue_capacity: usize,
    /// Queue players that have an index (incremental runs) before first
    /// runs.
    pub known_first: bool,
    pub policy: RunPolicy,
    /// Games per chunk at most.
    pub chunk_games: usize,
    /// A chunk is written at the latest after this time, so that waiting
    /// requests see progress.
    pub chunk_interval: Duration,
    /// Index runs give up their stream after this time and continue at the
    /// end of the queue (`None` to run until all games are indexed).
    pub max_run: Option<Duration>,
    /// Rebuild a player after `max(rebuild_min_games, rebuild_factor *
    /// games at the last rebuild)` new games.
    pub rebuild_min_games: u32,
    pub rebuild_factor: f64,
    /// Parallel rebuilds at most.
    pub rebuilds: usize,
    /// Retries of a failed stream, in the same slot.
    pub retries: u32,
    pub retry_delay: Duration,
}

impl Default for IndexerConfig {
    fn default() -> IndexerConfig {
        IndexerConfig {
            indexers: 8,
            queue_capacity: 2000,
            known_first: true,
            policy: RunPolicy::default(),
            chunk_games: 2000,
            chunk_interval: Duration::from_secs(1),
            max_run: Some(Duration::from_secs(30)),
            rebuild_min_games: 1000,
            rebuild_factor: 0.5,
            rebuilds: 1,
            retries: 3,
            retry_delay: Duration::from_secs(5),
        }
    }
}

/// Counters of the indexers, for `/monitor`.
#[derive(Default)]
pub struct IndexerMetrics {
    pub runs: AtomicU64,
    pub runs_complete: AtomicU64,
    pub runs_not_found: AtomicU64,
    pub runs_failed: AtomicU64,
    pub runs_abandoned: AtomicU64,
    pub runs_requeued: AtomicU64,
    pub games_received: AtomicU64,
    pub skipped_ongoing: AtomicU64,
    pub skipped_unfinished: AtomicU64,
    pub skipped_anonymous: AtomicU64,
    pub skipped_invalid: AtomicU64,
    pub malformed_lines: AtomicU64,
    pub lila_errors: AtomicU64,
    pub lila_timeouts: AtomicU64,
    pub stream_retries: AtomicU64,
    /// Time that streams waited for chunk writes.
    pub writer_wait_micros: AtomicU64,
    /// Time that streams were open.
    pub stream_micros: AtomicU64,
    pub rebuild_errors: AtomicU64,
    pub queue_full: AtomicU64,
}

fn inc(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// How an index run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunEnd {
    /// All games received.
    Complete,
    /// Unknown or closed account.
    NotFound,
    /// Too many errors.
    Failed,
    /// No request waits for the player any more.
    Abandoned,
    /// Out of time, to continue later.
    Requeue,
}

/// A chunk of an index run, for the writer.
#[derive(Default)]
struct Chunk {
    games: Vec<PlayerGame>,
    latest_created_at: u64,
    /// Earliest `createdAt` of the ongoing games of the chunk.
    ongoing: Option<u64>,
    name: Option<String>,
    finish: Option<Finish>,
}

struct Finish {
    indexed_at: u64,
    /// For a complete revisit: the new earliest ongoing game.
    revisit: Option<Option<u64>>,
}

fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

pub struct Indexer {
    queue: Arc<Queue>,
    db: Arc<PlayerDb>,
    lila: Arc<LilaClient>,
    config: IndexerConfig,
    rebuilds: Arc<Semaphore>,
    pub metrics: IndexerMetrics,
}

impl Indexer {
    /// Starts the indexer tasks.
    pub fn start(db: Arc<PlayerDb>, lila: Arc<LilaClient>, config: IndexerConfig) -> Arc<Indexer> {
        let indexer = Arc::new(Indexer {
            queue: Arc::new(Queue::new(config.queue_capacity, config.known_first)),
            rebuilds: Arc::new(Semaphore::new(config.rebuilds.max(1))),
            db,
            lila,
            config,
            metrics: IndexerMetrics::default(),
        });
        for idx in 0..indexer.config.indexers {
            let indexer = Arc::clone(&indexer);
            task::spawn(async move {
                loop {
                    let item = indexer.queue.acquire().await;
                    indexer.index(idx, item).await;
                }
            });
        }
        indexer
    }

    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    pub fn lila(&self) -> &LilaClient {
        &self.lila
    }

    pub fn db(&self) -> &Arc<PlayerDb> {
        &self.db
    }

    /// The ticket for a request of the player: queues the player if an index
    /// run is due.
    pub async fn request(&self, user: &UserId) -> Result<Ticket, QueueFull> {
        if let Some(ticket) = self.queue.watch(user) {
            return Ok(ticket);
        }
        let db = Arc::clone(&self.db);
        let uh = user.hash();
        let status = task::spawn_blocking(move || db.status(uh))
            .await
            .expect("status task");
        let status = match status {
            Ok(status) => status,
            Err(err) => {
                log::error!("reading status of {user}: {err}");
                return Ok(Ticket::completed());
            }
        };
        if self
            .config
            .policy
            .next_run(status.as_ref(), now_ms())
            .is_none()
        {
            return Ok(Ticket::completed());
        }
        let known = status.is_some_and(|status| status.games > 0);
        self.queue.submit(user.clone(), known).inspect_err(|_| {
            inc(&self.metrics.queue_full);
        })
    }

    async fn index(self: &Arc<Self>, idx: usize, item: QueueItem) {
        let user = item.user().clone();
        let uh = user.hash();
        let db = Arc::clone(&self.db);
        let status = match task::spawn_blocking(move || db.status(uh))
            .await
            .expect("status task")
        {
            Ok(status) => status,
            Err(err) => {
                log::error!("indexer {idx:02}: reading status of {user}: {err}");
                return;
            }
        };
        let Some(run) = self.config.policy.next_run(status.as_ref(), now_ms()) else {
            // Indexed meanwhile.
            return;
        };
        let max_ply = self.db.config().max_ply;
        let status = status.unwrap_or_else(|| PlayerStatus::new(user.clone(), max_ply as u32));
        let games_before = status.games;
        inc(&self.metrics.runs);
        let started = Instant::now();
        log::debug!(
            "indexer {idx:02}: starting {user} since {}{}",
            run.since,
            if run.revisit { " (revisit)" } else { "" }
        );

        let (tx, rx) = mpsc::channel::<Chunk>(2);
        let writer = {
            let db = Arc::clone(&self.db);
            let progress = item.progress();
            task::spawn_blocking(move || write_chunks(&db, status, run, rx, progress))
        };
        let end = self.stream(idx, &item, &user, run, &tx).await;
        drop(tx);
        let status = writer.await.expect("writer task");

        let counter = match end {
            RunEnd::Complete => &self.metrics.runs_complete,
            RunEnd::NotFound => &self.metrics.runs_not_found,
            RunEnd::Failed => &self.metrics.runs_failed,
            RunEnd::Abandoned => &self.metrics.runs_abandoned,
            RunEnd::Requeue => &self.metrics.runs_requeued,
        };
        inc(counter);
        let Some(status) = status else {
            return;
        };
        let new_games = status.games - games_before;
        let elapsed = started.elapsed();
        if new_games > 0 || end != RunEnd::Complete {
            log::info!(
                "indexer {idx:02}: {user}: {new_games} new games in {elapsed:.3?} ({:.0} games/s), {end:?}",
                f64::from(new_games) / elapsed.as_secs_f64()
            );
        }

        if end == RunEnd::Requeue && !item.abandoned() {
            item.requeue();
            return;
        }
        if end != RunEnd::Requeue
            && self.db.rebuild_due(
                &status,
                self.config.rebuild_min_games,
                self.config.rebuild_factor,
            )
        {
            let busy = item.finish_busy();
            let indexer = Arc::clone(self);
            task::spawn(async move {
                let _permit = Arc::clone(&indexer.rebuilds)
                    .acquire_owned()
                    .await
                    .expect("rebuild semaphore");
                let db = Arc::clone(&indexer.db);
                let started = Instant::now();
                match task::spawn_blocking(move || db.rebuild(uh))
                    .await
                    .expect("rebuild task")
                {
                    Ok(Some(stats)) => log::info!(
                        "rebuilt {user} in {:.3?}: {} games, {} warm, {} cold groups, {} bytes{}",
                        started.elapsed(),
                        stats.games,
                        stats.warm_entries,
                        stats.cold_groups,
                        stats.bytes,
                        if stats.ingested { ", ingested" } else { "" }
                    ),
                    Ok(None) => (),
                    Err(err) => {
                        inc(&indexer.metrics.rebuild_errors);
                        log::error!("rebuilding {user}: {err}");
                    }
                }
                drop(busy);
            });
        }
    }

    /// Streams the games of the run from lila into chunks for the writer.
    async fn stream(
        &self,
        idx: usize,
        item: &QueueItem,
        user: &UserId,
        run: IndexRun,
        tx: &mpsc::Sender<Chunk>,
    ) -> RunEnd {
        let max_ply = self.db.config().max_ply;
        let mut slot = self.lila.slot().await;
        let started = Instant::now();
        let mut since = run.since;
        let mut failures = 0;
        let mut chunk = Chunk::default();
        let mut last_flush = Instant::now();
        // Ongoing games of this run, for the revisit pointer.
        let mut ongoing: Option<u64> = None;
        let m = &self.metrics;

        let flush = async |chunk: &mut Chunk, finish: Option<Finish>| {
            let chunk = Chunk {
                finish,
                ..std::mem::take(chunk)
            };
            let waited = Instant::now();
            let ok = tx.send(chunk).await.is_ok();
            m.writer_wait_micros
                .fetch_add(waited.elapsed().as_micros() as u64, Ordering::Relaxed);
            ok
        };

        let end = 'run: loop {
            let mut stream = match slot.user_games(user, since).await {
                Ok(stream) => stream,
                Err(LilaError::NotFound) => {
                    log::warn!("indexer {idx:02}: did not find player {user}");
                    break 'run RunEnd::NotFound;
                }
                Err(err) => {
                    if matches!(err, LilaError::Timeout) {
                        inc(&m.lila_timeouts);
                    } else {
                        inc(&m.lila_errors);
                    }
                    log::error!("indexer {idx:02}: request for {user}: {err}");
                    failures += 1;
                    if failures > self.config.retries {
                        break 'run RunEnd::Failed;
                    }
                    inc(&m.stream_retries);
                    tokio::time::sleep(self.config.retry_delay * failures).await;
                    continue;
                }
            };
            loop {
                let line = match stream.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => break 'run RunEnd::Complete,
                    Err(err) => {
                        inc(&m.lila_errors);
                        log::error!("indexer {idx:02}: stream for {user}: {err}");
                        break;
                    }
                };
                if line.trim_ascii().is_empty() {
                    continue;
                }
                let game = match ExportGame::parse(&line) {
                    Ok(game) => game,
                    Err(err) => {
                        inc(&m.malformed_lines);
                        log::warn!("indexer {idx:02}: malformed line for {user}: {err}");
                        continue;
                    }
                };
                inc(&m.games_received);
                since = since.max(game.created_at + 1);
                chunk.latest_created_at = chunk.latest_created_at.max(game.created_at);
                match game.classify(user, max_ply) {
                    Classified::Game { game, name } => {
                        chunk.games.push(*game);
                        chunk.name = Some(name);
                    }
                    Classified::Skip(Skip::Ongoing) => {
                        inc(&m.skipped_ongoing);
                        chunk.ongoing = min_opt(chunk.ongoing, Some(game.created_at));
                        ongoing = min_opt(ongoing, Some(game.created_at));
                    }
                    Classified::Skip(Skip::Unfinished) => inc(&m.skipped_unfinished),
                    Classified::Skip(Skip::Anonymous) => inc(&m.skipped_anonymous),
                    Classified::Skip(Skip::NotPlayer | Skip::Invalid) => inc(&m.skipped_invalid),
                }
                if chunk.games.len() >= self.config.chunk_games
                    || last_flush.elapsed() >= self.config.chunk_interval
                {
                    last_flush = Instant::now();
                    if !flush(&mut chunk, None).await {
                        break 'run RunEnd::Failed;
                    }
                    if item.abandoned() {
                        break 'run RunEnd::Abandoned;
                    }
                    if self
                        .config
                        .max_run
                        .is_some_and(|max_run| started.elapsed() >= max_run)
                    {
                        break 'run RunEnd::Requeue;
                    }
                }
            }
            // The stream broke: retry from the checkpoint, in the same slot.
            drop(stream);
            failures += 1;
            if failures > self.config.retries {
                break 'run RunEnd::Failed;
            }
            inc(&m.stream_retries);
            tokio::time::sleep(self.config.retry_delay).await;
        };
        drop(slot);
        m.stream_micros
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);

        // Finished runs start the cooldown, also after errors (to not
        // retry a failing player at once). Interrupted runs continue from
        // the checkpoint with the next request.
        let finish = match end {
            RunEnd::Complete | RunEnd::NotFound | RunEnd::Failed => Some(Finish {
                indexed_at: now_ms(),
                revisit: (run.revisit && end == RunEnd::Complete).then_some(ongoing),
            }),
            RunEnd::Abandoned | RunEnd::Requeue => None,
        };
        flush(&mut chunk, finish).await;
        end
    }
}

/// Writes the chunks of a run, returning the final status (`None` after a
/// database error).
fn write_chunks(
    db: &PlayerDb,
    mut status: PlayerStatus,
    run: IndexRun,
    mut rx: mpsc::Receiver<Chunk>,
    progress: ProgressHandle,
) -> Option<PlayerStatus> {
    let mut failed = false;
    while let Some(chunk) = rx.blocking_recv() {
        if failed {
            continue;
        }
        status.latest_created_at = status.latest_created_at.max(chunk.latest_created_at);
        if !run.revisit {
            status.revisit_ongoing = min_opt(status.revisit_ongoing, chunk.ongoing);
        }
        if let Some(name) = chunk.name {
            status.name = name;
        }
        if let Some(finish) = &chunk.finish {
            status.indexed_at = finish.indexed_at;
            if let Some(revisit) = finish.revisit {
                status.revisit_ongoing = revisit;
                status.revisited_at = finish.indexed_at;
            }
        }
        if chunk.games.is_empty() && chunk.finish.is_none() && chunk.latest_created_at == 0 {
            continue;
        }
        match db.write_chunk(&mut status, &chunk.games) {
            Ok(stats) => {
                if stats.games > 0 {
                    progress.committed();
                }
            }
            Err(err) => {
                log::error!("writing games of {}: {err}", status.user);
                failed = true;
            }
        }
    }
    (!failed).then_some(status)
}
