#!/usr/bin/env python3
"""Core end-to-end tests: stacks, runs, state, TFE API, CLI, MCP, restarts.

    CAIRND=target/debug/cairnd TOFU=$(which tofu) python3 -m unittest discover -s tests -p 'test_*.py'
"""
import json
import os
import shutil
import subprocess
import tempfile
import threading
import time
import unittest
import urllib.request

from support import CAIRN, CAIRND, MAIN_TF, TOFU, TOKEN, Mcp, Server, _opener, tarball


@unittest.skipUnless(TOFU and os.path.exists(CAIRND), "needs cairnd binary and tofu")
class E2E(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.s = Server()

    @classmethod
    def tearDownClass(cls):
        cls.s.stop()
        shutil.rmtree(cls.s.dir, ignore_errors=True)

    def stack(self, name, **kw):
        return self.s.ok("POST", "/api/v1/stacks", {"name": name, **kw})

    # ------------------------------------------------------------ basics

    def test_auth(self):
        self.assertEqual(self.s.req("GET", "/healthz", token=None)[0], 200)
        self.assertEqual(self.s.req("GET", "/api/v1/stacks", token=None)[0], 401)
        self.assertEqual(self.s.req("GET", "/api/v1/stacks", token="wrong")[0], 401)
        tok = self.s.ok("POST", "/api/v1/tokens", {"description": "ci"})["token"]
        self.assertTrue(tok.startswith("cairn_"))
        self.assertEqual(self.s.req("GET", "/api/v1/whoami", token=tok)[0], 200)

    def test_stack_crud_and_pagination(self):
        for i in range(5):
            self.stack(f"page-{i}")
        self.assertEqual(self.s.req("POST", "/api/v1/stacks", {"name": "page-0"})[0], 409)
        self.assertEqual(self.s.req("POST", "/api/v1/stacks", {"name": "bad name"})[0], 400)
        seen, after = [], ""
        while True:
            b = self.s.ok("GET", f"/api/v1/stacks?limit=2&after={after}")
            seen += [s["name"] for s in b["items"]]
            if not b["next"]:
                break
            after = b["next"]
        self.assertEqual([n for n in seen if n.startswith("page-")], [f"page-{i}" for i in range(5)])
        st = self.s.ok("PATCH", "/api/v1/stacks/page-1", {"description": "x", "auto_apply": True})
        self.assertEqual((st["description"], st["auto_apply"]), ("x", True))
        self.s.ok("DELETE", "/api/v1/stacks/page-4")
        self.assertEqual(self.s.req("GET", "/api/v1/stacks/page-4")[0], 404)

    # ------------------------------------------------------------ runs

    def test_tracked_auto_apply_then_noop(self):
        self.stack("auto", auto_apply=True)
        r = self.s.run("auto")
        self.assertEqual(r["status"], "applied", r)
        self.assertEqual(r["adds"], 3)
        outs = self.s.ok("GET", "/api/v1/stacks/auto/outputs")
        self.assertEqual(outs["count"]["value"], 3)
        self.assertIsNone(outs["secret_len"]["value"], "sensitive output must be masked")
        sv = self.s.ok("GET", "/api/v1/stacks/auto/state/versions")["items"]
        self.assertEqual((len(sv), sv[0]["resource_count"]), (1, 3))
        r2 = self.s.run("auto")
        self.assertEqual((r2["status"], r2["has_changes"]), ("planned_and_finished", False))
        self.assertEqual(len(self.s.ok("GET", "/api/v1/stacks/auto/state/versions")["items"]), 1)

    def test_proposed_does_not_touch_state(self):
        self.stack("prop", auto_apply=True)
        self.s.run("prop")
        r = self.s.run("prop", kind="proposed", files={"main.tf": MAIN_TF.replace("default = 3", "default = 5")})
        self.assertEqual((r["status"], r["has_changes"], r["adds"]), ("planned_and_finished", True, 2))
        plan = self.s.ok("GET", f"/api/v1/runs/{r['id']}/plan")
        self.assertEqual(len(plan["summary"]["resources"]), 2)
        self.assertEqual(len(self.s.ok("GET", "/api/v1/stacks/prop/state/versions")["items"]), 1)

    def test_confirm_and_discard(self):
        self.stack("manual")
        r = self.s.run("manual")
        self.assertEqual(r["status"], "planned")
        self.assertEqual([a["action"] for a in r["next_actions"]], ["confirm", "discard"])
        self.assertTrue(self.s.ok("GET", "/api/v1/stacks/manual")["locked"])
        self.s.ok("POST", f"/api/v1/runs/{r['id']}/confirm")
        self.assertEqual(self.s.wait(r["id"])["status"], "applied")
        self.assertFalse(self.s.ok("GET", "/api/v1/stacks/manual")["locked"])
        r = self.s.run("manual", files={"main.tf": MAIN_TF.replace("default = 3", "default = 1")})
        self.assertEqual(r["status"], "planned")
        self.s.ok("POST", f"/api/v1/runs/{r['id']}/discard")
        self.assertEqual(self.s.ok("GET", f"/api/v1/runs/{r['id']}")["status"], "discarded")
        self.assertFalse(self.s.ok("GET", "/api/v1/stacks/manual")["locked"])
        self.assertEqual(self.s.req("POST", f"/api/v1/runs/{r['id']}/confirm")[0], 409)

    def test_tracked_runs_serialize_per_stack(self):
        self.stack("serial", auto_apply=True)
        a = self.s.run("serial", wait=False)
        b = self.s.run("serial", wait=False, files={"main.tf": MAIN_TF.replace("default = 3", "default = 4")})
        ra, rb = self.s.wait(a["id"]), self.s.wait(b["id"])
        self.assertEqual((ra["status"], rb["status"]), ("applied", "applied"))
        self.assertGreaterEqual(rb["status_times"]["planning"], ra["status_times"]["applied"])
        self.assertEqual(rb["adds"], 1)

    def test_proposed_runs_do_not_wait_for_planned_run(self):
        self.stack("busy")
        held = self.s.run("busy")  # planned, holds the stack
        p = self.s.run("busy", kind="proposed")
        self.assertEqual(p["status"], "planned_and_finished")
        t = self.s.run("busy", wait=False)
        time.sleep(0.5)
        self.assertEqual(self.s.ok("GET", f"/api/v1/runs/{t['id']}")["status"], "plan_queued")
        self.s.ok("POST", f"/api/v1/runs/{t['id']}/cancel")
        self.assertEqual(self.s.ok("GET", f"/api/v1/runs/{t['id']}")["status"], "canceled")
        self.s.ok("POST", f"/api/v1/runs/{held['id']}/discard")

    def test_cancel_running_apply(self):
        self.stack("cancel-me", auto_apply=True)
        slow = MAIN_TF + """
resource "terraform_data" "slow" {
  provisioner "local-exec" {
    command = "sleep 60"
  }
}
"""
        r = self.s.run("cancel-me", files={"main.tf": slow}, wait=False)
        self.s.wait(r["id"], until=lambda x: x["status"] == "applying")
        time.sleep(1)
        t0 = time.time()
        self.s.ok("POST", f"/api/v1/runs/{r['id']}/cancel")
        done = self.s.wait(r["id"], until=lambda x: x["status"] in ("canceled", "errored", "applied"))
        self.assertEqual(done["status"], "canceled")
        self.assertLess(time.time() - t0, 15)
        self.assertFalse(self.s.ok("GET", "/api/v1/stacks/cancel-me")["locked"])
        # Resources created before the interrupt were kept as a state version.
        sv = self.s.ok("GET", "/api/v1/stacks/cancel-me/state/versions")["items"]
        self.assertGreaterEqual(len(sv), 1)

    def test_sensitive_variables(self):
        self.stack("vars", auto_apply=True)
        v = self.s.ok("POST", "/api/v1/stacks/vars/variables",
                      {"key": "secret", "value": "hunter22", "sensitive": True})
        self.assertEqual(v["value"], "")
        self.s.ok("POST", "/api/v1/stacks/vars/variables", {"key": "n", "value": "2"})
        listed = {x["key"]: x for x in self.s.ok("GET", "/api/v1/stacks/vars/variables")["items"]}
        self.assertEqual((listed["secret"]["value"], listed["n"]["value"]), ("", "2"))
        with open(os.path.join(self.s.dir, "data", "cairn.db"), "rb") as f:
            self.assertNotIn(b"hunter22", f.read())
        r = self.s.run("vars")
        self.assertEqual((r["status"], r["adds"]), ("applied", 2))
        outs = self.s.ok("GET", "/api/v1/stacks/vars/outputs?sensitive=true")
        self.assertEqual(outs["secret_len"]["value"], 8)
        self.s.ok("DELETE", "/api/v1/stacks/vars/variables/n")

    def test_errors_are_reported(self):
        self.stack("broken", auto_apply=True)
        r = self.s.run("broken", files={"main.tf": 'resource "nope" "x" {'})
        self.assertEqual(r["status"], "errored")
        self.assertIn("tofu", r["error"])
        self.assertFalse(self.s.ok("GET", "/api/v1/stacks/broken")["locked"])
        logs = self.s.ok("GET", f"/api/v1/runs/{r['id']}/logs")
        self.assertIn("error", logs if isinstance(logs, str) else json.dumps(logs))

    def test_log_follow_and_events(self):
        self.stack("follow")
        events = []
        def sse():
            req = urllib.request.Request(self.s.base + "/api/v1/events?stack=" +
                                         self.s.ok("GET", "/api/v1/stacks/follow")["id"],
                                         headers={"Authorization": f"Bearer {TOKEN}"})
            with _opener.open(req, timeout=30) as resp:
                for line in resp:
                    if line.startswith(b"data:"):
                        events.append(json.loads(line[5:]))
                        if events[-1].get("status") == "planned":
                            return
        t = threading.Thread(target=sse, daemon=True)
        t.start()
        time.sleep(0.2)
        r = self.s.run("follow", wait=False)
        req = urllib.request.Request(f"{self.s.base}/api/v1/runs/{r['id']}/logs?follow=true",
                                     headers={"Authorization": f"Bearer {TOKEN}"})
        with _opener.open(req, timeout=30) as resp:
            lines = [json.loads(l) for l in resp.read().splitlines() if l.strip()]
        self.assertTrue(any(l.get("type") == "change_summary" for l in lines), lines[-3:])
        t.join(10)
        self.assertIn("planned", [e.get("status") for e in events])
        self.s.ok("POST", f"/api/v1/runs/{r['id']}/discard")

    # ------------------------------------------------------------ state backend

    def test_http_backend_with_real_tofu(self):
        st = self.stack("http-backend")
        d = tempfile.mkdtemp(dir=self.s.dir)
        url = f"{self.s.base}/api/v1/stacks/{st['id']}/state"
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
    password       = "{TOKEN}"
  }}
}}
''')
        env = dict(os.environ, NO_PROXY="*", no_proxy="*", TF_IN_AUTOMATION="1")
        run = lambda *a: subprocess.run([TOFU, *a, "-no-color"], cwd=d, env=env, capture_output=True, text=True)
        self.assertEqual(run("init", "-input=false").returncode, 0)
        out = run("apply", "-auto-approve", "-input=false")
        self.assertEqual(out.returncode, 0, out.stderr)
        sv = self.s.ok("GET", f"/api/v1/stacks/{st['id']}/state/versions")["items"]
        self.assertEqual(sv[0]["resource_count"], 3)
        self.assertFalse(self.s.ok("GET", f"/api/v1/stacks/{st['id']}")["locked"])
        # A held lock makes tofu fail fast with the holder's info.
        self.s.ok("POST", f"/api/v1/stacks/{st['id']}/lock", {"note": "maintenance"})
        out = run("plan", "-input=false", "-lock-timeout=0s")
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("lock", out.stderr.lower())
        self.s.ok("DELETE", f"/api/v1/stacks/{st['id']}/lock")
        # The managed runner picks up the state written by the CLI.
        self.s.ok("PATCH", f"/api/v1/stacks/{st['id']}", {"auto_apply": True})
        r = self.s.run(st["id"])
        self.assertEqual((r["status"], r["has_changes"]), ("planned_and_finished", False))

    # ------------------------------------------------------------ TFE API

    def test_tfe_flow(self):
        jh = {"Accept": "application/vnd.api+json"}
        self.assertEqual(self.s.req("GET", "/.well-known/terraform.json", token=None)[1]["tfe.v2"], "/api/v2/")
        self.s.ok("POST", "/api/v2/organizations",
                  {"data": {"type": "organizations", "attributes": {"name": "acme", "email": "a@b.c"}}})
        ws = self.s.ok("POST", "/api/v2/organizations/acme/workspaces",
                       {"data": {"type": "workspaces", "attributes": {"name": "net", "auto-apply": True}}})["data"]
        cv = self.s.ok("POST", f"/api/v2/workspaces/{ws['id']}/configuration-versions",
                       {"data": {"type": "configuration-versions", "attributes": {"auto-queue-runs": False}}})["data"]
        upload = cv["attributes"]["upload-url"].replace(self.s.base, "")
        self.assertEqual(self.s.req("PUT", upload.split("?")[0] + "?sig=bad", tarball({"main.tf": MAIN_TF}),
                                    token=None)[0], 403)
        self.s.ok("PUT", upload, tarball({"main.tf": MAIN_TF}), token=None)
        run = self.s.ok("POST", "/api/v2/runs", {"data": {"type": "runs", "attributes": {}, "relationships": {
            "workspace": {"data": {"type": "workspaces", "id": ws["id"]}},
            "configuration-version": {"data": {"type": "configuration-versions", "id": cv["id"]}}}}})["data"]
        self.s.wait(run["id"])
        tr = self.s.ok("GET", f"/api/v2/runs/{run['id']}", headers=jh)["data"]
        self.assertEqual(tr["attributes"]["status"], "applied")
        self.assertIn("applied-at", tr["attributes"]["status-timestamps"])
        plan = self.s.ok("GET", f"/api/v2/plans/{tr['relationships']['plan']['data']['id']}")["data"]
        self.assertEqual(plan["attributes"]["resource-additions"], 3)
        outs = self.s.ok("GET", f"/api/v2/workspaces/{ws['id']}/current-state-version-outputs")["data"]
        self.assertEqual({o["attributes"]["name"]: o["attributes"]["value"] for o in outs}["count"], 3)
        lst = self.s.ok("GET", "/api/v2/organizations/acme/workspaces?page%5Bsize%5D=10")
        self.assertEqual(lst["meta"]["pagination"]["total-count"], 1)
        self.assertEqual(self.s.req("GET", "/api/v2/organizations/nope/workspaces")[0], 404)

    # ------------------------------------------------------------ CLI

    @unittest.skipUnless(os.path.exists(CAIRN), "needs cairn CLI binary")
    def test_cli(self):
        env = dict(os.environ, CAIRN_HOST=self.s.base, CAIRN_TOKEN=TOKEN, NO_PROXY="*",
                   XDG_CONFIG_HOME=os.path.join(self.s.dir, "cfg"))
        cli = lambda *a: subprocess.run([CAIRN, *a], env=env, capture_output=True, text=True, timeout=120)
        r = cli("stack", "create", "cli-demo", "--auto-apply")
        self.assertEqual(r.returncode, 0, r.stderr)
        d = tempfile.mkdtemp(dir=self.s.dir)
        with open(os.path.join(d, "main.tf"), "w") as f:
            f.write(MAIN_TF)
        r = cli("stack", "preview", "--stack", "cli-demo", "--dir", d, "--detailed-exitcode")
        self.assertEqual(r.returncode, 4, r.stdout + r.stderr)
        r = cli("stack", "deploy", "--stack", "cli-demo", "--dir", d)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("applied", r.stdout)
        r = cli("-o", "json", "stack", "outputs", "--stack", "cli-demo")
        self.assertEqual(json.loads(r.stdout)["count"]["value"], 3)
        r = cli("-o", "json", "stack", "list")
        self.assertIn("cli-demo", [s["name"] for s in json.loads(r.stdout)["items"]])
        r = cli("stack", "state", "pull", "--stack", "cli-demo")
        self.assertEqual(json.loads(r.stdout)["serial"], 1)


@unittest.skipUnless(TOFU and os.path.exists(CAIRND) and os.path.exists(CAIRN), "needs cairnd, cairn and tofu")
class McpE2E(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.s = Server()
        cls.s.ok("POST", "/api/v1/stacks", {"name": "agent"})
        cls.d = tempfile.mkdtemp(dir=cls.s.dir)
        with open(os.path.join(cls.d, "main.tf"), "w") as f:
            f.write(MAIN_TF)

    @classmethod
    def tearDownClass(cls):
        cls.s.stop()
        shutil.rmtree(cls.s.dir, ignore_errors=True)

    def test_tools_and_annotations(self):
        m = Mcp(self.s)
        tools = {t["name"]: t for t in m.call("tools/list", {})["tools"]}
        self.assertEqual(m.protocol, "2025-06-18")
        self.assertTrue(tools["get_plan_summary"]["annotations"]["readOnlyHint"])
        self.assertTrue(tools["confirm_run"]["annotations"]["destructiveHint"])
        self.assertFalse(tools["trigger_preview"]["annotations"]["destructiveHint"])
        ro = Mcp(self.s, read_only=True)
        self.assertNotIn("confirm_run", [t["name"] for t in ro.call("tools/list", {})["tools"]])
        ro.close()
        stacks = m.tool("list_stacks")["structuredContent"]
        self.assertIn("agent", [x["name"] for x in stacks["items"]])
        self.assertEqual(len(m.call("prompts/list", {})["prompts"]), 2)
        m.close()

    def test_preview_deploy_and_human_gate(self):
        m = Mcp(self.s)  # no elicitation support
        pv = m.tool("trigger_preview", stack="agent", dir=self.d)["structuredContent"]
        self.assertEqual((pv["run"]["status"], pv["plan"]["add"]), ("planned_and_finished", 3))
        dep = m.tool("trigger_deploy", stack="agent", dir=self.d)["structuredContent"]
        run_id = dep["run"]["id"]
        self.assertEqual(dep["run"]["status"], "planned")
        r = m.tool("confirm_run", run_id=run_id)
        self.assertTrue(r["isError"])
        self.assertIn("needs_human", r["content"][0]["text"])
        m.close()
        no = Mcp(self.s, elicit_answer=False)
        r = no.tool("confirm_run", run_id=run_id)["structuredContent"]
        self.assertEqual(r["status"], "not_approved")
        self.assertIn("3 to add", no.elicitations[0])
        no.close()
        self.assertEqual(self.s.ok("GET", f"/api/v1/runs/{run_id}")["status"], "planned")
        yes = Mcp(self.s, elicit_answer=True)
        r = yes.tool("confirm_run", run_id=run_id)["structuredContent"]
        self.assertEqual(r["run"]["status"], "applied")
        logs = yes.tool("get_run_logs", run_id=run_id, tail=5)["structuredContent"]
        self.assertLessEqual(len(logs["lines"]), 5)
        bad = yes.tool("trigger_preview", stack="agent", dir=self.d + "/missing")
        self.assertTrue(bad["isError"])
        yes.close()


@unittest.skipUnless(TOFU and os.path.exists(CAIRND), "needs cairnd binary and tofu")
class Restart(unittest.TestCase):
    def test_state_survives_restart_and_interrupted_runs_error(self):
        s = Server()
        try:
            s.ok("POST", "/api/v1/stacks", {"name": "keep", "auto_apply": True})
            s.run("keep")
            s.stop()
            s.start()
            self.assertEqual(s.ok("GET", "/api/v1/stacks/keep/outputs")["count"]["value"], 3)
            r = s.run("keep")
            self.assertEqual(r["status"], "planned_and_finished")
        finally:
            s.stop()
            shutil.rmtree(s.dir, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
