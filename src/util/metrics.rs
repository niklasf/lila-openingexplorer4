use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

pub fn push_field(out: &mut Vec<String>, name: &str, value: u64) {
    out.push(format!("{name}={value}u"));
}

pub fn push_counters<'a>(
    out: &mut Vec<String>,
    counters: impl IntoIterator<Item = (&'a str, &'a Counter)>,
) {
    for (name, counter) in counters {
        push_field(out, name, counter.get());
    }
}

#[derive(Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.add(1);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Maximum of the values recorded since it was last taken.
#[derive(Default)]
pub struct Max(AtomicU64);

impl Max {
    pub fn record(&self, value: u64) {
        self.0.fetch_max(value, Ordering::Relaxed);
    }

    pub fn take(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }
}

/// Latency histogram buckets, upper bounds in microseconds.
const BUCKETS: [u64; 8] = [250, 500, 1_000, 2_500, 5_000, 10_000, 50_000, u64::MAX];

#[derive(Default)]
pub struct Histogram {
    counts: [Counter; BUCKETS.len()],
    sum_micros: Counter,
    max_micros: Max,
}

impl Histogram {
    pub fn record(&self, duration: Duration) {
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        let bucket = BUCKETS
            .iter()
            .position(|&max| micros <= max)
            .expect("last bucket is unbounded");
        self.counts[bucket].inc();
        self.sum_micros.add(micros);
        self.max_micros.record(micros);
    }

    /// Cumulative bucket counts, count, sum, and the maximum since the
    /// previous call.
    pub fn fields(&self, prefix: &str, out: &mut Vec<String>) {
        let mut count = 0;
        for (counter, max) in self.counts.iter().zip(BUCKETS) {
            count += counter.get();
            if max == u64::MAX {
                out.push(format!("{prefix}_le_inf_us={count}u"));
            } else {
                out.push(format!("{prefix}_le_{max}us={count}u"));
            }
        }
        out.push(format!("{prefix}_count={count}u"));
        out.push(format!("{prefix}_sum_us={}u", self.sum_micros.get()));
        out.push(format!("{prefix}_max_us={}u", self.max_micros.take()));
    }
}
