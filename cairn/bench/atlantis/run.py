#!/usr/bin/env python3
"""Benchmark Atlantis with the shared phases in lib/harness.py.

Atlantis has no workspace inventory or run queue; it is driven by VCS
webhooks (or its small /api). So the e2e phases use:
  * /api/plan and /api/apply (synchronous; apply = plan + apply), which is
    the closest analogue of "create a run" in OTF/Terrakube, and
  * the real PR workflow against local Gitea: open PR -> autoplan comment,
    comment `atlantis apply` -> apply comment, close PR (releases locks).

State: the fixture uses the local backend, so Atlantis does not upload
state anywhere (it "brings no backend"). OTF/Terrakube store state
themselves; with 100 terraform_data resources that is a ~40 KB object, so
this favours Atlantis only slightly. No-op runs are therefore not measured.
"""
import os
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "lib"))
import harness as h  # noqa: E402
import vcs  # noqa: E402

TOOL = "atlantis"
VERSION = "v0.49.0"
IMAGE = f"cairn-bench/atlantis:{VERSION}"
PORT = 4141
BASE = f"http://localhost:{PORT}"
API_SECRET = "bench-api-secret"
WEBHOOK_SECRET = "bench-webhook-secret"
REPO = "tofu-bench"
SERIAL_RUNS = int(os.environ.get("SERIAL_RUNS", 5))
PARALLEL_RUNS = int(os.environ.get("PARALLEL_RUNS", 5))
PR_RUNS = int(os.environ.get("PR_RUNS", 3))
IDLE_SECONDS = int(os.environ.get("IDLE_SECONDS", 60))
CONTAINERS = ["atlantis"]
HDR = {"X-Atlantis-Token": API_SECRET, "Content-Type": "application/json"}
REQ = {"Repository": f"{vcs.USER}/{REPO}", "Ref": "main", "Type": "Gitea",
       "Paths": [{"Directory": ".", "Workspace": "default"}]}


def build_image():
    """Build the binary from the submodule and wrap it (see Dockerfile)."""
    if h.image_size(IMAGE):
        return
    ctx = os.path.join(h.WORK, "atlantis-ctx")
    os.makedirs(ctx, exist_ok=True)
    src = os.path.join(h.BENCH, "..", "upstream", "atlantis")
    t = time.monotonic()
    h.sh(["go", "build", "-trimpath", "-ldflags",
          f"-s -w -X main.version={VERSION} -X main.commit=5718f12", "-o",
          os.path.join(ctx, "atlantis"), "."], cwd=src, env=dict(os.environ, CGO_ENABLED="0"))
    h.log(f"go build took {time.monotonic() - t:.0f}s")
    h.sh(f"cp {h.TOFU_BIN} {ctx}/tofu && cp {h.BENCH}/atlantis/Dockerfile {ctx}/")
    h.sh(["docker", "build", "-q", "--network", "host", "-t", IMAGE, ctx], capture=True)


def atlantis_cmd(token):
    """`docker run` argv for Atlantis (also used by memory_floor.py)."""
    return ["docker", "run", "-d", "--name", "atlantis", "--network", "host", *h.RUN_FLAGS,
          "-e", f"ATLANTIS_PORT={PORT}", "-e", f"ATLANTIS_ATLANTIS_URL={BASE}",
          "-e", f"ATLANTIS_GITEA_BASE_URL={vcs.URL}", "-e", f"ATLANTIS_GITEA_USER={vcs.USER}",
          "-e", f"ATLANTIS_GITEA_TOKEN={token}", "-e", f"ATLANTIS_GITEA_WEBHOOK_SECRET={WEBHOOK_SECRET}",
          "-e", "ATLANTIS_REPO_ALLOWLIST=*", "-e", f"ATLANTIS_API_SECRET={API_SECRET}",
          "-e", "ATLANTIS_DEFAULT_TF_DISTRIBUTION=opentofu",
          "-e", f"ATLANTIS_DEFAULT_TF_VERSION={h.TOFU_VERSION}", "-e", "ATLANTIS_TF_DOWNLOAD=false",
          "-e", "ATLANTIS_DATA_DIR=/home/atlantis/data",
          IMAGE, "server"]


def start_atlantis(token):
    cmd = atlantis_cmd(token)
    h.docker_rm("atlantis")
    t0 = time.monotonic()
    h.sh(cmd, capture=True)
    ready = h.wait_http(f"{BASE}/healthz", timeout=300, container="atlantis")
    return {"healthz_s": round(time.monotonic() - t0, 2), "poll_s": round(ready, 2)}


def api_run(endpoint, directory="."):
    t0 = time.monotonic()
    req = dict(REQ, Paths=[{"Directory": directory, "Workspace": "default"}])
    s, b = h.http("POST", f"{BASE}/api/{endpoint}", headers=HDR, body=req, timeout=600)
    ok = s == 200
    return {"status": "ok" if ok else f"http {s}", "total_s": time.monotonic() - t0,
            "detail": None if ok else str(b)[:300]}


def wait_comment(number, needle, timeout=300):
    t0 = time.monotonic()
    while time.monotonic() - t0 < timeout:
        for c in vcs.comments(REPO, number):
            if needle in c["body"]:
                return time.monotonic()
        time.sleep(0.2)
    raise TimeoutError(f"PR #{number}: no comment containing {needle!r}")


