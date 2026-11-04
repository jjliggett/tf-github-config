#!/usr/bin/env python3
"""Build results/SUMMARY.md from the JSON files the drivers wrote."""
import json
import os

BENCH = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
R = os.path.join(BENCH, "results")
TOOLS = ("otf", "terrakube", "atlantis")
APP = {"otf": ["otf-otfd"], "terrakube": ["tk-api", "tk-executor", "tk-registry", "tk-ui"],
       "atlantis": ["atlantis"]}
LABEL = {"otf": "OTF 0.6.3", "terrakube": "Terrakube 2.33.3", "atlantis": "Atlantis 0.49.0"}


def load(name):
    p = os.path.join(R, f"{name}.json")
    return json.load(open(p)) if os.path.exists(p) else None


def mib(b):
    return f"{b / 2**20:.0f} MiB" if b else "-"


def g(d, *keys, default=None):
    for k in keys:
        if not isinstance(d, dict) or k not in d:
            return default
        d = d[k]
    return d


def phase(res, tool, ph, key, which="stack"):
    w = g(res, "resources", ph)
    if not w or len(w) <= 1:  # only "_total": no samples landed in the window
        return None
    if which == "stack":
        return g(w, "_total", key)
    return round(sum(g(w, c, key, default=0) for c in APP[tool]), 1)


def fmt(v, unit="", nd=1):
    if v is None:
        return "-"
    if isinstance(v, float):
        v = int(round(v)) if nd == 0 else round(v, nd)
        if isinstance(v, int):
            v = f"{v:,}"
    return f"{v}{unit}"


def section(suffix, title):
    data = {t: load(t + suffix) for t in TOOLS}
    if not any(data.values()):
        return []
    host = next(d["host"] for d in data.values() if d)
    c = next((d.get("constraint") for d in data.values() if d and d.get("constraint")), None)
    lines = [f"## {title}", ""]
    lines.append(f"Host: {host['cpus']} × {host['cpu_model']}, {host['mem_mib']} MiB RAM, "
                 f"kernel {host['kernel']}, {host['arch']}, run {host['date']}."
                 + (f" **Whole stack capped at {c['cpus']:g} CPU** (shared cgroup quota)." if c else ""))
    lines += ["", "| | " + " | ".join(LABEL[t] for t in TOOLS) + " |", "|---|" + "---|" * len(TOOLS)]

    def row(name, fn):
        lines.append(f"| {name} | " + " | ".join(fn(t, data[t]) if data[t] else "-" for t in TOOLS) + " |")

    def cold(t, d):
        cs = d["cold_start"]
        if "all_s" in cs:
            parts = ", ".join(f"{k} {v}" for k, v in cs.items() if k != "all_s")
            return f"**{cs['all_s']} s** ({parts})"
        return f"**{cs['healthz_s']} s**"
    row("Cold start → healthy", cold)
    row("Idle RSS, app containers", lambda t, d: fmt(phase(d, t, "idle", "rss_mib_avg", "app"), " MiB", 0))
    row("Idle RSS, whole stack", lambda t, d: fmt(phase(d, t, "idle", "rss_mib_avg"), " MiB", 0))
    row("Idle CPU, whole stack", lambda t, d: fmt(
        round(100 * phase(d, t, "idle", "cpu_s") / g(d, "resources", "idle", "_total", default={}).get("window_s", 60), 2)
        if phase(d, t, "idle", "cpu_s") is not None and g(d, "resources", "idle") else None, " % of a core", 2))
    row("API read, 1 client: p50 / p95", lambda t, d: f"{fmt(g(d,'api_load','c1','p50_ms'))} / {fmt(g(d,'api_load','c1','p95_ms'))} ms")
    row("API read, 16 clients: req/s", lambda t, d: fmt(g(d, 'api_load', 'c16', 'rps'), "", 0))
    row("API read, 16 clients: p99", lambda t, d: fmt(g(d, 'api_load', 'c16', 'p99_ms'), " ms"))
    row("Peak RSS during API load (stack)", lambda t, d: fmt(phase(d, t, "api_load", "rss_mib_max"), " MiB", 0))
    row("First run (cold engine/cache)", lambda t, d: fmt(g(d, "first_run", "total_s"), " s", 2))
    row("Run → planned, median", lambda t, d: fmt(g(d, "e2e_serial", "create_to_planned_s", "median"), " s", 2))
    row("Run → applied, median", lambda t, d: fmt(g(d, "e2e_serial", "create_to_applied_s", "median"), " s", 2))
    row("No-op run, median", lambda t, d: fmt(g(d, "e2e_noop", "total_s", "median"), " s", 2))
    row("5 runs at once: wall time", lambda t, d: fmt(g(d, "e2e_parallel", "wall_s"), " s", 2))
    row("Peak RSS, 5 runs at once (stack)", lambda t, d: fmt(phase(d, t, "e2e_parallel", "rss_mib_max"), " MiB", 0))
    row("CPU time per serial run (stack)", lambda t, d: fmt(
        round(phase(d, t, "e2e_serial", "cpu_s") / max(1, len(g(d, "e2e_serial", "statuses", default=[1]))), 2)
        if phase(d, t, "e2e_serial", "cpu_s") is not None else None, " s", 2))
    row("PR opened → plan comment", lambda t, d: fmt(g(d, "pr_workflow", "pr_open_to_plan_comment_s", "median"), " s", 2))
    statuses = []
    for t in TOOLS:
        d = data[t]
        if not d:
            continue
        bad = [s for k in ("e2e_serial", "e2e_noop", "e2e_parallel") for s in
               g(d, k, "statuses", default=[]) + g(d, k, "plan_statuses", default=[])
               if s not in ("applied", "planned_and_finished", "completed", "noChanges", "ok")]
        if bad:
            statuses.append(f"{LABEL[t]}: {len(bad)} run(s) did not succeed: {sorted(set(bad))}")
    lines.append("")
    lines += [f"> ⚠️ {s}" for s in statuses] or ["All measured runs succeeded."]
    lines.append("")
    return lines


