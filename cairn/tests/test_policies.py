"""Policy as code: plan, approval, notification policies, attachment and dry runs."""
import hashlib
import hmac
import http.server
import json
import os
import subprocess
import tempfile
import shutil
import threading
import time
import unittest

from support import CAIRN, MAIN_TF, TOKEN, Server, needs, tarball

DENY_BIG = """package cairn
import rego.v1
deny contains sprintf("too many resources: %d", [n]) if {
  n := count([rc | some rc in input.plan.resource_changes; "create" in rc.change.actions])
  n > 2
}
warn contains sprintf("creating %s", [rc.address]) if {
  some rc in input.plan.resource_changes
  "create" in rc.change.actions
}
"""

ONE_APPROVAL = """package cairn
import rego.v1
approve if count([r | some r in input.reviews; r.decision == "approve"]) >= 1
reject if { some r in input.reviews; r.decision == "reject" }
"""

SMALL_TF = MAIN_TF.replace("default = 3", "default = 1")


class Receiver:
    """A local webhook endpoint recording deliveries."""

    def __init__(self):
        self.got = []
        outer = self

        class H(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                outer.got.append((dict(self.headers), body))
                self.send_response(204)
                self.end_headers()

            def log_message(self, *a):
                pass

        self.srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.srv.server_address[1]}/hook"
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def wait(self, n, timeout=20):
        deadline = time.time() + timeout
        while len(self.got) < n and time.time() < deadline:
            time.sleep(0.05)
        return self.got

    def close(self):
        self.srv.shutdown()


