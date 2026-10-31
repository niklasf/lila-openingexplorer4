use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Instant,
};

use rocksdb::{
    BlockBasedTablePinningTier, Cache, ColumnFamily, ColumnFamilyDescriptor, DB, DBPinnableSlice,
    ErrorKind, IngestExternalFileOptions, Options, ReadOptions, ReadTier,
    statistics::{StatsLevel, Ticker},
};
use thiserror::Error;

use crate::{
    packed::{ColdKey, GameId, Zobrist96},
    sst::SstConfig,
};

pub const GAMES_CF: &str = "games";
pub const POSITIONS_CF: &str = "positions";
pub const COLD_CF: &str = "cold_positions";

/// Subdirectory of the database for hard links of SST files that are about
/// to be ingested.
const STAGING_DIR: &str = "staging";

#[derive(Debug, Clone)]
pub struct DbConfig {
    /// Size of the shared block cache in bytes.
    pub cache_bytes: u64,
    /// Use an LRU cache instead of a hyper clock cache.
    pub lru_cache: bool,
    /// Bypass the page cache.
    pub direct_reads: bool,
    /// Collect RocksDB statistics.
    pub statistics: bool,
    /// Threads for opening table files at startup.
    pub file_opening_threads: usize,
    /// Block size the SST files were written with.
    pub block_size: usize,
    /// Load all index and filter partitions when opening and keep them in
    /// the block cache, so that a lookup reads at most one data block.
    pub pin_metadata: bool,
}

#[derive(Debug, Error)]
pub enum DbError {
    #[error("rocksdb: {0}")]
    RocksDb(#[from] rocksdb::Error),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

fn sst_config(config: &DbConfig) -> SstConfig {
    SstConfig {
        block_size: config.block_size,
        // Only relevant for writing. Readers detect compression per block.
        zstd_level: 3,
        zstd_dict: 0,
        direct_io: false,
    }
}

fn descriptors(config: &DbConfig, cache: &Cache) -> Vec<ColumnFamilyDescriptor> {
    let sst = sst_config(config);
    [(GAMES_CF, false), (POSITIONS_CF, true), (COLD_CF, false)]
        .into_iter()
        .map(|(name, filter)| {
            let mut table_opts = sst.table_options(filter);
            table_opts.set_block_cache(cache);
            table_opts.set_cache_index_and_filter_blocks(true);
            table_opts.set_cache_index_and_filter_blocks_with_high_priority(true);
            table_opts.set_pin_top_level_index_and_filter(true);
            if config.pin_metadata {
                table_opts.set_top_level_index_pinning_tier(BlockBasedTablePinningTier::All);
                table_opts.set_partition_pinning_tier(BlockBasedTablePinningTier::All);
                table_opts.set_unpartitioned_pinning_tier(BlockBasedTablePinningTier::All);
            }
            let mut opts = sst.options_with_table(&table_opts);
            opts.set_disable_auto_compactions(true);
            ColumnFamilyDescriptor::new(name, opts)
        })
        .collect()
}

fn db_options(config: &DbConfig) -> Options {
    let mut opts = Options::default();
    opts.set_max_open_files(-1);
    opts.set_max_file_opening_threads(config.file_opening_threads.clamp(1, 64) as i32);
    opts.set_use_direct_reads(config.direct_reads);
    opts.set_skip_stats_update_on_db_open(true);
    opts.set_table_cache_num_shard_bits(6);
    if config.statistics {
        opts.enable_statistics();
        opts.set_statistics_level(StatsLevel::ExceptDetailedTimers);
    }
    opts
}

fn make_cache(config: &DbConfig) -> Cache {
    if config.lru_cache {
        Cache::new_lru_cache(config.cache_bytes as usize)
    } else {
        // Estimated entry charge 0: automatic (block sizes vary with
        // compression).
        Cache::new_hyper_clock_cache(config.cache_bytes as usize, 0)
    }
}

/// SST files of a directory, in key order (file names sort by key range).
fn sst_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .filter(|path| {
            path.as_ref()
                .is_ok_and(|path| path.extension().is_some_and(|ext| ext == "sst"))
        })
        .collect::<io::Result<_>>()?;
    files.sort();
    Ok(files)
}

#[derive(Debug, Default, Clone)]
pub struct IngestStats {
    pub games_files: usize,
    pub positions_files: usize,
    pub cold_files: usize,
}

/// Creates a database at `path` from the SST files of the three
/// directories, which are hard-linked (the originals are left untouched).
/// The directories must be on the same file system as the database.
pub fn ingest(
    path: &Path,
    games: &Path,
    positions: &Path,
    cold: &Path,
    config: &DbConfig,
) -> Result<IngestStats, DbError> {
    let cache = make_cache(&DbConfig {
        cache_bytes: 64 << 20,
        ..config.clone()
    });
    let mut opts = db_options(config);
    opts.create_if_missing(true);
    opts.create_missing_column_families(true);
    let db = DB::open_cf_descriptors(&opts, path, descriptors(config, &cache))?;
    let staging = path.join(STAGING_DIR);
    fs::create_dir_all(&staging)?;

    let mut stats = IngestStats::default();
    for (cf_name, dir, count) in [
        (GAMES_CF, games, &mut stats.games_files),
        (POSITIONS_CF, positions, &mut stats.positions_files),
        (COLD_CF, cold, &mut stats.cold_files),
    ] {
        let started = Instant::now();
        let files = sst_files(dir)?;
        // RocksDB removes the "moved" files after linking them into the
        // database, so link into a staging directory first.
        let staged: Vec<PathBuf> = files
            .iter()
            .map(|file| {
                let staged = staging.join(format!(
                    "{cf_name}_{}",
                    file.file_name().expect("file name").to_string_lossy()
                ));
                fs::hard_link(file, &staged)?;
                Ok::<_, io::Error>(staged)
            })
            .collect::<Result<_, _>>()?;
        let mut ingest_opts = IngestExternalFileOptions::default();
        ingest_opts.set_move_files(true);
        ingest_opts.set_snapshot_consistency(false);
        ingest_opts.set_allow_blocking_flush(false);
        let cf = db.cf_handle(cf_name).expect("column family");
        db.ingest_external_file_cf_opts(cf, &ingest_opts, staged)?;
        *count = files.len();
        log::info!(
            "ingested {} files into {cf_name} in {:.1?}",
            files.len(),
            started.elapsed()
        );
    }
    fs::remove_dir(&staging)?;
    Ok(stats)
}

/// A read-only view of an ingested database.
pub struct Database {
    db: DB,
    cache: Cache,
    opts: Options,
}

impl Database {
    pub fn open_read_only(path: &Path, config: &DbConfig) -> Result<Database, DbError> {
        let started = Instant::now();
        let cache = make_cache(config);
        let opts = db_options(config);
        let db =
            DB::open_cf_descriptors_read_only(&opts, path, descriptors(config, &cache), false)?;
        log::info!(
            "opened {} read-only in {:.1?}",
            path.display(),
            started.elapsed()
        );
        Ok(Database { db, cache, opts })
    }

