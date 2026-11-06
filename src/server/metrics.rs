use crate::{
    server::app::Kind,
    util::{Counter, Histogram, push_counters},
};

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
    /// Loaded games that were not in the block cache, fetched with
    /// --parallel-fetch.
    pub parallel_fetch_games: Counter,
    /// Lichess only: loaded games hidden because a player is blacklisted.
    pub blacklisted_games: Counter,
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
            ("parallel_fetch_games", &self.parallel_fetch_games),
        ];
        if kind == Kind::Lichess {
            counters.push(("blacklisted_games", &self.blacklisted_games));
        }
        if kind == Kind::Masters {
            counters.push(("pgn_requests", &self.pgn_requests));
        }
        let mut out = Vec::new();
        push_counters(&mut out, counters);
        self.compute.fields("compute", &mut out);
        self.request.fields("request", &mut out);
        out
    }
}
