# lila-openingexplorer v4

Next generation opening explorer for lichess.org, indexing billions of
chess games (currently ~10 B rated games across all variants, growing fast).

## Indexing from scratch

* [x] `pgn_to_parquet`: Convert `.pgn` or `.pgn.zst` files to a single
  `.parquet` file optimized for the following stages.
* [x] `parquet_to_games`: Produce RocksDB SST files for games.
* [x] `parquet_to_positions`: Produce RocksDB SST files for positions.

## Serving

* [x] `explorer`: Read-only HTTP server with the API of the current opening
  explorer (`/lichess`, `/masters`, `/masters/pgn/{id}`, `/monitor`), one
  process per database.

```
# Once: link the SST files into a RocksDB directory (same file system,
# no copies, ~40 s for the full lichess set).
explorer ingest --db db_lichess --games sst --positions sst_positions
explorer ingest --db db_masters --games masters_sst/games --positions masters_sst

# Serve (opens the databases strictly read-only).
explorer lichess --db db_lichess --cache 32g --bind 0.0.0.0:9002
explorer masters --db db_masters --max-ply 0 --cache 2g --bind 0.0.0.0:9003
```

`--max-ply` must match the `parquet_to_positions` run (50 for lichess, 0 for
masters, which indexes all plies). Tuning on a 186 GB machine with the SST
sets on NVMe: lichess `--cache 32g` plus the page cache (buffered reads;
`--direct-reads` performs the same but loses the page cache as a second
level); masters needs no more than 2 GB of block cache (the database is
3.3 GB). `--blocking-threads` defaults to half the available parallelism:
more threads add no throughput but lengthen tail latencies. `--warm-up
sample-log.txt.zst` replays a request log before listening (89k positions in
4 s), so that the block cache holds the hot entries from the start.

On hard disks every lookup that misses the block cache costs a seek, so
the aim is to need as few seeks per request as possible and to issue them
concurrently: `--pin-metadata` loads all index and filter partitions at
startup and keeps them in the block cache (the lichess set has about 27 GB
of them, so give it `--cache 64g`), which leaves at most one data block
read per position or game lookup; `--parallel-fetch` reads the games of a
response that are not cached in parallel instead of one after the other;
`--blocking-threads 64` oversubscribes the cores so that the disks always
have a queue to reorder; `--direct-reads` keeps the page cache from
competing with the block cache for memory. Both `--pin-metadata` and
`--parallel-fetch` are off by default because they cost a few percent on
NVMe.

Serving reads entries in place, without decoding them: groups (move, speed
and rating group) that the query does not select are skipped by their byte
length, the subtotal of a group stands in for its months when the query has
no month range, and months and games are stored newest first, so that the
scan of a group stops as soon as no more games can make it into the response.
On the sample log this takes aggregation of the busiest entries (4-12 MB)
from 3 ms to 0.15 ms per request, which made an in-memory cache of decoded
entries unnecessary. `examples/entry_decode_bench.rs` times the stages of
serving a large entry, `examples/log_entry_sizes.rs` prints the entry size
distribution of logged requests, and `examples/load_test.rs` replays a
request log against a running server and reports throughput, latency
percentiles and response validity.

Semantics that differ from the current explorer: rating groups use the
rating of the side to move instead of the average of both players (as
indexed), masters top games are the highest rated among the most recent
games of the top rating groups (entries keep only the 8 most recent games
per move, rating group and month), banned players are not filtered, and
games without a rating report `"rating": null`.
