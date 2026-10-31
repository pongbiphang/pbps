#!/usr/bin/env python3
"""Decide whether a rebase left the pull request's own patch unchanged (DEC-2553.3).

Given the pair a full gate last passed on (`--old-base`, `--old-head`) and the
rebased pair (`--new-base`, `--new-head`), compare per file the multiset of the
patch's `+` and `-` lines, with no context (`git diff -U0`). Positions and blob
ids move in any rebase and are ignored; a line added, removed or rewritten by
the conflict resolution is not. A master line the resolution dropped shows as a
new `-` line.

The patch is unchanged only if every file's multiset is equal and neither diff
touches a manifest, the lockfile, the toolchain file, CI or a live runner or its
fixtures. Unknown is not unaffected (as in ci_release_scope.py): an unreadable
ref, a head that does not descend from its base, or a binary patch refuses.

Exit 0 only for `unchanged`. The JSON verdict carries a digest of the compared
multisets so a reviewer can rerun the script and compare one value.

Standard library and Git only.
"""

import argparse
from collections import Counter
import hashlib
import json
import re
import subprocess
import sys

from ci_release_scope import diff_paths

# Inputs the live gate depends on beyond the patch's own lines.
BLOCKED = tuple(re.compile(p) for p in (
    r"(^|/)Cargo\.(toml|lock)$",
    r"^rust-toolchain(\.toml)?$",
    r"^\.cargo/",
    r"^\.github/",
    r"^scripts/live[-_]",
    r"^scripts/check-live-fixture",
    r"^scripts/resolver-mssql-fixtures\.sh$",
    r"^scripts/fixtures/",
    r"^scripts/(gate_scope|known_flakes|known-flakes)",
))
# Header lines that belong to the change; `index`, `---`, `+++` and `@@` move.
HEADER = (b"old mode ", b"new mode ", b"new file mode ", b"deleted file mode ")


class Refused(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise Refused(message)


def git(repo, *args):
    result = subprocess.run(["git", *args], cwd=repo, capture_output=True)
    require(result.returncode == 0, f"git {args[0]} failed: {result.stderr.decode(errors='replace').strip()}")
    return result.stdout


def resolve(repo, ref):
    sha = git(repo, "rev-parse", "--verify", "--quiet", ref + "^{commit}").decode().strip()
    require(re.fullmatch(r"[0-9a-f]{40,64}", sha), f"{ref} is not a commit")
    return sha


def descends(repo, base, head):
    result = subprocess.run(["git", "merge-base", "--is-ancestor", base, head], cwd=repo, capture_output=True)
    require(result.returncode in (0, 1), f"cannot compare {base[:12]} with {head[:12]}")
    return result.returncode == 0


def patch_lines(repo, base, head, path):
    """The multiset of one file's patch lines: signed lines, modes, `\\ No newline`."""
    out = git(repo, "--literal-pathspecs", "diff", "--no-renames", "--no-ext-diff", "--no-textconv",
              "--no-color", "-U0", base, head, "--", path)
    lines, hunk = Counter(), False
    for line in out.split(b"\n"):
        require(not line.startswith(b"Binary files ") and line != b"GIT binary patch",
                f"{path} has a binary patch, which is not comparable")
        if line.startswith(b"@@"):
            hunk = True
        elif hunk and line[:1] in (b"+", b"-", b"\\"):
            lines[line] += 1
        elif not hunk and line.startswith(HEADER):
            lines[line] += 1
    return lines


def patch(repo, base, head):
    return {path: patch_lines(repo, base, head, path) for path in diff_paths(base, head, repo)}


def decide(old, new):
    """Return (unchanged, reason) for two {path: Counter} patches."""
    blocked = sorted(p for p in {*old, *new} if any(b.search(p) for b in BLOCKED))
    if blocked:
        return False, "touches " + ", ".join(blocked)
    differing = sorted(p for p in {*old, *new} if old.get(p) != new.get(p))
    if differing:
        return False, "the patch differs in " + ", ".join(differing)
    return True, f"{len(new)} files carry the same added and removed lines"


def digest(patches):
    body = {path: sorted([line.decode("utf-8", "surrogateescape"), n] for line, n in lines.items())
            for path, lines in sorted(patches.items())}
    return hashlib.sha256(json.dumps(body, sort_keys=True).encode()).hexdigest()


def verdict(repo, refs):
    old_base, old_head, new_base, new_head = (resolve(repo, ref) for ref in refs)
    result = {"old": [old_base, old_head], "new": [new_base, new_head]}
    require(descends(repo, old_base, old_head) and descends(repo, new_base, new_head),
            "a head does not descend from its base")
    old, new = patch(repo, old_base, old_head), patch(repo, new_base, new_head)
    unchanged, reason = decide(old, new)
    result.update(verdict="unchanged" if unchanged else "changed", reason=reason)
    if unchanged:
        result.update(files=len(new), added=sum(n for c in new.values() for l, n in c.items() if l[:1] == b"+"),
                      removed=sum(n for c in new.values() for l, n in c.items() if l[:1] == b"-"),
                      digest=digest(new))
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    for name in ("old-base", "old-head", "new-base", "new-head"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--repo", default=".")
    args = parser.parse_args(argv)
    try:
        result = verdict(args.repo, [args.old_base, args.old_head, args.new_base, args.new_head])
    except (Refused, OSError, subprocess.SubprocessError) as error:
        result = {"verdict": "refused", "reason": str(error)}
    print(json.dumps(result, sort_keys=True))
    return 0 if result["verdict"] == "unchanged" else 1


if __name__ == "__main__":
    sys.exit(main())
