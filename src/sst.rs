//! Writing SST files for later ingestion.

use std::path::PathBuf;

use rocksdb::{BlockBasedOptions, DBCompressionType, EnvOptions, Options, SstFileWriter};

#[derive(Debug, Copy, Clone)]
pub struct SstConfig {
    /// Block size of the SST files.
    pub block_size: usize,
    /// Zstd level of the SST files, or 0 for no compression.
    pub zstd_level: i32,
    /// Maximum size of the zstd dictionary trained for each SST file (0 to
    /// disable dictionary compression).
    pub zstd_dict: i32,
    /// Use O_DIRECT for writing.
    pub direct_io: bool,
}

impl SstConfig {
    pub fn options(&self) -> (Options, EnvOptions) {
        let mut opts = Options::default();
        if self.zstd_level == 0 {
            opts.set_compression_type(DBCompressionType::None);
        } else {
            opts.set_compression_type(DBCompressionType::Zstd);
            // Window bits and strategy as rocksdb defaults.
            opts.set_compression_options(-14, self.zstd_level, 0, self.zstd_dict);
            if self.zstd_dict != 0 {
                opts.set_zstd_max_train_bytes(self.zstd_dict * 100);
            }
        }
        let mut table_opts = BlockBasedOptions::default();
        table_opts.set_block_size(self.block_size);
        opts.set_block_based_table_factory(&table_opts);
        let mut env_opts = EnvOptions::default();
        env_opts.set_use_direct_writes(self.direct_io);
        (opts, env_opts)
    }
}

/// Size statistics of written SST files.
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

    /// Ratio of SST bytes to uncompressed key and value bytes.
    pub fn ratio(&self) -> f64 {
        if self.raw_bytes == 0 {
            0.0
        } else {
            self.bytes as f64 / self.raw_bytes as f64
        }
    }

    /// SST bytes per key.
    pub fn bytes_per_key(&self) -> f64 {
        if self.keys == 0 {
            0.0
        } else {
            self.bytes as f64 / self.keys as f64
        }
    }
}

/// Writes keys in ascending order to a sequence of SST files, starting a new
/// file once the current one reaches the target size, like compactions do
/// with `target_file_size_base`. Files are named `{prefix}_{seq:04}.sst`, so
/// that they sort in key order.
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

    fn finish_file(&mut self) -> Result<(), rocksdb::Error> {
        if let Some(mut w) = self.writer.take() {
            w.finish()?;
            self.stats
                .add_file(self.keys_in_file, w.file_size(), self.raw_in_file);
            self.keys_in_file = 0;
            self.raw_in_file = 0;
        }
        Ok(())
    }

    /// Finishes the last file and returns the statistics of all files.
    pub fn finish(mut self) -> Result<SstStats, rocksdb::Error> {
        self.finish_file()?;
        Ok(self.stats)
    }
}
