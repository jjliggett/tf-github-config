"""MCP tools for policies, drift, tasks, dependencies, schedules and the
registry — and the human gate on everything that changes infrastructure."""
import json
import shutil
import unittest

from support import MAIN_TF, Mcp, Server, needs, tarball

GATE = """package cairn
import rego.v1
approve if count([r | some r in input.reviews; r.decision == "approve"]) > 0
"""


@needs("cairnd", "cairn", "tofu")
class AgentTools(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        s = cls.s = Server()
        s.ok("POST", "/api/v1/spaces", {"name": "ai"})
        s.ok("POST", "/api/v1/stacks", {"name": "web", "space": "ai", "auto_apply": True})
        s.ok("POST", "/api/v1/stacks", {"name": "edge", "space": "ai", "auto_apply": True})
        r = s.ok("POST", "/api/v1/stacks/web/runs?space=ai&kind=tracked", tarball({"main.tf": MAIN_TF}))
        assert s.wait(r["id"])["status"] == "applied"
        s.ok("POST", "/api/v1/stacks/edge/dependencies?space=ai", {"stack": "web", "space": "ai",
                                                                     "references": [{"output": "count", "input": "n"}]})
        s.ok("POST", "/api/v1/stacks/web/schedules?space=ai", {"kind": "drift", "cron": "@hourly"})
        s.ok("POST", "/api/v1/registry/modules/ai/cdn/null/1.0.0", tarball({"main.tf": "output \"x\" { value = 1 }\n"}))

    @classmethod
    def tearDownClass(cls):
        cls.s.stop()
        shutil.rmtree(cls.s.dir, ignore_errors=True)

    def structured(self, res):
        self.assertFalse(res.get("isError"), res)
        return res["structuredContent"]

    def test_read_only_tools(self):
        m = Mcp(self.s, read_only=True)
        try:
            names = {t["name"] for t in m.call("tools/list", {})["tools"]}
            for t in ("detect_drift", "get_run_policies", "list_policies", "list_dependencies", "list_schedules", "search_registry"):
                self.assertIn(t, names)
            for t in ("review_run", "run_task", "sync_stack", "confirm_run"):
                self.assertNotIn(t, names)
            deps = self.structured(m.tool("list_dependencies", stack="web", space="ai"))
            self.assertEqual(deps["dependents"][0]["references"], [{"output": "count", "input": "n"}])
            self.assertEqual(self.structured(m.tool("list_schedules", stack="web", space="ai"))["items"][0]["kind"], "drift")
            reg = self.structured(m.tool("search_registry", query="cdn"))
            self.assertEqual(reg["modules"][0]["versions"], ["1.0.0"])
            self.assertTrue(reg["modules"][0]["source"].endswith("/ai/cdn/null"))
            drift = self.structured(m.tool("detect_drift", stack="web", space="ai"))
            self.assertEqual((drift["run"]["kind"], drift["drifted"]), ("drift", False))
        finally:
            m.close()

    def test_tasks_are_gated_unless_read_only(self):
        m = Mcp(self.s, elicit_answer=False)
        try:
            out = self.structured(m.tool("run_task", stack="web", space="ai", command="state list"))
            self.assertEqual(out["run"]["status"], "finished")
            self.assertTrue(any("terraform_data.r[0]" in l for l in out["output"]))
            self.assertFalse(getattr(m, "elicitations", []), "read-only commands don't ask")
            out = self.structured(m.tool("run_task", stack="web", space="ai", command="state rm terraform_data.r[0]"))
            self.assertEqual(out["status"], "not_approved")
            self.assertIn("state rm", m.elicitations[0])
        finally:
            m.close()
        self.assertEqual(self.s.ok("GET", "/api/v1/stacks/web/state/versions?space=ai")["items"][0]["resource_count"], 3)
        # A client that can't ask a human gets told who has to do it.
        m = Mcp(self.s)
        try:
            res = m.tool("run_task", stack="web", space="ai", command="state rm terraform_data.r[0]")
            self.assertTrue(res["isError"])
            self.assertIn("needs_human", res["content"][0]["text"])
        finally:
            m.close()

    def test_review_needs_a_human_to_approve(self):
        self.s.ok("POST", "/api/v1/stacks", {"name": "gated", "space": "ai", "auto_apply": True, "labels": ["gated"]})
        self.s.ok("POST", "/api/v1/policies", {"name": "ai-gate", "kind": "approval", "space": "ai", "body": GATE,
                                               "labels": ["autoattach:gated"]})
        r = self.s.ok("POST", "/api/v1/stacks/gated/runs?space=ai&kind=tracked", tarball({"main.tf": MAIN_TF}))
        self.assertEqual(self.s.wait(r["id"])["approval"], "pending")
        m = Mcp(self.s, elicit_answer=False)
        try:
            self.assertEqual(self.structured(m.tool("review_run", run_id=r["id"], decision="approve"))["status"], "not_approved")
        finally:
            m.close()
        self.assertEqual(self.s.ok("GET", f"/api/v1/runs/{r['id']}/policies")["reviews"], [])
        m = Mcp(self.s, elicit_answer=True)
        try:
            out = self.structured(m.tool("review_run", run_id=r["id"], decision="approve", note="agent + human"))
            self.assertEqual(out["approval"], "approved")
            pol = self.structured(m.tool("get_run_policies", run_id=r["id"]))
            self.assertEqual(pol["reviews"][0]["note"], "agent + human")
            self.assertEqual([p["name"] for p in self.structured(m.tool("list_policies", kind="approval"))["items"]], ["ai-gate"])
            self.assertNotIn("body", json.dumps(self.structured(m.tool("list_policies"))["items"][0]))
        finally:
            m.close()
        self.assertEqual(self.s.wait(r["id"], until=lambda x: x["status"] in ("applied", "errored"))["status"], "applied")


if __name__ == "__main__":
    unittest.main()
