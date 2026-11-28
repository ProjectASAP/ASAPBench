#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Run ClickBench's hits table on DuckDB, ClickHouse, TimescaleDB, Druid and PilotDB on DuckDB:
builds the workload from a ClickBench checkout, then runs it with scripts/one-shot-workload.sh
(see its --help for every option).

  scripts/one-shot-clickbench.sh <clickbench-checkout> <data-directory> <run-directory>

  clickbench-checkout  a clone of https://github.com/ClickHouse/ClickBench; the workload
                       takes create.sql and queries.sql of duckdb/, clickhouse/ and
                       timescaledb/, and queries.sql of druid/
  data-directory       holds hits.parquet, from
                       https://datasets.clickhouse.com/hits_compatible/athena/hits.parquet;
                       ClickHouse loads it with file(), so the server's user_files_path
                       must contain it; Druid loads it with a local input source, so the
                       Druid server must read the same path
  run-directory        created if missing

The workload is clickbench-hits: the 43 queries, ids 1 to 43, in each engine's dialect, as
ClickBench runs them:
  TimescaleDB  the timescaledb extension in the database of POSTGRESQL_CONNECTION; the
               table is a compressed hypertable, loaded from hits.parquet through the
               DuckDB command line (DUCKDB) with psql's \copy
  Druid        EventTime is __time; loaded with SQL-based ingestion, which needs
               druid-parquet-extensions in druid.extensions.loadList
Variant approximate: the 8 queries with COUNT(DISTINCT x), with
distinct_count(approx_count_distinct(x)) of the timescaledb_toolkit extension on TimescaleDB.

