#!/usr/bin/env python3
"""Track Zed's release channels in this fork.

Zed cuts a release branch `vX.Y.x` from `main` every week. Its first tag is
`vX.Y.0-pre` (preview); fixes are cherry-picked and tagged `vX.Y.N-pre`, and a
week later the line ships as stable `vX.Y.N`. Stable and preview tags live on
those release branches, not on Zed's `main`. This fork mirrors the channels:

  nightly  <- Zed main
  preview  <- newest vX.Y.Z-pre tag   (fork patches normally land here)
  main     <- newest vX.Y.Z tag; before taking a new stable, preview is merged as
              of just before it took the next minor's -pre, so fork patches reach
              stable the same way Zed's own fixes do

Hotfixes for stable go on main and are merged back into preview by hand.

Downstream branches hold fork features and are fed from another local branch, not from upstream:

  experimental  <- nightly        (fork features are developed here)
  liquid-glass  <- experimental   (stable line of the liquid glass work)

Upstream history comes in through `git filter-repo` "streams", one per
script/upstream-paths/*.txt. filter-repo is deterministic, so re-filtering a
newer Zed yields the same hashes for old commits and merging keeps working.
NEVER edit an existing stream file: that rewrites every hash in the stream. To
carry another upstream path, add a new stream file.

Refs in this repo:
  refs/zed/<stream>/main, refs/zed/<stream>/tags/<tag>  filtered upstream, per stream
  tags zed-<tag>                                          same commits as refs/zed/<first stream>/tags/<tag>
  tags vX.Y.Z[-pre][-fork.N]                              fork releases, created by `tag`

Channel branches share one fork-only root commit (the first-parent root of each branch,
upstream always comes in as a second parent); a missing channel branch is recreated from it.

Usage:
  script/sync-upstream.py fetch                       refresh the Zed clone and refs/zed/*
  script/sync-upstream.py update [--no-fetch] [BRANCH...]
                                                      merge upstream into nightly, preview, main (default: these
                                                      three), or a downstream branch's source into it
  script/sync-upstream.py update --to TAG BRANCH      merge a specific upstream release instead of the newest
  script/sync-upstream.py status                      show what each branch is based on
  script/sync-upstream.py tag [--no-verify]           check, test and tag the current branch as a fork release

Env: ZED_UPSTREAM  unfiltered Zed clone (default ../zed-upstream, cloned if missing)
"""

import argparse
import glob
import os
import re
import shutil
import subprocess
import sys

FORK = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT = os.path.join(FORK, "script")
UPSTREAM_URL = "https://github.com/zed-industries/zed.git"
UPSTREAM_TAGS = ("refs/tags/v*:refs/tags/v*", "refs/tags/gpui-v*:refs/tags/gpui-v*")
# Commit hashes are only stable across runs of the same filter-repo release, so every machine
# (and CI) must run this one. `git filter-repo --version` prints this for 2.47.0.
FILTER_REPO_VERSION = "a40bce548d2c"
GENERATED = ("Cargo.toml", "Cargo.lock")
VERSION_RE = re.compile(r"^v(\d+)\.(\d+)\.(\d+)(-pre)?$")
# channel branch -> whether it follows -pre tags (None: follows Zed main)
CHANNELS = {"nightly": None, "preview": True, "main": False}
# downstream branch -> the local branch it merges from, in update order
DOWNSTREAM = {"experimental": "nightly", "liquid-glass": "experimental"}
GENERATED_FROM_RE = re.compile(r"zed-industries/zed@([0-9a-f]{40})")


def run(*cmd, cwd=FORK, check=True):
    result = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if not check:
        return result
    if result.returncode:
        sys.exit(f"$ {' '.join(cmd)}\n{result.stdout}{result.stderr}")
    return result.stdout.strip()


def git(*args, cwd=FORK, check=True):
    return run("git", *args, cwd=cwd, check=check)


def git_ok(*args):
    return git(*args, check=False).returncode == 0


def ref_exists(ref):
    return git_ok("rev-parse", "--verify", "--quiet", ref)


def is_ancestor(a, b):
    return git_ok("merge-base", "--is-ancestor", a, b)


def upstream_dir():
    return os.path.abspath(os.environ.get("ZED_UPSTREAM", os.path.join(FORK, "..", "zed-upstream")))


def streams():
    return [os.path.basename(p)[: -len(".txt")] for p in sorted(glob.glob(f"{SCRIPT}/upstream-paths/*.txt"))]


def parse_version(tag):
    m = VERSION_RE.match(tag)
    return (int(m[1]), int(m[2]), int(m[3]), bool(m[4])) if m else None


