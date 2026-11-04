#!/usr/bin/env python3
"""Benchmark OTF (otfd + Postgres) with the shared phases in lib/harness.py.

Runs go through OTF's TFE-compatible API, which is exactly what `tofu` does
with a `cloud {}` block: create configuration version -> upload tarball ->
create run -> poll. The fork executor runs tofu inside the otfd container.
"""
import os
import shutil
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "lib"))
import harness as h  # noqa: E402

TOOL = "otf"
VERSION = "0.6.3"
IMAGE = f"leg100/otfd:{VERSION}"
PORT = 8833
PG_PORT = 5433
BASE = f"http://localhost:{PORT}"
API = f"{BASE}/api/v2"
TOKEN = "site-token-for-benchmarks-only"
ORG = "bench"
N_WORKSPACES = int(os.environ.get("N_WORKSPACES", 50))
SERIAL_RUNS = int(os.environ.get("SERIAL_RUNS", 5))
PARALLEL_RUNS = int(os.environ.get("PARALLEL_RUNS", 5))
IDLE_SECONDS = int(os.environ.get("IDLE_SECONDS", 60))
CONTAINERS = ["otf-otfd", "otf-postgres"]


def otfd_cmd():
    """`docker run` argv for otfd (also used by memory_floor.py)."""
    bins = os.path.join(h.WORK, "otf-bins")
    dst = os.path.join(bins, h.TOFU_VERSION)
    os.makedirs(dst, exist_ok=True)
    if not os.path.exists(os.path.join(dst, "tofu")):
        shutil.copy(h.TOFU_BIN, os.path.join(dst, "tofu"))
    h.sh(["chown", "-R", "4096:4096", bins])
    return ["docker", "run", "-d", "--name", "otf-otfd", "--network", "host", *h.RUN_FLAGS,
          "-e", f"OTF_DATABASE=postgres://bench:bench@localhost:{PG_PORT}/otf?sslmode=disable",
          "-e", "OTF_SECRET=6b07b57377755b07cf61709780ee7484",
          "-e", f"OTF_SITE_TOKEN={TOKEN}",
          "-e", f"OTF_ADDRESS=:{PORT}",
          "-e", f"OTF_HOSTNAME=localhost:{PORT}",
          "-e", "OTF_DEFAULT_ENGINE=tofu",
          "-e", "OTF_ENGINE_BINS_DIR=/engine-bins",
          "-e", f"HTTPS_PROXY={h.PROXY}", "-e", "NO_PROXY=localhost,127.0.0.1",
          "-e", "SSL_CERT_FILE=/ca.crt", "-v", f"{h.CA_BUNDLE}:/ca.crt:ro",
          "-v", f"{bins}:/engine-bins",
          IMAGE]


def start_otfd():
    cmd = otfd_cmd()
    h.docker_rm("otf-otfd")
    t0 = time.monotonic()
    h.sh(cmd, capture=True)
    ready = h.wait_http(f"{BASE}/healthz", timeout=300, container="otf-otfd")
    return {"healthz_s": round(time.monotonic() - t0, 2), "poll_s": round(ready, 2)}


def create_org():
    s, b = h.http("POST", f"{API}/organizations", TOKEN,
                  {"data": {"type": "organizations", "attributes": {"name": ORG, "email": "bench@example.com"}}})
    assert s in (200, 201), (s, b)


def create_workspace(name):
    s, b = h.http("POST", f"{API}/organizations/{ORG}/workspaces", TOKEN,
                  {"data": {"type": "workspaces", "attributes": {
                      "name": name, "auto-apply": True, "terraform-version": h.TOFU_VERSION}}})
    assert s in (200, 201), (s, b)
    return b["data"]["id"]


