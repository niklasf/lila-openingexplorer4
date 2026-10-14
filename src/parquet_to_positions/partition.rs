//! Memory partitions: the records of one slice of the key space, as sorted
//! runs that are merged when writing.

use std::{cmp::Reverse, collections::BinaryHeap, sync::Mutex, thread};

use crate::{
    packed::Zobrist96,
    parquet_to_positions::record::{CHUNK_RECORDS, Record, decode_chunk},
    scatter::{AlignedBuf, ChunkDecompressor, ScratchFile},
};

/// A memory partition: sorted runs of records.
#[derive(Default)]
pub struct MemoryPartition {
    pub runs: Vec<Vec<Record>>,
}

impl MemoryPartition {
    pub fn len(&self) -> usize {
        self.runs.iter().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Loads a scratch file into one run per thread, decompressing and
    /// sorting in parallel. Chunks are assigned to threads round robin, so
    /// that the runs can be allocated exactly.
    pub fn load(scratch: &ScratchFile, threads: usize) -> MemoryPartition {
        let chunks = scratch.chunks();
        let threads = threads.clamp(1, chunks.len().max(1));
        let runs: Vec<Mutex<Vec<Record>>> = (0..threads)
            .map(|t| {
                let records: usize = chunks
                    .iter()
                    .skip(t)
                    .step_by(threads)
                    .map(|c| c.raw_len as usize / std::mem::size_of::<Record>())
                    .sum();
                Mutex::new(Vec::with_capacity(records))
            })
            .collect();

        thread::scope(|s| {
            for (t, run) in runs.iter().enumerate() {
                s.spawn(move || {
                    let mut run = run.lock().expect("lock run");
                    let mut decompressor = ChunkDecompressor::new(scratch.zstd_level());
                    let mut compressed = AlignedBuf::new();
                    let mut raw = Vec::with_capacity(CHUNK_RECORDS * std::mem::size_of::<Record>());
                    for (i, chunk) in chunks.iter().enumerate().skip(t).step_by(threads) {
                        let bytes = scratch.read_chunk(i, &mut compressed).expect("read chunk");
                        raw.clear();
                        raw.resize(chunk.raw_len as usize, 0);
                        decompressor.decompress(bytes, &mut raw);
                        decode_chunk(&raw, &mut run);
                    }
                    run.sort_unstable();
                });
            }
        });

        MemoryPartition {
            runs: runs
                .into_iter()
                .map(|run| run.into_inner().expect("run"))
                .filter(|run| !run.is_empty())
                .collect(),
        }
    }

    /// Sorts each run, in parallel.
    pub fn sort_runs(&mut self, threads: usize) {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let slots: Vec<Mutex<&mut Vec<Record>>> = self.runs.iter_mut().map(Mutex::new).collect();
        thread::scope(|s| {
            for _ in 0..threads.clamp(1, slots.len().max(1)) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(slot) = slots.get(i) else { break };
                        slot.lock().expect("lock run").sort_unstable();
                    }
                });
            }
        });
    }

    /// Merges the runs over the hashes in `lo..hi` (`hi` exclusive, or
    /// unbounded if `None`).
    pub fn merge_range(&self, lo: Zobrist96, hi: Option<Zobrist96>) -> RangeMerge<'_> {
        let mut heap = BinaryHeap::with_capacity(self.runs.len());
        let mut cursors = Vec::with_capacity(self.runs.len());
        for (r, run) in self.runs.iter().enumerate() {
            let start = run.partition_point(|record| record.hash() < lo);
            let end = match hi {
                Some(hi) => run.partition_point(|record| record.hash() < hi),
                None => run.len(),
            };
            cursors.push((start, end));
            if start < end {
                heap.push(Reverse((run[start], r)));
            }
        }
        RangeMerge {
            runs: &self.runs,
            cursors,
            heap,
        }
    }
}

/// Iterator over the records of a key range, in order.
pub struct RangeMerge<'a> {
    runs: &'a [Vec<Record>],
    /// Next index and end index per run.
    cursors: Vec<(usize, usize)>,
    heap: BinaryHeap<Reverse<(Record, usize)>>,
}

impl Iterator for RangeMerge<'_> {
    type Item = Record;

    #[inline]
    fn next(&mut self) -> Option<Record> {
        let Reverse((record, r)) = self.heap.pop()?;
        let (next, end) = &mut self.cursors[r];
        *next += 1;
        if *next < *end {
            self.heap.push(Reverse((self.runs[r][*next], r)));
        }
        Some(record)
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;
    use crate::parquet_to_positions::record::RECORD_SIZE;

    #[test]
    fn test_merge_range() {
        arbtest(|u| {
            let mut partition = MemoryPartition::default();
            let mut all: Vec<Record> = Vec::new();
            for run in u.arbitrary_iter::<Vec<[u8; RECORD_SIZE]>>()? {
                let mut run: Vec<Record> = run?.into_iter().map(Record).collect();
                run.sort_unstable();
                all.extend(&run);
                partition.runs.push(run);
            }
            let bits = u.int_in_range(0..=4)?;
            let p = u.int_in_range(0..=(1usize << bits) - 1)?;
            let lo = Zobrist96::partition_start(p, bits);
            let hi = (p + 1 < 1 << bits).then(|| Zobrist96::partition_start(p + 1, bits));
            let merged: Vec<Record> = partition.merge_range(lo, hi).collect();
            all.sort_unstable();
            let expected: Vec<Record> = all
                .into_iter()
                .filter(|r| r.hash() >= lo && hi.is_none_or(|hi| r.hash() < hi))
                .collect();
            assert_eq!(merged, expected);
            Ok(())
        });
    }
}
