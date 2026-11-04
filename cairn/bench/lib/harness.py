"""Shared helpers for the provisioner benchmarks (stdlib only).

Every tool driver (bench/<tool>/run.py) uses the same phases so the numbers
are comparable:

  1. cold start    docker run -> first healthy HTTP response
  2. idle          settle, then a fixed idle window (CPU seconds, RSS)
  3. api load      `hey` against an authenticated read endpoint
  4. e2e serial    N sequential plan+apply runs of fixtures/tofu-bench
  5. e2e parallel  M runs at once on M workspaces/projects

Resource numbers come from cgstat.py (cgroup v1), sampled every second for
every container in the tool's stack, and are summarised per phase window.
"""
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import time
import urllib.error
import urllib.request

BENCH = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RESULTS = os.path.join(BENCH, "results")
FIXTURE = os.path.join(BENCH, "fixtures", "tofu-bench")
CGSTAT = os.path.join(BENCH, "lib", "cgstat.py")
WORK = os.environ.get("BENCH_WORK", "/tmp/cairn-bench")
TOFU_VERSION = os.environ.get("TOFU_VERSION", "1.11.6")
TOFU_BIN = os.environ.get("TOFU_BIN", os.path.join(WORK, "tofu", TOFU_VERSION, "tofu"))
HEY = os.environ.get("HEY", shutil.which("hey") or os.path.expanduser("~/go/bin/hey"))
# Outbound proxy/CA used only so containers can download engines; empty when unset.
PROXY = os.environ.get("HTTPS_PROXY", "")
CA_BUNDLE = os.environ.get("BENCH_CA_BUNDLE", "/root/.ccr/ca-bundle.crt")

# Constrained mode: BENCH_CPUS=1 puts every container of the tool's stack under
# one parent cgroup with a shared CPU quota (cgroup v1 cfs), approximating a
# small single-board host. Results are written as <tool>-<n>cpu.json.
BENCH_CPUS = os.environ.get("BENCH_CPUS")
CGROUP_PARENT = f"/cairn-bench-{BENCH_CPUS}cpu" if BENCH_CPUS else None
RUN_FLAGS = ["--cgroup-parent", CGROUP_PARENT] if CGROUP_PARENT else []
SUFFIX = f"-{BENCH_CPUS}cpu" if BENCH_CPUS else ""


def setup_cgroup():
    if not CGROUP_PARENT:
        return None
    for sub in ("cpu", "cpuacct", "memory", "cpuset", "pids", "blkio", "devices", "freezer"):
        os.makedirs(f"/sys/fs/cgroup/{sub}{CGROUP_PARENT}", exist_ok=True)
    cpu = f"/sys/fs/cgroup/cpu{CGROUP_PARENT}"
    with open(f"{cpu}/cpu.cfs_period_us", "w") as f:
        f.write("100000")
    with open(f"{cpu}/cpu.cfs_quota_us", "w") as f:
        f.write(str(int(float(BENCH_CPUS) * 100000)))
    cs = f"/sys/fs/cgroup/cpuset{CGROUP_PARENT}"
    for k in ("cpuset.cpus", "cpuset.mems"):
        with open(f"/sys/fs/cgroup/cpuset/{k}") as src, open(f"{cs}/{k}", "w") as dst:
            dst.write(src.read().strip())
    return {"cpus": float(BENCH_CPUS), "cgroup_parent": CGROUP_PARENT}


# urllib must never route localhost calls through the outbound proxy.
_opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, file=sys.stderr, flush=True)


def sh(cmd, check=True, capture=False, **kw):
    r = subprocess.run(cmd, shell=isinstance(cmd, str), text=True,
                       capture_output=capture, **kw)
    if check and r.returncode != 0:
        raise RuntimeError(f"command failed ({r.returncode}): {cmd}\n{r.stdout or ''}{r.stderr or ''}")
    return r


def docker_rm(*names):
    sh(["docker", "rm", "-f", "-v", *names], check=False, capture=True)


def http(method, url, token=None, body=None, headers=None, raw=None, timeout=60):
    h = {"Accept": "application/json"}
    if token:
        h["Authorization"] = f"Bearer {token}"
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        h["Content-Type"] = "application/vnd.api+json"
    if raw is not None:
        data = raw
        h["Content-Type"] = "application/octet-stream"
    h.update(headers or {})
    req = urllib.request.Request(url, data=data, method=method, headers=h)
    try:
        with _opener.open(req, timeout=timeout) as r:
            payload = r.read()
            status = r.status
    except urllib.error.HTTPError as e:
        payload = e.read()
        status = e.code
    try:
        return status, json.loads(payload) if payload else None
    except ValueError:
        return status, payload.decode(errors="replace")


