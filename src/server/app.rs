use std::{
    net::SocketAddr,
    ops::Deref,
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::Bytes,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use moka::future::Cache;
use rocksdb::DBPinnableSlice;
use shakmaty::{
    san::{San, SanPlus},
    variant::VariantPosition,
};
use tokio::{net::TcpListener, sync::Semaphore, task, time};

use crate::{
    packed::{
        BorrowedLichessGame, CellKey, GameId, GameSeq, GroupHeader, InvalidGameId, LichessEntry,
        Rating, RatingGroup,
    },
    server::{
        db::Database,
        explore::{GameRef, MAX_MASTERS_GAMES, PreparedMove, prepare_lichess, prepare_masters},
        metrics::Metrics,
        openings::Openings,
        pgn::masters_pgn,
        query::{LichessQuery, Limits, MastersQuery, PlayError, PlayPosition},
        replay::{find_visit, position_hash},
        response::{
            ExplorerGame, ExplorerGameWithUci, ExplorerMove, ExplorerResponse, HistoryEntry, Stats,
        },
    },
};

/// Which database the server serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Lichess,
    Masters,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub kind: Kind,
    pub bind: SocketAddr,
    pub cors: bool,
    /// Entries of the response cache.
    pub cache_entries: u64,
    pub cache_ttl: Duration,
    /// Concurrent database requests.
    pub blocking_threads: usize,
    /// Games were indexed up to this ply (`None` for all plies).
    pub max_ply: Option<usize>,
    /// Masters: most recent games of the highest rating groups to load and
    /// rank by rating, for the top games.
    pub masters_top_candidates: usize,
    /// Directory with a.tsv to e.tsv of chess-openings, instead of
    /// downloading them.
    pub openings_dir: Option<PathBuf>,
    /// Do not load opening names at all.
    pub no_openings: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("bad request: {0}")]
    Query(#[from] QueryRejection),
    #[error("bad request: {0}")]
    Play(#[from] PlayError),
    #[error("bad request: {0}")]
    GameId(#[from] InvalidGameId),
    #[error("not found")]
    NotFound,
    #[error("database: {0}")]
    Db(#[from] rocksdb::Error),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match self {
            Error::Query(_) | Error::Play(_) | Error::GameId(_) => {
                (StatusCode::BAD_REQUEST, self.to_string())
            }
            Error::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            Error::Db(_) => {
                log::error!("{self}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_owned(),
                )
            }
        }
        .into_response()
    }
}

type ResponseCache<Q> = Cache<Q, Bytes>;

pub struct AppState {
    config: AppConfig,
    db: Database,
    openings: RwLock<Openings>,
    lichess_cache: ResponseCache<LichessQuery>,
    masters_cache: ResponseCache<MastersQuery>,
    semaphore: Semaphore,
    metrics: Metrics,
    /// For sampling which deep positions fill the block cache.
    sample: AtomicU64,
}

type Shared = Arc<AppState>;

impl AppState {
    /// Whether a lookup of a position at the given ply should fill the
    /// block cache: deep positions are rarely requested again.
    fn fill_cache(&self, ply: u32) -> bool {
        let percent = if ply < 15 {
            return true;
        } else if ply < 20 {
            5
        } else if ply < 25 {
            2
        } else {
            1
        };
        self.sample.fetch_add(1, Ordering::Relaxed) % 100 < percent
    }

    /// The packed entry of a position: from the positions column family,
    /// or synthesized from the single game that reached it.
    fn entry(&self, pos: &VariantPosition, ply: u32) -> Result<Option<Entry<'_>>, rocksdb::Error> {
        let hash = position_hash(pos);
        let fill_cache = self.fill_cache(ply);
        if let Some(entry) = self.db.positions_entry(&hash, fill_cache)? {
            Metrics::inc(&self.metrics.positions_hits);
            return Ok(Some(Entry::Pinned(entry)));
        }
        let candidates = self.db.cold_candidates(&hash, fill_cache)?;
        if candidates.is_empty() {
            Metrics::inc(&self.metrics.misses);
            return Ok(None);
        }
        Metrics::add(&self.metrics.cold_candidates, candidates.len() as u64);
        let mut entry = LichessEntry::new();
        let mut found = false;
        for (seq, (id, game)) in candidates
            .iter()
            .zip(self.load_games(&candidates, fill_cache)?)
            .enumerate()
        {
            let Some(game) = game else {
                log::warn!("cold candidate {id} not found in games");
                continue;
            };
            let game = BorrowedLichessGame::unpack_entire(&game);
            if let Some(visit) = find_visit(&game, &hash, self.config.max_ply) {
                let rating = game.players[visit.turn].rating.unwrap_or_default();
                entry.add_game(
                    CellKey::new(
                        visit.m,
                        GroupHeader {
                            speed: game.speed,
                            rating_group: RatingGroup::select(rating),
                        },
                        game.date.earliest_month(),
                    ),
                    rating,
                    game.outcome,
                    GameSeq(seq as u64),
                    *id,
                );
                found = true;
            }
        }
        if found {
            Metrics::inc(&self.metrics.cold_hits);
            let mut buf = Vec::new();
            entry.pack_entire(&mut buf);
            Ok(Some(Entry::Owned(buf)))
        } else {
            Metrics::inc(&self.metrics.misses);
            Ok(None)
        }
    }

    /// Loads games by id, keeping the order.
    fn load_games(
        &self,
        ids: &[GameId],
        fill_cache: bool,
    ) -> Result<Vec<Option<Vec<u8>>>, rocksdb::Error> {
        Metrics::add(&self.metrics.games_loaded, ids.len() as u64);
        Ok(self
            .db
            .games(ids, fill_cache)?
            .into_iter()
            .map(|game| game.map(|game| game.to_vec()))
            .collect())
    }

    fn explorer_moves(
        &self,
        pos: &VariantPosition,
        moves: Vec<PreparedMove>,
        games: &mut Vec<GameId>,
    ) -> Vec<(ExplorerMove, Option<usize>)> {
        let openings = self.openings.read().expect("read openings");
        moves
            .into_iter()
            .map(|pm| {
                let uci = pm.m.unpack();
                let mut pos_after = pos.clone();
                let san = match uci.to_move(pos) {
                    Ok(m) => SanPlus::from_move_and_play_unchecked(&mut pos_after, m),
                    Err(_) => SanPlus {
                        san: San::Null,
                        suffix: None,
                    },
                };
                let game = pm.game.map(|id| {
                    games.push(id);
                    games.len() - 1
                });
                (
                    ExplorerMove {
                        uci,
                        san,
                        average_rating: pm.stats.average_rating(),
                        stats: Stats::from(&pm.stats),
                        game: None,
                        opening: openings.classify_exact(&pos_after).cloned(),
                    },
                    game,
                )
            })
            .collect()
    }

    fn with_uci(
        refs: &[GameRef],
        games: &[Option<Vec<u8>>],
        offset: usize,
        masters: bool,
    ) -> Vec<ExplorerGameWithUci> {
        refs.iter()
            .zip(&games[offset..])
            .filter_map(|(game_ref, game)| {
                game.as_ref().map(|game| ExplorerGameWithUci {
                    uci: game_ref.m.unpack(),
                    game: ExplorerGame::new(
                        game_ref.id,
                        &BorrowedLichessGame::unpack_entire(game),
                        masters,
                    ),
                })
            })
            .collect()
    }

    fn lichess(&self, query: &LichessQuery, play: PlayPosition) -> Result<Bytes, rocksdb::Error> {
        let PlayPosition { pos, opening, ply } = play;
        let entry = self.entry(&pos, ply)?;
        let bytes: &[u8] = entry.as_deref().unwrap_or(&[]);
        let prepared = prepare_lichess(bytes, &query.filter, &query.limits, query.history);

        let mut ids = Vec::new();
        let mut moves = self.explorer_moves(&pos, prepared.moves, &mut ids);
        let recent_offset = ids.len();
        ids.extend(prepared.recent_games.iter().map(|g| g.id));
        let top_offset = ids.len();
        ids.extend(prepared.top_games.iter().map(|g| g.id));
        let games = self.load_games(&ids, self.fill_cache(ply))?;
        attach_single_games(&mut moves, &ids, &games, false);

        let response = ExplorerResponse {
            total: Stats::from(&prepared.total),
            moves: moves.into_iter().map(|(m, _)| m).collect(),
            recent_games: Some(Self::with_uci(
                &prepared.recent_games,
                &games,
                recent_offset,
                false,
            )),
            top_games: Some(Self::with_uci(
                &prepared.top_games,
                &games,
                top_offset,
                false,
            )),
            opening,
            history: prepared.history.map(|history| {
                history
                    .into_iter()
                    .map(|segment| HistoryEntry {
                        month: segment.month,
                        stats: Stats::from(&segment.stats),
                    })
                    .collect()
            }),
        };
        Ok(Bytes::from(
            serde_json::to_vec(&response).expect("serialize"),
        ))
    }

    fn masters(&self, query: &MastersQuery, play: PlayPosition) -> Result<Bytes, rocksdb::Error> {
        let PlayPosition { pos, opening, ply } = play;
        let entry = self.entry(&pos, ply)?;
        let bytes: &[u8] = entry.as_deref().unwrap_or(&[]);
        let prepared = prepare_masters(
            bytes,
            &query.filter(),
            &query.limits,
            self.config.masters_top_candidates,
        );

        let mut ids = Vec::new();
        let mut moves = self.explorer_moves(&pos, prepared.moves, &mut ids);
        let candidates_offset = ids.len();
        ids.extend(prepared.candidates.iter().map(|g| g.id));
        let games = self.load_games(&ids, self.fill_cache(ply))?;
        attach_single_games(&mut moves, &ids, &games, true);

        // Rank the candidates by their actual ratings.
        let mut top_games: Vec<(i32, usize)> = prepared
            .candidates
            .iter()
            .zip(&games[candidates_offset..])
            .enumerate()
            .filter_map(|(i, (_, game))| {
                game.as_ref().map(|game| {
                    let game = BorrowedLichessGame::unpack_entire(game);
                    let rating = |rating: Option<Rating>| rating.map_or(0, i32::from);
                    (
                        rating(game.players.white.rating) + rating(game.players.black.rating),
                        i,
                    )
                })
            })
            .collect();
        top_games.sort_by_key(|&(sum, i)| (std::cmp::Reverse(sum), i));
        top_games.truncate(query.limits.top_games.min(MAX_MASTERS_GAMES));
        let top_games = top_games
            .into_iter()
            .map(|(_, i)| {
                let game_ref = &prepared.candidates[i];
                ExplorerGameWithUci {
                    uci: game_ref.m.unpack(),
                    game: ExplorerGame::new(
                        game_ref.id,
                        &BorrowedLichessGame::unpack_entire(
                            games[candidates_offset + i].as_deref().expect("loaded"),
                        ),
                        true,
                    ),
                }
            })
            .collect();

        let response = ExplorerResponse {
            total: Stats::from(&prepared.total),
            moves: moves.into_iter().map(|(m, _)| m).collect(),
            recent_games: None,
            top_games: Some(top_games),
            opening,
            history: None,
        };
        Ok(Bytes::from(
            serde_json::to_vec(&response).expect("serialize"),
        ))
    }

    fn pgn(&self, id: GameId) -> Result<String, Error> {
        Metrics::inc(&self.metrics.pgn_requests);
        match self.db.game(id, true)? {
            Some(game) => Ok(masters_pgn(id, &BorrowedLichessGame::unpack_entire(&game))),
            None => Err(Error::NotFound),
        }
    }
}

