//! Certified lowering observes retained sources; admission observes wire inputs.
use super::expand::{Expansion, Order, Rewrite};
use super::{OrderedPlan, PlanView, PlanningPlan, Schedule, Source, admit, propose, seal};
use pbps_dialect::lower::{Lowering, lower_all};
use pbps_dialect::{DialectError, MinimalDialect, Statement};
use pbps_model::operation::Origin;
use pbps_model::resolver::OrderReason;
use pbps_model::{
    Change, ChangeSet, Column, Generated, IdsFile, PlanBaseline, PlanMode, PlanOrigin,
    PlannedChange, SavedPlan, Table, TableName, ValueSource,
};

// The fixture exercises the real expansion driver and settlement. Engine
// policy and emitted SQL remain pinned by the existing engine byte oracles.
struct GeneratedParts;
impl Expansion for GeneratedParts {
    type Obligation = usize;
    fn rule(&self) -> &'static str {
        "payload-generated-parts"
    }
    fn unmet(&self, plan: &[PlannedChange]) -> Result<Vec<usize>, String> {
        Ok(plan
            .iter()
            .enumerate()
            .filter_map(|(at, planned)| {
                if let Change::CreateTable { table, .. } = &planned.change {
                    table.columns.contains_key("g").then_some(at)
                } else {
                    None
                }
            })
            .collect())
    }
    fn expand(&self, plan: &[PlannedChange], at: &usize) -> Result<Rewrite, String> {
        let mut bare = plan[*at].clone();
        let Change::CreateTable { name, table, .. } = &mut bare.change else {
            unreachable!("the fixture's obligation names its generated table")
        };
        let generated = table.columns.shift_remove("g").unwrap();
        let add = PlannedChange::new(Change::AddColumn {
            uid: "c_aaaaab".parse().unwrap(),
            table: name.clone(),
            name: "g".into(),
            column: Box::new(generated),
        });
        Ok(Rewrite::Split {
            at: *at,
            parts: Box::new([bare, add]),
            tail: Vec::new(),
            reason: "the generated column follows the bare table".into(),
        })
    }
    fn order(&self, _: &[PlannedChange]) -> Result<Vec<Order>, String> {
        Ok(Vec::new())
    }
}

struct Probe;
impl Lowering for Probe {
    type Input = dyn PlanView<MinimalDialect>;
    fn emit_position(
        &self,
        input: &Self::Input,
        position: usize,
    ) -> Result<Vec<Statement>, DialectError> {
        Ok(vec![Statement::new(format!(
            "-- {}",
            input.changes().changes[position].change.subject()
        ))])
    }
}

fn expanded_and_reordered() -> PlanningPlan {
    let mut table = Table::default();
    table
        .columns
        .insert("n".into(), Column::new("integer".parse().unwrap()));
    let mut generated = Column::new("integer".parse().unwrap());
    generated.value_source = ValueSource::Generated(Generated {
        expression: "n + 1".into(),
        stored: true,
    });
    table.columns.insert("g".into(), generated);
    let input = propose(ChangeSet {
        changes: vec![
            PlannedChange::new(Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: TableName::new("app", "t"),
                table: Box::new(table),
                beside_default: None,
                moves_at_most: None,
            }),
            PlannedChange::new(Change::CreateRole {
                uid: "r_aaaaaa".parse().unwrap(),
                name: "reader".into(),
            }),
        ],
    });
    let expanded = super::expand::expand(input, &GeneratedParts, super::expand::FACTOR).unwrap();
    assert_eq!(expanded.rewrites, 1);
    let mut schedule = Schedule::new(3);
    schedule.edge(0, 1, Source::Effect, OrderReason::Structural);
    schedule.edge(2, 0, Source::Effect, OrderReason::Structural);
    let settled = schedule.settle(expanded.changes).unwrap();
    assert_eq!(settled.sequence, [2, 0, 1]);
    assert_eq!(
        settled.changes.origins(),
        [Origin::Whole(1), Origin::SplitPart(0), Origin::SplitPart(0)]
    );
    settled.into_planning()
}