def static_tables():
    lines = []
    fp = load("footprint") or {}
    sizes = {}
    p = os.path.join(R, "image-sizes.jsonl")
    if os.path.exists(p):
        for line in open(p):
            d = json.loads(line)
            sizes[d["image"]] = d
    stacks = {
        "otf": (["leg100/otfd:0.6.3"], ["postgres:16-alpine"], "otfd (+ optional otf-agent)", "Postgres"),
        "terrakube": (["azbuilder/api-server:2.33.3", "azbuilder/executor:2.33.3",
                       "azbuilder/open-registry:2.33.3", "azbuilder/terrakube-ui:2.33.3"],
                      ["dexidp/dex:v2.43.1", "valkey/valkey:8-alpine", "postgres:16-alpine"],
                      "api, executor, registry, ui", "Dex, Valkey, Postgres (+ MinIO/S3, Traefik in the reference compose)"),
        "atlantis": (["ghcr.io/runatlantis/atlantis:v0.49.0"], [], "atlantis", "none (state backend is yours)"),
    }
    lines += ["## Static footprint", "",
              "| | " + " | ".join(LABEL[t] for t in TOOLS) + " |", "|---|" + "---|" * len(TOOLS)]

    def size(imgs, arch):
        tot = sum(g(sizes, i, f"{arch}_compressed_bytes", default=0) for i in imgs)
        return mib(tot)
    lines.append("| App services | " + " | ".join(stacks[t][2] for t in TOOLS) + " |")
    lines.append("| Required dependencies | " + " | ".join(stacks[t][3] for t in TOOLS) + " |")
    lines.append("| App images, compressed amd64 | " + " | ".join(size(stacks[t][0], "amd64") for t in TOOLS) + " |")
    lines.append("| App images, compressed arm64 | " + " | ".join(size(stacks[t][0], "arm64") for t in TOOLS) + " |")
    lines.append("| App + deps images, arm64 | " + " | ".join(size(stacks[t][0] + stacks[t][1], "arm64") for t in TOOLS) + " |")
    lines.append("| Source lines (non-test) | " + " | ".join(
        ", ".join(f"{k} {v:,}" for k, v in g(fp, t, "loc", default={}).items()) for t in TOOLS) + " |")
    lines.append("| Declared dependencies | " + " | ".join(
        ", ".join(f"{k.replace('_', ' ')} {v}" for k, v in fp.get(t, {}).items() if k not in ("loc", "version") and v)
        for t in TOOLS) + " |")
    lines.append("")
    return lines


def main():
    base = load("baseline")
    out = ["# Benchmark results", "",
           "Generated by `lib/report.py` from the JSON in this directory. Method: see `../README.md`.", ""]
    out += static_tables()
    if base:
        r = base["runs"]
        out += [f"Baseline, plain `tofu` {base['version']} on the host with the same fixture "
                f"(100 `terraform_data`), medians: init {r['init_s']['median']} s, "
                f"plan {r['plan_s']['median']} s, apply {r['apply_s']['median']} s. "
                "Everything above that is orchestration.", ""]
    out += section("", "Unconstrained (all host CPUs)")
    out += section("-1cpu", "1-CPU budget (rough stand-in for a small ARM board)")
    floor = load("memory-floor")
    if floor:
        out += ["## Memory floor (smallest hard limit, no swap, that reaches healthy and stays up 15 s)", "",
                "| Tool | Service | Floor | RSS at floor | Time to healthy |", "|---|---|---|---|---|"]
        for tool in TOOLS:
            for svc, d in (floor.get(tool) or {}).items():
                ok = next((a for a in d["attempts"] if a["ok"]), {})
                out.append(f"| {LABEL[tool]} | {svc} | {fmt(d['floor_mib'], ' MiB')} | "
                           f"{fmt(ok.get('rss_mib'), ' MiB')} | {fmt(ok.get('ready_s'), ' s', 2)} |")
        out.append("")
    qc = load("otf-query-count")
    if qc:
        out += ["## OTF: Postgres transactions per request", "", "| Request | Transactions | Tuples returned |", "|---|---|---|"]
        out += [f"| {k} | {v['transactions_per_request']} | {v['tuples_returned_per_request']} |"
                for k, v in qc["requests"].items()]
        out.append("")
    path = os.path.join(R, "SUMMARY.md")
    with open(path, "w") as f:
        f.write("\n".join(out) + "\n")
    print(open(path).read())


if __name__ == "__main__":
    main()
