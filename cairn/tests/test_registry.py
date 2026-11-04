"""Module and provider registry: publish over the API, then a real `tofu init`
over TLS (module download, provider download with GPG signature check), and
a Cairn run that consumes a registry module with its run-scoped token."""
import base64
import hashlib
import io
import json
import os
import platform
import shutil
import subprocess
import tempfile
import unittest
import zipfile

from support import CAIRN, TOFU, TOKEN, Server, free_port, needs, tarball

MODULE_V1 = """
variable "n" {
  type    = number
  default = 2
}
resource "terraform_data" "item" {
  count = var.n
  input = "item-${count.index}"
}
output "ids" { value = terraform_data.item[*].input }
"""
ARCH = {"x86_64": "amd64", "amd64": "amd64", "aarch64": "arm64", "arm64": "arm64"}[platform.machine().lower()]


def gpg(home, *args, input=None):
    return subprocess.run(["gpg", "--homedir", home, "--batch", "--yes", *args], input=input, capture_output=True, check=True)


def provider_zip(kind, version):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        info = zipfile.ZipInfo(f"terraform-provider-{kind}_v{version}")
        info.external_attr = 0o755 << 16
        z.writestr(info, "#!/bin/sh\necho 'not a real provider'\nexit 1\n")
    return buf.getvalue()


