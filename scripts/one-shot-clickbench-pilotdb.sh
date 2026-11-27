#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Compare PilotDB on DuckDB with exact DuckDB and exact ClickHouse on ClickBench's
hits table: builds the workload from a ClickBench checkout, then runs it with
scripts/one-shot-pilotdb-duckdb-clickhouse.sh (see its --help for every option).

  scripts/one-shot-clickbench-pilotdb.sh <clickbench-checkout> <data-directory> <run-directory>

  clickbench-checkout  a clone of https://github.com/ClickHouse/ClickBench; the workload
                       takes duckdb/ and clickhouse/ create.sql and queries.sql from it
  data-directory       holds hits.parquet, from
                       https://datasets.clickhouse.com/hits_compatible/athena/hits.parquet;
                       ClickHouse loads it with file(), so the server's user_files_path
                       must contain it
  run-directory        created if missing

The workload is clickbench-hits: the 43 queries, ids 1 to 43, in both dialects. PilotDB runs
APPROXIMATE_QUERIES [1 2 3 4 8 21 30, the ones PilotDB's paper runs]. Every other option is an
environment variable of scripts/one-shot-pilotdb-duckdb-clickhouse.sh.

Example, with a ClickHouse server on CPUs 0-15 and user_files_path /data/clickbench:
  ENGINE_CPUS=0-15 THREADS=16 PILOTDB_PYTHON=~/pilotdb/.venv/bin/python \
    scripts/one-shot-clickbench-pilotdb.sh ~/ClickBench /data/clickbench runs/clickbench
EOF
}

if [ $# -ge 1 ] && { [ "$1" = -h ] || [ "$1" = --help ]; }; then
  usage
  exit 0
fi
if [ $# -ne 3 ]; then
  usage >&2
  exit 2
fi
clickbench=$1; data=$2; run=$3
command -v python3 >/dev/null || { echo "needs python3" >&2; exit 1; }
for file in duckdb/create.sql duckdb/queries.sql clickhouse/create.sql clickhouse/queries.sql; do
  [ -f "$clickbench/$file" ] || { echo "no $file in $clickbench" >&2; exit 1; }
done
[ -f "$data/hits.parquet" ] || { echo "no hits.parquet in $data" >&2; exit 1; }
mkdir -p "$run"
workload=$(mktemp)
trap 'rm -f "$workload"' EXIT

python3 - "$clickbench" "$workload" <<'PY'
import json, sys

clickbench, path = sys.argv[1:]


def read(name):
    with open(f"{clickbench}/{name}") as file:
        return file.read()


def statement(text):
    return text[: text.rindex(";")].strip()


duckdb_queries = [line.strip() for line in read("duckdb/queries.sql").splitlines() if line.strip()]
clickhouse_queries = [line.strip() for line in read("clickhouse/queries.sql").splitlines() if line.strip()]
if len(duckdb_queries) != len(clickhouse_queries):
    sys.exit("duckdb/queries.sql and clickhouse/queries.sql differ in length")
workload = {
    "name": "clickbench-hits",
    "queries": [],
    "data": [],
    "tables": [{
        "name": "hits",
        "file_name": "hits.parquet",
        "url": "https://datasets.clickhouse.com/hits_compatible/athena/hits.parquet",
        "rows": 99997497,
        "load_statements": [
            {"dialect": "DuckDb", "statements": [
                statement(read("duckdb/create.sql")),
                "INSERT INTO hits SELECT * REPLACE (make_date(EventDate) AS EventDate, "
                "epoch_ms(EventTime * 1000) AS EventTime, "
                "epoch_ms(ClientEventTime * 1000) AS ClientEventTime, "
                "epoch_ms(LocalEventTime * 1000) AS LocalEventTime) "
                "FROM read_parquet('{file}', binary_as_string=True)",
            ]},
            {"dialect": "ClickHouse", "statements": [
                statement(read("clickhouse/create.sql")),
                "INSERT INTO hits SELECT * FROM file('{file}', Parquet)",
            ]},
        ],
    }],
    "one_shot_queries": [
        {
            "id": str(number),
            "texts": [
                {"language": "Sql", "text": duckdb_query, "dialect": "DuckDb"},
                {"language": "Sql", "text": clickhouse_query, "dialect": "ClickHouse"},
            ],
        }
        for number, (duckdb_query, clickhouse_query) in enumerate(zip(duckdb_queries, clickhouse_queries), 1)
    ],
}
with open(path, "w") as file:
    json.dump(workload, file, indent=1)
PY

DUCKDB_DATABASE=${DUCKDB_DATABASE:-$run/hits.duckdb} \
  APPROXIMATE_QUERIES=${APPROXIMATE_QUERIES:-1 2 3 4 8 21 30} \
  "$(dirname "$0")/one-shot-pilotdb-duckdb-clickhouse.sh" "$workload" "$data" "$run"
