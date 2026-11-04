#!/usr/bin/env python3
"""Count Postgres transactions per OTF API request (pg_stat_database).

Shows the per-workspace authorization lookups behind OTF's slow workspace
list: each listed workspace costs ~20 queries. Writes results/otf-query-count.json.

pg_stat_database is flushed lazily by idle backends (up to ~10 s), so each
measurement waits 15 s and forces a flush before and after.
"""
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "lib"))
sys.path.insert(0, HERE)
import harness as h  # noqa: E402
import run as o  # noqa: E402


def stat():
    r = h.sh(["docker", "exec", "otf-postgres", "psql", "-U", "bench", "-d", "otf", "-p", str(o.PG_PORT),
              "-Atc", "select pg_stat_force_next_flush(); "
                      "select xact_commit + xact_rollback, tup_returned from pg_stat_database where datname = 'otf'"],
             capture=True)
    return [int(x) for x in r.stdout.strip().splitlines()[-1].split("|")]


def measure(url, n=5):
    time.sleep(15)
    a = stat()
    for _ in range(n):
        h.http("GET", url, o.TOKEN)
    time.sleep(15)
    b = stat()
    # -1: the stat query's own transaction
    return {"transactions_per_request": round((b[0] - a[0] - 1) / n, 1),
            "tuples_returned_per_request": round((b[1] - a[1]) / n, 1)}


def main():
    h.docker_rm(*o.CONTAINERS)
    h.start_postgres("otf-postgres", o.PG_PORT, "otf")
    o.start_otfd()
    o.create_org()
    ws = [o.create_workspace(f"ws-{i:03d}") for i in range(50)]
    out = {
        "background, no requests (n=1 window)": measure(o.BASE + "/healthz", n=1),
        "list 50 workspaces": measure(f"{o.API}/organizations/{o.ORG}/workspaces?page%5Bsize%5D=50"),
        "list 10 workspaces": measure(f"{o.API}/organizations/{o.ORG}/workspaces?page%5Bsize%5D=10"),
        "get 1 workspace": measure(f"{o.API}/workspaces/{ws[0]}"),
    }
    print(json.dumps(out, indent=2))
    h.save("otf-query-count", {"tool": "otf", "version": o.VERSION, "requests": out})
    h.docker_rm(*o.CONTAINERS)


if __name__ == "__main__":
    main()
