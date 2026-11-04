"""VCS integrations against fake provider APIs (all six providers) and a real Gitea.

Webhooks are built and signed here the way each provider does it; the fake
APIs check each provider's auth header and record statuses and comments.
"""
import base64
import hashlib
import hmac
import json
import os
import shutil
import subprocess
import time
import unittest
import urllib.request

from fakes import FakeForge
from support import CAIRN, MAIN_TF, TOKEN, Server, free_port, needs

SMALL_TF = MAIN_TF.replace("default = 3", "default = 1")
PROVIDERS = ["github", "gitea", "forgejo", "gitlab", "bitbucket", "azure_devops"]
REPO = {"azure_devops": "Platform/infra"}
SECRET = "hook-secret"


def repo_of(provider):
    return REPO.get(provider, "acme/infra")


def _sign(provider, kind, payload, secret=SECRET):
    body = json.dumps(payload).encode()
    mac = hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
    h = {"Content-Type": "application/json"}
    if provider == "github":
        h.update({"X-GitHub-Event": kind, "X-Hub-Signature-256": f"sha256={mac}"})
    elif provider == "gitea":
        h.update({"X-Gitea-Event": kind, "X-Gitea-Signature": mac})
    elif provider == "forgejo":
        h.update({"X-Forgejo-Event": kind, "X-Forgejo-Signature": mac})
    elif provider == "gitlab":
        h.update({"X-Gitlab-Event": kind, "X-Gitlab-Token": secret})
    elif provider == "bitbucket":
        h.update({"X-Event-Key": kind, "X-Hub-Signature": f"sha256={mac}"})
    elif provider == "azure_devops":
        h["Authorization"] = "Basic " + base64.b64encode(f"cairn:{secret}".encode()).decode()
    return h, body


def push(provider, branch, sha, message="change", files=None):
    repo = repo_of(provider)
    commits = [{"added": files or [], "modified": [], "removed": []}] if files is not None else []
    if provider in ("github", "gitea", "forgejo"):
        return _sign(provider, "push", {"ref": f"refs/heads/{branch}", "after": sha, "repository": {"full_name": repo},
                                        "head_commit": {"message": message}, "pusher": {"name": "dev"}, "commits": commits})
    if provider == "gitlab":
        return _sign(provider, "Push Hook", {"ref": f"refs/heads/{branch}", "checkout_sha": sha, "user_username": "dev",
                                             "project": {"path_with_namespace": repo},
                                             "commits": [dict(c, message=message) for c in commits]})
    if provider == "bitbucket":
        return _sign(provider, "repo:push", {"repository": {"full_name": repo}, "actor": {"display_name": "dev"},
                                             "push": {"changes": [{"new": {"type": "branch", "name": branch,
                                                                           "target": {"hash": sha, "message": message}}}]}})
    project, name = repo.split("/")
    return _sign(provider, None, {"eventType": "git.push", "resource": {
        "refUpdates": [{"name": f"refs/heads/{branch}", "newObjectId": sha}], "commits": [{"comment": message}],
        "pushedBy": {"displayName": "dev"}, "repository": {"name": name, "project": {"name": project}}}})


