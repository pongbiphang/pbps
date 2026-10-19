//! PostgreSQL's engine effects, through the operation-level verifier (#1954).

use pbps_diff::order::{Baseline, Violation, analyse};
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
use pbps_model::plan_names::PlanNames;
use pbps_model::{
    Change, Column, ColumnType, Identity, IdsFile, PlannedChange, Schema, SequenceOptions, Table,
    TableName, Uid, UidKind, ValueSource,
};
use pbps_pg::Postgres;

fn t() -> TableName {
    TableName::new("public", "t")
}

fn ty(name: &str) -> ColumnType {
    name.parse().unwrap()
}

fn schema(column_ty: &str, identity: Identity) -> Schema {
    let mut column = Column::new(ty(column_ty));
    column.nullable = false;
    column.value_source = ValueSource::Identity(identity);
    let mut table = Table::default();
    table.columns.insert("id".into(), column);
    let mut schema = Schema::default();
    schema.tables.insert(t(), table);
    schema
}

/// A kept identity whose bounds change beside a retype of its column, checked
/// in `order` (indexes into `[retune, retype]`).
fn violations(from: &str, to: &str, order: &[usize]) -> Vec<Violation> {
    let retuned = Identity {
        options: SequenceOptions {
            max: Some(100),
            ..SequenceOptions::default()
        },
        ..Identity::new(1, 1)
    };
    let (before, after) = (schema(from, Identity::new(1, 1)), schema(to, retuned));
    let mut ids = IdsFile::default();
    let uid = Uid::derived(UidKind::Column, "t.id", 0);
    ids.tables.insert(Uid::derived(UidKind::Table, "t", 0), t());
    ids.columns.insert(uid.clone(), t().column("id"));
    let logical: Vec<PlannedChange> = vec![
        Change::AlterColumnNumbering {
            uid: uid.clone(),
            column: t().column("id"),
            ty: ty(to),
            from: ValueSource::Identity(Identity::new(1, 1)),
            to: ValueSource::Identity(retuned),
        },
        Change::AlterColumnType {
            uid,
            column: t().column("id"),
            from: ty(from),
            to: ty(to),
            from_nullable: false,
            to_nullable: false,
            from_collation: None,
            to_collation: None,
            through_external_partitions: false,
        },
    ]
    .into_iter()
    .map(PlannedChange::new)
    .collect();
    let ops = Operations::of_plan(&logical, ModuleEdits::InPlace);
    let names = PlanNames::of(&logical, &ids).unwrap();
    let owners = PlanOwners::new(names, &ids, &ids);
    let (was, will) = (
        Snapshot {
            schema: &before,
            ids: &ids,
        },
        Snapshot {
            schema: &after,
            ids: &ids,
        },
    );
    let baseline = Baseline::of(was, &owners);
    let analysed = analyse(&ops, &owners, was, will, &Postgres::new());
    assert!(analysed.unowned.is_empty(), "{:?}", analysed.unowned);
    let ordered = ops.reordered(order);
    let effects: Vec<_> = order.iter().map(|&i| analysed.effects[i].clone()).collect();
    pbps_diff::order::verify(ordered.as_slice(), &effects, &baseline, &Default::default())
}

#[test]
fn a_kept_identity_retuned_goes_before_a_retype_that_narrows_its_column() {
    // Measured on 16 and 18: the engine moves an identity's sequence with its
    // column and refuses the retype while a bound for the wider type stands.
    assert_eq!(violations("integer", "smallint", &[0, 1]), []);
}

#[test]
fn a_kept_identity_retuned_goes_after_a_retype_that_widens_its_column() {
    assert_eq!(violations("smallint", "integer", &[1, 0]), []);
    // Negative: its new bounds are set for the wider type, which the narrower
    // column refuses.
    assert!(
        !violations("smallint", "integer", &[0, 1]).is_empty(),
        "the retune before a widening is refused"
    );
}
