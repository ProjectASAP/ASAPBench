#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Measure the write throughput of an ASAPQuery precompute engine that is already
running, with `approxbench benchmark-external write-throughput`: one
measurement, open loop (the most the engine acknowledges) or at a target rate.
Linux only: the engine's CPU and memory are read from /proc. Needs python3.

  scripts/write-throughput-asapquery-precompute-engine.sh <engine-url> <engine-pid> <streaming-config> <run-directory>

  engine-url        the engine's ingest address, e.g. http://127.0.0.1:8081
  engine-pid        the engine's process id; its CPU and memory are read from /proc
  streaming-config  the streaming config the engine was started with
  run-directory     created if missing; the result is appended to
                    <run-directory>/results/asapquery-precompute-engine/write-throughput.jsonl

Start a fresh engine for every measurement: the engine keeps the event time
and windows of earlier writes and drops later samples at earlier event times
while still acknowledging them.

Environment (defaults in brackets):
  APPROXBENCH                  the approxbench binary [approxbench]
  METRIC                       the metric written [the config's first aggregation's metric]
  LABELS                       its label names, space-separated [the config's metrics entry
                               for METRIC]; reading the config needs PyYAML, setting both does not
  SERIES                       series written [100000]
  SAMPLE_INTERVAL_MS           event time between two samples of a series [1000]
  WRITERS                      concurrent writers, one request in flight each [16]
  SAMPLES_PER_REQUEST          at most this many samples per Remote Write request [2000]
  MAX_EVENT_TIME_LEAD_MS       how far a writer may run ahead of the slowest one; keep it below
                               the engine's --allowed-lateness-ms, or the engine drops the
                               slowest writer's samples while acknowledging them [2000]
  TARGET_SAMPLES_PER_SECOND    unset: open loop
  WARMUP_SECONDS               [15]
  MEASURE_SECONDS              [30]
  LOAD_CPUS                    a taskset CPU list for approxbench, disjoint from the engine's
  CPU_FREQUENCY_GOVERNOR       e.g. performance: held on every CPU during the measurement and
                               restored afterwards; runs approxbench with sudo

Example, with an engine on CPUs 0-7 started from its own config:
  CPU_FREQUENCY_GOVERNOR=performance LOAD_CPUS=8-23 \
    scripts/write-throughput-asapquery-precompute-engine.sh \
    http://127.0.0.1:8081 "$(pgrep -o -f 'precompute_engine --streaming-config')" kll.yaml runs/kll-100k
EOF
}

