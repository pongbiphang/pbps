use super::apply::Keyed;
use super::contract::{self, NoResolver, Operation};
use super::server::PlanningDialect;
use pbps_db::Driver;
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_dialect::MinimalDialect;
use pbps_model::resolver::*;
use pbps_model::{ChangeSet, IdsFile, PlanBaseline, PlanOrigin, SavedPlan};
use std::collections::BTreeSet;

/// A third engine that has no resolver. It borrows the minimal dialect, which
/// exists to stand in for an engine the product does not ship.
impl PlanningDialect for MinimalDialect {
    type Resolver = NoResolver<Self>;
    // Unused: nothing here connects, so no driver is ever asked for.
    const DRIVER: Driver = Driver::Postgres;
    const NAME: &'static str = "Stand-in";
    const FILTER_BINDS: bool = false;
    fn configured(_: &[String], _: Option<i64>) -> Self {
        Self
    }
    fn binding_classes(_: &Surface) -> Option<&'static [&'static str]> {
        None
    }
    fn dependent_object(
        _: &pbps_pg::resolver::capture::BindingRecord,
        _: &BTreeSet<ObjectIdentity>,
    ) -> Option<ObjectIdentity> {
        None
    }
}

fn resolved() -> SavedPlan {
    let changes = ChangeSet::default();
    let manifest = InputManifest::new(
        pbps_mssql::resolver::capture::RULE.into(),
        17,
        "01".repeat(8),
        ReadScope {
            retained: BTreeSet::new(),
            candidates: BTreeSet::new(),
        },
        "02".repeat(32),
        "03".repeat(32),
        vec![],
        vec![],
        BTreeSet::new(),
        vec![],
    )
    .unwrap();
    let evidence = ResolverEvidence::new(
        &changes,
        Qualification {
            rule: pbps_mssql::resolver::compatibility::REPORTED_RULE.into(),
            authorization: AuthorizationReproduction::Reproduced,
            target_environment: "04".repeat(32),
            target_environment_after: "04".repeat(32),
            resolver_environment: "05".repeat(32),
            target_build: "06".repeat(32),
            resolver_build: "07".repeat(32),
            channels: "08".repeat(32),
            target_cluster: "09".repeat(32),
            limitations: Default::default(),
            target_scope: TargetScope {
                schemas: vec!["app".into()],
                write_path_extras: vec![],
            },
            runtime: ResolverRuntime::Scratch {
                entry: "scratch".into(),
                scratch: ScratchSource::Server {},
                layout: ScratchLayout::Supplied,
                outside_writes: OutsideWrites::Refused,
                same_instance: SameInstance::Refused,
                dependencies: Default::default(),
            },
        },
        AuthorizationCondition {
            rule: pbps_mssql::resolver::authorization::RULE.into(),
            before: "10".repeat(32),
            after: "10".repeat(32),
            changes: BTreeSet::new(),
            object_grants: Vec::new(),
        },
        manifest.clone(),
        &manifest,
        vec![],
        vec![],
        OrderingProof::new(&changes, BTreeSet::new()).unwrap(),
    )
    .unwrap();
    SavedPlan::new(
        PlanOrigin::Database,
        "mssql",
        "fixture",
        PlanBaseline {
            time_zone: None,
            description: "target database".into(),
            checksum: "00".repeat(32),
            database_collation: None,
        },
        changes,
        IdsFile::default(),
    )
    .with_resolution(evidence)
    .unwrap()
}

fn ordinary() -> SavedPlan {
    let mut plan = resolved();
    plan.analysis = PlanAnalysis::Ordinary;
    plan
}

/// What `apply` asks first, with a key that would pass nothing: the engine's
/// answer is the one that shows.
fn first_check<D: PlanningDialect>(plan: &SavedPlan, dialect: &D) -> anyhow::Result<bool> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (checked, requests) = runtime.block_on(pbps_db::request_count::scope(async {
        Keyed::check(
            plan,
            Ok(EnvironmentFingerprintKey::for_one_run()),
            "target",
            dialect,
        )
        .map(|keyed| keyed.is_some())
    }));
    assert_eq!(requests, 0, "the check must not reach a connection");
    checked
}