def pull_request(provider, number, action, sha, head="feature", base="main"):
    repo = repo_of(provider)
    if provider in ("github", "gitea", "forgejo"):
        act = {"opened": "opened", "updated": "synchronized" if provider != "github" else "synchronize",
               "closed": "closed", "merged": "closed"}[action]
        return _sign(provider, "pull_request", {"action": act, "number": number, "repository": {"full_name": repo},
                                                "pull_request": {"number": number, "merged": action == "merged", "title": "t",
                                                                 "head": {"sha": sha, "ref": head}, "base": {"ref": base},
                                                                 "user": {"login": "dev"}}})
    if provider == "gitlab":
        act = {"opened": "open", "updated": "update", "closed": "close", "merged": "merge"}[action]
        return _sign(provider, "Merge Request Hook", {"project": {"path_with_namespace": repo}, "user": {"username": "dev"},
                                                      "object_attributes": {"iid": number, "action": act, "oldrev": "x",
                                                                            "last_commit": {"id": sha}, "title": "t",
                                                                            "source_branch": head, "target_branch": base}})
    if provider == "bitbucket":
        key = {"opened": "created", "updated": "updated", "closed": "rejected", "merged": "fulfilled"}[action]
        return _sign(provider, f"pullrequest:{key}", {"repository": {"full_name": repo}, "actor": {"display_name": "dev"},
                                                      "pullrequest": {"id": number, "title": "t",
                                                                      "source": {"commit": {"hash": sha}, "branch": {"name": head}},
                                                                      "destination": {"branch": {"name": base}}}})
    project, name = repo.split("/")
    et = {"opened": "created", "updated": "updated", "closed": "updated", "merged": "merged"}[action]
    status = {"closed": "abandoned", "merged": "completed"}.get(action, "active")
    return _sign(provider, None, {"eventType": f"git.pullrequest.{et}", "resource": {
        "pullRequestId": number, "status": status, "title": "t", "createdBy": {"displayName": "dev"},
        "repository": {"name": name, "project": {"name": project}}, "lastMergeSourceCommit": {"commitId": sha},
        "sourceRefName": f"refs/heads/{head}", "targetRefName": f"refs/heads/{base}"}})


def comment(provider, number, text):
    repo = repo_of(provider)
    if provider in ("github", "gitea", "forgejo"):
        return _sign(provider, "issue_comment", {"action": "created", "repository": {"full_name": repo}, "is_pull": True,
                                                 "issue": {"number": number, "pull_request": {}},
                                                 "comment": {"body": text, "user": {"login": "dev"}}})
    if provider == "gitlab":
        return _sign(provider, "Note Hook", {"project": {"path_with_namespace": repo}, "user": {"username": "dev"},
                                             "object_attributes": {"noteable_type": "MergeRequest", "note": text},
                                             "merge_request": {"iid": number}})
    if provider == "bitbucket":
        return _sign(provider, "pullrequest:comment_created", {"repository": {"full_name": repo}, "actor": {"display_name": "dev"},
                                                               "pullrequest": {"id": number}, "comment": {"content": {"raw": text}}})
    project, name = repo.split("/")
    return _sign(provider, None, {"eventType": "ms.vss-code.git-pullrequest-comment-event", "resource": {
        "comment": {"content": text, "author": {"displayName": "dev"}},
        "pullRequest": {"pullRequestId": number, "repository": {"name": name, "project": {"name": project}}}}})


def until(fn, timeout=30, what="condition"):
    deadline = time.time() + timeout
    while time.time() < deadline:
        v = fn()
        if v:
            return v
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {what}")


