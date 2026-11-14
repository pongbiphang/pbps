#!/usr/bin/env python3
"""Decide how much of the local gate a head needs (#2859, DEC-2859.1).

AGENTS.md's full gate runs both live suites. A head that changes only files no
live run reads cannot change their result, so it takes the `non-live` gate:
the Python checks, `fmt`, `clippy` and
`cargo test --workspace --all-targets --no-fail-fast`. Every other head takes
`full`. The merge group runs every job either way, so this saves local time and
never the final proof.

The rule is an allow-list. A head is `non-live` only if every path of
`git diff <merge-base> <head>` is in a class named in FREE. A path in no class
is `full`: unknown is not unaffected (as in ci_release_scope.py). So are an
unreadable ref, a head with no merge base, an empty diff and a directory that
is not a repository. A symlink or a submodule (mode 120000 or 160000, before or
after) is `full` too: what it points at is not its name. Exit 0 only for
`non-live`.

KEEP_FULL lists the files under `scripts/` that a live run reads, executes,
sources, copies or imports, beside code and build input. It is checked first,
because the `scripts/` class is otherwise "everything not named here". The
drift test in local_gate_scope_test.py walks the live runners and fails when
they name a `scripts/` file this list does not cover, so a new helper cannot
slip in.

Standard library and Git only.
"""

import argparse
import json
import os
import re
import subprocess
import sys

from ci_release_scope import diff_paths

# Paths that keep the full gate. Under `scripts/` they are the files a live run
# reads, executes, sources, copies or imports, measured from the two live
# runners, the sharded driver and the Rust tests they start (DEC-2859.1). Tests
# of those scripts (`*_test.py`) are Python checks, not inputs. The rest is code
# and build input, named only so the reason says why.
KEEP_FULL = tuple((re.compile(p), why) for p, why in (
    (r"scripts/live[-_](?![^/]*_test\.)[^/]*", "a live runner or its driver"),
    (r"scripts/check-live-fixture[^/]*", "fixture admission, run by both live runners"),
    (r"scripts/(resolver-mssql-fixtures|install-pg-partman)\.sh", "sourced or copied into a live fixture"),
    (r"scripts/compatibility-(matrix\.json|tests\.sh)", "a compatibility cell: a live test includes the matrix"),
    (r"scripts/fixtures/.*", "copied into the live containers"),
    (r"scripts/(gate_rerun|known_flakes)\.py", "imported by the sharded driver"),
    (r"scripts/(local_gate_scope|gate_scope|ci_release_scope)\.py", "a scope rule, which its own change must not judge"),
    (r"schemas/.*", "the envelope schema a live flow test reads"),
    (r"docs/measurements/.*", "compiled into a test binary with include_str!"),
    (r"crates/.*", "crate source and its test fixtures"),
    (r"(.*/)?Cargo\.(toml|lock)", "a manifest or lockfile"),
    (r"rust-toolchain(\.toml)?|\.cargo/.*", "toolchain or Cargo configuration"),
))
# The classes no live run reads. `scripts/` is a class of its own: a top-level
# file there that KEEP_FULL does not name (a Python check, a CI helper, a test).
FREE = tuple((re.compile(p), name) for p, name in (
    (r"docs/.*|(README|AGENTS|CLAUDE)\.md|LICENSE", "documentation"),
    (r"\.github/.*", "workflow"),
    (r"scripts/[^/]+", "script outside the live closure"),
))


def classify(path):
    """Return (free, label): the class of a free path, or why a path is not free."""
    for pattern, why in KEEP_FULL:
        if pattern.fullmatch(path):
            return False, why
    for pattern, name in FREE:
        if pattern.fullmatch(path):
            return True, name
    return False, "in no class the rule names as unread by a live run"


def decide(paths, indirect=()):
    """Return (verdict, reason, classes) for the changed paths.

    `indirect` names the paths that are a symlink or a submodule on either side:
    what they point at is not their name, so no class can vouch for them."""
    paths = sorted(set(paths))
    if not paths:
        return "full", "the diff lists no path, which is not evidence that nothing is read", {}
    held, classes = {}, {}
    for path in paths:
        free, label = classify(path)
        if path in indirect:
            free, label = False, "a symlink or submodule, which points somewhere its name does not say"
        (classes if free else held).setdefault(label, []).append(path)
    if held:
        return "full", "; ".join(
            f"{label}: {', '.join(found[:3])}" + (f" and {len(found) - 3} more" if len(found) > 3 else "")
            for label, found in sorted(held.items())), {}
    return ("non-live", f"{len(paths)} changed paths, all in classes no live run reads",
            {label: len(found) for label, found in sorted(classes.items())})


def git(repo, *args):
    result = subprocess.run(["git", *args], cwd=repo, capture_output=True)
    if result.returncode != 0:
        raise ValueError(f"git {args[0]} failed: {result.stderr.decode(errors='replace').strip() or 'exit ' + str(result.returncode)}")
    return result.stdout.decode().strip()


def indirect_paths(repo, base, head):
    """Paths whose mode is a symlink (120000) or a gitlink (160000) before or after."""
    out = subprocess.run(["git", "diff", "--raw", "--no-renames", "-z", base, head],
                         check=True, capture_output=True, cwd=repo).stdout.split(b"\0")
    return {os.fsdecode(path) for meta, path in zip(out[0::2], out[1::2])
            if {meta.split()[0][1:], meta.split()[1]} & {b"120000", b"160000"}}


def verdict(repo, base, head):
    """The JSON-able verdict for `head` against the merge base with `base`."""
    result = {"base": base, "head": head}
    try:
        base_sha = git(repo, "rev-parse", "--verify", "--quiet", base + "^{commit}")
        head_sha = git(repo, "rev-parse", "--verify", "--quiet", head + "^{commit}")
        merge_base = git(repo, "merge-base", base_sha, head_sha)
        result.update(head=head_sha, merge_base=merge_base)
        scope, reason, classes = decide(diff_paths(merge_base, head_sha, repo),
                                        indirect_paths(repo, merge_base, head_sha))
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        scope, reason, classes = "full", f"unknown is not unaffected: {error}", {}
    result.update(verdict=scope, reason=reason)
    if classes:
        result["classes"] = classes
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base", default="origin/master", help="the ref the pull request will merge into")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument("--repo", default=".")
    args = parser.parse_args(argv)
    result = verdict(args.repo, args.base, args.head)
    print(json.dumps(result, sort_keys=True))
    return 0 if result["verdict"] == "non-live" else 1


if __name__ == "__main__":
    sys.exit(main())