#[test]
fn an_engine_without_apply_support_refuses_resolver_evidence_by_name() {
    let error = first_check(&resolved(), &MinimalDialect).unwrap_err();
    assert_eq!(
        error.to_string(),
        "Stand-in cannot recheck this plan's resolver evidence"
    );
    // The same plan on an engine that has the recheck is asked for its key
    // instead, so the refusal above is the engine's and not the plan's.
    let error = first_check(&resolved(), &pbps_mssql::Mssql).unwrap_err();
    assert!(error.to_string().contains("fingerprint key"), "{error}");
}

#[test]
fn an_ordinary_plan_needs_no_apply_support() {
    assert!(!first_check(&ordinary(), &MinimalDialect).unwrap());
    assert!(!first_check(&ordinary(), &pbps_mssql::Mssql).unwrap());
}

#[test]
fn a_resolved_plan_never_degrades_to_an_ordinary_apply() {
    // Whatever the environment's key is, an engine that cannot recheck has no
    // way through: neither a key refusal nor `None` stands for its answer.
    let project = pbps_config::Project {
        root: std::env::temp_dir(),
        config: pbps_config::Config::parse("dialect: mssql\n", std::path::Path::new("pbps.yml"))
            .unwrap(),
    };
    for key in [
        Ok(EnvironmentFingerprintKey::for_one_run()),
        super::sealing::environment_key(&project, None),
    ] {
        let plan = resolved();
        let refused = Keyed::check(&plan, key, "target", &MinimalDialect);
        assert!(
            refused
                .err()
                .is_some_and(|error| error.to_string().contains("cannot recheck")),
            "a resolved plan on an engine without a recheck must be refused by name"
        );
    }
}

#[test]
fn every_unsupported_operation_names_the_engine() {
    let engine = MinimalDialect;
    let refused = [
        contract::planning(&engine).err().unwrap().to_string(),
        contract::rehearsal(&engine).err().unwrap().to_string(),
        contract::preview(&engine).err().unwrap().to_string(),
        contract::apply(&engine).err().unwrap().to_string(),
    ];
    assert_eq!(
        refused,
        [
            "Stand-in does not support resolver-backed planning",
            "Stand-in does not support connected rehearsal",
            "Stand-in does not support target-less dev rehearsal",
            "Stand-in cannot recheck this plan's resolver evidence",
        ]
    );
    // The shipped engines say yes where they have it, and PostgreSQL has no
    // target-less dev rehearsal.
    let (pg, ms) = (pbps_pg::Postgres::new(), pbps_mssql::Mssql);
    assert!(contract::planning(&pg).is_ok() && contract::planning(&ms).is_ok());
    assert!(contract::rehearsal(&pg).is_ok() && contract::rehearsal(&ms).is_ok());
    assert!(contract::apply(&pg).is_ok() && contract::apply(&ms).is_ok());
    assert!(contract::preview(&ms).is_ok());
    let error = contract::preview(&pg).err().unwrap();
    assert_eq!(error.operation, Operation::Preview);
    assert_eq!(
        error.to_string(),
        "PostgreSQL does not support target-less dev rehearsal"
    );
}

#[test]
fn apply_and_producer_seal_under_one_cluster_domain() {
    let pg = contract::apply(&pbps_pg::Postgres::new())
        .unwrap()
        .cluster();
    assert_eq!(pg.rule, super::server::CLUSTER_RULE);
    assert_eq!(pg.component, "system-identifier");
    let ms = contract::apply(&pbps_mssql::Mssql).unwrap().cluster();
    assert_eq!(ms.rule, super::server::MSSQL_CLUSTER_RULE);
    assert_eq!(ms.component, "family-guid");
    // Planning declares the same domain apply recomputes.
    let planned = contract::planning(&pbps_mssql::Mssql).unwrap().cluster;
    assert_eq!(planned, ms);
    assert_ne!(pg, ms, "the engines must not share a cluster domain");
}
