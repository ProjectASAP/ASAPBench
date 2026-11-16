#!/usr/bin/env python3

import argparse
import glob
import json
import os
import sys
from datetime import datetime, timezone

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(REPO, "scripts", "use_cases"))
import cases
import rollout

DS = {"type": "yesoreyeram-infinity-datasource", "uid": "approx-rows"}
DATA = "http://localhost:8000/runs/"
PHASE3_DATA = "http://localhost:8000/phase3/runs/"
RUN = DATA + "${run}/"
INDEX = DATA + "index.json"
FIGURES = "http://localhost:8001/"
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "dashboards")
REAL_DATA = False


def synthetic_only():
    return "" if REAL_DATA else ' and $not($contains(run_id, "-real"))'


def iq(ref, url, root="", columns=None, empty="[]"):
    return {"refId": ref, "datasource": DS, "type": "json", "source": "url", "parser": "backend",
            "format": "table", "url": url, "url_options": {"method": "GET", "data": ""},
            "root_selector": or_empty(root, empty) if root else "", "columns": columns or []}


def col(selector, text, type_):
    return {"selector": selector, "text": text, "type": type_}


T = col("time", "time", "timestamp_epoch")


def rows_of(ref, path, sel, name, type_="number", by=None, empty="[]"):
    columns = [T, col("value", name, type_)]
    root = f"$[{sel}]"
    if by:
        root = f'$[{sel}].{{"time": time, "value": value, "{by}": labels.{by}}}'
        columns.append(col(by, by, "string"))
    target = iq(ref, RUN + path + ".json", root, columns, empty)
    if by:
        target["format"] = "timeseries"
    return target


def or_empty(root, empty="[]"):
    return f"( $r := {root}; $exists($r) ? $r : {empty} )"


def metric_values(sel, body="$s"):
    return f"( $m := $[{sel}]; $exists($m) ? ( $s := $m{{metric: value}}; {body} ) : [] )"


def entity_file(kind, variable):
    return f"{kind}/${{{variable}}}"


def named(*names):
    return [{"matcher": {"id": "byName", "options": n}, "properties": [{"id": "displayName", "value": n}]}
            for n in names]


def by_ref(ref, color, width=1):
    return {"matcher": {"id": "byFrameRefID", "options": ref},
            "properties": [{"id": "color", "value": {"mode": "fixed", "fixedColor": color}},
                           {"id": "custom.lineWidth", "value": width}]}


def dashed(ref, color):
    return {"matcher": {"id": "byFrameRefID", "options": ref},
            "properties": [{"id": "color", "value": {"mode": "fixed", "fixedColor": color}},
                           {"id": "custom.lineStyle", "value": {"fill": "dash", "dash": [10, 10]}}]}


def exact_solid_approx_dashed(path, colors, metric=None):
    targets, overrides = [], []
    for query, color in colors.items():
        for arm, style in (("exact", by_ref), ("approx", dashed)):
            ref = f"{query} {arm}"
            sel = f'labels.query="{query}" and labels.arm="{arm}"'
            targets.append(rows_of(ref, path, f'metric="{metric}" and {sel}' if metric else sel, ref))
            overrides.append(style(ref, color))
    return targets, overrides


def order(*names):
    return [{"id": "organize", "options": {"indexByName": {n: i for i, n in enumerate(names)},
                                           "excludeByName": {}, "renameByName": {}}}]


def in_list(values):
    return "[" + ", ".join(f'"{v}"' for v in values) + "]"


LINES = {"custom": {"lineWidth": 1, "showPoints": "never"}}
LEGEND = {"legend": {"displayMode": "list", "placement": "bottom", "showLegend": True},
          "tooltip": {"mode": "multi", "sort": "none"}}
STAT = {"reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False},
        "textMode": "value_and_name", "colorMode": "none", "graphMode": "none",
        "orientation": "horizontal", "justifyMode": "auto"}
TEXT = {"color": {"mode": "fixed", "fixedColor": "text"}}
OFF = {"color": "transparent", "text": " "}
ANNOTATION = {"none": OFF, "onset delay": {"color": "#FADE2A"}, "gap": {"color": "#8F3BB8"},
              "end delay": {"color": "#19B5B5"}, "false positive": {"color": "#FF9830"},
              "false negative": {"color": "#D3369E"}, "missed episode": {"color": "#F5A3C7"},
              "decision delay": {"color": "#A0703C"}}
COMBINED = "#9E9E9E"
OUTCOMES = {"true_positive": "true positive", "false_positive": "false positive",
            "false_negative": "false negative", "true_negative": "true negative"}


def value_mappings(states):
    options = {value: {**result, "index": i} for i, (value, result) in enumerate(states.items())}
    return [{"type": "value", "options": options},
            {"type": "regex", "options": {"pattern": ".+, .+", "result": {"color": COMBINED, "index": len(states)}}}]


def strip_panel(title, rows, states, path="app_state"):
    return {"type": "state-timeline", "title": title,
            "targets": [rows_of(chr(ord("A") + i), path, sel, name, "string",
                                empty='( $exists($$[0]) ? [{"time": $$[0].time}] : [] )')
                        for i, (sel, name) in enumerate(rows)],
            "fieldConfig": {"defaults": {"custom": {"fillOpacity": 80, "lineWidth": 0},
                                         "color": {"mode": "fixed", "fixedColor": "transparent"},
                                         "mappings": value_mappings({**ANNOTATION, **states})},
                            "overrides": named(*[name for _, name in rows])},
            "options": {"mergeValues": False, "showValue": "auto", "rowHeight": 0.8, "alignValue": "center",
                        **LEGEND, "legend": {"showLegend": False}}}


def outcome_stat(title, summary, sel, units=None):
    names = {metric: f"{text} ({units[metric]})" if units else text for metric, text in OUTCOMES.items()}
    return {"type": "stat", "title": title,
            "targets": [iq("A", summary, metric_values(sel),
                           by_metric_columns(*names.items()))],
            "transformations": order(*names.values()),
            "fieldConfig": {"defaults": TEXT, "overrides": []},
            "options": STAT}


def disagreement_panel(uid, name, key, title):
    return {"type": "table", "title": title,
            "targets": [iq("A", RUN + "fidelity_summary.json",
                           f'$[metric="disagreements" and value > 0].'
                           f'{{"{key}": labels.{key}, "disagreements": value}}',
                           [col(key, key, "string"), col("disagreements", "disagreements", "number")])],
            "transformations": sort_limit("disagreements", 20),
            "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": [
                {"matcher": {"id": "byName", "options": key},
                 "properties": [{"id": "links", "value": [{
                     "title": f"Show this {name}",
                     "url": f"/d/{uid}/{uid}?var-run=${{run}}&var-{name}=${{__data.fields.{key}}}"
                            "&${__url_time_range}"}]}]}]},
            "options": {"showHeader": True}}


def by_disagreements(name, key):
    ranked = ('( $s := $sort($[metric="disagreements"], function($a, $b) { $a.value < $b.value or '
              f'($a.value = $b.value and $a.labels.{key} > $b.labels.{key}) }}); $s.labels )')
    out = variable(name, RUN + "fidelity_summary.json", ranked, key)
    out["sort"] = 0
    return out


def in_run(name, key, chosen):
    root = (f'( $m := $[metric="disagreements" and labels.{key} in [${{{chosen}:doublequote}}]].labels; '
            f'$a := $[metric="disagreements"].labels; $exists($m) ? $m : $a[0] )')
    out = variable(name, RUN + "fidelity_summary.json", root, key)
    out.update(hide=2, skipUrlSync=True)
    return out


class Board:
    def __init__(self):
        self.panels = []

    def _id(self):
        return len(self.panels) + 1

    def panel(self, p, x, y, w, h):
        p.update(id=self._id(), gridPos={"x": x, "y": y, "w": w, "h": h})
        p.setdefault("datasource", DS)
        if p["type"] == "table":
            p.setdefault("fieldConfig", {"defaults": {}, "overrides": []})["defaults"].setdefault(
                "custom", {})["wrapHeaderText"] = True
        self.panels.append(p)

    def row(self, title, y):
        self.panels.append({"id": self._id(), "type": "row", "title": title, "collapsed": False,
                            "gridPos": {"x": 0, "y": y, "w": 24, "h": 1}, "panels": []})


RUN_FIELDS = ("run_id", "dataset", "configuration", "policy", "use_case", "start", "end", "planner_rev")


def runs_panel(board, uid, use_cases, x, y, w, h):
    runs = (f'$[use_case in {in_list(use_cases)}{synthetic_only()}].{{"run_id": run_id, "use_case": use_case, "start": start, '
            '"end": end, "planner_rev": details.planner_rev, '
            '"dataset": $contains(run_id, "-real") ? ( $d := $substringAfter(run_id, "-"); '
            '$d := $substringBefore($d, "-systems"); $d := $substringBefore($d, "-noraw"); '
            '$d := $substringBefore($d, "-raw"); $replace($d, "-", " ", 1) ) : "synthetic", '
            '"configuration": "error target " & $string(details.arm.max_error * 100) & "%" '
            '& (details.arm.raw_data_retained ? ", raw samples kept" : ", raw samples not kept") '
            '& (details.arm.ignore_guarantees ? ", plans without an accuracy guarantee allowed" : ""), '
            '"policy": $contains(run_id, "-rerun") ? "rerun" : ($contains(run_id, "-bound") ? "bound" : "plain")}')
    board.panel({"type": "table", "title": "Runs",
                 "description": "Dataset: the Data section above describes it. Configuration: the largest "
                                "relative error the approximate plan may have, whether raw samples are kept so a "
                                "query can still read them, and whether the planner may pick a plan that cannot "
                                "promise that error target. Policy: what a rule does when an answer's error bound "
                                "leaves its check open (plain ignores the bound, bound takes no action, rerun "
                                "asks the exact plan).",
                 "targets": [iq("A", INDEX, runs,
                                [col(k, k, "number" if k in ("start", "end") else "string") for k in RUN_FIELDS])],
                 "fieldConfig": {"defaults": {}, "overrides": [
                     {"matcher": {"id": "byRegexp", "options": "start|end"},
                      "properties": [{"id": "unit", "value": "dateTimeAsIso"}]},
                     {"matcher": {"id": "byName", "options": "run_id"},
                      "properties": [{"id": "links", "value": [{
                          "title": "Open this run",
                          "url": f"/d/{uid}/{uid}?var-run=${{__data.fields.run_id}}"
                                 "&from=${__data.fields.start}&to=${__data.fields.end}"}]}]}]},
                 "transformations": order(*RUN_FIELDS),
                 "options": {"showHeader": True}}, x, y, w, h)


def plan_fields(plan):
    return (f'"sketch": $type({plan}) = "string" ? $substringBefore({plan}, " ") : "", '
            f'"parameters": $type({plan}) = "string" ? ($contains({plan}, "{{") ? '
            f'$replace($replace($substringBefore($substringAfter({plan}, ":{{"), "}}}}"), "\\"", ""), ",", ", ") '
            ': "") : ""')