if [ $# -ge 1 ] && { [ "$1" = -h ] || [ "$1" = --help ]; }; then
  usage
  exit 0
fi
if [ $# -ne 4 ]; then
  usage >&2
  exit 2
fi
url=$1; pid=$2; config=$3; run=$4
[ "$(uname -s)" = Linux ] || { echo "Linux only: the engine is measured through /proc" >&2; exit 1; }
command -v python3 >/dev/null || { echo "needs python3" >&2; exit 1; }
[ -f "$config" ] || { echo "no streaming config at $config" >&2; exit 1; }
[[ "$pid" =~ ^[0-9]+$ ]] || { echo "engine-pid must be one process id, got: $pid" >&2; exit 1; }
[ -d "/proc/$pid" ] || { echo "no process $pid" >&2; exit 1; }

approxbench=${APPROXBENCH:-approxbench}
command -v "$approxbench" >/dev/null || { echo "no $approxbench on PATH; set APPROXBENCH" >&2; exit 1; }
if [ -n "${METRIC:-}" ] && [ -n "${LABELS:-}" ]; then
  metric=$METRIC
  labels=$LABELS
else
  metric_and_labels=$(python3 - "$config" "${METRIC:-}" <<'PY'
import sys
try:
    import yaml
except ImportError:
    sys.exit("reading the streaming config needs PyYAML; set METRIC and LABELS instead")
config, metric = sys.argv[1], sys.argv[2]
with open(config) as file:
    parsed = yaml.safe_load(file)
metric = metric or parsed["aggregations"][0]["metric"]
labels = (parsed.get("metrics") or {}).get(metric)
if not labels:
    sys.exit(f"{config} has no metrics entry for {metric}; set LABELS")
print(metric, " ".join(labels))
PY
  )
  read -r metric labels <<<"$metric_and_labels"
fi
read -r -a label_names <<<"$labels"
series=${SERIES:-100000}
sample_interval_ms=${SAMPLE_INTERVAL_MS:-1000}
workload=precompute-engine-$series-series

mkdir -p "$run/data" "$run/plans" "$run/results" "$run/workloads"
python3 - "$run/workloads/$workload.json" "$workload" "$metric" "$series" "$sample_interval_ms" "${label_names[@]}" <<'PY'
import json, sys
path, name, metric, series, interval, *labels = sys.argv[1:]
series, interval = int(series), int(interval)
cardinalities = [10] * (len(labels) - 1)
rest = 1
for cardinality in cardinalities:
    rest *= cardinality
cardinalities.append(max(1, -(-series * 10 // rest)))
workload = {
    "name": name,
    "queries": [],
    "data": [{
        "metric": metric,
        "description": {
            "labels": [{"name": label, "cardinality": cardinality}
                       for label, cardinality in zip(labels, cardinalities)],
            "series_num": series,
            "start_ms": 1_700_000_000_000,
            "sample_interval_ms": interval,
            "duration_ms": 10_000_000_000,
            "value_distribution": {"kind": "uniform", "lower_bound": 0.0, "upper_bound": 10000.0, "seed": 0},
            "counter": False,
            "seed": 1,
        },
    }],
}
with open(path, "w") as file:
    json.dump(workload, file)
PY

command=("$(command -v "$approxbench")" benchmark-external write-throughput
  --run-directory "$run"
  --engine asapquery-precompute-engine
  --workload "$workload"
  --asapquery-precompute-engine-url "$url"
  --asapquery-precompute-engine-streaming-config "$config"
  --server-pid "$pid"
  --writers "${WRITERS:-16}"
  --samples-per-request "${SAMPLES_PER_REQUEST:-2000}"
  --max-event-time-lead-ms "${MAX_EVENT_TIME_LEAD_MS:-2000}"
  --warmup-seconds "${WARMUP_SECONDS:-15}"
  --measure-seconds "${MEASURE_SECONDS:-30}")
if [ -n "${TARGET_SAMPLES_PER_SECOND:-}" ]; then
  command+=(--target-samples-per-second "$TARGET_SAMPLES_PER_SECOND")
fi
if [ -n "${LOAD_CPUS:-}" ]; then
  command=(taskset -c "$LOAD_CPUS" "${command[@]}")
fi
if [ -n "${CPU_FREQUENCY_GOVERNOR:-}" ]; then
  command=(sudo "${command[@]}" --cpu-frequency-governor "$CPU_FREQUENCY_GOVERNOR")
fi

status=0
output=$("${command[@]}") || status=$?
if [ -n "${CPU_FREQUENCY_GOVERNOR:-}" ]; then
  sudo chown -R "$(id -u):$(id -g)" "$run/results"
fi
[ "$status" -eq 0 ] || exit "$status"

python3 - "$output" <<'PY'
import json, sys
result = json.loads(sys.argv[1])
memory = result.get("server_process_memory_after") or {}
peak = memory.get("peak_resident_set_size_bytes")
governors = ", ".join(result.get("cpu_frequency_governors", [])) or "unreadable"
rows = [
    ("acknowledged samples/s", f"{result['acknowledged_samples_per_second']:,.0f}"),
    ("engine CPU cores", f"{result['engine_cpu_cores']:.2f}"),
    ("approxbench CPU cores", f"{result['harness_cpu_cores']:.2f}"),
    ("engine peak RSS", "unreadable" if peak is None else f"{peak / 2**20:,.0f} MiB"),
    ("widest event-time lead", f"{result['widest_event_time_lead_ms']} ms"),
    ("CPU frequency governors", governors),
]
for name, value in rows:
    print(f"{name:<24}{value}")
PY
