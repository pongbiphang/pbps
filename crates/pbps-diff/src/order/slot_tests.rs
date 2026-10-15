//! Name slots on the negatives #1964 names, and the guard checks that show
//! each one needs what it uses.

use std::borrow::Cow;
use std::collections::BTreeSet;

use pbps_dialect::slots::{CatalogFacts, Namespace, SlotKey};
use pbps_dialect::{Dialect, DialectError, MinimalDialect, Statement};
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot, effects};
use pbps_model::plan_names::PlanNames;
use pbps_model::{
    Change, ColumnType, IdsFile, ModuleDeps, PlannedChange, Schema, Strategy, Table, TableName,
    Uid, UidKind,
};

use super::edges::Baseline;
use super::slots::{Skipped, SlotViolation, analyse, edges, verify};

/// An engine that renames across schemas in two statements, through the
/// table's old name in the new schema, as PostgreSQL's `SET SCHEMA` and SQL
/// Server's `TRANSFER` do. `exact` is whether it compares names by their
/// spelling (PostgreSQL) or under a collation (SQL Server).
struct Slotted {
    exact: bool,
}

impl Dialect for Slotted {
    fn name(&self) -> &'static str {
        if self.exact { "exact" } else { "collated" }
    }
    fn compares_names_exactly(&self) -> bool {
        self.exact
    }
    fn quote_ident(&self, ident: &str) -> Result<String, DialectError> {
        MinimalDialect.quote_ident(ident)
    }
    fn emit(&self, change: &Change, strategy: Strategy) -> Result<Vec<Statement>, DialectError> {
        if let Change::RenameTable { from, to, .. } = change
            && from.schema != to.schema
        {
            let stop = TableName::new(to.schema.clone(), from.name.clone());
            return Ok(vec![
                Statement::new(format!("move {from}")).renaming(from.clone(), stop.clone()),
                Statement::new(format!("rename {stop}")).renaming(stop, to.clone()),
            ]);
        }
        MinimalDialect.emit(change, strategy)
    }
    fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
        MinimalDialect.normalize_type(ty)
    }
    fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> pbps_dialect::TypeChangeRisk {
        MinimalDialect.type_change_risk(from, to)
    }
    fn fold_ident<'a>(&self, ident: &'a str) -> Cow<'a, str> {
        MinimalDialect.fold_ident(ident)
    }
    fn lexicon(&self) -> pbps_dialect::Lexicon {
        MinimalDialect.lexicon()
    }
    fn validate_table(&self, name: &TableName, table: &Table) -> Vec<DialectError> {
        MinimalDialect.validate_table(name, table)
    }
    fn transaction_framing(
        &self,
        zone: &pbps_model::SessionZone,
    ) -> pbps_dialect::TransactionFraming {
        MinimalDialect.transaction_framing(zone)
    }
    fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
        MinimalDialect.probe_framing()
    }
}

fn uid(name: &str) -> Uid {
    Uid::derived(UidKind::Table, name, 0)
}
fn name(qualified: &str) -> TableName {
    qualified.parse().unwrap()
}

/// Tables before and after, by `(uid seed, name)`, and the plan between.
struct Plan {
    logical: Vec<PlannedChange>,
    before: Schema,
    after: Schema,
    before_ids: IdsFile,
    after_ids: IdsFile,
}

impl Plan {
    fn new(before: &[(&str, &str)], after: &[(&str, &str)], changes: Vec<Change>) -> Plan {
        let side = |tables: &[(&str, &str)]| {
            let mut schema = Schema::default();
            let mut ids = IdsFile::default();
            for (seed, at) in tables {
                schema.tables.insert(name(at), Table::default());
                ids.tables.insert(uid(seed), name(at));
            }
            (schema, ids)
        };
        let (before, before_ids) = side(before);
        let (after, after_ids) = side(after);
        Plan {
            logical: changes.into_iter().map(PlannedChange::new).collect(),
            before,
            after,
            before_ids,
            after_ids,
        }
    }

