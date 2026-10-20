#!/usr/bin/env python3
"""Generate the root Cargo.toml of this fork from Zed's root Cargo.toml.

Zed's root manifest is not part of the filtered history (it lists hundreds of
crates we don't carry), so it is regenerated after every upstream sync:

  * members                 -> upstream members that exist here, plus our own crates/* and examples/*
  * workspace.dependencies  -> only the entries our crates reference with `workspace = true`
  * patch.crates-io         -> only patches for crates in our dependency graph
  * profile.*.package       -> only packages in our dependency graph
  * workspace.package, lints, other profiles -> copied verbatim

Usage: script/gen-workspace-manifest.py [--upstream DIR] [--rev REV] [--lock]
  --upstream DIR  unfiltered Zed clone (default: $ZED_UPSTREAM or ../zed-upstream)
  --rev REV       upstream revision to read the manifest from, e.g. v1.24.3 (default: HEAD)
  --lock          also copy upstream Cargo.lock so we resolve the same versions as Zed
                  (cargo prunes the unused entries on the next build)
"""

import argparse
import glob
import os
import re
import subprocess
import sys
import tomllib

FORK = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ENTRY_RE = re.compile(r'^"?([A-Za-z0-9_.-]+)"?\s*=')
DEP_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


def load(path):
    with open(path, "rb") as f:
        return tomllib.load(f)


def git_show(upstream, rev, path):
    return subprocess.run(
        ["git", "-C", upstream, "show", f"{rev}:{path}"], capture_output=True, text=True, check=True
    ).stdout


def split_sections(text):
    """Split a TOML file into (header, lines) pairs, header None for the preamble."""
    sections, header, lines = [], None, []
    for line in text.splitlines():
        if line.startswith("["):
            sections.append((header, lines))
            header, lines = line.strip(), []
        else:
            lines.append(line)
    sections.append((header, lines))
    return sections


def filter_entries(lines, keep):
    """Keep `key = value` entries (with their continuation lines and leading comments) where keep(key)."""
    out, pending, current_kept = [], [], False
    for line in lines:
        m = ENTRY_RE.match(line)
        if m:
            current_kept = keep(m.group(1))
            if current_kept:
                out.extend(pending)
                out.append(line)
            pending = []
        elif line[:1] in (" ", "\t", "]", "}"):
            if current_kept:
                out.append(line)
        else:
            # comment or blank line: attach to the next entry
            pending.append(line)
    out.extend(l for l in pending if not l.strip())
    collapsed = []
    for line in out:
        if line.strip() or (collapsed and collapsed[-1].strip()):
            collapsed.append(line)
    while collapsed and not collapsed[0].strip():
        collapsed.pop(0)
    return collapsed


def manifest_dep_tables(manifest):
    for name in DEP_TABLES:
        yield manifest.get(name, {})
    for target in manifest.get("target", {}).values():
        for name in DEP_TABLES:
            yield target.get(name, {})


def lock_closure(lock, roots):
    deps = {}
    for pkg in lock.get("package", []):
        deps.setdefault(pkg["name"], set()).update(d.split(" ")[0] for d in pkg.get("dependencies", []))
    seen, stack = set(), list(roots)
    while stack:
        name = stack.pop()
        if name in seen:
            continue
        seen.add(name)
        stack.extend(deps.get(name, ()))
    return seen


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--upstream", default=os.environ.get("ZED_UPSTREAM", os.path.join(FORK, "..", "zed-upstream")))
    parser.add_argument("--rev", default="HEAD")
    parser.add_argument("--lock", action="store_true")
    args = parser.parse_args()
    upstream = os.path.abspath(args.upstream)

    upstream_text = git_show(upstream, args.rev, "Cargo.toml")
    upstream_lock_text = git_show(upstream, args.rev, "Cargo.lock")
    upstream_manifest = tomllib.loads(upstream_text)
    upstream_members = upstream_manifest["workspace"]["members"]

    members = [m for m in upstream_members if os.path.isfile(os.path.join(FORK, m, "Cargo.toml"))]
    own = glob.glob(os.path.join(FORK, "crates", "*", "Cargo.toml")) + glob.glob(os.path.join(FORK, "examples", "*", "Cargo.toml"))
    for path in sorted(own):
        member = os.path.relpath(os.path.dirname(path), FORK)
        if member not in members:
            members.append(member)

    member_manifests = [load(os.path.join(FORK, m, "Cargo.toml")) for m in members]
    member_names = {m["package"]["name"] for m in member_manifests}
    used_workspace_deps = {
        key
        for manifest in member_manifests
        for table in manifest_dep_tables(manifest)
        for key, spec in table.items()
        if isinstance(spec, dict) and spec.get("workspace")
    }
    graph = lock_closure(tomllib.loads(upstream_lock_text), member_names) | member_names

    def keep_patch(key):
        spec = upstream_manifest["patch"]["crates-io"][key]
        if "path" in spec and not os.path.exists(os.path.join(FORK, spec["path"])):
            return False
        return key in graph

    rev = subprocess.run(
        ["git", "-C", upstream, "rev-parse", f"{args.rev}^{{commit}}"], capture_output=True, text=True, check=True
    ).stdout.strip()
    source = rev if args.rev == "HEAD" else f"{rev} ({args.rev})"
    out = [
        f"# @generated by script/gen-workspace-manifest.py from zed-industries/zed@{source}",
        "# Re-run the script after syncing upstream instead of editing this file by hand.",
        "",
    ]
    for header, lines in split_sections(upstream_text):
        if header is None:
            continue
        if header == "[workspace]":
            body = [f'resolver = "{upstream_manifest["workspace"].get("resolver", "2")}"', "members = ["]
            body += [f'    "{m}",' for m in members]
            body += ["]"]
        elif header == "[workspace.dependencies]":
            body = filter_entries(lines, lambda k: k in used_workspace_deps)
        elif header.startswith("[workspace.dependencies."):
            if header[len("[workspace.dependencies.") : -1] not in used_workspace_deps:
                continue
            body = lines
        elif header == "[patch.crates-io]":
            body = filter_entries(lines, keep_patch)
        elif re.fullmatch(r"\[profile\.[\w-]+\.package\]", header):
            body = filter_entries(lines, lambda k: k in graph)
        elif header.startswith("[workspace.metadata."):
            continue
        else:
            body = lines
        if not any(l.strip() and not l.lstrip().startswith("#") for l in body):
            continue
        while body and not body[-1].strip():
            body = body[:-1]
        out += [header, *body, ""]

    with open(os.path.join(FORK, "Cargo.toml"), "w") as f:
        f.write("\n".join(out))
    print(f"wrote Cargo.toml: {len(members)} members, {len(used_workspace_deps)} workspace deps (zed@{source})")

    if args.lock:
        with open(os.path.join(FORK, "Cargo.lock"), "w") as f:
            f.write(upstream_lock_text)
        print("copied upstream Cargo.lock")


if __name__ == "__main__":
    sys.exit(main())
