//! An unsealed, code-only goal of the scratch run (DEC-2309.1).
use super::super::{Error, ProduceError, engine, producer::request};
use super::{Provenance, Standard, Tokens};
use crate::resolver::scope;
use pbps_db::resolver::environment::Verdict;
use pbps_db::{Conn, Driver};
use pbps_dialect::Dialect;
use pbps_model::{Hints, IdsFile, PlanMode, Schema};

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
    standard: impl Into<Standard>,
    externals: Option<&str>,
    provenance: &Provenance,
) -> Result<(), Refused> {
    let types = Schema {
        types: desired.schema.types.clone(),
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
    let hints = Hints::default();
    let dialect = pbps_pg::Postgres::new();
    let unavailable = |why: String| Refused::Unavailable(ProduceError::Run(Error::Binding(why)));
    let bootstrap = pbps_diff::diff(base, desired_types, &dialect, &hints).map_err(|errors| {
        unavailable(format!("the declared types cannot be ordered: {errors:?}"))
    })?;
    let ordered = pbps_diff::order::deployment::schedule::schedule(
        bootstrap.clone(),
        &pbps_diff::order::deployment::schedule::Declarations {
            schema: &types,
            sources: pbps_diff::order::deployment::Sources {
                before: base,
                after: &ids,
            },
            module_deps: &hints.module_deps,
        },
        &Default::default(),
        &dialect,
    )
    .map_err(|error| unavailable(error.to_string()))?
    .changes
    .ordered(PlanMode::Transactional, &dialect);
    let lowered = pbps_dialect::lower::lower_all(
        &ordered as &dyn pbps_diff::order::PlanView<pbps_pg::Postgres>,
        &dialect,
    )
    .map_err(|error| unavailable(error.to_string()))?;
    let request = request(Driver::Postgres, base, desired_types, &hints, &[])
        .map_err(|error| Refused::Unavailable(ProduceError::Run(error)))?;
    let standard = standard.into();
    let mut target = Conn::connect(Driver::Postgres, target_connection)
        .await
        .map_err(|error| unavailable(format!("the vet's target session: {error}")))?;
    let tokens = Tokens::generate();
    let mut admin = engine::connect_scratch(Driver::Postgres, scratch, &tokens)
        .await
        .map_err(Refused::Unavailable)?;
    let (provisioning, mut state) = engine::scratch(
        Driver::Postgres,
        &mut target,
        &mut admin,
        &tokens,
        identity,
        scratch,
        &request,
        &standard,
        externals,
        provenance,
    )
    .await
    .map_err(Refused::Unavailable)?;
    let result = async {
        let read = |error: pbps_db::DbError| Refused::Unavailable(ProduceError::Run(Error::Read(error.to_string())));
        let run = |error| Refused::Unavailable(ProduceError::Run(error));
        let schemas = scope::scope_schemas(Driver::Postgres, &request.schemas, &[])
            .map_err(|why| run(Error::Scope(why)))?;
        let (catalog, authorization) = scope::read_target(
            &mut target, Driver::Postgres, &request.schemas, &[], &schemas, &[],
        ).await.map_err(read)?;
        let mut prepared = state.prepare_code(
            provisioning, &mut admin, &catalog, &authorization, &schemas,
        ).await.map_err(run)?;
        let report = scope::compare_reported(Driver::Postgres, &catalog, prepared.catalog())
            .map_err(|why| run(Error::Scope(why)))?;
        // A different engine version cannot give a verdict about these trees.
        // Missing extension types still reach CREATE, which names the absent type.
        let version = "server_version_num";
        if catalog.observations.get(version) != prepared.catalog().observations.get(version) {
            return Err(run(Error::Incompatible(vec![version.into()])));
        }
        let zone = target.session_zone().cloned().ok_or_else(|| {
            run(Error::Scope("the target reported no session time zone to vet in".into()))
        })?;
        let session = prepared.session(&mut admin);
        // Compile under the same pins as binding reconstruction and apply,
        // rather than the database/role defaults reproduced on scratch.
        // Session pins also survive the bound-tree reader's transactions.
        if let Some(pins) = dialect.session_pins(&zone) {
            session.execute(&pins).await.map_err(read)?;
        }
        for statement in lowered.groups().iter().flat_map(|g| g.statements()) {
            if let Err(error) = session.execute(&statement.sql).await {
                let Some(annotation) = &statement.declared_type else {
                    return Err(read(error));
                };
                let code = error.server_error_code();
                // Undefined functions are not proof of user code unless the
                // missing function is declared by this project. Other compile
                // failures are advisory; apply still owns the binding verdict.
                let managed = code.as_deref() == Some("42883") && desired.schema.modules.keys().any(|id| {
                    [format!("{}.{}(", id.schema(), id.name()), format!("function {}(", id.name())]
                        .iter().any(|name| error.to_string().contains(name))
                });
                let missing_type = code.as_deref() == Some("42704")
                    && error.to_string().contains("type ");
                if missing_type || (managed && matches!(report.verdict(), Verdict::Verified)) {
                    let note = if managed {
                        " The function is managed and has not been created by this types-only vet; apply also refuses user-written domain code."
                    } else { "" };
                    return Err(Refused::Code(format!(
                        "declared domain `{}` could not be created for {}: {error}.{note}",
                        annotation.name.shown(), expression_parts(annotation),
                    )));
                }
                return Err(read(error));
            }
            if matches!(report.verdict(), Verdict::Verified)
                && let Some(annotation) = &statement.declared_type
            {
                pbps_pg::type_refs::vet_code(session, pbps_pg::pins::Within::OwnTransaction,
                    std::slice::from_ref(&annotation.name)).await
                    .map_err(|error| match error {
                        pbps_pg::type_refs::CodeVetError::Refused(why) => Refused::Code(why),
                        pbps_pg::type_refs::CodeVetError::Unreadable(why) => run(Error::Read(why)),
                    })?;
            }
        }
        match report.verdict() {
            Verdict::Verified => Ok(()),
            Verdict::Mismatch(facts) => Err(run(Error::Incompatible(facts))),
            Verdict::Unknown(facts) => Err(run(Error::Scope(facts.join("; ")))),
        }
    }.await;
    let released = state.release(&mut admin).await;
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
