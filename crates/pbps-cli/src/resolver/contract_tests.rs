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
    fn configured(_: &[String], _: Option<i64>) -> Self {
        Self
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

/// The seal reads these where it once matched the driver: a string that moved
/// to the wrong engine would key one engine's evidence under the other's rule.
#[test]
fn each_engine_states_the_facts_its_evidence_is_sealed_under() {
    let pg = contract::planning(&pbps_pg::Postgres::new()).unwrap();
    assert_eq!(pg.channels_rule, "pbps/pg-channels/v1");
    assert_eq!(pg.cluster.rule, "pbps/pg-cluster-identity/v1");
    assert_eq!(pg.cluster.component, "system-identifier");
    // A supplied database compiles as its own login; a run-owned one
    // reproduces the deployer.
    assert_eq!(
        (pg.reproduction)(ScratchLayout::RunOwned),
        AuthorizationReproduction::Reproduced
    );
    assert_eq!(
        (pg.reproduction)(ScratchLayout::Supplied),
        AuthorizationReproduction::LoginStandin
    );
    let ms = contract::planning(&pbps_mssql::Mssql).unwrap();
    assert_eq!(ms.channels_rule, "pbps/mssql-channels/v1");
    assert_eq!(ms.cluster.rule, "pbps/mssql-instance-identity/v1");
    assert_eq!(ms.cluster.component, "family-guid");
    for layout in [ScratchLayout::RunOwned, ScratchLayout::Supplied] {
        assert_eq!(
            (ms.reproduction)(layout),
            AuthorizationReproduction::Reproduced
        );
    }
    assert_ne!(pg.channels_rule, ms.channels_rule);
}

/// What a run asked of an engine that has none must show nothing but the
/// engine's own refusal: the binding sides disagree and no key is configured,
/// so either would answer first if the engine were asked later, and the
/// request counter shows that no connection was tried.
fn unservable() -> (
    pbps_model::Schema,
    pbps_model::Schema,
    pbps_config::Project,
    pbps_diff::order::DiffPlan,
) {
    let schema = pbps_model::Schema::default();
    let mut different = pbps_model::Schema::default();
    different
        .tables
        .insert("app.other".parse().unwrap(), Default::default());
    let project = pbps_config::Project {
        root: std::env::temp_dir(),
        config: pbps_config::Config::parse("dialect: postgres\n", std::path::Path::new("pbps.yml"))
            .unwrap(),
    };
    let ids = IdsFile::default();
    let side = pbps_diff::Side {
        schema: &schema,
        ids: &ids,
    };
    let bootstrap =
        pbps_diff::diff(side, side, &MinimalDialect, &pbps_model::Hints::default()).unwrap();
    (schema, different, project, bootstrap)
}

#[tokio::test]
async fn an_engine_without_planning_support_is_refused_before_a_connection_opens() {
    use super::server::scratch::{Provenance, produce_unsealed};
    use super::server::{BindingRequest, ProduceError};
    let (schema, different, project, bootstrap) = unservable();
    let ids = IdsFile::default();
    let side = pbps_diff::Side {
        schema: &schema,
        ids: &ids,
    };
    let (outcome, requests) = pbps_db::request_count::scope(produce_unsealed::<MinimalDialect>(
        "invalid scratch connection",
        "invalid target connection",
        "",
        &BindingRequest {
            dependencies: &Default::default(),
            bootstrap: &bootstrap,
            base: &different,
            desired: &schema,
        },
        side,
        side,
        &pbps_model::Hints::default(),
        &[],
        &project,
        None,
        &(),
        None,
        &Provenance::server(),
    ))
    .await;
    match outcome {
        Err(ProduceError::Acquire(why)) => {
            assert_eq!(why, "Stand-in does not support resolver-backed planning");
        }
        Err(other) => panic!("not the engine's refusal: {other}"),
        Ok(_) => panic!("an engine without a resolver planned"),
    }
    assert_eq!(requests, 0, "the refusal must precede every connection");
}

#[tokio::test]
async fn an_engine_without_rehearsal_support_is_refused_before_a_connection_opens() {
    use super::server::scratch::{Provenance, rehearse::with_session};
    use super::server::{BindingRequest, ProduceError};
    let (schema, different, _, bootstrap) = unservable();
    let ids = IdsFile::default();
    let side = pbps_diff::Side {
        schema: &schema,
        ids: &ids,
    };
    let (outcome, requests) =
        pbps_db::request_count::scope(with_session::<MinimalDialect, (), ()>(
            "invalid scratch connection",
            "invalid target connection",
            "",
            &BindingRequest {
                dependencies: &Default::default(),
                bootstrap: &bootstrap,
                base: &different,
                desired: &schema,
            },
            side,
            side,
            &pbps_model::Hints::default(),
            &(),
            None,
            &Provenance::server(),
            &BTreeSet::new(),
            async |_, _, _| Ok(()),
        ))
        .await;
    match outcome {
        Err(ProduceError::Acquire(why)) => {
            assert_eq!(why, "Stand-in does not support connected rehearsal");
        }
        Err(other) => panic!("not the engine's refusal: {other}"),
        Ok(_) => panic!("an engine without a resolver rehearsed"),
    }
    assert_eq!(requests, 0, "the refusal must precede every connection");
}

/// What a resolver the engine does not have can still be asked, and answers
/// by name: nothing it could produce exists to hold.
#[tokio::test]
async fn a_resolver_that_does_not_exist_acquires_nothing_and_names_the_engine() {
    use super::contract::ResolverEngine;
    type Missing = <MinimalDialect as PlanningDialect>::Resolver;
    let refused = Missing::standard(None).err().unwrap().to_string();
    assert!(
        refused.contains("Stand-in does not support resolver-backed planning"),
        "{refused}"
    );
    let endpoint = Missing::endpoint("Server=db").await.unwrap_err();
    assert!(
        matches!(&endpoint, super::server::ProduceError::Acquire(why)
            if why == "Stand-in does not support resolver-backed planning"),
        "{endpoint}"
    );
}

#[test]
fn a_rehearsal_states_its_scope_request_and_key_policy_per_engine() {
    use super::contract::RehearsalRequest;
    let pg = contract::rehearsal(&pbps_pg::Postgres::new()).unwrap();
    assert_eq!(
        (pg.request, pg.run_key),
        (RehearsalRequest::Projected, false)
    );
    let ms = contract::rehearsal(&pbps_mssql::Mssql).unwrap();
    assert_eq!(
        (ms.request, ms.run_key),
        (RehearsalRequest::AsItStands, true)
    );
}
