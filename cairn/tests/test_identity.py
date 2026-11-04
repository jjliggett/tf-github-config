"""Identity and access: spaces, roles, tokens, audit, and OIDC against a real Dex."""
import json
import os
import shutil
import subprocess
import tempfile
import time
import unittest
import urllib.error
import urllib.parse
import urllib.request

from support import CAIRN, MAIN_TF, TOKEN, Server, free_port, needs, tarball

DEX_IMAGE = os.environ.get("DEX_IMAGE", "dexidp/dex:v2.43.1")


@needs("cairnd", "tofu")
class Rbac(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        s = cls.s = Server()
        s.ok("POST", "/api/v1/spaces", {"name": "team"})
        s.ok("POST", "/api/v1/spaces", {"name": "prod", "parent": "team"})
        s.ok("POST", "/api/v1/spaces", {"name": "other"})
        s.ok("POST", "/api/v1/stacks", {"name": "net", "space": "team"})
        s.ok("POST", "/api/v1/stacks", {"name": "db", "space": "prod", "auto_apply": True})
        s.ok("POST", "/api/v1/stacks", {"name": "secret", "space": "other"})

    @classmethod
    def tearDownClass(cls):
        cls.s.stop()
        shutil.rmtree(cls.s.dir, ignore_errors=True)

    def token(self, roles, **kw):
        return self.s.ok("POST", "/api/v1/tokens", {"description": "test", "kind": "service", "roles": roles, **kw})["token"]

    def test_inherited_roles_and_visibility(self):
        planner = self.token([{"space": "team", "role": "planner"}])
        names = {x["name"] for x in self.s.ok("GET", "/api/v1/stacks", token=planner)["items"]}
        self.assertEqual(names, {"net", "db"}, "team + inherited prod, not other")
        self.assertEqual(self.s.req("GET", "/api/v1/stacks/secret?space=other", token=planner)[0], 404)
        spaces = {x["name"]: x["your_role"] for x in self.s.ok("GET", "/api/v1/spaces", token=planner)["items"]}
        self.assertEqual(spaces, {"team": "planner", "prod": "planner"})
        # Planner: proposed yes, tracked / confirm / variables no.
        st, _ = self.s.req("POST", "/api/v1/stacks/db/runs?space=prod&kind=proposed", tarball({"main.tf": MAIN_TF}), token=planner)
        self.assertEqual(st, 201)
        st, _ = self.s.req("POST", "/api/v1/stacks/db/runs?space=prod&kind=tracked", tarball({"main.tf": MAIN_TF}), token=planner)
        self.assertEqual(st, 403)
        st, _ = self.s.req("POST", "/api/v1/stacks/db/variables?space=prod", {"key": "n", "value": "1"}, token=planner)
        self.assertEqual(st, 403)
        self.assertEqual(self.s.req("GET", "/api/v1/stacks/db/state?space=prod", token=planner)[0], 403)

    def test_writer_on_child_space_only(self):
        writer = self.token([{"space": "prod", "role": "writer"}])
        st, run = self.s.req("POST", "/api/v1/stacks/db/runs?space=prod&kind=tracked", tarball({"main.tf": MAIN_TF}), token=writer)
        self.assertEqual(st, 201, run)
        self.assertEqual(self.s.wait(run["id"])["status"], "applied")
        self.assertEqual(self.s.req("POST", "/api/v1/stacks/net/runs?space=team&kind=proposed",
                                    tarball({"main.tf": MAIN_TF}), token=writer)[0], 404)
        self.assertTrue(run["triggered_by"].startswith("token:"))

    def test_no_privilege_escalation(self):
        admin_prod = self.token([{"space": "prod", "role": "admin"}])
        # Can grant on prod…
        self.assertEqual(self.s.req("POST", "/api/v1/spaces/prod/roles",
                                    {"principal_kind": "team", "principal": "devs", "role": "writer"}, token=admin_prod)[0], 201)
        # …but not on the parent, and can't mint site admins or tokens for other spaces.
        self.assertIn(self.s.req("POST", "/api/v1/spaces/team/roles",
                                 {"principal_kind": "team", "principal": "devs", "role": "admin"}, token=admin_prod)[0], (403, 404))
        self.assertEqual(self.s.req("POST", "/api/v1/tokens", {"kind": "service", "site_admin": True,
                                                               "roles": [{"space": "prod", "role": "reader"}]}, token=admin_prod)[0], 403)
        self.assertIn(self.s.req("POST", "/api/v1/tokens", {"kind": "service",
                                                            "roles": [{"space": "team", "role": "reader"}]}, token=admin_prod)[0], (403, 404))
        self.assertEqual(self.s.req("POST", "/api/v1/spaces", {"name": "rogue"}, token=admin_prod)[0], 403)
        self.assertEqual(self.s.req("GET", "/api/v1/audit", token=admin_prod)[0], 403)

    def test_expiry_revocation_and_audit(self):
        r = self.s.ok("POST", "/api/v1/tokens", {"kind": "service", "roles": [{"space": "team", "role": "reader"}],
                                                 "expires_in_hours": 0})
        time.sleep(0.01)
        self.assertEqual(self.s.req("GET", "/api/v1/whoami", token=r["token"])[0], 401)
        r = self.s.ok("POST", "/api/v1/tokens", {"kind": "service", "roles": [{"space": "team", "role": "reader"}]})
        self.assertEqual(self.s.req("GET", "/api/v1/whoami", token=r["token"])[0], 200)
        self.s.ok("DELETE", f"/api/v1/tokens/{r['id']}")
        self.assertEqual(self.s.req("GET", "/api/v1/whoami", token=r["token"])[0], 401)
        audit = self.s.ok("GET", "/api/v1/audit?limit=200")["items"]
        self.assertTrue(any(a["method"] == "DELETE" and a["path"] == f"/api/v1/tokens/{r['id']}" for a in audit))
        self.assertTrue(all(a["principal"] for a in audit))

    def test_tfe_is_scoped_too(self):
        reader = self.token([{"space": "team", "role": "reader"}])
        self.assertEqual(self.s.req("GET", "/api/v2/organizations/other/workspaces", token=reader)[0], 404)
        ws = self.s.ok("GET", "/api/v2/organizations/team/workspaces", token=reader)["data"]
        self.assertEqual([w["attributes"]["name"] for w in ws], ["net"])
        self.assertEqual(self.s.req("POST", "/api/v2/organizations/team/workspaces",
                                    {"data": {"attributes": {"name": "x"}}}, token=reader)[0], 403)


def http(method, url, data=None, headers=None):
    """No-redirect HTTP helper returning (status, headers, body)."""
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *a, **k):
            return None
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect)
    req = urllib.request.Request(url, data=data, method=method, headers=headers or {})
    try:
        with opener.open(req, timeout=20) as r:
            return r.status, {k.lower(): v for k, v in r.headers.items()}, r.read()
    except urllib.error.HTTPError as e:
        return e.code, {k.lower(): v for k, v in e.headers.items()}, e.read()