    /// The slot replay over the plan in `order`, with the catalog's facts
    /// where given, and the effects' own replay beside it.
    fn run(
        &self,
        order: &[usize],
        dialect: &dyn Dialect,
        facts: Option<&CatalogFacts>,
    ) -> (Vec<SlotViolation>, Vec<Skipped>, usize) {
        let logical = Operations::of_plan(&self.logical, ModuleEdits::InPlace);
        let names = PlanNames::of(&self.logical, &self.after_ids).unwrap();
        let owners = PlanOwners::new(names, &self.before_ids, &self.after_ids);
        let before = Snapshot {
            schema: &self.before,
            ids: &self.before_ids,
        };
        let after = Snapshot {
            schema: &self.after,
            ids: &self.after_ids,
        };
        let ops = logical.reordered(order);
        let slots = analyse(ops.as_slice(), &owners, before, dialect);
        let (violations, skipped) = verify(ops.as_slice(), &slots, dialect, facts, &owners, before);
        // The effects' replay on the same order: what the slots add to it.
        let fx: Vec<_> = ops
            .iter()
            .map(|op| effects(op, &owners, before, after).unwrap())
            .collect();
        let effect_violations = super::verify::verify(
            ops.as_slice(),
            &fx,
            &Baseline::of(before, &owners),
            &ModuleDeps::new(),
        )
        .len();
        (violations, skipped, effect_violations)
    }
}

fn drop_table(seed: &str, at: &str) -> Change {
    Change::DropTable {
        uid: uid(seed),
        name: name(at),
        detach_from: None,
    }
}
fn rename_table(seed: &str, from: &str, to: &str) -> Change {
    Change::RenameTable {
        uid: uid(seed),
        from: name(from),
        to: name(to),
        defaults: Vec::new(),
    }
}

fn held(violations: &[SlotViolation]) -> Vec<String> {
    violations
        .iter()
        .map(|v| match v {
            SlotViolation::Held { slot, .. } => format!("held {slot}"),
            SlotViolation::IntermediateHeld { slot, .. } => format!("intermediate {slot}"),
            SlotViolation::Edge { slot, .. } => format!("edge {slot}"),
            SlotViolation::Contested { slot, .. } => format!("contested {slot}"),
        })
        .collect()
}

/// A rename into a name the plan frees runs after the drop that frees it.
/// The other order claims a name still held, and the slots refuse it by
/// name. The effects alone do not: a name is not an identity, so nothing in
/// them orders the two (the guard check).
#[test]
fn a_rename_into_a_name_still_held_is_refused() {
    let plan = Plan::new(
        &[("a", "app.a"), ("b", "app.b")],
        &[("a", "app.b")],
        vec![
            drop_table("b", "app.b"),
            rename_table("a", "app.a", "app.b"),
        ],
    );
    let exact = Slotted { exact: true };
    let (ok, _, _) = plan.run(&[0, 1], &exact, None);
    assert!(ok.is_empty(), "{ok:?}");
    let (wrong, _, effect_violations) = plan.run(&[1, 0], &exact, None);
    assert_eq!(held(&wrong), ["held app.b", "edge app.b"], "{wrong:?}");
    assert_eq!(effect_violations, 0, "the guard: only the slots see it");
}

/// A move to another schema passes through its old name there between its
/// two statements. A table that still holds that name refuses the move at
/// that statement, though neither end of the move is held: only the
/// statement-level check sees it (the guard check clears the lowering's
/// intermediate names and watches the negative pass).
#[test]
fn a_claim_on_an_intermediate_name_held_at_that_statement_is_refused() {
    let plan = Plan::new(
        &[("x", "s1.x"), ("y", "s2.x")],
        &[("x", "s2.z")],
        vec![drop_table("y", "s2.x"), rename_table("x", "s1.x", "s2.z")],
    );
    let exact = Slotted { exact: true };
    let (ok, _, _) = plan.run(&[0, 1], &exact, None);
    assert!(ok.is_empty(), "{ok:?}");
    let (wrong, _, _) = plan.run(&[1, 0], &exact, None);
    assert_eq!(held(&wrong), ["intermediate s2.x"], "{wrong:?}");

    // The guard: without the lowering's intermediate names, nothing refuses.
    let logical = Operations::of_plan(&plan.logical, ModuleEdits::InPlace);
    let names = PlanNames::of(&plan.logical, &plan.after_ids).unwrap();
    let owners = PlanOwners::new(names, &plan.before_ids, &plan.after_ids);
    let before = Snapshot {
        schema: &plan.before,
        ids: &plan.before_ids,
    };
    let ops = logical.reordered(&[1, 0]);
    let mut slots = analyse(ops.as_slice(), &owners, before, &exact);
    for fx in &mut slots.effects {
        fx.intermediate.clear();
    }
    let (unguarded, _) = verify(ops.as_slice(), &slots, &exact, None, &owners, before);
    assert!(unguarded.is_empty(), "{unguarded:?}");
}

