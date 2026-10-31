#!/usr/bin/env python3
"""The register of known flaky live tests (DEC-2553.2).

`scripts/known-flakes.json` names each flake by its exact libtest name and the
cargo invocation prefix that runs it, the open issue that records it and an
expiry. `scripts/live-shards.py --resume` may complete a gate that failed only
on registered, unexpired entries (DEC-2553.1); the register never skips or
selects a test.

This check validates the file and then compares it with the compiled test
inventory. Always refused: an entry that cites no issue, a name or invocation
that no longer exists, a duplicate, an unknown field, and an expiry more than
30 days out (an entry is renewed by editing it, never made permanent).

An expired entry is already inactive at resume time, so it does not fail every
pull request that happens to run after its date. It is refused only when this
change touches the register, and in the nightly run on `master`: strict mode.
`--strict-expiry` forces it; otherwise CI's `EVENT` and `BASE_SHA` decide, and an
unreadable diff is strict, as unknown is not unaffected.

Standard library and the repository's test inventory only; `live-shards.py` imports
this module for `load` and `active` alone.
"""

import datetime
import json
import os
from pathlib import Path
import re
import subprocess
import sys


REGISTER = "scripts/known-flakes.json"
MAX_DAYS = 30
FIELDS = {"test", "cargo", "issue", "expires"}


class RegisterError(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise RegisterError(message)


def expiry(entry):
    return datetime.date.fromisoformat(entry["expires"])


def load(text):
    """Validate the register's schema and return its entries."""
    try:
        data = json.loads(text)
    except ValueError as error:
        raise RegisterError(f"{REGISTER} is not valid JSON: {error}") from error
    require(isinstance(data, dict) and set(data) == {"version", "flakes"} and data["version"] == 1
            and isinstance(data["flakes"], list), f"{REGISTER} must be {{version: 1, flakes: [...]}}")
    seen = set()
    for index, entry in enumerate(data["flakes"]):
        label = f"entry {index}"
        require(isinstance(entry, dict), f"{label} is not an object")
        label = f"entry {index} ({entry.get('test')})"
        require(set(entry) == FIELDS, f"{label} needs exactly {sorted(FIELDS)}; missing "
                f"{sorted(FIELDS - set(entry))}, unknown {sorted(set(entry) - FIELDS)}")
        require(type(entry["issue"]) is int and entry["issue"] > 0, f"{label} cites no open issue")
        require(isinstance(entry["test"], str) and entry["test"], f"{label} has no test name")
        cargo = entry["cargo"]
        require(isinstance(cargo, list) and all(isinstance(a, str) and a for a in cargo)
                and cargo[:1] == ["test"] and cargo[-1:] == ["--"],
                f"{label}: cargo must be the invocation prefix from `test` to `--`")
        require(isinstance(entry["expires"], str) and re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}", entry["expires"]),
                f"{label}: expires must be YYYY-MM-DD")
        try:
            expiry(entry)
        except ValueError as error:
            raise RegisterError(f"{label}: expires is not a date") from error
        identity = (tuple(cargo), entry["test"])
        require(identity not in seen, f"{label} is registered twice")
        seen.add(identity)
    return data["flakes"]


def active(entries, today):
    """Unexpired entries keyed by (cargo prefix, test name); expired ones waive nothing."""
    return {(tuple(e["cargo"]), e["test"]): e for e in entries if expiry(e) >= today}


def check(entries, today, targets, strict):
    """Raise RegisterError unless every entry is valid against the compiled inventory."""
    import ignored_test_inventory as inventory  # the resume path needs only load and active
    for entry in entries:
        label = f"{entry['test']} (#{entry['issue']})"
        require(expiry(entry) <= today + datetime.timedelta(days=MAX_DAYS),
                f"{label} expires {entry['expires']}, more than {MAX_DAYS} days out")
        require(not strict or expiry(entry) >= today,
                f"{label} expired {entry['expires']}: remove it or renew it with a new date")
        try:
            selection = inventory.cargo_selection(["cargo", *entry["cargo"]])
        except inventory.InventoryError as error:
            raise RegisterError(f"{label}: {error}") from error
        require(selection is not None and selection[0] in targets,
                f"{label}: no test target for {' '.join(entry['cargo'])}")
        require(entry["test"] in targets[selection[0]]["all"],
                f"{label} no longer exists in {'::'.join(selection[0])}")


def strict_expiry(event, base, forced, changed):
    """Return (strict, reason). `changed(base)` lists the paths this change touches or raises."""
    if forced:
        return True, "--strict-expiry"
    if event == "schedule":
        return True, "the nightly run"
    if not base:
        return False, "no change scope here"
    try:
        paths = changed(base)
    except Exception as error:  # an unreadable diff must not read as "register untouched"
        return True, f"the diff against {base[:12]} could not be read ({error})"
    return (REGISTER in paths), (f"this change touches {REGISTER}" if REGISTER in paths
                                 else f"this change leaves {REGISTER} alone")


def changed_since(base):
    """Paths this checkout changes against `base`, fetching the base only if it is missing.

    CI's checkout is shallow and lacks the base; a full local clone has it, and a
    `--depth` fetch there would make the shared repository shallow.
    """
    import ci_release_scope
    if subprocess.run(["git", "cat-file", "-e", base + "^{commit}"], capture_output=True).returncode:
        shallow = subprocess.run(["git", "rev-parse", "--is-shallow-repository"],
                                 capture_output=True, text=True, check=True).stdout.strip() == "true"
        subprocess.run(["git", "fetch", "--quiet", "--no-tags", *(["--depth=1"] if shallow else []), "origin", base],
                       check=True)
    return ci_release_scope.diff_paths(base, "HEAD")


def main():
    import ignored_test_inventory as inventory
    root = Path(__file__).resolve().parents[1]
    today = datetime.datetime.now(datetime.timezone.utc).date()
    strict, why = strict_expiry(os.environ.get("EVENT", ""), os.environ.get("BASE_SHA", ""),
                                "--strict-expiry" in sys.argv[1:],
                                changed_since)
    entries = load((root / REGISTER).read_text(encoding="utf-8"))
    check(entries, today, inventory.discover(root), strict)
    print(f"Known-flake register verified ({len(entries)} entries; expiry strict: {strict}, {why}).")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError) as error:
        print(f"known flakes: {error}", file=sys.stderr)
        sys.exit(1)