def plans_panel(board, x, y, w, h):
    each = ('( $e := $[run_id="${run}"]; $append('
            '$e.details.plans ~> $each(function($v, $k) { {"query": $k, ' + plan_fields("$v") + ', '
            '"exact plan": $lookup($e.details.exact_plans, $k), "promql": $lookup($e.details.promql, $k)} }), '
            '$e.details.unsupported ~> $each(function($v, $k) { {"query": $k, "why no approx plan": $v, '
            '"promql": $lookup($e.details.promql, $k)} })) )')
    board.panel({"type": "table", "title": "Plans in ${run}",
                 "targets": [iq("A", INDEX, each,
                                [col("query", "query", "string"), col("sketch", "sketch", "string"),
                                 col("parameters", "parameters", "string"),
                                 col("exact plan", "exact plan", "string"),
                                 col("why no approx plan", "why no approx plan", "string"),
                                 col("promql", "promql", "string")])],
                 "transformations": order("query", "sketch", "parameters", "exact plan", "why no approx plan",
                                          "promql"),
                 "fieldConfig": {"defaults": {"custom": {"cellOptions": {"type": "auto", "wrapText": True}}},
                                 "overrides": []},
                 "options": {"showHeader": True}}, x, y, w, h)


def run_variable(use_cases):
    return {"name": "run", "label": "run", "type": "query", "datasource": DS, "refresh": 1,
            "query": {"queryType": "infinity", "query": "", "refId": "variable",
                      "infinityQuery": iq("variable", INDEX, f"$[use_case in {in_list(use_cases)}{synthetic_only()}]",
                                          [col("run_id", "run_id", "string")])}}


def variable(name, url, root, selector, empty="[]"):
    return {"name": name, "label": name, "type": "query", "datasource": DS, "refresh": 1,
            "query": {"queryType": "infinity", "query": "", "refId": "variable",
                      "infinityQuery": iq("variable", url, root, [col(selector, name, "string")], empty)}}


def dashboard(uid, title, board, variables, annotations, time_range):
    return {
        "uid": uid,
        "title": title,
        "tags": ["approx-rows"],
        "timezone": "utc",
        "editable": False,
        "schemaVersion": 41,
        "time": time_range,
        "templating": {"list": variables},
        "annotations": {"list": [
            {"name": "Annotations & Alerts", "builtIn": 1, "enable": True, "hide": True,
             "iconColor": "rgba(0, 211, 255, 1)",
             "datasource": {"type": "grafana", "uid": "-- Grafana --"}, "type": "dashboard"},
            *annotations]},
        "panels": board.panels,
    }


def part(board, variables=(), annotations=()):
    return {"panels": board.panels, "variables": list(variables), "annotations": list(annotations)}


def runs_part(uid, use_case):
    b = Board()
    b.row("Runs", 0)
    runs_panel(b, uid, [use_case], 0, 1, 12, 7)
    plans_panel(b, 12, 1, 12, 7)
    return part(b, [run_variable([use_case])])


def figure_part(figure):
    b = Board()
    b.row("Scenario", 0)
    b.panel({"type": "text", "title": "", "transparent": True,
             "options": {"mode": "html",
                         "content": f'<img src="{FIGURES}{figure}" style="height: 100%; max-width: 100%">'}},
            0, 1, 24, 13)
    return part(b)


def number(v):
    if isinstance(v, (int, float)) and float(v).is_integer() and abs(v) < 1e15:
        return f"{int(v):,}"
    return f"{v:g}"


def distribution(d, whole=False):
    kind = d["kind"]
    if kind == "zipf":
        return f"Zipf distribution (skew {number(d['skewness'])}) over {number(d['population_size'])} values"
    if kind == "normal":
        return (f"normal distribution (mean {number(d['mean'])}, standard deviation "
                f"{number(d['standard_deviation'])})")
    if kind == "uniform":
        low, high = d["lower_bound"], d["upper_bound"]
        if whole and high - low == 1 and float(low).is_integer():
            return f"always {number(low)}"
        if whole:
            return f"uniform over the whole numbers {number(low)} to {number(-(-high // 1) - 1)}"
        return f"uniform between {number(low)} and {number(high)}"
    if kind == "pareto":
        return f"Pareto distribution (alpha {number(d['alpha'])}, scale {number(d['scale'])})"
    return json.dumps(d)


def span(ms):
    seconds = ms / 1000
    return f"{number(seconds)} s" if seconds < 120 else f"{number(round(seconds / 60, 1))} minutes"


def interval(ms):
    return f"{number(ms)} ms" if ms < 1000 else f"{number(round(ms / 1000, 2))} s"


def values_between(d, whole):
    text = distribution(d, whole)
    if text.startswith("always "):
        return text[len("always "):]
    if d["kind"] == "uniform":
        return "random values, " + text
    return "drawn from a " + text


def change_text(change, whole=False):
    out = []
    for kind, c in change.items():
        if kind == "mix":
            out.append(f"{number(round(c['share'] * 100, 3))}% of samples are "
                       f"{values_between(c['distribution'], whole)}")
        elif kind == "rate":
            out.append(f"{number(c['factor'])}× as many samples per second")
        elif kind == "scale":
            out.append(f"values multiplied by {number(c['factor'])}")
        else:
            out.append(f"{kind} {json.dumps(c)}")
    return ", ".join(out)


def minutes(ms):
    value = ms / 60000
    return f"{value:g} min" if value.is_integer() else f"{value:.1f} min"


def shown(value, count):
    if count > 1 and "{i}" in str(value):
        return f"{value.replace('{i}', '0')} to {value.replace('{i}', str(count - 1))}"
    return str(value)


def groups(metric):
    return [(s.get("labels", {}), s.get("count", 1), s) for s in metric["series"]]


def who(members, everyone):
    total = sum(c for _, c, _ in everyone)
    count = sum(c for _, c, _ in members)
    if count == total:
        return ""
    if len(members) == 1:
        labels, n, _ = members[0]
        return " on " + ", ".join(f"{k} {shown(v, n)}" for k, v in labels.items()
                                  if any(l.get(k) != v for l, _, _ in everyone))
    if len(members) <= 3:
        names = [who([m], everyone)[len(" on "):] for m in members]
        return " on " + (names[0] if len(names) == 1 else ", ".join(names[:-1]) + " and " + names[-1])
    shared = set(members[0][0].items()).intersection(*(l.items() for l, _, _ in members[1:]))
    shared -= set(everyone[0][0].items()).intersection(*(l.items() for l, _, _ in everyone[1:]))
    return f" on the {count} series" + (" where " + " and ".join(f"{k} is {v}" for k, v in sorted(shared))
                                         if shared else "")


def events(metric, duration_ms):
    found = {}
    for labels, count, s in groups(metric):
        for p in s.get("phases", []):
            key = (p["start_offset_ms"], p["end_offset_ms"], p.get("ramp_ms", 0), p.get("stagger_ms", 0))
            found.setdefault(key, {}).setdefault(change_text(p["change"], bool(metric.get("floor"))),
                                                 []).append((labels, count, s))
    everyone = groups(metric)
    out = []
    for (start, end, ramp, stagger), texts in sorted(found.items()):
        when_text = (("Throughout" if start == 0 and end >= duration_ms else
                      f"Minutes {number(round(start / 60000, 1))} to "
                      + ("the end" if end >= duration_ms else number(round(end / 60000, 1))))
                     + (f", ramping in and out over {span(ramp)}" if ramp else "")
                     + (f", each series starting {span(stagger)} after the previous one" if stagger else ""))
        out.append((start, min(end, duration_ms), when_text + ": "
                    + "; ".join(t + who(m, everyone) for t, m in texts.items())))
    return out


def baselines(metric):
    whole = bool(metric.get("floor"))
    everyone = groups(metric)
    by_kind = {}
    for labels, count, s in everyone:
        b = {k: v for k, v in s["baseline"].items() if k != "seed"}
        by_kind.setdefault(b["kind"], []).append((labels, count, b))
    out = []
    for kind, found in by_kind.items():
        members = [(l, c, None) for l, c, _ in found]
        distinct = {json.dumps(b, sort_keys=True): (l, c, b) for l, c, b in found}
        if len(distinct) == 1:
            out.append(distribution(found[0][2], whole) + who(members, everyone))
        elif len(distinct) <= 3:
            for text in dict.fromkeys(distribution(b, whole) for _, _, b in found):
                out.append(text + who([(l, c, None) for l, c, b in found if distribution(b, whole) == text],
                                      everyone))
        else:
            keys = [k for k in found[0][2] if k != "kind" and len({json.dumps(b.get(k)) for _, _, b in found}) > 1]
            ranges = ", ".join(f"{k.replace('_', ' ')} from {number(min(b[k] for _, _, b in found))} to "
                               f"{number(max(b[k] for _, _, b in found))}" for k in keys)
            out.append(f"{kind}, {ranges} depending on the series" + who(members, everyone))
    return out


def label_values(metrics):
    values = {}
    for m in metrics:
        for labels, count, _ in groups(m):
            for k, v in labels.items():
                values.setdefault(k, {})[v] = count
    out = {}
    for k, found in values.items():
        templated = {v: c for v, c in found.items() if c > 1 and "{i}" in str(v)}
        items = [shown(v, c) + (f" ({c})" if v in templated else "") for v, c in found.items()]
        total = sum(templated.values()) + len(found) - len(templated)
        listed = ", ".join(items) if len(items) <= 6 else ", ".join(items[:3]) + f" and {len(items) - 3} more"
        out[k] = listed if total == 1 else f"{total} values: {listed}"
    return out


def notes(metric):
    out = [text for key, text in (("floor", "whole numbers"), ("non_negative", "never below 0")) if metric.get(key)]
    if metric.get("label_from_value"):
        out.append(f"each sample's value is stored as the label {metric['label_from_value']}")
    if metric.get("max_distinct_per_evaluation"):
        out.append(f"the planner is told to expect at most {number(metric['max_distinct_per_evaluation'])} "
                   "distinct values per evaluation")
    return out


def rates(metrics):
    def every(ms):
        return f"one sample every {interval(ms)} ({number(round(1000 / ms, 2))} per second)"

    def text(m):
        found = [(l, c, s, s.get("sample_interval_ms", m["sample_interval_ms"])) for l, c, s in groups(m)]
        ms = sorted({f[3] for f in found})
        if len(ms) == 1:
            return every(ms[0]) + (" per series" if sum(f[1] for f in found) > 1 else "")
        if len(found) <= 3:
            return "; ".join(every(i) + who([(l, c, s)], groups(m)) for l, c, s, i in found)
        return f"one sample every {interval(ms[0])} to {interval(ms[-1])}, depending on the series"
    texts = {m["name"]: text(m) for m in metrics}
    if len(set(texts.values())) == 1:
        return [next(iter(texts.values())).capitalize()]
    return [f"{name}: {t}" for name, t in texts.items()]


def real_datasets(module):
    stem = module.CASE["spec"][:-len(".json")]
    out = []
    for path in sorted(glob.glob(os.path.join(cases.WORKLOADS, stem + "_real*.json"))):
        suffix = os.path.basename(path)[len(stem) + 1:-len(".json")].split("_", 1)
        name = " ".join([suffix[0], suffix[1].replace("_", "-")]) if len(suffix) > 1 else suffix[0]
        with open(path) as f:
            out.append((name, json.load(f)))
    return out


