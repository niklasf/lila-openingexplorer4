#!/bin/bash
# Indexing pipeline and servers on the sample data (also run by CI).
set -euo pipefail
B=${BIN:-target/release}
W=${WORK:-target/pipeline-test}
rm -rf "$W"; mkdir -p "$W/lichess/sst" "$W/masters/sst"

# lichess
pgns=$(ls samples/lichess_db/*/lichess_db_*_rated_*.pgn.zst | awk -F_ '{print $NF, $0}' | sort -s -k1,1 | cut -d' ' -f2)
$B/pgn_to_parquet --threads 4 --memory-budget 1g --out $W/lichess.parquet $pgns
$B/parquet_to_positions --threads 4 --no-direct-io --scratch $W/scratch --scratch-budget 4g --memory-budget 6g --out $W/lichess/sst $W/lichess.parquet
$B/parquet_to_games --threads 4 --no-direct-io --out $W/lichess/sst/games $W/lichess.parquet
$B/explorer ingest --db $W/lichess/db --sst $W/lichess/sst

# masters
echo deterministic,legacy > $W/legacy_ids.csv
$B/masters_to_parquet samples/masters-db/games --out $W/masters.parquet --legacy-ids $W/legacy_ids.csv
$B/parquet_to_positions --threads 4 --no-direct-io --scratch $W/scratch --scratch-budget 4g --memory-budget 6g --out $W/masters/sst $W/masters.parquet
$B/parquet_to_games --threads 4 --no-direct-io --out $W/masters/sst/games --game-ids $W/masters/sst/game_ids $W/masters.parquet
$B/explorer ingest --db $W/masters/db --sst $W/masters/sst

# serve and replay the sample requests
$B/explorer lichess --db $W/lichess/db --bind 127.0.0.1:9002 --cache 256m --no-openings --no-blacklist > $W/lichess.log 2>&1 &
lichess=$!
$B/explorer masters --db $W/masters/db --bind 127.0.0.1:9003 --cache 64m --no-openings > $W/masters.log 2>&1 &
masters=$!
trap 'kill $lichess $masters 2>/dev/null || true' EXIT
for port in 9002 9003; do
    for _ in $(seq 100); do
        curl -s -o /dev/null "http://127.0.0.1:$port/" && break
        sleep 0.1
    done
done

# Responses of selected requests must equal the recorded ones. To record
# them anew (after an intended change): UPDATE_GOLDEN=1 samples/test-pipeline.sh
for uri in $(grep -v '^#' samples/golden/requests.txt); do
    case $uri in
        /lichess*) port=9002 ;;
        *) port=9003 ;;
    esac
    printf 'GET %s\n' "$uri"
    curl -s -w '\n%{http_code}\n\n' "http://127.0.0.1:$port$uri"
done > $W/responses.txt
if [[ -n ${UPDATE_GOLDEN:-} ]]; then
    cp $W/responses.txt samples/golden/responses.txt
elif ! diff -u samples/golden/responses.txt $W/responses.txt; then
    echo "responses differ from samples/golden/responses.txt"; exit 1
fi

# Fails on invalid responses, server errors and failed requests.
$B/stress_explorer samples/requests/*.tsv.zst \
    --lichess-url http://127.0.0.1:9002 --masters-url http://127.0.0.1:9003 \
    --concurrency 8 --limit 20000 --timeout 30
if grep -iq panic $W/lichess.log $W/masters.log; then
    echo "server panicked"; exit 1
fi
echo "pipeline ok"
