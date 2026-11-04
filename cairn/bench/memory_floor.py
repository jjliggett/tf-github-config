#!/usr/bin/env python3
"""Smallest hard memory limit (no swap) under which each app service starts.

Idle RSS on a 16 GiB host overstates what a JVM needs: Terrakube's images use
the Paketo memory calculator, which sizes the heap from the container limit
(or the whole host when there is none). On a small board the limit is what
matters, so this tries ascending `--memory` limits per service and records
the first one that becomes healthy and stays up for 15 s, along with RSS and
start time at that limit.

Only startup and idle are covered: a run adds tofu's own memory (counted in
OTF's and Terrakube's executor containers), so treat these as floors.

    BENCH_WORK=... python3 memory_floor.py [otf] [terrakube] [atlantis]
"""
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "lib"))
import harness as h  # noqa: E402

LIMITS_GO = [12, 16, 24, 32, 48, 64]
LIMITS_JVM = [256, 384, 512, 640, 768, 1024, 1536, 2048]


def with_limit(cmd, mib):
    i = cmd.index("run") + 1
    return cmd[:i] + ["--memory", f"{mib}m", "--memory-swap", f"{mib}m"] + cmd[i:]


def oom_killed(name):
    r = h.sh(["docker", "inspect", "-f", "{{.State.OOMKilled}} {{.State.ExitCode}}", name],
             capture=True, check=False)
    return r.stdout.strip()


def probe(name, cmd, url, limits, timeout):
    results = []
    for mib in limits:
        h.docker_rm(name)
        t0 = time.monotonic()
        h.sh(with_limit(cmd, mib), capture=True)
        try:
            h.wait_http(url, timeout=timeout, container=name)
            ready = time.monotonic() - t0
            time.sleep(15)
            ok = h.running(name)
            h.wait_http(url, timeout=5, container=name) if ok else None
        except Exception:
            ok, ready = False, None
        snap = None
        if ok:
            import json
            r = h.sh([sys.executable, h.CGSTAT, "snapshot", name], capture=True)
            snap = json.loads(r.stdout).get(name)
        res = {"limit_mib": mib, "ok": ok, "ready_s": round(ready, 2) if ready else None,
               "rss_mib": round(snap["rss"] / 2**20, 1) if snap else None,
               "state": None if ok else oom_killed(name)}
        h.log(name, res)
        results.append(res)
        if ok:
            break
    h.docker_rm(name)
    floor = next((r["limit_mib"] for r in results if r["ok"]), None)
    return {"floor_mib": floor, "attempts": results}


def otf():
    sys.path.insert(0, os.path.join(HERE, "otf"))
    import run as o
    h.start_postgres("otf-postgres", o.PG_PORT, "otf")
    out = {"otfd": probe("otf-otfd", o.otfd_cmd(), f"{o.BASE}/healthz", LIMITS_GO, 60)}
    h.docker_rm("otf-otfd", "otf-postgres")
    sys.modules.pop("run")
    sys.path.pop(0)
    return out


def terrakube():
    sys.path.insert(0, os.path.join(HERE, "terrakube"))
    import run as t
    import vcs
    vcs.up()
    tools = vcs.mirror_github("terrakube-io/terrakube-extensions", "terrakube-extensions")
    out = {}
    h.docker_rm(*t.CONTAINERS)
    with t.Mirror() as mirror:
        t.start_deps(mirror)
        cmds = t.service_cmds(mirror, tools)
        for svc in ("api", "executor", "registry"):
            out[svc] = probe(t.NAMES[svc], cmds[svc], t.HEALTH[svc], LIMITS_JVM, 240)
            if svc == "api" and out[svc]["floor_mib"]:
                # keep an API up so the others see a realistic environment
                h.sh(with_limit(cmds["api"], out[svc]["floor_mib"] * 2), capture=True)
                h.wait_http(t.HEALTH["api"], timeout=240)
    h.docker_rm(*t.CONTAINERS)
    sys.modules.pop("run")
    sys.path.pop(0)
    return out


def atlantis():
    sys.path.insert(0, os.path.join(HERE, "atlantis"))
    import run as a
    import vcs
    token = vcs.up()
    out = {"atlantis": probe("atlantis", a.atlantis_cmd(token), f"{a.BASE}/healthz", LIMITS_GO, 60)}
    sys.modules.pop("run")
    sys.path.pop(0)
    return out


def cairn():
    sys.path.insert(0, os.path.join(HERE, "cairn"))
    import run as cr
    cr.build_image()
    out = {"cairnd": probe("cairn-cairnd", cr.cairnd_cmd(), f"{cr.BASE}/healthz", [6, 8, 10, 12, 16], 60)}
    sys.modules.pop("run")
    sys.path.pop(0)
    return out


def main():
    tools = sys.argv[1:] or ["otf", "terrakube", "atlantis", "cairn"]
    # Merge with earlier results so tools can be (re)measured one at a time.
    path = os.path.join(h.RESULTS, "memory-floor.json")
    out = json.load(open(path)) if os.path.exists(path) else {}
    out.pop("host", None)
    for t in tools:
        h.log("memory floor:", t)
        out[t] = globals()[t]()
    out["note"] = __doc__.strip().splitlines()[0]
    h.save("memory-floor", out)


if __name__ == "__main__":
    main()