def cell(text):
    return str(text).replace("|", "\\|").replace("\n", " ")


def data_markdown(module):
    spec = cases.spec_of(module)
    about = module.DASHBOARD["data"]
    meaning = about["columns"]
    metrics = spec["metrics"]
    series = sum(c for m in metrics[:1] for _, c, _ in groups(m))
    lines = [f"{about['summary']} The synthetic data runs {number(spec['duration_ms'] / 60000)} minutes "
             f"with {series} series per metric.", "",
             "| column | what it is | values |", "|---|---|---|",
             f"| time | Time of the sample | {'<br>'.join(rates(metrics))} |"]
    for k, v in label_values(metrics).items():
        lines.append(f"| {k} | {cell(meaning.get(k, 'label'))} | {cell(v)} |")
    for m in metrics:
        normal = "Usual values: " + "; ".join(baselines(m))
        rows = ([normal] + [text for _, _, text in events(m, spec["duration_ms"])]
                + (["Note: " + "; ".join(notes(m))] if notes(m) else []))
        lines.append(f"| {m['name']} | {cell(meaning.get(m['name'], 'value'))} | {cell('<br>'.join(rows))} |")
    if REAL_DATA:
        lines += ["", "**Real data**: the same columns read from a recorded trace, with no injected events.", ""]
        for name, real in real_datasets(module):
            sources = []
            for m in real["metrics"]:
                table = m.get("import", {}).get("table")
                if table:
                    sources.append(f"{m['name']} from column {table['value_column']} of {table['dataset']}"
                                   + (f", time stretched {number(m['import']['time_scale'])}×"
                                      if m["import"].get("time_scale", 1) != 1 else ""))
                else:
                    sources.append(f"{m['name']} generated as in the synthetic data")
            lines.append(f"- {name}: {number(round(real['duration_ms'] / 60000, 1))} minutes; " + "; ".join(sources))
    return "\n".join(lines)


def query_lines(spec):
    lines = ["| query | PromQL | computed |", "|---|---|---|"]
    for q in spec["queries"]:
        lines.append(f"| {q['id']} | `{cell(q['promql'])}` | every {number(q['interval_ms'] / 1000)} s over "
                     f"the last {number(q['window_ms'] / 1000)} s |")
    return lines


def rule_catalog(module):
    spec = cases.spec_of(module)
    out = []
    if module.CASE["dashboard"] == "threshold-alert":
        for q in spec["queries"]:
            if "threshold" in q:
                unit = q.get("unit", "")
                limit = number(q["threshold"]) + (f" {unit}" if unit and unit != "entropy" else "")
                source = ("one of the PromSketch thresholds, taken one query at a time"
                          if q["id"] in getattr(module, "THRESHOLDS", {}) else "threshold ours")
                out.append({"rule": q["id"], "name": f"{q['id']} > {number(q['threshold'])}", "uses": q["id"],
                            "does": f"Fires at every evaluation where {q['id']} is above {limit}, with no waiting "
                                    f"period ({source})"})
    decision = getattr(module, "DECISION", None)
    if decision:
        r = decision["rule"]
        out.append({"rule": decision["signal"], "name": decision["signal"],
                    "uses": f"{decision['cpu']}, {decision['memory']}",
                    "does": f"Scale a container up when its p95 CPU > {number(r['cpu_up'])} cores or p95 memory > "
                            f"{number(r['memory_up'] / 1e9)} GB; scale it down when p95 CPU < {number(r['cpu_down'])} "
                            f"cores and p95 memory < {number(r['memory_down'] / 1e9)} GB"})
    if module.CASE["dashboard"] == "topk":
        out.append({"rule": "top_clients", "name": "top_clients", "uses": "top10_clients",
                    "does": "A client is capped while it is in the top 10 list"})
    if module.CASE["dashboard"] == "rollout":
        for row in rule_descriptions().split("\n")[4:]:
            name, uses, fails, decides = [c.strip() for c in row.strip("| ").split(" | ")]
            out.append({"rule": name, "name": name, "uses": uses,
                        "does": f"A check fails when {fails}. A rollout rolls back when {decides}; otherwise it "
                                "promotes."})
    else:
        for name, rule in module.RULES.items():
            out.append({"rule": name, "name": name, "uses": ", ".join(rule["queries"]), "does": rule["description"]})
    return out


def rule_lines(module):
    return ["| rule | queries it uses | what it does |", "|---|---|---|"] + [
        f"| {r['name']} | {r['uses']} | {cell(r['does'])} |" for r in rule_catalog(module)]


def rule_variable_name(module):
    return "rule" if module.CASE["dashboard"] == "rollout" else "signal"


def rule_note_part(module):
    name = rule_variable_name(module)
    fields = ("rule", "queries it uses", "what it does")
    target = iq("A", "", f'$[rule="${{{name}}}"].{{"rule": name, "queries it uses": uses, "what it does": does}}',
                [col(k, k, "string") for k in fields])
    target.update(source="inline", data=json.dumps(rule_catalog(module)), url="")
    b = Board()
    b.panel({"type": "table", "title": "What the selected rule does",
             "targets": [target], "transformations": order(*fields),
             "fieldConfig": {"defaults": {"custom": {"cellOptions": {"type": "auto", "wrapText": True}}},
                             "overrides": [{"matcher": {"id": "byRegexp", "options": "^(rule|queries it uses)$"},
                                            "properties": [{"id": "custom.width", "value": 200}]}]},
             "options": {"showHeader": True}}, 0, 0, 24, 5)
    return part(b)


TEXT_WIDTH, CHAR, LINE, CELL_PAD, BLOCK_GAP = 979, 7.0, 21, 16, 16


