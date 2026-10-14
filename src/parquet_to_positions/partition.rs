//! Memory partitions: the records of one slice of the key space, as runs
//! of sorted buckets that are merged when writing.

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use crate::{
    parquet_to_positions::record::{CHUNK_RECORDS, Record, decode_chunk},
    scatter::{AlignedBuf, ChunkDecompressor, ScratchFile},
};

/// Bits of the hash, after the partition bits, that split a run into
/// buckets. Sorting small buckets is faster than sorting a whole run, and
/// the writer merges bucket by bucket.
pub const RUN_BUCKET_BITS: u32 = 12;

/// Buckets per run.
pub const RUN_BUCKETS: usize = 1 << RUN_BUCKET_BITS;

/// Records of one loading thread, by bucket, each bucket sorted.
pub struct Run {
    buckets: Vec<Vec<Record>>,
}

impl Run {
    fn with_expected_records(records: usize) -> Run {
        // Hashes are uniform, so buckets rarely exceed the expected size
        // by more than a few percent.
        let per_bucket = records / RUN_BUCKETS;
        let capacity = per_bucket + per_bucket / 16 + 16;
        Run {
            buckets: (0..RUN_BUCKETS)
                .map(|_| Vec::with_capacity(capacity))
                .collect(),
        }
    }

    #[inline]
    fn push(&mut self, record: Record, shift: u32) {
        let bucket = (record.hash_prefix() >> shift) as usize & (RUN_BUCKETS - 1);
        self.buckets[bucket].push(record);
    }

    fn sort(&mut self) {
        for bucket in &mut self.buckets {
            bucket.sort_unstable();
        }
    }

    fn len(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }
}

/// A memory partition: runs of sorted buckets.
#[derive(Default)]
pub struct MemoryPartition {
    pub runs: Vec<Run>,
}

impl MemoryPartition {
    pub fn len(&self) -> usize {
        self.runs.iter().map(Run::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Loads a scratch file into one run per thread, decompressing and
    /// sorting in parallel. Chunks are assigned to threads round robin.
    /// `partition_bits` are the hash bits shared by all records of the
    /// partition.
    pub fn load(scratch: &ScratchFile, threads: usize, partition_bits: u32) -> MemoryPartition {
        let chunks = scratch.chunks();
        let threads = threads.clamp(1, chunks.len().max(1));
        let shift = 64 - partition_bits - RUN_BUCKET_BITS;
        let runs: Vec<Mutex<Run>> = (0..threads)
            .map(|t| {
                let records: usize = chunks
                    .iter()
                    .skip(t)
                    .step_by(threads)
                    .map(|c| c.raw_len as usize / std::mem::size_of::<Record>())
                    .sum();
                Mutex::new(Run::with_expected_records(records))
            })
            .collect();

        thread::scope(|s| {
            for (t, run) in runs.iter().enumerate() {
                s.spawn(move || {
                    let mut run = run.lock().expect("lock run");
                    let mut decompressor = ChunkDecompressor::new(scratch.zstd_level());
                    let mut compressed = AlignedBuf::new();
                    let mut raw = Vec::with_capacity(CHUNK_RECORDS * std::mem::size_of::<Record>());
                    let mut records = Vec::with_capacity(CHUNK_RECORDS);
                    for (i, chunk) in chunks.iter().enumerate().skip(t).step_by(threads) {
                        let bytes = scratch.read_chunk(i, &mut compressed).expect("read chunk");
                        raw.clear();
                        raw.resize(chunk.raw_len as usize, 0);
                        decompressor.decompress(bytes, &mut raw);
                        records.clear();
                        decode_chunk(&raw, &mut records);
                        for &record in &records {
                            run.push(record, shift);
                        }
                    }
                    run.sort();
                });
            }
        });

        MemoryPartition {
            runs: runs
                .into_iter()
                .map(|run| run.into_inner().expect("run"))
                .collect(),
        }
    }

    /// Builds a partition from unsorted chunks of records, one run per
    /// thread.
    pub fn from_unsorted(
        chunks: Vec<Vec<Record>>,
        threads: usize,
        partition_bits: u32,
    ) -> MemoryPartition {
        let threads = threads.clamp(1, chunks.len().max(1));
        let shift = 64 - partition_bits - RUN_BUCKET_BITS;
        let chunks: Vec<Mutex<Option<Vec<Record>>>> =
            chunks.into_iter().map(|c| Mutex::new(Some(c))).collect();
        let records: usize = chunks
            .iter()
            .map(|c| c.lock().expect("lock chunk").as_ref().map_or(0, Vec::len))
            .sum();
        let next = AtomicUsize::new(0);
        let runs: Vec<Mutex<Run>> = (0..threads)
            .map(|_| Mutex::new(Run::with_expected_records(records / threads)))
            .collect();
        thread::scope(|s| {
            for run in &runs {
                s.spawn(|| {
                    let mut run = run.lock().expect("lock run");
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(chunk) = chunks.get(i) else { break };
                        let chunk = chunk.lock().expect("lock chunk").take().expect("chunk");
                        for record in chunk {
                            run.push(record, shift);
                        }
                    }
                    run.sort();
                });
            }
        });
        MemoryPartition {
            runs: runs
                .into_iter()
                .map(|run| run.into_inner().expect("run"))
                .collect(),
        }
    }

    /// Records in a range of buckets.
    pub fn bucket_range_len(&self, buckets: std::ops::Range<usize>) -> usize {
        self.runs
            .iter()
            .map(|run| {
                run.buckets[buckets.clone()]
                    .iter()
                    .map(Vec::len)
                    .sum::<usize>()
            })
            .sum()
    }

    /// Merges the runs over a range of buckets.
    pub fn merge_buckets(&self, buckets: std::ops::Range<usize>) -> BucketMerge<'_> {
        let mut merge = BucketMerge {
            runs: &self.runs,
            buckets,
            cursors: vec![0; self.runs.len()],
            heap: BinaryHeap::with_capacity(self.runs.len()),
        };
        merge.start_bucket();
        merge
    }
}

