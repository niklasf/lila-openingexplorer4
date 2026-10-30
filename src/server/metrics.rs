use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

/// Latency histogram buckets, upper bounds in microseconds.
const BUCKETS: [u64; 8] = [250, 500, 1_000, 2_500, 5_000, 10_000, 50_000, u64::MAX];

#[derive(Default)]
pub struct Histogram {
    counts: [AtomicU64; BUCKETS.len()],
    total_micros: AtomicU64,
    max_micros: AtomicU64,
}

impl Histogram {
    pub fn record(&self, duration: Duration) {
        let micros = duration.as_micros() as u64;
        let bucket = BUCKETS.iter().position(|&max| micros <= max).unwrap_or(0);
        self.counts[bucket].fetch_add(1, Ordering::Relaxed);
        self.total_micros.fetch_add(micros, Ordering::Relaxed);
        self.max_micros.fetch_max(micros, Ordering::Relaxed);
    }

    fn fields(&self, prefix: &str, out: &mut Vec<String>) {
        let mut count = 0;
        for (i, max) in BUCKETS.iter().enumerate() {
            count += self.counts[i].load(Ordering::Relaxed);
            let name = if *max == u64::MAX {
                "inf".to_owned()
            } else {
                max.to_string()
            };
            out.push(format!("{prefix}_le_{name}us={count}u"));
        }
        out.push(format!("{prefix}_count={count}u"));
        out.push(format!(
            "{prefix}_sum_us={}u",
            self.total_micros.load(Ordering::Relaxed)
        ));
        out.push(format!(
            "{prefix}_max_us={}u",
            self.max_micros.load(Ordering::Relaxed)
        ));
    }
}

/// Request counters for `/monitor`.
#[derive(Default)]
pub struct Metrics {
    /// Requests answered from the response cache.
    pub cache_hits: AtomicU64,
    /// Requests computed (cache misses).
    pub computed: AtomicU64,
    pub bad_requests: AtomicU64,
    /// Positions found in the positions column family.
    pub positions_hits: AtomicU64,
    /// Of those, served from the decoded entry cache.
    pub decoded_hits: AtomicU64,
    /// Positions validated from a cold candidate.
    pub cold_hits: AtomicU64,
    /// Cold candidates loaded and replayed.
    pub cold_candidates: AtomicU64,
    /// Positions found in neither column family.
    pub misses: AtomicU64,
    /// Games loaded for responses.
    pub games_loaded: AtomicU64,
    pub pgn_requests: AtomicU64,
    /// Time spent computing responses on the blocking pool.
    pub compute: Histogram,
    /// Time from receiving a request to the response (including queueing).
    pub request: Histogram,
}

impl Metrics {
    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub fn fields(&self) -> Vec<String> {
        let mut out = vec![
            format!("cache_hits={}u", self.cache_hits.load(Ordering::Relaxed)),
            format!("computed={}u", self.computed.load(Ordering::Relaxed)),
            format!(
                "bad_requests={}u",
                self.bad_requests.load(Ordering::Relaxed)
            ),
            format!(
                "positions_hits={}u",
                self.positions_hits.load(Ordering::Relaxed)
            ),
            format!(
                "decoded_hits={}u",
                self.decoded_hits.load(Ordering::Relaxed)
            ),
            format!("cold_hits={}u", self.cold_hits.load(Ordering::Relaxed)),
            format!(
                "cold_candidates={}u",
                self.cold_candidates.load(Ordering::Relaxed)
            ),
            format!("misses={}u", self.misses.load(Ordering::Relaxed)),
            format!(
                "games_loaded={}u",
                self.games_loaded.load(Ordering::Relaxed)
            ),
            format!(
                "pgn_requests={}u",
                self.pgn_requests.load(Ordering::Relaxed)
            ),
        ];
        self.compute.fields("compute", &mut out);
        self.request.fields("request", &mut out);
        out
    }
}