@needs("cairnd", "tofu", "docker")
class Oidc(unittest.TestCase):
    """Real OIDC flows against Dex (mock connector: kilgore@kilgore.trout in group "authors")."""

    @classmethod
    def setUpClass(cls):
        cls.dex_port = free_port()
        cls.cairn_port = free_port()
        cls.issuer = f"http://127.0.0.1:{cls.dex_port}/dex"
        cls.tmp = tempfile.mkdtemp(prefix="cairn-dex-")
        cfg = os.path.join(cls.tmp, "dex.yaml")
        with open(cfg, "w") as f:
            f.write(f"""issuer: {cls.issuer}
storage: {{type: memory}}
web: {{http: 127.0.0.1:{cls.dex_port}}}
oauth2:
  skipApprovalScreen: true
  passwordConnector: local
enablePasswordDB: true
connectors:
  - {{type: mockCallback, id: mock, name: Mock}}
staticClients:
  - id: cairn
    secret: cairn-secret
    name: Cairn
    redirectURIs: ["http://127.0.0.1:{cls.cairn_port}/auth/callback"]
staticPasswords:
  - email: admin@example.com
    hash: "$2a$10$2b2cU8CPhOTaGrs1HRQuAueS7JTT5ZHsHSzYiFPm1leZck7Mc8T4W"
    username: admin
    userID: 08a8684b-db88-4b73-90a9-3cd1661f5466
""")
        cls.dex = f"cairn-test-dex-{cls.dex_port}"
        subprocess.run(["docker", "run", "-d", "--rm", "--name", cls.dex, "--network", "host",
                        "-v", f"{cfg}:/etc/dex/config.yaml:ro", DEX_IMAGE, "dex", "serve", "/etc/dex/config.yaml"],
                       check=True, capture_output=True)
        for _ in range(200):
            try:
                if http("GET", f"{cls.issuer}/.well-known/openid-configuration")[0] == 200:
                    break
            except OSError:
                pass
            time.sleep(0.05)
        cls.s = Server(port=cls.cairn_port, env={
            "CAIRN_OIDC_ISSUER": cls.issuer, "CAIRN_OIDC_CLIENT_ID": "cairn",
            "CAIRN_OIDC_CLIENT_SECRET": "cairn-secret", "CAIRN_EXTERNAL_URL": f"http://127.0.0.1:{cls.cairn_port}"})
        cls.s.ok("POST", "/api/v1/spaces", {"name": "books"})
        cls.s.ok("POST", "/api/v1/stacks", {"name": "library", "space": "books"})

    @classmethod
    def tearDownClass(cls):
        cls.s.stop()
        subprocess.run(["docker", "rm", "-f", cls.dex], capture_output=True)
        shutil.rmtree(cls.s.dir, ignore_errors=True)
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def id_token(self):
        body = urllib.parse.urlencode({"grant_type": "password", "username": "admin@example.com", "password": "password",
                                       "scope": "openid email profile", "client_id": "cairn",
                                       "client_secret": "cairn-secret"}).encode()
        st, _, b = http("POST", f"{self.issuer}/token", body, {"Content-Type": "application/x-www-form-urlencoded"})
        self.assertEqual(st, 200, b)
        return json.loads(b)["id_token"]

    def test_bearer_id_token(self):
        tok = self.id_token()
        who = self.s.ok("GET", "/api/v1/whoami", token=tok)
        self.assertEqual((who["principal"]["kind"], who["principal"]["name"]), ("user", "admin@example.com"))
        self.assertEqual(self.s.req("GET", "/api/v1/stacks/library?space=books", token=tok)[0], 404)
        self.s.ok("POST", "/api/v1/spaces/books/roles", {"principal_kind": "user", "principal": "admin@example.com", "role": "reader"})
        self.assertEqual(self.s.req("GET", "/api/v1/stacks/library?space=books", token=tok)[0], 200)
        # Tampered or foreign tokens are rejected.
        bad = tok[:-4] + ("AAAA" if not tok.endswith("AAAA") else "BBBB")
        self.assertEqual(self.s.req("GET", "/api/v1/whoami", token=bad)[0], 401)

    def test_browser_login_with_cli_loopback_and_team_roles(self):
        base = f"http://127.0.0.1:{self.cairn_port}"
        loopback = "http://127.0.0.1:65530/callback"
        st, h, _ = http("GET", f"{base}/auth/login?redirect={urllib.parse.quote(loopback)}")
        self.assertEqual(st, 303)
        url = h["location"] + "&connector_id=mock"
        for _ in range(10):  # follow Dex's redirect chain until it lands on our loopback
            st, h, b = http("GET", url)
            self.assertIn(st, (301, 302, 303), b[:300])
            url = urllib.parse.urljoin(url, h["location"])
            if url.startswith(loopback):
                break
        q = urllib.parse.parse_qs(urllib.parse.urlparse(url).query)
        token = q["token"][0]
        self.assertTrue(token.startswith("cairn_"))
        who = self.s.ok("GET", "/api/v1/whoami", token=token)
        self.assertEqual(who["principal"]["groups"], ["authors"])
        self.assertEqual(self.s.req("GET", "/api/v1/stacks/library?space=books", token=token)[0], 404)
        self.s.ok("POST", "/api/v1/spaces/books/roles", {"principal_kind": "team", "principal": "authors", "role": "planner"})
        self.assertEqual(self.s.req("GET", "/api/v1/stacks/library?space=books", token=token)[0], 200)
        # Non-loopback redirects are refused (no open redirect).
        self.assertEqual(http("GET", f"{base}/auth/login?redirect=https://evil.example/")[0], 400)
        users = self.s.ok("GET", "/api/v1/users")["items"]
        self.assertIn("kilgore@kilgore.trout", [u["email"] for u in users])

    def test_login_policy(self):
        self.s.ok("POST", "/api/v1/spaces", {"name": "vault"})
        self.s.ok("POST", "/api/v1/stacks", {"name": "safe", "space": "vault"})
        tok = self.id_token()
        self.assertEqual(self.s.req("GET", "/api/v1/stacks/safe?space=vault", token=tok)[0], 404)
        pol = self.s.ok("POST", "/api/v1/policies", {"name": "vault-writers", "kind": "login", "space": "vault", "body": """package cairn
import rego.v1
space_role := {"vault": "writer"} if endswith(input.session.email, "@example.com")
"""})
        self.assertEqual(self.s.req("GET", "/api/v1/stacks/safe?space=vault", token=tok)[0], 200)
        who = self.s.ok("GET", "/api/v1/whoami", token=tok)
        self.assertEqual(self.s.req("POST", "/api/v1/stacks/safe/variables?space=vault", {"key": "n", "value": "1"}, token=tok)[0],
                         201, who)
        self.s.ok("PATCH", f"/api/v1/policies/{pol['id']}", {"body": "package cairn\nimport rego.v1\ndeny if input.session.email == \"admin@example.com\"\n"})
        self.assertEqual(self.s.req("GET", "/api/v1/whoami", token=tok)[0], 401)
        self.s.ok("DELETE", f"/api/v1/policies/{pol['id']}")
        self.assertEqual(self.s.req("GET", "/api/v1/stacks/safe?space=vault", token=tok)[0], 404, "policy roles are gone")

    @needs("cairn")
    def test_cli_browser_login(self):
        """`cairn login` without --token: the CLI listens on loopback, we play the browser."""
        base = f"http://127.0.0.1:{self.cairn_port}"
        cfg = tempfile.mkdtemp(dir=self.s.dir)
        env = dict(os.environ, XDG_CONFIG_HOME=cfg, NO_PROXY="*")
        p = subprocess.Popen([CAIRN, "login", "--host", base, "--no-browser"], env=env,
                             stderr=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        url = None
        for line in p.stderr:
            if line.strip().startswith("http"):
                url = line.strip()
                break
        st, h, _ = http("GET", url)
        url = h["location"] + "&connector_id=mock"
        for _ in range(10):
            st, h, _ = http("GET", url)
            url = urllib.parse.urljoin(url, h["location"])
            if url.startswith("http://127.0.0.1:") and "/callback?token=" in url and str(self.cairn_port) not in url:
                break
        st, _, body = http("GET", url)  # the "browser" lands on the CLI's loopback listener
        self.assertIn(b"Logged in", body)
        self.assertEqual(p.wait(20), 0)
        saved = json.load(open(os.path.join(cfg, "cairn", "config.json")))
        self.assertEqual(saved["host"], base)
        r = subprocess.run([CAIRN, "-o", "json", "whoami"], env=env, capture_output=True, text=True)
        self.assertEqual(json.loads(r.stdout)["principal"]["name"], "kilgore@kilgore.trout")


if __name__ == "__main__":
    unittest.main()
