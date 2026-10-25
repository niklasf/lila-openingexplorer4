mod scratch;

use std::{
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use crossbeam::channel;
pub use scratch::{ChunkCompressor, Compressed, ScratchFile};

use crate::util::{Gauge, seconds, timed};

/// Memory partitions per pass that are kept in memory while reading the
/// input, rather than being written to scratch space and read back.
pub const RESIDENT_PARTITIONS_PER_PASS: usize = 1;

/// How the key space is split: `1 << disk_bits` passes over the input, each
/// covering `1 << (memory_bits - disk_bits)` memory partitions.
#[derive(Debug, Copy, Clone)]
pub struct Layout {
    pub memory_bits: u32,
    pub disk_bits: u32,
}

impl Layout {
    pub fn passes(self) -> usize {
        1 << self.disk_bits
    }

    pub fn memory_partitions(self) -> usize {
        1 << self.memory_bits
    }

    pub fn memory_partitions_per_pass(self) -> usize {
        1 << (self.memory_bits - self.disk_bits)
    }

    pub fn scratch_partitions_per_pass(self) -> usize {
        self.memory_partitions_per_pass() - RESIDENT_PARTITIONS_PER_PASS
    }

    /// Index of the given memory partition within the given pass, or `None`
    /// if it belongs to a different pass.
    #[inline]
    pub fn pass_memory_partition(self, memory_partition: usize, pass: usize) -> Option<usize> {
        let disk_partition = memory_partition >> (self.memory_bits - self.disk_bits);
        (disk_partition == pass)
            .then_some(memory_partition & (self.memory_partitions_per_pass() - 1))
    }

    /// Picks the fewest passes that fit the scratch budget, and the fewest
    /// memory partitions that fit the memory budget. `fits` returns whether
    /// a layout fits the memory and the scratch budget, respectively.
    pub fn choose(fits: impl Fn(Layout) -> (bool, bool)) -> Layout {
        let mut layout = Layout {
            memory_bits: 0,
            disk_bits: 0,
        };
        loop {
            let (fits_memory, fits_scratch) = fits(layout);
            if fits_memory && fits_scratch {
                return layout;
            }
            if !fits_scratch && layout.disk_bits < layout.memory_bits {
                layout.disk_bits += 1;
            } else {
                layout.memory_bits += 1;
            }
            assert!(layout.memory_bits <= 16, "no layout fits the budgets");
        }
    }
}

/// Accumulated time of the chunk writer threads.
#[derive(Default)]
pub struct ChunkWriterTimes {
    pub recv: AtomicU64,
    pub compress: AtomicU64,
    pub write: AtomicU64,
    pub throttled: AtomicU64,
}

impl ChunkWriterTimes {
    pub fn summary(&self) -> String {
        format!(
            "recv {:.0}s, compress {:.0}s, write {:.0}s, throttled {:.0}s",
            seconds(&self.recv),
            seconds(&self.compress),
            seconds(&self.write),
            seconds(&self.throttled),
        )
    }
}

pub struct ChunkWriterConfig<'a> {
    pub threads: usize,
    pub zstd_level: i32,
    /// Scratch budget to throttle against, if a previous pass is still
    /// freeing scratch space.
    pub budget: Option<u64>,
    /// Bytes currently occupied by scratch files.
    pub usage: &'a Gauge,
}

/// Spawns threads that compress chunks received as `(file, raw)` and append
/// them to the scratch files, throttled by the scratch budget. They finish
/// once all senders are dropped.
pub fn spawn_chunk_writers<'s>(
    s: &'s thread::Scope<'s, '_>,
    config: &'s ChunkWriterConfig<'s>,
    files: &'s [Mutex<ScratchFile>],
    rx: channel::Receiver<(usize, Vec<u8>)>,
    times: &'s ChunkWriterTimes,
) {
    for _ in 0..config.threads {
        let rx = rx.clone();
        s.spawn(move || {
            let mut compressor = ChunkCompressor::new(config.zstd_level);
            while let Ok((file, raw)) = timed(&times.recv, || rx.recv()) {
                let compressed = timed(&times.compress, || compressor.compress(&raw));
                if let Some(budget) = config.budget {
                    let waited = config
                        .usage
                        .wait_until(|usage| usage + compressed.padded_len() as u64 <= budget);
                    times
                        .throttled
                        .fetch_add(waited.as_nanos() as u64, Ordering::Relaxed);
                }
                timed(&times.write, || {
                    files[file].lock().expect("lock scratch file").append(
                        raw.len(),
                        compressed,
                        config.usage,
                    )
                })
                .expect("append chunk");
            }
        });
    }
}
