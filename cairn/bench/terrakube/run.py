#!/usr/bin/env python3
"""Benchmark Terrakube with the shared phases in lib/harness.py.

Stack (the smallest that does real runs): api, executor, registry, ui,
Dex, Valkey, Postgres. Compared with docker-compose/docker-compose.yml we
drop Traefik, OpenLDAP and MinIO (state/logs go to a shared local volume),
which only makes Terrakube look lighter than the reference deployment.

Auth: a Terrakube PAT is an HS256 JWT signed with PatSecret (see
api/.../token/pat/PatService.java). We mint one with TERRAKUBE_ADMIN in
`groups`, which is exactly what the UI hands out after a Dex login.

Engine binaries: the executor resolves tofu versions from a GitHub releases
list proxied by the API. A tiny local mirror (CustomTofuReleasesUrl) serves
that list and the zip, matching the pre-seeded engine used for OTF/Atlantis.
"""
import base64
import hashlib
import hmac
import http.server
import json
import os
import shutil
import socketserver
import sys
import threading
import time
import uuid
import zipfile

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "lib"))
import harness as h  # noqa: E402
import vcs  # noqa: E402

TOOL = "terrakube"
VERSION = "2.33.3"
IMAGES = {s: f"azbuilder/{s}:{VERSION}" for s in ("api-server", "executor", "open-registry", "terrakube-ui")}
DEX_IMAGE = "dexidp/dex:v2.43.1"
VALKEY_IMAGE = "valkey/valkey:8-alpine"
API_PORT, REG_PORT, EXE_PORT, UI_PORT, DEX_PORT, REDIS_PORT, PG_PORT, MIRROR_PORT = (
    8080, 8075, 8090, 3001, 5556, 6380, 5434, 9999)
API = f"http://localhost:{API_PORT}"
PAT_SECRET = base64.urlsafe_b64encode(b"pat-secret-for-benchmarks-only!!").decode().rstrip("=")
INTERNAL_SECRET = base64.urlsafe_b64encode(b"internal-secret-for-benchmarks!!").decode().rstrip("=")
REDIS_PASSWORD = "bench-redis"
N_WORKSPACES = int(os.environ.get("N_WORKSPACES", 50))
SERIAL_RUNS = int(os.environ.get("SERIAL_RUNS", 5))
PARALLEL_RUNS = int(os.environ.get("PARALLEL_RUNS", 5))
IDLE_SECONDS = int(os.environ.get("IDLE_SECONDS", 60))
JAVA = ["tk-api", "tk-executor", "tk-registry"]
CONTAINERS = JAVA + ["tk-ui", "tk-dex", "tk-redis", "tk-postgres"]
JSONAPI = {"Content-Type": "application/vnd.api+json", "Accept": "application/vnd.api+json"}


def b64(b):
    return base64.urlsafe_b64encode(b).decode().rstrip("=")


def mint_pat():
    hdr = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    now = int(time.time())
    body = b64(json.dumps({
        "iss": "Terrakube", "aud": "Terrakube", "sub": "bench (Token)", "jti": str(uuid.uuid4()),
        "email": "admin@example.com", "email_verified": True, "name": "bench (Token)",
        "groups": ["TERRAKUBE_ADMIN"], "iat": now, "exp": now + 86400}).encode())
    key = base64.urlsafe_b64decode(PAT_SECRET + "=" * (-len(PAT_SECRET) % 4))
    sig = b64(hmac.new(key, f"{hdr}.{body}".encode(), hashlib.sha256).digest())
    return f"{hdr}.{body}.{sig}"


class Mirror:
    """Serves tofu-releases.json, terraform-index.json and the tofu zip."""

    def __enter__(self):
        d = os.path.join(h.WORK, "tk-mirror")
        os.makedirs(d, exist_ok=True)
        z = os.path.join(d, f"tofu_{h.TOFU_VERSION}_linux_amd64.zip")
        if not os.path.exists(z):
            with zipfile.ZipFile(z, "w", zipfile.ZIP_DEFLATED) as zf:
                zf.write(h.TOFU_BIN, "tofu")
        url = f"http://localhost:{MIRROR_PORT}"
        with open(os.path.join(d, "tofu-releases.json"), "w") as f:
            json.dump([{"name": f"v{h.TOFU_VERSION}", "tag_name": f"v{h.TOFU_VERSION}", "assets": [
                {"name": os.path.basename(z), "browser_download_url": f"{url}/{os.path.basename(z)}"}]}], f)
        with open(os.path.join(d, "terraform-index.json"), "w") as f:
            json.dump({"name": "terraform", "versions": {}}, f)
        handler = lambda *a, **k: http.server.SimpleHTTPRequestHandler(*a, directory=d, **k)
        http.server.SimpleHTTPRequestHandler.log_message = lambda *a: None
        socketserver.TCPServer.allow_reuse_address = True
        self.srv = socketserver.ThreadingTCPServer(("127.0.0.1", MIRROR_PORT), handler)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        return url

    def __exit__(self, *a):
        self.srv.shutdown()


