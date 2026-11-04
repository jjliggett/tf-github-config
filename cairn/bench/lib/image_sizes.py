#!/usr/bin/env python3
"""Report compressed (registry) image sizes per architecture.

Usage: image_sizes.py IMAGE[:TAG] ...   (prints JSON lines)

Reads manifests straight from the registry so it works for images that
are not pulled locally and for architectures other than the host's.
"""
import json
import sys
import time
import urllib.error
import urllib.request

ACCEPT = ",".join([
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
])


def parse(ref):
    name, _, tag = ref.rpartition(":")
    first = name.split("/")[0]
    if "/" in name and ("." in first or ":" in first):
        registry, _, name = name.partition("/")
    else:
        registry = "registry-1.docker.io"
        if "/" not in name:
            name = "library/" + name
    return registry, name, tag


def token(registry, name):
    if registry == "registry-1.docker.io":
        url = f"https://auth.docker.io/token?service=registry.docker.io&scope=repository:{name}:pull"
    else:
        url = f"https://{registry}/token?scope=repository:{name}:pull"
    with urllib.request.urlopen(url) as r:
        return json.load(r)["token"]


def get(registry, name, ref, tok, attempts=4):
    req = urllib.request.Request(
        f"https://{registry}/v2/{name}/manifests/{ref}",
        headers={"Authorization": f"Bearer {tok}", "Accept": ACCEPT},
    )
    for i in range(attempts):
        try:
            with urllib.request.urlopen(req) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            if e.code != 429 or i == attempts - 1:
                raise
            time.sleep(15 * (i + 1))  # Docker Hub anonymous rate limit


def main():
    for ref in sys.argv[1:]:
        registry, name, tag = parse(ref)
        tok = token(registry, name)
        top = get(registry, name, tag, tok)
        out = {"image": ref}
        manifests = top.get("manifests")
        if manifests is None:  # single-arch image
            manifests = [{"digest": tag, "platform": {"architecture": "amd64", "os": "linux"}, "_m": top}]
        for m in manifests:
            p = m.get("platform", {})
            arch = p.get("architecture")
            if p.get("os") != "linux" or arch not in ("amd64", "arm64"):
                continue
            man = m.get("_m") or get(registry, name, m["digest"], tok)
            out[f"{arch}_compressed_bytes"] = sum(l["size"] for l in man["layers"])
            out[f"{arch}_layers"] = len(man["layers"])
        print(json.dumps(out))


if __name__ == "__main__":
    main()