@needs("cairnd", "tofu")
@unittest.skipIf(shutil.which("gpg") is None or shutil.which("openssl") is None, "needs gpg and openssl")
class Registry(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="cairn-registry-")
        # A CA and a server certificate signed by it (rustls rejects CA certs used as server certs).
        j = lambda n: os.path.join(cls.tmp, n)  # noqa: E731
        ec = ["-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes"]
        for cmd in (["openssl", "req", "-x509", *ec, "-keyout", j("ca.key"), "-out", j("ca.pem"), "-days", "1", "-subj", "/CN=Cairn Test CA",
                     "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign"],
                    ["openssl", "req", *ec, "-keyout", j("key.pem"), "-out", j("srv.csr"), "-subj", "/CN=localhost"],
                    ["openssl", "x509", "-req", "-in", j("srv.csr"), "-CA", j("ca.pem"), "-CAkey", j("ca.key"), "-CAcreateserial",
                     "-out", j("srv.pem"), "-days", "1", "-extfile", "/dev/stdin"]):
            subprocess.run(cmd, check=True, capture_output=True,
                           input=b"subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid,issuer\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\n" if "x509" in cmd and "-req" in cmd else None)
        with open(j("fullchain.pem"), "w") as f:
            f.write(open(j("srv.pem")).read() + open(j("ca.pem")).read())
        cls.cert, key = j("ca.pem"), j("key.pem")
        port = free_port()
        cls.host = f"127.0.0.1:{port}"
        cls.s = Server(args=["--tls-cert", j("fullchain.pem"), "--tls-key", key], port=port, https=True, ca=cls.cert,
                       env={"CAIRN_EXTERNAL_URL": f"https://{cls.host}", "SSL_CERT_FILE": cls.cert})
        cls.s.ok("POST", "/api/v1/spaces", {"name": "team"})
        cls.s.ok("POST", "/api/v1/spaces", {"name": "apps", "parent": "team"})
        cls.s.ok("POST", "/api/v1/spaces", {"name": "other"})
        # A signing key, like a provider author's goreleaser setup.
        cls.gnupg = os.path.join(cls.tmp, "gnupg")
        os.mkdir(cls.gnupg, 0o700)
        gpg(cls.gnupg, "--passphrase", "", "--quick-gen-key", "Cairn Test <test@example.com>", "rsa2048", "sign", "never")
        cls.armor = gpg(cls.gnupg, "--armor", "--export").stdout.decode()
        colons = gpg(cls.gnupg, "--with-colons", "--list-keys").stdout.decode()
        cls.key_id = next(l.split(":")[4] for l in colons.splitlines() if l.startswith("pub:"))

    @classmethod
    def tearDownClass(cls):
        cls.s.stop()
        shutil.rmtree(cls.s.dir, ignore_errors=True)
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def publish_provider(self, kind, version, sign=True):
        z = provider_zip(kind, version)
        fname = f"terraform-provider-{kind}_{version}_linux_{ARCH}.zip"
        sums = f"{hashlib.sha256(z).hexdigest()}  {fname}\n"
        signed = sums if sign else sums.replace(fname, fname + "x")
        sig = gpg(self.gnupg, "--detach-sign", "--output", "-", input=signed.encode()).stdout
        v = self.s.ok("POST", f"/api/v1/registry/providers/team/{kind}/{version}",
                      {"gpg_public_key": self.armor, "shasums": sums, "shasums_sig": base64.b64encode(sig).decode()})
        self.s.ok("PUT", f"/api/v1/registry/providers/team/{kind}/{version}/linux/{ARCH}", z, ctype="application/zip")
        return v

    def tofu_init(self, tf, token=TOKEN):
        d = tempfile.mkdtemp(dir=self.tmp)
        with open(os.path.join(d, "main.tf"), "w") as f:
            f.write(tf)
        rc = os.path.join(d, "tofurc")
        with open(rc, "w") as f:
            f.write(f'credentials "{self.host}" {{\n  token = "{token}"\n}}\n')
        env = dict(os.environ, TF_CLI_CONFIG_FILE=rc, SSL_CERT_FILE=self.cert, HOME=d, NO_PROXY="*", no_proxy="*",
                   TF_PLUGIN_CACHE_DIR="")
        r = subprocess.run([TOFU, "init", "-no-color", "-input=false"], cwd=d, env=env, capture_output=True, text=True)
        return r, d

    def test_module_and_provider_install_with_tofu(self):
        tb = tarball({"main.tf": MODULE_V1})
        for v in ("1.0.0", "1.1.0", "2.0.0-beta.1"):
            self.s.ok("POST", f"/api/v1/registry/modules/team/counter/null/{v}", tb)
        v = self.publish_provider("dummy", "1.0.0")
        self.assertEqual(v["key_id"], self.key_id, "key ID derived from the armored key")
        versions = self.s.ok("GET", "/registry/modules/v1/team/counter/null/versions")["modules"][0]["versions"]
        self.assertEqual([x["version"] for x in versions], ["2.0.0-beta.1", "1.1.0", "1.0.0"])

        r, d = self.tofu_init(f"""
terraform {{
  required_providers {{
    dummy = {{
      source  = "{self.host}/team/dummy"
      version = "1.0.0"
    }}
  }}
}}
module "counter" {{
  source  = "{self.host}/team/counter/null"
  version = "~> 1.0"
}}
""")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        mods = json.load(open(os.path.join(d, ".terraform", "modules", "modules.json")))["Modules"]
        self.assertEqual([m["Version"] for m in mods if m["Key"] == "counter"], ["1.1.0"])
        exe = os.path.join(d, ".terraform", "providers", self.host, "team", "dummy", "1.0.0", f"linux_{ARCH}",
                           "terraform-provider-dummy_v1.0.0")
        self.assertTrue(os.path.exists(exe), r.stdout)
        self.assertIn(self.key_id[-8:].upper(), r.stdout.upper(), "tofu reports the signing key")

    def test_bad_signature_is_rejected_by_tofu(self):
        self.publish_provider("tampered", "1.0.0", sign=False)
        r, _ = self.tofu_init(f"""
terraform {{
  required_providers {{
    tampered = {{ source = "{self.host}/team/tampered" }}
  }}
}}
""")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("signature", (r.stdout + r.stderr).lower())

    def test_run_uses_registry_module(self):
        self.s.ok("POST", "/api/v1/registry/modules/team/pair/null/0.1.0", tarball({"main.tf": MODULE_V1}))
        self.s.ok("POST", "/api/v1/stacks", {"name": "consumer", "space": "apps", "auto_apply": True})
        tf = f"""
module "pair" {{
  source  = "{self.host}/team/pair/null"
  version = "0.1.0"
}}
output "ids" {{ value = module.pair.ids }}
"""
        r = self.s.ok("POST", "/api/v1/stacks/consumer/runs?space=apps&kind=tracked", tarball({"main.tf": tf}))
        r = self.s.wait(r["id"])
        self.assertEqual(r["status"], "applied", self.s.req("GET", f"/api/v1/runs/{r['id']}/logs")[1])
        out = self.s.ok("GET", "/api/v1/stacks/consumer/outputs?space=apps")
        self.assertEqual(out["ids"]["value"], ["item-0", "item-1"])
        # A stack outside the namespace's subtree can't read it.
        self.s.ok("POST", "/api/v1/stacks", {"name": "outsider", "space": "other", "auto_apply": True})
        r = self.s.ok("POST", "/api/v1/stacks/outsider/runs?space=other&kind=tracked", tarball({"main.tf": tf}))
        self.assertEqual(self.s.wait(r["id"])["status"], "errored")

    def test_access_and_validation(self):
        self.s.ok("POST", "/api/v1/registry/modules/team/acl/null/1.0.0", tarball({"main.tf": MODULE_V1}))
        self.assertEqual(self.s.req("GET", "/registry/modules/v1/team/acl/null/versions", token=None)[0], 401)
        other = self.s.ok("POST", "/api/v1/tokens", {"kind": "service", "roles": [{"space": "other", "role": "admin"}]})["token"]
        self.assertEqual(self.s.req("GET", "/registry/modules/v1/team/acl/null/versions", token=other)[0], 404)
        self.assertEqual(self.s.req("POST", "/api/v1/registry/modules/team/acl/null/1.0.1", tarball({"main.tf": ""}), token=other)[0], 404)
        reader = self.s.ok("POST", "/api/v1/tokens", {"kind": "service", "roles": [{"space": "team", "role": "reader"}]})["token"]
        self.assertEqual(self.s.req("GET", "/registry/modules/v1/team/acl/null/versions", token=reader)[0], 200)
        self.assertEqual(self.s.req("POST", "/api/v1/registry/modules/team/acl/null/1.0.1", tarball({"main.tf": ""}), token=reader)[0], 403)
        # Duplicates, bad versions, bad archives.
        self.assertEqual(self.s.req("POST", "/api/v1/registry/modules/team/acl/null/1.0.0", tarball({"main.tf": ""}))[0], 409)
        self.assertEqual(self.s.req("POST", "/api/v1/registry/modules/team/acl/null/1.0", tarball({"main.tf": ""}))[0], 400)
        self.assertEqual(self.s.req("POST", "/api/v1/registry/modules/team/acl/null/1.0.2", b"not gzip")[0], 400)
        # Pre-signed download URLs can't be altered.
        st, _ = self.s.req("GET", "/registry/modules/v1/team/acl/null/1.0.0/download")
        self.assertEqual(st, 204)
        url = self._download_url("acl", "1.0.0")
        path = url.split(self.host, 1)[1]
        self.assertEqual(self.s.req("GET", path, token=None)[0], 200)
        self.assertEqual(self.s.req("GET", path.replace("/module/", "/provider/"), token=None)[0], 403)
        # Provider zips must match SHA256SUMS.
        self.s.ok("POST", "/api/v1/registry/providers/team/strict/1.0.0",
                  {"gpg_public_key": self.armor, "shasums": "00  terraform-provider-strict_1.0.0_linux_amd64.zip\n", "shasums_sig": "AAAA"})
        st, b = self.s.req("PUT", "/api/v1/registry/providers/team/strict/1.0.0/linux/amd64", b"zip", ctype="application/zip")
        self.assertEqual(st, 400, b)

    def _download_url(self, name, version):
        import urllib.request
        req = urllib.request.Request(f"{self.s.base}/registry/modules/v1/team/{name}/null/{version}/download",
                                     headers={"Authorization": f"Bearer {TOKEN}"})
        with self.s.opener.open(req) as r:
            return r.headers["X-Terraform-Get"]

    @needs("cairn")
    def test_cli_publish(self):
        d = tempfile.mkdtemp(dir=self.tmp)
        with open(os.path.join(d, "main.tf"), "w") as f:
            f.write(MODULE_V1)
        env = dict(os.environ, CAIRN_HOST=self.s.base, CAIRN_TOKEN=TOKEN, NO_PROXY="*", SSL_CERT_FILE=self.cert)

        def cli(*args):
            r = subprocess.run([CAIRN, "-o", "json", *args], env=env, capture_output=True, text=True)
            self.assertEqual(r.returncode, 0, r.stderr)
            return json.loads(r.stdout) if r.stdout.strip() else None

        v = cli("module", "publish", "team/fromcli/null", "1.2.3", "--dir", d)
        self.assertEqual(v["source"], f"{self.host}/team/fromcli/null")
        # Provider from a goreleaser-style dist directory.
        dist = tempfile.mkdtemp(dir=self.tmp)
        z = provider_zip("clidummy", "0.3.0")
        fname = f"terraform-provider-clidummy_0.3.0_linux_{ARCH}.zip"
        with open(os.path.join(dist, fname), "wb") as f:
            f.write(z)
        sums = os.path.join(dist, "terraform-provider-clidummy_0.3.0_SHA256SUMS")
        with open(sums, "w") as f:
            f.write(f"{hashlib.sha256(z).hexdigest()}  {fname}\n")
        gpg(self.gnupg, "--detach-sign", "--output", sums + ".sig", sums)
        keyfile = os.path.join(dist, "key.asc")
        with open(keyfile, "w") as f:
            f.write(self.armor)
        v = cli("provider", "publish", "team/clidummy", "0.3.0", "--dir", dist, "--gpg-key", keyfile)
        self.assertEqual(v["platforms"], [f"linux/{ARCH}"])
        names = {m["name"] for m in cli("module", "list")["items"]}
        self.assertIn("fromcli", names)
        self.assertIn("clidummy", {p["type"] for p in cli("provider", "list")["items"]})


if __name__ == "__main__":
    unittest.main()
