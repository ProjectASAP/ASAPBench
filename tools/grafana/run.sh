#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
data=${GRAFANA_DATA_DIR:-$repo/target/grafana-data}
state=${GRAFANA_STATE_DIR:-$repo/target/grafana-home}
plugin=yesoreyeram-infinity-datasource
dashboards=$repo/tools/grafana/dashboards
check=0
phase=()
while [ $# -gt 0 ]; do
  case $1 in
    --check) check=1; shift ;;
    --phase) [ $# -ge 2 ] || { echo "usage: $0 [--check] [--phase 2|3]... [--real-data]" >&2; exit 2; }; phase+=(--phase "$2"); shift 2 ;;
    --real-data) phase+=(--real-data); shift ;;
    *) echo "usage: $0 [--check] [--phase 2|3]... [--real-data]" >&2; exit 2 ;;
  esac
done

if [ -n "${GRAFANA_HOME:-}" ]; then
  home=$GRAFANA_HOME
  grafana=$home/bin/grafana
elif command -v brew >/dev/null && [ -d "$(brew --prefix)/opt/grafana/share/grafana" ]; then
  home=$(brew --prefix)/opt/grafana/share/grafana
  grafana=$(brew --prefix)/opt/grafana/bin/grafana
else
  echo "grafana not found; on macOS run: brew install grafana; elsewhere unpack a Grafana release and set GRAFANA_HOME to its directory" >&2
  exit 1
fi
if [ ! -x "$grafana" ]; then
  echo "no grafana binary at $grafana" >&2
  exit 1
fi

mkdir -p "$state/data" "$state/plugins" "$state/logs" "$data"
if [ ! -d "$state/plugins/$plugin" ]; then
  "$grafana" cli --homepath "$home" --pluginsDir "$state/plugins" plugins install "$plugin"
fi
if [ $check = 1 ]; then
  exit 0
fi
if [ ${#phase[@]} -gt 0 ]; then
  dashboards=$repo/target/grafana-dashboards
  rm -rf "$dashboards"
  python3 "$repo/tools/grafana/generate_dashboards.py" --out "$dashboards" "${phase[@]}"
fi

python3 -m http.server 8000 --bind 127.0.0.1 --directory "$data" >/dev/null 2>&1 &
server=$!
python3 -m http.server 8001 --bind 127.0.0.1 --directory "$repo/docs/figures/phase2-use-cases" >/dev/null 2>&1 &
figures=$!
trap 'kill $server $figures 2>/dev/null' EXIT

echo "dashboards at http://localhost:3000 reading $data" >&2
GRAFANA_DASHBOARDS_DIR=$dashboards "$grafana" server \
  --homepath "$home" \
  cfg:default.paths.data="$state/data" \
  cfg:default.paths.logs="$state/logs" \
  cfg:default.paths.plugins="$state/plugins" \
  cfg:default.paths.provisioning="$repo/tools/grafana/provisioning" \
  cfg:server.http_addr=127.0.0.1 \
  cfg:auth.anonymous.enabled=true \
  cfg:auth.anonymous.org_role=Viewer