def dex_config():
    path = os.path.join(h.WORK, "tk-dex.yaml")
    with open(path, "w") as f:
        f.write(f"""issuer: http://localhost:{DEX_PORT}/dex
storage:
  type: memory
web:
  http: 0.0.0.0:{DEX_PORT}
oauth2:
  skipApprovalScreen: true
enablePasswordDB: true
staticClients:
  - id: terrakube-app
    name: Terrakube
    public: true
    redirectURIs: ["http://localhost:{UI_PORT}"]
staticPasswords:
  - email: admin@example.com
    # bcrypt("password")
    hash: "$2a$10$2b2cU8CPhOTaGrs1HRQuAueS7JTT5ZHsHSzYiFPm1leZck7Mc8T4W"
    username: admin
    userID: 08a8684b-db88-4b73-90a9-3cd1661f5466
""")
    return path


def start_deps(mirror):
    h.start_postgres("tk-postgres", PG_PORT, "terrakube")
    h.sh(["docker", "run", "-d", "--name", "tk-redis", "--network", "host", *h.RUN_FLAGS, VALKEY_IMAGE,
          "valkey-server", "--port", str(REDIS_PORT), "--requirepass", REDIS_PASSWORD], capture=True)
    h.sh(["docker", "run", "-d", "--name", "tk-dex", "--network", "host", *h.RUN_FLAGS,
          "-v", f"{dex_config()}:/etc/dex/config.yaml:ro", DEX_IMAGE,
          "dex", "serve", "/etc/dex/config.yaml"], capture=True)
    h.wait_http(f"http://localhost:{DEX_PORT}/dex/.well-known/openid-configuration", timeout=60)


NAMES = {"api": "tk-api", "executor": "tk-executor", "registry": "tk-registry", "ui": "tk-ui"}
HEALTH = {"api": f"{API}/actuator/health/readiness",
          "executor": f"http://localhost:{EXE_PORT}/actuator/health",
          "registry": f"http://localhost:{REG_PORT}/actuator/health",
          "ui": f"http://localhost:{UI_PORT}/"}