/// On an engine that compares names under a collation, two spellings the
/// catalog reports alike are one name: a rename into `ab` waits for the drop
/// of `Ab`. Without the fact the check is skipped and says so; it never reads
/// absence as "different names". With the catalog's word that they differ,
/// nothing is refused and nothing skipped.
#[test]
fn spellings_the_catalog_reports_alike_are_one_slot() {
    let plan = Plan::new(
        &[("ab", "app.Ab"), ("cd", "app.cd")],
        &[("cd", "app.ab")],
        vec![
            drop_table("ab", "app.Ab"),
            rename_table("cd", "app.cd", "app.ab"),
        ],
    );
    let collated = Slotted { exact: false };
    let key = |n: &str| SlotKey {
        namespace: Namespace::Relations("app".into()),
        name: n.into(),
    };
    let compared: BTreeSet<SlotKey> = ["Ab", "ab", "cd"].into_iter().map(key).collect();
    let alike = CatalogFacts {
        compared: compared.clone(),
        alike: [(key("Ab"), key("ab"))].into_iter().collect(),
        ..CatalogFacts::default()
    };
    let (ok, _, _) = plan.run(&[0, 1], &collated, Some(&alike));
    assert!(ok.is_empty(), "{ok:?}");
    let (wrong, skipped, _) = plan.run(&[1, 0], &collated, Some(&alike));
    assert_eq!(held(&wrong), ["held app.ab", "edge app.Ab"], "{wrong:?}");
    assert!(skipped.is_empty(), "{skipped:?}");

    // No fact: skipped and reported, never passed as distinct.
    let (unknown, skipped, _) = plan.run(&[1, 0], &collated, None);
    assert!(unknown.is_empty(), "{unknown:?}");
    assert_eq!(
        skipped
            .iter()
            .map(|s| format!("{} beside {}", s.claimed, s.held))
            .collect::<Vec<_>>(),
        ["app.ab beside app.Ab"]
    );

    // The catalog compared them and found them different.
    let different = CatalogFacts {
        compared,
        ..CatalogFacts::default()
    };
    let (clear, skipped, _) = plan.run(&[1, 0], &collated, Some(&different));
    assert!(
        clear.is_empty() && skipped.is_empty(),
        "{clear:?} {skipped:?}"
    );

    // The guard: an engine that compares exactly reads them as two names.
    let (exact, skipped, _) = plan.run(&[1, 0], &Slotted { exact: true }, None);
    assert!(
        exact.is_empty() && skipped.is_empty(),
        "{exact:?} {skipped:?}"
    );
}

/// The evidence orders a name's first holder before every other claim, and
/// every other release before its last holder. Objects that take the name and
/// give it up again in between have no place the evidence fixes, and get no
/// edge from position: here `b` and `d` each borrow `app.t` between `a` and
/// `c`, in either order, and only the two ends are ordered against them.
#[test]
fn only_the_first_and_last_holders_of_a_name_are_ordered_by_evidence() {
    let plan = Plan::new(
        &[
            ("a", "app.t"),
            ("b", "app.b"),
            ("c", "app.c"),
            ("d", "app.d"),
        ],
        &[("b", "app.b2"), ("c", "app.t"), ("d", "app.d2")],
        vec![
            drop_table("a", "app.t"),
            rename_table("b", "app.b", "app.t"),
            rename_table("b", "app.t", "app.b2"),
            rename_table("d", "app.d", "app.t"),
            rename_table("d", "app.t", "app.d2"),
            rename_table("c", "app.c", "app.t"),
        ],
    );
    let exact = Slotted { exact: true };
    let logical = Operations::of_plan(&plan.logical, ModuleEdits::InPlace);
    let names = PlanNames::of(&plan.logical, &plan.after_ids).unwrap();
    let owners = PlanOwners::new(names, &plan.before_ids, &plan.after_ids);
    let before = Snapshot {
        schema: &plan.before,
        ids: &plan.before_ids,
    };
    let slots = analyse(logical.as_slice(), &owners, before, &exact);
    let found: Vec<(usize, usize)> = edges(&slots, None)
        .into_iter()
        .filter(|e| e.slot.name == "t")
        .map(|e| (e.before, e.after))
        .collect();
    // `a` gives `t` up before anyone takes it; `b` and `d` give it up before
    // `c`, the last holder, takes it. Nothing orders `b` against `d`.
    assert_eq!(found, [(0, 1), (0, 3), (0, 5), (2, 5), (4, 5)], "{found:?}");
    for order in [[0, 1, 2, 3, 4, 5], [0, 3, 4, 1, 2, 5]] {
        let (ok, _, _) = plan.run(&order, &exact, None);
        assert!(ok.is_empty(), "{order:?}: {ok:?}");
    }
    // `c` takes the name while `d` still holds it.
    let (wrong, _, _) = plan.run(&[0, 1, 2, 3, 5, 4], &exact, None);
    assert_eq!(held(&wrong), ["held app.t", "edge app.t"], "{wrong:?}");
}

