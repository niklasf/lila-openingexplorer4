use std::path::PathBuf;

use rocksdb::{
    BlockBasedIndexType, BlockBasedOptions, DBCompressionType, DataBlockIndexType, EnvOptions,
    Options, SstFileWriter,
};

/// Bloom-equivalent bits per key of filters, for about 1% false positives.
const FILTER_BITS_PER_KEY: f64 = 10.0;

#[derive(Debug, Copy, Clone)]
pub struct SstConfig {
    pub block_size: usize,
    /// Zstd level of the SST files, or 0 for no compression.
    pub zstd_level: i32,
    /// Maximum size of the zstd dictionary trained for each SST file (0 to
    /// disable dictionary compression).
    pub zstd_dict: i32,
    pub direct_io: bool,
}

impl SstConfig {
    /// Options for SST files that serve point lookups. Use a `filter` if
    /// lookups often miss and the keys are sparse.
    pub fn options(&self, filter: bool) -> Options {
        let mut opts = Options::default();
        if self.zstd_level == 0 {
            opts.set_compression_type(DBCompressionType::None);
        } else {
            opts.set_compression_type(DBCompressionType::Zstd);
            opts.set_compression_options(-14, self.zstd_level, 0, self.zstd_dict);
            if self.zstd_dict != 0 {
                opts.set_zstd_max_train_bytes(self.zstd_dict * 100);
            }
        }
        let mut table_opts = BlockBasedOptions::default();
        table_opts.set_block_size(self.block_size);
        // Hash lookup within data blocks, instead of binary search.
        table_opts.set_data_block_index_type(DataBlockIndexType::BinaryAndHash);
        table_opts.set_data_block_hash_ratio(0.75);
        // Large files: partition the index (and the filter), so that only
        // the top-level index needs to stay in memory.
        table_opts.set_index_type(BlockBasedIndexType::TwoLevelIndexSearch);
        table_opts.set_metadata_block_size(4096);
        if filter {
            table_opts.set_ribbon_filter(FILTER_BITS_PER_KEY);
            table_opts.set_partition_filters(true);
            table_opts.set_optimize_filters_for_memory(true);
        }
        opts.set_block_based_table_factory(&table_opts);
        opts
    }

    pub fn env_options(&self) -> EnvOptions {
        let mut env_opts = EnvOptions::default();
        env_opts.set_use_direct_writes(self.direct_io);
        env_opts
    }
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
    writer: Option<SstFileWriter<'a>>,
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

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), rocksdb::Error> {
        let w = match &mut self.writer {
            Some(w) => w,
            None => {
                let w = SstFileWriter::create_with_env_options(self.opts, self.env_opts);
                let mut path = self.prefix.clone().into_os_string();
                path.push(format!("_{:04}.sst", self.seq));
                w.open(path)?;
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
    pub fn finish_file(&mut self) -> Result<(), rocksdb::Error> {
        if let Some(mut w) = self.writer.take() {
            w.finish()?;
            self.stats
                .add_file(self.keys_in_file, w.file_size(), self.raw_in_file);
            self.keys_in_file = 0;
            self.raw_in_file = 0;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<SstStats, rocksdb::Error> {
        self.finish_file()?;
        Ok(self.stats)
    }
}
