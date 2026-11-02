# lila-openingexplorer v4

Next generation opening explorer for lichess.org, indexing billions of
chess games (currently ~10 B rated games across all variants, growing fast).

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
* [ ] Determine best disk layout and migration strategy
* [ ] Document recommended options for each workload
* [ ] Consider --lru-cache utility
* [ ] Make cold optional

Minor things:

* [ ] Performance rating
* [ ] Banned player blacklist
* [ ] Unify masters/lichess API

Pending documentation changes:

* [ ] title on GamePlayer

# License

* [ ] TODO