@needs("cairnd", "tofu")
class Policies(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        s = cls.s = Server(["--allow-private-webhooks"])
        s.ok("POST", "/api/v1/spaces", {"name": "pol"})
        s.ok("POST", "/api/v1/stacks", {"name": "guarded", "space": "pol", "labels": ["guarded"], "auto_apply": True})
        s.ok("POST", "/api/v1/stacks", {"name": "gated", "space": "pol", "labels": ["gated"], "auto_apply": True})
        s.ok("POST", "/api/v1/stacks", {"name": "manual", "space": "pol", "labels": ["gated"]})
        s.ok("POST", "/api/v1/stacks", {"name": "plain", "space": "pol", "auto_apply": True})
        s.ok("POST", "/api/v1/policies", {"name": "no-big", "kind": "plan", "space": "pol", "body": DENY_BIG,
                                          "labels": ["autoattach:guarded"]})
        s.ok("POST", "/api/v1/policies", {"name": "one-approval", "kind": "approval", "space": "pol",
                                          "body": ONE_APPROVAL, "labels": ["autoattach:gated"]})

    @classmethod
    def tearDownClass(cls):
        cls.s.stop()
        shutil.rmtree(cls.s.dir, ignore_errors=True)

    def run_on(self, stack, files, kind="tracked"):
        r = self.s.ok("POST", f"/api/v1/stacks/{stack}/runs?space=pol&kind={kind}", tarball(files))
        return self.s.wait(r["id"])

    def logs(self, run_id):
        return self.s.req("GET", f"/api/v1/runs/{run_id}/logs")[1]

    def test_plan_policy_denies_and_warns(self):
        r = self.run_on("guarded", {"main.tf": MAIN_TF})
        self.assertEqual(r["status"], "errored")
        self.assertIn("too many resources: 3", r["error"])
        ev = self.s.ok("GET", f"/api/v1/runs/{r['id']}/policies")["evaluations"]
        self.assertEqual([e["policy"] for e in ev], ["no-big"])
        self.assertEqual(len(ev[0]["result"]["warn"]), 3)
        # The stack wasn't touched and stays unlocked.
        self.assertEqual(self.s.req("GET", "/api/v1/stacks/guarded/state?space=pol")[0], 204)
        # Proposed runs are checked too.
        pr = self.run_on("guarded", {"main.tf": MAIN_TF}, kind="proposed")
        self.assertEqual(pr["status"], "errored")
        ok = self.run_on("guarded", {"main.tf": SMALL_TF})
        self.assertEqual(ok["status"], "applied", ok)
        self.assertIn("creating terraform_data.r[0]", self.logs(ok["id"]))

    def test_approval_policy_gates_auto_apply(self):
        r = self.run_on("gated", {"main.tf": MAIN_TF})
        self.assertEqual((r["status"], r["approval"]), ("planned", "pending"))
        self.assertEqual(r["next_actions"][0]["action"], "review")
        self.assertEqual(self.s.req("POST", f"/api/v1/runs/{r['id']}/confirm")[0], 409)
        self.s.ok("POST", f"/api/v1/runs/{r['id']}/reviews", {"decision": "approve", "note": "lgtm"})
        r = self.s.wait(r["id"], until=lambda x: x["status"] in ("applied", "errored"))
        self.assertEqual((r["status"], r["approval"]), ("applied", "approved"))
        info = self.s.ok("GET", f"/api/v1/runs/{r['id']}/policies")
        self.assertEqual(info["reviews"][0]["decision"], "approve")
        self.assertEqual(len(info["evaluations"]), 2, "evaluated after plan and after the review")

    def test_rejection_discards_and_unlocks(self):
        r = self.run_on("gated", {"main.tf": SMALL_TF})
        self.assertEqual(r["approval"], "pending")
        r = self.s.ok("POST", f"/api/v1/runs/{r['id']}/reviews", {"decision": "reject"})
        self.assertEqual((r["status"], r["approval"]), ("discarded", "rejected"))
        self.assertIsNone(self.s.ok("GET", "/api/v1/stacks/gated?space=pol").get("locked_by"))

    def test_approved_manual_run_still_needs_confirm(self):
        r = self.run_on("manual", {"main.tf": SMALL_TF})
        r = self.s.ok("POST", f"/api/v1/runs/{r['id']}/reviews", {"decision": "approve"})
        self.assertEqual((r["status"], r["approval"]), ("planned", "approved"))
        self.s.ok("POST", f"/api/v1/runs/{r['id']}/confirm")
        self.assertEqual(self.s.wait(r["id"], until=lambda x: x["status"] in ("applied", "errored"))["status"], "applied")

    def test_attach_detach_and_listing(self):
        body = "package cairn\nimport rego.v1\nwarn contains \"hi\" if true\n"
        p = self.s.ok("POST", "/api/v1/policies", {"name": "hello", "kind": "plan", "space": "pol", "body": body})
        self.assertEqual(self.s.ok("GET", "/api/v1/stacks/plain/policies?space=pol")["items"], [])
        self.s.ok("POST", f"/api/v1/policies/{p['id']}/attach/plain?space=pol")
        names = [x["name"] for x in self.s.ok("GET", "/api/v1/stacks/plain/policies?space=pol")["items"]]
        self.assertEqual(names, ["hello"])
        r = self.run_on("plain", {"main.tf": SMALL_TF})
        self.assertEqual(r["status"], "applied")
        self.assertIn("policy hello: hi", self.logs(r["id"]))
        self.s.ok("DELETE", f"/api/v1/policies/{p['id']}/attach/plain?space=pol")
        self.assertEqual(self.s.ok("GET", "/api/v1/stacks/plain/policies?space=pol")["items"], [])
        kinds = {x["kind"] for x in self.s.ok("GET", "/api/v1/policies")["items"]}
        self.assertTrue({"plan", "approval"} <= kinds)
        self.assertEqual(len(self.s.ok("GET", "/api/v1/policies?kind=approval")["items"]), 1)

    def test_evaluate_and_validation(self):
        r = self.s.ok("POST", "/api/v1/policies/evaluate", {"kind": "approval", "body": ONE_APPROVAL,
                                                            "input": {"reviews": [{"decision": "approve"}]}})
        self.assertEqual(r["result"], {"approve": True})
        st, b = self.s.req("POST", "/api/v1/policies", {"name": "bad", "kind": "plan", "space": "pol", "body": "package other\n"})
        self.assertEqual(st, 400)
        self.assertIn("package cairn", b["detail"] if isinstance(b, dict) and "detail" in b else str(b))
        self.assertEqual(self.s.req("POST", "/api/v1/policies", {"name": "x", "kind": "nope", "space": "pol", "body": "package cairn\n"})[0], 400)
        pid = [x for x in self.s.ok("GET", "/api/v1/policies")["items"] if x["name"] == "no-big"][0]["id"]
        r = self.s.ok("POST", f"/api/v1/policies/{pid}/evaluate", {"input": {"plan": {"resource_changes": []}}})
        self.assertEqual(r["result"], {"deny": [], "warn": []})

    def test_rbac(self):
        reader = self.s.ok("POST", "/api/v1/tokens", {"kind": "service", "roles": [{"space": "pol", "role": "reader"}]})["token"]
        admin = self.s.ok("POST", "/api/v1/tokens", {"kind": "service", "roles": [{"space": "pol", "role": "admin"}]})["token"]
        body = "package cairn\n"
        self.assertEqual(self.s.req("POST", "/api/v1/policies", {"name": "r", "kind": "plan", "space": "pol", "body": body},
                                    token=reader)[0], 403)
        self.assertEqual(self.s.req("POST", "/api/v1/policies", {"name": "a", "kind": "plan", "space": "pol", "body": body},
                                    token=admin)[0], 201)
        self.assertEqual(self.s.req("POST", "/api/v1/policies", {"name": "l", "kind": "login", "space": "pol", "body": body},
                                    token=admin)[0], 403, "login policies need a site admin")
        self.assertIn(self.s.req("POST", "/api/v1/spaces/pol/webhooks", {"url": "https://example.com/x"}, token=reader)[0],
                      (403, 404))

    @needs("cairn")
    def test_cli(self):
        env = dict(os.environ, CAIRN_HOST=self.s.base, CAIRN_TOKEN=TOKEN, NO_PROXY="*", CAIRN_SPACE="pol")

        def cli(*args, stdin=None):
            r = subprocess.run([CAIRN, "-o", "json", *args], env=env, capture_output=True, text=True, input=stdin)
            self.assertEqual(r.returncode, 0, r.stderr)
            return json.loads(r.stdout) if r.stdout.strip() else None

        with tempfile.NamedTemporaryFile("w", suffix=".rego", delete=False) as f:
            f.write(ONE_APPROVAL)
        try:
            self.assertEqual(cli("policy", "eval", "--kind", "approval", "--file", f.name, "--input", "-",
                                 stdin='{"reviews": [{"decision": "approve"}]}'), {"approve": True})
            p = cli("policy", "create", "cli-approval", "--kind", "approval", "--file", f.name)
        finally:
            os.unlink(f.name)
        self.s.ok("POST", "/api/v1/stacks", {"name": "via-cli", "space": "pol", "auto_apply": True})
        cli("policy", "attach", p["id"], "via-cli")
        r = self.run_on("via-cli", {"main.tf": SMALL_TF})
        self.assertEqual(r["approval"], "pending")
        r = cli("run", "review", r["id"], "--approve", "--note", "from cli")
        r = self.s.wait(r["id"], until=lambda x: x["status"] in ("applied", "errored"))
        self.assertEqual(r["status"], "applied")
        self.assertEqual(cli("run", "policies", r["id"])["reviews"][0]["note"], "from cli")
        self.assertIn("cli-approval", [x["name"] for x in cli("policy", "list", "--kind", "approval")["items"]])

    def test_notifications(self):
        rx = Receiver()
        try:
            self.s.ok("POST", "/api/v1/spaces", {"name": "notify"})
            self.s.ok("POST", "/api/v1/stacks", {"name": "n1", "space": "notify", "auto_apply": True})
            hook = self.s.ok("POST", "/api/v1/spaces/notify/webhooks", {"url": rx.url, "secret": "s3cret"})
            r = self.s.ok("POST", "/api/v1/stacks/n1/runs?space=notify&kind=tracked", tarball({"main.tf": SMALL_TF}))
            self.s.wait(r["id"])
            (headers, body), = rx.wait(1)
            want = "sha256=" + hmac.new(b"s3cret", body, hashlib.sha256).hexdigest()
            self.assertEqual(headers["X-Cairn-Signature"], want)
            doc = json.loads(body)
            self.assertEqual((doc["event"], doc["stack"]["name"], doc["run"]["id"]), ("run.applied", "n1", r["id"]))
            # A notification policy reshapes and filters: only errored runs, custom payload.
            self.s.ok("POST", "/api/v1/policies", {"name": "only-errors", "kind": "notification", "space": "notify",
                                                   "labels": ["autoattach:*"], "body": f"""package cairn
import rego.v1
webhook contains {{"endpoint_id": "{hook['id']}", "payload": {{"text": sprintf("%s failed", [input.stack.name])}}}} if input.event == "run.errored"
"""})
            ok = self.s.ok("POST", "/api/v1/stacks/n1/runs?space=notify&kind=tracked", tarball({"main.tf": SMALL_TF.replace('"item-', '"v2-')}))
            self.assertEqual(self.s.wait(ok["id"])["status"], "applied")
            bad = self.s.ok("POST", "/api/v1/stacks/n1/runs?space=notify&kind=tracked", tarball({"main.tf": "resource {"}))
            self.assertEqual(self.s.wait(bad["id"])["status"], "errored")
            got = rx.wait(2)
            self.assertEqual(len(got), 2, [b for _, b in got])
            self.assertEqual(json.loads(got[1][1]), {"text": "n1 failed"})
        finally:
            rx.close()


@needs("cairnd", "tofu")
class WebhookSsrf(unittest.TestCase):
    def test_private_targets_refused_by_default(self):
        s = Server()
        try:
            s.ok("POST", "/api/v1/spaces", {"name": "x"})
            st, b = s.req("POST", "/api/v1/spaces/x/webhooks", {"url": "http://127.0.0.1:9/x"})
            self.assertEqual(st, 400, b)
            self.assertEqual(s.req("POST", "/api/v1/spaces/x/webhooks", {"url": "file:///etc/passwd"})[0], 400)
        finally:
            s.stop()
            shutil.rmtree(s.dir, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
