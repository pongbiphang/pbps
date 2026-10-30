/// The native PG18 identity tree is an existing reader capability limit.
/// The actual product differ/route must still name the retained identity
/// limitation, rather than treating that reader error as a successful plan.
pub(super) fn identity_meeting_refusal(major: i64) {
    use pbps_model::{
        BoundDatum, Column, Identity, OwnedSequence, PartitionBound, PartitionBy, PartitionOf,
        Schema, Table, TableName, ValueSource,
    };
    let parent: TableName = "app.p".parse().unwrap();
    let child: TableName = "app.a".parse().unwrap();
    let mut p = Table {
        partition_by: Some(PartitionBy {
            strategy: Default::default(),
            columns: vec!["k".into()],
            external: false,
        }),
        ..Default::default()
    };
    p.columns.insert(
        "k".into(),
        Column::new("integer".parse().unwrap()).not_null(),
    );
    let mut d = Column::new("integer".parse().unwrap()).not_null();
    d.value_source = ValueSource::Identity(Identity::new(1, 1));
    p.columns.insert("d".into(), d);
    let mut before = Schema::default();
    before.tables.insert(parent.clone(), p.clone());
    before.tables.insert(
        child.clone(),
        Table {
            partition_of: Some(PartitionOf {
                parent: parent.clone(),
                bound: PartitionBound::Range {
                    from: vec![BoundDatum::MinValue],
                    to: vec![BoundDatum::MaxValue],
                },
                columns: Default::default(),
            }),
            ..Default::default()
        },
    );
    let ctx = pbps_diff::Context {
        operator: "fixture".into(),
        today: "2026-10-24".into(),
    };
    let base = pbps_diff::resolve(&before, &Default::default(), &[], &ctx)
        .unwrap()
        .ids;
    let mut after = before.clone();
    let mut sequence = OwnedSequence::serial();
    sequence.name = Some("seq".into());
    after
        .tables
        .get_mut(&parent)
        .unwrap()
        .columns
        .get_mut("d")
        .unwrap()
        .value_source = ValueSource::Sequence(sequence);
    p.partition_by = None;
    p.columns.get_mut("d").unwrap().value_source = ValueSource::None;
    after.tables.insert(child, p);
    let ids = pbps_diff::resolve(&after, &base, &[], &ctx).unwrap().ids;
    let pg = pbps_pg::Postgres::new().with_server_version(major * 10000);
    let a = pbps_diff::Side {
        schema: &before,
        ids: &base,
    };
    let b = pbps_diff::Side {
        schema: &after,
        ids: &ids,
    };
    let detail = match pbps_diff::diff(a, b, &pg, &Default::default()) {
        Err(e) => e
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "),
        Ok(plan) => {
            let policies = base
                .tables
                .keys()
                .chain(base.columns.keys())
                .cloned()
                .map(|u| (u, Vec::new()))
                .collect();
            match pbps_diff::order::deployment::schedule::schedule(
                plan,
                &pbps_diff::order::deployment::schedule::Declarations {
                    schema: &after,
                    sources: pbps_diff::order::deployment::Sources {
                        before: a,
                        after: &ids,
                    },
                    module_deps: &Default::default(),
                },
                &pbps_diff::order::deployment::schedule::Facts::default()
                    .with_policy_dependents(&policies)
                    .with_module_dependents(&Default::default())
                    .with_membership_catalog(&Default::default()),
                &pg,
            ) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("identity capability unexpectedly admitted"),
            }
        }
    };
    assert!(detail.contains("identity"), "{detail}");
    println!("RETAINED-IDENTITY-CAPABILITY PG{major}: {detail}");
}
