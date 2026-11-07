//! The scratch session of a connected rehearsal (#2379).
//!
//! A rehearsal acquires the same scratch the resolver does, with the same
//! separation, layout, emptiness and externals checks, and releases it the
//! same way. What it does not do is qualify the scratch against the target or
//! seal anything: it asks whether the plan's statements run, in order, on a
//! reproduction of the target's managed state, and nothing it reads leaves
//! as evidence. So no fingerprint key is needed and no compatibility verdict
//! is reached; the scratch is the operator's choice, and a rehearsal on
//! another build proves syntax and convergence, never the build's behaviour.

use super::super::producer::{request, request_as_it_stands};
use super::super::{BindingRequest, Error, ProduceError, engine};
use super::{Provenance, Tokens};
use crate::resolver::scope;
use pbps_db::{Conn, Driver};
use pbps_model::resolver::OutsideWrites;
use pbps_model::resolver::ScratchLayout;
use pbps_model::role_mapping::RoleMapping;
use pbps_model::{Hints, RoleName};
use std::collections::BTreeSet;

/// The roles the plan names, as the run can have them: under their own names
/// where the scratch is the target's cluster or a cluster of the run's own, and
/// under names of the run's own on a cluster other runs use (`Naming`). `Err`
/// is why a rehearsal there cannot run the plan's authorization, and says so.
pub type Roles = Result<RoleMapping, String>;

