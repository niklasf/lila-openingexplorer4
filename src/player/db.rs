//! The live database of the personal explorer.

use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamily, ColumnFamilyDescriptor, DB, DBCompressionType,
    Direction, EnvOptions, IngestExternalFileOptions, IteratorMode, MergeOperands, Options,
    ReadOptions, SnapshotWithThreadMode, SstFileWriter, WriteBatch,
    statistics::{StatsLevel, Ticker},
};
use shakmaty::{Color, variant::VariantPosition};
use thiserror::Error;

use crate::{
    packed::{CellKey, GameId, GameIndex, PlayerEntry, PlayerGroupHeader, PositionKey},
    player::{
        UserHash,
        aggregate::{Aggregation, Placed, Placement, cold_bits_for, find_visit},
        cold::PlayerColdGroup,
        keys::{
            ColdBucket, ColdGroupKey, GameIdKey, GameKey, GameSeq, Generation, PlayerRange,
            StatusKey, WarmKey,
        },
        record::{BorrowedPlayerGame, PlayerGame},
        status::{FORMAT_VERSION, PlayerStatus},
    },
    sst::SstConfig,
};

pub const STATUS_CF: &str = "player_status";
pub const GAMES_CF: &str = "player_games";
pub const GAME_IDS_CF: &str = "player_game_ids";
pub const WARM_CF: &str = "player_warm";
pub const COLD_CF: &str = "player_cold";

const ALL_CFS: [&str; 5] = [STATUS_CF, GAMES_CF, GAME_IDS_CF, WARM_CF, COLD_CF];

/// Subdirectory of the database for SST files of rebuilds, before they are
/// ingested.
const REBUILD_DIR: &str = "rebuild";