def table_height(rows):
    cells = [[c.strip() for c in r.strip().strip("|").split(" | ")] for r in rows]
    columns = range(max(len(r) for r in cells))
    longest = [max(len(r[i]) if i < len(r) else 0 for r in cells) * CHAR + CELL_PAD for i in columns]
    shortest = [max(max((len(w) for w in r[i].split()), default=0) if i < len(r) else 0 for r in cells) * CHAR
                + CELL_PAD for i in columns]
    if sum(longest) <= TEXT_WIDTH:
        widths = longest
    else:
        room = max(TEXT_WIDTH - sum(shortest), 0)
        slack = sum(l - m for l, m in zip(longest, shortest)) or 1
        widths = [m + room * (l - m) / slack for l, m in zip(longest, shortest)]
    return sum(max(-(-len(c) * CHAR // max(w - CELL_PAD, CHAR)) for c, w in zip(r, widths)) * LINE
               + CELL_PAD + 1 for r in cells)


def markdown_rows(text):
    pixels, table = 0, []
    for line in text.split("\n") + [""]:
        if line.startswith("|"):
            if not line.startswith("|---"):
                table.append(line)
            continue
        if table:
            pixels += table_height(table) + BLOCK_GAP
            table = []
        if line.strip():
            pixels += -(-len(line) * CHAR // TEXT_WIDTH) * LINE + BLOCK_GAP
    return int(-(-(pixels * 1.1 + 60) // 38))


def about_part(module):
    spec = cases.spec_of(module)
    b = Board()
    data = data_markdown(module)
    height = markdown_rows(data)
    b.row("Data", 0)
    b.panel({"type": "text", "title": "", "options": {"mode": "markdown", "content": data}}, 0, 1, 24, height)
    rules = "\n".join(["Each query runs twice, once with the exact plan and once with the approximate plan. "
                       "A rule reads query results and decides an action; exact and approx decide separately, "
                       "and the dashboard compares their decisions. On the time charts below, shaded areas are "
                       "the problem periods injected into the synthetic data.", "", "**Queries**", ""]
                      + query_lines(spec) + ["", "**Rules**", ""] + rule_lines(module))
    b.row("Queries and rules", 1 + height)
    b.panel({"type": "text", "title": "", "options": {"mode": "markdown", "content": rules}},
            0, 2 + height, 24, markdown_rows(rules))
    return part(b, annotations=[change_annotation(spec)])


def change_annotation(spec):
    found = []
    for m in spec["metrics"]:
        for start, end, text in events(m, spec["duration_ms"]):
            if start == 0 and end >= spec["duration_ms"]:
                continue
            found.append({"time": spec["start_ms"] + start, "timeEnd": spec["start_ms"] + end,
                          "text": f"{m['name']}: {text}"})
    root = (f'( $e := $[run_id="${{run}}" and $not($contains(run_id, "-real"))]; '
            f'$exists($e) ? {json.dumps(found)} : [] )')
    return {"name": "problem periods", "enable": True, "hide": True, "iconColor": "#8AB8FF", "datasource": DS,
            "target": iq("Anno", INDEX, root, [T, col("timeEnd", "timeEnd", "timestamp_epoch"),
                                                col("text", "text", "string")])}


def by_metric_columns(*pairs):
    return [col(selector, text, "number") for selector, text in pairs]


def sort_limit(field, limit):
    return [{"id": "sortBy", "options": {"sort": [{"field": field, "desc": True}]}},
            {"id": "limit", "options": {"limitField": limit}}]


def alert_dashboard(module):
    sel = 'labels.signal="${signal}"'
    b = Board()

    uses = {r["rule"]: [q.strip() for q in r["uses"].split(",")] for r in rule_catalog(module)}
    of_rule = f'labels.query in $lookup({json.dumps(uses)}, "${{signal}}")'
    b.row("Query result", 8)
    b.panel({"type": "timeseries", "title": "Queries behind ${signal}: exact vs approx",
             "targets": [rows_of("A", "query_result", f'{of_rule} and labels.arm="exact"', "exact", by="query"),
                         rows_of("B", "query_result", f'{of_rule} and labels.arm="approx"', "approx", by="query"),
                         rows_of("C", "app_state", f'{sel} and metric="threshold"', "threshold")],
             "fieldConfig": {"defaults": {**LINES, "displayName": "${__field.labels.query} ${__field.name}",
                                          "custom": {**LINES["custom"], "scaleDistribution": {"type": "log", "log": 10}}},
                             "overrides": [by_ref("A", "blue", 2), by_ref("B", "orange"), dashed("C", "red")]
                             + named("threshold")},
             "options": LEGEND}, 0, 9, 24, 9)
    b.panel({"type": "timeseries", "title": "Relative error of approx",
             "targets": [rows_of("A", "low_level", f'{of_rule} and metric="relative_error"', "relative_error",
                                 by="query")],
             "fieldConfig": {"defaults": {**LINES, "unit": "percentunit", "displayName": "${__field.labels.query}"},
                             "overrides": []},
             "options": LEGEND}, 0, 18, 24, 6)

    b.row("Application: alert", 24)
    b.panel(strip_panel("${signal}: alert per evaluation, with what differs",
                        [(f'{sel} and metric="alert" and labels.arm="exact"', "exact alert"),
                         (f'{sel} and metric="alert" and labels.arm="approx"', "approx alert"),
                         (f'{sel} and metric="annotation"', "annotation")],
                        {"firing": {"color": "red", "text": "firing"}, "ok": OFF}), 0, 25, 24, 7)

    b.row("Fidelity", 32)
    summary = RUN + "fidelity_summary.json"
    by_metric = metric_values(sel)
    b.panel(outcome_stat("Evaluations by outcome", summary, sel), 0, 33, 6, 8)
    b.panel({"type": "stat", "title": "Episodes",
             "targets": [iq("A", summary, by_metric, by_metric_columns(
                 ("episodes", "exact episodes"), ("missed_episodes", "missed episodes"),
                 ("mean_onset_delay", "mean onset delay (evaluations)"),
                 ("mean_absolute_end_delay", "mean absolute end delay (evaluations)"),
                 ("early_ends", "early ends"), ("late_ends", "late ends"), ("gaps", "gaps"),
                 ("episode_coverage", "episode coverage")))],
             "transformations": order("exact episodes", "missed episodes", "mean onset delay (evaluations)",
                                      "mean absolute end delay (evaluations)", "early ends", "late ends", "gaps",
                                      "episode coverage"),
             "fieldConfig": {"defaults": TEXT, "overrides": [
                 {"matcher": {"id": "byName", "options": "episode coverage"},
                  "properties": [{"id": "unit", "value": "percentunit"}, {"id": "decimals", "value": 1}]}]},
             "options": STAT}, 6, 33, 6, 8)
    b.panel({"type": "table", "title": "Confusion matrix (evaluations)",
             "targets": [iq("A", summary,
                            metric_values(sel, '['
                            '{"exact": "firing", "approx firing": $s.true_positive, "approx ok": $s.false_negative}, '
                            '{"exact": "ok", "approx firing": $s.false_positive, "approx ok": $s.true_negative}]'),
                            [col("exact", "exact", "string"), col("approx firing", "approx firing", "number"),
                             col("approx ok", "approx ok", "number")])],
             "transformations": order("exact", "approx firing", "approx ok"),
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": []},
             "options": {"showHeader": True}}, 12, 33, 6, 8)
    b.panel({"type": "table", "title": "Where approx ≠ exact for ${signal}",
             "targets": [iq("A", RUN + "fidelity_event.json",
                            f"$[{sel}]",
                            [T, col("metric", "event", "string"), col("value", "evaluations", "number")])],
             "transformations": order("time", "event", "evaluations"),
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": []},
             "options": {"showHeader": True}}, 18, 33, 6, 8)

    return part(b, [variable("signal", summary, '$[metric="episodes"].labels', "signal")])


def decision_dashboard(uid, module):
    scale = module.DECISION
    here = 'labels.container="${container}"'
    overall = 'labels.app="autoscaler" and $not($exists(labels.container))'
    summary = RUN + "fidelity_summary.json"
    b = Board()
    b.row("Query result for ${container}", 8)
    for i, (query, title, unit, low, high) in enumerate(((scale["cpu"], "p95 CPU", "none", scale["rule"]["cpu_down"],
                                                          scale["rule"]["cpu_up"]),
                                                         (scale["memory"], "p95 memory", "bytes",
                                                          scale["rule"]["memory_down"], scale["rule"]["memory_up"]))):
        sel = f'{here} and labels.query="{query}"'
        results = entity_file("query_result", "container_in_run")
        b.panel({"type": "timeseries", "title": f"{title} of ${{container}}: exact vs approx",
                 "targets": [rows_of("A", results, f'{sel} and labels.arm="exact"', "exact"),
                             rows_of("B", results, f'{sel} and labels.arm="approx"', "approx")],
                 "fieldConfig": {"defaults": {**LINES, "unit": unit,
                                              "custom": {**LINES["custom"], "thresholdsStyle": {"mode": "dashed"}},
                                              "thresholds": {"mode": "absolute", "steps": [
                                                  {"color": "blue", "value": None}, {"color": "green", "value": low},
                                                  {"color": "red", "value": high}]}},
                                 "overrides": [by_ref("A", "blue", 2), by_ref("B", "orange")] + named("exact", "approx")},
                 "options": LEGEND}, 12 * i, 9, 12, 8)
    for i, (metric, title) in enumerate((("relative_error", "Relative error"), ("rank_error", "Rank error"))):
        b.panel({"type": "timeseries", "title": title + " of approx for ${container}",
                 "targets": [rows_of("A", entity_file("low_level", "container_in_run"), f'{here} and metric="{metric}"',
                                     metric, by="query")],
                 "fieldConfig": {"defaults": {**LINES, "unit": "percentunit", "displayName": "${__field.labels.query}"},
                                 "overrides": []},
                 "options": LEGEND}, 12 * i, 17, 12, 6)

    b.row("Application: autoscaler", 23)
    scaling = {"up": {"color": "red", "text": "scale up"}, "down": {"color": "blue", "text": "scale down"}}
    b.panel(strip_panel("${container}: scaling action per evaluation, with what differs",
                        [(f'{here} and metric="action" and labels.arm="exact"', "exact action"),
                         (f'{here} and metric="action" and labels.arm="approx"', "approx action"),
                         (f'{here} and metric="annotation"', "annotation")], scaling,
                        entity_file("app_state", "container_in_run")), 0, 24, 24, 6)
    b.panel(strip_panel("${container}: scaling decision per evaluation",
                        [(f'{here} and metric="decision" and labels.arm="exact"', "exact decision"),
                         (f'{here} and metric="decision" and labels.arm="approx"', "approx decision")],
                        {**scaling, "none": {"color": "green", "text": "no change"}},
                        entity_file("app_state", "container_in_run")), 0, 30, 24, 5)

    b.row("Fidelity over all containers", 35)
    b.panel(outcome_stat("Scaling outcomes", summary, overall,
                         {"true_positive": "actions", "false_positive": "actions", "false_negative": "actions",
                          "true_negative": "container-evaluations"}), 0, 36, 6, 7)
    fields = (("true_positive", "matched"), ("early_actions", "early"), ("late_actions", "late"),
              ("false_positive", "false"), ("false_negative", "missed"),
              ("mean_absolute_onset_delay", "mean absolute onset delay"))
    per_action = metric_values(overall, '["up", "down"].( $a := $; {"action": $a, '
                               + ", ".join(f'"{text}": $lookup($s, $a & "_{metric}")' for metric, text in fields)
                               + "} )")
    b.panel({"type": "table", "title": "Scaling actions by kind",
             "targets": [iq("A", summary, per_action, [col("action", "action", "string")]
                            + [col(text, text, "number") for _, text in fields])],
             "transformations": order("action", *[text for _, text in fields]),
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": []},
             "options": {"showHeader": True}}, 6, 36, 7, 7)
    matrix = metric_values(overall, '["up", "down", "none"].( $e := $; {"exact": $e, '
                           + ", ".join(f'"approx {a}": $lookup($s, "exact_" & $e & "_approx_{a}")'
                                       for a in ("up", "down", "none"))
                           + "} )")
    b.panel({"type": "table", "title": "Decisions, exact by approx (container-evaluations)",
             "targets": [iq("A", summary, matrix, [col("exact", "exact", "string")]
                            + [col(f"approx {a}", f"approx {a}", "number") for a in ("up", "down", "none")])],
             "transformations": order("exact", "approx up", "approx down", "approx none"),
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": []},
             "options": {"showHeader": True}}, 13, 36, 6, 7)
    b.panel(disagreement_panel(uid, "container", "container", "Containers with the most action disagreements"), 19, 36, 5, 7)
    b.panel({"type": "table", "title": "Where approx ≠ exact for scale_decision on ${container}",
             "targets": [iq("A", RUN + "fidelity_event.json", f"$[{here}]",
                            [T, col("labels.action", "action", "string"), col("metric", "event", "string"),
                             col("value", "evaluations", "number")])],
             "transformations": order("time", "action", "event", "evaluations"),
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": []},
             "options": {"showHeader": True}}, 0, 43, 24, 6)

    container = by_disagreements("container", "container")
    container["label"] = "container (scale_decision)"
    return part(b, [container, in_run("container_in_run", "container", "container")])


DECISION_MARKS = {"failed check": ("#FF9830", 5), "rollback": ("red", 14), "promote": ("green", 14)}
DECISION_LEVEL = {"exact": 101, "approx": 100}


def rule_descriptions():
    r = rollout
    every = {name: rule["every"] for name, rule in r.RULES.items()}
    lines = [
        "Every minute, each query is evaluated over the last 5 minutes. A rule turns the query results "
        "into a pass or fail per check, then decides rollback or promote per rollout. Exact and approx decide "
        "separately from their own results.",
        "",
        "| rule | queries | a check fails when | a rollout rolls back when, else promotes |",
        "|---|---|---|---|",
        f"| ours | p99_canary, p99_stable | canary p99 / stable p99 > {r.TOLERANCE} | {r.STREAK} failed checks in a "
        f"row out of {r.CHECKS} |",
        f"| flagger | p99_canary, success_rate_canary | canary p99 > {r.MAX_P99} s or canary success rate < "
        f"{r.FLAGGER_MIN_SUCCESS_RATE:.0%} | {r.FAILURE_THRESHOLD} failed checks in total; promotes after "
        f"{r.PASSES_TO_PROMOTE} passed checks (up to {r.RULES['flagger']['checks']} checks) |",
        f"| argo_rollouts | success_rate_canary | canary success rate < {r.ARGO_ROLLOUTS_MIN_SUCCESS_RATE:.0%} "
        f"(a check every {every['argo_rollouts']} evaluations) | more than {r.FAILURE_LIMIT} failed checks out of "
        f"{r.ARGO_ROLLOUTS_COUNT} |",
        f"| cloudwatch | p99_canary | canary p99 > {r.MAX_P99} s | {r.BREACHING_POINTS} of the last "
        f"{r.EVALUATED_POINTS} checks failed, within {r.CLOUDWATCH_CHECKS} checks |",
        f"| keptn | p95_canary | canary p95 > {r.KEPTN_RELATIVE} × the mean of the last {r.KEPTN_PASSING} passing "
        f"checks, or ≥ {r.KEPTN_MAX_P95} s (a check every {every['keptn']} evaluations) | the check fails; "
        "each check is its own evaluation |",
    ]
    return "\n".join(lines)


def query_and_decision_panel(app, module):
    queries = json.dumps({name: list(rule["queries"]) for name, rule in module.RULES.items()})
    lines = (f'$[labels.query in $lookup({queries}, "${{rule}}")]'
             '.{"time": time, "value": value, "series": labels.query & " " & labels.arm}')
    level = f'labels.arm="exact" ? {DECISION_LEVEL["exact"]} : {DECISION_LEVEL["approx"]}'
    marks = (f'$[{app} and (metric="check" or metric="action")].( $level := {level}; metric="check" '
             '? [{"time": time, "value": value="failed" ? $level : 0, "series": labels.arm & " failed check"}] '
             ': [{"time": time, "value": value="rollback" ? $level : 0, "series": labels.arm & " rollback"}, '
             '{"time": time, "value": value="promote" ? $level : 0, "series": labels.arm & " promote"}] )')
    targets = []
    for ref, path, root in (("A", "query_result", lines), ("B", "app_state", marks)):
        target = iq(ref, RUN + path + ".json", root, [T, col("value", "value", "number"),
                                                       col("series", "series", "string")])
        target["format"] = "timeseries"
        targets.append(target)
    overrides = []
    for query, color in module.QUERY_COLORS.items():
        for arm in ("exact", "approx"):
            properties = [{"id": "color", "value": {"mode": "fixed", "fixedColor": color}}]
            if arm == "approx":
                properties.append({"id": "custom.lineStyle", "value": {"fill": "dash", "dash": [10, 10]}})
            else:
                properties.append({"id": "custom.lineWidth", "value": 2})
            if query.startswith("success_rate"):
                properties += [{"id": "unit", "value": "percentunit"},
                               {"id": "custom.axisPlacement", "value": "right"},
                               {"id": "custom.scaleDistribution", "value": {"type": "linear"}}]
            overrides.append({"matcher": {"id": "byRegexp", "options": f".*{query} {arm}.*"},
                              "properties": properties})
    for arm, level in DECISION_LEVEL.items():
        for mark, (color, size) in DECISION_MARKS.items():
            overrides.append({"matcher": {"id": "byRegexp", "options": f".*{arm} {mark}.*"}, "properties": [
                {"id": "color", "value": {"mode": "fixed", "fixedColor": color}},
                {"id": "custom.drawStyle", "value": "points"},
                {"id": "custom.showPoints", "value": "always"},
                {"id": "custom.spanNulls", "value": False},
                {"id": "custom.pointSize", "value": size},
                {"id": "custom.axisPlacement", "value": "hidden"},
                {"id": "unit", "value": "none"},
                {"id": "min", "value": min(DECISION_LEVEL.values()) - 9},
                {"id": "max", "value": max(DECISION_LEVEL.values()) + 0.5},
                {"id": "custom.scaleDistribution", "value": {"type": "linear"}},
                {"id": "mappings", "value": [{"type": "value", "options": {str(level): {"text": mark}}}]}]})
    return {"type": "timeseries",
            "title": "Query results of rule ${rule}, exact solid and approx dashed, with each arm's failed checks "
                     "and decisions on the same time axis",
            "description": "The top two rows of marks are the rule's checks and decisions: the upper row is "
                           "exact, the lower row approx. Small orange: a failed check. Large red: rollback. "
                           "Large green: promote. Hover a time to see every query value and mark at that time.",
            "targets": targets,
            "fieldConfig": {"defaults": {**LINES, "unit": "s", "displayName": "${__field.labels.series}",
                                         "custom": {**LINES["custom"], "spanNulls": True,
                                                    "scaleDistribution": {"type": "log", "log": 10}}},
                            "overrides": overrides},
            "options": {**LEGEND, "tooltip": {**LEGEND["tooltip"], "hideZeros": True}}}


COST_PANELS = (("query_latency", "Query latency per evaluation", "s", "Plan compile time excluded. "),
               ("query_memory", "Bytes per evaluation: sketch state plus raw samples read", "bytes", ""))
PALETTE = ("blue", "orange", "green", "purple", "red", "yellow")


def cost_panel(colors, metric, title, unit, note):
    targets, overrides = exact_solid_approx_dashed("low_level", colors, metric)
    return {"type": "timeseries", "title": f"{title}: exact vs approx",
            "description": note + "Trials of an arm share one exact run.", "targets": targets,
            "fieldConfig": {"defaults": {**LINES, "unit": unit, "displayName": "${__field.name}",
                                         "custom": {**LINES["custom"],
                                                    "scaleDistribution": {"type": "log", "log": 10}}},
                            "overrides": overrides},
            "options": LEGEND}


def cost_dashboard(module):
    colors = dict(zip((q["id"] for q in cases.spec_of(module)["queries"]), PALETTE))
    b = Board()
    b.row("Query cost", 0)
    for i, cost in enumerate(COST_PANELS):
        b.panel(cost_panel(colors, *cost), 12 * i, 1, 12, 8)
    return part(b)


def rollout_dashboard(module):
    app = 'labels.app="rollout_controller" and labels.signal="${rule}"'
    summary = RUN + "fidelity_summary.json"
    b = Board()
    b.row("Query result", 8)
    for x, w, title, queries, unit in ((0, 16, "p99 latency of canary and stable", ("p99_canary", "p99_stable"), "s"),
                                       (16, 8, "Success rate of canary", ("success_rate_canary",), "percentunit")):
        targets, overrides = exact_solid_approx_dashed("query_result", {q: module.QUERY_COLORS[q] for q in queries})
        b.panel({"type": "timeseries", "title": f"{title}: exact vs approx", "targets": targets,
                 "fieldConfig": {"defaults": {**LINES, "unit": unit, "displayName": "${__field.name}"},
                                 "overrides": overrides},
                 "options": LEGEND}, x, 9, w, 8)
    ranked = ("p99_canary", "p99_stable")
    b.panel({"type": "timeseries", "title": "Rank error of approx",
             "targets": [rows_of(q, "low_level", f'metric="rank_error" and labels.query="{q}"', q) for q in ranked],
             "fieldConfig": {"defaults": {**LINES, "unit": "percentunit", "displayName": "${__field.name}"},
                             "overrides": [by_ref(q, module.QUERY_COLORS[q]) for q in ranked]},
             "options": LEGEND}, 0, 17, 12, 6)
    for (x, y, w), cost in zip(((12, 17, 12), (0, 23, 24)), COST_PANELS):
        b.panel(cost_panel(module.QUERY_COLORS, *cost), x, y, w, 6)

    b.row("Application: rollout controller, rule ${rule}", 29)
    ratio = 'labels.app="rollout_controller"'
    b.panel({"type": "timeseries", "title": "Canary p99 over stable p99 (rule ours compares it with the tolerance)",
             "targets": [rows_of("A", "app_state", f'{ratio} and metric="ratio" and labels.arm="exact"', "exact"),
                         rows_of("B", "app_state", f'{ratio} and metric="ratio" and labels.arm="approx"', "approx"),
                         rows_of("C", "app_state", f'{ratio} and metric="tolerance"', "tolerance")],
             "fieldConfig": {"defaults": LINES,
                             "overrides": [by_ref("A", "blue", 2), by_ref("B", "orange"), dashed("C", "red")]
                             + named("exact", "approx", "tolerance")},
             "options": LEGEND}, 0, 36, 24, 7)
    b.panel(strip_panel("Check result and rollout decision per check, with what differs",
                        [(f'{app} and metric="check" and labels.arm="exact"', "exact check"),
                         (f'{app} and metric="check" and labels.arm="approx"', "approx check"),
                         (f'{app} and metric="action" and labels.arm="exact"', "exact action"),
                         (f'{app} and metric="action" and labels.arm="approx"', "approx action"),
                         (f'{app} and metric="annotation"', "annotation")],
                        {"failed": {"color": "red", "text": "failed"}, "passed": OFF,
                         "rollback": {"color": "red", "text": "rollback"},
                         "promote": {"color": "green", "text": "promote"}}), 0, 43, 24, 9)

    b.row("Query result and decision on one time axis, rule ${rule}", 52)
    b.panel(query_and_decision_panel(app, module), 0, 53, 24, 12)

    b.row("Fidelity over rollouts", 65)
    b.panel(outcome_stat("Rollouts by outcome (positive: rollback)", summary, app), 0, 66, 6, 8)
    b.panel({"type": "stat", "title": "Rollouts",
             "targets": [iq("A", summary, metric_values(app), by_metric_columns(
                 ("rollouts", "rollouts"), ("agreement", "same decision"),
                 ("mean_absolute_rollback_delay", "mean absolute rollback delay (evaluations)"),
                 ("early_rollbacks", "early rollbacks"), ("late_rollbacks", "late rollbacks"),
                 ("gaps", "gaps in failed checks")))],
             "transformations": order("rollouts", "same decision", "mean absolute rollback delay (evaluations)",
                                      "early rollbacks", "late rollbacks", "gaps in failed checks"),
             "fieldConfig": {"defaults": TEXT, "overrides": [
                 {"matcher": {"id": "byName", "options": "same decision"},
                  "properties": [{"id": "unit", "value": "percentunit"}, {"id": "decimals", "value": 1}]}]},
             "options": STAT}, 6, 66, 6, 8)
    b.panel({"type": "table", "title": "Rollouts",
             "targets": [iq("A", RUN + "fidelity_event.json", f'$[{app} and metric="rollout"]',
                            [T, col("labels.rollout", "rollout", "string"), col("labels.exact", "exact", "string"),
                             col("labels.approx", "approx", "string"), col("value", "outcome", "string")])],
             "transformations": order("time", "rollout", "exact", "approx", "outcome"),
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": [
                 {"matcher": {"id": "byName", "options": "outcome"},
                  "properties": [{"id": "mappings", "value": [{"type": "value", "options": {
                      value: {"text": text, "index": i} for i, (value, text) in enumerate(OUTCOMES.items())}}]}]}]},
             "options": {"showHeader": True}}, 12, 66, 7, 8)
    b.panel({"type": "table", "title": "Where approx ≠ exact for rule ${rule}",
             "targets": [iq("A", RUN + "fidelity_event.json", f'$[{app} and metric!="rollout"]',
                            [T, col("metric", "event", "string"), col("value", "evaluations", "number")])],
             "transformations": order("time", "event", "evaluations"),
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": []},
             "options": {"showHeader": True}}, 19, 66, 5, 8)
    return part(b, [variable("rule", summary, '$[metric="rollouts"].labels', "signal")])


def topk_dashboard(uid):
    app = 'labels.app="topk_set"'
    here = f'{app} and labels.client_id="${{client_in_run}}"'
    overall = f'{app} and $not($exists(labels.client_id))'
    summary = RUN + "fidelity_summary.json"
    b = Board()
    b.row("Query result", 8)
    b.panel({"type": "table", "title": "Top clients by evaluation, highest count first",
             "targets": [rows_of("A", "app_state", f'{app} and metric="ranking" and labels.arm="exact"', "exact", "string"),
                         rows_of("B", "app_state", f'{app} and metric="ranking" and labels.arm="approx"', "approx", "string")],
             "transformations": [{"id": "joinByField", "options": {"byField": "time", "mode": "outer"}},
                                 {"id": "organize", "options": {"indexByName": {"time": 0, "exact A": 1, "approx B": 2},
                                                                "excludeByName": {},
                                                                "renameByName": {"exact A": "exact", "approx B": "approx"}}}]
             + [{"id": "sortBy", "options": {"sort": [{"field": "time", "desc": True}]}}],
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": []},
             "options": {"showHeader": True}}, 0, 9, 14, 9)
    b.panel({"type": "timeseries", "title": "Relative error of matched clients' counts",
             "targets": [rows_of("A", "low_level", 'metric="relative_error"', "relative_error", by="client_id")],
             "fieldConfig": {"defaults": {**LINES, "unit": "percentunit", "displayName": "${__field.labels.client_id}",
                                          "custom": {**LINES["custom"], "showPoints": "always", "pointSize": 3}},
                             "overrides": []},
             "options": {**LEGEND, "legend": {"showLegend": False}}}, 14, 9, 10, 9)

    b.row("Application: rate limiter", 18)
    b.panel({"type": "timeseries", "title": "Clients shared by the exact and approx top lists",
             "targets": [rows_of("A", "app_state", f'{app} and metric="overlap"', "in both"),
                         rows_of("B", "app_state", f'{app} and metric="approx_only"', "approx only")],
             "fieldConfig": {"defaults": {**LINES, "min": 0},
                             "overrides": [by_ref("A", "green", 2), by_ref("B", "orange")] + named("in both", "approx only")},
             "options": LEGEND}, 0, 19, 18, 7)
    b.panel(disagreement_panel(uid, "client", "client_id", "Clients with the most disagreements"), 18, 19, 6, 7)
    strip = strip_panel("Client ${client_in_run}: capped per evaluation, with what differs",
                        [(f'{here} and metric="capped" and labels.arm="exact"', "exact capped"),
                         (f'{here} and metric="capped" and labels.arm="approx"', "approx capped"),
                         (f'{here} and metric="annotation"', "annotation")],
                        {"capped": {"color": "red", "text": "capped"}, "not capped": OFF},
                        entity_file("app_state", "client_in_run"))
    strip.update(repeat="client_in_run", repeatDirection="v")
    b.panel(strip, 0, 26, 24, 5)

    b.row("Fidelity", 31)
    b.panel(outcome_stat("Client-evaluations by outcome", summary, overall), 0, 32, 6, 8)
    by_metric = metric_values(overall)
    b.panel({"type": "stat", "title": "Top list per evaluation",
             "targets": [iq("A", summary, by_metric, by_metric_columns(
                 ("precision", "precision"), ("recall", "recall"), ("identical_sets", "identical lists"),
                 ("evaluations", "evaluations"), ("intruder_slots", "approx-only slots")))],
             "transformations": order("precision", "recall", "identical lists", "evaluations", "approx-only slots"),
             "fieldConfig": {"defaults": TEXT, "overrides": [
                 {"matcher": {"id": "byRegexp", "options": "precision|recall"},
                  "properties": [{"id": "unit", "value": "percentunit"}, {"id": "decimals", "value": 1}]}]},
             "options": STAT}, 6, 32, 6, 8)
    b.panel({"type": "stat", "title": "Clients entering the exact top list",
             "targets": [iq("A", summary, by_metric, by_metric_columns(
                 ("entries", "entries"), ("missed_entries", "missed"),
                 ("mean_onset_delay", "mean onset delay (evaluations)"), ("gaps", "gaps"),
                 ("false_member_spans", "approx-only spans")))],
             "transformations": order("entries", "missed", "mean onset delay (evaluations)", "gaps",
                                      "approx-only spans"),
             "fieldConfig": {"defaults": TEXT, "overrides": []},
             "options": STAT}, 12, 32, 6, 8)
    b.panel({"type": "table", "title": "Where approx ≠ exact for top_clients",
             "targets": [iq("A", RUN + "fidelity_event.json", f"$[{app}]",
                            [T, col("labels.client_id", "client", "string"), col("metric", "event", "string"),
                             col("value", "evaluations", "number")])],
             "transformations": order("time", "client", "event", "evaluations"),
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": []},
             "options": {"showHeader": True}}, 18, 32, 6, 8)
    client = by_disagreements("client", "client_id")
    client["label"] = "client (top_clients)"
    client.update(multi=True, includeAll=False)
    shown = in_run("client_in_run", "client_id", "client")
    shown.update(multi=True, includeAll=True, current={"text": ["All"], "value": ["$__all"]})
    return part(b, [client, shown])


def rules_dashboard(uid, noun):
    app = 'labels.app="rule" and labels.signal="${signal}"'
    overall = f'{app} and $not($exists(labels.entity))'
    here = f'{app} and labels.entity="${{entity_in_run}}"'
    summary = RUN + "fidelity_summary.json"
    b = Board()
    b.row("Rules in ${run}", 8)
    columns = (("agreement", "agreement"), ("true_positive", "true positive"),
               ("false_positive", "false positive"), ("false_negative", "false negative"),
               ("entries", "exact episodes"), ("missed_entries", "missed episodes"), ("gaps", "gaps"),
               ("mean_onset_delay", "mean onset delay (evaluations)"), ("entities", f"{noun}s"))
    every_rule = ('( $m := $[labels.app="rule" and $not($exists(labels.entity))]; '
                  '$m[metric="agreement"].( $s := labels.signal; '
                  '$merge([{"rule": $s}, $m[labels.signal=$s]{metric: value}]) ) )')
    b.panel({"type": "table", "title": f"Every rule: exact against approx, counted per {noun} and evaluation",
             "targets": [iq("A", summary, every_rule,
                            [col("rule", "rule", "string")] + by_metric_columns(*columns))],
             "transformations": order("rule", *[text for _, text in columns]),
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": [
                 {"matcher": {"id": "byName", "options": "agreement"},
                  "properties": [{"id": "unit", "value": "percentunit"}, {"id": "decimals", "value": 2}]}]},
             "options": {"showHeader": True}}, 0, 9, 24, 8)

    b.row(f"Rule ${{signal}}, {noun} by {noun}", 17)
    b.panel({"type": "table", "title": f"{noun.capitalize()}s where approx and exact disagree most",
             "targets": [iq("A", summary,
                            f'$[{app} and metric="disagreements" and value > 0].'
                            '{"entity": labels.entity, "disagreements": value}',
                            [col("entity", noun, "string"), col("disagreements", "disagreements", "number")])],
             "transformations": sort_limit("disagreements", 20),
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": [
                 {"matcher": {"id": "byName", "options": noun},
                  "properties": [{"id": "links", "value": [{
                      "title": f"Show this {noun}",
                      "url": f"/d/{uid}/{uid}?var-run=${{run}}&var-signal=${{signal}}"
                             f"&var-entity=${{__data.fields.{noun}}}&${{__url_time_range}}"}]}]}]},
             "options": {"showHeader": True}}, 0, 18, 8, 8)
    b.panel({"type": "table", "title": "Where approx ≠ exact for rule ${signal}",
             "targets": [iq("A", RUN + "fidelity_event.json", f"$[{app}]",
                            [T, col("labels.entity", noun, "string"), col("metric", "event", "string"),
                             col("value", "evaluations", "number")])],
             "transformations": order("time", noun, "event", "evaluations"),
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": []},
             "options": {"showHeader": True}}, 8, 18, 10, 8)
    b.panel(outcome_stat(f"Evaluations by outcome, counted per {noun}", summary, overall), 18, 18, 6, 8)
    strip = strip_panel(f"${{signal}} on {noun} ${{entity_in_run}}: state per evaluation, with what differs",
                        [(f'{here} and metric="state" and labels.arm="exact"', "exact"),
                         (f'{here} and metric="state" and labels.arm="approx"', "approx"),
                         (f'{here} and metric="annotation"', "annotation")],
                        {"active": {"color": "red", "text": "active"}, "idle": OFF},
                        entity_file("app_state", "entity_in_run"))
    strip.update(repeat="entity_in_run", repeatDirection="v")
    b.panel(strip, 0, 26, 24, 5)

    signal = variable("signal", summary, '$[labels.app="rule" and metric="agreement"].labels', "signal")
    ranked = ('( $s := $sort($[labels.app="rule" and labels.signal="${signal}" and metric="disagreements"], '
              'function($a, $b) { $a.value < $b.value or '
              '($a.value = $b.value and $a.labels.entity > $b.labels.entity) }); $s.labels )')
    entity = variable("entity", summary, ranked, "entity", f'[{{"entity": "none for this rule"}}]')
    entity["label"] = noun
    entity.update(sort=0, multi=True, includeAll=False)
    shown = in_run("entity_in_run", "entity", "entity")
    shown.update(multi=True, includeAll=True, current={"text": ["All"], "value": ["$__all"]})
    return part(b, [signal, entity, shown])


