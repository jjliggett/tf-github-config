#!/usr/bin/env python3
"""Per-container resource sampler built on cgroup v1 accounting files.

    cgstat.py sample OUT.csv INTERVAL NAME...   # sample until SIGTERM/SIGINT
    cgstat.py summary OUT.csv [T0 T1]           # JSON summary, optional window (unix secs)
    cgstat.py snapshot NAME...                  # one JSON reading

Columns: ts, name, rss, working_set, cache, cpu_ns
  rss          anonymous memory (total_rss) - the memory the process really owns
  working_set  usage - inactive_file (what `docker stats` and the OOM killer see)
  cpu_ns       cumulative CPU time (cpuacct.usage)

Docker must use cgroup v1 (as on this bench host); paths are resolved from
/proc/<pid>/cgroup so containers started with --cgroup-parent work too.
If the host runs cgroup v2, swap the paths for memory.current/memory.stat and cpu.stat.
"""
import csv
import json
import signal
import subprocess
import sys
import time

def cgroup_dirs(pid):
    """Map subsystem -> cgroup dir for a pid (works with or without --cgroup-parent)."""
    dirs = {}
    with open(f"/proc/{pid}/cgroup") as f:
        for line in f:
            _, subsys, path = line.strip().split(":", 2)
            for s in subsys.split(","):
                if s in ("memory", "cpuacct"):
                    dirs[s] = f"/sys/fs/cgroup/{s}{path}"
    return dirs


def container_ids(names):
    """Map container name -> its cgroup dirs (resolved once, via the init pid)."""
    out = {}
    for n in names:
        pid = subprocess.run(["docker", "inspect", "-f", "{{.State.Pid}}", n],
                             capture_output=True, text=True).stdout.strip()
        if pid and pid != "0":
            try:
                out[n] = cgroup_dirs(pid)
            except FileNotFoundError:
                pass
    return out


def read(dirs):
    stat = {}
    with open(dirs["memory"] + "/memory.stat") as f:
        for line in f:
            k, v = line.split()
            stat[k] = int(v)
    with open(dirs["memory"] + "/memory.usage_in_bytes") as f:
        usage = int(f.read())
    with open(dirs["cpuacct"] + "/cpuacct.usage") as f:
        cpu = int(f.read())
    return {
        "rss": stat["total_rss"],
        "working_set": max(0, usage - stat.get("total_inactive_file", 0)),
        "cache": stat["total_cache"],
        "cpu_ns": cpu,
    }


def sample(path, interval, names):
    stop = False

    def _stop(*_):
        nonlocal stop
        stop = True

    signal.signal(signal.SIGTERM, _stop)
    signal.signal(signal.SIGINT, _stop)
    ids = {}
    with open(path, "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["ts", "name", "rss", "working_set", "cache", "cpu_ns"])
        while not stop:
            missing = [n for n in names if n not in ids]
            if missing:
                ids.update(container_ids(missing))
            ts = time.time()
            for n, cid in list(ids.items()):
                try:
                    r = read(cid)
                except FileNotFoundError:
                    ids.pop(n)  # container gone (or restarted with a new id)
                    continue
                w.writerow([f"{ts:.3f}", n, r["rss"], r["working_set"], r["cache"], r["cpu_ns"]])
            fh.flush()
            time.sleep(interval)


def summary(path, t0=None, t1=None):
    rows = {}
    with open(path) as fh:
        for r in csv.DictReader(fh):
            ts = float(r["ts"])
            if t0 is not None and not (t0 <= ts <= t1):
                continue
            rows.setdefault(r["name"], []).append(r)
    out = {}
    for n, rs in rows.items():
        rss = [int(r["rss"]) for r in rs]
        ws = [int(r["working_set"]) for r in rs]
        span = float(rs[-1]["ts"]) - float(rs[0]["ts"])
        cpu_s = (int(rs[-1]["cpu_ns"]) - int(rs[0]["cpu_ns"])) / 1e9
        out[n] = {
            "samples": len(rs),
            "window_s": round(span, 1),
            "rss_mib_avg": round(sum(rss) / len(rss) / 2**20, 1),
            "rss_mib_max": round(max(rss) / 2**20, 1),
            "ws_mib_avg": round(sum(ws) / len(ws) / 2**20, 1),
            "ws_mib_max": round(max(ws) / 2**20, 1),
            "cpu_s": round(cpu_s, 2),
            "cpu_pct_avg": round(100 * cpu_s / span, 2) if span > 0 else None,
        }
    tot = {k: round(sum(v[k] for v in out.values()), 1)
           for k in ("rss_mib_avg", "rss_mib_max", "ws_mib_avg", "ws_mib_max", "cpu_s")}
    out["_total"] = tot
    return out


def main():
    cmd = sys.argv[1]
    if cmd == "sample":
        sample(sys.argv[2], float(sys.argv[3]), sys.argv[4:])
    elif cmd == "summary":
        win = (float(sys.argv[3]), float(sys.argv[4])) if len(sys.argv) > 4 else (None, None)
        print(json.dumps(summary(sys.argv[2], *win), indent=2))
    elif cmd == "snapshot":
        ids = container_ids(sys.argv[2:])
        print(json.dumps({n: read(c) for n, c in ids.items()}, indent=2))
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
