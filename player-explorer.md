# Personal opening explorer (`/player`)

Draft plan for porting the personal opening explorer. API compatible with
the previous version (`../lila-openingexplorer`, `src/main.rs` `player()`,
`src/indexer/player*.rs`, `src/model/player.rs`); implementation free to
differ.

## What the request log says

`requests.tsv`: unsampled production log, 2026-11-03 23:30 to 2026-11-04
20:49 UTC (21.3 h), columns: log time, status, request time, upstream time,
path. 500,870 of 8,131,533 requests (6.2%) are `/player` (none
`/personal`). Request time of a `/player` request is the duration of its
whole stream.

* Rate: 6.5 req/s on average; by UTC hour from 3.2 (night) to 10.7 req/s
  (18:00), busiest minute 17 req/s, busiest second 111. Reads are cheap at
  that rate; indexing (writes, lila load) is the cost. (The 1% sample
  suggested ~12 req/s, so its scaling was off by about 2x; the share of
  6.2% agrees.)
* Status: 200 96.6%, 429 3.1% (0 ms, rejected by the reverse proxy's rate
  limit, 796 players, 1362 for the top one), 499 0.2% (client gone mid-stream),
  400 0.05% (invalid FENs).
* 11,196 distinct players (12,991 player/color pairs) in 21 h: 2,482 with
  a single request, 5,916 with >= 10, 1,250 with >= 100, 21 with >= 1000
  (top: 3,533). A player's requests come in bursts while browsing: median
  gap 1.3 s, 95.5% of gaps <= 60 s.
* Index runs implied by the cooldown (per player, first request after the
  cooldown since the last run started): 49,545 (0.65/s) with 1 min,
  37,110 (0.48/s) with 2 min, 27,768 (0.36/s) with 5 min.
* Streams are long: request time p50 0.96 s (the old server waits 1 s
  before its second snapshot), p90 8.5 s, p99 235 s, p99.9 867 s, max
  36 min. 49% take > 1 s, 15,150 requests > 60 s. Longest stream per
  player: p50 39 s, p90 411 s, p99 1,090 s. On average 65 streams are
  open (max 250), from a median of 68 (p90 103, max 131) distinct players
  with a stream > 2 s at the same time, while production runs the default
  8 indexers. So most of the waiting is probably the queue, not the
  indexing itself (the log cannot tell `queuePosition` > 0 from indexing).
  Since the number of lila streams is fixed (see requirements), latency
  depends on how well those 8 slots are used.
* `color`: white 68.8%, black 31.2%. `variant`: standard 98.5%, atomic
  0.65%, crazyhouse 0.29%, racingKings 0.13%, antichess 0.11%, chess960
  0.07% (parsed as standard by shakmaty). Root `fen` other than the
  initial position: 4.4% of standard requests.
* `modes`: both 94.1%, rated only 5.2%, casual only 0.5%. So casual games
  matter, and those are not in the monthly dumps.
* `speeds`: all but ultraBullet 74.9%, all 7.3%, without bullet 5.2%,
  then blitz/rapid/classical subsets.
* `since` 7.4%, `until` 3.3%; `recentGames` 0.7% (all but a handful `0`),
  `moves` 0.1%, `topGames` 0.5% (always `0`, ignored), `source` analysis
  97.8%, mobile 1.8%.
* Ply of the queried position: 28% at 0-9, decreasing to 13% at 40-49,
  then nearly nothing (2,145 requests at ply >= 50). Matches
  `MAX_PLIES = 50`.

## Requirements

* **At most the configured number of parallel game streams to lila**
  (`--indexers`, default 8, which production uses), to not overload lila.
  Not negotiable. This counts every request to the games export
  (`/api/games/user/...`): first runs, incremental runs, revisits of
  ongoing games, retries after errors or timeouts. No prefetching,
  speculative or overlapping streams; a retry reuses the slot of the
  failed stream. Enforced in one place (a semaphore in the lila client,
  held for the whole lifetime of a stream), and checked in tests by the
  mock lila counting concurrent connections.
* No rate limiting in the server: all rate limits are in the reverse
  proxy (nginx). The server only bounds its own resources (queue
  capacity, 503 when full).
* API compatible with the previous version (below).

Consequences of the stream limit, since the queue is the main source of
latency:

* A slot must never wait on us: indexing (replay, aggregation, batched
  writes) has to keep up with lila's 300 games/s per stream (below), with
  margin, also while RocksDB compacts.