def run_once(ws_id, tar):
    """Upload config and drive one plan+apply run. Returns timing dict."""
    t0 = time.monotonic()
    s, b = h.http("POST", f"{API}/workspaces/{ws_id}/configuration-versions", TOKEN,
                  {"data": {"type": "configuration-versions", "attributes": {"auto-queue-runs": False}}})
    assert s in (200, 201), (s, b)
    cv_id, upload = b["data"]["id"], b["data"]["attributes"]["upload-url"]
    if upload.startswith("/"):
        upload = BASE + upload
    s, b = h.http("PUT", upload, TOKEN, raw=tar)
    assert s in (200, 201, 204), (s, b)
    t_upload = time.monotonic()
    s, b = h.http("POST", f"{API}/runs", TOKEN, {"data": {
        "type": "runs", "attributes": {"auto-apply": True},
        "relationships": {
            "workspace": {"data": {"type": "workspaces", "id": ws_id}},
            "configuration-version": {"data": {"type": "configuration-versions", "id": cv_id}}}}})
    assert s in (200, 201), (s, b)
    run_id = b["data"]["id"]
    t_created = time.monotonic()
    planned = None
    while True:
        s, b = h.http("GET", f"{API}/runs/{run_id}", TOKEN)
        st = b["data"]["attributes"]["status"]
        if planned is None and st in ("planned", "apply_queued", "applying", "applied", "planned_and_finished"):
            planned = time.monotonic()
        if st in ("applied", "planned_and_finished", "errored", "canceled", "discarded"):
            break
        time.sleep(0.1)
    done = time.monotonic()
    return {"status": st, "upload_s": t_upload - t0, "create_to_planned_s": planned - t_created,
            "create_to_applied_s": done - t_created, "total_s": done - t0}


def main():
    out = {"tool": TOOL, "version": VERSION, "images": {IMAGE: h.image_size(IMAGE),
                                                         "postgres:16-alpine": h.image_size("postgres:16-alpine")}}
    h.docker_rm(*CONTAINERS)
    h.setup_cgroup()
    h.start_postgres("otf-postgres", PG_PORT, "otf")
    with h.Sampler(TOOL, CONTAINERS) as smp:
        h.log("starting otfd")
        out["cold_start"] = start_otfd()
        h.log("cold start", out["cold_start"])

        time.sleep(30)
        t0 = time.time()
        time.sleep(IDLE_SECONDS)
        smp.window("idle", t0, time.time())

        create_org()
        t = time.monotonic()
        ws = [create_workspace(f"ws-{i:03d}") for i in range(N_WORKSPACES)]
        out["create_workspace_avg_ms"] = round((time.monotonic() - t) / N_WORKSPACES * 1000, 1)

        h.log("api load")
        t0 = time.time()
        out["api_load"] = h.api_load(f"{API}/organizations/{ORG}/workspaces?page%5Bsize%5D=50",
                                     headers={"Authorization": f"Bearer {TOKEN}"})
        smp.window("api_load", t0, time.time())
        out["api_load"]["endpoint"] = "GET /api/v2/organizations/:org/workspaces (all 50 in one page)"
        h.log("api load", out["api_load"])

        tar = h.tarball()
        h.log("warm-up run")
        out["first_run"] = run_once(ws[0], tar)
        h.log("first run", out["first_run"])

        h.log("serial runs (fresh workspace each: 100 creates)")
        t0 = time.time()
        serial = [run_once(ws[10 + i], tar) for i in range(SERIAL_RUNS)]
        smp.window("e2e_serial", t0, time.time())
        out["e2e_serial"] = {k: h.stats([r[k] for r in serial])
                             for k in ("create_to_planned_s", "create_to_applied_s", "total_s")}
        out["e2e_serial"]["statuses"] = [r["status"] for r in serial]
        h.log("serial", out["e2e_serial"])

        h.log("no-op runs (same workspace, nothing to change)")
        noop = [run_once(ws[0], tar) for _ in range(SERIAL_RUNS)]
        out["e2e_noop"] = {"total_s": h.stats([r["total_s"] for r in noop]),
                           "statuses": [r["status"] for r in noop]}
        h.log("noop", out["e2e_noop"])

        h.log("parallel runs")
        res = [None] * PARALLEL_RUNS
        def worker(i):
            res[i] = run_once(ws[30 + i], tar)
        t0 = time.time()
        threads = [threading.Thread(target=worker, args=(i,)) for i in range(PARALLEL_RUNS)]
        [t.start() for t in threads]
        [t.join() for t in threads]
        t1 = time.time()
        smp.window("e2e_parallel", t0, t1)
        out["e2e_parallel"] = {"runs": PARALLEL_RUNS, "wall_s": round(t1 - t0, 2),
                               "per_run_total_s": h.stats([r["total_s"] for r in res]),
                               "statuses": [r["status"] for r in res]}
        h.log("parallel", out["e2e_parallel"])
        out["resources"] = smp.summarize()
    h.sh(["docker", "logs", "otf-otfd"], capture=True, check=False)
    h.save(TOOL, out)
    if "--keep" not in sys.argv:
        h.docker_rm(*CONTAINERS)


if __name__ == "__main__":
    main()