def service_cmds(mirror_url, tools_repo):
    """`docker run` argv per Terrakube service (also used by memory_floor.py)."""
    shared = os.path.join(h.WORK, "tk-local")
    shutil.rmtree(shared, ignore_errors=True)
    os.makedirs(shared)
    h.sh(["chown", "-R", "1002:1000", shared])
    # LOCAL storage lives under ~/.terraform-spring-boot on every service; share the whole
    # directory (mounting only .../local leaves a root-owned parent the executor can't write).
    vol = ["-v", f"{shared}:/home/cnb/.terraform-spring-boot"]
    redis = ["-e", "TerrakubeRedisHostname=localhost", "-e", f"TerrakubeRedisPort={REDIS_PORT}",
             "-e", f"TerrakubeRedisPassword={REDIS_PASSWORD}"]
    common = ["-e", f"PatSecret={PAT_SECRET}", "-e", f"InternalSecret={INTERNAL_SECRET}",
              "-e", f"DexIssuerUri=http://localhost:{DEX_PORT}/dex",
              "-e", f"TerrakubeUiURL=http://localhost:{UI_PORT}",
              "-e", f"TerrakubeToolsRepository={tools_repo}", "-e", "TerrakubeToolsBranch=main",
              "-e", f"CustomTerraformReleasesUrl={mirror_url}/terraform-index.json"]
    api = ["docker", "run", "-d", "--name", "tk-api", "--network", "host", *h.RUN_FLAGS, *vol, *redis, *common,
           "-e", "ApiDataSourceType=POSTGRESQL", "-e", "DatasourceHostname=localhost",
           "-e", f"DatasourcePort={PG_PORT}", "-e", "DatasourceDatabase=terrakube",
           "-e", "DatasourceUser=bench", "-e", "DatasourcePassword=bench",
           "-e", "GroupValidationType=DEX", "-e", "UserValidationType=DEX",
           "-e", "AuthenticationValidationType=DEX", "-e", f"TerrakubeHostname=localhost:{API_PORT}",
           "-e", f"AzBuilderExecutorUrl=http://localhost:{EXE_PORT}/api/v1/terraform-rs",
           "-e", "StorageType=LOCAL", "-e", "DexClientId=terrakube-app",
           "-e", f"CustomTofuReleasesUrl={mirror_url}/tofu-releases.json",
           IMAGES["api-server"]]
    exe = ["docker", "run", "-d", "--name", "tk-executor", "--network", "host", *h.RUN_FLAGS, *vol, *redis, *common,
           "-e", "TerrakubeEnableSecurity=true", "-e", "TerraformStateType=LocalTerraformStateImpl",
           "-e", "TerraformOutputType=LocalTerraformOutputImpl",
           "-e", f"AzBuilderApiUrl={API}", "-e", f"TerrakubeApiUrl={API}",
           "-e", "ExecutorFlagBatch=false", "-e", "ExecutorFlagDisableAcknowledge=false",
           "-e", f"TerrakubeRegistryDomain=localhost:{REG_PORT}",
           IMAGES["executor"]]
    reg = ["docker", "run", "-d", "--name", "tk-registry", "--network", "host", *h.RUN_FLAGS, *vol, *common,
           "-e", f"AzBuilderRegistry=http://localhost:{REG_PORT}", "-e", f"AzBuilderApiUrl={API}",
           "-e", "AuthenticationValidationTypeRegistry=DEX", "-e", "TerrakubeEnableSecurity=true",
           "-e", "RegistryStorageType=Local", "-e", "AppClientId=terrakube-app",
           "-e", f"AppIssuerUri=http://localhost:{DEX_PORT}/dex",
           IMAGES["open-registry"]]
    # The image's nginx config also listens on [::]:8080; strip that so it starts on IPv4-only hosts.
    conf = os.path.join(h.WORK, "tk-ui-default.conf")
    r = h.sh(["docker", "run", "--rm", "--entrypoint", "cat", IMAGES["terrakube-ui"],
              "/etc/nginx/conf.d/default.conf"], capture=True)
    with open(conf, "w") as f:
        f.write("\n".join(l for l in r.stdout.splitlines() if "[::]" not in l) + "\n")
    ui = ["docker", "run", "-d", "--name", "tk-ui", *h.RUN_FLAGS, "-p", f"{UI_PORT}:8080",
          "-v", f"{conf}:/etc/nginx/conf.d/default.conf:ro",
          "-e", f"REACT_APP_TERRAKUBE_API_URL={API}/api/v1/", "-e", "REACT_APP_CLIENT_ID=terrakube-app",
          "-e", f"REACT_APP_AUTHORITY=http://localhost:{DEX_PORT}/dex",
          "-e", f"REACT_APP_REDIRECT_URI=http://localhost:{UI_PORT}",
          "-e", f"REACT_APP_REGISTRY_URI=http://localhost:{REG_PORT}",
          "-e", "REACT_APP_SCOPE=email openid profile offline_access groups",
          IMAGES["terrakube-ui"]]
    return {"api": api, "executor": exe, "registry": reg, "ui": ui}


def start_terrakube(mirror_url, tools_repo):
    cmds = service_cmds(mirror_url, tools_repo)
    t0 = time.monotonic()
    for cmd in cmds.values():  # same as `docker compose up`: everything at once
        h.sh(cmd, capture=True)
    ready = {}
    def wait(name, url):
        h.wait_http(url, timeout=600, container=NAMES[name])
        ready[name] = round(time.monotonic() - t0, 2)
    ts = [threading.Thread(target=wait, args=kv) for kv in HEALTH.items()]
    [t.start() for t in ts]
    [t.join() for t in ts]
    ready["all_s"] = max(ready.values())
    return ready


def jget(path, tok):
    return h.http("GET", f"{API}/api/v1{path}", tok, headers=JSONAPI)


def jpost(path, tok, body):
    return h.http("POST", f"{API}/api/v1{path}", tok, body, headers=JSONAPI)


def create_org(tok):
    s, b = jpost("/organization", tok, {"data": {"type": "organization", "attributes": {
        "name": "bench", "description": "benchmark", "executionMode": "remote"}}})
    assert s in (200, 201), (s, b)
    org = b["data"]["id"]
    # Org admins still need a team grant before they can manage workspaces/jobs.
    perms = {k: True for k in ("manageState", "manageCollection", "manageJob", "manageWorkspace",
                               "manageModule", "manageProvider", "manageVcs", "manageTemplate",
                               "planJob", "approveJob")}
    s, b = jpost(f"/organization/{org}/team", tok, {"data": {"type": "team", "attributes": {
        "name": "TERRAKUBE_ADMIN", **perms}}})
    assert s in (200, 201), (s, b)
    s, b = jget(f"/organization/{org}/template", tok)
    tmpl = {t["attributes"]["name"]: t["id"] for t in b["data"]}
    return org, tmpl["Plan and apply"]


