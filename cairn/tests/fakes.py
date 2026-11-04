"""In-process fake services for tests: S3 (SigV4) and Azure Blob (Shared Key).

Each fake re-implements its provider's signature check from the provider's
documentation, independently of the Rust code, so a signing bug fails the
test instead of passing silently.
"""
import base64
import datetime
import hashlib
import hmac
import http.server
import threading
import urllib.parse

from support import free_port


class _Server(http.server.ThreadingHTTPServer):
    daemon_threads = True


class FakeService:
    """Base: run a handler class on a free port in a background thread."""

    def __init__(self):
        self.objects = {}      # key -> bytes
        self.requests = []     # (method, path, status)
        self.port = free_port()
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def _body(self):
                n = int(self.headers.get("Content-Length") or 0)
                return self.rfile.read(n) if n else b""

            def handle_any(self):
                body = self._body()
                status, payload = outer.handle(self.command, self.path, self.headers, body)
                outer.requests.append((self.command, self.path, status))
                self.send_response(status)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                if self.command != "HEAD":
                    self.wfile.write(payload)

            do_GET = do_PUT = do_POST = do_DELETE = do_HEAD = do_PATCH = handle_any

        self.httpd = _Server(("127.0.0.1", self.port), Handler)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.port}"

    def stop(self):
        self.httpd.shutdown()

    def store(self, method, key, body):
        if method == "PUT":
            self.objects[key] = body
            return 200, b""
        if method in ("GET", "HEAD"):
            return (200, self.objects[key]) if key in self.objects else (404, b"")
        if method == "DELETE":
            self.objects.pop(key, None)
            return 204, b""
        return 405, b""


class FakeS3(FakeService):
    """Path-style S3 that enforces AWS Signature Version 4."""

    def __init__(self, access="AKIDTEST", secret="secret/key+test", region="us-east-1"):
        self.access, self.secret, self.region = access, secret, region
        self.auth_failures = 0
        super().__init__()

    def handle(self, method, raw_path, headers, body):
        path, _, query = raw_path.partition("?")
        if not self.verify(method, path, query, headers, body):
            self.auth_failures += 1
            return 403, b"<Error><Code>SignatureDoesNotMatch</Code></Error>"
        _, bucket, key = path.split("/", 2)
        return self.store(method, f"{bucket}/{urllib.parse.unquote(key)}", body)

    def verify(self, method, path, query, headers, body):
        auth = headers.get("Authorization", "")
        if not auth.startswith("AWS4-HMAC-SHA256 "):
            return False
        parts = dict(p.strip().split("=", 1) for p in auth[len("AWS4-HMAC-SHA256 "):].split(","))
        access, date, region, service, _ = parts["Credential"].split("/")
        if access != self.access or region != self.region or service != "s3":
            return False
        payload_hash = headers.get("x-amz-content-sha256", "")
        if payload_hash != hashlib.sha256(body).hexdigest():
            return False
        signed = parts["SignedHeaders"].split(";")
        canon_headers = "".join(f"{h}:{headers.get(h, '').strip()}\n" for h in signed)
        canonical = "\n".join([method, path, query, canon_headers, ";".join(signed), payload_hash])
        amz_date = headers.get("x-amz-date", "")
        scope = f"{date}/{region}/s3/aws4_request"
        to_sign = "\n".join(["AWS4-HMAC-SHA256", amz_date, scope, hashlib.sha256(canonical.encode()).hexdigest()])
        k = hmac.new(("AWS4" + self.secret).encode(), date.encode(), hashlib.sha256).digest()
        for part in (region, "s3", "aws4_request"):
            k = hmac.new(k, part.encode(), hashlib.sha256).digest()
        return hmac.compare_digest(hmac.new(k, to_sign.encode(), hashlib.sha256).hexdigest(), parts["Signature"])


