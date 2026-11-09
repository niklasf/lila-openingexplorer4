use std::{fs, io, path::PathBuf};

use rocksdb::{
    BlockBasedIndexType, BlockBasedOptions, DBCompressionType, DataBlockIndexType, EnvOptions,
    Options, SstFileWriter,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SstError {
    #[error("rocksdb: {0}")]
    RocksDb(#[from] rocksdb::Error),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// An SST file being written under a temporary name.
pub struct SstFile<'a> {
    writer: SstFileWriter<'a>,
    path: PathBuf,
}

impl<'a> SstFile<'a> {
    pub fn create(
        opts: &'a Options,
        env_opts: &'a EnvOptions,
        path: PathBuf,
    ) -> Result<SstFile<'a>, SstError> {
        let writer = SstFileWriter::create_with_env_options(opts, env_opts);
        writer.open(path.with_added_extension("tmp"))?;
        Ok(SstFile { writer, path })
    }

    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<(), SstError> {
        Ok(self.writer.put(key, value)?)
    }

    pub fn file_size(&self) -> u64 {
        self.writer.file_size()
    }

    /// Finishes the file and renames it into place. Returns its size.
    pub fn finish(mut self) -> Result<u64, SstError> {
        self.writer.finish()?;
        fs::rename(self.path.with_added_extension("tmp"), &self.path)?;
        Ok(self.writer.file_size())
    }
}

/// Keep a compressed block if it saves at least 1/16. Blocks of games and
/// positions typically save about 10%, just below the default minimum of 1/8.
const MAX_COMPRESSED_BYTES_PER_KB: i32 = 960;

/// Bloom-equivalent bits per key of filters, for about 1% false positives.
const FILTER_BITS_PER_KEY: f64 = 10.0;

#[derive(Debug, Clone)]
pub struct SstConfig {
    pub block_size: usize,
    /// Zstd level of the SST files, or 0 for no compression.
    pub zstd_level: i32,
    /// Maximum size of the zstd dictionary trained for each SST file (0 to
    /// disable dictionary compression).
    pub zstd_dict: i32,
    pub direct_io: bool,
}

/// Table options that depend on the column family.
#[derive(Debug, Clone)]
pub struct SstTableConfig {
    /// Use a filter if lookups often miss and the keys are sparse.
    pub filter: bool,
    /// Store the keys and the values of each data block in separate
    /// sections. Measured on the full lichess database (2026-11-09):
    /// games -3.1%, positions -0.9%, with no slower point lookups. Not for
    /// cold positions (+0.1%).
    pub separate_key_value_in_data_block: bool,
}

impl SstConfig {
    /// Options for SST files that serve point lookups.
    pub fn options(&self, config: &SstTableConfig) -> Options {
        self.options_with_table(&self.table_options(config))
    }

    /// Column family options with the given table options, which should be
    /// based on `table_options()`.
    pub fn options_with_table(&self, table_opts: &BlockBasedOptions) -> Options {
        let mut opts = Options::default();
        if self.zstd_level == 0 {
            opts.set_compression_type(DBCompressionType::None);
        } else {
            opts.set_compression_type(DBCompressionType::Zstd);
            opts.set_compression_options(-14, self.zstd_level, 0, self.zstd_dict);
            opts.set_zstd_max_train_bytes(self.zstd_dict * 100);
            opts.set_compression_options_max_compressed_bytes_per_kb(MAX_COMPRESSED_BYTES_PER_KB);
        }
        opts.set_block_based_table_factory(table_opts);
        opts
    }

    /// Table options for SST files that serve point lookups. Readers must
    /// use the same filter policy to make use of the filters.
    pub fn table_options(&self, config: &SstTableConfig) -> BlockBasedOptions {
        let mut table_opts = BlockBasedOptions::default();
        table_opts.set_block_size(self.block_size);
        // Hash lookup within data blocks, instead of binary search.
        table_opts.set_data_block_index_type(DataBlockIndexType::BinaryAndHash);
        table_opts.set_data_block_hash_ratio(0.75);
        // Large files: partition the index (and the filter), so that only
        // the top-level index needs to stay in memory.
        table_opts.set_index_type(BlockBasedIndexType::TwoLevelIndexSearch);
        table_opts.set_metadata_block_size(4096);
        // Delta-encode index keys in runs of 16, instead of storing every
        // key in full.
        table_opts.set_index_block_restart_interval(16);
        table_opts.set_separate_key_value_in_data_block(config.separate_key_value_in_data_block);
        if config.filter {
            set_filter(&mut table_opts);
            table_opts.set_partition_filters(true);
            table_opts.set_optimize_filters_for_memory(true);
        }
        table_opts
    }

    pub fn env_options(&self) -> EnvOptions {
        let mut env_opts = EnvOptions::default();
        env_opts.set_use_direct_writes(self.direct_io);
        env_opts
    }
}

fn set_filter(table_opts: &mut BlockBasedOptions) {
    table_opts.set_ribbon_filter(FILTER_BITS_PER_KEY);
}

/// Table options for reading SST files written with
/// `SstConfig::table_options()`. Block size, index type and compression are
/// taken from the files themselves, but the filter policy must match.
pub fn read_table_options(filter: bool) -> BlockBasedOptions {
    let mut table_opts = BlockBasedOptions::default();
    if filter {
        set_filter(&mut table_opts);
    }
    table_opts
}

#[derive(Debug, Default, Clone)]
pub struct SstStats {
    pub keys: u64,
    pub files: u64,
    pub bytes: u64,
    /// Uncompressed key and value bytes.
    pub raw_bytes: u64,
    pub largest_file: u64,
}

impl SstStats {
    pub fn add_file(&mut self, keys: u64, bytes: u64, raw_bytes: u64) {
        self.keys += keys;
        self.files += 1;
        self.bytes += bytes;
        self.raw_bytes += raw_bytes;
        self.largest_file = self.largest_file.max(bytes);
    }

    pub fn merge(&mut self, other: &SstStats) {
        self.keys += other.keys;
        self.files += other.files;
        self.bytes += other.bytes;
        self.raw_bytes += other.raw_bytes;
        self.largest_file = self.largest_file.max(other.largest_file);
    }

    pub fn ratio(&self) -> f64 {
        if self.raw_bytes == 0 {
            0.0
        } else {
            self.bytes as f64 / self.raw_bytes as f64
        }
    }

    pub fn bytes_per_key(&self) -> f64 {
        if self.keys == 0 {
            0.0
        } else {
            self.bytes as f64 / self.keys as f64
        }
    }
}

pub struct RollingSstWriter<'a> {
    opts: &'a Options,
    env_opts: &'a EnvOptions,
    prefix: PathBuf,
    target_size: u64,
    seq: usize,
    writer: Option<SstFile<'a>>,
    keys_in_file: u64,
    raw_in_file: u64,
    stats: SstStats,
}