#[derive(Debug, Error)]
pub enum PlayerDbError {
    #[error("rocksdb: {0}")]
    RocksDb(#[from] rocksdb::Error),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone)]
pub struct PlayerDbConfig {
    /// Size of the block cache.
    pub cache_bytes: u64,
    /// Plies of each game that are indexed.
    pub max_ply: usize,
    pub placement: Placement,
    /// Bytes per second of flushes and compactions (`None` for unlimited).
    pub rate_limit: Option<u64>,
    /// Directory of the write ahead log, if not the database directory.
    pub wal_dir: Option<PathBuf>,
    /// Collect RocksDB statistics.
    pub statistics: bool,
    /// Rebuilds with more output are written as SST files and ingested.
    pub ingest_bytes: u64,
    /// Bytes per write batch of a rebuild.
    pub batch_bytes: usize,
}

impl Default for PlayerDbConfig {
    fn default() -> PlayerDbConfig {
        PlayerDbConfig {
            cache_bytes: 1 << 30,
            max_ply: 50,
            placement: Placement::default(),
            rate_limit: None,
            wal_dir: None,
            statistics: false,
            ingest_bytes: 64 << 20,
            batch_bytes: 4 << 20,
        }
    }
}

fn warm_merge(_key: &[u8], existing: Option<&[u8]>, operands: &MergeOperands) -> Option<Vec<u8>> {
    let mut entry = PlayerEntry::new();
    if let Some(existing) = existing {
        entry.merge_unpack_entire(existing);
    }
    for operand in operands {
        entry.merge_unpack_entire(operand);
    }
    let mut buf = Vec::new();
    entry.pack_entire(&mut buf);
    Some(buf)
}

fn cold_merge(_key: &[u8], existing: Option<&[u8]>, operands: &MergeOperands) -> Option<Vec<u8>> {
    Some(PlayerColdGroup::merge(existing.into_iter().chain(operands)))
}

fn table_options(cache: &Cache, filter: bool) -> BlockBasedOptions {
    let mut table_opts = SstConfig {
        block_size: 16 * 1024,
        zstd_level: 3,
        zstd_dict: 0,
        direct_io: false,
    }
    .table_options(filter);
    table_opts.set_block_cache(cache);
    table_opts.set_cache_index_and_filter_blocks(true);
    table_opts.set_cache_index_and_filter_blocks_with_high_priority(true);
    table_opts.set_pin_top_level_index_and_filter(true);
    table_opts
}

fn cf_options(name: &str, cache: &Cache) -> Options {
    let mut opts = Options::default();
    opts.set_block_based_table_factory(&table_options(cache, name != GAMES_CF));
    opts.set_level_compaction_dynamic_level_bytes(true);
    opts.set_compression_type(DBCompressionType::Lz4);
    opts.set_bottommost_compression_type(DBCompressionType::Zstd);
    opts.set_bottommost_compression_options(-14, 3, 0, 0, true);
    opts.set_write_buffer_size(64 << 20);
    opts.set_target_file_size_base(64 << 20);
    match name {
        WARM_CF => opts.set_merge_operator_associative("player_warm", warm_merge),
        COLD_CF => opts.set_merge_operator_associative("player_cold", cold_merge),
        _ => (),
    }
    opts
}

/// Counters of the database, for `/monitor`.
#[derive(Default)]
pub struct DbCounters {
    pub chunks: AtomicU64,
    pub games_written: AtomicU64,
    pub games_known: AtomicU64,
    pub warm_operands: AtomicU64,
    pub cold_operands: AtomicU64,
    pub cold_candidates: AtomicU64,
    pub cold_false: AtomicU64,
    pub rebuilds: AtomicU64,
    pub rebuilds_ingested: AtomicU64,
    pub rebuild_micros: AtomicU64,
    pub rebuild_bytes: AtomicU64,
}

/// What a chunk write did.
#[derive(Debug, Default, Clone, Copy)]
pub struct ChunkStats {
    pub games: u64,
    /// Games skipped because they were already indexed.
    pub known: u64,
    pub warm_operands: u64,
    pub cold_operands: u64,
    pub batch_bytes: u64,
}

/// What a rebuild did.
#[derive(Debug, Default, Clone, Copy)]
pub struct RebuildStats {
    pub games: u64,
    pub warm_entries: u64,
    pub cold_groups: u64,
    pub bytes: u64,
    pub ingested: bool,
}

pub struct PlayerDb {
    db: DB,
    path: PathBuf,
    cache: Cache,
    opts: Options,
    warm_opts: Options,
    cold_opts: Options,
    config: PlayerDbConfig,
    pub counters: DbCounters,
}

impl PlayerDb {
    pub fn open(path: &Path, config: PlayerDbConfig) -> Result<PlayerDb, PlayerDbError> {
        let started = Instant::now();
        let mut lru = rocksdb::LruCacheOptions::default();
        lru.set_capacity(config.cache_bytes as usize);
        lru.set_high_pri_pool_ratio(0.5);
        let cache = Cache::new_lru_cache_opts(&lru);

        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_max_background_jobs(4);
        opts.set_bytes_per_sync(1 << 20);
        opts.set_db_write_buffer_size(512 << 20);
        opts.set_max_total_wal_size(1 << 30);
        if let Some(rate) = config.rate_limit {
            opts.set_ratelimiter(rate as i64, 100_000, 10);
        }
        if let Some(wal_dir) = &config.wal_dir {
            opts.set_wal_dir(wal_dir);
        }
        if config.statistics {
            opts.enable_statistics();
            opts.set_statistics_level(StatsLevel::ExceptDetailedTimers);
        }
        let db = DB::open_cf_descriptors(
            &opts,
            path,
            ALL_CFS.map(|name| ColumnFamilyDescriptor::new(name, cf_options(name, &cache))),
        )?;
        let rebuild_dir = path.join(REBUILD_DIR);
        if rebuild_dir.exists() {
            fs::remove_dir_all(&rebuild_dir)?;
        }
        log::info!("opened {} in {:.1?}", path.display(), started.elapsed());
        Ok(PlayerDb {
            db,
            path: path.to_owned(),
            warm_opts: cf_options(WARM_CF, &cache),
            cold_opts: cf_options(COLD_CF, &cache),
            cache,
            opts,
            config,
            counters: DbCounters::default(),
        })
    }

    pub fn config(&self) -> &PlayerDbConfig {
        &self.config
    }

    fn cf(&self, name: &str) -> &ColumnFamily {
        self.db.cf_handle(name).expect("column family")
    }

    pub fn status(&self, uh: UserHash) -> Result<Option<PlayerStatus>, PlayerDbError> {
        Ok(self
            .db
            .get_pinned_cf(self.cf(STATUS_CF), StatusKey::new(uh))?
            .map(|value| PlayerStatus::unpack(&value)))
    }

    pub fn put_status(&self, status: &PlayerStatus) -> Result<(), PlayerDbError> {
        let mut buf = Vec::new();
        status.pack(&mut buf);
        self.db
            .put_cf(self.cf(STATUS_CF), StatusKey::new(status.user.hash()), buf)?;
        Ok(())
    }

    /// Writes a chunk of new games of a player and the status checkpoint
    /// in one atomic batch. Assigns the games their numbers, skipping games
    /// that are already indexed, and updates the status. Reads only the
    /// game ids, never the warm or cold index.
    pub fn write_chunk(
        &self,
        status: &mut PlayerStatus,
        games: &[PlayerGame],
    ) -> Result<ChunkStats, PlayerDbError> {
        let uh = status.user.hash();
        let id_keys: Vec<GameIdKey> = games
            .iter()
            .map(|game| GameIdKey::new(uh, game.id))
            .collect();
        let known: Vec<bool> = self
            .db
            .batched_multi_get_cf(self.cf(GAME_IDS_CF), &id_keys, false)
            .into_iter()
            .map(|result| result.map(|value| value.is_some()))
            .collect::<Result<_, _>>()?;
        let mut seen: HashSet<GameId> = HashSet::new();
        let fresh = status.games == 0;

        let mut batch = WriteBatch::default();
        let mut stats = ChunkStats::default();
        let mut aggregation = Aggregation::new();
        let mut buf = Vec::new();
        for (game, known) in games.iter().zip(known) {
            if known || !seen.insert(game.id) {
                stats.known += 1;
                continue;
            }
            let seq = status.next_seq();
            status.games += 1;
            buf.clear();
            game.pack(&mut buf);
            batch.put_cf(self.cf(GAMES_CF), GameKey::new(uh, seq), &buf);
            batch.put_cf(
                self.cf(GAME_IDS_CF),
                GameIdKey::new(uh, game.id),
                seq.to_bytes(),
            );
            aggregation.add_game(seq, &game.as_borrowed(), self.config.max_ply);
            stats.games += 1;
        }

        let current_bits = status.cold_bits;
        let placed = aggregation.place(self.config.placement, |color, items| {
            if fresh {
                cold_bits_for(items)
            } else {
                *current_bits.get(color)
            }
        });
        if fresh {
            status.cold_bits = placed.cold_bits;
            status.max_ply = self.config.max_ply as u32;
        }
        let generation = status.generation;
        let Placed { warm, cold, .. } = placed;
        for (color, key, mut entry) in warm {
            buf.clear();
            entry.pack_entire(&mut buf);
            batch.merge_cf(
                self.cf(WARM_CF),
                WarmKey::new(uh, generation, color, &key),
                &buf,
            );
            stats.warm_operands += 1;
        }
        for (color, bucket, mut items) in cold {
            batch.merge_cf(
                self.cf(COLD_CF),
                ColdGroupKey::new(uh, generation, color, bucket),
                Placed::pack_cold(&mut items),
            );
            stats.cold_operands += 1;
        }
        buf.clear();
        status.pack(&mut buf);
        batch.put_cf(self.cf(STATUS_CF), StatusKey::new(uh), &buf);
        stats.batch_bytes = batch.size_in_bytes() as u64;
        self.db.write(batch)?;

        let c = &self.counters;
        c.chunks.fetch_add(1, Ordering::Relaxed);
        c.games_written.fetch_add(stats.games, Ordering::Relaxed);
        c.games_known.fetch_add(stats.known, Ordering::Relaxed);
        c.warm_operands
            .fetch_add(stats.warm_operands, Ordering::Relaxed);
        c.cold_operands
            .fetch_add(stats.cold_operands, Ordering::Relaxed);
        Ok(stats)
    }

    /// Whether a rebuild of the player is due: the games since the last
    /// rebuild reached `max(min_games, factor * games at the last rebuild)`,
    /// or the format or the indexed plies changed.
    pub fn rebuild_due(&self, status: &PlayerStatus, min_games: u32, factor: f64) -> bool {
        let since = status.games - status.games_at_rebuild.min(status.games);
        status.games > 0
            && (status.version != FORMAT_VERSION
                || status.max_ply != self.config.max_ply as u32
                || since >= min_games.max((factor * f64::from(status.games_at_rebuild)) as u32))
    }

    /// Rebuilds the warm and cold index of a player from its games, into
    /// the next generation, with the optimal placement for its current
    /// size. Must not run concurrently with writes of the same player.
    pub fn rebuild(&self, uh: UserHash) -> Result<Option<RebuildStats>, PlayerDbError> {
        let started = Instant::now();
        let Some(mut status) = self.status(uh)? else {
            return Ok(None);
        };
        let new = status.generation.next();

        // Leftovers of an interrupted rebuild.
        let (start, end) = PlayerRange::generations(uh, new, None);
        for cf in [WARM_CF, COLD_CF] {
            self.db.delete_range_cf(self.cf(cf), &start, &end)?;
        }

        let mut aggregation = Aggregation::new();
        let mut stats = RebuildStats::default();
        let (start, end) = PlayerRange::all(uh);
        let mut read_opts = ReadOptions::default();
        read_opts.set_iterate_lower_bound(start.clone());
        read_opts.set_iterate_upper_bound(end);
        read_opts.fill_cache(false);
        for item in self.db.iterator_cf_opt(
            self.cf(GAMES_CF),
            read_opts,
            IteratorMode::From(&start, Direction::Forward),
        ) {
            let (key, value) = item?;
            let seq = GameKey::seq(&key);
            aggregation.add_game(
                seq,
                &BorrowedPlayerGame::unpack(&value),
                self.config.max_ply,
            );
            stats.games += 1;
        }
        let placed = aggregation.place(self.config.placement, |_, items| cold_bits_for(items));
        let cold_bits = placed.cold_bits;

        // Pack everything, in key order.
        let mut warm: Vec<(WarmKey, Vec<u8>)> = Vec::with_capacity(placed.warm.len());
        for (color, key, mut entry) in placed.warm {
            let mut buf = Vec::new();
            entry.pack_entire(&mut buf);
            stats.bytes += (WarmKey::SIZE + buf.len()) as u64;
            warm.push((WarmKey::new(uh, new, color, &key), buf));
        }
        let mut cold: Vec<(ColdGroupKey, Vec<u8>)> = Vec::with_capacity(placed.cold.len());
        for (color, bucket, mut items) in placed.cold {
            let buf = Placed::pack_cold(&mut items);
            stats.bytes += (ColdGroupKey::SIZE + buf.len()) as u64;
            cold.push((ColdGroupKey::new(uh, new, color, bucket), buf));
        }
        debug_assert!(warm.is_sorted_by(|a, b| a.0 < b.0));
        debug_assert!(cold.is_sorted_by(|a, b| a.0 < b.0));
        stats.warm_entries = warm.len() as u64;
        stats.cold_groups = cold.len() as u64;

        if stats.bytes > self.config.ingest_bytes {
            stats.ingested = true;
            let dir = self.path.join(REBUILD_DIR);
            fs::create_dir_all(&dir)?;
            for (cf, opts, entries) in [
                (
                    WARM_CF,
                    &self.warm_opts,
                    warm.iter()
                        .map(|(k, v)| (k.as_ref(), &v[..]))
                        .collect::<Vec<_>>(),
                ),
                (
                    COLD_CF,
                    &self.cold_opts,
                    cold.iter().map(|(k, v)| (k.as_ref(), &v[..])).collect(),
                ),
            ] {
                if entries.is_empty() {
                    continue;
                }
                let path = dir.join(format!("{cf}_{}_{}.sst", hex(uh), new.0));
                let env_opts = EnvOptions::default();
                let mut writer = SstFileWriter::create_with_env_options(opts, &env_opts);
                writer.open(&path)?;
                for (key, value) in entries {
                    writer.put(key, value)?;
                }
                writer.finish()?;
                let mut ingest_opts = IngestExternalFileOptions::default();
                ingest_opts.set_move_files(true);
                self.db
                    .ingest_external_file_cf_opts(self.cf(cf), &ingest_opts, vec![path])?;
            }
        } else {
            let mut batch = WriteBatch::default();
            for (cf, entries) in [
                (
                    WARM_CF,
                    warm.iter()
                        .map(|(k, v)| (k.as_ref(), &v[..]))
                        .collect::<Vec<_>>(),
                ),
                (
                    COLD_CF,
                    cold.iter().map(|(k, v)| (k.as_ref(), &v[..])).collect(),
                ),
            ] {
                for (key, value) in entries {
                    batch.put_cf(self.cf(cf), key, value);
                    if batch.size_in_bytes() >= self.config.batch_bytes {
                        self.db.write(std::mem::take(&mut batch))?;
                    }
                }
            }
            if !batch.is_empty() {
                self.db.write(batch)?;
            }
        }

        // Switch to the new generation and drop the old ones.
        status.generation = new;
        status.cold_bits = cold_bits;
        status.games_at_rebuild = status.games;
        status.max_ply = self.config.max_ply as u32;
        status.version = FORMAT_VERSION;
        let mut batch = WriteBatch::default();
        let mut buf = Vec::new();
        status.pack(&mut buf);
        batch.put_cf(self.cf(STATUS_CF), StatusKey::new(uh), &buf);
        let (start, end) = PlayerRange::generations(uh, Generation(0), Some(new));
        batch.delete_range_cf(self.cf(WARM_CF), &start, &end);
        batch.delete_range_cf(self.cf(COLD_CF), &start, &end);
        self.db.write(batch)?;

        let c = &self.counters;
        c.rebuilds.fetch_add(1, Ordering::Relaxed);
        c.rebuilds_ingested
            .fetch_add(u64::from(stats.ingested), Ordering::Relaxed);
        c.rebuild_bytes.fetch_add(stats.bytes, Ordering::Relaxed);
        c.rebuild_micros
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        Ok(Some(stats))
    }

    /// Deletes all data of a player.
    pub fn delete_player(&self, uh: UserHash) -> Result<(), PlayerDbError> {
        let (start, end) = PlayerRange::all(uh);
        let mut batch = WriteBatch::default();
        for cf in ALL_CFS {
            batch.delete_range_cf(self.cf(cf), &start, &end);
        }
        self.db.write(batch)?;
        Ok(())
    }

    /// A consistent view for reading.
    pub fn reader(&self) -> PlayerReader<'_> {
        PlayerReader {
            db: self,
            snapshot: self.db.snapshot(),
        }
    }

    pub fn cache_usage(&self) -> usize {
        self.cache.get_usage()
    }

    pub fn ticker(&self, ticker: Ticker) -> Option<u64> {
        self.config
            .statistics
            .then(|| self.opts.get_ticker_count(ticker))
    }

    pub fn property(&self, name: &str) -> Result<Option<String>, rocksdb::Error> {
        self.db.property_value(name)
    }

    pub fn cf_property(&self, cf: &str, name: &str) -> Result<Option<String>, rocksdb::Error> {
        match self.db.cf_handle(cf) {
            Some(cf) => self.db.property_value_cf(cf, name),
            None => Ok(None),
        }
    }

    pub fn cf_int_property(&self, cf: &str, name: &str) -> Result<Option<u64>, rocksdb::Error> {
        match self.db.cf_handle(cf) {
            Some(cf) => self.db.property_int_value_cf(cf, name),
            None => Ok(None),
        }
    }

    pub fn column_families() -> [&'static str; 5] {
        ALL_CFS
    }

    /// Flushes the memtables (for tests and measurements).
    pub fn flush(&self) -> Result<(), PlayerDbError> {
        for cf in ALL_CFS {
            self.db.flush_cf(self.cf(cf))?;
        }
        Ok(())
    }

    /// Compacts everything (for tests and measurements).
    pub fn compact(&self) {
        for cf in ALL_CFS {
            self.db
                .compact_range_cf(self.cf(cf), None::<&[u8]>, None::<&[u8]>);
        }
    }
}

fn hex(uh: UserHash) -> String {
    uh.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// Statistics of a position read.
#[derive(Debug, Default, Clone, Copy)]
pub struct PositionStats {
    pub warm_hit: bool,
    pub cold_candidates: u64,
    pub cold_validated: u64,
}

/// Reads of a consistent snapshot of the database.
pub struct PlayerReader<'a> {
    db: &'a PlayerDb,
    snapshot: SnapshotWithThreadMode<'a, DB>,
}

impl PlayerReader<'_> {
    pub fn status(&self, uh: UserHash) -> Result<Option<PlayerStatus>, PlayerDbError> {
        Ok(self
            .snapshot
            .get_pinned_cf(self.db.cf(STATUS_CF), StatusKey::new(uh))?
            .map(|value| PlayerStatus::unpack(&value)))
    }