class FakeAzureBlob(FakeService):
    """Azurite-style (account in the path) Blob service enforcing Shared Key."""

    def __init__(self, account="devstoreaccount1", key=base64.b64encode(b"k" * 32).decode()):
        self.account, self.key = account, key
        self.auth_failures = 0
        super().__init__()

    def handle(self, method, raw_path, headers, body):
        path, _, _ = raw_path.partition("?")
        if not self.verify(method, path, headers, body):
            self.auth_failures += 1
            return 403, b"<Error><Code>AuthenticationFailed</Code></Error>"
        if method == "PUT" and headers.get("x-ms-blob-type") != "BlockBlob":
            return 400, b"missing x-ms-blob-type"
        _, account, rest = path.split("/", 2)
        status, payload = self.store(method, urllib.parse.unquote(rest), body)
        return (201 if method == "PUT" and status == 200 else status), payload

    def verify(self, method, path, headers, body):
        auth = headers.get("Authorization", "")
        prefix = f"SharedKey {self.account}:"
        if not auth.startswith(prefix):
            return False
        date = headers.get("x-ms-date", "")
        try:
            sent = datetime.datetime.strptime(date, "%a, %d %b %Y %H:%M:%S GMT")
        except ValueError:
            return False
        if abs((datetime.datetime.now(datetime.UTC).replace(tzinfo=None) - sent).total_seconds()) > 900:
            return False
        ms = sorted((k.lower(), v.strip()) for k, v in headers.items() if k.lower().startswith("x-ms-"))
        canon_headers = "".join(f"{k}:{v}\n" for k, v in ms)
        length = headers.get("Content-Length", "")
        length = "" if length in ("", "0") else length
        fields = [method, headers.get("Content-Encoding", ""), headers.get("Content-Language", ""), length,
                  headers.get("Content-MD5", ""), headers.get("Content-Type", ""), headers.get("Date", ""),
                  headers.get("If-Modified-Since", ""), headers.get("If-Match", ""), headers.get("If-None-Match", ""),
                  headers.get("If-Unmodified-Since", ""), headers.get("Range", "")]
        to_sign = "\n".join(fields) + "\n" + canon_headers + f"/{self.account}{path}"
        want = base64.b64encode(hmac.new(base64.b64decode(self.key), to_sign.encode(), hashlib.sha256).digest()).decode()
        return hmac.compare_digest(want, auth[len(prefix):])