/// A packed entry, borrowed from the block cache or synthesized.
enum Entry<'a> {
    Pinned(DBPinnableSlice<'a>),
    Owned(Vec<u8>),
}

impl Deref for Entry<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Entry::Pinned(slice) => slice,
            Entry::Owned(vec) => vec,
        }
    }
}

/// Attaches the only game of each move, once the games are loaded.
fn attach_single_games(
    moves: &mut [(ExplorerMove, Option<usize>)],
    ids: &[GameId],
    games: &[Option<Vec<u8>>],
    masters: bool,
) {
    for (m, index) in moves {
        if let Some(i) = *index
            && let Some(game) = &games[i]
        {
            m.game = Some(ExplorerGame::new(
                ids[i],
                &BorrowedLichessGame::unpack_entire(game),
                masters,
            ));
        }
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
    task::spawn_blocking(move || {
        let started = Instant::now();
        let result = f(&state);
        state.metrics.compute.record(started.elapsed());
        result
    })
    .await
    .expect("blocking task")
}

fn json(bytes: Bytes) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        bytes,
    )
        .into_response()
}

/// Maps a rejected query string to a bad request.
fn parse_query<T>(state: &AppState, query: Result<Query<T>, QueryRejection>) -> Result<T, Error> {
    query.map(|Query(query)| query).map_err(|err| {
        Metrics::inc(&state.metrics.bad_requests);
        Error::from(err)
    })
}