    /// The games of the player that reached a position, from the warm entry
    /// and the validated candidates of the cold group, as one entry.
    pub fn position(
        &self,
        status: &PlayerStatus,
        color: Color,
        pos: &VariantPosition,
    ) -> Result<(PlayerEntry, PositionStats), PlayerDbError> {
        let uh = status.user.hash();
        let key = PositionKey::new(pos);
        let mut stats = PositionStats::default();
        let mut entry = PlayerEntry::new();
        if let Some(warm) = self.snapshot.get_pinned_cf(
            self.db.cf(WARM_CF),
            WarmKey::new(uh, status.generation, color, &key),
        )? {
            entry.merge_unpack_entire(&warm);
            stats.warm_hit = true;
        }

        let (bucket, fingerprint) = ColdBucket::of(&key, *status.cold_bits.get(color));
        let candidates: Vec<GameSeq> = match self.snapshot.get_pinned_cf(
            self.db.cf(COLD_CF),
            ColdGroupKey::new(uh, status.generation, color, bucket),
        )? {
            Some(group) => PlayerColdGroup::candidates(&group, fingerprint).collect(),
            None => Vec::new(),
        };
        stats.cold_candidates = candidates.len() as u64;
        if !candidates.is_empty() {
            for (seq, game) in candidates.iter().zip(self.game_values(uh, &candidates)?) {
                let Some(game) = game else {
                    log::warn!("cold candidate {seq} of {} not found", status.user);
                    continue;
                };
                let game = BorrowedPlayerGame::unpack(&game);
                if game.color != color {
                    continue;
                }
                if let Some(m) = find_visit(&game, &key, self.db.config.max_ply) {
                    entry.add_game(
                        CellKey::new(
                            m,
                            PlayerGroupHeader {
                                speed: game.speed,
                                mode: game.mode,
                            },
                            game.month,
                        ),
                        game.opponent_rating,
                        game.outcome,
                        GameIndex::new(u64::from(seq.0)).expect("game index"),
                    );
                    stats.cold_validated += 1;
                }
            }
            let c = &self.db.counters;
            c.cold_candidates
                .fetch_add(stats.cold_candidates, Ordering::Relaxed);
            c.cold_false.fetch_add(
                stats.cold_candidates - stats.cold_validated,
                Ordering::Relaxed,
            );
        }
        Ok((entry, stats))
    }

    fn game_values(
        &self,
        uh: UserHash,
        seqs: &[GameSeq],
    ) -> Result<Vec<Option<Vec<u8>>>, PlayerDbError> {
        let cf = self.db.cf(GAMES_CF);
        self.snapshot
            .multi_get_cf(seqs.iter().map(|&seq| (cf, GameKey::new(uh, seq))))
            .into_iter()
            .map(|result| result.map_err(PlayerDbError::from))
            .collect()
    }

    /// Records of games of a player, in the order of the numbers.
    pub fn games(
        &self,
        uh: UserHash,
        seqs: &[GameSeq],
    ) -> Result<Vec<Option<PlayerGame>>, PlayerDbError> {
        Ok(self
            .game_values(uh, seqs)?
            .into_iter()
            .map(|value| value.map(|value| BorrowedPlayerGame::unpack(&value).into_owned()))
            .collect())
    }
}