def running(container):
    r = sh(["docker", "inspect", "-f", "{{.State.Running}}", container], check=False, capture=True)
    return r.stdout.strip() == "true"


def wait_http(url, timeout=600, ok=(200,), headers=None, interval=0.1, container=None):
    """Poll until url answers with a status in ok. Returns seconds waited.

    With container set, fail fast if that container stops while we wait.
    """
    t0 = time.monotonic()
    last_check = t0
    while time.monotonic() - t0 < timeout:
        if container and time.monotonic() - last_check > 3:
            last_check = time.monotonic()
            if not running(container):
                logs = sh(["docker", "logs", "--tail", "20", container], check=False, capture=True)
                raise RuntimeError(f"{container} exited while waiting for {url}:\n{logs.stdout}{logs.stderr}")
        try:
            req = urllib.request.Request(url, headers=headers or {})
            with _opener.open(req, timeout=2) as r:
                if r.status in ok:
                    return time.monotonic() - t0
        except urllib.error.HTTPError as e:
            if e.code in ok:
                return time.monotonic() - t0
        except Exception:
            pass
        time.sleep(interval)
    raise TimeoutError(url)


def tarball(directory=FIXTURE):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tf:
        for name in sorted(os.listdir(directory)):
            if name.startswith(".terraform") or name.endswith(".tfstate"):
                continue
            tf.add(os.path.join(directory, name), arcname=name)
    return buf.getvalue()


class Sampler:
    """Background cgstat sampler; mark() records named phase windows."""

    def __init__(self, tool, names, interval=0.2):
        self.path = os.path.join(WORK, f"{tool}-cgstat.csv")
        self.names = names
        self.interval = interval
        self.windows = {}
        self.proc = None

    def __enter__(self):
        os.makedirs(WORK, exist_ok=True)
        self.proc = subprocess.Popen([sys.executable, CGSTAT, "sample", self.path,
                                      str(self.interval), *self.names])
        return self

    def __exit__(self, *exc):
        self.proc.terminate()
        self.proc.wait()

    def window(self, name, t0, t1):
        self.windows[name] = (t0, t1)

    def summarize(self):
        time.sleep(self.interval * 1.5)  # make sure the last sample landed
        out = {}
        for name, (t0, t1) in self.windows.items():
            r = sh([sys.executable, CGSTAT, "summary", self.path, str(t0), str(t1)], capture=True)
            out[name] = json.loads(r.stdout)
        r = sh([sys.executable, CGSTAT, "summary", self.path], capture=True)
        out["whole_run"] = json.loads(r.stdout)
        return out


def hey(url, n=3200, c=16, headers=None):
    """Run hey and parse its text report into a dict.

    hey gives each worker n//c requests, so keep n a multiple of c.
    """
    cmd = [HEY, "-n", str(n), "-c", str(c), "-disable-keepalive=false"]
    for k, v in (headers or {}).items():
        cmd += ["-H", f"{k}: {v}"]
    cmd.append(url)
    env = dict(os.environ, NO_PROXY="*", no_proxy="*", HTTPS_PROXY="", HTTP_PROXY="")
    out = sh(cmd, capture=True, env=env).stdout
    res = {"requests": n, "concurrency": c, "status": {}}
    section = None
    for line in out.splitlines():
        s = line.strip()
        if s.startswith("Requests/sec:"):
            res["rps"] = round(float(s.split()[1]), 1)
        elif s.startswith("Average:"):
            res["avg_ms"] = round(float(s.split()[1]) * 1000, 2)
        elif s.startswith("Slowest:"):
            res["max_ms"] = round(float(s.split()[1]) * 1000, 2)
        elif s.startswith("Latency distribution"):
            section = "lat"
        elif s.startswith("Status code distribution"):
            section = "status"
        elif s.startswith("Error distribution"):
            section = "err"
            res["errors"] = []
        elif section == "lat" and "% in" in s:
            pct, _, val = s.replace("%%", "%").partition("% in ")
            if pct in ("50", "95", "99"):
                res[f"p{pct}_ms"] = round(float(val.split()[0]) * 1000, 2)
        elif section == "status" and s.startswith("["):
            code, cnt = s[1:].split("]")
            res["status"][code] = int(cnt.split()[0])
        elif section == "err" and s:
            res["errors"].append(s)
    return res


