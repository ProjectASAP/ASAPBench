#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Run any one-shot workload on DuckDB, ClickHouse, PostgreSQL, Druid and PilotDB over DuckDB
with `approxbench benchmark-external one-shot-queries`, then report:

  1. duckdb/exact        every query on DuckDB; loads the workload's tables on first use
  2. clickhouse/exact    every query on a running ClickHouse server
  3. postgresql/exact    every query on a running PostgreSQL server, e.g. with TimescaleDB
  4. druid/exact         every query on a running Druid, with COUNT(DISTINCT) and TopN exact
  5. druid/defaults      every query with Druid's defaults, which approximate COUNT(DISTINCT)
                         and TopN, as ClickBench runs Druid
  6. <engine>/<variant>  for each query variant of the workload, e.g. approximate: the queries
                         with a text in that variant for the engine's dialect, on each engine
                         above that runs
  7. pilotdb/<target>    the approximate queries through PilotDB over DuckDB's database
  8. one-shot-report     accuracy against duckdb/exact, speedups over each exact run

A server loads the tables on first use with the workload's statements for its dialect.

  scripts/one-shot-workload.sh <workload-file> <data-directory> <run-directory>

  workload-file   a workload JSON: its tables (file_name, rows, and load_statements per
                  dialect, with {file} for the source file's path) and its one_shot_queries
                  (id without spaces, texts, optionally variants and
                  group_by_column_positions; when left out, DuckDB's parser reads them from
                  the SELECT and GROUP BY lists, and a query it cannot read stops the run
                  before any engine starts)
  data-directory  holds every table's file_name; a server loads with the statements the
                  workload gives, so a server-side load needs the file where the server reads
  run-directory   created if missing; holds the workload, the results, the report and, by
                  default, the DuckDB database. A configuration whose result exists is kept,
                  so a stopped run resumes; a different workload of the same name is refused
                  once it has results.

Example of a workload query:
  {"id": "users", "group_by_column_positions": [0],
   "texts": [{"language": "Sql", "text": "SELECT RegionID, COUNT(DISTINCT UserID) FROM hits GROUP BY RegionID"}],
   "variants": {"approximate": [{"language": "Sql", "dialect": "PostgreSql",
     "text": "SELECT RegionID, distinct_count(approx_count_distinct(UserID)) FROM hits GROUP BY RegionID"}]}}
A text with "dialect": "DuckDb", "ClickHouse", "PostgreSql" or "Druid" is used by that engine
instead of the plain one. A variant is named with letters, digits, '-' and '_', other than exact
and defaults.

Needs python3 and a DuckDB command line of the version PilotDB's duckdb package reads.

Environment (defaults in brackets):
  APPROXBENCH                  the approxbench binary [approxbench]
  DUCKDB                       the duckdb command line [duckdb]
  DUCKDB_DATABASE              [<run-directory>/<workload>.duckdb]
  DUCKDB_STORAGE_VERSION       the storage version DuckDB creates the database with [latest, as
                               ClickBench]; empty: DuckDB's default
  CLICKHOUSE_URL               the server's HTTP interface [http://127.0.0.1:8123]; empty skips it
  CLICKHOUSE_RESTART_COMMAND   a shell command that restarts the server; needed for cold runs
  CLICKHOUSE_SETTINGS          space-separated name=value settings for every ClickHouse
                               statement, e.g. use_query_condition_cache=0 [none, as ClickBench]
  POSTGRESQL_CONNECTION        a libpq connection string, e.g. "host=127.0.0.1 dbname=bench"
                               [empty: skipped]
  PSQL                         the psql command line [psql]
  POSTGRESQL_RESTART_COMMAND   a shell command that restarts the server; needed for cold runs
  POSTGRESQL_SETTINGS          space-separated name=value settings SET before every query
  DRUID_URL                    the router, e.g. http://127.0.0.1:8888 [empty: skipped]
  DRUID_RESTART_COMMAND        a shell command that restarts Druid; needed for cold runs
  DRUID_CONTEXT                space-separated name=value query context for every Druid
                               configuration
  PILOTDB_PYTHON              a Python that imports pilotdb and duckdb [python3]; empty skips it
  PILOTDB_RELATIVE_ERROR       [0.05]
  PILOTDB_FAILURE_PROBABILITY  [0.05]
  APPROXIMATE_QUERIES          space-separated query ids PilotDB runs [every query]
  RUNS                         runs per query of every configuration but PilotDB's [3, as
                               ClickBench]
  PILOTDB_RUNS                 runs per query of PilotDB [20, as PilotDB's accuracy experiments]
  THREADS                      DuckDB threads and ClickHouse max_threads [engine default]
  ENGINE_CPUS                  a taskset CPU list for approxbench and the DuckDB, psql and PilotDB
                               processes it starts; pin the servers to the same CPUs
  COLD_FIRST_RUN               1: restart the engine and drop the page cache before each query's
                               first run, as ClickBench's cold runs; runs approxbench with sudo
  CPU_FREQUENCY_GOVERNOR       e.g. performance: held during each configuration and restored
                               afterwards; runs approxbench with sudo

Example:
  ENGINE_CPUS=0-15 THREADS=16 POSTGRESQL_CONNECTION="host=127.0.0.1 dbname=bench" \
    DRUID_URL=http://127.0.0.1:8888 PILOTDB_PYTHON=~/pilotdb/.venv/bin/python \
    scripts/one-shot-workload.sh my-workload.json /data/tables runs/mine
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
workload_file=$1; data=$2; run=$3
command -v python3 >/dev/null || { echo "needs python3" >&2; exit 1; }
[ -f "$workload_file" ] || { echo "no workload file at $workload_file" >&2; exit 1; }
[ -d "$data" ] || { echo "no data directory at $data" >&2; exit 1; }
approxbench=${APPROXBENCH:-approxbench}
command -v "$approxbench" >/dev/null || { echo "no $approxbench on PATH; set APPROXBENCH" >&2; exit 1; }
duckdb=${DUCKDB:-duckdb}
command -v "$duckdb" >/dev/null || { echo "no $duckdb on PATH; set DUCKDB" >&2; exit 1; }
clickhouse_url=${CLICKHOUSE_URL-http://127.0.0.1:8123}
postgresql_connection=${POSTGRESQL_CONNECTION:-}
druid_url=${DRUID_URL:-}
pilotdb_python=${PILOTDB_PYTHON-python3}
relative_error=${PILOTDB_RELATIVE_ERROR:-0.05}
failure_probability=${PILOTDB_FAILURE_PROBABILITY:-0.05}

mkdir -p "$run/data" "$run/plans" "$run/results" "$run/workloads"
run=$(cd "$run" && pwd)
data=$(cd "$data" && pwd)

workload=$(python3 - "$workload_file" "$run" "$data" "$(command -v "$duckdb")" <<'PY'
import glob, json, os, re, subprocess, sys

source, run, data, duckdb = sys.argv[1:]
with open(source) as file:
    workload = json.load(file)

SHAPES = {
    "workload": ({"name", "one_shot_queries"}, {"queries", "data", "tables"}),
    "table": ({"name", "file_name", "rows", "load_statements"}, {"url"}),
    "load": ({"dialect", "statements"}, set()),
    "query": ({"id", "texts"}, {"group_by_column_positions", "variants"}),
    "text": ({"language", "text"}, {"dialect"}),
}
LANGUAGES = {"PromQl", "Sql", "MetricsQl"}


def check(what, value, where):
    required, optional = SHAPES[what]
    if not isinstance(value, dict):
        sys.exit(f"{where}: a {what} must be a JSON object")
    missing = required - value.keys()
    unknown = value.keys() - required - optional
    if missing or unknown:
        sys.exit(f"{where}: {what} missing {sorted(missing)}, unknown {sorted(unknown)}")


check("workload", workload, source)
name = workload["name"]
if not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]*", name) or "__" in name:
    sys.exit(f"{source}: the name must be letters, digits, '-', '_' and '.', not start with '.', and not contain '__'")
queries = workload["one_shot_queries"]
if not queries:
    sys.exit(f"{source} has no one_shot_queries")
for table in workload.get("tables", []):
    check("table", table, f"{source}: table")
    for load in table["load_statements"]:
        check("load", load, f"{source}: table {table['name']}")
    if not os.path.isfile(os.path.join(data, table["file_name"])):
        sys.exit(f"table {table['name']}: no {table['file_name']} in {data}")
ids = set()
for query in queries:
    check("query", query, f"{source}: query")
    if not query["id"] or re.search(r"\s", query["id"]) or query["id"] in ids:
        sys.exit(f"query id '{query['id']}' must be unique, non-empty and without spaces")
    ids.add(query["id"])
    variants = query.get("variants", {})
    if not isinstance(variants, dict):
        sys.exit(f"query {query['id']}: variants must map a name to texts")
    for variant, texts in [(None, query["texts"])] + sorted(variants.items()):
        where = f"query {query['id']}" + (f" variant {variant}" if variant else "")
        if variant is not None and (not re.fullmatch(r"[A-Za-z0-9_-]+", variant)
                                    or "__" in variant or variant in ("exact", "defaults")):
            sys.exit(f"{where}: a variant is named with letters, digits, '-' and '_', other than exact and defaults")
        if not isinstance(texts, list) or not texts:
            sys.exit(f"{where}: needs a list of texts")
        for text in texts:
            check("text", text, where)
            if text["language"] not in LANGUAGES:
                sys.exit(f"{where}: unknown language {text['language']}")
workload.setdefault("queries", [])
workload.setdefault("data", [])


def without_locations(node):
    if isinstance(node, dict):
        return {key: without_locations(value) for key, value in node.items()
                if key not in ("query_location", "alias")}
    if isinstance(node, list):
        return [without_locations(value) for value in node]
    return node


def parsed(sql):
    quoted = sql.replace("'", "''")
    output = subprocess.run(
        [duckdb, "-json", "-c", f"SELECT json_serialize_sql('{quoted}') AS tree"],
        capture_output=True, text=True, check=True).stdout
    tree = json.loads(output)[0]["tree"]
    return tree if isinstance(tree, dict) else json.loads(tree)


def group_by_column_positions(query_id, sql):
    tree = parsed(sql)
    if tree.get("error"):
        sys.exit(f"query {query_id}: DuckDB cannot parse it: {tree.get('error_message')}")
    node = tree["statements"][0]["node"]
    ask = f"query {query_id}: declare group_by_column_positions"
    if node.get("type") != "SELECT_NODE":
        sys.exit(f"{ask}; its group columns are not read from a {node.get('type')}")
    items = node["select_list"]
    groups = node.get("group_expressions") or []
    if node.get("aggregate_handling") != "STANDARD_HANDLING" or len(node.get("group_sets") or []) > 1:
        sys.exit(f"{ask}; GROUP BY ALL, ROLLUP, CUBE and GROUPING SETS are not read")
    distinct = any(modifier.get("type") == "DISTINCT_MODIFIER" for modifier in node.get("modifiers", []))
    if not groups and not distinct:
        return []
    if any(item.get("class") == "STAR" for item in items):
        sys.exit(f"{ask}; a SELECT * with GROUP BY or DISTINCT is not read")
    if not groups:
        return list(range(len(items)))
    aliases = {item["alias"].lower(): position for position, item in enumerate(items) if item.get("alias")}
    shapes = [without_locations(item) for item in items]
    positions = set()
    for group in groups:
        if group.get("class") == "CONSTANT" and group["value"]["type"]["id"] in ("INTEGER", "BIGINT"):
            positions.add(group["value"]["value"] - 1)
        elif group.get("class") == "COLUMN_REF" and len(group["column_names"]) == 1 \
                and group["column_names"][0].lower() in aliases:
            positions.add(aliases[group["column_names"][0].lower()])
        elif without_locations(group) in shapes:
            positions.add(shapes.index(without_locations(group)))
    if not positions:
        sys.exit(f"{ask}; none of its GROUP BY keys is in its SELECT list")
    return sorted(positions)


for query in queries:
    if "group_by_column_positions" in query:
        continue
    texts = [text for text in query["texts"] if text["language"] == "Sql"]
    chosen = ([text for text in texts if text.get("dialect") == "DuckDb"]
              or [text for text in texts if "dialect" not in text])
    if not chosen:
        sys.exit(f"query {query['id']} has no plain or DuckDB SQL text")
    query["group_by_column_positions"] = group_by_column_positions(query["id"], chosen[0]["text"])
    print(f"query {query['id']}: group columns {query['group_by_column_positions']}", file=sys.stderr)
path = os.path.join(run, "workloads", f"{name}.json")
if os.path.exists(path):
    with open(path) as file:
        same = json.load(file) == workload
    if not same and glob.glob(os.path.join(run, "results", "*", f"{name}__*.one-shot.json")):
        sys.exit(f"{path} holds a different workload {name} with results; use a new run directory")
with open(path, "w") as file:
    file.write(json.dumps(workload, indent=1) + "\n")
print(name, " ".join(query["id"] for query in queries))
variant_dialects = {}
for query in queries:
    for variant, texts in query.get("variants", {}).items():
        for text in texts:
            if text["language"] == "Sql":
                variant_dialects.setdefault(variant, set()).add(text.get("dialect", "plain"))
for variant, dialects in sorted(variant_dialects.items()):
    print(variant, " ".join(sorted(dialects)))
PY
)
variant_lines=()
{
  read -r name all_queries
  while read -r line; do
    variant_lines+=("$line")
  done
} <<<"$workload"
read -r -a approximate <<<"${APPROXIMATE_QUERIES:-$all_queries}"
database=${DUCKDB_DATABASE:-$run/$name.duckdb}

prefix=()
if [ -n "${COLD_FIRST_RUN:-}" ] || [ -n "${CPU_FREQUENCY_GOVERNOR:-}" ]; then
  prefix=(sudo env "PATH=$PATH" "LD_LIBRARY_PATH=${LD_LIBRARY_PATH:-}")
fi
if [ -n "${ENGINE_CPUS:-}" ]; then
  prefix+=(taskset -c "$ENGINE_CPUS")
fi
common=(--run-directory "$run" --workload "$name" --data-directory "$data")
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
  local result=$run/results/$1/${name}__$2.one-shot.json
  if [ -f "$result" ]; then
    echo "keeping $result" >&2
    return
  fi
  ${prefix[@]+"${prefix[@]}"} "$(command -v "$approxbench")" benchmark-external one-shot-queries \
    "${common[@]}" --engine "$1" --configuration "$2" "${@:3}"
}

