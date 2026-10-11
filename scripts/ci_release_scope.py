#!/usr/bin/env python3
"""Decide whether `release-windows` runs its full qualification (#1834).

DEC-1133.1's checks (the static MSVC build, its imports, and the native TLS
and CLI cases against both engines) depend on a small set of inputs: the
dependency graph and build configuration, the driver and TLS code in
`crates/pbps-db`, and the qualification itself. A change touching none of them
cannot change the outcome, so the job reports success without building
(DEC-1834.1).

The job never skips: `ci-gate` treats a skipped job as a failure. Only its steps
after this one are conditioned on the `run` output written here.

When to run in full:
- `schedule` and `workflow_dispatch` always run. The nightly run is the
  backstop (DEC-1836.1).
- `merge_group` runs when `merge_group.base_sha..checkout` touches a trigger
  path. That covers this pull request and everything queued ahead of it.
- Any other event runs. A pull request never reaches this job (DEC-1835.1),
  but if one did, it would get the full qualification, not a skip.
- Unknown is not unaffected: a missing base, an unreadable diff or an unknown
  event runs in full rather than skipping.

Standard library and Git only.
"""

import os
import re
import subprocess
import sys

# The inputs DEC-1133.1's checks depend on. A crate that builds a driver or TLS
# configuration belongs here; today that is `crates/pbps-db` alone (`rustls`,
# `tiberius::Config` and `tokio_postgres::Config` appear nowhere else outside
# tests).
TRIGGERS = (
    re.compile(r"Cargo\.(toml|lock)"),
    re.compile(r"crates/[^/]+/Cargo\.toml"),
    re.compile(r"rust-toolchain(\.toml)?"),
    re.compile(r"\.cargo/config(\.toml)?"),
    re.compile(r"crates/pbps-db/.+"),
    re.compile(r"scripts/qualify-release\.py"),
    re.compile(r"scripts/release_qualification_test\.py"),
    re.compile(r"scripts/release-consumer-windows\.Dockerfile"),
    re.compile(r"scripts/release-windows-[^/]+\.ps1"),
    re.compile(r"scripts/ci_release_scope\.py"),
    re.compile(r"\.github/workflows/ci\.yml"),
)

ALWAYS = {"schedule", "workflow_dispatch"}
SHA = re.compile(r"[0-9a-f]{40}")


def triggered(paths):
    """The changed paths that are inputs of the qualification, sorted."""
    return sorted(p for p in paths if any(t.fullmatch(p) for t in TRIGGERS))


def decide(event, base, head, changed):
    """Return (run, reason). `changed(base, head)` lists paths or raises."""
    if event in ALWAYS:
        return True, f"`{event}` always runs the full qualification"
    if event != "merge_group":
        return True, f"`{event}` has no change scope here, so it runs in full"
    if not (base and SHA.fullmatch(base) and head and SHA.fullmatch(head)):
        return True, "the merge group's base or head is unknown, so it runs in full"
    try:
        paths = changed(base, head)
    except (OSError, subprocess.CalledProcessError) as error:
        return True, f"the diff {base[:12]}..{head[:12]} could not be read ({error}), so it runs in full"
    hits = triggered(paths)
    if hits:
        return True, f"{base[:12]}..{head[:12]} changes {', '.join(hits)}"
    return False, f"{base[:12]}..{head[:12]} changes no input of the Windows release qualification"


def diff_paths(base, head, cwd=None):
    """Every path on either side of the diff: a rename lists both names, so a
    file moved out of the trigger set still counts as a change to it."""
    out = subprocess.run(["git", "diff", "--no-renames", "--name-only", base, head],
                         check=True, capture_output=True, text=True, cwd=cwd).stdout
    return [line for line in out.splitlines() if line]


def git_changed(base, head):
    # The checkout is shallow: fetch the base so the diff compares two trees
    # instead of failing, which would only force a full run.
    subprocess.run(["git", "fetch", "--quiet", "--no-tags", "--depth=1", "origin", base], check=True)
    return diff_paths(base, head)


def main():
    run, reason = decide(os.environ.get("EVENT", ""), os.environ.get("BASE_SHA", ""),
                         os.environ.get("HEAD_SHA", ""), git_changed)
    verdict = "full qualification" if run else "skipped: no trigger path changed"
    print(f"release-windows: {verdict} ({reason})")
    for name, text in (("GITHUB_OUTPUT", f"run={'true' if run else 'false'}\n"),
                       ("GITHUB_STEP_SUMMARY", f"**release-windows:** {verdict}. {reason}.\n")):
        path = os.environ.get(name)
        if path:
            # Bytes, not text: on the Windows runner text mode would write
            # `run=true\r\n`, the output would read `true\r`, and every step
            # conditioned on it would skip.
            with open(path, "ab") as handle:
                handle.write(text.encode("utf-8"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