AGREEMENT = ("Defined per application. Threshold alert: evaluations where exact and approx agree on firing. "
             "Autoscaler: matched scaling actions over matched, false and missed ones. "
             "Rollout controller: rollouts decided the same. "
             "Top clients: mean clients shared by the exact and approx top lists, as a fraction of the list size. "
             "Rule: entity-evaluations where exact and approx agree on whether the rule is active.")


STAGE1 = ("run_id", "trial", "query", "error metric", "target", "failure probability", "max error", "mean error",
          "evaluations over target", "evaluations", "mean query ms", "mean compile ms", "planning ms",
          "insert samples per second", "peak state bytes", "peak raw sample bytes read at query",
          "exact mean query ms", "exact mean compile ms", "exact planning ms", "exact mean samples read at query",
          "exact peak raw sample bytes read at query")
SAME, FALSE_ROLLBACK, MISSED_ROLLBACK = "same decision", "false rollback", "missed rollback"


def rollouts_by_error(ref, use_case, outcomes, name, y):
    root = (f'$[use_case="{use_case}"{synthetic_only()} and $exists(details.stage2)].details.stage2'
            f'[rule="${{rule}}" and outcome in {in_list(outcomes)}]'
            f'.{{"max error": $lookup(`max error`, "${{query}}"), "{name}": {y}}}')
    return iq(ref, INDEX, root, [col("max error", "max error", "number"), col(name, name, "number")])


