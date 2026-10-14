//! Shared machinery for sorting records from parquet files into SST files:
//! a key space split into passes and memory partitions, and scratch files
//! for partitions that do not stay in memory during a pass.

pub mod chunk;
pub mod scratch;

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

pub use chunk::{AlignedBuf, ChunkCompressor, ChunkDecompressor, Compressed, DIRECT_ALIGN};
pub use scratch::{ChunkMeta, Gauge, ScratchFile};

/// How the key space is split: `1 << disk_bits` passes over the input, each
/// covering `1 << (memory_bits - disk_bits)` memory partitions, of which the
/// last `resident` are kept in memory rather than written to scratch space.
#[derive(Debug, Copy, Clone)]
pub struct Layout {
    pub memory_bits: u32,
    pub disk_bits: u32,
    pub resident: usize,
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
        self.memory_partitions_per_pass() - self.resident
    }

    /// Pass that handles the given memory partition.
    pub fn pass_of(self, memory_partition: usize) -> usize {
        memory_partition >> (self.memory_bits - self.disk_bits)
    }

    /// Index of the memory partition within the given pass, or `None` if it
    /// belongs to a different pass.
    pub fn pass_memory_partition(self, memory_partition: usize, pass: usize) -> Option<usize> {
        (self.pass_of(memory_partition) == pass)
            .then_some(memory_partition & (self.memory_partitions_per_pass() - 1))
    }

    /// Next layout to try when the budgets are exceeded: another pass if
    /// scratch space is short and passes can still be added, otherwise twice
    /// the memory partitions.
    pub fn grow(self, fits_scratch: bool) -> Layout {
        if !fits_scratch && self.disk_bits < self.memory_bits {
            Layout {
                disk_bits: self.disk_bits + 1,
                ..self
            }
        } else {
            Layout {
                memory_bits: self.memory_bits + 1,
                ..self
            }
        }
    }
}

/// Runs `f` and adds the elapsed nanoseconds to `counter`.
pub fn timed<T>(counter: &AtomicU64, f: impl FnOnce() -> T) -> T {
    let timer = Instant::now();
    let result = f();
    counter.fetch_add(timer.elapsed().as_nanos() as u64, Ordering::Relaxed);
    result
}

/// Seconds accumulated in a `timed` counter.
pub fn seconds(counter: &AtomicU64) -> f64 {
    counter.load(Ordering::Relaxed) as f64 / 1e9
}