def zed_tags(pre):
    """Upstream release tags of one channel, as {tag: version}."""
    refs = git("for-each-ref", "--format=%(refname:strip=4)", f"refs/zed/{streams()[0]}/tags")
    versions = {name: parse_version(name) for name in refs.splitlines()}
    return {name: v for name, v in versions.items() if v and v[3] == pre}


def latest_zed_tag(pre):
    tags = zed_tags(pre)
    return max(tags, key=tags.get) if tags else None


def based_on(branch, pre):
    """Newest upstream release tag of the channel merged into `branch`."""
    names = git("tag", "--merged", branch, "--list", "zed-v*").splitlines()
    versions = {n[len("zed-") :]: parse_version(n[len("zed-") :]) for n in names}
    versions = {name: v for name, v in versions.items() if v and v[3] == pre}
    return max(versions, key=versions.get) if versions else None


def fork_root():
    """The fork-only root commit, found as the first-parent root of any existing channel branch."""
    for branch in CHANNELS:
        if ref_exists(f"refs/heads/{branch}"):
            return git("rev-list", "--first-parent", "--max-parents=0", branch).splitlines()[0]
    return None


def require_clean():
    if git("status", "--porcelain", "--untracked-files=no"):
        sys.exit("working tree has uncommitted changes; commit or stash them first")


def fetch():
    version = run("git", "filter-repo", "--version", check=False).stdout.strip()
    if version != FILTER_REPO_VERSION:
        sys.exit(
            f"git filter-repo {version or '(not installed)'} found, need {FILTER_REPO_VERSION} (2.47.0): "
            "another release may produce different hashes and break merging"
        )
    upstream = upstream_dir()
    if not os.path.isdir(os.path.join(upstream, ".git")):
        print(f"cloning {UPSTREAM_URL} into {upstream} ...")
        run("git", "clone", "--single-branch", "--branch", "main", "--no-tags", UPSTREAM_URL, upstream)
    git("pull", "--quiet", "--ff-only", "--no-tags", "origin", "main", cwd=upstream)
    git("fetch", "--quiet", "--no-tags", "origin", *UPSTREAM_TAGS, cwd=upstream)
    print(f"upstream: zed@{git('rev-parse', '--short', 'main', cwd=upstream)}")

    work = upstream + ".filter-work"
    for stream in streams():
        shutil.rmtree(work, ignore_errors=True)
        git("clone", "--quiet", "--bare", "--no-local", upstream, work)
        # --preserve-commit-hashes: by default filter-repo rewrites hashes quoted in commit messages
        # ("This reverts commit ..."), which makes the result depend on which refs were filtered
        git(
            "filter-repo", "--quiet", "--preserve-commit-hashes",
            "--paths-from-file", f"{SCRIPT}/upstream-paths/{stream}.txt",
            cwd=work,
        )
        git(
            "fetch", "--quiet", "--no-tags", "--prune", work,
            f"+refs/heads/main:refs/zed/{stream}/main",
            f"+refs/tags/*:refs/zed/{stream}/tags/*",
        )
        print(f"stream {stream}: refs/zed/{stream}/main = {git('rev-parse', '--short', f'refs/zed/{stream}/main')}")
    shutil.rmtree(work, ignore_errors=True)

    existing = dict(
        line.split() for line in git("for-each-ref", "--format=%(refname:strip=2) %(objectname)", "refs/tags/zed-*").splitlines()
    )
    created, rewritten = [], []
    refs = git("for-each-ref", "--format=%(refname:strip=4) %(objectname) %(*objectname)", f"refs/zed/{streams()[0]}/tags")
    for line in refs.splitlines():
        name, obj, *peeled = line.split()
        commit, tag = (peeled[0] if peeled else obj), f"zed-{name}"
        if tag not in existing:
            created.append(f"create refs/tags/{tag} {commit}")
        elif existing[tag] != commit:
            rewritten.append(tag)
    if created:
        subprocess.run(["git", "update-ref", "--stdin"], input="\n".join(created) + "\n", text=True, cwd=FORK, check=True)
    print(f"zed-* tags: {len(created)} new, {len(existing) + len(created)} total")
    if rewritten:
        # either upstream moved a release tag, or filtering stopped being deterministic (different
        # filter-repo, edited stream file); merging now could duplicate the whole history
        sys.exit(
            f"filtered commits no longer match existing zed-* tags: {', '.join(rewritten[:10])}"
            + (" ..." if len(rewritten) > 10 else "")
            + "\nIf upstream really moved these tags, delete the local zed-* tags and fetch again."
        )