    fn cf(&self, name: &str) -> &ColumnFamily {
        self.db.cf_handle(name).expect("column family")
    }

    fn read_options(fill_cache: bool) -> ReadOptions {
        let mut opts = ReadOptions::default();
        opts.fill_cache(fill_cache);
        opts
    }

    /// The packed `LichessEntry` of a position, if it was reached by more
    /// than one game.
    pub fn positions_entry(
        &self,
        hash: &Zobrist96,
        fill_cache: bool,
    ) -> Result<Option<DBPinnableSlice<'_>>, rocksdb::Error> {
        self.db.get_pinned_cf_opt(
            self.cf(POSITIONS_CF),
            hash.as_bytes(),
            &Self::read_options(fill_cache),
        )
    }

    /// Games that may have reached the position exactly once (to be
    /// validated by replaying them).
    pub fn cold_candidates(
        &self,
        hash: &Zobrist96,
        fill_cache: bool,
    ) -> Result<Vec<GameId>, rocksdb::Error> {
        let key = ColdKey::new(hash);
        Ok(self
            .db
            .get_pinned_cf_opt(
                self.cf(COLD_CF),
                key.prefix(),
                &Self::read_options(fill_cache),
            )?
            .map_or_else(Vec::new, |group| key.candidates(&group).collect()))
    }

    pub fn game(
        &self,
        id: GameId,
        fill_cache: bool,
    ) -> Result<Option<DBPinnableSlice<'_>>, rocksdb::Error> {
        self.db.get_pinned_cf_opt(
            self.cf(GAMES_CF),
            id.to_bytes(),
            &Self::read_options(fill_cache),
        )
    }

    /// Packed games, in the order of the ids (`None` for unknown ids).
    pub fn games(
        &self,
        ids: &[GameId],
        fill_cache: bool,
    ) -> Result<Vec<Option<DBPinnableSlice<'_>>>, rocksdb::Error> {
        let keys: Vec<[u8; GameId::SIZE]> = ids.iter().map(|id| id.to_bytes()).collect();
        self.db
            .batched_multi_get_cf_opt(
                self.cf(GAMES_CF),
                &keys,
                false,
                &Self::read_options(fill_cache),
            )
            .into_iter()
            .collect()
    }

    /// Packed games that are in the block cache, in the order of the ids:
    /// `None` for games that would require reading from disk, `Some(None)`
    /// for unknown ids.
    ///
    /// Uses single lookups: RocksDB's MultiGet reads data blocks that are
    /// missing from the cache regardless of the read tier (as of 11.8, once
    /// the index partitions are cached).
    pub fn games_cached(
        &self,
        ids: &[GameId],
        fill_cache: bool,
    ) -> Result<Vec<Option<Option<DBPinnableSlice<'_>>>>, rocksdb::Error> {
        let mut opts = Self::read_options(fill_cache);
        opts.set_read_tier(ReadTier::BlockCache);
        let cf = self.cf(GAMES_CF);
        ids.iter()
            .map(
                |id| match self.db.get_pinned_cf_opt(cf, id.to_bytes(), &opts) {
                    Ok(game) => Ok(Some(game)),
                    Err(err) if err.kind() == ErrorKind::Incomplete => Ok(None),
                    Err(err) => Err(err),
                },
            )
            .collect()
    }

    /// The first entries of the positions column family (a random sample
    /// of positions, as keys are hashes).
    pub fn scan_positions(&self, limit: usize) -> Result<Vec<Vec<u8>>, rocksdb::Error> {
        let mut opts = ReadOptions::default();
        opts.fill_cache(false);
        self.db
            .iterator_cf_opt(self.cf(POSITIONS_CF), opts, rocksdb::IteratorMode::Start)
            .take(limit)
            .map(|item| item.map(|(_, value)| value.into_vec()))
            .collect()
    }

    pub fn cache_usage(&self) -> usize {
        self.cache.get_usage()
    }

    pub fn cache_pinned_usage(&self) -> usize {
        self.cache.get_pinned_usage()
    }

    pub fn ticker(&self, ticker: Ticker) -> u64 {
        self.opts.get_ticker_count(ticker)
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

    pub fn estimated_keys(&self, cf: &str) -> u64 {
        self.db
            .property_int_value_cf(self.cf(cf), rocksdb::properties::ESTIMATE_NUM_KEYS)
            .ok()
            .flatten()
            .unwrap_or(0)
    }
}