/// Iterator over the records of a range of buckets, in order.
pub struct BucketMerge<'a> {
    runs: &'a [Run],
    /// Remaining buckets; the first is the current one.
    buckets: std::ops::Range<usize>,
    /// Next index per run within the current bucket.
    cursors: Vec<usize>,
    heap: BinaryHeap<Reverse<(Record, usize)>>,
}

impl BucketMerge<'_> {
    /// Starts merging the first non-empty bucket at or after the current
    /// one, if any.
    fn start_bucket(&mut self) {
        while self.buckets.start < self.buckets.end {
            let bucket = self.buckets.start;
            for (r, run) in self.runs.iter().enumerate() {
                self.cursors[r] = 0;
                if let Some(&record) = run.buckets[bucket].first() {
                    self.heap.push(Reverse((record, r)));
                }
            }
            if !self.heap.is_empty() {
                return;
            }
            self.buckets.start += 1;
        }
    }
}

impl Iterator for BucketMerge<'_> {
    type Item = Record;

    #[inline]
    fn next(&mut self) -> Option<Record> {
        loop {
            if let Some(Reverse((record, r))) = self.heap.pop() {
                let bucket = &self.runs[r].buckets[self.buckets.start];
                self.cursors[r] += 1;
                if let Some(&next) = bucket.get(self.cursors[r]) {
                    self.heap.push(Reverse((next, r)));
                }
                return Some(record);
            }
            // Current bucket exhausted.
            if self.buckets.start >= self.buckets.end {
                return None;
            }
            self.buckets.start += 1;
            self.start_bucket();
            if self.heap.is_empty() {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;
    use crate::parquet_to_positions::record::RECORD_SIZE;

    #[test]
    fn test_from_unsorted_and_merge() {
        arbtest(|u| {
            let partition_bits = u.int_in_range(0..=8)?;
            let mut all: Vec<Record> = Vec::new();
            let mut chunks: Vec<Vec<Record>> = Vec::new();
            for chunk in u.arbitrary_iter::<Vec<[u8; RECORD_SIZE]>>()? {
                let chunk: Vec<Record> = chunk?
                    .into_iter()
                    .map(|mut r| {
                        // Within the partition.
                        r[0] &= (0xffu32 >> partition_bits) as u8;
                        Record(r)
                    })
                    .collect();
                all.extend(&chunk);
                chunks.push(chunk);
            }
            let partition = MemoryPartition::from_unsorted(chunks, 3, partition_bits);
            assert_eq!(partition.len(), all.len());
            all.sort_unstable();
            let lo = u.int_in_range(0..=RUN_BUCKETS)?;
            let hi = u.int_in_range(lo..=RUN_BUCKETS)?;
            let merged: Vec<Record> = partition.merge_buckets(lo..hi).collect();
            let shift = 64 - partition_bits - RUN_BUCKET_BITS;
            let expected: Vec<Record> = all
                .iter()
                .copied()
                .filter(|r| {
                    let b = (r.hash_prefix() >> shift) as usize & (RUN_BUCKETS - 1);
                    (lo..hi).contains(&b)
                })
                .collect();
            assert_eq!(merged, expected);
            assert_eq!(partition.bucket_range_len(lo..hi), expected.len());
            Ok(())
        });
    }
}
