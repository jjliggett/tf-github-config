"""Blob storage backends, encryption at rest, and TLS."""
import json
import os
import shutil
import subprocess
import tempfile
import unittest

from fakes import FakeAzureBlob, FakeS3
from support import MAIN_TF, TOFU, TOKEN, Server, needs


def run_and_check(t, s, stack):
    s.ok("POST", "/api/v1/stacks", {"name": stack, "auto_apply": True})
    r = s.run(stack)
    t.assertEqual(r["status"], "applied", r)
    state = s.ok("GET", f"/api/v1/stacks/{stack}/state")
    t.assertEqual(len(state["resources"][0]["instances"]), 3)
    return r


@needs("cairnd", "tofu")
class S3Backend(unittest.TestCase):
    def setUp(self):
        self.fake = FakeS3()
        self.env = {"AWS_ACCESS_KEY_ID": self.fake.access, "AWS_SECRET_ACCESS_KEY": self.fake.secret,
                    "CAIRN_BLOB_STORE": f"s3://cairn/pre/fix?endpoint={self.fake.url}&region=us-east-1"}

    def tearDown(self):
        self.fake.stop()

    def test_runs_store_encrypted_blobs_in_s3(self):
        s = Server(env=self.env)
        try:
            run_and_check(self, s, "s3")
            keys = sorted(self.fake.objects)
            self.assertTrue(any(k.startswith("cairn/pre/fix/config/") for k in keys), keys)
            state_keys = [k for k in keys if k.startswith("cairn/pre/fix/state/")]
            self.assertEqual(len(state_keys), 1)
            blob = self.fake.objects[state_keys[0]]
            self.assertTrue(blob.startswith(b"CAIRNENC1"), "encrypted at rest")
            self.assertNotIn(b"terraform_data", blob)
            self.assertEqual(self.fake.auth_failures, 0)
            # State survives a restart (it lives in S3, metadata in SQLite).
            s.stop()
            s.start()
            self.assertEqual(s.ok("GET", "/api/v1/stacks/s3/outputs")["count"]["value"], 3)
            # Deleting the stack removes its blobs.
            s.ok("DELETE", "/api/v1/stacks/s3")
            self.assertEqual([k for k in self.fake.objects if k.startswith("cairn/pre/fix/")], [])
        finally:
            s.stop()
            shutil.rmtree(s.dir, ignore_errors=True)

    def test_wrong_credentials_fail_the_run(self):
        env = dict(self.env, AWS_SECRET_ACCESS_KEY="wrong")
        s = Server(env=env)
        try:
            s.ok("POST", "/api/v1/stacks", {"name": "bad", "auto_apply": True})
            st, body = s.req("POST", "/api/v1/stacks/bad/runs?kind=tracked", b"\x1f\x8b" + b"x" * 20)
            self.assertEqual(st, 500)
            self.assertIn("403", json.dumps(body))
            self.assertGreater(self.fake.auth_failures, 0)
        finally:
            s.stop()
            shutil.rmtree(s.dir, ignore_errors=True)


@needs("cairnd", "tofu")
class AzureBackend(unittest.TestCase):
    def test_runs_store_blobs_in_azure(self):
        fake = FakeAzureBlob()
        s = Server(env={"AZURE_STORAGE_KEY": fake.key, "CAIRN_NO_BLOB_ENCRYPTION": "true",
                        "CAIRN_BLOB_STORE": f"azblob://{fake.account}/cairn/blobs?endpoint={fake.url}/{fake.account}"})
        try:
            run_and_check(self, s, "az")
            state_keys = [k for k in fake.objects if k.startswith("cairn/blobs/state/")]
            self.assertEqual(len(state_keys), 1)
            self.assertIn(b"terraform_data", fake.objects[state_keys[0]], "plaintext with --no-blob-encryption")
            self.assertEqual(fake.auth_failures, 0)
        finally:
            s.stop()
            fake.stop()
            shutil.rmtree(s.dir, ignore_errors=True)


@needs("cairnd", "tofu")
class Tls(unittest.TestCase):
    def test_https_with_real_tofu_http_backend(self):
        tmp = tempfile.mkdtemp(prefix="cairn-tls-")
        cert, key = os.path.join(tmp, "cert.pem"), os.path.join(tmp, "key.pem")
        subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                        "-nodes", "-keyout", key, "-out", cert, "-days", "1", "-subj", "/CN=localhost",
                        "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"], check=True, capture_output=True)
        s = Server(args=["--tls-cert", cert, "--tls-key", key], env={}, https=True, ca=cert)
        try:
            st = s.ok("POST", "/api/v1/stacks", {"name": "tls"})
            d = tempfile.mkdtemp(dir=tmp)
            url = f"{s.base}/api/v1/stacks/{st['id']}/state"
            with open(os.path.join(d, "main.tf"), "w") as f:
                f.write(MAIN_TF + f'''
terraform {{
  backend "http" {{
    address        = "{url}"
    lock_address   = "{url}/lock"
    unlock_address = "{url}/lock"
    lock_method    = "POST"
    unlock_method  = "DELETE"
    username       = "cairn"
  }}
}}
''')
            env = dict(os.environ, NO_PROXY="*", no_proxy="*", SSL_CERT_FILE=cert, TF_HTTP_PASSWORD=TOKEN)
            for args in (["init", "-input=false"], ["apply", "-auto-approve", "-input=false"]):
                out = subprocess.run([TOFU, *args, "-no-color"], cwd=d, env=env, capture_output=True, text=True)
                self.assertEqual(out.returncode, 0, out.stderr)
            self.assertEqual(s.ok("GET", f"/api/v1/stacks/{st['id']}/state/versions")["items"][0]["resource_count"], 3)
            plain = subprocess.run(["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}",
                                    f"http://127.0.0.1:{s.port}/healthz"], capture_output=True, text=True)
            self.assertNotEqual(plain.stdout, "200", "plain HTTP must not be served on the TLS port")
        finally:
            s.stop()
            shutil.rmtree(s.dir, ignore_errors=True)
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
