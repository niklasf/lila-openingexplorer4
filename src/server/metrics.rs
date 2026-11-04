use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use crate::server::app::Kind;

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
    fn fields(&self, prefix: &str, out: &mut Vec<String>) {
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

/// Request counters for `/monitor`.
#[derive(Default)]
pub struct Metrics {
    /// Valid explorer requests, answered from the response cache or
    /// computed.
    pub requests: Counter,
    /// Explorer requests that computed their response.
    pub computed: Counter,
    pub bad_requests: Counter,
    /// Positions found in the positions column family.
    pub positions_hits: Counter,
    /// Positions validated from a cold candidate.
    pub cold_hits: Counter,
    /// Positions found in neither column family.
    pub misses: Counter,
    /// Cold candidates loaded and replayed.
    pub cold_games_loaded: Counter,
    /// Games loaded for the moves, top and recent games of responses.
    pub response_games_loaded: Counter,
    /// Loaded games that were not in the block cache (only counted with
    /// parallel fetch).
    pub games_fetched: Counter,
    /// Masters only.
    pub pgn_requests: Counter,
    /// Computed responses: time spent computing on the blocking pool.
    pub compute: Histogram,
    /// Computed responses: time from receiving the request to the response
    /// (including queueing for the blocking pool).
    pub request: Histogram,
}

impl Metrics {
    /// The fields relevant for the served database. Takes the maxima of the
    /// histograms, so that they cover the time since the previous call.
    pub fn fields(&self, kind: Kind) -> Vec<String> {
        let mut counters = vec![
            ("requests", &self.requests),
            ("computed", &self.computed),
            ("bad_requests", &self.bad_requests),
            ("positions_hits", &self.positions_hits),
            ("cold_hits", &self.cold_hits),
            ("misses", &self.misses),
            ("cold_games_loaded", &self.cold_games_loaded),
            ("response_games_loaded", &self.response_games_loaded),
            ("games_fetched", &self.games_fetched),
        ];
        if kind == Kind::Masters {
            counters.push(("pgn_requests", &self.pgn_requests));
        }
        let mut out: Vec<String> = counters
            .into_iter()
            .map(|(name, counter)| format!("{name}={}u", counter.get()))
            .collect();
        self.compute.fields("compute", &mut out);
        self.request.fields("request", &mut out);
        out
    }
}