* Scheduling of the 8 slots is the main lever, see
  [Queue scheduling](#queue-scheduling-to-explore).

## lila export

* Assume **300 games/s per stream**, so at most 2400 games/s with 8
  streams. A first run of a player with 100k games holds a slot for at
  least 5.5 min; the long tail in `requests.tsv` (longest stream per player
  p90 411 s, p99 1,090 s) fits first runs of heavy players plus queueing.
* lila does not bound our parallel streams, hence the requirement above.
* Request parameters: only `since`, `until`, `sort`, `ongoing`; filter
  everything else (rated, speed, color) ourselves, because those filters
  take a much more expensive path in lila.
* Unknown and closed accounts answer 404.

## API

### `GET /player` (and `GET /personal`, for backwards compatibility)

Query parameters, unchanged:

| Parameter | Default | Notes |
| --- | --- | --- |
| `player` | required | username or id, case-insensitive, validated as a lichess username |
| `color` | required | `white` or `black`: the side the player played |
| `variant` | `standard` | shared with `/lichess` (`Play`) |
| `fen` | initial position | shared with `/lichess` |
| `play` | empty | shared with `/lichess` |
| `speeds` | all | comma separated |
| `modes` | all | `casual`, `rated` |
| `since` | `1952-01` | month, inclusive |
| `until` | `3000-12` | month, inclusive |
| `moves` | all | number of most common moves |
| `recentGames` | 8 | clamped to 8 |
| `topGames`, `source`, ... | | accepted and ignored |

Response: `application/x-ndjson`, a stream of snapshots, each:

```json
{
  "opening": {"eco": "D06", "name": "Queen's Gambit"},
  "queuePosition": 0,
  "white": 366, "draws": 23, "black": 279,
  "moves": [
    {"uci": "h2h4", "san": "h4", "averageOpponentRating": 1674,
     "performance": 874, "white": 0, "draws": 0, "black": 1,
     "game": {"id": "9vA24xBn", "winner": "black", "speed": "bullet",
              "mode": "rated", "white": {"name": "revoof", "rating": 1657},
              "black": {"name": "MentalBlood", "rating": 1674},
              "year": 2020, "month": "2020-06"},
     "opening": null}
  ],
  "recentGames": [{"uci": "...", "id": "...", "winner": "...", "...": "..."}]
}
```

* `game` on a move only when it has exactly one game. `recentGames` are the
  newest games over all (filtered) moves. No `topGames`, no `history`.
* `averageOpponentRating`, `performance` from the player's point of view.
* Stream protocol, as before: the first snapshot immediately (whatever is
  indexed so far); while the player is queued, the same snapshot with an
  updated `queuePosition`; while indexing, a fresh snapshot about every
  second; a last snapshot once indexing is complete, then the stream ends.
  Consecutive snapshots with the same `(queuePosition, total)` are
  dropped. Empty keep-alive lines after 8 s without a line (the old
  server's `NdJson` stream in `api/nd_json.rs` did that too, for example
  while a player waited in the queue with an unchanged position).
* No new indexing if the last run was less than the cooldown ago (2 min,
  as in the old code; the API doc says 1 min); previously ongoing games are revisited at most
  once per day. The stream then has a single snapshot.
* Errors: 400 for bad parameters or illegal moves, 503 if the indexing queue
  is full. An unknown player is not an error: empty result (indexer logs it).
* Authentication of users (the OAuth requirement in `lichess-api.yaml`) is
  handled in front of us, not by this server.

### Monitoring

`/monitor` gets player metrics: requests by ply bucket and final/partial,
queue length, indexers busy, games indexed/skipped per second, lila errors
and timeouts, cold candidates (validated, false), rebuilds (count,
duration, bytes, ingested), RocksDB write stall/compaction stats.

Similar to monitoring on `/lichess`. Can break compatibility with the old
server, remove useless metrics, and add new ones.

### Possible new internal endpoints (to decide)

* `DELETE /player/{id}`: drop everything of a player (reset, for example
  after a format change, or on request). One `DeleteRange` over the
  player's key range. Closed accounts are not deleted (as before).

## Architecture

**Separate process** (decided): `explorer player --db DIR ...`, routed by
nginx.

* The lichess and masters servers are read-only and switched A/B every
  month; the personal DB is one live, writable RocksDB that only one
  process may open. Separate lifecycles, crash isolation, separate cache
  and RocksDB rate limiter settings.
* Request rate limits live in the reverse proxy, not in the server.
* Shares code with the other servers: `Play`/openings, `response.rs`,
  metrics, `packed::{GameId, Month, Speed, ...}`, `PositionKey`, the lila
  client from `blacklist.rs` (bearer handling).
* Production placement: the server has 12 hardware threads, 128 GB RAM,
  4x 6 TB HDDs (lichess and masters sets, about 1/3 of RAM as block
  caches) and 2x 512 GB NVMe in RAID 1 (OS, and reserved for the personal
  explorer). WAL and upper levels on the NVMe (`wal_dir`, `cf_paths` with a
  size budget), last level on the HDDs, compaction/flushes throttled by
  the RocksDB rate limiter.

## Data source

lila only: `GET /api/games/user/{id}?sort=dateAsc&ongoing=true&since=...`
as ndjson with the OpeningExplorer bearer token.

## Storage and indexing (decided)

A warm/cold split as in the lichess index, made live: warm entries for
positions with several games, compact cold entries for positions reached
by a single game, validated by replay. Measured 3.2x smaller than warm
entries only (273 vs 862 B per indexed game, see
[Size](#size-and-indexing-rate-estimate)), because 96% of
(player, color, position) keys have a single game.

### Data

All keys start with the user hash `uh` (8 B of a stable hash of the
lowercase user id), so each player is one contiguous key range: an index
run writes one range, a browsing session hits the same blocks (soon all in
the cache), a player is deleted or reset with one `DeleteRange`. (The old
key XORed the user hash into the Zobrist hash, which scatters a player over
the whole key space.) SST prefix compression makes the repeated prefix
cheap. A collision of two players' 64-bit hashes is ~10^-5 likely with
~10^7 players; the status stores the user id, so it would at least be
detected.

* `status`: `uh -> {user id, gen, cold bucket bits per color, games, games
  at last rebuild, latest createdAt, earliest ongoing game to revisit,
  indexed at, revisited at, format version}`.
* `games`: `uh | seq -> record + packed moves up to --max-ply`. `seq` is a
  per-player game number in indexing order. The record has the game id,
  speed, mode, month, outcome, color, both ratings and the opponent's name
  (~18 B), the moves ~40 B. A game of two indexed players is stored once
  per player. **The source of truth**: everything else is derived from it
  and can be rebuilt locally, without lila.
* `game_ids`: `uh | GameId -> seq`, to skip games already indexed (revisit
  overlap).
* `warm`: `uh | gen | color | PositionKey -> entry`. Per move, cells by
  `(speed, mode, month)`, months newest first, with stats (w/d/b, opponent
  rating sum) and the up to 8 newest `seq`s of the cell. Month in the value
  (as for lichess): one point lookup per request; it saves only 11% of keys
  here, but keeps reads simple. Ideally reuses `packed/entry.rs` (header
  with speed and mode instead of speed and rating group). Merge operator:
  add stats, keep the 8 newest games per cell.
* `cold`: `uh | gen | color | bucket -> group` of `(fingerprint suffix,
  seq)`, Rice-coded suffixes and fixed-width `seq`s like `packed/cold.rs`,
  ~64 entries per group, bucket bits per (player, color) chosen at rebuild
  time. Merge operator: union of the entries (duplicates collapse: two
  positions of one game can share a fingerprint, one entry covers both).

### Invariant: disjoint union, not "trust warm"

Every (game, position) visit is in exactly one of `warm` and `cold` of the
current `gen`. A position may have games in both, or several cold entries.
Reads take the union: `warm` get + `cold` group get, validate cold
candidates by replaying their moves (as the lichess server does), add up.

Then **placement is only an optimization, never a correctness question**:
any rule for what goes to warm or cold gives correct responses, so the rule
(and a threshold like "cold up to T games") can be tuned freely.

One constraint on placement keeps the union disjoint (found by the storage
property test): **all visits of a game with the same cold fingerprint
(color, bucket, 16 fingerprint bits) are either all cold or all warm**.
Otherwise, if position P of a game is warm and position Q of the same game
is cold with the same fingerprint, a read of P finds the game's cold item
of Q, replay confirms that the game reached P, and the game is counted
twice. With 0 bucket bits (small players) about 1.5% of games have such a
collision. The placement step enforces it after choosing the bucket bits
(colliding cold visits move to warm). Validation by replay is still needed
for the other games of the group.

Why not "trust a warm hit": then every write must first know whether the
position is already warm (else a new warm entry would hide a cold game),
and whether it is cold (to upgrade). That is a read before each position
write, an existence check of keys that still have merge operands (which
resolves the whole value, expensive for the hot entries of heavy players),
and a placement bug silently loses games. What it buys, skipping the cold
lookup on a warm hit, is worth little here: 6.5 req/s, and a browsing
session stays within one player's range, which is soon in the cache.

### Write path (no reads)

Per chunk of an index run, one atomic `WriteBatch`:

1. Replay the chunk's games (loops removed, up to `--max-ply`), aggregate
   visits per position in memory.
2. Positions with >= 2 games in the chunk: one `warm` merge operand. Others:
   one `cold` merge operand per touched group.
3. Game records with moves, `game_ids`, status checkpoint.

Only the status and `game_ids` are read. Over time placement drifts from
optimal: positions that became multi-game across chunks stay split between
warm and cold, or as several cold entries. Costs bytes and a few extra
validations per read, fixed by rebuilds.

### Rebuild per player

Rebuild the player's index from `games`: replay all games, aggregate
fully, write warm and cold of `gen + 1` with the optimal placement and cold
bucket bits for the current size, flip `gen` in the status in the last
batch, `DeleteRange` the old `gen`.

* Triggers: games since the last rebuild >= max(N, f x games at the last
  rebuild) (geometric, so with f = 0.5 each game is replayed at most ~3
  times in total), after a multi-chunk first run, after a format or
  `--max-ply` change.
* Runs in the player's indexer context (single writer per player), but
  needs no lila slot: separate small pool (1-2 threads), throttled by the
  RocksDB rate limiter.
* Cost: a player with 500k games ~1-3 s of replay, ~0.5 GB memory (or
  sorted chunks), writes about the player's size.
* Crash-safe: a new `gen` is invisible until the status flips; leftovers of
  an interrupted rebuild are deleted by the next one.
* Readers use a RocksDB snapshot per request, so the status and the keys
  they read belong to the same `gen`.

### Heavy players (up to ~1 M games)

* Index runs: batches are bounded per chunk (games and bytes, e.g. a few
  thousand games, ~45 visits each: ~100-150k operands, a few MB), never
  per run. Fine for the WAL and memtables.
* Rebuilds are the large case: ~45 M visits (~1 GB in memory while
  aggregating) and an output of roughly 0.3-1 GB for one player. Not one
  `WriteBatch`: a batch larger than the memtable stalls other writes and
  blows up memory and the WAL record. Instead:
  * Small rebuilds: several bounded batches into `gen + 1` (invisible until
    the status flip, so no atomicity is lost).
  * Large rebuilds (above ~64 MB of output): write the new `gen` with an
    `SstFileWriter` (keys are produced sorted anyway) and ingest it. The
    range `uh | gen + 1` overlaps nothing, so the file can go straight to
    the bottom level: no WAL, no memtable, no compaction rewrites. The
    threshold avoids many tiny files.
  * Old `gen`: `DeleteRange`, space comes back with compaction.
  * At most 1-2 rebuilds at a time (memory), under the rate limiter.
* Hot warm entries (the root position of such a player: thousands of cells,
  some 100 KB) collect one merge operand per chunk until compaction, so
  reads resolve many operands. Acceptable at our request rate; rebuilds
  reset it.
* Cold groups: bucket bits chosen at rebuild for ~64 entries per group,
  so ~20 M singletons per color need ~18 bits.
* The real problem is time: 1 M games at 300 games/s hold a lila slot for
  ~55 min. That is for the queue scheduling (chunked runs that requeue).

### Testing

Arbtest against a naive aggregation: random games, random chunk
boundaries, random placement decisions and random rebuild points must all
give identical responses.

## Size and indexing rate estimate

Basis: in the previous version the `player` CF has consistently stayed
near half the keys of the `lichess` CF. Both keyed by month, so a key
there is a `(position, month)` pair for lichess and a
`(player, color, position, month)` tuple for players.

* Old `lichess` keys per game: the README `/monitor` example of
  2023-02-19 (dba9df5) has `lichess=121,970,833,029u` and
  `lichess_game=4,331,746,117u`, so ~28 keys per game (`lichess_game` also
  holds personal-only games, so per lichess game slightly more).
  (Same snapshot: `player=18,693,470,276u`, 15% of `lichess`, and
  `player_status=182,129u`. The "half" probably held later; RocksDB key
  estimates are rough anyway.)
* Today, 8.25 B games: ~230 B old `lichess` keys, so **~115 B old-format
  player keys**.
* Growth at ~100 M games/month: ~2.8 B `lichess` keys
  per month, so ~1.4 B player keys per month, **~540 keys/s on average**,
  ~1,100/s at peak. That is only ~14 games/s on average (measured below),
  against a lila limit of 2400 games/s. The
  average write rate is small; first runs of heavy players come in bursts.
* Measured: 38.1 old-format keys per indexed (game, side), so ~3.0 B
  indexed (game, side) pairs today and ~37 M per month (~14/s). The old
  production database actually indexed ~28 pairs/s (see below).

### Measured (2026-11-04)

`examples/player_layout_stats.rs` on the complete rated histories of the
11,195 players of `requests.tsv` (9,676 with rated games): one parquet
pass (4 min), then real SST files per layout (16 KiB blocks, zstd 3,
10-bit filters on the index CFs), keys `uh | color | ...`. Logs and SSTs in
`/mnt/workspace_nvme/agent/scratch/player_layout/`.

* 76.2 M indexed (game, side) pairs, 19,250 sides, up to 868k games on one
  side. 3.50 B visits (45.9 per game).
* 2.57 B keys `(player, color, position)` (33.7 per game) vs 2.90 B with
  month (38.1 per game): folding the month into the value saves only 11%.
* **96.0% of the keys are singletons** (2.47 B). The 4% multi-game keys
  hold 1.04 B visits (30%).

| Layout | Size | per entry | per game |
| --- | --- | --- | --- |
| `warm_all`: every key a full entry | 64.4 GB | 25.0 B | 844 B |
| `warm_multi`: keys with >= 2 games | 6.2 GB | 59.3 B | 81 B |
| `cold_plain`: `uh\|c\|fp32\|seq32`, no value | 21.6 GB | 8.8 B | 284 B |
| `cold_grouped`: ~64 per group, Rice-coded | 10.2 GB | 4.1 B per singleton | 134 B |
| `records`: id, metadata, opponent | 1.4 GB | 18.3 B | 18 B |
| `moves`: packed, up to 50 plies | 3.0 GB | 39.7 B | 40 B |

Per indexed (game, side), with records (and moves where cold needs them):

* warm only: **862 B**
* split, plain cold keys: 423 B (2.0x smaller)
* split, grouped cold: **273 B (3.2x smaller)**

Caveats: rated games only (casual games, about half in reality, add more
of the same kind);
entry format a stand-in for the real one; cold groups capped at 16 bucket
bits, so the heaviest players have larger groups than intended; no drift
between rebuilds counted.

Extrapolated with the "half the old lichess keys" basis (~115 B old-format
keys, so ~3.0 B indexed (game, side) pairs today): warm only ~2.6 TB,
split plain ~1.3 TB, **split grouped ~0.8 TB**. Growth ~37 M pairs per
month (~14/s on average), ~10 GB per month with the grouped split. The disk
planning reserved ~1.4 TiB (half a lichess set) for the personal explorer:
the split fits with room to spare, warm only would not.

### Old production database (2026-11-04)

`rocksdb.stats` of the old server (different schema: month in the key,
player hash XORed into the position hash, no cold split), after 121 days
of uptime:

* `player` CF: **3.48 TB**, 99% in L6 (3.45 TB, 56k files); L0-L5 together
  only ~30 GB. At ~25-30 B per old-format key that is ~115-140 B keys, so
  ~3.0-3.7 B indexed (game, side) pairs: consistent with the estimate
  above. With the measured 273 B per pair: **~0.8-1.0 TB** in the new
  format (before the small-player correction below).
* `player_status`: **~1.28 M players**. So ~2.4-2.9k games per player on
  average, a third of the 7.9k of the `requests.tsv` players (the daily
  active players are the heavy ones). Small players have small cold groups
  with relatively more per-key overhead, so the measured 273 B per game is
  optimistic. Re-measure on a population that matches (see below).
* Indexing rate: 343 GB flushed and ~11 G keys into L0 in 121 days:
  ~1,050 merge operands/s, so **~28 (game, side) pairs/s** on average
  (~72 M per month, first runs of new players included), about twice the
  growth-based estimate. Use this for the write load.
* **Write amplification ~91**: 31.3 TB of compaction writes for 343 GB
  flushed, 3 MB/s around the clock on the HDDs. Almost all in L6 (W-Amp
  75: 26 TB of L6 rewritten to merge 355 GB). The levels are statically
  sized (base 256 MB, x10: L3 at 0.6 of 25.6 GB, L4/L5 nearly empty with
  data trivially moved through), so L6 is ~350x its upper level instead
  of 10x, and every byte merged into it rewrites ~350 bytes. For the new
  database: `level_compaction_dynamic_level_bytes` (each level ~1/10 of
  the next, from the bottom up), rebuilds of large players by SST ingest
  instead of compaction, and compaction bytes per indexed game as an
  explicit benchmark metric.
* Block cache (100 GB, shared by all CFs): **68 GB filter blocks**, 31 GB
  data, 1 GB index. Filters crowd out data. New: ~4 B warm keys and ~2 B
  cold groups at 10 bits each are ~8 GB of filters; partitioned filters,
  and our own cache (the lichess server is a separate process now).
* Read latency L6: ~80% of reads in < 15 us (cache), ~19% from disk at
  10-250 ms (busy HDDs: compaction plus lichess). Player-clustered keys and
  upper levels on the SSDs should cut both the number and the cost of disk
  reads per request.
* `lichess_game` (games of both explorers): 225 GB, nearly all in L4.

Possible further levers: a cold threshold T > 1 (positions with 2-3 games
as cold entries instead of a ~59 B warm entry; needs the per-key count
histogram), and a compact singleton warm format.

## Queue scheduling (to explore)

With 8 slots at 300 games/s, scheduling decides latency. And with no
migration (decided: start empty), every player's first request after the
launch is a first run, so the launch is the worst case for weeks.

Ideas:

* Drop runs nobody waits for any more (old: skipped when acquiring if all
  tickets are dropped); also stop a running stream once all its clients
  are gone, and resume from the checkpoint later.
* Chunked runs: a slot indexes at most K games or T seconds, then the run
  requeues at the end with `since` = checkpoint. Short incremental runs
  are no longer stuck behind a first run of 300k games. Costs one more
  lila request per chunk (cheap: indexed `createdAt` cursor).
* Priority by expected run length (games since the last run; unknown for
  first runs, perhaps from lila's user counts or our own last run), or by
  how many requests a player has open.
* Revisit interval as a parameter of the simulation (cooldown fixed at
  2 min).

How: a discrete-event simulation driven by `requests.tsv` (arrival time
per player, burst structure, client gives up = request time), with run
lengths from game counts per player (rated games from the parquet, x2
for casual games) at 300 games/s, comparing policies by time to first
complete result, p50/p99 stream duration and slot utilization, for the
launch (empty DB) and the steady state. Then implement the winner.

### Simulation results (2026-11-05)

`examples/player_queue_sim.rs` (uncommitted), games per player from the
mock lila fixture. Client model from the log: **the UI aborts a stream
when the user moves on** (78% of the streams > 2 s end within 0.3 s of the
player's next request), so a ticket lives through a browsing burst and only
the last request of a burst waits; plus a patience limit for that one.

* Steady state (every player indexed up to the start of the log): the
  21 h add only ~0.1 M games for these players, 2% slot utilization, every
  policy alike (99.7% of requests complete, p99 0 s). Scheduling only
  matters while there is a backlog, above all at the launch.
* Launch (empty database), all 21 h, patience 600 s (120 s in brackets):

| Policy | complete | run p50 | run p90 | util |
| --- | --- | --- | --- | --- |
| FIFO, run to the end (old) | 43% (57%) | 104 s | 420 s | 83% |
| FIFO, stop abandoned runs | 50% (77%) | 56 s | 306 s | 80% |
| quantum 30 s, requeue | 69% (82%) | 15 s | 81 s | 80% |
| quantum 60 s | 63% (80%) | 24 s | 126 s | 79% |
| quantum 30 s, known players first | **80% (85%)** | **4 s** | 71 s | 80% |
| quantum 60 s, known players first | 78% (84%) | 5 s | 93 s | 79% |

  "complete": requests that got their final snapshot before the client
  closed; "run": time from the ticket to the end of indexing. The launch
  backlog of these players alone is ~150 M games (x2 casual), 17 h of all
  8 slots, so the first day is saturated whatever the policy.

Check against the implementation (`examples/player_replay.rs`, uncommitted):
first 2 h of the log at 10x against an empty database, mock lila at 3000
games/s per stream (scaled), cooldown and quantum scaled: 7.9 M games
streamed, 46% slot utilization, p99 time to the final snapshot 20 s, 79%
of requests complete, 0 stream limit violations (max 8 streams), 538
requeues. The simulation of the same window with the same volume (the mock
marks half the rated games casual instead of doubling them, so
`--casual-factor 1`): 7.9 M games, 44%, p99 19 s, 86%.

**Implemented**: runs give up their slot after `--max-run` (default 30 s)
at a chunk boundary and requeue at the end; players with an index
(incremental runs) are queued before the first waiting first run;
running runs stop at the next chunk once no request waits (resuming from
the checkpoint later). `--fifo` and `--max-run 0` give the old order.

## Indexer

Ported mostly as is:

* Queue: deduplicated tickets per player, capacity 2000, queue position
  for the stream, completion notification. Policy from the scheduling
  exploration above.
* N indexer tasks (`--indexers`, default 8), each holding at most one lila
  stream (the requirement above), each: read status, decide run (index new
  since `latest_created_at + 1 ms`, or revisit ongoing since the earliest
  ongoing game), stream games from lila with timeouts, index them on the
  blocking pool.
* lila 404 (unknown or closed account): log, finish the run normally (so
  the cooldown applies), keep what is already indexed. As before.
* Per game: skip ongoing (remember for revisit), aborted/noStart/
  unknownFinish, games where either side has no account or no rating, and
  games already indexed (`game_ids`; covers the revisit overlap). `chess960` and `fromPosition` map to standard with
  the initial FEN (shakmaty parses them as `Chess`), like before.
* Replay up to `--max-ply` (default 50, as before: positions at ply 0-49
  get entries); within a game, a repeated position counts once,
  with the move played last from it (old loop removal); key by
  `PositionKey`.
* Writes per chunk as in the [write path](#write-path-no-reads): one
  atomic `WriteBatch` with warm and cold operands, game records, `game_ids`
  and the status checkpoint, so an interrupted run resumes cleanly.
* After the run, schedule a rebuild of the player if a trigger fires.

## Closed accounts and marked players

Confirmed in the old code, and kept that way: neither is hidden or
deleted.

* Closed accounts: lila answers 404 for disabled users, the old indexer
  logs "did not find player" and indexes nothing new; existing data stays
  and is served.
* Marked players: the old `/player` handler passes an empty blacklist
  (`finalize_lichess_games(..., &HashSet::new())`), so games against or of
  marked players are shown. The new player server does not load the
  blacklist at all.

## Read path

Under a RocksDB snapshot: `status` (for `gen` and the cold bucket bits)
-> `warm` get + `cold` group get -> load the moves of the cold candidates
from `games` and replay them to validate (and to find the move played),
like the lichess server's cold lookup -> union of warm cells and validated
cold games -> filter by speeds/modes/months -> totals, per-move stats,
opponent rating average, performance, single-game ref, recent games ->
`multi_get` of `games` for the referenced records -> SAN, openings. On the
blocking pool, as in the lichess server. No response cache (data changes
while indexing).

### Streaming: notify instead of poll (decided)

Old: completion already wakes the stream immediately (`biased` select on
the ticket), but during a run every stream re-reads and re-renders its
position every second, whether anything changed or not, and drops
unchanged snapshots afterwards; `queuePosition` is also only refreshed by
that timer.

New, small change on top of the queue's existing `watch` channel: the
ticket's channel carries a `Progress { queue_position, generation, done }`
instead of `()`. The queue updates `queue_position` when it hands out
work; the indexer bumps `generation` after each committed batch. The stream
handler:

1. Sends the first snapshot immediately.
2. Waits for a change of the channel (or completion).
3. Debounces explicitly: at least 1 s since the last snapshot, except for
   the final one.
4. Re-reads only if `generation` changed since its last read; a change of
   only `queue_position` repeats the last snapshot with the new position,
   without a DB read.
5. Keeps the old dedup on `(queuePosition, total)` for compatibility, and
   ends after the final snapshot.

Same output timing as before at most (first immediately, then at most one
per second, final immediately), but no work while queued or between
batches, and it composes with requeued chunked runs (a run going back into
the queue is just another `queue_position` change). The read cost of
polling was small anyway (~65 open streams on average), so this is about
clean semantics rather than load; no experiment needed. Not done: waking
only streams whose position was touched by a batch (would need per-batch
key sets).

An empty keep-alive line after 8 s without a line, as before (the API
documents them), for proxies with idle timeouts while a player waits in
the queue.

## Mock lila (preparation step)

A stand-in for lila's games export, serving realistic game streams much
faster than 300 games/s, for tests, benchmarks, the scheduling
experiments and the compatibility comparison. Built before the indexer.

### What it has to answer

`GET /api/games/user/{id}?sort=dateAsc|dateDesc&ongoing=true&since=&until=`
with `Accept: application/x-ndjson`, one game per line in lila's export
JSON: `id`, `rated`, `variant`, `speed`, `perf`, `createdAt`,
`lastMoveAt`, `status`, `players.{white,black}.{user.{name,id,title},
rating}`, `winner`, `moves` (SAN, space separated), `initialFen`. 404 for
unknown players.

### Data preparation

Source: `lichess_db_rated.parquet`. It has id, players (name, title,
rating), date (year, month, day), outcome, speed, rated/unrated, setup
(variant, initial position) and moves, in dump order (chronological). It
lacks time of day, time control, termination, casual games and ongoing
games, which the mock synthesizes (below).

Player set: the 11,196 players of `requests.tsv` by default (lowercase
ids), optionally plus extra players (a random sample, or the heaviest
accounts) for experiments.

`mock_lila_prepare`, one pass over the parquet (all columns, similar to
`parquet_to_games`, ~20 min at 30 threads):

1. Keep rows where either player is in the set (hash set of lowercase
   names). Duplicate a game for both players if both are in the set.
2. Decode moves to SAN once (no CPU for that at serve time).
3. Group by player, keep the dump order, write the fixture into
   `/mnt/workspace_nvme/agent/mock_lila/`:
   * `games.bin`: per player, contiguous and chronological, length-prefixed
     records with the structured fields (base `createdAt`, duration,
     id, variant, speed, outcome, players, initial FEN, SAN string), so
     that the mock can still apply time shift, ongoing truncation and
     casual flags at serve time.
   * `index.bin`: sorted lowercase id -> (offset, count), and per player
     the `createdAt` of every k-th game for a binary search on `since` /
     `until`.

Size: not known yet; the first pass reports the number of games for the
player set. At ~300-400 B per game (SAN dominates), 100 M games would be
30-40 GB. If experiments need arbitrary players (for example all players
ever, for the launch), a second tier: a player -> `GameIndex` index for
all players (~2 x 8.25 B references, ~40-80 GB delta coded, external sort
via scratch) and games read from the game SST set at serve time instead
of a fixture. Only if needed.

### Synthesized fields

All deterministic (hash of game id and a seed), parameterized by flags:

* `createdAt`: day from the parquet, time of day spread over the day by
  the row's position within its day (dump order), so per player strictly
  increasing. `lastMoveAt`: `createdAt` plus a duration from speed and
  number of plies.
* Time shift: games are presented with `createdAt + delta`, so that the
  newest parquet month ends just before the start of `requests.tsv`,
  and the mock has a clock (real time, or replay time from the log
  replayer). Games with `createdAt` after the clock are not served yet;
  games with `createdAt <= now < lastMoveAt` are served as ongoing
  (`status: started`, moves so far). So a replay sees new games arrive,
  incremental runs and revisits of ongoing games, as in production.
* Casual games: in reality close to half of all games, but the dumps
  contain only rated games, so the parquet has about half of each player's
  real history. Default: mark 50% of the games `rated: false` (realistic
  mix, but half the real volume per player). Where volume matters
  (scheduling simulation, prod-like database), account for the missing
  half explicitly: run lengths x2 in the simulation, more players for the
  size target.
* `status` from the outcome and the final position: `mate`, `stalemate`,
  `variantEnd`, `draw`, otherwise `resign` or `outoftime` at random.
* Noise at small rates, to exercise the skip paths: aborted/noStart games,
  opponents without account or rating, illegal SAN at a random ply,
  players answering 404 (closed account).

### Serving

* A small axum binary (or example), memory-maps the fixture, serves
  `since`/`until` by binary search plus a short scan, `dateAsc` and
  `dateDesc`, formats JSON from the records.
* `--games-per-second` per stream: unlimited for benchmarks of our side,
  300 to emulate production for the scheduling experiments.
* Counts concurrent streams and fails loudly (and in the metrics) if
  there are ever more than `--max-streams`, which tests set to
  `--indexers`.
* Fault injection: time to first byte, stalls longer than our timeouts,
  disconnects mid-stream, 5xx, malformed lines.
* Logs every request (player, since, games served, duration) for the
  analysis of scheduling experiments.

## Testing and benchmarks

* Unit tests: entry and cold group pack/merge round trips, queue
  semantics, status transitions, loop removal.
* Arbtest of the storage against a naive aggregation, with random chunk
  boundaries, placement decisions and rebuild points (see
  [Testing](#testing) of the storage).
* All indexing tests and benchmarks run against the mock lila (below), never
  the real one.
* Compatibility: index the same players with the old server against the
  mock lila and compare final responses for the `/player` requests of
  `requests.tsv` (like `masters_compare`).
* Indexing benchmark: games/s per indexer and total, bytes per indexed
  game (against the 273 B measured offline, including drift between
  rebuilds), rebuild cost, write amplification, on NVMe and on NVMe (WAL, upper levels) + HDD
  (last level); lichess latency on the HDDs while indexing at the expected rate, with and
  without the rate limiter.
* `load_test`/`stress_explorer` support for ndjson streams, to replay the
  `/player` part of `requests.tsv` with its timing (bursts per player
  decide how often runs are triggered and how long the queue gets).

## Prod-like database for benchmarks

Scaling effects (cache misses, read amplification across levels, compaction
debt, bloom filter memory, rebuilds of large players next to everything
else) only show on a database of production size and shape: ~3 B indexed
(game, side) pairs, ~0.8-1 TB, and an LSM tree that has lived for a while,
not one level of non-overlapping SSTs. Indexing that through the live path
at 2400 games/s would take ~15 days. Instead, three steps:

1. **Population.** ~1.3 M players, as in the old production database,
   whose rated games add up to the target (~3-3.7 B pairs, adjustable to
   project growth), so ~2.4-2.9k games per player on average: the
   `requests.tsv` players (heavy, ~7.9k on average; needed so that the log
   can be replayed) plus a larger sample of lighter players. The real
   distribution is unknown; use the mean as the constraint, for example
   sampling lichess players with at least some games. Re-run
   `player_layout_stats` on this population to correct the 273 B per
   game.
   Casual flags, `createdAt` etc. as synthesized by the mock lila (same
   deterministic rules), so the database and the mock agree.
2. **Base: bulk build up to a cutoff per player.** One parquet pass that
   scatters the selected players' games into scratch partitions by `uh`
   range (the `scatter` machinery of `parquet_to_positions`), then per
   partition the rebuild code (replay, optimal placement) writing sorted,
   non-overlapping SSTs, ingested into the bottom level. Per player, only
   games before a cutoff: the last x% of the games or the last months, with
   the cutoff varying by player (some players recently indexed, some long
   ago, some not at all, for first runs). Scale: ~140 B visits, roughly a
   third of a `parquet_to_positions` run, so about an hour.
3. **Aging: the tail through the real write path.** Games after the cutoffs
   go through the live indexing code (chunks, warm/cold operands, `games`
   records, status, rebuild triggers including SST-ingest rebuilds and
   `DeleteRange` of old generations), fed directly from local data instead
   of the HTTP mock and without the 300 games/s limit (~50-100k games/s on
   many threads). Runs interleaved in time order across players, like
   real arrivals, so that memtables, L0 and upper levels hold mixed
   players, operand stacks and drift. To fill the upper levels of a
   leveled LSM (each ~1/10 of the next) about 10% of the database has to
   pass through: ~300 M games, roughly 1-2 h plus compaction. At the
   production rate of ~28 pairs/s this corresponds to ~4 months of
   indexing.
   * Faster variant for most of the tail: ingest it as SSTs of merge
     operands (`SstFileWriter::merge`), per time slice, which land in upper
     levels on top of the base, and let compaction work them down; then
     only the last part through the live path, for memtable and L0
     realism.

Then:

* Keep the aged state as a RocksDB checkpoint (hard links, cheap) and start
  every benchmark from a fresh checkpoint copy: SSTs are immutable, so the
  original stays intact. The mock lila serves the games after each
  player's last indexed game, so replaying `requests.tsv` gives realistic
  incremental runs, revisits and first runs.
* For the HDD workspace: copy once with the production placement (WAL and
  upper levels on NVMe via `cf_paths`, bottom level on the HDDs), then
  checkpoint there.
* Sanity checks on the shape: files and bytes per level, merge operands
  per key (table properties), stall/compaction stats, block reads per
  request. If the old production server's `rocksdb.stats` of the `player`
  CF is available, compare the level shape with it.

### Built (2026-11-05)

`player_bulk` (`src/player/bulk/`, `src/bin/player_bulk.rs`), all in
`/mnt/workspace_nvme/agent/scratch/bulk/`:

1. `count` (6 min): 23.7 M players, 16.7 G (game, side) pairs in the
   parquet. `select`: the 9,676 fixture players with games (76 M pairs)
   plus 1.27 M players sampled with weight `games^0.268`, so that their
   mean is 2,500 games (p50 176, p90 6.3k, p99 36k, max 608k): 1.28 M
   players, 3.25 G pairs.
2. `scatter` (12 min, 294 GB): the records the live indexer would write,
   built directly from the parquet for standard games (`via export`, i.e.
   the mock's `present` + `classify`, for the 2% variant and noisy games).
   `--verify` on a slice: 14.9 M standard games identical on both paths.
   The mock's rules (shift, durations, casual, noise) are shared in
   `mock_lila::synth`.
3. `build` (57 min, 30 GB RSS): plans per player, aging window
   2026-08-01 .. 2026-11-03 (database time):
   * requests players: 20% unindexed (first runs in the replay), 20%
     dormant, 10% new, 50% active; sampled: 60% dormant, 30% active, 10%
     new;
   * dormant: last indexed at the window start minus an exponential delay
     (mean 180 days); active: indexed to the window start, then visits
     every 1-60 days (log-uniform); new: first run at a random time in the
     window, then visits.
   * Result: 748 k dormant, 402 k active, 128 k new, 1.9 k unindexed;
     2.68 G base games (748 GB of SSTs, 279 B/game), 354 M tail games in
     1.24 M visits; 188 M games after dormant cutoffs stay unindexed.
4. `ingest` (11 s): everything in L6, 714 GB. Checkpoint `ckpt_base`.
5. `age` (44 min, 16 workers, 8 background jobs): 352 M games through
   `write_chunk`, 150 k first runs, 39 k rebuilds (4 by ingest), 140-280 k
   games/s; write stalls 36% of the time (compaction behind at ~5000x the
   production rate). Flushed 132 GB, compaction writes 1,275 GB (write
   amplification ~9.7; old production: 91). Checkpoint `ckpt_aged`.

Shape after aging (MB per level):

| CF | L0 | L2-L4 | L5 | L6 |
| --- | --- | --- | --- | --- |
| games | 125 | 1,837 | 16,280 | 163,479 |
| game_ids | 0 | 477 | 3,095 | 31,100 |
| warm | 62 | 2,331 | 21,670 | 218,197 |
| cold | 0 | 3,634 | 32,362 | 325,044 |

801 GB for 3.03 G indexed games: **264 B per game** (cold 116, warm 78,
games 58, ids 11). L5 is ~10% of L6 in every column family, as for a
long-lived leveled LSM.

`check` on 300 fixture players (233 indexed): their 1.56 M games are
identical (records and order) to what the indexer gets from the mock up to
their `latest createdAt`, and all 53.2 M of their positions read the same
entries as a naive aggregation.

Smoke benchmark from a hard-linked copy of `ckpt_aged` (`--cache 16g`, NVMe
only), the first 2 h of `requests.tsv` at 10x with the mock at 300
games/s per stream, as the replay on an initially empty database before:

| | empty database | aged database |
| --- | --- | --- |
| complete / aborted by the next request / gave up | 21,794 / 5,692 / 2 | 21,140 / 6,326 / 22 |
| p99 to the final snapshot | 20 s | 43 s |
| requests that waited for indexing (p50 / p90 / p99) | 314 (20 / 90 / 308 s) | 548 (20 / 143 / 491 s) |
| lila slot use | 46% | 78% (1,226 runs, 1.36 M games) |

The aged database makes the indexer busier: dormant players come back
with months of games, and 20% of the requests players need first runs.
How realistic that is depends on the plan shares, which are guesses.
Position reads: mean 0.7 ms, 92% within 1 ms, max 19 ms; block cache hit
rate 52% from a cold 16 GB cache. No errors, no stream limit violations.

## Code conventions

Consistent with the existing code: strong types, and binary formats
encapsulated in types that own their packing, rather than raw byte
slices passed around. References to follow:

* Newtypes for identifiers and keys, with checked constructors:
  `GameIndex` (`src/packed/game_index.rs:11`, `new` returns `None` beyond
  the bit budget), `GameId` (`src/packed/game_id.rs:8`, `from_ascii` with
  `InvalidGameId`), `PositionKey` (`src/packed/position_key.rs:16`,
  `from_zobrist` mixes the variant in, `as_bytes` for keys), `Month`
  (`src/packed/month.rs:9`). For the player explorer: a `UserHash`, a
  per-player `GameSeq`, a `Generation`, and key types such as
  `WarmKey`/`ColdGroupKey`/`GameRecordKey` that build and parse the RocksDB
  keys, instead of ad-hoc `Vec<u8>` concatenation.
* Binary formats with the grammar in the doc comment and a builder plus a
  borrowed view: `ColdKey`/`ColdGroup` (`src/packed/cold.rs:8`, `:79`;
  the `value := uint(count) index* gap*` grammar), `LichessEntry`
  (`src/packed/entry.rs:283`, invariants documented) with the in-place
  readers `EntryView`/`GroupView`/`GroupCells` (`:456`, `:486`, `:579`),
  `PackedMoves` with `PackedMovesEncoder`/`PackedMovesDecoder`
  (`src/packed/moves.rs:10`, `:38`, `:78`), the fixed-size
  `IntermediateEntry` (`src/intermediate/entry.rs:12`). The warm entry
  should reuse `LichessEntry` machinery (header type generic or a
  `PlayerGroupHeader` next to `GroupHeader`, `src/packed/entry.rs:18`),
  the cold groups `packed/cold.rs` with a per-player bit width.
* Owned and borrowed pairs for records: `LichessGame` /
  `BorrowedLichessGame` (`src/packed/game.rs:342`, `:448`); the `games`
  record of the player explorer the same way.
* Enums and bitflags instead of loose integers: `Speed` and `Speeds`
  (`src/packed/game.rs:132`, `:221`), `RatingMode`, `RatingGroup`
  (`src/packed/rating.rs:100`), with explicit `bits()`/`from_bits()`.
* Errors as `thiserror` types per module: `SstError` (`src/sst.rs:10`),
  `PlayError` (`src/server/query.rs:58`), `InvalidMonth` and friends.
* Configuration structs instead of long argument lists: `SstConfig`
  (`src/sst.rs:58`), `LichessFilter`/`Limits` (`src/server/query.rs:137`,
  `:177`).
* Property tests with `arbtest` and `Arbitrary` impls producing realistic
  values (`LichessEntry`'s `Arbitrary` with chronological game indexes,
  `src/packed/entry.rs`), and naive oracles (`aggregate_naive`,
  `src/server/explore.rs:605`). The storage arbtest above follows this.

Not a reference: `examples/player_layout_stats.rs` is a throwaway
measurement with ad-hoc byte formats.

## Milestones

0. Mock lila: `mock_lila_prepare` fixture for the `requests.tsv` players,
   serving binary with rate limit, stream counting, clock and fault
   injection.
1. Storage: keys, `games` records, warm entry and cold group formats with
   merge operators, status with generations, union read, rebuild (batches
   and SST ingest); arbtest.
2. Indexer against the mock lila: queue, runs, chunked batches, rebuild
   triggers; indexing benchmark and size per game; queue scheduling
   simulation.
3. `explorer player` server: query, streaming, read path, metrics;
   compatibility comparison with the old server.
4. Prod-like database: population, bulk base, aging, checkpoints (above).
5. Production layout: `wal_dir`/`cf_paths`, rate limiter, interference
   benchmark with lichess on the HDDs, on the prod-like database.

## Status (2026-11-05)

Branch `personal-explorer`:

* Milestone 0 (mock lila): done. Fixture of the `requests.tsv` players in
  `/mnt/workspace_nvme/agent/mock_lila/` (76.2 M games, 26.6 GB, 5 min).
* Milestone 1 (storage): done, with the property test. Rebuilds by batches
  or SST ingest (above 64 MB).
* Milestone 2 (indexer): done, tested against the mock with faults.
  Scheduling simulated and chosen (above).
* Milestone 3 (server): done. Compatibility: final responses of 9,670
  `requests.tsv` requests of 250 players, indexed by the old server and the
  new one from the same mock, are identical after normalizing titles (new),
  name case and the order of moves with equal counts.
* Measured: 291 B per indexed game after compaction (warm 94, cold 124,
  records with moves 62, game ids 12; the old server's database of the same
  250 players is 6.5x larger); the writer does ~70k games/s per indexer,
  far above lila's 300 games/s; a rebuild of a player with 1.7k games takes
  14 ms.
* Milestone 4 (prod-like database): done (see [Built](#built-2026-11-05)):
  1.28 M players, 3.03 G indexed games, 801 GB, aged LSM shape, checked
  against the mock and a naive aggregation; checkpoints `ckpt_base` and
  `ckpt_aged` in `/mnt/workspace_nvme/agent/scratch/bulk/` (about 2.5 h
  end to end).
* Milestone 5: `--wal-dir`, `--db-path PATH:SIZE` (upper levels on NVMe,
  last level on HDD) and `--rate-limit` exist; the interference benchmark
  on the HDDs is not done.
* Not decided, not implemented: `DELETE /player/{id}`.

## Decided

* Separate process.
* Data source: lila only.
* Casual games: close to 50% of all games.
* Prototype: incremental commits on a feature branch; order mock lila,
  storage, indexer; mock lila as a binary in `src/bin/`.
* No migration of the old DB: start empty, players reindex on demand.
* Closed accounts and marked players: not hidden, as before.
* Lila streams: at most `--indexers` (default 8) at once.
* Rate limits: only in the reverse proxy.
* Streaming: notify with explicit 1 s debounce instead of polling.
* Max ply: `--max-ply`, default 50 (as before, same as the lichess index
  and the lila analysis board). Expected not to change on an existing DB;
  not checked.
* Cooldown: 2 min.
* lila export rate: assume 300 games/s per stream.
* Storage: warm/cold split (grouped cold) with disjoint-union reads, a
  per-player game log with moves as the source of truth, generational
  per-player rebuilds.

## Open questions

* Revisit interval for ongoing games: stays 24 h? (simulation)
* Cold threshold T (1, or 2-3 games per position in cold) and rebuild
  trigger parameters: to tune; correctness does not depend on them.
