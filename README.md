# lila-openingexplorer v4

Next generation opening explorer for lichess.org, indexing billions of
chess games (currently ~10 B rated games across all variants, growing fast).

# Usage

Production: 128 GB RAM, 12 hardware threads, SST files on hard disks,
two instances running intermittently.

Create the database directories from the SST files. The SST files are
hard-linked, so they must be on the same file system as the database:

```sh
explorer ingest --db /hdd/db_lichess --games /hdd/sst --positions /hdd/sst_positions
explorer ingest --db /hdd/db_masters --games /hdd/masters_sst/games --positions /hdd/masters_sst --game-ids /hdd/masters_sst/game_ids
```

Serve them:

```sh
explorer lichess --db /hdd/db_lichess --bind 127.0.0.1:9002 --cors \
    --cache 32g \
    --blocking-threads 16 \
    --response-cache 300000

explorer masters --db /hdd/db_masters --bind 127.0.0.1:9003 --cors \
    --max-ply 0 \
    --cache 4g \
    --blocking-threads 12
```

* `--cache`: Should fit index and filter blocks (10.9 GB as of 2026-11,
  growing with the number of games). Adding more is good, but keep a
  significant amount of RAM for the page cache to avoid cold starts.
* No `--direct-reads`: Avoid when cold starts matter. Otherwise good combined
  with assigning all RAM to `--cache`.
* `--blocking-threads 16`: To be measured. On hard disks, it might be good
  to oversubscribe.
* `--response-cache 100000`: 28% hit rate in 2026-11. More gains little.
* `--max-ply` must match the indexing run: 50 (the default) for lichess,
  0 (all plies) for masters.

Replay a request log against both:

```sh
stress_explorer sample-log.txt.zst \
    --lichess-url http://127.0.0.1:9002 \
    --masters-url http://127.0.0.1:9003
```

# Indexing

1. Convert PGN files to a more compact parquet file. Inputs must be in
   chronological order by month.

   ```sh
   pgns=$(ls /mnt/lichess_db/*/*.pgn.zst | awk -F_ '{print $NF, $0}' | sort -s -k1,1 | cut -d' ' -f2)
   pgn_to_parquet --out lichess_db_rated.parquet $pgns
   ```

2. Walk the parquet files to produce SSTs indexed by position. Benefits from
   large memory and disk scratch space.

   ```sh
   parquet_to_positions lichess_db_rated.parquet --out sst_positions --scratch scratch --scratch-budget 3400g --memory-budget 100g
   ```

3. Walk the parquet files to produce SSTs indexed by game id.

   ```sh
   parquet_to_games lichess_db_rated.parquet --out sst
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
* [x] Determine best disk layout and migration strategy
* [x] Document recommended options for each workload
* [x] Investigate why LRU beats CLOCK
* [x] Investigate cache admission policy
* [x] Make cold optional
* [x] Partial sorting
* [x] Recheck compression on latest SSTs
* [x] Elias-Fano vs rice on cold entries
* [x] Optimize SST block sizes

Needs HDDs:

* [ ] Optimize blocking thread count
* [ ] Recheck parallel game loading

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