def stage1_panel(uid, use_case):
    stage1 = (f'$[use_case="{use_case}"{synthetic_only()} and $exists(details.stage1)].( $e := $; $e.details.stage1.( '
              '$merge([$, {"run_id": $e.run_id, "trial": $e.details.trial}]) ) )')
    text = ("run_id", "query", "error metric")
    return {"type": "table",
             "title": "Per run and query: measured error against the target, and the cost "
                      "(failure probability: the share of evaluations allowed over the target)",
             "targets": [iq("A", INDEX, stage1, [col(k, k, "string") for k in text]
                            + [col(k, k, "number") for k in STAGE1 if k not in text])],
             "transformations": order(*STAGE1) + [{"id": "sortBy", "options": {"sort": [{"field": "run_id"}]}}],
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60, "wrapHeaderText": True,
                                                     "cellOptions": {"type": "auto", "wrapText": True}}},
                             "overrides": [
                 {"matcher": {"id": "byRegexp", "options": "^(target|max error|mean error)$"},
                  "properties": [{"id": "unit", "value": "percentunit"}, {"id": "decimals", "value": 2}]},
                 {"matcher": {"id": "byRegexp", "options": "bytes"},
                  "properties": [{"id": "unit", "value": "bytes"}]},
                 {"matcher": {"id": "byRegexp", "options": " ms$"},
                  "properties": [{"id": "decimals", "value": 3}]},
                 {"matcher": {"id": "byRegexp", "options": "per second|samples read"},
                  "properties": [{"id": "unit", "value": "short"}]},
                 {"matcher": {"id": "byName", "options": "run_id"},
                  "properties": [{"id": "custom.width", "value": 200}, {"id": "links", "value": [{
                      "title": "Open this run", "url": f"/d/{uid}/{uid}?var-run=${{__data.fields.run_id}}"}]}]}]},
             "options": {"showHeader": True}}