variant_runs() {
  local engine=$1 dialect=$2 line variant dialects
  shift 2
  for line in ${variant_lines[@]+"${variant_lines[@]}"}; do
    read -r variant dialects <<<"$line"
    case " $dialects " in
      *" $dialect "* | *" plain "*)
        if [ "$status" -eq 0 ]; then
          one_shot "$engine" "$variant" --query-variant "$variant" --runs "${RUNS:-3}" "$@" || status=$?
        fi
        ;;
    esac
  done
}

status=0
one_shot duckdb exact --runs "${RUNS:-3}" "${duckdb_options[@]}" || status=$?
baselines=(--baseline duckdb/exact)
variant_runs duckdb DuckDb "${duckdb_options[@]}"
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
  variant_runs clickhouse ClickHouse "${clickhouse_options[@]}"
fi
if [ "$status" -eq 0 ] && [ -n "$postgresql_connection" ]; then
  psql=${PSQL:-psql}
  command -v "$psql" >/dev/null || { echo "no $psql on PATH; set PSQL" >&2; exit 1; }
  postgresql_options=(--postgresql-program "$(command -v "$psql")"
    --postgresql-connection "$postgresql_connection")
  if [ -n "${POSTGRESQL_RESTART_COMMAND:-}" ]; then
    postgresql_options+=(--postgresql-restart-command "$POSTGRESQL_RESTART_COMMAND")
  fi
  for setting in ${POSTGRESQL_SETTINGS:-}; do
    postgresql_options+=(--postgresql-setting "$setting")
  done
  one_shot postgresql exact --runs "${RUNS:-3}" "${postgresql_options[@]}" || status=$?
  baselines+=(--baseline postgresql/exact)
  variant_runs postgresql PostgreSql "${postgresql_options[@]}"
fi
if [ "$status" -eq 0 ] && [ -n "$druid_url" ]; then
  druid_options=(--druid-url "$druid_url")
  if [ -n "${DRUID_RESTART_COMMAND:-}" ]; then
    druid_options+=(--druid-restart-command "$DRUID_RESTART_COMMAND")
  fi
  for setting in ${DRUID_CONTEXT:-}; do
    druid_options+=(--druid-context "$setting")
  done
  one_shot druid exact --runs "${RUNS:-3}" "${druid_options[@]}" \
    --druid-context useApproximateCountDistinct=false \
    --druid-context useApproximateTopN=false || status=$?
  baselines+=(--baseline druid/exact)
  if [ "$status" -eq 0 ]; then
    one_shot druid defaults --runs "${RUNS:-3}" "${druid_options[@]}" || status=$?
  fi
  variant_runs druid Druid "${druid_options[@]}"
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
