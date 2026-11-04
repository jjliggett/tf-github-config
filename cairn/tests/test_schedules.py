"""Destroy runs, task runs (and task policies), drift detection with
reconciliation, and schedules (one-shot and cron)."""
import datetime
import json
import os
import shutil
import subprocess
import time
import unittest

from support import CAIRN, MAIN_TF, TOKEN, Server, needs, tarball


def in_seconds(n):
    return (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=n)).isoformat()


@needs("cairnd", "tofu")
class Schedules(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.s = Server()
        cls.s.ok("POST", "/api/v1/spaces", {"name": "ops"})

    @classmethod
    def tearDownClass(cls):
        cls.s.stop()
        shutil.rmtree(cls.s.dir, ignore_errors=True)

    def deployed(self, name, **kw):
        self.s.ok("POST", "/api/v1/stacks", {"name": name, "space": "ops", "auto_apply": True, **kw})
        r = self.s.ok("POST", f"/api/v1/stacks/{name}/runs?space=ops&kind=tracked", tarball({"main.tf": MAIN_TF}))
        self.assertEqual(self.s.wait(r["id"])["status"], "applied")

    def start(self, name, body, expect=(201,)):
        return self.s.ok("POST", f"/api/v1/stacks/{name}/runs?space=ops", body, expect=expect)

    def resources(self, name):
        st = self.s.ok("GET", f"/api/v1/stacks/{name}?space=ops")
        sv = self.s.ok("GET", f"/api/v1/stacks/{name}/state/versions?space=ops")["items"]
        return sv[0]["resource_count"] if sv and st["current_state_id"] else 0

    def test_destroy_run(self):
        self.deployed("doomed")
        r = self.s.wait(self.start("doomed", {"kind": "destroy", "auto_apply": True})["id"])
        self.assertEqual((r["kind"], r["status"], r["destroys"]), ("destroy", "applied", 3))
        self.assertEqual(self.resources("doomed"), 0)
        # Destroy needs writer, and an uploaded body isn't accepted (it destroys what's deployed).
        planner = self.s.ok("POST", "/api/v1/tokens", {"kind": "service", "roles": [{"space": "ops", "role": "planner"}]})["token"]
        self.assertEqual(self.s.req("POST", "/api/v1/stacks/doomed/runs?space=ops", {"kind": "destroy"}, token=planner)[0], 403)
        self.assertEqual(self.s.req("POST", "/api/v1/stacks/doomed/runs?space=ops&kind=destroy", tarball({"main.tf": MAIN_TF}))[0], 400)

    def test_tasks_and_task_policy(self):
        self.deployed("tasked")
        r = self.s.wait(self.start("tasked", {"kind": "task", "command": "tofu state list"})["id"])
        self.assertEqual(r["status"], "finished")
        logs = self.s.req("GET", f"/api/v1/runs/{r['id']}/logs")[1]
        self.assertIn("terraform_data.r[2]", logs)
        r = self.s.wait(self.start("tasked", {"kind": "task", "command": ["state", "rm", "terraform_data.r[0]"]})["id"])
        self.assertEqual(r["status"], "finished")
        self.assertEqual(self.resources("tasked"), 2)
        bad = self.s.wait(self.start("tasked", {"kind": "task", "command": "state show nope.nope"})["id"])
        self.assertEqual(bad["status"], "errored")
        self.assertEqual(self.s.req("POST", "/api/v1/stacks/tasked/runs?space=ops", {"kind": "task", "command": "console"})[0], 400)
        self.assertEqual(self.s.req("POST", "/api/v1/stacks/tasked/runs?space=ops", {"kind": "task", "command": "state 'rm"})[0], 400)
        self.s.ok("POST", "/api/v1/policies", {"name": "no-state-surgery", "kind": "task", "space": "ops", "labels": ["autoattach:*"],
                                               "body": """package cairn
import rego.v1
deny contains "only site admins may change state by hand" if {
  input.task.command[0] == "state"
  input.task.command[1] in {"rm", "mv", "push", "replace-provider"}
  not input.principal.site_admin
}
"""})
        writer = self.s.ok("POST", "/api/v1/tokens", {"kind": "service", "roles": [{"space": "ops", "role": "writer"}]})["token"]
        st, b = self.s.req("POST", "/api/v1/stacks/tasked/runs?space=ops", {"kind": "task", "command": "state rm terraform_data.r[1]"}, token=writer)
        self.assertEqual(st, 403, b)
        self.assertIn("only site admins", json.dumps(b))
        st, _ = self.s.req("POST", "/api/v1/stacks/tasked/runs?space=ops", {"kind": "task", "command": "state list"}, token=writer)
        self.assertEqual(st, 201)

    def test_drift_detection_and_reconcile(self):
        self.deployed("drifty")
        clean = self.s.wait(self.start("drifty", {"kind": "drift"})["id"])
        self.assertEqual((clean["status"], clean["has_changes"]), ("planned_and_finished", False))
        self.assertFalse(self.s.ok("GET", "/api/v1/stacks/drifty?space=ops")["drift"]["detected"])
        # Someone removes a resource behind Cairn's back.
        self.s.wait(self.start("drifty", {"kind": "task", "command": "state rm terraform_data.r[1]"})["id"])
        drift = self.s.wait(self.start("drifty", {"kind": "drift", "reconcile": True})["id"])
        self.assertEqual((drift["has_changes"], drift["adds"]), (True, 1))
        stack = self.s.ok("GET", "/api/v1/stacks/drifty?space=ops")
        self.assertEqual((stack["drift"]["detected"], stack["drift"]["run_id"]), (True, drift["id"]))
        runs = self.s.ok("GET", "/api/v1/stacks/drifty/runs?space=ops")["items"]
        fix = next(r for r in runs if r["triggered_by"] == "drift:reconcile")
        self.assertEqual(self.s.wait(fix["id"])["status"], "applied")
        self.assertEqual(self.resources("drifty"), 3)

    def test_one_shot_schedules_fire(self):
        self.deployed("timed")
        sch = self.s.ok("POST", "/api/v1/stacks/timed/schedules?space=ops", {"kind": "drift", "at": in_seconds(1)})
        self.assertTrue(sch["next_run_at"])
        deadline = time.time() + 15
        while time.time() < deadline:
            got = self.s.ok("GET", "/api/v1/stacks/timed/schedules?space=ops")["items"][0]
            if got["last_run_id"]:
                break
            time.sleep(0.1)
        self.assertIsNone(got["next_run_at"], "one-shot schedules don't repeat")
        run = self.s.wait(got["last_run_id"])
        self.assertEqual((run["kind"], run["triggered_by"]), ("drift", f"schedule:{sch['id']}"))
        # A destroy schedule ("delete schedule").
        self.s.ok("POST", "/api/v1/stacks/timed/schedules?space=ops", {"kind": "destroy", "at": in_seconds(1)})
        deadline = time.time() + 20
        while time.time() < deadline and self.resources("timed") != 0:
            time.sleep(0.2)
        self.assertEqual(self.resources("timed"), 0)

    def test_cron_schedules_and_validation(self):
        self.deployed("cronned")
        sch = self.s.ok("POST", "/api/v1/stacks/cronned/schedules?space=ops", {"kind": "drift", "cron": "*/5 * * * *", "reconcile": True})
        nxt = datetime.datetime.fromisoformat(sch["next_run_at"].replace("Z", "+00:00"))
        self.assertEqual((nxt.minute % 5, nxt.second), (0, 0))
        self.assertEqual(sch["params"], {"reconcile": True})
        for bad in ({"kind": "drift", "cron": "61 * * * *"}, {"kind": "drift"}, {"kind": "nope", "cron": "@daily"},
                    {"kind": "drift", "cron": "@daily", "at": in_seconds(60)}, {"kind": "task", "cron": "@daily"}):
            self.assertEqual(self.s.req("POST", "/api/v1/stacks/cronned/schedules?space=ops", bad)[0], 400, bad)
        writer = self.s.ok("POST", "/api/v1/tokens", {"kind": "service", "roles": [{"space": "ops", "role": "writer"}]})["token"]
        self.assertEqual(self.s.req("POST", "/api/v1/stacks/cronned/schedules?space=ops", {"kind": "destroy", "cron": "@daily"},
                                    token=writer)[0], 403)
        self.assertEqual(self.s.req("POST", "/api/v1/stacks/cronned/schedules?space=ops", {"kind": "task", "cron": "@daily",
                                                                                             "command": "state list"}, token=writer)[0], 201)
        self.s.ok("DELETE", f"/api/v1/stacks/cronned/schedules/{sch['id']}?space=ops")
        self.assertEqual(len(self.s.ok("GET", "/api/v1/stacks/cronned/schedules?space=ops")["items"]), 1)

    @needs("cairn")
    def test_cli(self):
        self.deployed("viacli")
        env = dict(os.environ, CAIRN_HOST=self.s.base, CAIRN_TOKEN=TOKEN, NO_PROXY="*", CAIRN_SPACE="ops", CAIRN_STACK="viacli")

        def cli(*args, code=0):
            r = subprocess.run([CAIRN, *args], env=env, capture_output=True, text=True)
            self.assertEqual(r.returncode, code, r.stdout + r.stderr)
            return r.stdout

        self.assertIn("terraform_data.r[0]", cli("-o", "table", "stack", "task", "--", "state", "list"))
        cli("stack", "task", "--", "state", "rm", "terraform_data.r[2]")
        cli("stack", "drift", code=4)  # drift found: exit 4 like -detailed-exitcode
        sch = json.loads(cli("-o", "json", "schedule", "create", "--kind", "drift", "--cron", "@hourly"))
        self.assertIn(sch["id"], cli("schedule", "list"))
        cli("schedule", "delete", sch["id"])
        cli("stack", "destroy", "--auto-confirm")
        self.assertEqual(self.resources("viacli"), 0)


if __name__ == "__main__":
    unittest.main()
