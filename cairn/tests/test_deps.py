"""Stack dependencies: output references, triggering on change, ordering,
cycles, trigger policies and the CLI."""
import json
import os
import shutil
import subprocess
import time
import unittest

from support import CAIRN, TOKEN, Server, needs, tarball

NET_TF = """
variable "n" {
  type    = number
  default = 1
}
variable "note" {
  type    = string
  default = "a"
}
resource "terraform_data" "vpc" { input = "vpc-${var.n}" }
output "vpc_id" { value = terraform_data.vpc.output }
output "cidrs" { value = ["10.0.${var.n}.0/24"] }
output "note" { value = var.note }
"""

APP_TF = """
variable "vpc_id" { type = string }
variable "cidrs" { type = list(string) }
resource "terraform_data" "app" { input = var.vpc_id }
output "seen" { value = "${var.vpc_id} ${var.cidrs[0]}" }
"""

LEAF_TF = """
resource "terraform_data" "leaf" { input = "x" }
output "ok" { value = true }
"""


@needs("cairnd", "tofu")
class Dependencies(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        s = cls.s = Server()
        s.ok("POST", "/api/v1/spaces", {"name": "dep"})
        for name in ("net", "app", "db", "audit"):
            s.ok("POST", "/api/v1/stacks", {"name": name, "space": "dep", "auto_apply": True})

    @classmethod
    def tearDownClass(cls):
        cls.s.stop()
        shutil.rmtree(cls.s.dir, ignore_errors=True)

    def apply(self, stack, tf):
        r = self.s.ok("POST", f"/api/v1/stacks/{stack}/runs?space=dep&kind=tracked", tarball({"main.tf": tf}))
        return self.s.wait(r["id"])

    def runs(self, stack):
        return self.s.ok("GET", f"/api/v1/stacks/{stack}/runs?space=dep")["items"]

    def output(self, stack, name):
        return self.s.ok("GET", f"/api/v1/stacks/{stack}/outputs?space=dep")[name]["value"]

    def wait_runs(self, stack, n, timeout=60):
        deadline = time.time() + timeout
        while time.time() < deadline:
            rs = self.runs(stack)
            if len(rs) >= n and rs[0]["status"] in ("applied", "planned_and_finished", "errored", "canceled"):
                return rs
            time.sleep(0.1)
        raise AssertionError(f"{stack}: expected {n} settled runs, have {self.runs(stack)}")

    def test_references_triggering_ordering_and_policies(self):
        s = self.s
        s.ok("POST", "/api/v1/stacks/app/dependencies?space=dep", {"stack": "net", "space": "dep",
              "references": [{"output": "vpc_id", "input": "vpc_id"}, {"output": "cidrs", "input": "cidrs"}]})
        s.ok("POST", "/api/v1/stacks/db/dependencies?space=dep", {"stack": "app", "space": "dep"})
        deps = s.ok("GET", "/api/v1/stacks/app/dependencies?space=dep")
        self.assertEqual(deps["depends_on"][0]["stack"]["name"], "net")
        self.assertEqual(deps["dependents"][0]["stack"]["name"], "db")
        # Cycles are refused, directly and transitively.
        self.assertEqual(s.req("POST", "/api/v1/stacks/net/dependencies?space=dep", {"stack": "app", "space": "dep"})[0], 400)
        self.assertEqual(s.req("POST", "/api/v1/stacks/net/dependencies?space=dep", {"stack": "db", "space": "dep"})[0], 400)

        # Before net exists, app can't get its inputs.
        r = self.apply("app", APP_TF)
        self.assertEqual(r["status"], "errored")
        self.assertIn("has no output", r["error"])

        self.assertEqual(self.apply("net", NET_TF)["status"], "applied")
        # app has only errored so far, so it is triggered with its latest configuration.
        app_runs = self.wait_runs("app", 2)
        self.assertEqual(app_runs[0]["status"], "applied", app_runs[0])
        self.assertTrue(app_runs[0]["triggered_by"].startswith("dependency:"))
        self.assertEqual(self.output("app", "seen"), "vpc-1 10.0.1.0/24")
        # db depends on app without references: it follows every apply of app.
        s.ok("POST", "/api/v1/stacks/db/runs?space=dep&kind=tracked", tarball({"main.tf": LEAF_TF}))
        db_before = len(self.wait_runs("db", 1))

        # A referenced output changes: app runs again, then db.
        self.assertEqual(self.apply("net", NET_TF.replace("default = 1", "default = 2"))["status"], "applied")
        app_runs = self.wait_runs("app", 3)
        self.assertEqual(self.output("app", "seen"), "vpc-2 10.0.2.0/24")
        self.wait_runs("db", db_before + 1)
        # An unreferenced output changes: app is left alone.
        self.assertEqual(self.apply("net", NET_TF.replace("default = 1", "default = 2").replace('default = "a"', 'default = "b"'))["status"],
                         "applied")
        time.sleep(1)
        self.assertEqual(len(self.runs("app")), 3)

        # Ordering: app's tracked run waits for net's queued/running run.
        r_net = s.ok("POST", "/api/v1/stacks/net/runs?space=dep&kind=tracked",
                     tarball({"main.tf": NET_TF.replace("default = 1", "default = 3")}))
        r_app = s.ok("POST", "/api/v1/stacks/app/runs?space=dep", {"kind": "tracked"})
        net_done, app_done = s.wait(r_net["id"]), s.wait(r_app["id"])
        self.assertEqual((net_done["status"], app_done["status"]), ("applied", "applied"))
        self.assertGreaterEqual(app_done["status_times"]["planning"], net_done["status_times"]["applied"])
        self.assertEqual(self.output("app", "seen").split()[0], "vpc-3")

        # Trigger policies can add stacks.
        s.ok("POST", "/api/v1/stacks/audit/runs?space=dep&kind=tracked", tarball({"main.tf": LEAF_TF}))
        audit_before = len(self.wait_runs("audit", 1))
        s.ok("POST", "/api/v1/policies", {"name": "net-audit", "kind": "trigger", "space": "dep", "labels": ["autoattach:*"],
                                          "body": """package cairn
import rego.v1
trigger contains "dep/audit" if { input.stack.name == "net"; "vpc_id" in input.outputs_changed }
"""})
        self.apply("net", NET_TF.replace("default = 1", "default = 4"))
        audit = self.wait_runs("audit", audit_before + 1)
        self.assertTrue(audit[0]["triggered_by"].startswith("dependency:"))

    def test_access_and_validation(self):
        s = self.s
        s.ok("POST", "/api/v1/spaces", {"name": "secret"})
        s.ok("POST", "/api/v1/stacks", {"name": "vault", "space": "secret"})
        s.ok("POST", "/api/v1/stacks", {"name": "consumer", "space": "dep"})
        admin = s.ok("POST", "/api/v1/tokens", {"kind": "service", "roles": [{"space": "dep", "role": "admin"}]})["token"]
        st, _ = s.req("POST", "/api/v1/stacks/consumer/dependencies?space=dep", {"stack": "vault", "space": "secret"}, token=admin)
        self.assertEqual(st, 404, "can't wire in outputs from a stack you can't read")
        for bad in ([{"output": "x", "input": "1bad"}], [{"output": "a", "input": "v"}, {"output": "b", "input": "v"}]):
            self.assertEqual(s.req("POST", "/api/v1/stacks/consumer/dependencies?space=dep",
                                   {"stack": "audit", "space": "dep", "references": bad})[0], 400, bad)
        self.assertEqual(s.req("POST", "/api/v1/stacks/consumer/dependencies?space=dep", {"stack": "consumer", "space": "dep"})[0], 400)
        up = s.ok("GET", "/api/v1/stacks/audit?space=dep")["id"]
        s.ok("POST", "/api/v1/stacks/consumer/dependencies?space=dep", {"stack": "audit", "space": "dep"})
        s.ok("DELETE", f"/api/v1/stacks/consumer/dependencies/{up}?space=dep")
        self.assertEqual(s.req("DELETE", f"/api/v1/stacks/consumer/dependencies/{up}?space=dep")[0], 404)

    @needs("cairn")
    def test_cli(self):
        self.s.ok("POST", "/api/v1/stacks", {"name": "cli-up", "space": "dep", "auto_apply": True})
        self.s.ok("POST", "/api/v1/stacks", {"name": "cli-down", "space": "dep", "auto_apply": True})
        env = dict(os.environ, CAIRN_HOST=self.s.base, CAIRN_TOKEN=TOKEN, NO_PROXY="*", CAIRN_SPACE="dep")
        r = subprocess.run([CAIRN, "-o", "json", "dep", "add", "-s", "cli-down", "--on", "cli-up", "--on-space", "dep",
                            "--ref", "vpc_id=vpc_id"], env=env, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.loads(r.stdout)["references"], [{"output": "vpc_id", "input": "vpc_id"}])
        r = subprocess.run([CAIRN, "-o", "table", "dep", "list", "-s", "cli-up"], env=env, capture_output=True, text=True)
        self.assertIn("dep/cli-down", r.stdout)
        self.assertIn("vpc_id→vpc_id", r.stdout)


if __name__ == "__main__":
    unittest.main()
