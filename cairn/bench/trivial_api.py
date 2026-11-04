#!/usr/bin/env python3
"""Smallest authenticated read, back to back: Atlantis GET /api/locks vs Cairn GET /api/v1/whoami
(with the site token, and with a writer-scoped service token).

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
    if callable(headers):
        headers = headers()
    reps = [h.hey(url, n=3200, c=16, headers=headers) for _ in range(REPS)]
    h.docker_rm(name)
    med = lambda k: sorted(r[k] for r in reps)[len(reps) // 2]
    return {"endpoint": url.split("://")[1].split("/", 1)[1], "reps": REPS,
            "rps_median": med("rps"), "p99_ms_median": med("p99_ms"), "p50_ms_median": med("p50_ms")}


def scoped_token(c):
    s, b = h.http("POST", f"{c.BASE}/api/v1/spaces", c.TOKEN, {"name": c.ORG})
    assert s == 201, (s, b)
    s, b = h.http("POST", f"{c.BASE}/api/v1/tokens", c.TOKEN,
                  {"kind": "service", "roles": [{"space": c.ORG, "role": "writer"}]})
    assert s == 201, (s, b)
    return b["token"]


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
        # Same read with a writer-scoped service token (the BENCH_FEATURES=1 setup).
        "cairn-features": bench("cairn-cairnd", c.cairnd_cmd(), f"{c.BASE}/healthz", f"{c.BASE}/api/v1/whoami",
                                lambda: {"Authorization": f"Bearer {scoped_token(c)}"}),
    }
    h.log(out)
    h.save("trivial-api", out)


if __name__ == "__main__":
    main()
