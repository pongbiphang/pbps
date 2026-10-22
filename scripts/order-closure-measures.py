#!/usr/bin/env python3
"""Reproduce S7's bounded ordering maintenance inventory, without live engines.

This is an audited checklist, not a static analyzer or compiler-enforcement
fallback. Counts are distinct file:symbol decisions to revisit; conditional
rows are not a claim that every new kind edits every listed function. Common
model/emitter/risk work is separate from ordering-specific work. Git objects,
not the working tree, supply both snapshots, so results name immutable SHAs.
"""
import argparse
import json
from pathlib import Path
import subprocess


def site(path, symbol, anchor, when):
    return dict(path=path, symbol=symbol, anchor=anchor, when=when)


DIFF = "crates/pbps-diff/src/"
CLI = "crates/pbps-cli/src/"
MODEL = "crates/pbps-model/src/"
BEFORE = [
    site(DIFF+"schema_diff.rs", "order_key", "fn order_key(", "new kind: base class"),
    site(DIFF+"schema_diff.rs", "sort_class", "let sort_class =", "within-class policy, if relevant"),
    site(DIFF+"schema_diff.rs", "dependency_rank", "fn dependency_rank(", "content-dependent base rank, if relevant"),
    site(DIFF+"schema_diff.rs", "order_role_drops", "pub fn order_role_drops(", "role membership rule"),
    *[site(CLI+"dependents.rs", name, "fn "+name+"(", "catalog/lexical rule, if relevant")
      for name in ("weave", "split_new_tables", "before_its_partitions_indexes",
                   "after_their_parents_defaults", "after_the_rebuilds", "after_their_functions")],
    site(CLI+"engine.rs", "order_after_releases", "pub(crate) fn order_after_releases(", "connected release rule"),
    site(CLI+"deploy.rs", "order_carried_moves", "pub(crate) fn order_carried_moves(", "catalog name handover"),
    site(CLI+"computed_order.rs", "order_by_edges", "pub(crate) fn order_by_edges(", "SQL Server computed release rule"),
    site(CLI+"object_order.rs", "order_occupied_objects_under", "fn order_occupied_objects_under(", "SQL Server name search"),
    site(DIFF+"resolver/graph.rs", "constraints", "fn constraints(", "resolver dependency rule"),
    site("crates/pbps-pg/src/resolver/reconstruct.rs", "after_their_functions", "fn after_their_functions(", "scratch function dependency rule"),
]
# Mirror every old responsibility before counting the new declarative layer.
# Omitting retained compatibility/search sites would manufacture a reduction.
AFTER = [
    *BEFORE[:3],
    site(DIFF+"order/roles.rs", "constraints", "fn constraints(", "role membership/depth compatibility rule"),
    site(DIFF+"order/deployment.rs", "weave", "pub fn weave(", "catalog expansion obligation"),
    site(DIFF+"order/new_tables.rs", "NewTables::expand", "    fn expand(", "callable CREATE parts obligation"),
    site(DIFF+"order/deployment.rs", "before_its_partitions_indexes", "pub fn before_its_partitions_indexes(", "partition-index adoption constraints"),
    site(DIFF+"order/parent_defaults.rs", "ParentDefaults::expand", "    fn expand(", "partition default restoration obligation"),
    site(DIFF+"order/deployment.rs", "after_the_rebuilds", "pub fn after_the_rebuilds(", "lexical compatibility rule"),
    site(DIFF+"order/deployment.rs", "after_their_functions", "pub fn after_their_functions(", "function hold/compatibility constraints"),
    site(CLI+"engine.rs", "order_after_releases", "pub(crate) fn order_after_releases<", "connected fact adapter"),
    site(CLI+"deploy.rs", "order_carried_moves", "pub(crate) fn order_carried_moves<", "catalog handover proposals"),
    site(CLI+"computed_order.rs", "order_by_edges", "pub(crate) fn order_by_edges<", "SQL Server computed-edge proposals"),
    site(CLI+"object_order.rs", "order_occupied_objects_under", "fn order_occupied_objects_under<", "retained namespace search proposals"),
    site(DIFF+"order/resolver_finish.rs", "settle", "pub(crate) fn settle(", "resolver completion; shared effect/slot graph"),
    site(DIFF+"order/scratch.rs", "function_edges", "fn function_edges<", "scratch constraints, if relevant"),
    site(MODEL+"operation/effects.rs", "Cx::by_kind", "fn by_kind(", "new portable kind/effect rule"),
    site(MODEL+"operation/slots.rs", "names", "pub fn names(", "new name claim/release kind"),
    *[site("crates/"+crate+"/src/lib.rs", "engine_effects", "fn engine_effects(", "engine-only semantics, if relevant")
      for crate in ("pbps-pg", "pbps-mssql")],
    site(DIFF+"order/deployment/schedule.rs", "scheduled", "fn scheduled<", "new obligation/fact stage only; effect-only rules need no call-site change"),
    site(DIFF+"order/expand.rs", "Expansion", "pub trait Expansion", "new obligation implementation; existing driver unchanged"),
    site(DIFF+"order/deployment.rs", "needs", "fn needs(", "scoped compatibility rule review"),
    site(DIFF+"order/deployment.rs", "rests_on", "fn rests_on(", "partition-index compatibility review"),
]
COMMON = [
    site(MODEL+"change.rs", "Change", "pub enum Change", "new logical kind: model/payload dispatch"),
    site(MODEL+"change.rs", "Change::intrinsic_risks_with", "fn intrinsic_risks_with(", "new kind: intrinsic risk semantics"),
    site("crates/pbps-dialect/src/lib.rs", "Dialect::change_risks", "fn change_risks(", "new kind: dialect risk augmentation, if relevant"),
    site(DIFF+"schema_diff.rs", "diff_partial_rebuilding", "fn diff_partial_rebuilding(", "new kind: generation and refusal policy"),
    site("crates/pbps-pg/src/emit.rs", "emit", "fn emit(", "new kind: PostgreSQL lowering/metadata"),
    site("crates/pbps-mssql/src/emit.rs", "emit", "fn emit(", "new kind: SQL Server lowering/metadata"),
    site("docs/ORDERING.md", "requires table", "## What each change requires", "kind/effect table and pair audit; tests pin agreement"),
]