def stage1_dashboard(uid, use_case):
    b = Board()
    b.row("Stage 1: sketch configuration to approx query result", 0)
    b.panel(stage1_panel(uid, use_case), 0, 1, 24, 14)
    return part(b)


def two_stages_dashboard(uid, module):
    use_case = cases.spec_of(module)["use_case"]
    b = Board()
    b.row("Stage 1: sketch configuration to approx query result", 0)
    b.panel(stage1_panel(uid, use_case), 0, 1, 24, 14)

    b.row("Stage 2: approx query result to rollout decision, rule ${rule}", 15)
    b.panel({"type": "xychart",
             "title": "${query}: each rollout's decision against the query's max error over its checks",
             "description": "Rank error for p99, relative error for success rate.",
             "repeat": "query", "repeatDirection": "h", "maxPerRow": 2,
             "targets": [rollouts_by_error("A", use_case, ["true_positive", "true_negative"], SAME, 1),
                         rollouts_by_error("B", use_case, ["false_positive"], FALSE_ROLLBACK, 0),
                         rollouts_by_error("C", use_case, ["false_negative"], MISSED_ROLLBACK, 0)],
             "fieldConfig": {"defaults": {"custom": {"show": "points", "pointSize": {"fixed": 7}},
                                          "min": -0.2, "max": 1.2},
                             "overrides": [
                 {"matcher": {"id": "byName", "options": "max error"},
                  "properties": [{"id": "unit", "value": "percentunit"}, {"id": "min", "value": 0},
                                 {"id": "max", "value": None}]},
                 *[{"matcher": {"id": "byName", "options": n},
                    "properties": [{"id": "color", "value": {"mode": "fixed", "fixedColor": c}}]}
                   for n, c in ((SAME, "green"), (FALSE_ROLLBACK, "orange"), (MISSED_ROLLBACK, "red"))]]},
             "options": {"mapping": "manual", "series": [
                 {"frame": {"matcher": {"id": "byIndex", "options": i}},
                  "x": {"matcher": {"id": "byName", "options": "max error"}},
                  "y": {"matcher": {"id": "byName", "options": name}}}
                 for i, name in enumerate((SAME, FALSE_ROLLBACK, MISSED_ROLLBACK))], **LEGEND}},
            0, 16, 12, 12)
    stage2 = f'$[use_case="{use_case}"{synthetic_only()} and $exists(details.stage2)].details.stage2'
    rules = f'$map($distinct({stage2}.rule), function($v) {{ {{"rule": $v}} }})'
    queries = f'$map($distinct({stage2}[rule="${{rule}}"].$keys(`max error`)), function($v) {{ {{"query": $v}} }})'
    query = variable("query", INDEX, queries, "query")
    query.update(multi=True, includeAll=True, current={"text": ["All"], "value": ["$__all"]})
    return part(b, [variable("rule", INDEX, rules, "rule"), query])


def rule_error_dashboard(rule_set, metrics, file_var=None, subpopulation=None):
    queries = json.dumps({name: list(rule["queries"]) for name, rule in rule_set.items()})
    sel = f'labels.query in $lookup({queries}, "${{signal}}")'
    series = "labels.query"
    if subpopulation:
        key = "$join([" + ", ".join(f"labels.{d}" for d in subpopulation) + '], ".")'
        sel += f" and {key} in [${{entity_in_run:doublequote}}]"
        series = f'{key} & " " & labels.query'
    path = "low_level" if file_var is None else entity_file("low_level", file_var)
    b = Board()
    b.row("Rule ${signal}: query error", 0)
    for i, (metric, title) in enumerate(metrics):
        target = iq("A", RUN + path + ".json",
                    f'$[metric="{metric}" and {sel}].{{"time": time, "value": value, "series": {series}}}',
                    [T, col("value", "value", "number"), col("series", "series", "string")])
        target["format"] = "timeseries"
        panel = {"type": "timeseries",
                 "title": f"{title} of approx, queries of rule ${{signal}}"
                          + (f" (${{{file_var}}})" if file_var else ""),
                 "targets": [target],
                 "fieldConfig": {"defaults": {**LINES, "unit": "percentunit", "displayName": "${__field.labels.series}"},
                                 "overrides": []},
                 "options": LEGEND}
        if file_var:
            panel.update(repeat=file_var, repeatDirection="v")
        b.panel(panel, 0, 1 + 7 * i, 24, 7)
    variables = []
    if subpopulation:
        cities = ('$map($distinct([${entity_in_run:doublequote}].$substringBefore($, ".")), '
                  'function($v) { {"city": $v} })')
        city = variable(file_var, INDEX, cities, "city")
        city.update(hide=2, multi=True, includeAll=True, skipUrlSync=True,
                    current={"text": ["All"], "value": ["$__all"]})
        variables.append(city)
    return part(b, variables)


ERRORS = (("relative_error", "Relative error"), ("rank_error", "Rank error"))


OVERVIEW = ("Each row is one query in one run, paired with one rule or alert that reads it; a query read by "
            "three rules has three rows. Sketch and parameters are the approximate plan the planner chose: the sketch type and its size (for "
            "KMV, k is how many hashes it keeps; for HLL, precision p means 2^p registers). Fidelity compares the rule's decisions on the exact "
            "and approx answers: an episode is a stretch of consecutive evaluations where the exact rule fires, "
            "an incident is an attack or change the data generator injected (synthetic data only). Hover a "
            "fidelity cell to read all of it.")


def overview_dashboard(uid, use_case, names, sketch_bounds=True):
    rows = (f'$[use_case="{use_case}"{synthetic_only()} and $exists(details.fidelity)].( $e := $; '
            '$append($keys($e.details.plans), $keys($e.details.unsupported)).( $q := $; '
            '$p := $lookup($e.details.plans, $q); '
            '$lookup($e.details.signals, $q).( $s := $; $f := $lookup($e.details.fidelity, $s); '
            '{"run_id": $e.run_id, "use_case": $e.use_case, "start": $e.start, "end": $e.end, "query": $q, '
            + plan_fields("$p") + ', '
            '"why no approx plan": $lookup($e.details.unsupported, $q), '
            '"target error": $e.details.arm.max_error, '
            '"measured max error": $lookup($e.details.max_relative_error, $q), '
            '"rule_id": $s, "rule": ( $t := $lookup(' + json.dumps(names) + ', $s); $exists($t) ? $t : $s ), '
            '"agreement": $f.agreement, "fidelity": $f.fidelity, '
            '"dashboard": ( $d := $lookup($e.details.signal_dashboards, $s); $exists($d) ? $d : $e.details.dashboard )'
            '} ) ) )')
    text = ("use_case", "run_id", "query", "sketch", "parameters", "why no approx plan", "rule", "fidelity",
            "dashboard", "rule_id")
    numbers = ("start", "end", "target error", "measured max error", "agreement")
    shown = ("use_case", "run_id", "query", "sketch", "parameters", "why no approx plan", "target error",
             "measured max error", "rule", "agreement", "fidelity")
    b = Board()
    b.row("Every run", 0)
    b.panel({"type": "table", "title": "Every run and query: plan, query error, and what the application saw",
             "description": OVERVIEW,
             "targets": [iq("A", INDEX, rows, [col(k, k, "string") for k in text]
                            + [col(k, k, "number") for k in numbers])],
             "transformations": [{"id": "organize", "options": {
                 "indexByName": {k: i for i, k in enumerate(shown)},
                 "excludeByName": {}, "renameByName": {}}},
                 {"id": "sortBy", "options": {"sort": [{"field": "run_id"}]}}],
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60, "cellOptions": {"type": "auto"}}},
                             "overrides": [
                 {"matcher": {"id": "byRegexp", "options": "^(dashboard|start|end|use_case|rule_id)$"},
                  "properties": [{"id": "custom.hidden", "value": True}]},
                 {"matcher": {"id": "byRegexp", "options": "^(target error|measured max error)$"},
                  "properties": [{"id": "unit", "value": "percentunit"}, {"id": "decimals", "value": 1},
                                 {"id": "custom.width", "value": 90}]},
                 {"matcher": {"id": "byName", "options": "agreement"},
                  "properties": [{"id": "description", "value": AGREEMENT},
                                 {"id": "unit", "value": "percentunit"}, {"id": "decimals", "value": 1},
                                 {"id": "min", "value": 0}, {"id": "max", "value": 1},
                                 {"id": "custom.width", "value": 110},
                                 {"id": "custom.cellOptions", "value": {"type": "color-background", "mode": "basic"}},
                                 {"id": "color", "value": {"mode": "continuous-RdYlGr"}},
                                 {"id": "mappings", "value": [{"type": "special", "options": {
                                     "match": "null+nan", "result": {"text": "not run: a query it uses has no approximate plan",
                                                                     "color": "transparent", "index": 0}}}]}]},
                 {"matcher": {"id": "byName", "options": "parameters"},
                  "properties": [{"id": "custom.width", "value": 230}]},
                 {"matcher": {"id": "byName", "options": "why no approx plan"},
                  "properties": [{"id": "custom.width", "value": 150}]},
                 {"matcher": {"id": "byRegexp", "options": "^(query|rule|sketch)$"},
                  "properties": [{"id": "custom.width", "value": 115}]},
                 {"matcher": {"id": "byName", "options": "run_id"},
                  "properties": [{"id": "custom.width", "value": 170}, {"id": "links", "value": [{
                      "title": "Open this run",
                      "url": f"/d/{uid}/{uid}?var-run=${{__data.fields.run_id}}"
                             "&var-signal=${__data.fields.rule_id}"
                             "&from=${__data.fields.start}&to=${__data.fields.end}"}]}]}]},
             "options": {"showHeader": True}}, 0, 1, 24, 16)
    if not sketch_bounds:
        return part(b)
    bounds = (f'$[use_case="{use_case}"{synthetic_only()} and $exists(details.sketch_bounds)].( $e := $; $e.details.sketch_bounds.( '
              '$merge([$, {"run_id": $e.run_id, '
              '"max relative error": $lookup($e.details.max_relative_error, query)}]) ) )')
    text = ("run_id", "query", "sketch", "statistic", "bound metric", "merged error")
    numbers = ("epsilon", "delta", "max relative error", "sketchlib bytes per state", "planner peak bytes for the plan",
               "update ops")
    b.panel({"type": "table",
             "title": "Every planned sketch: asap_sketchlib's error bound and memory next to the measured error "
                      "and the planner's memory",
             "targets": [iq("A", INDEX, bounds, [col(k, k, "string") for k in text]
                            + [col(k, k, "number") for k in numbers])],
             "transformations": [{"id": "organize", "options": {
                 "indexByName": {k: i for i, k in enumerate(text[:5] + ("epsilon", "delta", "max relative error",
                                                                        "merged error") + numbers[3:])},
                 "excludeByName": {}, "renameByName": {}}},
                 {"id": "sortBy", "options": {"sort": [{"field": "run_id"}]}}],
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60, "cellOptions": {"type": "auto", "wrapText": True}}},
                             "overrides": [
                 {"matcher": {"id": "byName", "options": "max relative error"},
                  "properties": [{"id": "unit", "value": "percentunit"}, {"id": "decimals", "value": 1}]},
                 {"matcher": {"id": "byRegexp", "options": "bytes"},
                  "properties": [{"id": "unit", "value": "bytes"}]},
                 {"matcher": {"id": "byName", "options": "sketch"},
                  "properties": [{"id": "custom.width", "value": 230}]}]},
             "options": {"showHeader": True}}, 0, 17, 24, 12)
    return part(b)


