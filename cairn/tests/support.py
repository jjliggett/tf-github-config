"""Shared helpers for the end-to-end tests (stdlib only).

A `Server` is a real cairnd on a free port with a fresh data dir; tests talk
to it over HTTP exactly like clients do.
"""
import io
import json
import os
import shutil
import socket
import subprocess
import tarfile
import tempfile
import time
import unittest
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CAIRND = os.environ.get("CAIRND", os.path.join(ROOT, "target", "debug", "cairnd"))
CAIRN = os.environ.get("CAIRN", os.path.join(ROOT, "target", "debug", "cairn"))
TOFU = os.environ.get("TOFU") or shutil.which("tofu")
TOKEN = "e2e-site-token"
_opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

MAIN_TF = """
variable "n" {
  type    = number
  default = 3
}
variable "secret" {
  type      = string
  default   = "none"
  sensitive = true
}
resource "terraform_data" "r" {
  count = var.n
  input = "item-${count.index}"
}
output "count" { value = length(terraform_data.r) }
output "secret_len" {
  value     = length(var.secret)
  sensitive = true
}
"""


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def tarball(files):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tf:
        for name, text in files.items():
            data = text.encode()
            info = tarfile.TarInfo(name)
            info.size = len(data)
            tf.addfile(info, io.BytesIO(data))
    return buf.getvalue()


class Server:
    def __init__(self, args=(), env=None, port=None, https=False, ca=None):
        self.dir = tempfile.mkdtemp(prefix="cairn-e2e-")
        self.port = port or free_port()
        self.extra_args = list(args)
        self.extra_env = dict(env or {})
        self.base = f"{'https' if https else 'http'}://127.0.0.1:{self.port}"
        self.opener = _opener
        if https:
            import ssl
            ctx = ssl.create_default_context(cafile=ca)
            self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=ctx))
        self.proc = None
        self.start()

    def start(self):
        env = dict(os.environ, CAIRN_SITE_TOKEN=TOKEN, CAIRN_LOG="warn", **self.extra_env)
        self.log = open(os.path.join(self.dir, "server.log"), "ab")
        self.proc = subprocess.Popen(
            [CAIRND, "--data", os.path.join(self.dir, "data"), "--listen", f"127.0.0.1:{self.port}",
             "--tofu-bin", TOFU, *self.extra_args],
            stdout=self.log, stderr=self.log, env=env)
        for _ in range(200):
            try:
                if self.req("GET", "/healthz", token=None)[0] == 200:
                    return
            except OSError:
                pass
            time.sleep(0.02)
        raise RuntimeError("cairnd did not start")

    def stop(self):
        self.proc.terminate()
        self.proc.wait(10)
        self.log.close()

    def req(self, method, path, body=None, token=TOKEN, ctype=None, headers=None, timeout=60):
        h = dict(headers or {})
        if token:
            h["Authorization"] = f"Bearer {token}"
        data = None
        if isinstance(body, (bytes, bytearray)):
            data = bytes(body)
            h["Content-Type"] = ctype or "application/gzip"
        elif body is not None:
            data = json.dumps(body).encode()
            h["Content-Type"] = ctype or "application/json"
        r = urllib.request.Request(self.base + path, data=data, method=method, headers=h)
        try:
            with self.opener.open(r, timeout=timeout) as resp:
                raw = resp.read()
                status = resp.status
        except urllib.error.HTTPError as e:
            raw, status = e.read(), e.code
        try:
            return status, json.loads(raw) if raw else None
        except ValueError:
            return status, raw.decode(errors="replace")

    def ok(self, method, path, body=None, expect=(200, 201, 204), **kw):
        s, b = self.req(method, path, body, **kw)
        if s not in expect:
            raise AssertionError(f"{method} {path} -> {s}: {b}")
        return b

    def run(self, stack, kind="tracked", files=None, auto_apply=None, wait=True):
        q = f"?kind={kind}" + (f"&auto_apply={'true' if auto_apply else 'false'}" if auto_apply is not None else "")
        r = self.ok("POST", f"/api/v1/stacks/{stack}/runs{q}", tarball(files or {"main.tf": MAIN_TF}))
        return self.wait(r["id"]) if wait else r

    def wait(self, run_id, until=lambda r: r["status"] in (
            "applied", "planned_and_finished", "finished", "discarded", "errored", "canceled", "planned"), timeout=60):
        deadline = time.time() + timeout
        while time.time() < deadline:
            r = self.ok("GET", f"/api/v1/runs/{run_id}?wait=10")
            if until(r):
                return r
        raise AssertionError(f"run {run_id} stuck: {r}")


class Mcp:
    """Drive `cairn mcp serve` over stdio like an MCP client would."""

    def __init__(self, server, elicit_answer=None, read_only=False):
        env = dict(os.environ, CAIRN_HOST=server.base, CAIRN_TOKEN=TOKEN, NO_PROXY="*")
        args = [CAIRN, "mcp", "serve"] + (["--read-only"] if read_only else [])
        self.p = subprocess.Popen(args, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
        self.n = 0
        self.elicit_answer = elicit_answer  # None: client lacks elicitation; else True/False
        caps = {"elicitation": {}} if elicit_answer is not None else {}
        init = self.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": caps,
                                        "clientInfo": {"name": "e2e", "version": "1"}})
        self.protocol = init["protocolVersion"]
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")

    def call(self, method, params):
        self.n += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params}) + "\n")
        self.p.stdin.flush()
        while True:
            msg = json.loads(self.p.stdout.readline())
            if msg.get("method") == "elicitation/create":
                self.elicitations = getattr(self, "elicitations", []) + [msg["params"]["message"]]
                result = ({"action": "accept", "content": {"approve": True}} if self.elicit_answer
                          else {"action": "decline"})
                self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}) + "\n")
                self.p.stdin.flush()
                continue
            if msg.get("id") == self.n:
                if "error" in msg:
                    raise AssertionError(msg["error"])
                return msg["result"]

    def tool(self, name, **args):
        return self.call("tools/call", {"name": name, "arguments": args})

    def close(self):
        self.p.stdin.close()
        self.p.wait(10)
        self.p.stdout.close()




def needs(*what):
    """Skip unless the binaries/tools are available."""
    have = {"cairnd": os.path.exists(CAIRND), "cairn": os.path.exists(CAIRN), "tofu": bool(TOFU),
            "docker": shutil.which("docker") is not None and subprocess.run(
                ["docker", "info"], capture_output=True).returncode == 0}
    missing = [w for w in what if not have[w]]
    return unittest.skipIf(missing, f"needs {', '.join(missing)}")