def named(rows, symbol):
    return next(row for row in rows if row["symbol"] == symbol)


# Bounded cases make the units comparable rather than calling the union of
# every conditional helper an edit count for one hypothetical new feature.
CASES = {
    "new_portable_kind": {
        "scope": "ordering declarations only; existing resources and base class suffice; common feature dispatch excluded",
        "before": [named(BEFORE,"order_key")], "after": [named(AFTER,"order_key"), named(AFTER,"Cx::by_kind")],
    },
    "portable_resource_requirement_in_resolver": {
        "scope": "existing kind/resource and complete resolver effect route; no new obligation or engine fact",
        "before": [named(BEFORE,"constraints")], "after": [named(AFTER,"Cx::by_kind")],
    },
    "register_shared_declaration_rule": {
        "scope": "existing caller-maintained rule lists versus shared registration; offline had no old list",
        "before": [
            site(CLI+"engine.rs", "module_dependents rule list", "let added = crate::dependents::weave(", "connected list"),
            site(CLI+"deploy.rs", "bootstrap rule list", "let ordered = crate::dependents::split_new_tables(", "bootstrap list"),
        ],
        "after": [named(AFTER,"scheduled")],
    },
}

def git(*args):
    return subprocess.check_output(["git", *args], text=True, stderr=subprocess.PIPE)


def snapshot(sha, sites):
    rows = []
    for item in sites:
        lines = git("show", sha+":"+item["path"]).splitlines()
        found = [i for i, line in enumerate(lines, 1) if item["anchor"] in line
                 and (item["symbol"] != "emit" or line.startswith("pub"))]
        if len(found) != 1:
            raise ValueError(f"{sha}:{item['path']}:{item['symbol']}: {len(found)} anchors")
        rows.append(dict(item, line=found[0]))
    return dict(sha=sha, sites=rows, distinct_sites=len({(r["path"],r["symbol"]) for r in rows}),
                files=len({r["path"] for r in rows}))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", required=True)
    parser.add_argument("--after", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args=parser.parse_args()
    try:
        # Resolve both refs before reading any section: a ref may move during the inventory.
        before_sha=git("rev-parse", "--verify", args.before+"^{commit}").strip()
        after_sha=git("rev-parse", "--verify", args.after+"^{commit}").strip()
        report={"before_sha": before_sha, "after_sha": after_sha,
                "method": "audited conditional update/review sites, not universal edit counts",
                "before": snapshot(before_sha, BEFORE), "after": snapshot(after_sha, AFTER),
                "common_before": snapshot(before_sha, COMMON), "common_after": snapshot(after_sha, COMMON),
                "cases": {name: dict(scope=case["scope"], before=snapshot(before_sha,case["before"]),
                                          after=snapshot(after_sha,case["after"]))
                          for name,case in CASES.items()}}
    except (subprocess.CalledProcessError, ValueError) as error:
        parser.exit(2, str(error)+"\n")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2)+"\n")
    for side in ("before","after","common_before","common_after"):
        print(side,report[side]["sha"],report[side]["distinct_sites"],"sites",report[side]["files"],"files")
    for name,case in report["cases"].items():
        print(name,case["before"]["distinct_sites"],"->",case["after"]["distinct_sites"])


if __name__ == "__main__":
    main()
