#!/usr/bin/env python3
"""Fail when `ci-gate` does not wait on every other job in ci.yml (#637).

`ci-gate` is the one required check, and it is only as strong as its `needs`:
a job missing from that list runs, goes red, and the gate stays green. A
comment cannot keep the list complete; this check can.

It runs in `ci-gate` itself, because a check in a job the gate waits on is
dropped together with `needs` when someone deletes it. It reads the workflow
as text rather than through a YAML library, because that job installs
nothing beyond a checkout. The two shapes it reads
are the ones ci.yml uses: a job is a two-space-indented key directly under
`jobs:`, and the gate's `needs` is one flow-style list. Every two-space line
under `jobs:` must read as a job key (optionally quoted, optionally with a
comment), and anything else fails: a declaration the reader skipped would
fold that job into the previous one and hide it from the comparison.
"""

import re
import sys
from pathlib import Path

WORKFLOW = Path(__file__).resolve().parent.parent / ".github/workflows/ci.yml"
GATE = "gate"


JOB_KEY = re.compile(r"""  (["']?)([A-Za-z0-9_-]+)\1:\s*(#.*)?""")


def jobs(text):
    """Every job key, in order, with the lines that belong to it.

    Raises ValueError on a two-space line under `jobs:` that is not a job key.
    """
    found, current, inside = {}, None, False
    for line in text.splitlines():
        if line.startswith("jobs:"):
            inside = True
            continue
        if not inside:
            continue
        # A blank line may keep its indentation (spaces or a tab), and a
        # comment is no key; neither may end the section or declare a job.
        if not line.strip() or line.lstrip().startswith("#"):
            if current is not None:
                found[current].append(line)
            continue
        if not line.startswith(" "):
            break
        if line.startswith("  ") and not line.startswith("   "):
            key = JOB_KEY.fullmatch(line)
            if not key:
                raise ValueError(f"unrecognized job declaration: {line.strip()!r}")
            current = key.group(2)
            found[current] = []
        elif current is not None:
            found[current].append(line)
    return found


def gate_needs(lines):
    """The gate's `needs`, or None when it is not one flow-style list."""
    for line in lines:
        need = re.fullmatch(r"    needs:\s*\[([^\]]*)\]\s*", line)
        if need:
            return [n.strip() for n in need.group(1).split(",") if n.strip()]
    return None


def problems(text):
    try:
        found = jobs(text)
    except ValueError as error:
        return [str(error)]
    if GATE not in found:
        return [f"no `{GATE}` job"]
    needs = gate_needs(found[GATE])
    if needs is None:
        return [f"`{GATE}.needs` is not one `[a, b, ...]` list"]
    missing = [job for job in found if job != GATE and job not in needs]
    unknown = [job for job in needs if job not in found]
    return ([f"`{GATE}.needs` is missing job `{job}`" for job in missing]
            + [f"`{GATE}.needs` names no such job `{job}`" for job in unknown])


def main():
    errors = problems(WORKFLOW.read_text(encoding="utf-8"))
    for error in errors:
        print(f"::error file=.github/workflows/ci.yml::{error}", file=sys.stderr)
    if errors:
        return 1
    print(f"{GATE} waits on every job")
    return 0


if __name__ == "__main__":
    sys.exit(main())
