#!/usr/bin/env python3
"""Copies the licence files of every crate compiled into kalila's binaries for TARGET into
OUT_DIR/<crate>-<version>/, and writes OUT_DIR/README listing each crate with its licence.

    packaging/collect-licenses.py x86_64-unknown-linux-gnu OUT_DIR

The binaries link these crates statically; their licences (MIT, Apache-2.0, ...) ask for the
notice to travel with the binary. Workspace crates (kalila's own, GPL) are left out; liboni's
licence is copied by ci-build.sh. Uses only `cargo metadata` and the standard library.
"""
import json
import os
import shutil
import subprocess
import sys

target, out = sys.argv[1], sys.argv[2]
meta = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target]))
pkgs = {p["id"]: p for p in meta["packages"]}
nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
workspace = set(meta["workspace_members"])

# Normal and build dependencies reachable from the two shipped binaries' packages (dev
# dependencies are not compiled into them).
roots = [i for i in workspace if pkgs[i]["name"] in ("kalila", "engine")]
seen, stack = set(), list(roots)
while stack:
    i = stack.pop()
    if i in seen:
        continue
    seen.add(i)
    for d in nodes[i]["deps"]:
        kinds = {k["kind"] for k in d["dep_kinds"]}
        if kinds - {"dev"}:
            stack.append(d["pkg"])

os.makedirs(out, exist_ok=True)
rows, missing = [], []
for i in sorted(seen - workspace, key=lambda i: (pkgs[i]["name"], pkgs[i]["version"])):
    p = pkgs[i]
    src = os.path.dirname(p["manifest_path"])
    files = [f for f in sorted(os.listdir(src))
             if f.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE"))]
    dst = os.path.join(out, f"{p['name']}-{p['version']}")
    os.makedirs(dst, exist_ok=True)
    for f in files:
        s = os.path.join(src, f)
        if os.path.isfile(s):
            shutil.copy(s, dst)
    if not files:
        missing.append(p["name"])
    rows.append(f"{p['name']} {p['version']}: {p.get('license') or p.get('license_file') or '?'}")

with open(os.path.join(out, "README"), "w") as f:
    f.write("Crates compiled into kalila and kalila-headless, with their licences.\n")
    f.write("Each folder holds the licence files the crate ships.\n\n")
    f.write("\n".join(rows) + "\n")
    if missing:
        f.write("\nShipping no licence file (see its licence field above):\n")
        f.write("\n".join(missing) + "\n")
print(f"licences of {len(rows)} crates in {out} ({len(missing)} without a licence file)")
