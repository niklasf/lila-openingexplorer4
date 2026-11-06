# lila-openingexplorer v4

Next generation opening explorer for lichess.org, indexing billions of
chess games (currently ~10 billion rated games across all variants,
and growing).

## Usage

Production: 128 GB RAM, 12 hardware threads, SST files on hard disks,
two instances running intermittently.

Create the database directories from the SST files. The SST files are
hard-linked, so they must be on the same file system as the database:

```sh
explorer ingest --db /hdd/lichess/db --games /hdd/lichess/sst/games --positions /hdd/lichess/sst
explorer ingest --db /hdd/masters/db --games /hdd/masters/sst/games --positions /hdd/masters/sst --game-ids /hdd/masters/sst/game_ids
```

Serve them:

```sh
explorer lichess --db /hdd/lichess/db --bind 127.0.0.1:9002 --cors \
    --cache 32g \
    --blocking-threads 16 \
    --response-cache 300000 \
    --lila-bearer-file /etc/explorer-bearer

explorer masters --db /hdd/masters/db --bind 127.0.0.1:9003 --cors \
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
* `--lila-bearer-file`: Token of https://lichess.org/@/OpeningExplorer
  (or `EXPLORER_BEARER_FILE`), for the blacklist of players marked by
  moderators every `--blacklist-refresh` seconds, default 173 min.
  `--no-blacklist` to disable.
* `--max-ply` must match the indexing runs (of `parquet_to_positions` and
  `parquet_to_games`): 50 (the default) for lichess, 0 (all plies) for
  masters.
* `--admin-bind`: Provide `/monitor`.

The personal opening explorer (`/player`) has its own writable database,
created on first start, and indexes games from lila on demand:

```sh
explorer player --db /nvme/player/db --bind 127.0.0.1:9004 --cors \
    --db-path /nvme/player/sst:64g --db-path /hdd/player/sst:6t \
    --cache 8g \
    --lila-bearer-file /etc/explorer-bearer
```

* `--db-path`: The write ahead log (in `--db`), then multiple paths and
  their capacities (to put the upper levels on fast storage, if possible).
* `--rate-limit 16m` (default): Bounds write throughput per second of
  compactions.
* `--admin-bind`: Provide `/monitor` and `DELETE /player/{id}`.
* `--indexers 8` (default): Parallel index runs, which is also the most
  parallel streams from lila's games export. Uses `--lila-bearer-file`
  if specified.
* `--max-ply 50` (default). Must not change on an existing database.

Replay an nginx request log (tab separated `$msec`, `$status`,
`$request_time`, `$upstream_response_time`, `$request_uri`) against both:

```sh
stress_explorer samples/requests/explorer-2026-11-06T1520Z-1h.tsv.zst \
    --lichess-url http://127.0.0.1:9002 \
    --masters-url http://127.0.0.1:9003
```

## Indexing

1. Convert PGN files to a more compact parquet file. Inputs must be in
   chronological order by month.

   ```sh
   pgns=$(ls samples/lichess_db/*/lichess_db_*_rated_*.pgn.zst | awk -F_ '{print $NF, $0}' | sort -s -k1,1 | cut -d' ' -f2)
   pgn_to_parquet --out lichess/lichess_db_rated.parquet $pgns
   ```

2. Walk the parquet files to produce SSTs indexed by position. Benefits from
   large memory and disk scratch space.

   ```sh
   parquet_to_positions lichess/lichess_db_rated.parquet --out lichess/sst --scratch scratch --scratch-budget 3400g --memory-budget 100g
   ```

3. Walk the parquet files to produce SSTs indexed by game id.

   ```sh
   parquet_to_games lichess/lichess_db_rated.parquet --out lichess/sst/games --max-ply 50
   ```

   `--max-ply 50` stores the moves only up to the indexed ply. Masters must keep
   all moves (the default, 0), for `/masters/pgn`.

## Todo

### Indexing from scratch

* [x] `pgn_to_parquet`: Convert `.pgn` or `.pgn.zst` files to a single
  `.parquet` file optimized for the following stages.
* [x] `parquet_to_games`: Produce RocksDB SST files for games.
* [x] `parquet_to_positions`: Produce RocksDB SST files for positions.

### Live server

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
* [x] Truncate Lichess game moves
* [ ] Tune warm/cold threshold for player database

Needs HDDs:

* [ ] Optimize blocking thread count
* [ ] Recheck parallel game loading
* [ ] Consider loading and pinning meta data on startup

Minor things:

* [x] Performance rating
* [x] Banned player blacklist
* [ ] Unify masters/lichess API
* [ ] RocksDB PerfStatsLevel mismatch (patch prepared)

Pending documentation changes:

* [ ] title on GamePlayer

## Production hardware

* AMD Ryzen 5 PRO 3600 (12 threads @ 3.6 GHz)
* 128 GB RAM
* 4x HGST HUS726T6TAL (5.46 TB) in RAID 10
* 2x WDC CL SN720 SDAQNTW-512G-2000 (512 GB) in RAID 1

## License

Licensed under the GNU Affero General Public License v3. See the `LICENSE` file
for details.
