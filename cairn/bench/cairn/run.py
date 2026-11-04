#!/usr/bin/env python3
"""Benchmark Cairn (cairnd, SQLite) with the shared phases in lib/harness.py.

Deliberately uses the same TFE-compatible API calls, polling interval and
fixture as bench/otf/run.py, so the two are directly comparable:
create configuration version -> upload tarball -> create run -> poll run.

Build the image first (see ../../Dockerfile), or let build_image() do it.
"""
import os
import shutil
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "lib"))
import harness as h  # noqa: E402

TOOL = "cairn"
VERSION = "0.1.0"
IMAGE = f"cairn-bench/cairnd:{VERSION}"
PORT = 8844
BASE = f"http://localhost:{PORT}"
API = f"{BASE}/api/v2"
TOKEN = "site-token-for-benchmarks-only"
ORG = "bench"
N_WORKSPACES = int(os.environ.get("N_WORKSPACES", 50))
SERIAL_RUNS = int(os.environ.get("SERIAL_RUNS", 5))
PARALLEL_RUNS = int(os.environ.get("PARALLEL_RUNS", 5))
IDLE_SECONDS = int(os.environ.get("IDLE_SECONDS", 60))
CONTAINERS = ["cairn-cairnd"]
ROOT = os.path.abspath(os.path.join(h.BENCH, ".."))


# Same layout as ../../Dockerfile (glibc binary + its 4 libraries on scratch),
# assembled from this host's own files so no registry pull is needed.
BENCH_DOCKERFILE = """FROM scratch
COPY rootfs/ /
COPY --chown=65532:65532 data/ /data/
ENV LD_LIBRARY_PATH=/lib CAIRN_DATA=/data CAIRN_LISTEN=0.0.0.0:8833
USER 65532:65532
ENTRYPOINT ["/cairnd"]
"""


def build_image():
    if h.image_size(IMAGE) and "--rebuild" not in sys.argv:
        return
    h.sh(["cargo", "build", "--release", "-p", "cairnd"], cwd=ROOT)
    ctx = os.path.join(h.WORK, "cairn-image")
    shutil.rmtree(ctx, ignore_errors=True)
    rootfs = os.path.join(ctx, "rootfs")
    for d in ("lib", "lib64", "tmp", "etc/ssl/certs"):
        os.makedirs(os.path.join(rootfs, d))
    os.makedirs(os.path.join(ctx, "data"))
    os.chmod(os.path.join(rootfs, "tmp"), 0o1777)
    for lib in ("libc.so.6", "libm.so.6", "libgcc_s.so.1"):
        shutil.copy(f"/lib/x86_64-linux-gnu/{lib}", os.path.join(rootfs, "lib", lib))
    shutil.copy("/lib64/ld-linux-x86-64.so.2", os.path.join(rootfs, "lib64", "ld-linux-x86-64.so.2"))
    shutil.copy("/etc/ssl/certs/ca-certificates.crt", os.path.join(rootfs, "etc/ssl/certs/ca-certificates.crt"))
    shutil.copy(os.path.join(ROOT, "target", "release", "cairnd"), os.path.join(rootfs, "cairnd"))
    with open(os.path.join(ctx, "Dockerfile"), "w") as f:
        f.write(BENCH_DOCKERFILE)
    h.sh(["docker", "build", "-q", "-t", IMAGE, ctx], capture=True)


def cairnd_cmd():
    """`docker run` argv for cairnd with a fresh data dir (also used by memory_floor.py).

    The engine is hard-linked, not copied: writing 113 MB right before
    `docker run` slows container start with page-cache writeback.
    """
    data = os.path.join(h.WORK, "cairn-data")
    shutil.rmtree(data, ignore_errors=True)
    eng = os.path.join(data, "engines", h.TOFU_VERSION)
    os.makedirs(eng)
    try:
        os.link(h.TOFU_BIN, os.path.join(eng, "tofu"))
    except OSError:
        shutil.copy(h.TOFU_BIN, os.path.join(eng, "tofu"))
    h.sh(["chown", "-R", "65532:65532", data])
    return ["docker", "run", "-d", "--name", "cairn-cairnd", "--network", "host", *h.RUN_FLAGS,
            "-e", f"CAIRN_SITE_TOKEN={TOKEN}", "-e", f"CAIRN_LISTEN=127.0.0.1:{PORT}",
            "-e", f"CAIRN_DEFAULT_ENGINE_VERSION={h.TOFU_VERSION}", "-e", "CAIRN_NO_DOWNLOAD=1",
            "-e", "CAIRN_LOG=warn", "-v", f"{data}:/data", IMAGE]


def start_cairnd():
    cmd = cairnd_cmd()
    h.docker_rm("cairn-cairnd")
    t0 = time.monotonic()
    h.sh(cmd, capture=True)
    ready = h.wait_http(f"{BASE}/healthz", timeout=60, container="cairn-cairnd", interval=0.01)
    return {"healthz_s": round(time.monotonic() - t0, 2), "poll_s": round(ready, 3)}


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
    """Identical to bench/otf/run.py run_once (same calls, same 0.1 s polling)."""
    t0 = time.monotonic()
    s, b = h.http("POST", f"{API}/workspaces/{ws_id}/configuration-versions", TOKEN,
                  {"data": {"type": "configuration-versions", "attributes": {"auto-queue-runs": False}}})
    assert s in (200, 201), (s, b)
    cv_id, upload = b["data"]["id"], b["data"]["attributes"]["upload-url"]
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


def run_once_longpoll(ws_id, tar):
    """Same run, but waiting with Cairn's long-poll instead of 0.1 s polling (true latency)."""
    t0 = time.monotonic()
    s, b = h.http("POST", f"{BASE}/api/v1/stacks/{ws_id}/runs?kind=tracked&auto_apply=true", TOKEN, raw=tar)
    assert s == 201, (s, b)
    run_id = b["id"]
    while True:
        s, b = h.http("GET", f"{BASE}/api/v1/runs/{run_id}?wait=30", TOKEN)
        if b["status"] in ("applied", "planned_and_finished", "errored", "canceled", "discarded"):
            break
    return {"status": b["status"], "total_s": time.monotonic() - t0}


def main():
    build_image()
    out = {"tool": TOOL, "version": VERSION, "images": {IMAGE: h.image_size(IMAGE)}}
    h.docker_rm(*CONTAINERS)
    h.setup_cgroup()
    with h.Sampler(TOOL, CONTAINERS) as smp:
        h.log("starting cairnd")
        out["cold_start"] = start_cairnd()
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
        # Like-for-like with Atlantis's only authenticated read (GET /api/locks, empty).
        out["api_trivial"] = h.api_load(f"{BASE}/api/v1/whoami", headers={"Authorization": f"Bearer {TOKEN}"})
        out["api_trivial"]["endpoint"] = "GET /api/v1/whoami"
        h.log("api trivial", out["api_trivial"])

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

        h.log("long-poll runs (native API, no polling granularity)")
        lp = [run_once_longpoll(ws[20 + i], tar) for i in range(SERIAL_RUNS)]
        out["e2e_longpoll"] = {"total_s": h.stats([r["total_s"] for r in lp]),
                               "statuses": [r["status"] for r in lp]}
        h.log("long-poll", out["e2e_longpoll"])
        out["resources"] = smp.summarize()
    h.save(TOOL, out)
    if "--keep" not in sys.argv:
        h.docker_rm(*CONTAINERS)


if __name__ == "__main__":
    main()
