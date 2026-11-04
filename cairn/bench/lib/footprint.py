#!/usr/bin/env python3
"""Static footprint of each upstream submodule: code size and direct deps.

Counts non-blank lines in git-tracked, non-test, non-generated source files,
grouped by language. Writes results/footprint.json.
"""
import json
import os
import re
import subprocess

BENCH = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
UPSTREAM = os.path.join(BENCH, "..", "upstream")
LANGS = {".go": "Go", ".java": "Java", ".ts": "TypeScript", ".tsx": "TypeScript",
         ".js": "JavaScript", ".jsx": "JavaScript", ".templ": "templ", ".sql": "SQL",
         ".groovy": "Groovy", ".py": "Python", ".rs": "Rust"}
SKIP = re.compile(r"(^|/)(test|tests|testdata|e2e|node_modules|vendor|docs?|website|"
                  r"public|dist|build|mocks?|fixtures?)(/|$)|_test\.go$|\.test\.|\.spec\.|"
                  r"Test\.java$|_templ\.go$|\.pb\.go$|\.min\.js$|zz_generated|mock_")


def loc(repo):
    files = subprocess.run(["git", "-C", repo, "ls-files"], capture_output=True, text=True).stdout.split()
    out = {}
    for f in files:
        ext = os.path.splitext(f)[1]
        if ext not in LANGS or SKIP.search(f):
            continue
        try:
            with open(os.path.join(repo, f), errors="ignore") as fh:
                n = sum(1 for line in fh if line.strip())
        except OSError:
            continue
        out[LANGS[ext]] = out.get(LANGS[ext], 0) + n
    return dict(sorted(out.items(), key=lambda kv: -kv[1]))


def go_deps(repo):
    txt = open(os.path.join(repo, "go.mod")).read()
    direct = indirect = 0
    for line in txt.splitlines():
        line = line.strip()
        if re.match(r"^[\w.\-]+\.[\w.\-/]+ v", line) or line.startswith("require ") and " v" in line:
            if "// indirect" in line:
                indirect += 1
            else:
                direct += 1
    return {"go_direct": direct, "go_indirect": indirect}


def maven_deps(repo):
    total = 0
    for root, _, files in os.walk(repo):
        if "node_modules" in root:
            continue
        for f in files:
            if f == "pom.xml":
                total += open(os.path.join(root, f)).read().count("<dependency>")
    return {"maven_declared": total}


def npm_deps(repo):
    total = 0
    for root, dirs, files in os.walk(repo):
        dirs[:] = [d for d in dirs if d != "node_modules"]
        if "package.json" in files:
            pj = json.load(open(os.path.join(root, "package.json")))
            total += len(pj.get("dependencies", {})) + len(pj.get("devDependencies", {}))
    return {"npm_declared": total}


def main():
    out = {}
    for name in sorted(os.listdir(UPSTREAM)):
        repo = os.path.join(UPSTREAM, name)
        if not os.path.isdir(os.path.join(repo, ".git")) and not os.path.isfile(os.path.join(repo, ".git")):
            continue
        rev = subprocess.run(["git", "-C", repo, "describe", "--tags"], capture_output=True, text=True).stdout.strip()
        d = {"version": rev, "loc": loc(repo)}
        if os.path.exists(os.path.join(repo, "go.mod")):
            d.update(go_deps(repo))
        if os.path.exists(os.path.join(repo, "pom.xml")):
            d.update(maven_deps(repo))
        d.update(npm_deps(repo))
        out[name] = d
    path = os.path.join(BENCH, "results", "footprint.json")
    with open(path, "w") as f:
        json.dump(out, f, indent=2)
        f.write("\n")
    print(json.dumps(out, indent=2))


if __name__ == "__main__":
    main()
