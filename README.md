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
explorer masters --db db_masters --max-ply 0 --cache 2g \
    --decode-threshold 64k --decoded-cache 1g --bind 0.0.0.0:9003
```

`--max-ply` must match the `parquet_to_positions` run (50 for lichess, 0 for
masters, which indexes all plies). Tuning on a 186 GB machine with the SST
sets on NVMe: lichess `--cache 32g` plus the page cache (buffered reads;
`--direct-reads` performs the same but loses the page cache as a second
level), `--decoded-cache 8g` with the default `--decode-threshold 4m` (the
568 positions with entries of 4 MB and more receive 28% of the requests in
`sample-log.txt.zst`, ~300 each); masters needs no more than 2 GB of block
cache (the database is 3.3 GB) and `--decode-threshold 64k`. Replaying the
sample log with `examples/load_test.rs` at 32 concurrent clients: lichess
30k requests/s (mean 1.0 ms, p99 5.6 ms) on first contact, masters 100k
requests/s (p99 1.1 ms). `examples/log_entry_sizes.rs` prints the entry
size distribution of logged requests, to size the decoded cache.

Semantics that differ from the current explorer: rating groups use the
rating of the side to move instead of the average of both players (as
indexed), masters top games are the highest rated among the most recent
games of the top rating groups (entries keep only the 8 most recent games
per move, rating group and month), banned players are not filtered, and
games without a rating report `"rating": null`.
