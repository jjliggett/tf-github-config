#!/usr/bin/env python3
"""Repeated cold starts: median of N `docker run` -> first healthy response.

The single cold start in each driver is one noisy sample dominated by
`docker run` itself. This repeats it (images cached, dependencies already
running) and also reports `process_ready_s`: the time from `docker run`
returning to the first healthy response, i.e. the application's own startup.

    BENCH_WORK=... python3 cold_start.py [otf] [atlantis] [cairn]
"""
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "lib"))
import harness as h  # noqa: E402

TRIALS = int(os.environ.get("TRIALS", 10))


def trial(name, cmd, url):
    h.docker_rm(name)
    t0 = time.monotonic()
    h.sh(cmd, capture=True)
    t1 = time.monotonic()
    h.wait_http(url, timeout=120, container=name, interval=0.005)
    t2 = time.monotonic()
    h.docker_rm(name)
    return t2 - t0, t2 - t1


def measure(name, make_cmd, url):
    totals, ready = [], []
    for _ in range(TRIALS):
        a, b = trial(name, make_cmd(), url)
        totals.append(a)
        ready.append(b)
    return {"trials": TRIALS, "docker_run_to_healthy_s": h.stats(totals), "process_ready_s": h.stats(ready)}


def load(tool):
    sys.path.insert(0, os.path.join(HERE, tool))
    import run
    sys.modules.pop("run")
    sys.path.pop(0)
    return run


def main():
    tools = sys.argv[1:] or ["otf", "atlantis", "cairn"]
    # Merge with earlier results so tools can be re-measured one at a time.
    path = os.path.join(h.RESULTS, "cold-start.json")
    out = json.load(open(path)) if os.path.exists(path) else {}
    out.pop("host", None)
    for t in tools:
        h.log("cold starts:", t)
        m = load(t)
        if t == "otf":
            h.start_postgres("otf-postgres", m.PG_PORT, "otf")
            out[t] = measure("otf-otfd", m.otfd_cmd, f"{m.BASE}/healthz")
            h.docker_rm("otf-postgres")
        elif t == "atlantis":
            import vcs
            token = vcs.up()
            out[t] = measure("atlantis", lambda: m.atlantis_cmd(token), f"{m.BASE}/healthz")
        elif t == "cairn":
            m.build_image()
            out[t] = measure("cairn-cairnd", m.cairnd_cmd, f"{m.BASE}/healthz")
        h.log(t, out[t])
    h.save("cold-start", out)


if __name__ == "__main__":
    main()