fn ordered() -> OrderedPlan<MinimalDialect> {
    OrderedPlan::new(
        expanded_and_reordered(),
        PlanMode::Transactional,
        &MinimalDialect,
    )
}
fn wire(ordered: &OrderedPlan<MinimalDialect>) -> SavedPlan {
    let mut ids = IdsFile::default();
    let table = TableName::new("app", "t");
    ids.tables
        .insert("t_aaaaaa".parse().unwrap(), table.clone());
    ids.columns
        .insert("c_aaaaaa".parse().unwrap(), table.column("n"));
    ids.columns
        .insert("c_aaaaab".parse().unwrap(), table.column("g"));
    ids.roles
        .insert("r_aaaaaa".parse().unwrap(), "reader".into());
    SavedPlan::new(
        PlanOrigin::Database,
        "minimal",
        "2026-10-22",
        PlanBaseline {
            description: "target".into(),
            checksum: "baseline".into(),
            database_collation: None,
            time_zone: None,
        },
        ordered.changes().clone(),
        ids,
    )
}
fn assert_retained(plan: &(dyn PlanView<MinimalDialect> + 'static)) {
    let lowered = lower_all(plan, &Probe).unwrap();
    assert_eq!(
        lowered
            .groups()
            .iter()
            .map(|g| g.operation().to_string())
            .collect::<Vec<_>>(),
        ["1.0", "0.0", "0.1"]
    );
    assert_eq!(
        lowered
            .groups()
            .iter()
            .map(|g| g.origin().clone())
            .collect::<Vec<_>>(),
        [Origin::Whole(1), Origin::SplitPart(0), Origin::SplitPart(0)]
    );
    assert_eq!(
        lowered.groups()[1].origin().source(),
        lowered.groups()[2].origin().source()
    );
    assert_ne!(
        lowered.groups()[0].operation().source(),
        lowered.groups()[1].operation().source()
    );
}

#[test]
fn ordered_lowering_retains_split_sources_and_a_moved_changes_id() {
    let plan = ordered();
    assert_retained(&plan);
    assert_retained(&plan.clone());
}
#[test]
fn sealed_lowering_delegates_the_ordered_sources_without_changing_wire_bytes() {
    let plan = ordered();
    let wire = wire(&plan);
    let before = serde_json::to_vec(&wire).unwrap();
    let checksum = wire.checksum();
    let sealed = seal(plan, wire).unwrap();
    assert_retained(&sealed);
    assert_retained(&sealed.clone());
    assert_eq!(serde_json::to_vec(sealed.wire()).unwrap(), before);
    assert_eq!(sealed.checksum(), checksum);
}
#[test]
fn admission_uses_wire_step_ids_instead_of_guessing_lost_planning_sources() {
    let plan = ordered();
    let wire = wire(&plan);
    let checksum = wire.checksum();
    // Admission must use the actual saved representation, not accidentally
    // recover unsealed annotations that survived an in-memory clone.
    assert!(
        wire.changes
            .changes
            .iter()
            .any(|part| part.provenance.is_some())
    );
    let bytes = serde_json::to_vec(&wire).unwrap();
    let loaded: SavedPlan = serde_json::from_slice(&bytes).unwrap();
    assert!(
        loaded
            .changes
            .changes
            .iter()
            .all(|part| part.provenance.is_none())
    );
    assert_eq!(serde_json::to_vec(&loaded).unwrap(), bytes);
    assert_eq!(loaded.checksum(), checksum);
    assert_eq!(loaded, wire);
    let admitted = admit(
        loaded,
        &MinimalDialect,
        std::path::Path::new("plan.json"),
        &checksum,
    )
    .unwrap();
    for certificate in [&admitted, &admitted.clone()] {
        let lowered = lower_all(certificate as &dyn PlanView<MinimalDialect>, &Probe).unwrap();
        assert_eq!(
            lowered
                .groups()
                .iter()
                .map(|g| g.operation().to_string())
                .collect::<Vec<_>>(),
            ["0.0", "1.0", "2.0"]
        );
        assert_eq!(
            lowered
                .groups()
                .iter()
                .map(|g| g.origin().clone())
                .collect::<Vec<_>>(),
            [Origin::Whole(0), Origin::Whole(1), Origin::Whole(2)]
        );
        assert_eq!(
            lowered.statements(),
            lower_all(&plan as &dyn PlanView<MinimalDialect>, &Probe)
                .unwrap()
                .statements()
        );
    }
    assert_eq!(admitted.checksum(), checksum);
}