def api_load(url, headers):
    """Same read endpoint at concurrency 1 (latency) and 16 (throughput).

    BENCH_API_N sets the 16-client request count (default 3200; the 1-client
    pass uses a tenth). Constrained mode defaults to 640 so slow endpoints
    finish in minutes.
    """
    n = int(os.environ.get("BENCH_API_N", 640 if BENCH_CPUS else 3200))
    n -= n % 16
    return {"c1": hey(url, n=max(16, n // 10), c=1, headers=headers),
            "c16": hey(url, n=n, c=16, headers=headers)}


def stats(values):
    v = sorted(values)
    if not v:
        return {}
    pick = lambda q: v[min(len(v) - 1, int(round(q * (len(v) - 1))))]
    return {"n": len(v), "min": round(v[0], 2), "median": round(pick(0.5), 2),
            "max": round(v[-1], 2), "mean": round(sum(v) / len(v), 2)}


def baseline_tofu(runs=5):
    """Plain `tofu plan` / `tofu apply` on the host: the floor every tool sits on."""
    import tempfile
    inits, plans, applies = [], [], []
    for _ in range(runs):
        d = tempfile.mkdtemp(prefix="tofu-baseline-", dir=WORK)
        for f in os.listdir(FIXTURE):
            if f.endswith(".tf"):
                shutil.copy(os.path.join(FIXTURE, f), d)
        env = dict(os.environ, TF_IN_AUTOMATION="1")
        t = time.monotonic()
        sh([TOFU_BIN, "init", "-input=false", "-no-color"], cwd=d, capture=True, env=env)
        inits.append(time.monotonic() - t)
        t = time.monotonic()
        sh([TOFU_BIN, "plan", "-input=false", "-no-color", "-out=plan"], cwd=d, capture=True, env=env)
        plans.append(time.monotonic() - t)
        t = time.monotonic()
        sh([TOFU_BIN, "apply", "-input=false", "-no-color", "plan"], cwd=d, capture=True, env=env)
        applies.append(time.monotonic() - t)
        shutil.rmtree(d)
    return {"init_s": stats(inits), "plan_s": stats(plans), "apply_s": stats(applies)}


def save(tool, data):
    os.makedirs(RESULTS, exist_ok=True)
    data["host"] = host_info()
    data["constraint"] = setup_cgroup() if CGROUP_PARENT else None
    path = os.path.join(RESULTS, f"{tool}{SUFFIX}.json")
    with open(path, "w") as f:
        json.dump(data, f, indent=2, sort_keys=False)
        f.write("\n")
    log("wrote", path)


def host_info():
    mem = int(open("/proc/meminfo").read().split()[1]) // 1024
    cpu = [l for l in open("/proc/cpuinfo") if l.startswith("model name")]
    return {"cpus": os.cpu_count(), "cpu_model": cpu[0].split(":", 1)[1].strip() if cpu else "?",
            "mem_mib": mem, "kernel": os.uname().release, "arch": os.uname().machine,
            "date": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}


def image_size(image):
    r = sh(["docker", "image", "inspect", "-f", "{{.Size}}", image], capture=True, check=False)
    return int(r.stdout.strip()) if r.returncode == 0 else None


def start_postgres(name, port, db, user="bench", password="bench"):
    docker_rm(name)
    sh(["docker", "run", "-d", "--name", name, "--network", "host", *RUN_FLAGS,
        "-e", f"POSTGRES_USER={user}", "-e", f"POSTGRES_PASSWORD={password}",
        "-e", f"POSTGRES_DB={db}", "postgres:16-alpine", "-p", str(port)], capture=True)
    t0 = time.monotonic()
    while time.monotonic() - t0 < 60:
        r = sh(["docker", "exec", name, "pg_isready", "-p", str(port), "-U", user, "-d", db],
               check=False, capture=True)
        if r.returncode == 0:
            # pg_isready flips before the init scripts' restart; give it a beat.
            time.sleep(2)
            r = sh(["docker", "exec", name, "pg_isready", "-p", str(port), "-U", user, "-d", db],
                   check=False, capture=True)
            if r.returncode == 0:
                return
        time.sleep(0.5)
    raise TimeoutError("postgres")
