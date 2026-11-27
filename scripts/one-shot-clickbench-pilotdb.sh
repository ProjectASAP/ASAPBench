#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Compare PilotDB on DuckDB with exact DuckDB and exact ClickHouse on ClickBench's
hits table, with `approxbench benchmark-external one-shot-queries`:

  1. duckdb/exact      every ClickBench query on DuckDB; loads hits on first use
  2. clickhouse/exact  every ClickBench query on a running ClickHouse server;
                       loads hits on first use
  3. pilotdb/<target>  the approximate queries through PilotDB over DuckDB's database
  4. one-shot-report   accuracy against duckdb/exact, speedups over both exact runs

  scripts/one-shot-clickbench-pilotdb.sh <clickbench-checkout> <data-directory> <run-directory>

  clickbench-checkout  a clone of https://github.com/ClickHouse/ClickBench; the workload
                       takes duckdb/ and clickhouse/ create.sql and queries.sql from it
  data-directory       holds hits.parquet, from
                       https://datasets.clickhouse.com/hits_compatible/athena/hits.parquet;
                       ClickHouse loads it with file(), so the server's user_files_path
                       must contain it
  run-directory        created if missing; holds the workload, the results, the report
                       and, by default, the DuckDB database

Needs python3 and a DuckDB command line of the version PilotDB's duckdb package reads.

Environment (defaults in brackets):
  APPROXBENCH                  the approxbench binary [approxbench]
  DUCKDB                       the duckdb command line [duckdb]
  DUCKDB_DATABASE              [<run-directory>/hits.duckdb]
  DUCKDB_STORAGE_VERSION       the storage version DuckDB creates the database with [latest, as
                               ClickBench]; empty: DuckDB's default
  CLICKHOUSE_URL               the server's HTTP interface [http://127.0.0.1:8123]; empty skips it
  CLICKHOUSE_RESTART_COMMAND   a shell command that restarts the server; needed for cold runs
  CLICKHOUSE_SETTINGS          space-separated name=value settings for every ClickHouse
                               statement, e.g. use_query_condition_cache=0 [none, as ClickBench]
  PILOTDB_PYTHON               a Python that imports pilotdb and duckdb [python3]; empty skips it
  PILOTDB_RELATIVE_ERROR       [0.05]
  PILOTDB_FAILURE_PROBABILITY  [0.05]
  APPROXIMATE_QUERIES          ClickBench query numbers PilotDB runs [1 2 3 4 8 21 30, the ones
                               PilotDB's paper runs]
  RUNS                         runs per query of the exact configurations [3, as ClickBench]
  PILOTDB_RUNS                 runs per query of PilotDB [20, as PilotDB's accuracy experiments]
  THREADS                      DuckDB threads and ClickHouse max_threads [engine default]
  ENGINE_CPUS                  a taskset CPU list for approxbench and the DuckDB and PilotDB
                               processes it starts; pin the ClickHouse server to the same CPUs
  COLD_FIRST_RUN               1: restart the engine and drop the page cache before each query's
                               first run, as ClickBench's cold runs; runs approxbench with sudo
  CPU_FREQUENCY_GOVERNOR       e.g. performance: held during each configuration and restored
                               afterwards; runs approxbench with sudo

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
approxbench=${APPROXBENCH:-approxbench}
command -v "$approxbench" >/dev/null || { echo "no $approxbench on PATH; set APPROXBENCH" >&2; exit 1; }
duckdb=${DUCKDB:-duckdb}
command -v "$duckdb" >/dev/null || { echo "no $duckdb on PATH; set DUCKDB" >&2; exit 1; }
clickhouse_url=${CLICKHOUSE_URL-http://127.0.0.1:8123}
pilotdb_python=${PILOTDB_PYTHON-python3}
relative_error=${PILOTDB_RELATIVE_ERROR:-0.05}
failure_probability=${PILOTDB_FAILURE_PROBABILITY:-0.05}
read -r -a approximate <<<"${APPROXIMATE_QUERIES:-1 2 3 4 8 21 30}"

mkdir -p "$run/data" "$run/plans" "$run/results" "$run/workloads"
run=$(cd "$run" && pwd)
data=$(cd "$data" && pwd)
database=${DUCKDB_DATABASE:-$run/hits.duckdb}

python3 - "$clickbench" "$run/workloads/clickbench-hits.json" <<'PY'
import json, re, sys

clickbench, path = sys.argv[1:]


def read(name):
    with open(f"{clickbench}/{name}") as file:
        return file.read()


def statement(text):
    return text[: text.rindex(";")].strip()


def top_level_split(text):
    parts, depth, current = [], 0, ""
    for character in text:
        if character == "(":
            depth += 1
        elif character == ")":
            depth -= 1
        if character == "," and depth == 0:
            parts.append(current.strip())
            current = ""
        else:
            current += character
    parts.append(current.strip())
    return parts


def top_level_from(text):
    depth = 0
    for position, character in enumerate(text):
        if character == "(":
            depth += 1
        elif character == ")":
            depth -= 1
        elif depth == 0 and re.match(r"(?i)\sFROM\s", text[position:]):
            return position
    return None


def group_by_column_positions(query):
    select = re.match(r"(?is)\s*SELECT\s+", query)
    end = top_level_from(query[select.end():]) if select else None
    group_by = re.search(r"(?is)\sGROUP\s+BY\s+(.*?)(\s+HAVING\s|\s+ORDER\s+BY\s|\s+LIMIT\s|;|$)", query)
    if end is None or not group_by:
        return []
    items = top_level_split(query[select.end():select.end() + end])
    keys = {key.lower() for key in top_level_split(group_by.group(1))}
    positions = []
    for position, item in enumerate(items):
        named = re.split(r"(?i)\s+AS\s+", item, maxsplit=1)
        expression = named[0].strip()
        alias = named[1].strip() if len(named) == 2 else None
        if (str(position + 1) in keys or expression.lower() in keys
                or (alias is not None and alias.lower() in keys)):
            positions.append(position)
    return positions


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
            "group_by_column_positions": group_by_column_positions(duckdb_query),
        }
        for number, (duckdb_query, clickhouse_query) in enumerate(zip(duckdb_queries, clickhouse_queries), 1)
    ],
}
with open(path, "w") as file:
    json.dump(workload, file, indent=1)
