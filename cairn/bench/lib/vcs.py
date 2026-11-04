"""Local Gitea as the VCS fixture (API-compatible stand-in for Forgejo).

Gitea is shared infrastructure, like the user's existing Forgejo, so it is
NOT included in any tool's resource totals.
"""
import os
import subprocess
import tempfile
import time

import harness as h

NAME = "bench-gitea"
IMAGE = "gitea/gitea:1.24-rootless"
PORT = 3000
URL = f"http://localhost:{PORT}"
USER = "bench"
PASSWORD = "bench-password-123"
_token = None


def up():
    """Start Gitea (idempotent) and return an admin API token."""
    global _token
    r = h.sh(["docker", "inspect", "-f", "{{.State.Running}}", NAME], check=False, capture=True)
    if r.stdout.strip() != "true":
        h.docker_rm(NAME)
        h.sh(["docker", "run", "-d", "--name", NAME, "--network", "host",
              "-e", "GITEA__security__INSTALL_LOCK=true",
              "-e", "GITEA__database__DB_TYPE=sqlite3",
              "-e", f"GITEA__server__ROOT_URL={URL}/",
              "-e", f"GITEA__server__HTTP_PORT={PORT}",
              "-e", "GITEA__server__SSH_LISTEN_PORT=2223",
              "-e", "GITEA__server__START_SSH_SERVER=false",
              "-e", "GITEA__webhook__ALLOWED_HOST_LIST=*",
              "-e", "GITEA__webhook__DELIVER_TIMEOUT=30",
              "-e", "GITEA__service__DISABLE_REGISTRATION=true",
              "-e", "GITEA__repository__DEFAULT_BRANCH=main",
              IMAGE], capture=True)
        h.wait_http(f"{URL}/api/healthz", timeout=120)
        h.sh(["docker", "exec", NAME, "gitea", "admin", "user", "create", "--admin",
              "--username", USER, "--password", PASSWORD, "--email", "bench@example.com",
              "--must-change-password=false"], check=False, capture=True)
    if _token is None:
        r = h.sh(["docker", "exec", NAME, "gitea", "admin", "user", "generate-access-token",
                  "--username", USER, "--token-name", f"bench-{time.time_ns()}",
                  "--scopes", "all", "--raw"], capture=True)
        _token = r.stdout.strip().splitlines()[-1]
    return _token


def api(method, path, body=None):
    return h.http(method, f"{URL}/api/v1{path}", body=body,
                  headers={"Authorization": f"token {up()}", "Content-Type": "application/json"})


def push_dir(repo, src_dir, branch="main", message="bench fixture"):
    """Create USER/repo if needed and force-push src_dir's files to branch."""
    s, _ = api("GET", f"/repos/{USER}/{repo}")
    if s == 404:
        s, b = api("POST", "/user/repos", {"name": repo, "default_branch": "main", "private": False})
        assert s in (200, 201), (s, b)
    with tempfile.TemporaryDirectory(dir=h.WORK) as d:
        h.sh(f"cp -r {src_dir}/. {d}/ && rm -rf {d}/.git {d}/.terraform*", capture=True)
        env = dict(os.environ, GIT_AUTHOR_NAME="bench", GIT_AUTHOR_EMAIL="bench@example.com",
                   GIT_COMMITTER_NAME="bench", GIT_COMMITTER_EMAIL="bench@example.com")
        remote = f"http://{USER}:{up()}@localhost:{PORT}/{USER}/{repo}.git"
        for cmd in (["git", "init", "-q", "-b", branch], ["git", "add", "-A"],
                    ["git", "-c", "commit.gpgsign=false", "commit", "-q", "-m", message],
                    ["git", "push", "-q", "-f", remote, f"HEAD:{branch}"]):
            subprocess.run(cmd, cwd=d, env=env, check=True, capture_output=True)
    return f"{URL}/{USER}/{repo}.git"


def mirror_github(owner_repo, repo, branch="main"):
    """Copy a public GitHub repo (shallow) into Gitea so runs never leave the host."""
    s, _ = api("GET", f"/repos/{USER}/{repo}")
    if s == 200:
        return f"{URL}/{USER}/{repo}.git"
    with tempfile.TemporaryDirectory(dir=h.WORK) as d:
        h.sh(["git", "clone", "-q", "--depth", "1", "-b", branch,
              f"https://github.com/{owner_repo}", d], capture=True)
        return push_dir(repo, d, branch=branch, message=f"mirror of {owner_repo}")


def down():
    h.docker_rm(NAME)


def push_branch(repo, branch, marker):
    """Branch off main with a one-line .tf change (enough to trigger autoplan)."""
    env = dict(os.environ, GIT_AUTHOR_NAME="bench", GIT_AUTHOR_EMAIL="bench@example.com",
               GIT_COMMITTER_NAME="bench", GIT_COMMITTER_EMAIL="bench@example.com")
    remote = f"http://{USER}:{up()}@localhost:{PORT}/{USER}/{repo}.git"
    with tempfile.TemporaryDirectory(dir=h.WORK) as d:
        run = lambda *c: subprocess.run(c, cwd=d, env=env, check=True, capture_output=True)
        run("git", "clone", "-q", "--depth", "1", remote, ".")
        run("git", "checkout", "-q", "-b", branch)
        with open(os.path.join(d, "main.tf"), "a") as f:
            f.write(f"\n# {marker}\n")
        run("git", "-c", "commit.gpgsign=false", "commit", "-q", "-am", marker)
        run("git", "push", "-q", "-f", remote, branch)


def open_pr(repo, branch, title):
    s, b = api("POST", f"/repos/{USER}/{repo}/pulls", {"head": branch, "base": "main", "title": title})
    assert s in (200, 201), (s, b)
    return b["number"]


def comments(repo, number):
    s, b = api("GET", f"/repos/{USER}/{repo}/issues/{number}/comments")
    return b if s == 200 else []


def comment(repo, number, body):
    s, b = api("POST", f"/repos/{USER}/{repo}/issues/{number}/comments", {"body": body})
    assert s in (200, 201), (s, b)


def close_pr(repo, number):
    api("PATCH", f"/repos/{USER}/{repo}/pulls/{number}", {"state": "closed"})


def add_webhook(repo, url, secret, events):
    s, hooks = api("GET", f"/repos/{USER}/{repo}/hooks")
    for hk in hooks or []:
        api("DELETE", f"/repos/{USER}/{repo}/hooks/{hk['id']}")
    s, b = api("POST", f"/repos/{USER}/{repo}/hooks", {
        "type": "gitea", "active": True, "events": events,
        "config": {"url": url, "content_type": "json", "secret": secret}})
    assert s in (200, 201), (s, b)


def merge_pr(repo, number):
    s, b = api("POST", f"/repos/{USER}/{repo}/pulls/{number}/merge",
               {"Do": "merge", "delete_branch_after_merge": True})
    assert s in (200, 204), (s, b)
