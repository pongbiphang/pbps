#!/usr/bin/env python3
"""Decide `ci-gate` from the results of the jobs it waits on (#1835).

A pull request runs a reduced job set; the merge group, `master`, a scheduled
and a dispatched run run every job (DEC-1835.1). The jobs a pull request omits
are named once, in the gate step's `PR_OMITTED`, and each carries
`if: ${{ github.event_name != 'pull_request' }}`; the CI policy test holds the
two together.

`skipped` still never passes by default (DECISIONS 206): it is accepted only
for a job on that list, and only on a `pull_request` run. There a listed job
that reports anything but `skipped` fails the gate too, because its condition
has drifted and the run is no longer the set this file promises. Every other
job, on every event, must be `success`.

Standard library only: `ci-gate` installs nothing beyond a checkout.
"""

import json
import os
import sys

# Every event ci.yml is triggered by. Anything else fails rather than
# defaulting to either verdict.
EVENTS = {"pull_request", "merge_group", "push", "schedule", "workflow_dispatch"}

# The fan-in every job waits on. Omitting either would let a pull request skip
# approval or compilation and still pass.
NEVER_OMITTED = {"approve", "lint"}


def verdict(event, needs, omitted):
    """Return the list of reasons the gate fails; empty means it passes."""
    if event not in EVENTS:
        return [f"unknown event `{event}`: no verdict is defined for it"]
    if not needs:
        return ["the gate waits on no job"]
    errors = []
    for name in sorted(omitted & NEVER_OMITTED):
        errors.append(f"{name}: may never be omitted from a pull request")
    for name in sorted(omitted - needs.keys()):
        errors.append(f"{name}: listed in PR_OMITTED but not a job the gate waits on")
    for name, entry in needs.items():
        result = entry.get("result") if isinstance(entry, dict) else None
        if event == "pull_request" and name in omitted:
            if result != "skipped":
                errors.append(f"{name}: {result} (omitted on a pull request, so it must be skipped)")
        elif result != "success":
            errors.append(f"{name}: {result}")
    return errors


def main():
    event = os.environ.get("EVENT", "")
    omitted = set(os.environ.get("PR_OMITTED", "").split())
    try:
        needs = json.loads(os.environ.get("NEEDS", ""))
    except json.JSONDecodeError as error:
        print(f"::error::NEEDS is not JSON: {error}")
        return 1
    if not isinstance(needs, dict):
        print("::error::NEEDS is not an object")
        return 1
    print(json.dumps(needs, indent=2, sort_keys=True))
    errors = verdict(event, needs, omitted)
    for error in errors:
        print(f"::error::{error}")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
