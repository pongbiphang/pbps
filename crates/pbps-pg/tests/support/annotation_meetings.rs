//! Annotation boundaries and catalog bindings share the real canonical meeting route.
use pbps_diff::{
    Side,
    order::deployment::{
        Sources,
        schedule::{Declarations, Facts, schedule},
    },
};
use pbps_model::object_ref::ObjectDeps;
use pbps_model::operation::{MembershipCatalog, ModuleEdits, Snapshot, StoredColumn};
use pbps_model::{
    Column, IdsFile, Module, ModuleKind, PartitionBy, PartitionColumn, PartitionOf, Schema, Table,
    TableName, ValueSource,
};
use pbps_pg::Postgres;

pub struct Scene {
    pub before: Schema,
    pub after: Schema,
    pub base: IdsFile,
    pub ids: IdsFile,
    pub stored: MembershipCatalog,
}
pub fn scene() -> Scene {
    let p: TableName = "app.p".parse().unwrap();
    let a: TableName = "app.a".parse().unwrap();
    let column = |default: &str| {
        let mut c = Column::new("integer".parse().unwrap());
        c.value_source = ValueSource::Default(default.into());
        c
    };
    let mut parent = Table {
        partition_by: Some(PartitionBy {
            strategy: pbps_model::PartitionStrategy::Range,
            columns: vec!["k".into()],
            external: false,
        }),
        ..Default::default()
    };
    parent.columns.insert(
        "k".into(),
        Column::new("integer".parse().unwrap()).not_null(),
    );
    parent.columns.insert("d".into(), column("app.f(1)"));
    let mut candidate = Table::default();
    candidate
        .columns
        .insert("k".into(), parent.columns["k"].clone());
    candidate.columns.insert("d".into(), column("9"));
    let function: pbps_model::ModuleId = "app.f(integer)".parse().unwrap();
    let before = Schema {
        tables: [(p.clone(), parent), (a.clone(), candidate)].into(),
        modules: [(
            function.clone(),
            Module {
                kind: ModuleKind::Function,
                definition: "(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$"
                    .into(),
                description: None,
                options: Default::default(),
            },
        )]
        .into(),
        ..Default::default()
    };
    let mut after = before.clone();
    after
        .tables
        .get_mut(&p)
        .unwrap()
        .columns
        .get_mut("d")
        .unwrap()
        .ty = "bigint".parse().unwrap();
    after.tables.insert(
        a.clone(),
        Table {
            partition_of: Some(PartitionOf {
                parent: p.clone(),
                bound: pbps_model::PartitionBound::Default,
                columns: [(
                    "d".into(),
                    PartitionColumn {
                        default: Some("9".into()),
                        not_null: false,
                    },
                )]
                .into(),
            }),
            ..Default::default()
        },
    );
    after.modules.insert(
        "app.v".parse().unwrap(),
        Module {
            kind: ModuleKind::View,
            definition: "SELECT d FROM app.p".into(),
            description: None,
            options: Default::default(),
        },
    );
    let ctx = pbps_diff::Context {
        operator: "fixture".into(),
        today: "2026-10-30".into(),
    };
    let base = pbps_diff::resolve(&before, &Default::default(), &[], &ctx)
        .unwrap()
        .ids;
    let ids = pbps_diff::resolve(&after, &base, &[], &ctx).unwrap().ids;
    let mut stored = MembershipCatalog::default();
    stored.columns.insert(
        p.column("d"),
        StoredColumn {
            default: Some("app.f(1)".into()),
            routines: [function].into(),
            ..Default::default()
        },
    );
    Scene {
        before,
        after,
        base,
        ids,
        stored,
    }
}
pub fn canonical(
    s: &Scene,
    version: i64,
    stored: Option<&MembershipCatalog>,
) -> Result<pbps_model::ChangeSet, String> {
    let pg = Postgres::new().with_server_version(version);
    let before = Side {
        schema: &s.before,
        ids: &s.base,
    };
    let after = Side {
        schema: &s.after,
        ids: &s.ids,
    };
    let annotation = ObjectDeps(
        [(
            "module::app.v".parse().unwrap(),
            ["column::app.p.d".parse().unwrap()].into(),
        )]
        .into(),
    );
    let plan = pbps_diff::diff(before, after, &pg, &Default::default())
        .map_err(|e| format!("{e:?}"))?
        .with_annotations(
            Snapshot {
                schema: &s.before,
                ids: &s.base,
            },
            Snapshot {
                schema: &s.after,
                ids: &s.ids,
            },
            &ObjectDeps::default(),
            &annotation,
            ModuleEdits::Rebuilt,
        )?;
    let policies = s
        .base
        .tables
        .keys()
        .chain(s.base.columns.keys())
        .cloned()
        .map(|uid| (uid, Vec::new()))
        .collect();
    let dependents = Default::default();
    let facts = Facts::default()
        .with_module_dependents(&dependents)
        .with_policy_dependents(&policies);
    let facts = if let Some(stored) = stored {
        facts.with_membership_catalog(stored)
    } else {
        facts
    };
    let done = schedule(
        plan,
        &Declarations {
            schema: &s.after,
            sources: Sources {
                before,
                after: &s.ids,
            },
            module_deps: &Default::default(),
        },
        &facts,
        &pg,
    )
    .map_err(|e| e.to_string())?;
    let edges = done.changes.annotation_edges()?;
    let provider = done
        .changes
        .changes
        .iter()
        .position(|step| {
            matches!(
                &step.change, pbps_model::Change::AlterColumnType { column, .. }
                    if column == &"app.p.d".parse().unwrap()
            )
        })
        .expect("the parent type writer must survive the meeting");
    let consumer = done
        .changes
        .changes
        .iter()
        .position(|step| {
            matches!(
                &step.change, pbps_model::Change::CreateModule { id, .. }
                    if id == &"app.v".parse().unwrap()
            )
        })
        .expect("the annotated consumer must survive the meeting");
    assert!(
        edges
            .iter()
            .any(|edge| edge.source == pbps_diff::order::Source::Annotation
                && edge.before == provider
                && edge.after == consumer),
        "{edges:?}"
    );
    assert!(provider < consumer);
    assert!(
        done.changes
            .changes
            .iter()
            .any(|step| matches!(step.change, pbps_model::Change::AttachPartition { .. }))
    );
    let ordered = done
        .changes
        .ordered(pbps_model::PlanMode::Transactional, &pg);
    assert!(!ordered.changes().changes.is_empty());
    Ok(ordered.changes().clone())
}
