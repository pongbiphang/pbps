#!/usr/bin/env python3
"""Delete each audited effect in turn and confirm a test notices (#2091).

The #1965 mutation audit found effects that could be deleted from
`crates/pbps-model/src/operation/effects.rs` with every test still green:
deleting an effect only makes the verifier more permissive. Each now has a
pin in `crates/pbps-diff/src/order/tests.rs`. This script deletes each effect
by an exact-text patch, runs the order tests, and restores the file.

Run from the repository root, with a clean `effects.rs`:

    python3 scripts/effect-mutations.py [--audit] [NAME ...]
    python3 scripts/effect-mutations.py --s7 [--output DIR] [NAME ...]

With `--audit` it runs the commutation suite's audit instead
(`PBPS_COMMUTE_ALL=1`, both engines, which need `PBPS_TEST_PG_DB` and
`PBPS_TEST_DB` as the live scripts set them): whether a scenario on the
engine holds the dependency each effect states.

The --s7 mode repeats the original M01–M20 sample and command matrix from
#1965 (including the five initially caught effects), with per-command logs,
source backup/checksums and shadow observations under target/s7-closure.

It first runs the same commands unmutated, and stops if they fail: a test
that fails for another reason would read as every mutation caught.

Exits 0 when every mutation fails a test (under `--audit`, every one but
those `UNSHOWABLE` lists, and none of those), 1 otherwise, and 2 when an
anchor is missing or the mutated crate does not compile: a patch that matches
nothing, or code that no longer builds, proves nothing about the tests.
S7 (#1959) re-runs it; an anchor that moved is updated here, never dropped.
"""
import argparse
import hashlib
import json
import os
import re
import time
import pathlib
import subprocess
import sys

EFFECTS = pathlib.Path("crates/pbps-model/src/operation/effects.rs")
TESTS = [["cargo", "test", "-p", "pbps-diff", "--lib", "order::tests"]]
AUDIT = [
    ["cargo", "test", "-p", crate, "--test", "live", "--", "--ignored", "--nocapture",
     "pairs_the_effects_call_independent_commute_on_the_engine"]
    for crate in ("pbps-pg", "pbps-mssql")
]