/// Two objects that take one name and never give it up collide in the
/// declarations, which no order fixes: recorded as contested, which is the
/// loader's to refuse (`check_constraint_names`), never as an order's
/// violation.
#[test]
fn a_name_two_objects_keep_is_contested_not_misordered() {
    let plan = Plan::new(
        &[("t", "app.t")],
        &[("t", "app.t")],
        vec![
            Change::AddCheck {
                table: name("app.t"),
                name: "shared".into(),
                constraint: pbps_model::CheckConstraint {
                    expression: "true".into(),
                },
            },
            Change::AddUnique {
                table: name("app.t"),
                name: "shared".into(),
                constraint: pbps_model::UniqueConstraint {
                    columns: Vec::new(),
                    storage_parameters: Default::default(),
                },
                clustered: false,
            },
        ],
    );
    let (found, _, _) = plan.run(&[0, 1], &Slotted { exact: true }, None);
    assert_eq!(held(&found), ["contested app.shared"], "{found:?}");
}

/// A catalog occupant that belongs to a column, a SQL Server default
/// constraint here, leaves at the step that changes that column's default.
/// A table created at its name waits for that step. Where no step touches the
/// column, the name is held whatever the order: contested, not misordered.
#[test]
fn a_columns_default_constraint_leaves_with_the_columns_default() {
    let column_uid = Uid::derived(UidKind::Column, "app.t.c", 0);
    let mut plan = Plan::new(
        &[("t", "app.t")],
        &[("t", "app.t"), ("n", "app.DF_t_c")],
        vec![
            Change::AlterColumnDefault {
                uid: column_uid.clone(),
                column: name("app.t").column("c"),
                from: Some("1".into()),
                to: None,
            },
            Change::CreateTable {
                uid: uid("n"),
                name: name("app.DF_t_c"),
                table: Box::default(),
                beside_default: None,
                moves_at_most: None,
            },
        ],
    );
    for (schema, ids) in [
        (&mut plan.before, &mut plan.before_ids),
        (&mut plan.after, &mut plan.after_ids),
    ] {
        schema
            .tables
            .get_mut(&name("app.t"))
            .unwrap()
            .columns
            .insert("c".into(), pbps_model::Column::new("int".parse().unwrap()));
        ids.columns
            .insert(column_uid.clone(), name("app.t").column("c"));
    }
    let facts = CatalogFacts {
        occupants: vec![pbps_dialect::slots::Occupant {
            slot: SlotKey {
                namespace: Namespace::Relations("app".into()),
                name: "DF_t_c".into(),
            },
            kind: "default constraint".into(),
            owner: Some(name("app.t")),
            column: Some("c".into()),
        }],
        ..CatalogFacts::default()
    };
    let exact = Slotted { exact: true };
    let (ok, _, _) = plan.run(&[0, 1], &exact, Some(&facts));
    assert!(ok.is_empty(), "{ok:?}");
    let (wrong, _, _) = plan.run(&[1, 0], &exact, Some(&facts));
    assert_eq!(held(&wrong), ["held app.DF_t_c"], "{wrong:?}");

    // Nothing takes the default away: no order frees the name.
    plan.logical.remove(0);
    let (kept, _, _) = plan.run(&[0], &exact, Some(&facts));
    assert_eq!(held(&kept), ["contested app.DF_t_c"], "{kept:?}");
}