PY

prefix=()
if [ -n "${COLD_FIRST_RUN:-}" ] || [ -n "${CPU_FREQUENCY_GOVERNOR:-}" ]; then
  prefix=(sudo env "PATH=$PATH" "LD_LIBRARY_PATH=${LD_LIBRARY_PATH:-}")
fi
if [ -n "${ENGINE_CPUS:-}" ]; then
  prefix+=(taskset -c "$ENGINE_CPUS")
fi
common=(--run-directory "$run" --workload clickbench-hits --data-directory "$data")
if [ -n "${COLD_FIRST_RUN:-}" ]; then
  common+=(--cold-first-run)
fi
if [ -n "${CPU_FREQUENCY_GOVERNOR:-}" ]; then
  common+=(--cpu-frequency-governor "$CPU_FREQUENCY_GOVERNOR")
fi
duckdb_options=(--duckdb-program "$(command -v "$duckdb")" --duckdb-database "$database")
if [ -n "${DUCKDB_STORAGE_VERSION-latest}" ]; then
  duckdb_options+=(--duckdb-storage-version "${DUCKDB_STORAGE_VERSION-latest}")
fi
if [ -n "${THREADS:-}" ]; then
  duckdb_options+=(--duckdb-threads "$THREADS")
fi

one_shot() {
  local result=$run/results/$1/clickbench-hits__$2.one-shot.json
  if [ -f "$result" ]; then
    echo "keeping $result" >&2
    return
  fi
  ${prefix[@]+"${prefix[@]}"} "$(command -v "$approxbench")" benchmark-external one-shot-queries \
    "${common[@]}" --engine "$1" --configuration "$2" "${@:3}"
}

status=0
one_shot duckdb exact --runs "${RUNS:-3}" "${duckdb_options[@]}" || status=$?
baselines=(--baseline duckdb/exact)
if [ "$status" -eq 0 ] && [ -n "$clickhouse_url" ]; then
  clickhouse_options=(--clickhouse-url "$clickhouse_url")
  if [ -n "${THREADS:-}" ]; then
    clickhouse_options+=(--clickhouse-max-threads "$THREADS")
  fi
  if [ -n "${CLICKHOUSE_RESTART_COMMAND:-}" ]; then
    clickhouse_options+=(--clickhouse-restart-command "$CLICKHOUSE_RESTART_COMMAND")
  fi
  for setting in ${CLICKHOUSE_SETTINGS:-}; do
    clickhouse_options+=(--clickhouse-setting "$setting")
  done
  one_shot clickhouse exact --runs "${RUNS:-3}" "${clickhouse_options[@]}" || status=$?
  baselines+=(--baseline clickhouse/exact)
fi
if [ "$status" -eq 0 ] && [ -n "$pilotdb_python" ]; then
  queries=()
  for query in "${approximate[@]}"; do
    queries+=(--query "$query")
  done
  one_shot pilotdb "relative-error-$relative_error-failure-probability-$failure_probability" \
    --runs "${PILOTDB_RUNS:-20}" "${duckdb_options[@]}" ${queries[@]+"${queries[@]}"} \
    --pilotdb-python "$pilotdb_python" \
    --pilotdb-relative-error "$relative_error" \
    --pilotdb-failure-probability "$failure_probability" || status=$?
fi
if [ ${#prefix[@]} -gt 0 ] && [ "${prefix[0]}" = sudo ]; then
  sudo chown -R "$(id -u):$(id -g)" "$run"
  for file in "$database" "$database.wal"; do
    if [ -e "$file" ]; then
      sudo chown "$(id -u):$(id -g)" "$file"
    fi
  done
fi
[ "$status" -eq 0 ] || exit "$status"

"$approxbench" benchmark-external one-shot-report --run-directory "$run" \
  --truth duckdb/exact "${baselines[@]}"