@needs("cairnd", "tofu")
class Providers(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.s = Server(["--allow-private-webhooks"])
        cls.fakes = {}

    @classmethod
    def tearDownClass(cls):
        for f in cls.fakes.values():
            f.stop()
        cls.s.stop()
        shutil.rmtree(cls.s.dir, ignore_errors=True)

    def setup_provider(self, prov, stack="app", **stack_kw):
        if prov not in self.fakes:
            f = self.fakes[prov] = FakeForge(prov)
            space = f"vcs-{prov.replace('_', '-')}"
            self.s.ok("POST", "/api/v1/spaces", {"name": space})
            body = {"name": f"forge-{prov.replace('_', '-')}", "provider": prov, "space": space, "token": f.token,
                    "url": f.url if prov in ("gitea", "forgejo") else f.api_url, "webhook_secret": SECRET}
            if prov == "bitbucket":
                body["web_url"] = f.url
            f.integration = self.s.ok("POST", "/api/v1/vcs", body)
            f.space = space
        f = self.fakes[prov]
        self.s.ok("POST", "/api/v1/stacks", {"name": stack, "space": f.space, "auto_apply": True, **stack_kw,
                                             "vcs": {"integration": f.integration["name"], "repo": repo_of(prov),
                                                     "branch": "main", **stack_kw.pop("vcs", {})}})
        return f

    def hook(self, f, hb, expect=202):
        headers, body = hb
        st, resp = self.s.req("POST", f"/webhooks/vcs/{f.integration['id']}", body, token=None,
                              ctype="application/json", headers=headers)
        self.assertEqual(st, expect, resp)
        return resp

    def runs(self, f, stack="app"):
        return self.s.ok("GET", f"/api/v1/stacks/{stack}/runs?space={f.space}")["items"]

    def settled(self, f, n, stack="app"):
        def done():
            rs = self.runs(f, stack)
            return rs if len(rs) >= n and all(r["status"] in ("applied", "planned_and_finished", "errored", "planned",
                                                                "discarded", "canceled") for r in rs) else None
        return until(done, 60, f"{n} settled runs on {stack}")

    def provider_flow(self, prov):
        f = self.setup_provider(prov)
        f.commit(repo_of(prov), "a" * 40, {"main.tf": SMALL_TF})
        resp = self.hook(f, push(prov, "main", "a" * 40, "first"))
        self.assertEqual(resp["actions"][0]["decision"], "track")
        run = self.settled(f, 1)[0]
        self.assertEqual((run["kind"], run["status"]), ("tracked", "applied"), run)
        self.assertEqual(run["source"]["sha"], "a" * 40)
        self.assertTrue(run["triggered_by"].startswith(f"vcs:{prov}:"))
        until(lambda: any(sha == "a" * 40 and b.get("state", b.get("state")) in ("success", "SUCCESSFUL", "succeeded")
                          for _, sha, b in f.statuses), what="success status")
        # Pushes elsewhere are ignored.
        self.assertEqual(self.hook(f, push(prov, "feature", "b" * 40))["actions"][0]["decision"], "ignore")
        # Pull request: proposed run, one comment that is updated in place.
        f.commit(repo_of(prov), "c" * 40, {"main.tf": MAIN_TF})
        self.assertEqual(self.hook(f, pull_request(prov, 7, "opened", "c" * 40))["actions"][0]["decision"], "propose")
        pr_run = self.settled(f, 2)[0]
        self.assertEqual((pr_run["kind"], pr_run["status"], pr_run["adds"]), ("proposed", "planned_and_finished", 2))
        c = until(lambda: [x for x in f.comments.values() if x["pr"] == 7], what="PR comment")[0]
        self.assertIn("### Cairn", c["body"])
        self.assertIn(f"`{f.space}/app`", c["body"])
        self.assertIn("+2 ~0 -0", c["body"])
        f.commit(repo_of(prov), "d" * 40, {"main.tf": MAIN_TF.replace("default = 3", "default = 2")})
        self.hook(f, pull_request(prov, 7, "updated", "d" * 40))
        self.settled(f, 3)
        until(lambda: f.comments[next(k for k, v in f.comments.items() if v["pr"] == 7)]["edits"] >= 1, what="comment edit")
        self.assertIn("+1 ~0 -0", [x for x in f.comments.values() if x["pr"] == 7][0]["body"])
        self.assertEqual(len([x for x in f.comments.values() if x["pr"] == 7]), 1, "one comment per PR")
        # Comment commands.
        self.assertEqual(self.hook(f, comment(prov, 7, "/cairn plan"))["actions"][0]["decision"], "propose")
        self.assertEqual(self.settled(f, 4)[0]["source"]["sha"], "d" * 40)
        self.assertEqual(self.hook(f, comment(prov, 7, "/cairn apply"))["actions"][0]["decision"], "ignore")
        self.assertEqual(self.hook(f, comment(prov, 7, "nice work"))["actions"], [])
        self.hook(f, pull_request(prov, 7, "merged", "d" * 40))
        self.assertEqual(f.unauthorized, 0)
        # Forged deliveries are rejected: tampered bodies (HMAC providers) and wrong secrets.
        h, b = push(prov, "main", "e" * 40)
        if prov not in ("gitlab", "azure_devops"):  # these send a shared token, not a body signature
            self.hook(f, (h, b + b" "), expect=401)
        h, _ = _sign(prov, "push", {}, secret="wrong")
        self.hook(f, (h, b), expect=401)

    def test_github(self):
        self.provider_flow("github")

    def test_gitea(self):
        self.provider_flow("gitea")

    def test_forgejo(self):
        self.provider_flow("forgejo")

    def test_gitlab(self):
        self.provider_flow("gitlab")

    def test_bitbucket(self):
        self.provider_flow("bitbucket")

    def test_azure_devops(self):
        self.provider_flow("azure_devops")

    def test_path_filter_and_push_policy(self):
        f = self.setup_provider("github", stack="mono", project_root="infra")
        f.commit(repo_of("github"), "1" * 40, {"infra/main.tf": SMALL_TF, "README.md": "hi"})
        actions = self.hook(f, push("github", "main", "1" * 40, files=["README.md"]))["actions"]
        self.assertEqual({a["stack"]: a["decision"] for a in actions}["mono"], "ignore")
        actions = self.hook(f, push("github", "main", "1" * 40, files=["infra/main.tf"]))["actions"]
        self.assertEqual({a["stack"]: a["decision"] for a in actions}["mono"], "track")
        self.assertEqual(self.settled(f, 1, "mono")[0]["status"], "applied")
        # Push policy: feature branches get proposed runs.
        self.s.ok("POST", "/api/v1/policies", {"name": "propose-branches", "kind": "push", "space": f.space,
                                               "labels": ["autoattach:mono"], "body": """package cairn
import rego.v1
track if input.push.branch == input.stack.branch
propose if { input.push; input.push.branch != input.stack.branch }
propose if input.pull_request
"""})
        self.s.ok("PATCH", f"/api/v1/stacks/mono?space={f.space}", {"labels": ["mono"]})
        f.commit(repo_of("github"), "2" * 40, {"infra/main.tf": MAIN_TF})
        actions = self.hook(f, push("github", "wip", "2" * 40))["actions"]
        self.assertEqual({a["stack"]: a["decision"] for a in actions}["mono"], "propose")
        r = self.settled(f, 2, "mono")[0]
        self.assertEqual((r["kind"], r["status"]), ("proposed", "planned_and_finished"))

    def test_pr_apply_and_close_releases_lock(self):
        f = self.setup_provider("github", stack="gated", labels=["gated"], vcs={"pr_apply": True})
        self.s.ok("POST", "/api/v1/policies", {"name": "gh-approval", "kind": "approval", "space": f.space,
                                               "labels": ["autoattach:gated"],
                                               "body": "package cairn\nimport rego.v1\napprove if count(input.reviews) > 0\n"})
        f.commit(repo_of("github"), "9" * 40, {"main.tf": SMALL_TF})
        self.hook(f, pull_request("github", 42, "opened", "9" * 40))
        self.settled(f, 1, "gated")
        self.assertEqual(self.hook(f, comment("github", 42, "/cairn apply gated"))["actions"][0]["decision"], "track")
        r = until(lambda: [x for x in self.runs(f, "gated") if x["kind"] == "tracked" and x["status"] == "planned"],
                  what="tracked PR run held for approval")[0]
        self.assertEqual(r["approval"], "pending")
        self.assertTrue(self.s.ok("GET", f"/api/v1/stacks/gated?space={f.space}")["locked"])
        resp = self.hook(f, pull_request("github", 42, "closed", "9" * 40))
        self.assertEqual(resp["actions"][0]["stopped_runs"], 1)
        self.assertEqual(self.s.ok("GET", f"/api/v1/runs/{r['id']}")["status"], "discarded")
        self.assertFalse(self.s.ok("GET", f"/api/v1/stacks/gated?space={f.space}")["locked"])

    def test_sync_and_validation(self):
        f = self.setup_provider("gitlab", stack="synced")
        f.commit(repo_of("gitlab"), "main", {"main.tf": SMALL_TF})
        r = self.s.ok("POST", f"/api/v1/stacks/synced/sync?space={f.space}")
        self.assertEqual(self.s.wait(r["id"])["status"], "applied")
        self.assertEqual(self.s.req("POST", "/api/v1/vcs", {"name": "x", "provider": "svn", "token": "t", "space": f.space})[0], 400)
        self.assertEqual(self.s.req("POST", "/api/v1/vcs", {"name": "y", "provider": "gitea", "token": "t", "space": f.space})[0], 400)
        st, _ = self.s.req("POST", "/api/v1/stacks", {"name": "bad", "space": f.space, "vcs": {"integration": "nope", "repo": "a/b"}})
        self.assertEqual(st, 400)
        # Integrations in other spaces can't be used.
        self.s.ok("POST", "/api/v1/spaces", {"name": "elsewhere"})
        st, _ = self.s.req("POST", "/api/v1/stacks", {"name": "x", "space": "elsewhere",
                                                      "vcs": {"integration": f.integration["name"], "repo": "a/b"}})
        self.assertEqual(st, 400)
        listed = self.s.ok("GET", "/api/v1/vcs")["items"]
        self.assertNotIn("webhook_secret", listed[0])
        self.assertTrue(listed[0]["webhook_url"].endswith(f"/webhooks/vcs/{listed[0]['id']}"))

    @needs("cairn")
    def test_cli(self):
        f = FakeForge("forgejo")
        self.fakes["cli-forgejo"] = f
        self.s.ok("POST", "/api/v1/spaces", {"name": "cli-vcs"})
        env = dict(os.environ, CAIRN_HOST=self.s.base, CAIRN_TOKEN=TOKEN, NO_PROXY="*", CAIRN_SPACE="cli-vcs")

        def cli(*args):
            r = subprocess.run([CAIRN, "-o", "json", *args], env=env, capture_output=True, text=True)
            self.assertEqual(r.returncode, 0, r.stderr)
            return json.loads(r.stdout) if r.stdout.strip() else None

        v = cli("vcs", "create", "codeberg-ish", "--provider", "forgejo", "--url", f.url, "--access-token", f.token)
        self.assertTrue(v["webhook_secret"])
        cli("stack", "create", "site", "--auto-apply", "--vcs", "codeberg-ish", "--repo", "acme/site", "--branch", "trunk")
        f.commit("acme/site", "trunk", {"main.tf": SMALL_TF})
        r = cli("stack", "sync", "-s", "site")
        self.assertEqual(self.s.wait(r["id"])["status"], "applied")
        self.assertIn("codeberg-ish", [x["name"] for x in cli("vcs", "list")["items"]])
        stack = self.s.ok("GET", "/api/v1/stacks/site?space=cli-vcs")
        self.assertEqual(stack["vcs"]["branch"], "trunk")


GITEA_IMAGE = os.environ.get("GITEA_IMAGE", "gitea/gitea:1.24-rootless")


@needs("cairnd", "tofu", "docker")
class RealGitea(unittest.TestCase):
    """End to end with a real Gitea (Forgejo's API is the same): its own
    webhook deliveries, archives, statuses and comments."""

    @classmethod
    def setUpClass(cls):
        cls.port = free_port()
        cls.url = f"http://127.0.0.1:{cls.port}"
        cls.name = f"cairn-test-gitea-{cls.port}"
        subprocess.run(["docker", "run", "-d", "--rm", "--name", cls.name, "--network", "host",
                        "-e", "GITEA__security__INSTALL_LOCK=true", "-e", "GITEA__database__DB_TYPE=sqlite3",
                        "-e", f"GITEA__server__ROOT_URL={cls.url}/", "-e", f"GITEA__server__HTTP_PORT={cls.port}",
                        "-e", "GITEA__server__START_SSH_SERVER=false", "-e", "GITEA__webhook__ALLOWED_HOST_LIST=*",
                        "-e", "GITEA__repository__DEFAULT_BRANCH=main", GITEA_IMAGE], check=True, capture_output=True)
        until(lambda: cls._get("/api/healthz")[0] == 200, 120, "gitea")
        subprocess.run(["docker", "exec", cls.name, "gitea", "admin", "user", "create", "--admin", "--username", "dev",
                        "--password", "dev-password-123", "--email", "dev@example.com", "--must-change-password=false"],
                       check=True, capture_output=True)
        out = subprocess.run(["docker", "exec", cls.name, "gitea", "admin", "user", "generate-access-token", "--username", "dev",
                              "--token-name", "cairn", "--scopes", "all", "--raw"], check=True, capture_output=True, text=True)
        cls.token = out.stdout.strip().splitlines()[-1]
        cls.s = Server(["--allow-private-webhooks"])

    @classmethod
    def tearDownClass(cls):
        cls.s.stop()
        subprocess.run(["docker", "rm", "-f", cls.name], capture_output=True)
        shutil.rmtree(cls.s.dir, ignore_errors=True)

    @classmethod
    def _get(cls, path):
        return cls.api("GET", path, raw=True)

    @classmethod
    def api(cls, method, path, body=None, raw=False):
        h = {"Content-Type": "application/json"}
        if not raw:
            h["Authorization"] = f"token {cls.token}"
        req = urllib.request.Request(cls.url + path, data=json.dumps(body).encode() if body is not None else None,
                                     method=method, headers=h)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        try:
            with opener.open(req, timeout=30) as r:
                data = r.read()
                return r.status, json.loads(data) if data else None
        except urllib.error.HTTPError as e:
            return e.code, e.read()
        except OSError:
            return 0, None

    def put_file(self, path, text, branch="main", new_branch=None, message="update"):
        st, cur = self.api("GET", f"/api/v1/repos/dev/infra/contents/{path}?ref={branch}")
        body = {"content": base64.b64encode(text.encode()).decode(), "message": message, "branch": branch}
        if new_branch:
            body["new_branch"] = new_branch
        if st == 200:
            body["sha"] = cur["sha"]
            st, r = self.api("PUT", f"/api/v1/repos/dev/infra/contents/{path}", body)
        else:
            st, r = self.api("POST", f"/api/v1/repos/dev/infra/contents/{path}", body)
        self.assertIn(st, (200, 201), r)
        return r["commit"]["sha"]

    def test_end_to_end(self):
        st, r = self.api("POST", "/api/v1/user/repos", {"name": "infra", "default_branch": "main", "auto_init": True})
        self.assertEqual(st, 201, r)
        s = self.s
        s.ok("POST", "/api/v1/spaces", {"name": "forge"})
        integ = s.ok("POST", "/api/v1/vcs", {"name": "gitea", "provider": "gitea", "space": "forge", "url": self.url,
                                             "token": self.token})
        s.ok("POST", "/api/v1/stacks", {"name": "infra", "space": "forge", "auto_apply": True,
                                        "vcs": {"integration": "gitea", "repo": "dev/infra", "branch": "main"}})
        st, r = self.api("POST", "/api/v1/repos/dev/infra/hooks", {
            "type": "gitea", "active": True, "events": ["push", "pull_request", "issue_comment"],
            "config": {"url": integ["webhook_url"], "content_type": "json", "secret": integ["webhook_secret"]}})
        self.assertEqual(st, 201, r)

        sha = self.put_file("main.tf", SMALL_TF, message="add infra")
        run = until(lambda: [x for x in s.ok("GET", "/api/v1/stacks/infra/runs?space=forge")["items"]
                             if x["status"] == "applied"], 90, "applied run from push")[0]
        self.assertEqual(run["source"]["sha"], sha)
        statuses = until(lambda: [x for x in (self.api("GET", f"/api/v1/repos/dev/infra/commits/{sha}/statuses")[1] or [])
                                  if x["status"] == "success"], 30, "gitea commit status")
        self.assertEqual(statuses[0]["context"], "cairn/forge/infra")

        head = self.put_file("main.tf", MAIN_TF, new_branch="grow", message="grow")
        st, pr = self.api("POST", "/api/v1/repos/dev/infra/pulls", {"head": "grow", "base": "main", "title": "Grow"})
        self.assertEqual(st, 201, pr)
        until(lambda: [x for x in s.ok("GET", "/api/v1/stacks/infra/runs?space=forge")["items"]
                       if x["kind"] == "proposed" and x["status"] == "planned_and_finished"], 90, "proposed run from PR")
        comments = until(lambda: [c for c in (self.api("GET", f"/api/v1/repos/dev/infra/issues/{pr['number']}/comments")[1] or [])
                                  if "### Cairn" in c["body"]], 30, "PR comment")
        self.assertIn("+2 ~0 -0", comments[0]["body"])
        self.assertTrue(any(x["status"] == "success" for x in
                            self.api("GET", f"/api/v1/repos/dev/infra/commits/{head}/statuses")[1] or []))


if __name__ == "__main__":
    unittest.main()
