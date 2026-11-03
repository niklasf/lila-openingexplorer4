# lila-openingexplorer v4

Next generation opening explorer for lichess.org, indexing billions of
chess games (currently ~10 B rated games across all variants, growing fast).

# Example usage

Create the database directories from the SST files. The SST files are
hard-linked, so they must be on the same file system as the database:

```sh
explorer ingest --db /hdd/db_lichess --games /hdd/sst --positions /hdd/sst_positions
explorer ingest --db /hdd/db_masters --games /hdd/masters_sst/games --positions /hdd/masters_sst
```

Serve them:

```sh
explorer lichess --db /hdd/db_lichess --bind 127.0.0.1:9002 \
    --cache 40g \ # ~30% of RAM, keeping a significant amount for page cache
    --blocking-threads 16 \ # oversubscribed when serving from hdd
    --response-cache 300000

explorer masters --db /hdd/db_masters --bind 127.0.0.1:9003 \
    --cache 6g \
    --blocking-threads 12
```

Replay a request log against both:

```sh
stress_explorer sample-log.txt.zst \
    --lichess-url http://127.0.0.1:9002 \
    --masters-url http://127.0.0.1:9003
```

# Todo

## Indexing from scratch

* [x] `pgn_to_parquet`: Convert `.pgn` or `.pgn.zst` files to a single
  `.parquet` file optimized for the following stages.
* [x] `parquet_to_games`: Produce RocksDB SST files for games.
* [x] `parquet_to_positions`: Produce RocksDB SST files for positions.

## Live server

Overall features:

* [x] Lichess games
* [x] Master games
* [ ] Broadcast games
* [ ] Lichess games indexed by player (aka personal opening explorer)

Critical performance work:

* [x] Stress testing tool
* [ ] Recheck parallel game loading
* [x] Determine best disk layout and migration strategy
* [ ] Document recommended options for each workload
* [ ] Investigate why LRU beats CLOCK
* [ ] fill_cache sampling heuristic
* [ ] cold-path entry synthesis
* [x] Make cold optional
* [ ] Partial sorting
* [ ] Recheck compression on latest SSTs
* [ ] Elias-Fano vs rice on cold entries
* [ ] Remove sampling cache admission
* [ ] 16k blocks for parquet_to_games
* [ ] Optimize blocking thread count

Minor things:

* [ ] Performance rating
* [ ] Banned player blacklist
* [ ] Unify masters/lichess API

Pending documentation changes:

* [ ] title on GamePlayer

# Production hardware

* AMD Ryzen 5 PRO 3600 (12 threads @ 3.6 GHz)
* 128 GB RAM
* 4x HGST HUS726T6TAL (5.46 TB) in RAID 10
* 2x WDC CL SN720 SDAQNTW-512G-2000 (512 GB) in RAID 1

# License

* [ ] TODO