impl<'a> RollingSstWriter<'a> {
    pub fn new(
        opts: &'a Options,
        env_opts: &'a EnvOptions,
        prefix: PathBuf,
        target_size: u64,
    ) -> RollingSstWriter<'a> {
        RollingSstWriter {
            opts,
            env_opts,
            prefix,
            target_size,
            seq: 0,
            writer: None,
            keys_in_file: 0,
            raw_in_file: 0,
            stats: SstStats::default(),
        }
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), SstError> {
        let w = match &mut self.writer {
            Some(w) => w,
            None => {
                let mut path = self.prefix.clone().into_os_string();
                path.push(format!("_{:04}.sst", self.seq));
                let w = SstFile::create(self.opts, self.env_opts, path.into())?;
                self.seq += 1;
                self.writer.insert(w)
            }
        };
        w.put(key, value)?;
        self.keys_in_file += 1;
        self.raw_in_file += (key.len() + value.len()) as u64;
        if w.file_size() >= self.target_size {
            self.finish_file()?;
        }
        Ok(())
    }

    /// Finishes the current file, if any. The next key starts a new file.
    pub fn finish_file(&mut self) -> Result<(), SstError> {
        if let Some(w) = self.writer.take() {
            let size = w.finish()?;
            self.stats
                .add_file(self.keys_in_file, size, self.raw_in_file);
            self.keys_in_file = 0;
            self.raw_in_file = 0;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<SstStats, SstError> {
        self.finish_file()?;
        Ok(self.stats)
    }
}
