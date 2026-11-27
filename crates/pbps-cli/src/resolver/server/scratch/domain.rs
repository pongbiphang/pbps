//! An unsealed, code-only goal of the scratch run (DEC-2309.1).
use super::super::{
    Error, PlanningDialect, ProduceError,
    producer::{request, request_as_it_stands},
};
use super::{Provenance, Standard, Tokens};
use crate::resolver::contract::{PreparedView, ResolverEngine};
use pbps_db::resolver::environment::Verdict;
use pbps_db::{Conn, Driver};
use pbps_dialect::Dialect;
use pbps_diff::order::finish;
use pbps_model::{Hints, IdsFile, ModuleKind, PlanMode, Schema};

/// The code vet is a PostgreSQL operation by design.
type Postgres = <pbps_pg::Postgres as PlanningDialect>::Resolver;

/// What a code-only run is for (DEC-2309.1, DEC-2354.1, DEC-1671.2).
pub(super) enum Goal<'a> {
    /// Compile the declared types and inspect their bound trees.
    Vet,
    Retained(&'a super::retained::Comparison<'a>),
    /// Rehearse attaches, over the base's declared types and functions.
    Attach(&'a super::attach::Rehearsal<'a>),
}

/// What the goal asked, where it asks anything.
#[derive(Default)]
pub(super) struct Answers {
    pub retained: Vec<pbps_model::RetainedExpression>,
    pub attach: Vec<pbps_model::adoption::AttachAnswer>,
}

/// An answered refusal is distinct from a scratch that could not answer.
#[derive(Debug)]
pub enum Refused {
    Code(String),
    Unavailable(ProduceError),
}