async fn lichess(
    State(state): State<Shared>,
    query: Result<Query<LichessQuery>, QueryRejection>,
) -> Result<Response, Error> {
    let query = parse_query(&state, query)?;
    lichess_inner(state, query).await
}

async fn lichess_history(
    State(state): State<Shared>,
    query: Result<Query<LichessQuery>, QueryRejection>,
) -> Result<Response, Error> {
    let query = LichessQuery {
        history: true,
        limits: Limits {
            moves: 0,
            top_games: 0,
            recent_games: 0,
        },
        ..parse_query(&state, query)?
    };
    lichess_inner(state, query).await
}

async fn lichess_inner(state: Shared, query: LichessQuery) -> Result<Response, Error> {
    let started = Instant::now();
    let play = query
        .play
        .position(&state.openings.read().expect("read openings"))
        .inspect_err(|_| Metrics::inc(&state.metrics.bad_requests))?;
    let cached = state.lichess_cache.contains_key(&query);
    let state2 = Arc::clone(&state);
    let bytes = state
        .lichess_cache
        .try_get_with(query.clone(), async move {
            Metrics::inc(&state2.metrics.computed);
            blocking(&state2, move |state| state.lichess(&query, play)).await
        })
        .await
        .map_err(|err| Error::Db((*err).clone()))?;
    if cached {
        Metrics::inc(&state.metrics.cache_hits);
    }
    state.metrics.request.record(started.elapsed());
    Ok(json(bytes))
}