class FakeForge(FakeService):
    """One provider's REST API (github, gitea, forgejo, gitlab, bitbucket,
    azure_devops): serves source archives in that provider's layout and
    records commit statuses and pull request comments."""

    AUTH = {
        "github": lambda t: ("Authorization", f"Bearer {t}"),
        "gitea": lambda t: ("Authorization", f"token {t}"),
        "forgejo": lambda t: ("Authorization", f"token {t}"),
        "gitlab": lambda t: ("Private-Token", t),
        "bitbucket": lambda t: ("Authorization", f"Bearer {t}"),
        "azure_devops": lambda t: ("Authorization", "Basic " + base64.b64encode(f":{t}".encode()).decode()),
    }

    def __init__(self, provider, token="forge-token"):
        self.provider, self.token = provider, token
        self.repos = {}        # repo -> {rev: {path: text}}
        self.statuses = []     # (repo, sha, body)
        self.comments = {}     # id -> {"repo", "pr", "body", "edits"}
        self.unauthorized = 0
        super().__init__()

    @property
    def api_url(self):
        return self.url + ("/api/v1" if self.provider in ("gitea", "forgejo") else "")

    def commit(self, repo, rev, files):
        self.repos.setdefault(repo, {})[rev] = files

    def _archive(self, repo, rev, prefix):
        import io
        import tarfile
        import zipfile
        files = self.repos.get(repo, {}).get(rev)
        if files is None:
            return 404, b"no such revision"
        buf = io.BytesIO()
        if self.provider == "azure_devops":
            with zipfile.ZipFile(buf, "w") as z:
                for name, text in files.items():
                    z.writestr(name, text)
            return 200, buf.getvalue()
        with tarfile.open(fileobj=buf, mode="w:gz") as tf:
            d = tarfile.TarInfo(prefix)
            d.type = tarfile.DIRTYPE
            tf.addfile(d)
            for name, text in files.items():
                data = text.encode()
                info = tarfile.TarInfo(f"{prefix}/{name}")
                info.size = len(data)
                tf.addfile(info, io.BytesIO(data))
        return 200, buf.getvalue()

    def _comment(self, method, repo, pr, cid, payload):
        import json
        content = payload.get("content")
        text = payload.get("body") or (content.get("raw") if isinstance(content, dict) else content) \
            or (payload.get("comments") or [{}])[0].get("content")
        if method == "POST":
            cid = str(len(self.comments) + 100)
            self.comments[cid] = {"repo": repo, "pr": int(pr), "body": text, "edits": 0}
        else:
            if cid not in self.comments:
                return 404, b"{}"
            self.comments[cid]["body"] = text
            self.comments[cid]["edits"] += 1
        return 200, json.dumps({"id": int(cid)}).encode()

    def handle(self, method, path, headers, body):
        import json
        import re
        name, value = self.AUTH[self.provider](self.token)
        if headers.get(name) != value:
            self.unauthorized += 1
            return 401, b'{"message": "bad credentials"}'
        u = urllib.parse.urlparse(path)
        p, q = urllib.parse.unquote(u.path), urllib.parse.parse_qs(u.query)
        payload = json.loads(body) if body else {}
        prov = self.provider
        if prov == "github":
            if m := re.fullmatch(r"/repos/([^/]+/[^/]+)/tarball/(.+)", p):
                return self._archive(m[1], m[2], f"{m[1].replace('/', '-')}-{m[2][:7]}")
            if m := re.fullmatch(r"/repos/([^/]+/[^/]+)/statuses/(\w+)", p):
                self.statuses.append((m[1], m[2], payload))
                return 201, b"{}"
        if prov in ("gitea", "forgejo"):
            if m := re.fullmatch(r"/api/v1/repos/([^/]+/[^/]+)/archive/(.+)\.tar\.gz", p):
                return self._archive(m[1], m[2], m[1].split("/")[1])
            if m := re.fullmatch(r"/api/v1/repos/([^/]+/[^/]+)/statuses/(\w+)", p):
                self.statuses.append((m[1], m[2], payload))
                return 201, b"{}"
            p = p[len("/api/v1"):]
        if prov in ("github", "gitea", "forgejo"):
            if m := re.fullmatch(r"/repos/([^/]+/[^/]+)/issues/(\d+)/comments", p):
                return self._comment(method, m[1], m[2], None, payload)
            if m := re.fullmatch(r"/repos/([^/]+/[^/]+)/issues/comments/(\d+)", p):
                return self._comment(method, m[1], 0, m[2], payload)
        if prov == "gitlab":
            if m := re.fullmatch(r"/projects/(.+)/repository/archive\.tar\.gz", p):
                return self._archive(m[1], q["sha"][0], f"{m[1].split('/')[-1]}-{q['sha'][0]}")
            if m := re.fullmatch(r"/projects/(.+)/statuses/(\w+)", p):
                self.statuses.append((m[1], m[2], payload))
                return 201, b"{}"
            if m := re.fullmatch(r"/projects/(.+)/merge_requests/(\d+)/notes(?:/(\d+))?", p):
                return self._comment(method, m[1], m[2], m[3], payload)
        if prov == "bitbucket":
            if m := re.fullmatch(r"/([^/]+/[^/]+)/get/(.+)\.tar\.gz", p):
                return self._archive(m[1], m[2], f"{m[1].replace('/', '-')}-{m[2][:12]}")
            if m := re.fullmatch(r"/repositories/([^/]+/[^/]+)/commit/(\w+)/statuses/build", p):
                self.statuses.append((m[1], m[2], payload))
                return 201, b"{}"
            if m := re.fullmatch(r"/repositories/([^/]+/[^/]+)/pullrequests/(\d+)/comments(?:/(\d+))?", p):
                return self._comment(method, m[1], m[2], m[3], payload)
        if prov == "azure_devops":
            if m := re.fullmatch(r"/([^/]+)/_apis/git/repositories/([^/]+)/items", p):
                assert q.get("$format") == ["zip"], q
                return self._archive(f"{m[1]}/{m[2]}", q["versionDescriptor.version"][0], "")
            if m := re.fullmatch(r"/([^/]+)/_apis/git/repositories/([^/]+)/commits/(\w+)/statuses", p):
                self.statuses.append((f"{m[1]}/{m[2]}", m[3], payload))
                return 201, b"{}"
            if m := re.fullmatch(r"/([^/]+)/_apis/git/repositories/([^/]+)/pullRequests/(\d+)/threads(?:/(\d+)/comments/1)?", p):
                return self._comment(method, f"{m[1]}/{m[2]}", m[3], m[4], payload)
        return 404, f"fake {prov}: no route for {method} {p}".encode()