/// Runs `run` on a session of the scratch as the target's deployer, with the
/// entry's externals staged and compared with the target, and releases the
/// scratch whatever `run` returns. The layout the run used comes back beside
/// the result: it is the run's own finding, as for the resolver.
///
/// `named` are the roles the plan, or the base it builds, names. How they are
/// named on this scratch is its placement's (`Naming`): the target's own cluster
/// already has them; a container gets those it lacks, under their names; a
/// cluster others use gets a role of the run's own for each. What the run makes
/// is recorded before it is made and removed with the scratch, and `run` is told
/// which names the roles have; or, when it cannot rehearse them, why not.
#[allow(clippy::too_many_arguments)]
pub async fn with_session<T, E>(
    driver: Driver,
    scratch: &str,
    target_connection: &str,
    planned_identity: &str,
    binding: &BindingRequest<'_>,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &Hints,
    standard: impl Into<engine::Standard>,
    externals: Option<&str>,
    provenance: &Provenance,
    named: &BTreeSet<RoleName>,
    run: impl AsyncFnOnce(&mut Conn, Option<Conn>, Roles) -> Result<T, E>,
) -> Result<(ScratchLayout, Result<T, E>), ProduceError> {
    // The write path's extras are the dialect's, and the CLI's PostgreSQL
    // dialect configures none, as for the resolver.
    let request = match driver {
        Driver::Postgres => {
            request(driver, base, desired, hints, &[]).map_err(ProduceError::Run)?
        }
        Driver::Mssql => request_as_it_stands(base, desired, &[]),
    };
    let standard = standard.into();
    // Every connection of the run opens the one endpoint found here
    // (DEC-2271.2), as for the resolver.
    let scratch = &engine::endpoint(driver, scratch).await?;
    let mut target = Conn::connect(driver, target_connection)
        .await
        .map_err(|error| {
            ProduceError::Run(Error::Unusable(format!(
                "the target could not be connected: {error}"
            )))
        })?;
    // The token first: the scratch session carries it from its first packet,
    // and a killed rehearsal is recovered by it (#2410).
    let tokens = Tokens::generate();
    let mut admin = engine::connect_scratch(driver, scratch, &tokens).await?;
    let (provisioning, mut state) = engine::scratch(
        driver,
        &mut target,
        &mut admin,
        &tokens,
        planned_identity,
        scratch,
        &request,
        &standard,
        externals,
        provenance,
    )
    .await?;
    let opened = async {
        let db = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        let scope_schemas =
            scope::scope_schemas(driver, &request.schemas, &request.write_path_extras)
                .map_err(Error::Scope)?;
        let (target_catalog, authorization) = scope::read_target(
            &mut target,
            driver,
            &request.schemas,
            &request.write_path_extras,
            &scope_schemas,
            &request.planned,
        )
        .await
        .map_err(db("the target's analysis scope"))?;
        let ambiguous = authorization.unpredictable(&request.planned);
        if !ambiguous.is_empty() {
            return Err(Error::Scope(format!(
                "the grantor of a planned grant cannot be predicted on the target: {}",
                ambiguous.join("; ")
            )));
        }
        // SQL Server's captures fingerprint what they read, and a rehearsal
        // compares in memory and seals nothing: the key is the run's own.
        let key = (driver == Driver::Mssql)
            .then(pbps_db::fingerprint::EnvironmentFingerprintKey::for_one_run);
        let mut prepared = state
            .prepare(
                provisioning,
                &mut admin,
                &mut target,
                &target_catalog,
                &authorization,
                &scope_schemas,
                binding,
                &request.write_path_extras,
                key.as_ref(),
            )
            .await?;
        // The externals the base and the plan read are the target's, as for
        // the resolver: a file that stages something else would rehearse a
        // plan on a state the target does not hold. The comparison needs the
        // resolver's reconstruction, which says which target objects are
        // managed, so a rehearsal with nothing staged never builds it.
        if driver == Driver::Mssql {
            // The base is the target's own managed namespace, compiled the way
            // the resolver compiles the declarations: through the engine's
            // creation trials, because a default or computed column that calls
            // a function the base also holds has no order the plain bootstrap
            // can write (DEC-2379.3). Its externals were staged and compared
            // with the target by `prepare`.
            let mut built = state
                .reconstruction(
                    &mut target,
                    binding,
                    base.ids,
                    &hints.module_deps,
                    &mut prepared,
                )
                .await?;
            let mut staged = prepared.completed_staged().ok_or_else(|| {
                Error::Scope("the prepared scratch holds no staged inventory".into())
            })?;
            built
                .compile(
                    &mut prepared,
                    &mut admin,
                    &mut target,
                    &[],
                    None,
                    OutsideWrites::Refused,
                    &mut staged,
                )
                .await?;
        } else if externals.is_some() || prepared.has_filled() {
            let mut reconstruction = state
                .reconstruction(
                    &mut target,
                    binding,
                    desired.ids,
                    &hints.module_deps,
                    &mut prepared,
                )
                .await?;
            if reconstruction.has_fill() {
                return Err(Error::Externals(vec![
                    "the externals fill reads managed relations, which a rehearsal cannot \
                     create between its statements; turn the rehearsal off with \
                     `rehearse: false` or `--no-rehearse`"
                        .into(),
                ]));
            }
            reconstruction.chains(&prepared);
            state
                .compare_externals(
                    provisioning,
                    &mut prepared,
                    &mut admin,
                    &mut target,
                    &reconstruction,
                )
                .await?;
        }
        let reader = prepared.reader().await?;
        let roles = if driver == Driver::Mssql {
            principals(named, base.schema, desired.schema)
        } else {
            roles(&mut state, &mut admin, named, &tokens).await?
        };
        // PostgreSQL's prepared scratch knows it holds a supplied database;
        // SQL Server's layout is the provisioning the acquisition chose, as for
        // the resolver's evidence.
        let layout = match provisioning {
            super::Provisioning::Supplied => ScratchLayout::Supplied,
            super::Provisioning::RunOwned if prepared.supplied() => ScratchLayout::Supplied,
            super::Provisioning::RunOwned => ScratchLayout::RunOwned,
        };
        let session = prepared.deployer(&mut admin);
        Ok((layout, run(session, reader, roles).await))
    }
    .await;
    let released = state.release(&mut admin).await;
    drop(target);
    match (opened, released) {
        (_, Err(names)) => Err(ProduceError::Cleanup(names)),
        (Err(refused), Ok(())) => Err(ProduceError::Run(refused)),
        (Ok(done), Ok(())) => Ok(done),
    }
}

/// The roles of the run, by the naming its placement decides. Here and in the
/// one place the plan is lowered again (`OrderedPlan::under_role_names`) is all
/// the code that tells the three apart.
async fn roles(
    state: &mut engine::Scratch<'_>,
    admin: &mut Conn,
    named: &BTreeSet<RoleName>,
    tokens: &Tokens,
) -> Result<Roles, Error> {
    // The engine's own roles keep their names: they are the scratch's too.
    let own = |role: &RoleName| role.raw().starts_with("pg_");
    let naming = state.naming()?;
    let mapping = match naming {
        engine::Naming::Existing => RoleMapping::default(),
        engine::Naming::Created => {
            RoleMapping::unchanged(named.iter().filter(|role| !own(role)).cloned())
        }
        engine::Naming::Mapped => {
            RoleMapping::generate(named, "pbps_rehearsal", &tokens.scratch, own)
        }
    };
    Ok(
        match state
            .rehearsal_roles(admin, mapping.created(), naming)
            .await?
        {
            Ok(()) => Ok(mapping),
            Err(why) => Err(why),
        },
    )
}