def regenerate(upstream_rev):
    run(sys.executable, f"{SCRIPT}/gen-workspace-manifest.py", "--upstream", upstream_dir(), "--rev", upstream_rev, "--lock")
    # resolving prunes the copied upstream Cargo.lock down to our dependency graph
    result = run("cargo", "metadata", "--format-version", "1", check=False)
    if result.returncode:
        print(f"warning: cargo metadata failed, Cargo.lock left unpruned:\n{result.stderr[-2000:]}")
    git("add", *GENERATED)


def merge(ref, message, upstream_rev):
    args = ["merge", "--no-ff", "--no-commit", "-m", message]
    if not git_ok("merge-base", "HEAD", ref):
        args.append("--allow-unrelated-histories")
    result = git(*args, ref, check=False)
    conflicted = set(git("diff", "--name-only", "--diff-filter=U").splitlines())
    if conflicted - set(GENERATED):
        sys.exit(
            f"merge of {ref} has conflicts:\n  " + "\n  ".join(sorted(conflicted)) + "\n"
            "Resolve them, then run\n"
            f"  script/gen-workspace-manifest.py --rev {upstream_rev} --lock && git add {' '.join(GENERATED)} && git commit --no-edit\n"
            "and re-run `script/sync-upstream.py update --no-fetch`."
        )
    if result.returncode and not conflicted:
        sys.exit(result.stdout + result.stderr)
    # Cargo.toml / Cargo.lock conflicts resolve themselves: they are regenerated for the merged tree
    regenerate(upstream_rev)
    git("commit", "--quiet", "--no-edit")


def preview_cut(stable):
    """Newest commit on preview's first-parent line before it merged a -pre tag newer than `stable`'s minor."""
    if not ref_exists("refs/heads/preview"):
        return None
    line = git("rev-list", "--first-parent", "preview").splitlines()  # newest first
    position = {commit: i for i, commit in enumerate(line)}
    cut = 0
    for tag, version in zed_tags(pre=True).items():
        if version[:2] <= parse_version(stable)[:2] or not is_ancestor(f"zed-{tag}", "preview"):
            continue
        # first-parent commits containing the tag, newest first; the last one is the merge that took it
        containing = git("rev-list", "--first-parent", "--ancestry-path", f"zed-{tag}..preview").splitlines()
        if containing:
            cut = max(cut, position[containing[-1]] + 1)
    return line[cut] if cut < len(line) else None


def update_branch(branch, target=None):
    pre = CHANNELS[branch]
    if pre is None:
        target, upstream_rev = "main", "main"
        label = f"Zed main @ {git('rev-parse', '--short', 'main', cwd=upstream_dir())}"
    else:
        if target is not None and target not in zed_tags(pre):
            sys.exit(f"{target} is not an upstream {'-pre' if pre else 'stable'} tag")
        target = target or latest_zed_tag(pre)
        if target is None:
            print(f"{branch}: no upstream {'-pre' if pre else 'stable'} tag found, skipping")
            return
        upstream_rev, label = f"refs/tags/{target}", f"zed {target}"

    if not ref_exists(f"refs/heads/{branch}"):
        root = fork_root()
        if root is None:
            sys.exit(f"branch {branch} does not exist and there is no other channel branch to find the fork root from")
        git("branch", branch, root)
    git("checkout", "--quiet", branch)

    merges = []
    if branch == "main":
        cut = preview_cut(target)
        if cut and not is_ancestor(cut, "HEAD"):
            merges.append((cut, f"Merge preview into main for {label}"))
    multi_stream = len(streams()) > 1
    for stream in streams():
        ref = f"refs/zed/{stream}/main" if target == "main" else f"refs/zed/{stream}/tags/{target}"
        if ref_exists(ref) and not is_ancestor(ref, "HEAD"):
            merges.append((ref, f"Merge {label} into {branch}" + (f" [{stream}]" if multi_stream else "")))

    changed = False
    for ref, message in merges:
        # an earlier merge may already contain it: release tags with no changes under our paths
        # filter to the same commit as the previous tag
        if is_ancestor(ref, "HEAD"):
            continue
        merge(ref, message, upstream_rev)
        print(f"{branch}: {message}")
        changed = True
    # Zed's manifest and lock can change between releases even when our paths don't
    regenerate(upstream_rev)
    if not git_ok("diff", "--cached", "--quiet"):
        git("commit", "--quiet", "-m", f"Regenerate workspace manifest for {label}")
        print(f"{branch}: regenerated workspace manifest for {label}")
        changed = True
    if not changed:
        print(f"{branch}: up to date with {label}")


def generated_from(ref):
    """The upstream commit that `ref`'s Cargo.toml was generated from."""
    m = GENERATED_FROM_RE.search(git("show", f"{ref}:Cargo.toml"))
    if not m:
        sys.exit(f"{ref}:Cargo.toml has no @generated header; run script/gen-workspace-manifest.py there first")
    return m[1]