PilotDB runs APPROXIMATE_QUERIES [1 2 3 4 8 21 30, the ones PilotDB's paper runs]. Every other
option is an environment variable of scripts/one-shot-workload.sh.

Example, with a ClickHouse server on CPUs 0-15 and user_files_path /data/clickbench:
  ENGINE_CPUS=0-15 THREADS=16 PILOTDB_PYTHON=~/pilotdb/.venv/bin/python \
    scripts/one-shot-clickbench.sh ~/ClickBench /data/clickbench runs/clickbench
Example, TimescaleDB and Druid against DuckDB:
  CLICKHOUSE_URL= PILOTDB_PYTHON= POSTGRESQL_CONNECTION="host=127.0.0.1 dbname=bench" \
    DRUID_URL=http://127.0.0.1:8888 scripts/one-shot-clickbench.sh ~/ClickBench /data/clickbench runs/clickbench
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
for file in duckdb/create.sql duckdb/queries.sql clickhouse/create.sql clickhouse/queries.sql \
  timescaledb/create.sql timescaledb/queries.sql druid/queries.sql; do
  [ -f "$clickbench/$file" ] || { echo "no $file in $clickbench" >&2; exit 1; }
done
duckdb=${DUCKDB:-duckdb}
command -v "$duckdb" >/dev/null || { echo "no $duckdb on PATH; set DUCKDB" >&2; exit 1; }
PATH=$(dirname "$(command -v "$duckdb")"):$PATH
export PATH
[ -f "$data/hits.parquet" ] || { echo "no hits.parquet in $data" >&2; exit 1; }
mkdir -p "$run"
workload=$(mktemp)
trap 'rm -f "$workload"' EXIT

python3 - "$clickbench" "$workload" "$(basename "$duckdb")" <<'PY'
import json, re, sys

clickbench, path, duckdb = sys.argv[1:]


def read(name):
    with open(f"{clickbench}/{name}") as file:
        return file.read()


def statement(text):
    return text[: text.rindex(";")].strip()


def queries(name):
    return [line.strip() for line in read(name).splitlines() if line.strip()]


duckdb_queries = queries("duckdb/queries.sql")
clickhouse_queries = queries("clickhouse/queries.sql")
timescaledb_queries = queries("timescaledb/queries.sql")
druid_queries = [re.sub(r"\bEventTime\b", "__time", query) for query in queries("druid/queries.sql")]
if len({len(duckdb_queries), len(clickhouse_queries), len(timescaledb_queries), len(druid_queries)}) != 1:
    sys.exit("the queries.sql files differ in length")

duckdb_rows = ("SELECT * REPLACE (make_date(EventDate) AS EventDate, "
               "epoch_ms(EventTime * 1000) AS EventTime, "
               "epoch_ms(ClientEventTime * 1000) AS ClientEventTime, "
               "epoch_ms(LocalEventTime * 1000) AS LocalEventTime) "
               "FROM read_parquet('{file}', binary_as_string=True)")
csv_rows = f"COPY ({duckdb_rows}) TO '/dev/stdout' (FORMAT csv, HEADER false)"
timescaledb_create = statement(read("timescaledb/create.sql"))
columns = re.findall(r"^\s*(\w+)\s+(\w+)", timescaledb_create.split("(", 1)[1], re.M)
druid_types = {"TIMESTAMP": "BIGINT", "DATE": "BIGINT", "TEXT": "VARCHAR", "VARCHAR": "VARCHAR",
               "CHAR": "VARCHAR"}
druid_columns = []
for name, kind in columns:
    if name == "EventTime":
        druid_columns.append('MILLIS_TO_TIMESTAMP("EventTime" * 1000) AS __time')
    elif name == "EventDate":
        druid_columns.append(
            "TIME_FORMAT(MILLIS_TO_TIMESTAMP(\"EventDate\" * 86400000), 'yyyy-MM-dd') AS \"EventDate\"")
    elif kind.upper() == "TIMESTAMP":
        druid_columns.append(
            f"TIME_FORMAT(MILLIS_TO_TIMESTAMP(\"{name}\" * 1000), 'yyyy-MM-dd HH:mm:ss') AS \"{name}\"")
    else:
        druid_columns.append(f'"{name}"')
druid_signature = ", ".join(f'"{name}" {druid_types.get(kind.upper(), "BIGINT")}' for name, kind in columns)
druid_ingestion = (
    "REPLACE INTO \"hits\" OVERWRITE ALL SELECT " + ", ".join(druid_columns) +
    " FROM TABLE(EXTERN('{\"type\": \"local\", \"files\": [\"{file}\"]}', "
    "'{\"type\": \"parquet\", \"binaryAsString\": true}')) EXTEND (" + druid_signature + ")"
    " PARTITIONED BY DAY CLUSTERED BY \"CounterID\", \"EventDate\", \"UserID\", \"WatchID\"")
count_distinct = re.compile(r"COUNT\(DISTINCT (\w+)\)", re.I)
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
                f"INSERT INTO hits {duckdb_rows}",
            ]},
            {"dialect": "ClickHouse", "statements": [
                statement(read("clickhouse/create.sql")),
                "INSERT INTO hits SELECT * FROM file('{file}', Parquet)",
            ]},
            {"dialect": "PostgreSql", "statements": [
                "CREATE EXTENSION IF NOT EXISTS timescaledb",
                "CREATE EXTENSION IF NOT EXISTS timescaledb_toolkit",
                timescaledb_create,
                "SELECT create_hypertable('hits', 'eventtime', chunk_time_interval => interval '3 day', "
                "create_default_indexes => false)",
                "ALTER TABLE hits SET (timescaledb.compress, timescaledb.compress_segmentby = '', "
                "timescaledb.compress_orderby = 'counterid, userid, eventtime')",
                "ALTER DATABASE :\"DBNAME\" SET timescaledb.enable_chunk_skipping TO ON",
                "ALTER DATABASE :\"DBNAME\" SET work_mem TO '1GB'",
                "ALTER DATABASE :\"DBNAME\" SET min_parallel_table_scan_size TO '0'",
                "SET timescaledb.enable_chunk_skipping TO ON",
                "SET work_mem TO '1GB'",
                "SET min_parallel_table_scan_size TO '0'",
                "SELECT enable_chunk_skipping('hits', 'counterid')",
                "\\copy hits FROM PROGRAM '" + f'{duckdb} -c "{csv_rows}"'.replace("'", "''") + "' WITH (FORMAT csv)",
                "SELECT count(compress_chunk(chunk, if_not_compressed => true)) FROM show_chunks('hits') chunk",
                "VACUUM FREEZE ANALYZE hits",
            ]},
            {"dialect": "Druid", "statements": [druid_ingestion]},
        ],
    }],
    "one_shot_queries": [],
}
for number, texts in enumerate(zip(duckdb_queries, clickhouse_queries, timescaledb_queries, druid_queries), 1):
    query = {
        "id": str(number),
        "texts": [
            {"language": "Sql", "text": text, "dialect": dialect}
            for dialect, text in zip(("DuckDb", "ClickHouse", "PostgreSql", "Druid"), texts)
        ],
    }
    if count_distinct.search(texts[2]):
        approximate = count_distinct.sub(r"distinct_count(approx_count_distinct(\1))", texts[2])
        query["variants"] = {"approximate": [{"language": "Sql", "text": approximate, "dialect": "PostgreSql"}]}
    workload["one_shot_queries"].append(query)
with open(path, "w") as file:
    json.dump(workload, file, indent=1)
PY

DUCKDB=$duckdb DUCKDB_DATABASE=${DUCKDB_DATABASE:-$run/hits.duckdb} \
  APPROXIMATE_QUERIES=${APPROXIMATE_QUERIES:-1 2 3 4 8 21 30} \
  "$(dirname "$0")/one-shot-workload.sh" "$workload" "$data" "$run"
