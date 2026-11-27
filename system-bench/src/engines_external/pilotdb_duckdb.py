import json
import os
import resource
import sys
import time

import duckdb
import pandas


def table_rows(connection, table):
    found = connection.execute(
        "SELECT count(*) FROM information_schema.tables WHERE table_name = ?", [table]
    ).fetchone()[0]
    if not found:
        return None
    return connection.execute(f'SELECT count(*) FROM "{table}"').fetchone()[0]


def table_metadata(connection):
    tables = [
        row[0]
        for row in connection.execute(
            "SELECT table_name FROM information_schema.tables "
            "WHERE table_schema = 'main' AND table_type = 'BASE TABLE'"
        ).fetchall()
    ]
    columns = {
        table: [
            row[0]
            for row in connection.execute(
                "SELECT column_name FROM information_schema.columns "
                "WHERE table_name = ? ORDER BY ordinal_position",
                [table],
            ).fetchall()
        ]
        for table in tables
    }
    rows = {table: table_rows(connection, table) for table in tables}
    largest_first = dict(sorted(rows.items(), key=lambda item: -item[1]))
    return columns, largest_first


def read_text(path):
    if not os.path.exists(path):
        return ""
    with open(path) as file:
        return file.read()


def answer(database, sql, relative_error, failure_probability, threads):
    with duckdb.connect(database, read_only=True) as connection:
        columns, rows = table_metadata(connection)
    from pilotdb.db_driver import duckdb_utils
    from pilotdb.execute import execute_aqp
    from pilotdb.query import Query

    if threads:
        duckdb_utils.connect_to_db = lambda path: duckdb.connect(
            database=path, read_only=False, config={"threads": threads}
        )
    query = Query(
        query=sql,
        table_cols=columns,
        table_size=rows,
        error=relative_error,
        failure_probability=failure_probability,
        name="query",
    )
    before = resource.getrusage(resource.RUSAGE_SELF)
    start = time.perf_counter()
    result, phases = execute_aqp(query, {"dbms": "duckdb", "path": database})
    wall_seconds = time.perf_counter() - start
    after = resource.getrusage(resource.RUSAGE_SELF)
    log = "".join(
        read_text(os.path.join("logs", name))
        for name in sorted(os.listdir("logs") if os.path.isdir("logs") else [])
    )
    lines = [line for line in read_text("all_results.jsonl").splitlines() if line.strip()]
    recorded_rate = json.loads(lines[-1]).get("final_sample_rate") if lines else None
    answered_exactly = (
        "pilot_query_execution" not in phases or "fall back to original queries" in log
    )
    if answered_exactly:
        sample_rate_percent = 100.0
    elif "pilot sampling is large enough" in log:
        sample_rate_percent = 0.05
    else:
        sample_rate_percent = recorded_rate
    return {
        "columns": [str(column) for column in result.columns],
        "rows": result.astype(object).where(pandas.notnull(result), None).values.tolist(),
        "wall_seconds": wall_seconds,
        "cpu_seconds": (after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime),
        "phases_seconds": phases,
        "answered_exactly": answered_exactly,
        "sample_rate_percent": sample_rate_percent,
    }


def main():
    mode, database = sys.argv[1], sys.argv[2]
    if mode == "table-rows":
        with duckdb.connect(database, read_only=True) as connection:
            print(json.dumps({"rows": table_rows(connection, sys.argv[3])}))
        return
    threads = int(sys.argv[6]) if sys.argv[6] else None
    print(
        json.dumps(
            answer(database, sys.argv[3], float(sys.argv[4]), float(sys.argv[5]), threads),
            default=str,
        )
    )


main()