/// Create only declared types, in the emitter's dependency order, and inspect
/// their bound trees. No namespace modules, seals, keys or publication recheck.
#[allow(clippy::too_many_arguments)]
pub async fn vet(
    scratch: &str,
    target_connection: &str,
    identity: &str,
    desired: pbps_diff::Side<'_>,
    standard: &Standard<pbps_pg::Postgres>,
    externals: Option<&str>,
    provenance: &Provenance,
) -> Result<(), Refused> {
    run(
        scratch,
        target_connection,
        identity,
        desired,
        standard,
        externals,
        provenance,
        Goal::Vet,
    )
    .await
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    scratch: &str,
    target_connection: &str,
    identity: &str,
    desired: pbps_diff::Side<'_>,
    standard: &Standard<pbps_pg::Postgres>,
    externals: Option<&str>,
    provenance: &Provenance,
    goal: Goal<'_>,
) -> Result<Answers, Refused> {
    let dialect = pbps_pg::Postgres::new();
    // An attach's index definitions can call a function as well as name a type.
    let (functions, hints) = match &goal {
        Goal::Attach(rehearsal) => {
            let functions: std::collections::BTreeMap<_, _> = desired
                .schema
                .modules
                .iter()
                .filter(|(_, module)| module.kind == ModuleKind::Function)
                .map(|(id, module)| (id.clone(), module.clone()))
                .collect();
            let mut hints = Hints::default();
            hints.annotations.declared = pbps_model::object_ref::ObjectDeps::from_legacy(
                &rehearsal
                    .hints
                    .annotations
                    .baseline()
                    .modules(desired.schema),
            );
            (functions, hints)
        }
        Goal::Vet | Goal::Retained(_) => Default::default(),
    };
    let types = Schema {
        types: desired.schema.types.clone(),
        modules: functions,
        ..Default::default()
    };
    let ids = IdsFile {
        types: desired.ids.types.clone(),
        ..Default::default()
    };
    let empty = Schema::default();
    let empty_ids = IdsFile::default();
    let base = pbps_diff::Side {
        schema: &empty,
        ids: &empty_ids,
    };
    let desired_types = pbps_diff::Side {
        schema: &types,
        ids: &ids,
    };
    let unavailable = |why: String| Refused::Unavailable(ProduceError::Run(Error::Binding(why)));
    let bootstrap =
        pbps_diff::bootstrap(base, desired_types, &dialect, &hints).map_err(|errors| {
            unavailable(format!("the declared types cannot be ordered: {errors:?}"))
        })?;
    let scheduled = pbps_diff::order::deployment::schedule::schedule(
        finish::standalone(bootstrap.clone()),
        &pbps_diff::order::deployment::schedule::Declarations {
            schema: &types,
            sources: pbps_diff::order::deployment::Sources {
                before: base,
                after: &ids,
            },
            annotations: &hints.annotations,
        },
        &Default::default(),
        &dialect,
    )
    .map_err(|error| unavailable(error.to_string()))?;
    let ordered = finish::offline(
        scheduled.changes,
        finish::Inputs {
            before: base,
            after: desired_types,
            modules: &hints.annotations.modules(&types),
            captured: &Default::default(),
        },
        &dialect,
    )
    .map_err(|error| unavailable(error.to_string()))?
    .ordered(PlanMode::Transactional, &dialect);
    let lowered = pbps_dialect::lower::lower_all(
        &ordered as &dyn pbps_diff::order::PlanView<pbps_pg::Postgres>,
        &dialect,
    )
    .map_err(|error| unavailable(error.to_string()))?;
    let request = match &goal {
        Goal::Retained(comparison) => {
            request::<pbps_pg::Postgres>(comparison.before, comparison.after, comparison.hints, &[])
        }
        // The shells go in the schemas of the relations they copy.
        Goal::Attach(_) => Ok(request_as_it_stands(desired, desired, &[])),
        Goal::Vet => request::<pbps_pg::Postgres>(base, desired_types, &hints, &[]),
    }
    .map_err(|error| Refused::Unavailable(ProduceError::Run(error)))?;
    let mut target = Conn::connect(Driver::Postgres, target_connection)
        .await
        .map_err(|error| unavailable(format!("the vet's target session: {error}")))?;
    let tokens = Tokens::generate();
    let mut admin = Postgres::connect_scratch(scratch, &tokens)
        .await
        .map_err(Refused::Unavailable)?;
    let (provisioning, mut state) = Postgres::acquire(
        &mut target,
        &mut admin,
        &tokens,
        identity,
        scratch,
        &request,
        standard,
        externals,
        provenance,
    )
    .await
    .map_err(Refused::Unavailable)?;
    let result = async {
        let read = |error: pbps_db::DbError| {
            Refused::Unavailable(ProduceError::Run(Error::Read(error.to_string())))
        };
        let run = |error| Refused::Unavailable(ProduceError::Run(error));
        let schemas =
            Postgres::scope_schemas(&request.schemas, &[]).map_err(|why| run(Error::Scope(why)))?;
        let (catalog, authorization) =
            Postgres::read_target(&mut target, &request.schemas, &[], &schemas, &[])
                .await
                .map_err(read)?;
        let mut prepared = Postgres::prepare_code(
            &mut state,
            provisioning,
            &mut admin,
            &catalog,
            &authorization,
            &schemas,
        )
        .await
        .map_err(run)?;
        let report = Postgres::compare_reported(&catalog, prepared.catalog())
            .map_err(|why| run(Error::Scope(why)))?;
        // A different engine version cannot give a verdict about these trees.
        // Missing extension types still reach CREATE, which names the absent type.
        let version = "server_version_num";
        if catalog.observations.get(version) != prepared.catalog().observations.get(version) {
            return Err(run(Error::Incompatible(vec![version.into()])));
        }
        let zone = target.session_zone().cloned().ok_or_else(|| {
            run(Error::Scope(
                "the target reported no session time zone to vet in".into(),
            ))
        })?;
        let session = prepared.session(&mut admin);
        // Compile under the same pins as binding reconstruction and apply,
        // rather than the database/role defaults reproduced on scratch.
        // Session pins also survive the bound-tree reader's transactions.
        if let Some(pins) = dialect.session_pins(&zone) {
            session.execute(&pins).await.map_err(read)?;
        }
        for statement in lowered.groups().iter().flat_map(|g| g.statements()) {
            if let Err(failure) = pbps_pg::statement::execute(session, statement).await {
                let error = failure.error;
                let Some(annotation) = &statement.declared_type else {
                    return Err(read(error));
                };
                let code = error.server_error_code();
                if code.as_deref() == Some("42601") {
                    return Err(Refused::Code(format!(
                        "declared type `{}` could not bind {} as one statement: {error}",
                        annotation.name.shown(),
                        expression_parts(annotation),
                    )));
                }
                // A function this plan creates is not in the types-only scratch,
                // and that is not a refusal: apply binds the domain in its
                // creating transaction and judges the routine by the one
                // predicate (DEC-2563.2). Other compile failures are advisory
                // too; apply still owns the binding verdict.
                let missing_type =
                    code.as_deref() == Some("42704") && error.to_string().contains("type ");
                if missing_type {
                    return Err(Refused::Code(format!(
                        "declared domain `{}` could not be created for {}: {error}.",
                        annotation.name.shown(),
                        expression_parts(annotation),
                    )));
                }
                return Err(read(error));
            }
            if matches!(report.verdict(), Verdict::Verified)
                && let Some(annotation) = &statement.declared_type
            {
                pbps_pg::type_refs::vet_code(
                    session,
                    pbps_db::TransactionOwner::OwnTransaction,
                    std::slice::from_ref(&annotation.name),
                )
                .await
                .map_err(|error| match error {
                    pbps_pg::type_refs::CodeVetError::Refused(why) => Refused::Code(why),
                    pbps_pg::type_refs::CodeVetError::Unreadable(why) => run(Error::Read(why)),
                })?;
            }
        }
        match report.verdict() {
            Verdict::Verified => match goal {
                Goal::Retained(comparison) => comparison
                    .run(session)
                    .await
                    .map(|retained| Answers {
                        retained,
                        ..Default::default()
                    })
                    .map_err(|why| run(Error::Binding(why))),
                Goal::Attach(rehearsal) => Ok(Answers {
                    attach: rehearsal.run(session).await,
                    ..Default::default()
                }),
                Goal::Vet => Ok(Answers::default()),
            },
            Verdict::Mismatch(facts) => Err(run(Error::Incompatible(facts))),
            Verdict::Unknown(facts) => Err(run(Error::Scope(facts.join("; ")))),
        }
    }
    .await;
    let released = Postgres::release(&mut state, &mut admin).await;
    match released {
        Err(names) => Err(Refused::Unavailable(ProduceError::Cleanup(names))),
        Ok(()) => result,
    }
}

fn expression_parts(statement: &pbps_dialect::DeclaredTypeStatement) -> String {
    let mut parts = Vec::new();
    if let Some(pbps_model::ManagedType::Domain {
        default, checks, ..
    }) = &statement.definition
    {
        if default.is_some() {
            parts.push("DEFAULT".into());
        }
        parts.extend(
            checks
                .keys()
                .map(|name| format!("CHECK `{}`", name.shown())),
        );
    }
    parts.join(" and ")
}