def update_downstream(branch):
    source = DOWNSTREAM[branch]
    if not ref_exists(f"refs/heads/{source}"):
        sys.exit(f"{branch}: source branch {source} does not exist")
    if not ref_exists(f"refs/heads/{branch}"):
        git("branch", branch, source)
        print(f"{branch}: created from {source}")
        return
    git("checkout", "--quiet", branch)
    if is_ancestor(source, "HEAD"):
        print(f"{branch}: up to date with {source}")
        return
    # the source is the upstream-ward side, so the merged tree is at its upstream revision
    message = f"Merge {source} into {branch}"
    merge(source, message, generated_from(source))
    print(f"{branch}: {message}")


def update(branches, no_fetch, to):
    unknown = [b for b in branches if b not in CHANNELS and b not in DOWNSTREAM]
    if unknown:
        sys.exit(f"unknown branch {', '.join(unknown)}; branches are {', '.join([*CHANNELS, *DOWNSTREAM])}")
    if to and (len(branches) != 1 or branches[0] not in ("preview", "main")):
        sys.exit("--to needs exactly one branch: preview or main")
    require_clean()
    channels = [b for b in CHANNELS if not branches or b in branches]
    if channels and not no_fetch:
        fetch()
    original = git("rev-parse", "--abbrev-ref", "HEAD")
    for branch in channels:
        update_branch(branch, to)
    for branch in DOWNSTREAM:
        if branch in branches:
            update_downstream(branch)
    if original != "HEAD":
        git("checkout", "--quiet", original)


def status():
    upstream_main = f"refs/zed/{streams()[0]}/main"
    for branch, pre in CHANNELS.items():
        if not ref_exists(f"refs/heads/{branch}"):
            print(f"{branch:8} (missing)")
            continue
        if pre is None:
            behind = git("rev-list", "--count", f"{branch}..{upstream_main}")
            print(f"{branch:8} Zed main, {behind} upstream commits not merged")
            continue
        current, latest = based_on(branch, pre), latest_zed_tag(pre)
        fork_tags = git("tag", "--points-at", branch, "--list", "v*").split()
        state = "up to date" if current == latest else f"behind (latest {latest})"
        print(f"{branch:8} zed {current or '-'}, {state}, fork tag at HEAD: {', '.join(fork_tags) or '-'}")
    for branch, source in DOWNSTREAM.items():
        if not ref_exists(f"refs/heads/{branch}"):
            print(f"{branch:8} (missing)")
            continue
        behind = git("rev-list", "--count", f"{branch}..{source}")
        print(f"{branch:8} {behind} commits of {source} not merged")


def tag(verify):
    branch = git("rev-parse", "--abbrev-ref", "HEAD")
    if branch not in ("main", "preview"):
        sys.exit("fork releases are tagged on main or preview")
    require_clean()
    base = based_on(branch, CHANNELS[branch])
    if base is None:
        sys.exit(f"{branch} has no upstream release merged yet")
    at_head = [t for t in git("tag", "--points-at", "HEAD", "--list", "v*").split() if t.startswith(base)]
    if at_head:
        print(f"HEAD is already tagged {', '.join(at_head)}")
        return
    name, n = base, 0
    while ref_exists(f"refs/tags/{name}"):
        n += 1
        name = f"{base}-fork.{n}"
    if verify:
        for cmd in (["cargo", "check", "--workspace", "--all-targets"], ["cargo", "test", "-p", "gpui"]):
            print(f"$ {' '.join(cmd)}")
            if subprocess.run(cmd, cwd=FORK).returncode:
                sys.exit(f"`{' '.join(cmd)}` failed; not tagging")
    zed_commit = git("rev-parse", "--short", f"zed-{base}")
    git("tag", "-a", name, "-m", f"gpui-fork {name}\n\nBased on zed {base} (filtered {zed_commit}).")
    print(f"tagged {name} on {branch}; publish with `git push origin {name}`")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("fetch")
    update_parser = commands.add_parser("update")
    update_parser.add_argument("branches", nargs="*")
    update_parser.add_argument("--no-fetch", action="store_true")
    update_parser.add_argument("--to", metavar="TAG", help="merge this upstream tag instead of the newest one")
    commands.add_parser("status")
    tag_parser = commands.add_parser("tag")
    tag_parser.add_argument("--no-verify", action="store_true")
    args = parser.parse_args()

    if args.command == "fetch":
        fetch()
    elif args.command == "update":
        update(args.branches, args.no_fetch, args.to)
    elif args.command == "status":
        status()
    else:
        tag(verify=not args.no_verify)


if __name__ == "__main__":
    main()
