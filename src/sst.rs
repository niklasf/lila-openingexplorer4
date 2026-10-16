//! Writing SST files of bounded size.

use std::sync::Mutex;

use rocksdb::{BlockBasedOptions, DBCompressionType, EnvOptions, Options};

/// Hard limit for the size of each SST file.
pub const SST_MAX_SIZE: u64 = 512 * 1024 * 1024;

/// Start a new SST file once the current one reaches this size. The margin
/// leaves room for the index and footer written when finishing the file.
pub const SST_ROLL_SIZE: u64 = SST_MAX_SIZE - 16 * 1024 * 1024;

/// Options of a set of SST files.
pub struct SstConfig {
    /// Block size of the SST files.
    pub block_size: usize,
    /// Zstd level of the SST files, or 0 for no compression.
    pub zstd_level: i32,
    /// Maximum size of the zstd dictionary trained for each SST file (0 to
    /// disable dictionary compression).
    pub zstd_dict: i32,
    /// Write with `O_DIRECT`.
    pub direct_io: bool,
}

impl SstConfig {
    pub fn options(&self) -> Options {
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
        opts
    }

    pub fn env_options(&self) -> EnvOptions {
        let mut env_opts = EnvOptions::default();
        env_opts.set_use_direct_writes(self.direct_io);
        env_opts
    }
}

/// Totals of a set of SST files.
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

    /// Ratio of SST bytes to uncompressed bytes, or `initial` before any
    /// file was written.
    pub fn ratio(&self, initial: f64) -> f64 {
        if self.raw_bytes == 0 {
            initial
        } else {
            self.bytes as f64 / self.raw_bytes as f64
        }
    }
}

/// Stats shared by the SST writer threads of a run.
#[derive(Default)]
pub struct SharedSstStats(Mutex<SstStats>);

impl SharedSstStats {
    pub fn add_file(&self, keys: u64, bytes: u64, raw_bytes: u64) {
        self.0
            .lock()
            .expect("lock stats")
            .add_file(keys, bytes, raw_bytes);
    }

    pub fn get(&self) -> SstStats {
        self.0.lock().expect("lock stats").clone()
    }
}

/// Where and how a set of SST files is written.
pub struct SstContext {
    pub opts: Options,
    pub env_opts: EnvOptions,
    pub out: std::path::PathBuf,
    pub stats: SharedSstStats,
}

impl SstContext {
    pub fn new(config: &SstConfig, out: &std::path::Path) -> SstContext {
        SstContext {
            opts: config.options(),
            env_opts: config.env_options(),
            out: out.to_path_buf(),
            stats: SharedSstStats::default(),
        }
    }

    /// Creates a writer for the given file name in the output directory.
    pub fn create_writer(&self, name: &str) -> Result<rocksdb::SstFileWriter<'_>, rocksdb::Error> {
        let w = rocksdb::SstFileWriter::create_with_env_options(&self.opts, &self.env_opts);
        w.open(self.out.join(name))?;
        Ok(w)
    }
}