SYSTEM_TEXT = ("run_id", "system", "write mode", "unsupported", "failed evaluations", "samples dropped")
SYSTEM_NUMBERS = (("insert samples per second", "short"), ("write seconds", "s"), ("visibility seconds", "s"),
                  ("peak resident bytes", "bytes"),
                  ("resident bytes at end", "bytes"), ("disk bytes", "bytes"), ("cpu seconds", "s"),
                  ("mean query ms", "ms"), ("median query ms", "ms"), ("p99 query ms", "ms"))
TRUTH_NUMBERS = ("max relative error", "answers differing", "answers")


def systems_dashboard(uid, use_case):
    each = (f'( $runs := $[use_case="{use_case}"{synthetic_only()} and $exists(details.systems)]; '
            '$reduce($runs, function($acc, $e) { $append($acc, $each($e.details.systems, function($v, $k) { '
            '$merge([$v, {"run_id": $e.run_id, "system": $k, '
            '"write mode": $lookup($e.details, "write mode"), '
            '"unsupported": ( $u := $keys($v.unsupported); $exists($u) ? $join($u, ", ") : "" ), '
            '"failed evaluations": ( $f := $lookup($v, "failed evaluations"); $count($keys($f)) > 0 ? $string($f) : "" ), '
            '"samples dropped": ( $d := $lookup($v, "samples dropped"); $count($keys($d)) > 0 ? $string($d) : "" )'
            '}]) })) }, []) )')
    b = Board()
    b.row("VictoriaMetrics vs ASAPQuery-backend", 0)
    b.panel({"type": "table",
             "title": "VictoriaMetrics, which keeps every raw sample, vs ASAPQuery-backend planned to keep none: "
                      "writes, memory, disk and query latency",
             "description": "VictoriaMetrics answers are the exact arm, ASAPQuery-backend answers the approx arm. "
                            "One sender writes both in time order; at each evaluation time it makes the samples "
                            "written so far visible (a watermark for ASAPQuery-backend, a flush for "
                            "VictoriaMetrics) and asks the queries due then, one instant query each. Write seconds "
                            "include the visibility calls. Peak resident bytes and CPU seconds cover the whole "
                            "process; disk bytes is the data directory at the end, and ASAPQuery-backend keeps "
                            "everything in memory.",
             "targets": [iq("A", INDEX, each, [col(k, k, "string") for k in SYSTEM_TEXT]
                            + [col(k, k, "number") for k, _ in SYSTEM_NUMBERS])],
             "transformations": order(*SYSTEM_TEXT[:3], *(k for k, _ in SYSTEM_NUMBERS), *SYSTEM_TEXT[3:])
             + [{"id": "sortBy", "options": {"sort": [{"field": "run_id"}]}}],
             "fieldConfig": {"defaults": {"custom": {"minWidth": 95, "cellOptions": {"type": "auto", "wrapText": True}}},
                             "overrides": [{"matcher": {"id": "byName", "options": k},
                                            "properties": [{"id": "unit", "value": unit}]}
                                           for k, unit in SYSTEM_NUMBERS]
                             + [{"matcher": {"id": "byRegexp", "options": "^(system|write mode)$"},
                                 "properties": [{"id": "custom.width", "value": 150}]}]
                             + [{"matcher": {"id": "byName", "options": "run_id"},
                                 "properties": [{"id": "custom.width", "value": 230}, {"id": "links", "value": [{
                                     "title": "Open this run",
                                     "url": f"/d/{uid}/{uid}?var-run=${{__data.fields.run_id}}"}]}]}]},
             "options": {"showHeader": True}}, 0, 1, 24, 14)
    truth = (f'( $runs := $[use_case="{use_case}"{synthetic_only()} and $exists(details.victoriametrics_vs_truth)]; '
             '$reduce($runs, function($acc, $e) { $append($acc, $each($e.details.victoriametrics_vs_truth, '
             'function($v, $k) { $merge([$v, {"run_id": $e.run_id, "query": $k}]) })) }, []) )')
    b.panel({"type": "table",
             "title": "VictoriaMetrics answers checked against the in-process exact plan over the same samples",
             "description": "A difference here comes from how VictoriaMetrics evaluates the query, not from "
                            "approximation.",
             "targets": [iq("A", INDEX, truth, [col("run_id", "run_id", "string"), col("query", "query", "string")]
                            + [col(k, k, "number") for k in TRUTH_NUMBERS])],
             "transformations": order("run_id", "query", *TRUTH_NUMBERS)
             + [{"id": "sortBy", "options": {"sort": [{"field": "run_id"}]}}],
             "fieldConfig": {"defaults": {"custom": {"minWidth": 60}}, "overrides": [
                 {"matcher": {"id": "byName", "options": "max relative error"},
                  "properties": [{"id": "unit", "value": "percentunit"}, {"id": "decimals", "value": 2}]}]},
             "options": {"showHeader": True}}, 0, 15, 24, 8)
    return part(b)


PHASE3_SKIPS = ("stage1", "two_stages")


def parts_of(module, phase3=False, figure=True):
    use_case = cases.spec_of(module)["use_case"]
    board = module.DASHBOARD
    rule_error = board.get("rule_error", {})
    builders = {
        "overview": lambda uid: overview_dashboard(
            uid, use_case, {r["rule"]: r["name"] for r in rule_catalog(module) if r["name"] != r["rule"]},
            sketch_bounds=not phase3),
        "cost": lambda uid: cost_dashboard(module),
        "stage1": lambda uid: stage1_dashboard(uid, use_case),
        "runs": lambda uid: runs_part(uid, use_case),
        "rule_note": lambda uid: rule_note_part(module),
        "alert": lambda uid: alert_dashboard(module),
        "decision": lambda uid: decision_dashboard(uid, module),
        "rollout": lambda uid: rollout_dashboard(module),
        "two_stages": lambda uid: two_stages_dashboard(uid, module),
        "topk": lambda uid: topk_dashboard(uid),
        "rules": lambda uid: rules_dashboard(uid, board.get("entity", "entity")),
        "rule_error": lambda uid: rule_error_dashboard(
            module.RULES, tuple(e for e in ERRORS if e[0] in rule_error.get("metrics", ())),
            rule_error.get("file_var"), rule_error.get("subpopulation")),
    }
    names = [n for name in board["parts"] for n in ((name, "rule_note") if name == "runs" else (name,))]
    parts = tuple(builders[name] for name in names if not (phase3 and name in PHASE3_SKIPS))
    if phase3:
        parts = (lambda uid: systems_dashboard(uid, use_case),) + parts
    parts = (lambda uid: about_part(module),) + parts
    if figure and "figure" in board:
        parts = (lambda uid: figure_part(board["figure"]),) + parts
    return parts


def merged_variable(first, other):
    if first == other or first["name"] != "signal":
        return first
    roots = [v["query"]["infinityQuery"]["root_selector"] for v in (first, other)]
    out = json.loads(json.dumps(first))
    out["query"]["infinityQuery"]["root_selector"] = "$append(" + ", ".join(roots) + ")"
    return out


def time_range(spec):
    iso = lambda ms: datetime.fromtimestamp(ms / 1000, timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.000Z")
    return {"from": iso(spec["start_ms"]), "to": iso(spec["start_ms"] + spec["duration_ms"])}


def use_case_dashboard(uid, title, parts, spec, module):
    b = Board()
    variables, annotations, y = {}, [], 0
    for build in parts:
        built = build(uid)
        top = min(p["gridPos"]["y"] for p in built["panels"])
        for p in built["panels"]:
            p["gridPos"]["y"] += y - top
            p["id"] = len(b.panels) + 1
            b.panels.append(p)
        y = max(p["gridPos"]["y"] + p["gridPos"]["h"] for p in b.panels)
        for v in built["variables"]:
            variables[v["name"]] = merged_variable(variables[v["name"]], v) if v["name"] in variables else v
        annotations += built["annotations"]
    signal = variables.get("signal")
    if signal:
        names = {r["rule"]: r["name"] for r in rule_catalog(module) if r["name"] != r["rule"]}
        query = signal["query"]["infinityQuery"]
        query["root_selector"] = (f'( $n := {json.dumps(names)}; ({query["root_selector"]}).{{"__value": signal, '
                                  '"__text": ( $t := $lookup($n, signal); $exists($t) ? $t : signal )} )')
        query["columns"] = [col("__text", "__text", "string"), col("__value", "__value", "string")]
        signal["label"] = "rule"
        for p in b.panels:
            if p.get("title"):
                p["title"] = p["title"].replace("${signal}", "${signal:text}")
    return dashboard(uid, title, b, list(variables.values()), annotations, time_range(spec))


def phase_boards(phase, figure):
    for m in cases.load().values():
        uid, title = m.DASHBOARD["uid"], m.DASHBOARD["title"]
        if phase == 2:
            yield uid, use_case_dashboard(uid, title, parts_of(m, figure=figure), cases.spec_of(m), m)
        else:
            board = use_case_dashboard("phase3-" + uid, "Phase 3 · " + title, parts_of(m, phase3=True, figure=figure),
                                       cases.spec_of(m), m)
            yield "phase3-" + uid, json.loads(json.dumps(board).replace(DATA, PHASE3_DATA))


def main():
    parser = argparse.ArgumentParser(description="Write the Grafana dashboards of the five use cases.")
    parser.add_argument("--phase", type=int, choices=(2, 3), action="append",
                        help="repeatable; the phase whose dashboards to write; default both")
    parser.add_argument("--no-figure", action="store_true",
                        help="leave out the scenario figure at the top of each dashboard")
    parser.add_argument("--out", default=OUT, help=f"directory to write into; default {OUT}")
    parser.add_argument("--real-data", action="store_true",
                        help="also show runs on real datasets and describe those datasets; by default the dashboards "
                             "show synthetic data only")
    args = parser.parse_args()
    global REAL_DATA
    REAL_DATA = args.real_data
    phases = sorted(set(args.phase or (2, 3)))
    os.makedirs(args.out, exist_ok=True)
    for name in os.listdir(args.out):
        if name.endswith(".json") and (3 if name.startswith("phase3-") else 2) in phases:
            os.remove(os.path.join(args.out, name))
    for phase in phases:
        for uid, board in phase_boards(phase, not args.no_figure):
            with open(os.path.join(args.out, uid + ".json"), "w") as f:
                json.dump(board, f, indent=2)
                f.write("\n")


if __name__ == "__main__":
    main()