def create_workspace(tok, org, name, source):
    s, b = jpost(f"/organization/{org}/workspace", tok, {"data": {"type": "workspace", "attributes": {
        "name": name, "source": source, "branch": "main", "folder": "/", "iacType": "tofu",
        "terraformVersion": h.TOFU_VERSION, "executionMode": "remote"}}})
    assert s in (200, 201), (s, b)
    return b["data"]["id"]


def run_once(tok, org, ws, template):
    t0 = time.monotonic()
    s, b = jpost(f"/organization/{org}/job", tok, {"data": {
        "type": "job", "attributes": {"templateReference": template},
        "relationships": {"workspace": {"data": {"type": "workspace", "id": ws}}}}})
    assert s in (200, 201), (s, b)
    job = b["data"]["id"]
    planned = None
    while True:
        s, b = jget(f"/organization/{org}/job/{job}?include=step", tok)
        st = b["data"]["attributes"]["status"]
        steps = [i for i in b.get("included", []) if i["type"] == "step"]
        if planned is None and steps:
            first = min(steps, key=lambda x: x["attributes"]["stepNumber"])
            if first["attributes"]["status"] in ("completed", "noChanges"):
                planned = time.monotonic()
        if st in ("completed", "noChanges", "failed", "cancelled", "rejected"):
            break
        time.sleep(0.1)
    done = time.monotonic()
    return {"status": st, "create_to_planned_s": (planned or done) - t0,
            "create_to_applied_s": done - t0, "total_s": done - t0}


def main():
    out = {"tool": TOOL, "version": VERSION,
           "images": {i: h.image_size(i) for i in [*IMAGES.values(), DEX_IMAGE, VALKEY_IMAGE, "postgres:16-alpine"]}}
    vcs.up()
    source = vcs.push_dir("tofu-bench", h.FIXTURE)
    tools = vcs.mirror_github("terrakube-io/terrakube-extensions", "terrakube-extensions")
    h.docker_rm(*CONTAINERS)
    h.setup_cgroup()
    with Mirror() as mirror:
        start_deps(mirror)
        with h.Sampler(TOOL, CONTAINERS) as smp:
            h.log("starting terrakube (api, executor, registry, ui)")
            out["cold_start"] = start_terrakube(mirror, tools)
            h.log("cold start", out["cold_start"])

            time.sleep(30)
            t0 = time.time()
            time.sleep(IDLE_SECONDS)
            smp.window("idle", t0, time.time())

            tok = mint_pat()
            org, template = create_org(tok)
            t = time.monotonic()
            ws = [create_workspace(tok, org, f"ws-{i:03d}", source) for i in range(N_WORKSPACES)]
            out["create_workspace_avg_ms"] = round((time.monotonic() - t) / N_WORKSPACES * 1000, 1)

            h.log("api load")
            t0 = time.time()
            # Terrakube rejects page[size] on workspaces ("Cannot paginate workspace"): the UI
            # fetches every workspace and pages client-side, so list all 50 (OTF does the same).
            out["api_load"] = h.api_load(f"{API}/api/v1/organization/{org}/workspace",
                                         headers={"Authorization": f"Bearer {tok}", **JSONAPI})
            smp.window("api_load", t0, time.time())
            out["api_load"]["endpoint"] = "GET /api/v1/organization/:org/workspace (all 50; endpoint cannot paginate)"
            h.log("api load", out["api_load"])

            h.log("warm-up run (includes engine download from the local mirror)")
            out["first_run"] = run_once(tok, org, ws[0], template)
            h.log("first run", out["first_run"])

            h.log("serial runs (fresh workspace each: 100 creates)")
            t0 = time.time()
            serial = [run_once(tok, org, ws[10 + i], template) for i in range(SERIAL_RUNS)]
            smp.window("e2e_serial", t0, time.time())
            out["e2e_serial"] = {k: h.stats([r[k] for r in serial])
                                 for k in ("create_to_planned_s", "create_to_applied_s", "total_s")}
            out["e2e_serial"]["statuses"] = [r["status"] for r in serial]
            h.log("serial", out["e2e_serial"])

            h.log("no-op runs (same workspace, nothing to change)")
            noop = [run_once(tok, org, ws[0], template) for _ in range(SERIAL_RUNS)]
            out["e2e_noop"] = {"total_s": h.stats([r["total_s"] for r in noop]),
                               "statuses": [r["status"] for r in noop]}
            h.log("noop", out["e2e_noop"])

            h.log("parallel runs")
            res = [None] * PARALLEL_RUNS
            def worker(i):
                res[i] = run_once(tok, org, ws[30 + i], template)
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
    h.save(TOOL, out)
    if "--keep" not in sys.argv:
        h.docker_rm(*CONTAINERS)


if __name__ == "__main__":
    main()
