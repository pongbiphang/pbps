#!/usr/bin/env python3
"""Turn `pbps <command> --format json` into GitHub Actions annotations.

    pbps validate --format json | scripts/findings-to-github.py

# Why this is a script and not a flag

Vendor annotation formats change on someone else's schedule, and every one that
enters the binary has to be kept working by this project forever — including for
users who will never run that CI system. Reading the envelope from outside costs
one file per vendor, breaks a script rather than a release when the format
moves, and can be copied and edited by a team whose CI is neither of the two
anyone thought of (SPEC 14.1).

# Why it exits with the same code

The pipeline step must still fail on a finding. Swallowing the exit code to
print annotations would produce a green build with red squiggles on it, which is
worse than either alone.

The code comes from the envelope's `result`, which carries the same three-way
split as pbps's own exit codes — a pipe loses the producer's status, and
re-deriving "could not answer" from the findings would put the routing rule in
two places. Deriving it here once turned `doctor`'s exit 1 into a 2, which sends
an unreachable database to whoever wrote the schema change.
"""

import json
import sys

# Annotation levels GitHub understands. `note` exists but is rendered so quietly
# that a warning is the honest floor for anything we bothered to report.
LEVEL = {"error": "error", "warning": "warning", "note": "notice"}


def escape(value: str) -> str:
    """GitHub's own escaping for annotation *properties* (not the message)."""
    return value.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def main() -> int:
    raw = sys.stdin.read()
    if not raw.strip():
        print("::error::pbps produced no output to annotate", file=sys.stderr)
        return 1
    try:
        report = json.loads(raw)
    except json.JSONDecodeError as e:
        # Passing the text through rather than swallowing it: the most likely
        # cause is a command that failed before it could produce an envelope,
        # and its own message is the useful thing here.
        print(f"::error::pbps did not produce a findings report ({e})", file=sys.stderr)
        print(raw, file=sys.stderr)
        return 1

    schema_version = report.get("schema_version")
    if schema_version != 1:
        # Refusing rather than guessing: a converter that silently drops fields
        # it does not recognise turns a new kind of finding into no finding.
        print(
            f"::error::this converter reads findings schema 1, not {schema_version}; "
            "update scripts/findings-to-github.py",
            file=sys.stderr,
        )
        return 1

    # ok -> 0, findings -> 2, unanswerable -> 1: the same split pbps itself uses
    # (SPEC 9.8). An unrecognised value is treated as a finding rather than as
    # success, because a green build is the one wrong answer that goes unnoticed.
    EXIT = {"ok": 0, "findings": 2, "unanswerable": 1}

    findings = report.get("findings", [])
    for f in findings:
        level = LEVEL.get(f.get("severity", "error"), "error")
        props = [f"title=pbps {escape(f.get('id', ''))}"]
        location = f.get("location")
        if location:
            props.append(f"file={escape(location['file'])}")
            if location.get("line"):
                props.append(f"line={location['line']}")
        message = f.get("message", "")
        if f.get("remedy"):
            message += f"\n\n{f['remedy']}"
        print(f"::{level} {','.join(props)}::{escape(message)}")

    command = report.get("command", "pbps")
    result = report.get("result", "findings")
    code = EXIT.get(result, 2)
    if code == 1:
        print(f"::error::{command}: could not complete; see the annotations above")
    elif code != 0:
        print(f"::error::{command}: {len(findings)} finding(s)")
    return code


if __name__ == "__main__":
    sys.exit(main())