# (name, exact text, replacement). Each text must occur exactly once.
MUTATIONS = [
    ("CreateTable: Exists of the referenced table",
     "                        self.exists(ObjectKey::Table(referenced));\n",
     "                        let _ = referenced;\n"),
    ("RenameTable: Rewrites of the table",
     "                self.rewrites(Resource::Object(\n"
     "                    ObjectKey::Table(owner),\n"
     "                    Incarnation::Final,\n"
     "                ));\n",
     "                let _ = owner;\n"),
    ("AddColumn: Creates of the column's values",
     "                self.creates(Resource::Column(ObjectKey::Column(column.clone())));\n",
     ""),
    ("DropColumn: Absent of the column's dependents",
     "                self.requires_dependents_of_column_gone(&owner, column.name.raw());\n",
     ""),
    ("RenameColumn: Rewrites of the column",
     "                let key = ObjectKey::Column(ColumnKey::Recorded(Recorded(uid.clone())));\n"
     "                self.exists(key.clone());\n"
     "                self.rewrites(Resource::Object(key, Incarnation::Final));\n",
     "                let key = ObjectKey::Column(ColumnKey::Recorded(Recorded(uid.clone())));\n"
     "                self.exists(key.clone());\n"),
    ("AlterColumnType: Writes of the column's values",
     "                self.writes_values(ObjectKey::Column(key.clone()), &owner);\n"
     "                self.nullability_moves(&key, *from_nullable, *to_nullable);\n",
     "                self.nullability_moves(&key, *from_nullable, *to_nullable);\n"),
    ("AlterColumnDefault: Exists of the column",
     "                    // with.\n"
     "                    self.exists(column);\n",
     "                    // with.\n"
     "                    let _ = column;\n"),
    ("AddUnique: Reads of the rows",
     "            Change::AddUnique {\n"
     "                table, constraint, ..\n"
     "            } => {\n"
     "                let owner = self.table(table)?;\n"
     "                self.exists(ObjectKey::Table(owner.clone()));\n"
     "                self.requires_valid_data(&owner);\n",
     "            Change::AddUnique {\n"
     "                table, constraint, ..\n"
     "            } => {\n"
     "                let owner = self.table(table)?;\n"
     "                self.exists(ObjectKey::Table(owner.clone()));\n"),
    ("AddForeignKey: Is of the referenced key",
     "                    self.requires(Requirement::Is(\n"
     "                        Self::part(&referenced, part),\n"
     "                        Incarnation::Final,\n"
     "                    ));\n",
     "                    let _ = part;\n"),
    ("AddCheck: Reads of the rows",
     "            Change::AddCheck { table, .. } => {\n"
     "                let owner = self.table(table)?;\n"
     "                self.exists(ObjectKey::Table(owner.clone()));\n"
     "                self.requires_valid_data(&owner);\n",
     "            Change::AddCheck { table, .. } => {\n"
     "                let owner = self.table(table)?;\n"
     "                self.exists(ObjectKey::Table(owner.clone()));\n"),
    ("AddIndex: Exists of the indexed columns",
     "                for key in index.columns.iter().map(|c| &c.key) {\n"
     "                    if let IndexKey::Column(c) = key {\n"
     "                        self.requires_column(table, c.raw(), false);\n"
     "                    }\n"
     "                }\n",
     ""),
    ("DeleteRow: rows blocking the delete",
     "                self.row_inbound_keys_gone(&owner);\n",
     ""),
    ("AlterModule: Rewrites of a module edited in place",
     "                    let key = ObjectKey::Module(id.clone());\n"
     "                    self.exists(key.clone());\n"
     "                    self.rewrites(Resource::Object(key, Incarnation::Final));\n",
     "                    let key = ObjectKey::Module(id.clone());\n"
     "                    self.exists(key.clone());\n"),
    ("SetPrimaryKey: Absent of the foreign keys into the old key",
     "                    self.requires_key_unreferenced(&owner, &old.columns);\n",
     ""),
    ("DropComputedColumn: Absent of what reads it",
     "                    self.requires_computed_dependents_gone(&owner, name.raw());\n",
     "                    let _ = name;\n"),
]


# The effects no plan the differ makes lets an engine show, so the audit
# cannot catch their deletion; the unit pin is their guard. The reasons are
# in the comment above `HELD` in `crates/pbps-pg/tests/support/commutation.rs`.
# Under `--audit`, one of these surviving is expected, and one caught means
# the list is stale.
UNSHOWABLE = {
    "CreateTable: Exists of the referenced table",
    "AddColumn: Creates of the column's values",
    "AlterColumnType: Writes of the column's values",
    "AddUnique: Reads of the rows",
    "AddCheck: Reads of the rows",
    "SetPrimaryKey: Absent of the foreign keys into the old key",
}


