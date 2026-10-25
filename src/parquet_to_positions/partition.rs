use std::{
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use crate::{intermediate::IntermediateEntry, scatter::ScratchFile};

/// Sub-buckets of a memory partition, by the hash bits below the partition
/// bits. They are sorted in parallel and later written in parallel.
pub const BUCKET_BITS: u32 = 8;
pub const BUCKETS: usize = 1 << BUCKET_BITS;

/// Records staged per bucket by each thread before they are appended to the
/// shared bucket.
const STAGE: usize = 1024;

/// The memory partition of an entry.
#[inline]
pub fn memory_partition(entry: &IntermediateEntry, memory_bits: u32) -> usize {
    (entry
        .hash_prefix()
        .checked_shr(64 - memory_bits)
        .unwrap_or(0)) as usize
}

#[inline]
fn bucket(entry: &IntermediateEntry, memory_bits: u32) -> usize {
    ((entry.hash_prefix() << memory_bits) >> (64 - BUCKET_BITS)) as usize
}

/// Converts raw chunk bytes to entries.
pub fn entries(raw: &[u8]) -> impl Iterator<Item = IntermediateEntry> + '_ {
    let (records, rest) = raw.as_chunks::<{ IntermediateEntry::SIZE }>();
    assert!(rest.is_empty(), "records truncated");
    records.iter().map(|record| IntermediateEntry(*record))
}

/// Records of a memory partition, distributed to buckets in key order.
pub struct Partition {
    pub buckets: Vec<Vec<IntermediateEntry>>,
}

impl Partition {
    pub fn records(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }

    /// Sorts the buckets in parallel. Afterwards all records of the
    /// partition are in order.
    pub fn sort(&mut self, threads: usize) {
        let next = AtomicUsize::new(0);
        let slots: Vec<Mutex<&mut Vec<IntermediateEntry>>> =
            self.buckets.iter_mut().map(Mutex::new).collect();
        thread::scope(|s| {
            for _ in 0..threads.clamp(1, BUCKETS) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(slot) = slots.get(i) else {
                            break;
                        };
                        slot.lock()
                            .expect("lock bucket")
                            .sort_unstable_by_key(IntermediateEntry::sort_key);
                    }
                });
            }
        });
    }
}

/// Distributes records to shared buckets, staging them per thread.
struct Bucketer<'a> {
    memory_bits: u32,
    buckets: &'a [Mutex<Vec<IntermediateEntry>>],
    staged: Vec<Vec<IntermediateEntry>>,
}

impl<'a> Bucketer<'a> {
    fn new(memory_bits: u32, buckets: &'a [Mutex<Vec<IntermediateEntry>>]) -> Bucketer<'a> {
        Bucketer {
            memory_bits,
            buckets,
            staged: (0..BUCKETS).map(|_| Vec::with_capacity(STAGE)).collect(),
        }
    }

    #[inline]
    fn push(&mut self, entry: IntermediateEntry) {
        let b = bucket(&entry, self.memory_bits);
        let staged = &mut self.staged[b];
        staged.push(entry);
        if staged.len() >= STAGE {
            self.buckets[b].lock().expect("lock bucket").append(staged);
        }
    }

    fn flush(&mut self) {
        for (b, staged) in self.staged.iter_mut().enumerate() {
            if !staged.is_empty() {
                self.buckets[b].lock().expect("lock bucket").append(staged);
            }
        }
    }
}

/// Shared buckets with capacity for about `records` uniformly distributed
/// records.
fn shared_buckets(records: usize) -> Vec<Mutex<Vec<IntermediateEntry>>> {
    let capacity = records / BUCKETS + records / BUCKETS / 16 + 2 * STAGE;
    (0..BUCKETS)
        .map(|_| Mutex::new(Vec::with_capacity(capacity)))
        .collect()
}

fn into_partition(buckets: Vec<Mutex<Vec<IntermediateEntry>>>) -> Partition {
    Partition {
        buckets: buckets
            .into_iter()
            .map(|bucket| bucket.into_inner().expect("bucket"))
            .collect(),
    }
}

/// Loads a scratch file in parallel, distributing its records to buckets.
pub fn load(scratch: &ScratchFile, memory_bits: u32, threads: usize) -> Partition {
    let records = scratch.raw_len() as usize / IntermediateEntry::SIZE;
    let buckets = shared_buckets(records);
    let bucketers = scratch.load(
        threads,
        || Bucketer::new(memory_bits, &buckets),
        |bucketer, _, raw| {
            for entry in entries(raw) {
                bucketer.push(entry);
            }
        },
    );
    for mut bucketer in bucketers {
        bucketer.flush();
    }
    into_partition(buckets)
}

/// Distributes the records collected by multiple threads to buckets, in
/// parallel. Each part is freed as soon as it is distributed.
pub fn from_parts(
    parts: Vec<Vec<IntermediateEntry>>,
    memory_bits: u32,
    threads: usize,
) -> Partition {
    let records = parts.iter().map(Vec::len).sum();
    let buckets = shared_buckets(records);
    let parts = Mutex::new(parts);
    thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| {
                let mut bucketer = Bucketer::new(memory_bits, &buckets);
                loop {
                    let Some(part) = parts.lock().expect("lock parts").pop() else {
                        break;
                    };
                    for entry in part {
                        bucketer.push(entry);
                    }
                }
                bucketer.flush();
            });
        }
    });
    into_partition(buckets)
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;

    #[test]
    fn test_buckets_in_key_order() {
        arbtest(|u| {
            let memory_bits = u.int_in_range(0..=12)?;
            let mut records: Vec<IntermediateEntry> = Vec::new();
            let partition_bits: u64 = u.arbitrary()?;
            for _ in 0..u.int_in_range(0..=2000)? {
                let mut bytes: [u8; IntermediateEntry::SIZE] = u.arbitrary()?;
                // All records of one memory partition.
                let prefix = u64::from_be_bytes(bytes[..8].try_into().unwrap());
                let mask = u64::MAX.checked_shr(memory_bits).unwrap_or(0);
                let prefix = (partition_bits & !mask) | (prefix & mask);
                bytes[..8].copy_from_slice(&prefix.to_be_bytes());
                records.push(IntermediateEntry(bytes));
            }
            let parts = records.chunks(7).map(<[_]>::to_vec).collect();
            let mut partition = from_parts(parts, memory_bits, 3);
            partition.sort(3);
            let mut actual: Vec<IntermediateEntry> = partition.buckets.concat();
            assert!(actual.is_sorted_by_key(IntermediateEntry::sort_key));

            actual.sort_unstable_by_key(|entry| entry.0);
            records.sort_unstable_by_key(|entry| entry.0);
            assert_eq!(actual, records);
            Ok(())
        });
    }
}