async fn masters(
    State(state): State<Shared>,
    query: Result<Query<MastersQuery>, QueryRejection>,
) -> Result<Response, Error> {
    let started = Instant::now();
    let query = parse_query(&state, query)?;
    let play = query
        .play
        .position(&state.openings.read().expect("read openings"))
        .inspect_err(|_| Metrics::inc(&state.metrics.bad_requests))?;
    let cached = state.masters_cache.contains_key(&query);
    let state2 = Arc::clone(&state);
    let bytes = state
        .masters_cache
        .try_get_with(query.clone(), async move {
            Metrics::inc(&state2.metrics.computed);
            blocking(&state2, move |state| state.masters(&query, play)).await
        })
        .await
        .map_err(|err| Error::Db((*err).clone()))?;
    if cached {
        Metrics::inc(&state.metrics.cache_hits);
    }
    state.metrics.request.record(started.elapsed());
    Ok(json(bytes))
}

async fn masters_pgn_handler(
    State(state): State<Shared>,
    Path(id): Path<String>,
) -> Result<Response, Error> {
    let id: GameId = id.parse()?;
    let pgn = blocking(&state, move |state| state.pgn(id)).await?;
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-chess-pgn"),
        )],
        pgn,
    )
        .into_response())
}

async fn monitor(State(state): State<Shared>) -> String {
    let fields = blocking(&state, |state| {
        let mut fields = state.metrics.fields();
        fields.push(format!(
            "lichess_cache={}u",
            state.lichess_cache.entry_count()
        ));
        fields.push(format!(
            "masters_cache={}u",
            state.masters_cache.entry_count()
        ));
        fields.push(format!("block_cache_usage={}u", state.db.cache_usage()));
        fields.push(format!(
            "block_cache_pinned={}u",
            state.db.cache_pinned_usage()
        ));
        fields.push(format!(
            "openings={}u",
            state.openings.read().expect("read openings").len()
        ));
        for (name, ticker) in [
            (
                "block_cache_hit",
                rocksdb::statistics::Ticker::BlockCacheHit,
            ),
            (
                "block_cache_miss",
                rocksdb::statistics::Ticker::BlockCacheMiss,
            ),
            (
                "block_cache_index_miss",
                rocksdb::statistics::Ticker::BlockCacheIndexMiss,
            ),
            (
                "block_cache_filter_miss",
                rocksdb::statistics::Ticker::BlockCacheFilterMiss,
            ),
            (
                "block_cache_data_miss",
                rocksdb::statistics::Ticker::BlockCacheDataMiss,
            ),
            (
                "bloom_filter_useful",
                rocksdb::statistics::Ticker::BloomFilterUseful,
            ),
        ] {
            fields.push(format!("{name}={}u", state.db.ticker(ticker)));
        }
        fields
    })
    .await;
    format!("opening_explorer {}\n", fields.join(","))
}