def legacy_main(args):
    commands = AUDIT if "--audit" in args else TESTS
    selected = [a for a in args if a != "--audit"]
    env = dict(os.environ, PBPS_COMMUTE_ALL="1") if commands is AUDIT else None
    original = EFFECTS.read_text()
    chosen = [m for m in MUTATIONS if not selected or any(s in m[0] for s in selected)]
    # A name that matches nothing would run no mutation and report success.
    unmatched = [s for s in selected if not any(s in m[0] for m in MUTATIONS)]
    if unmatched:
        print(f"no mutation is named by {unmatched}")
        return 2
    for name, text, _ in chosen:
        if original.count(text) != 1:
            print(f"anchor for {name!r} occurs {original.count(text)} times")
            return 2
    # The same commands on the file as it stands first: a failure here is the
    # environment's or the build's (a database not started, a connection
    # variable unset), and would read as every mutation caught.
    baseline = [subprocess.run(c, capture_output=True, text=True, env=env)
                for c in commands]
    if any(test_status(o.returncode, o.stdout + o.stderr) != "passed" for o in baseline):
        print("the unmutated run fails; nothing below would mean anything:")
        print("".join(o.stdout + o.stderr for o in baseline)[-4000:])
        return 2
    if commands is AUDIT and not all("commutation audit" in o.stderr + o.stdout
                                     for o in baseline):
        print("the unmutated run did not audit: is PBPS_COMMUTE_ALL read?")
        return 2
    survived, broken = [], []
    try:
        for name, text, replacement in chosen:
            EFFECTS.write_text(original.replace(text, replacement))
            outs = [subprocess.run(c, capture_output=True, text=True, env=env)
                    for c in commands]
            log = "".join(o.stdout + o.stderr for o in outs)
            expected = commands is AUDIT and name in UNSHOWABLE
            statuses = [test_status(o.returncode, o.stdout + o.stderr) for o in outs]
            if "invalid" in statuses:
                print(f"no build/empty run  {name}")
                broken.append(name)
            elif "failed" in statuses:
                print(f"caught    {name}")
                if expected:
                    print(f"          (listed as unshowable: update UNSHOWABLE)")
                    survived.append(name)
            elif expected:
                print(f"unshown   {name}")
            else:
                print(f"SURVIVED  {name}")
                survived.append(name)
    finally:
        EFFECTS.write_text(original)
    if broken:
        return 2
    return 1 if survived else 0


# Keep the original M01–M20 order, including the five effects already caught
# in #1965. The default/--audit modes retain #2091's narrower sample.
EXTRA = {
    "DropTable": ("DropTable: Absent of inbound foreign keys",
                  "                self.requires_inbound_keys_gone(&owner);\n", ""),
    "AlterColumnNullability": (
        "AlterColumnNullability: the NOT NULL produced or ended",
        "                self.nullability_moves(&key, !*to_nullable, *to_nullable);\n", ""),
    "InsertRow": ("InsertRow: the columns the row writes",
                  "                written.extend(defaults.keys().map(ColumnName::raw));\n"
                  "                self.row_writes(table, &owner, &written);\n"
                  "                // The statement's postcondition",
                  "                written.extend(defaults.keys().map(ColumnName::raw));\n"
                  "                // The statement's postcondition"),
    "Grant": ("Grant: every grant effect",
              "            Change::Grant { role, target, .. } => self.permission(role, target, true)?,\n",
              "            Change::Grant { .. } => {},\n"),
    "PublicExecution": (
        "PublicExecution: Is of the routine",
        "            Change::PublicExecution { routine, .. } => {\n"
        "                self.requires(Requirement::Is(\n"
        "                    ObjectKey::Module(crate::ModuleId::Routine(routine.clone())),\n"
        "                    Incarnation::Final,\n"
        "                ));\n",
        "            Change::PublicExecution { .. } => {\n"),
}
ORIGINAL_KINDS = (
    "CreateTable", "DropTable", "RenameTable", "AddColumn", "DropColumn",
    "RenameColumn", "AlterColumnType", "AlterColumnNullability", "AlterColumnDefault",
    "AddUnique", "AddForeignKey", "AddCheck", "AddIndex", "InsertRow", "DeleteRow",
    "AlterModule", "Grant", "SetPrimaryKey", "DropComputedColumn", "PublicExecution",
)
S7_MUTATIONS = [dict((m[0].split(":")[0], m) for m in MUTATIONS).get(k, EXTRA.get(k))
                for k in ORIGINAL_KINDS]
S7_COMMANDS = [
    ["cargo", "test", "-p", "pbps-model", "--lib"],
    ["cargo", "test", "-p", "pbps-diff", "--lib"],
    *[["cargo", "test", "-p", crate, "--test", "engine_effects"]
      for crate in ("pbps-pg", "pbps-mssql")],
    *AUDIT,
]


