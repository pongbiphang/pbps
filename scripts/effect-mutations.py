#!/usr/bin/env python3
"""Delete each audited effect in turn and confirm a test notices (#2091).

The #1965 mutation audit found effects that could be deleted from
`crates/pbps-model/src/operation/effects.rs` with every test still green:
deleting an effect only makes the verifier more permissive. Each now has a
pin in `crates/pbps-diff/src/order/tests.rs`. This script deletes each effect
by an exact-text patch, runs the order tests, and restores the file.

Run from the repository root, with a clean `effects.rs`:

    python3 scripts/effect-mutations.py [--audit] [NAME ...]

With `--audit` it runs the commutation suite's audit instead
(`PBPS_COMMUTE_ALL=1`, both engines, which need `PBPS_TEST_PG_DB` and
`PBPS_TEST_DB` as the live scripts set them): whether a scenario on the
engine holds the dependency each effect states.

It first runs the same commands unmutated, and stops if they fail: a test
that fails for another reason would read as every mutation caught.

Exits 0 when every mutation fails a test (under `--audit`, every one but
those `UNSHOWABLE` lists, and none of those), 1 otherwise, and 2 when an
anchor is missing or the mutated crate does not compile: a patch that matches
nothing, or code that no longer builds, proves nothing about the tests.
S7 (#1959) re-runs it; an anchor that moved is updated here, never dropped.
"""
import os
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
     "                self.requires_dependents_of_column_gone(&owner, &column.name);\n",
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
     "                        self.requires_column(table, c, false);\n"
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
     "                    self.requires_computed_dependents_gone(&owner, name);\n",
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


def main(args):
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
    if any(o.returncode != 0 for o in baseline):
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
            if "test result: FAILED" in log:
                print(f"caught    {name}")
                if expected:
                    print(f"          (listed as unshowable: update UNSHOWABLE)")
                    survived.append(name)
            elif any(o.returncode != 0 for o in outs):
                print(f"no build  {name}")
                broken.append(name)
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


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