async fn db_prop(State(state): State<Shared>, Path(prop): Path<String>) -> Result<String, Error> {
    blocking(&state, move |state| {
        state.db.property(&prop)?.ok_or(Error::NotFound)
    })
    .await
}

async fn cf_prop(
    State(state): State<Shared>,
    Path((cf, prop)): Path<(String, String)>,
) -> Result<String, Error> {
    blocking(&state, move |state| {
        state.db.cf_property(&cf, &prop)?.ok_or(Error::NotFound)
    })
    .await
}

async fn periodic_openings_refresh(state: Shared) {
    loop {
        match Openings::download().await {
            Ok(openings) => {
                log::info!("refreshed {} opening names", openings.len());
                *state.openings.write().expect("write openings") = openings;
                state.lichess_cache.invalidate_all();
                state.masters_cache.invalidate_all();
                time::sleep(Duration::from_secs(60 * 167)).await;
            }
            Err(err) => {
                log::error!("failed to refresh opening names: {err}");
                time::sleep(Duration::from_secs(60)).await;
            }
        }
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    log::info!("shutting down");
}

/// Runs the server until it is interrupted.
pub async fn serve(config: AppConfig, db: Database) -> Result<(), Box<dyn std::error::Error>> {
    let openings = match &config.openings_dir {
        Some(dir) => {
            let openings = Openings::load_dir(dir)?;
            log::info!(
                "loaded {} opening names from {}",
                openings.len(),
                dir.display()
            );
            openings
        }
        None => Openings::default(),
    };
    let download_openings = config.openings_dir.is_none() && !config.no_openings;

    let state: Shared = Arc::new(AppState {
        lichess_cache: Cache::builder()
            .max_capacity(config.cache_entries)
            .time_to_live(config.cache_ttl)
            .build(),
        masters_cache: Cache::builder()
            .max_capacity(config.cache_entries)
            .time_to_live(config.cache_ttl)
            .build(),
        semaphore: Semaphore::new(config.blocking_threads),
        openings: RwLock::new(openings),
        metrics: Metrics::default(),
        sample: AtomicU64::new(0),
        config,
        db,
    });
    if download_openings {
        task::spawn(periodic_openings_refresh(Arc::clone(&state)));
    }

    let app = Router::new()
        .route("/monitor", get(monitor))
        .route("/monitor/db/{prop}", get(db_prop))
        .route("/monitor/cf/{cf}/{prop}", get(cf_prop));
    let app = match state.config.kind {
        Kind::Lichess => app
            .route("/lichess", get(lichess))
            .route("/lichess/history", get(lichess_history)),
        Kind::Masters => app
            .route("/masters", get(masters))
            .route("/masters/pgn/{id}", get(masters_pgn_handler))
            .route("/master", get(masters))
            .route("/master/pgn/{id}", get(masters_pgn_handler)),
    };
    let app = app.with_state(Arc::clone(&state));
    let app = if state.config.cors {
        app.layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        ))
    } else {
        app
    };

    let listener = TcpListener::bind(state.config.bind).await?;
    log::info!("listening on {}", state.config.bind);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}