def test_status(code, log):
    """A compiler/endpoint/empty run must not masquerade as a killed mutation."""
    totals = re.findall(r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed", log)
    if not totals or not any(int(a) + int(b) for a, b in totals):
        return "invalid"
    if code == 0 and all(int(b) == 0 for _, b in totals):
        return "passed"
    if code != 0 and any(int(b) for _, b in totals):
        return "failed"
    return "invalid"


def run_case(directory, commands, env):
    directory.mkdir(parents=True, exist_ok=False)
    records = []
    for i, command in enumerate(commands, 1):
        started = time.monotonic()
        output = subprocess.run(command, capture_output=True, text=True, env=env)
        log = output.stdout + output.stderr
        (directory / f"{i:02}.log").write_text(log)
        records.append({"command": command, "returncode": output.returncode,
                        "status": test_status(output.returncode, log),
                        "seconds": time.monotonic() - started,
                        "failed_tests": re.findall(r"^test (.+) \.{3} FAILED$", log, re.M)})
    return records


def s7_main(args):
    parser = argparse.ArgumentParser(description="Repeat #1965's original 20 mutations")
    parser.add_argument("--output", type=pathlib.Path, default=pathlib.Path("target/s7-closure/mutations"))
    parser.add_argument("names", nargs="*")
    options = parser.parse_args(args)
    original = EFFECTS.read_bytes()
    text = original.decode()
    chosen = [(f"M{i:02}", m) for i, m in enumerate(S7_MUTATIONS, 1)
              if not options.names or any(n in m[0] for n in options.names)]
    if not chosen or any(not any(n in m[0] for m in S7_MUTATIONS) for n in options.names):
        print("no mutation matches a requested name")
        return 2
    for _, (name, anchor, _) in chosen:
        if text.count(anchor) != 1:
            print(f"anchor for {name!r} occurs {text.count(anchor)} times")
            return 2
    for key in ("PBPS_TEST_PG_DB", "PBPS_TEST_DB"):
        if not os.environ.get(key):
            print(f"{key} is required; no audit was run")
            return 2
    out = options.output
    out.mkdir(parents=True, exist_ok=False)
    (out / "original-effects.rs").write_bytes(original)
    env = dict(os.environ)
    # The original report used the capped suites, not #2091's all-pairs mode.
    env.pop("PBPS_COMMUTE_ALL", None)
    report = {"sha": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
              "source_sha256": hashlib.sha256(original).hexdigest(), "mutations": []}
    def persist():
        (out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    try:
        env["PBPS_ORDER_SHADOW"] = str((out / "baseline-shadow.log").resolve())
        report["baseline"] = run_case(out / "baseline", S7_COMMANDS, env)
        persist()
        if any(r["status"] != "passed" for r in report["baseline"]):
            print("unmutated baseline failed; see", out)
            return 2
        print("baseline passed", flush=True)
        for identity, (name, anchor, replacement) in chosen:
            EFFECTS.write_text(text.replace(anchor, replacement))
            env["PBPS_ORDER_SHADOW"] = str((out / f"{identity}-shadow.log").resolve())
            runs = run_case(out / identity, S7_COMMANDS, env)
            status = ("invalid" if any(r["status"] == "invalid" for r in runs) else
                      "killed" if any(r["status"] == "failed" for r in runs) else "survived")
            report["mutations"].append({"id": identity, "effect": name, "status": status,
                                        "live_unshowable": name in UNSHOWABLE, "runs": runs})
            persist()
            print(identity, status, name, flush=True)
            EFFECTS.write_bytes(original)
    finally:
        EFFECTS.write_bytes(original)
        report["restored_sha256"] = hashlib.sha256(EFFECTS.read_bytes()).hexdigest()
        persist()
    if any(m["status"] == "invalid" for m in report["mutations"]):
        return 2
    return int(any(m["status"] == "survived" for m in report["mutations"]))


def main(args):
    if "--s7" in args:
        return s7_main([a for a in args if a != "--s7"])
    return legacy_main(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
