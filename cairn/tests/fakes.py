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