/// The principals a SQL Server rehearsal's statements name, as the scratch
/// can hold them: every one is a role the declarations hold (the base creates
/// it) or one every database holds.
///
/// pbps declares roles and the grants to them; users and logins stay with the
/// environment (ADR-0005), and no statement it writes names one. Roles and
/// users are database-scoped, so the scratch database holds the target's own
/// role names whatever instance it is on, and nothing is made on the instance
/// (measured: a confined login makes the same names in two databases at once,
/// and dropping the database leaves the server's principals as they were).
/// A login is a server principal; a rehearsal never creates, alters or names
/// one (DEC-2379.3). A statement that did name a principal the declarations do
/// not hold would run against a user or login the scratch does not have and
/// fail for a reason that says nothing about the plan, so such a plan is not
/// rehearsed for authorization and the record says why.
fn principals(
    named: &BTreeSet<RoleName>,
    base: &pbps_model::Schema,
    desired: &pbps_model::Schema,
) -> Roles {
    let missing = undeclared(named, base, desired);
    if missing.is_empty() {
        Ok(RoleMapping::default())
    } else {
        Err(format!(
            "the plan names {}, which the declarations do not hold: a user or login belongs to \
             the environment, and a rehearsal makes none",
            missing
                .iter()
                .map(|name| format!("`{}`", name.shown()))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// The principals `named` that are neither declared in `base` or `desired` nor
/// built into every database.
fn undeclared<'a>(
    named: &'a BTreeSet<RoleName>,
    base: &pbps_model::Schema,
    desired: &pbps_model::Schema,
) -> Vec<&'a RoleName> {
    named
        .iter()
        .filter(|name| {
            !base.roles.contains_key(*name)
                && !desired.roles.contains_key(*name)
                && !builtin(name.raw())
        })
        .collect()
}

/// The fixed database roles, which every database holds.
const FIXED_ROLES: [&str; 9] = [
    "db_owner",
    "db_accessadmin",
    "db_securityadmin",
    "db_ddladmin",
    "db_backupoperator",
    "db_datareader",
    "db_datawriter",
    "db_denydatareader",
    "db_denydatawriter",
];

/// A principal every database holds: it is never made, and never mapped.
/// Compared as the engine compares a principal, ignoring case, and by the whole
/// name: a `db_` prefix is not the fixed roles (a user may name a role
/// `db_app_reader`, which a scratch does not have).
fn builtin(name: &str) -> bool {
    ["dbo", "public", "guest", "sys", "INFORMATION_SCHEMA"]
        .iter()
        .chain(FIXED_ROLES.iter())
        .any(|own| own.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod principals_tests {
    use super::*;

    fn role(name: &str) -> RoleName {
        name.into()
    }

    fn holding(roles: &[&str]) -> pbps_model::Schema {
        let mut schema = pbps_model::Schema::default();
        for name in roles {
            schema
                .roles
                .insert(role(name), pbps_model::role::Role::default());
        }
        schema
    }

    #[test]
    fn declared_and_built_in_principals_are_held_and_a_user_is_not() {
        let named: BTreeSet<_> = [
            "reader",
            "later",
            "dbo",
            "PUBLIC",
            "db_datareader",
            "app_user",
        ]
        .map(role)
        .into();
        let missing = undeclared(&named, &holding(&["reader"]), &holding(&["later"]));
        assert_eq!(missing, vec![&role("app_user")]);
        assert!(principals(&named, &holding(&["reader"]), &holding(&["later"])).is_err());
    }

    #[test]
    fn nothing_to_hold_is_a_rehearsal_under_the_targets_own_names() {
        let named: BTreeSet<_> = ["reader", "dbo"].map(role).into();
        assert!(principals(&named, &holding(&["reader"]), &holding(&[])).is_ok());
        assert!(principals(&BTreeSet::new(), &holding(&[]), &holding(&[])).is_ok());
    }

    /// The names are compared as the engine compares a principal, so a name
    /// that differs only in case from a built-in is the built-in's. Only the
    /// fixed roles are built in, not everything that starts with `db_`.
    #[test]
    fn a_name_that_only_looks_built_in_is_not() {
        assert!(builtin("DBO") && builtin("db_owner") && builtin("Guest"));
        assert!(builtin("DB_DATAREADER") && builtin("Db_DenyDataWriter"));
        assert!(!builtin("dbo_reader") && !builtin("database_user") && !builtin("sysadmin"));
        assert!(!builtin("db_app_reader") && !builtin("db_") && !builtin("db_owner2"));
        let named: BTreeSet<_> = ["db_app_reader", "DB_DATAREADER"].map(role).into();
        assert_eq!(
            undeclared(&named, &holding(&[]), &holding(&[])),
            vec![&role("db_app_reader")]
        );
    }
}