def wait_unlocked(timeout=60):
    t0 = time.monotonic()
    while time.monotonic() - t0 < timeout:
        s, b = h.http("GET", f"{BASE}/api/locks", headers=HDR)
        if s == 200 and not (b or {}).get("Locks"):
            return time.monotonic()
        time.sleep(0.2)
    raise TimeoutError("Atlantis still holds project locks")


def pr_flow(i):
    branch = f"bench-pr-{i}-{int(time.time())}"
    vcs.push_branch(REPO, branch, branch)
    t0 = time.monotonic()
    number = vcs.open_pr(REPO, branch, branch)
    t_plan = wait_comment(number, "Ran Plan")
    t1 = time.monotonic()
    vcs.comment(REPO, number, "atlantis apply")
    t_apply = wait_comment(number, "Ran Apply")
    # Merge rather than close: Atlantis' Gitea parser only treats *merged* PRs as
    # closed (ParseGiteaPullRequestEvent); a closed-unmerged PR keeps its locks.
    t2 = time.monotonic()
    vcs.merge_pr(REPO, number)
    t_unlock = wait_unlocked()
    return {"pr_open_to_plan_comment_s": t_plan - t0, "apply_comment_to_result_s": t_apply - t1,
            "merge_to_locks_released_s": t_unlock - t2}


def main():
    build_image()
    out = {"tool": TOOL, "version": VERSION, "images": {IMAGE: h.image_size(IMAGE)},
           "image_note": "locally built (see atlantis/Dockerfile); upstream ghcr.io sizes in image-sizes.jsonl"}
    token = vcs.up()
    layout = os.path.join(h.WORK, "atlantis-fixture")
    h.sh(f"rm -rf {layout} && mkdir -p {layout} && cp {h.FIXTURE}/*.tf {layout}/")
    for i in range(PARALLEL_RUNS):
        h.sh(f"mkdir -p {layout}/p{i} && cp {h.FIXTURE}/*.tf {layout}/p{i}/")
    vcs.push_dir(REPO, layout)
    vcs.add_webhook(REPO, f"{BASE}/events", WEBHOOK_SECRET,
                    ["pull_request", "pull_request_sync", "issue_comment", "pull_request_comment"])
    h.docker_rm(*CONTAINERS)
    h.setup_cgroup()
    with h.Sampler(TOOL, CONTAINERS) as smp:
        h.log("starting atlantis")
        out["cold_start"] = start_atlantis(token)
        h.log("cold start", out["cold_start"])

        time.sleep(30)
        t0 = time.time()
        time.sleep(IDLE_SECONDS)
        smp.window("idle", t0, time.time())

        h.log("api load")
        t0 = time.time()
        out["api_load"] = h.api_load(f"{BASE}/api/locks", headers={"X-Atlantis-Token": API_SECRET})
        smp.window("api_load", t0, time.time())
        out["api_load"]["endpoint"] = "GET /api/locks (only authenticated read API; no inventory to list)"
        h.log("api load", out["api_load"])

        h.log("warm-up run")
        out["first_run"] = api_run("apply")
        h.log("first run", out["first_run"])

        h.log("serial plan-only and plan+apply via /api")
        t0 = time.time()
        plans = [api_run("plan") for _ in range(SERIAL_RUNS)]
        smp.window("e2e_plans", t0, time.time())
        t0 = time.time()
        applies = [api_run("apply") for _ in range(SERIAL_RUNS)]
        smp.window("e2e_serial", t0, time.time())
        out["e2e_serial"] = {"create_to_planned_s": h.stats([r["total_s"] for r in plans]),
                             "create_to_applied_s": h.stats([r["total_s"] for r in applies]),
                             "total_s": h.stats([r["total_s"] for r in applies]),
                             "statuses": [r["status"] for r in applies],
                             "plan_statuses": [r["status"] for r in plans],
                             "errors": [r["detail"] for r in plans + applies if r["detail"]]}
        h.log("serial", out["e2e_serial"])

        h.log("parallel /api/apply")
        res = [None] * PARALLEL_RUNS
        def worker(i):
            # Atlantis locks per (repo, dir, workspace), so parallel runs need distinct projects.
            res[i] = api_run("apply", f"p{i}")
        t0 = time.time()
        threads = [threading.Thread(target=worker, args=(i,)) for i in range(PARALLEL_RUNS)]
        [t.start() for t in threads]
        [t.join() for t in threads]
        t1 = time.time()
        smp.window("e2e_parallel", t0, t1)
        out["e2e_parallel"] = {"runs": PARALLEL_RUNS, "wall_s": round(t1 - t0, 2),
                               "per_run_total_s": h.stats([r["total_s"] for r in res]),
                               "statuses": [r["status"] for r in res],
                               "errors": [r["detail"] for r in res if r["detail"]]}
        h.log("parallel", out["e2e_parallel"])

        h.log("PR workflow via Gitea webhooks")
        t0 = time.time()
        prs = [pr_flow(i) for i in range(PR_RUNS)]
        smp.window("pr_workflow", t0, time.time())
        out["pr_workflow"] = {k: h.stats([p[k] for p in prs]) for k in prs[0]}
        h.log("pr workflow", out["pr_workflow"])
        out["resources"] = smp.summarize()
    h.save(TOOL, out)
    if "--keep" not in sys.argv:
        h.docker_rm(*CONTAINERS)


if __name__ == "__main__":
    main()
