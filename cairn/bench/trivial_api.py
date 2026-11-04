#!/usr/bin/env python3
"""Smallest authenticated read, back to back: Atlantis GET /api/locks vs Cairn GET /api/v1/whoami.

Atlantis has no inventory API, so the 50-workspace list can't be compared;
this compares the per-request floor instead. 3 repetitions per tool, medians,
unconstrained and (with BENCH_CPUS=1) under a shared 1-CPU quota.
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "lib"))
import harness as h  # noqa: E402
import vcs  # noqa: E402

REPS = int(os.environ.get("REPS", 3))


def load(tool):
    sys.path.insert(0, os.path.join(HERE, tool))
    import run
    sys.modules.pop("run")
    sys.path.pop(0)
    return run


def bench(name, cmd, health, url, headers):
    h.docker_rm(name)
    h.sh(cmd, capture=True)
    h.wait_http(health, timeout=60, container=name)
    reps = [h.hey(url, n=3200, c=16, headers=headers) for _ in range(REPS)]
    h.docker_rm(name)
    med = lambda k: sorted(r[k] for r in reps)[len(reps) // 2]
    return {"endpoint": url.split("://")[1].split("/", 1)[1], "reps": REPS,
            "rps_median": med("rps"), "p99_ms_median": med("p99_ms"), "p50_ms_median": med("p50_ms")}


def main():
    h.setup_cgroup()
    a, c = load("atlantis"), load("cairn")
    token = vcs.up()
    c.build_image()
    out = {
        "atlantis": bench("atlantis", a.atlantis_cmd(token), f"{a.BASE}/healthz", f"{a.BASE}/api/locks",
                          {"X-Atlantis-Token": a.API_SECRET}),
        "cairn": bench("cairn-cairnd", c.cairnd_cmd(), f"{c.BASE}/healthz", f"{c.BASE}/api/v1/whoami",
                       {"Authorization": f"Bearer {c.TOKEN}"}),
    }
    h.log(out)
    h.save("trivial-api", out)


if __name__ == "__main__":
    main()
