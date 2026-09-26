//! Routine pins: what `plan` seals about the routines it does not manage, and
//! what `apply` checks against it (DEC-319.1).
//!
//! The drift check covers the managed set. A routine outside it can still run
//! inside an apply — a CHECK the plan adds calls it, a probe evaluates it, a
//! trigger fires it — and its owner can replace it after approval. Nothing can
//! lock `pg_proc` for an account short of a superuser, so this detects rather
//! than prevents: every routine a non-superuser could replace is pinned by its
//! catalog row, and `apply` recomputes the pins before its probes, before its
//! first statement and before it records.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, bail};
use pbps_db::fingerprint::FingerprintKey;
use pbps_db::{Conn, Driver};
use pbps_model::{ModuleId, RoutineArg, RoutineId, RoutinePins, SchemaPin, TableName};

use crate::db::Target;
use pbps_config::Project;

/// The versioned rule every pin digest is made under.
const RULE: &str = "pbps/external-routine-pin/v1";

/// Where a check runs, which decides how it reads and what its refusal says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// Under the lock, before the pre-flight probes, with no transaction open.
    BeforeProbes,
    /// Inside the apply transaction, before its first statement.
    BeforeStatements,
    /// Inside the apply transaction, after the read-back and before `record`.
    BeforeRecording,
    /// A staged step, before its statement runs. No transaction is open.
    BeforeStep { step: usize, total: usize },
    /// A staged step, after its statement committed and before its checkpoint.
    AfterStep { step: usize, total: usize },
    /// A staged row write, inside its own transaction, after the statement
    /// and before the commit (#428).
    BeforeStepCommits { step: usize, total: usize },
    /// A staged run's closing read, before the ordinary entry that says it
    /// finished. A `--resume` with no step left reaches only this one.
    BeforeClosing,
}

impl Check {
    fn within(self) -> pbps_pg::pins::Within {
        match self {
            Check::BeforeStatements | Check::BeforeRecording | Check::BeforeStepCommits { .. } => {
                pbps_pg::pins::Within::CallersTransaction
            }
            Check::BeforeProbes
            | Check::BeforeStep { .. }
            | Check::AfterStep { .. }
            | Check::BeforeClosing => pbps_pg::pins::Within::OwnTransaction,
        }
    }

    fn when(self) -> String {
        match self {
            Check::BeforeProbes => "before the pre-flight probes".to_owned(),
            Check::BeforeStatements => "before the first statement".to_owned(),
            Check::BeforeRecording => "after the statements, before recording".to_owned(),
            Check::BeforeStep { step, total } => format!("before statement {step} of {total}"),
            Check::AfterStep { step, total } => {
                format!("after statement {step} of {total} committed")
            }
            Check::BeforeClosing => "before the staged run's closing entry".to_owned(),
            Check::BeforeStepCommits { step, total } => {
                format!("after statement {step} of {total} ran, before it committed")
            }
        }
    }
}

/// The pins `plan` writes into a PostgreSQL plan, or `None` when nothing is
/// in the set.
///
/// A non-empty set needs the environment's key. Without one the plan is
/// refused rather than written unpinned, because an unpinned plan is the
/// window this exists to close.
pub async fn seal(
    conn: &mut Conn,
    project: &Project,
    target: &Target,
    managed: &BTreeSet<ModuleId>,
) -> anyhow::Result<Option<RoutinePins>> {
    if conn.driver() != Driver::Postgres {
        return Ok(None);
    }
    let read = pbps_pg::pins::read(conn, pbps_pg::pins::Within::OwnTransaction)
        .await
        .context("could not read the routines this plan pins (DEC-319.1)")?;
    let schemas = unmanaged_by_schema(read.routines, managed);
    if schemas.is_empty() {
        return Ok(None);
    }
    let key = key(project, target).map_err(|why| {
        anyhow::anyhow!(
            "`{}` has routines outside the managed set that a role short of a superuser can \
             replace, in {}. A plan pins them against replacement until it has run \
             (DEC-319.1), under the environment's fingerprint key, and {why}.\nGenerate one \
             with `pbps key generate`, give it to this environment as `fingerprint_key_env` \
             or `fingerprint_key_file` in pbps.yml, and plan again.",
            target.label,
            schema_names(&schemas)
        )
    })?;
    Ok(Some(RoutinePins {
        key_id: key.id().as_str().to_owned(),
        server_version: read.server_version,
        schemas: digests(&key, &schemas),
    }))
}

/// Recomputes the pins and refuses any difference from the plan's.
///
/// `managed` is the union of every module the plan could hold at any point of
/// its run, so that a routine it creates, rebuilds or drops never reads as an
/// unmanaged one arriving or leaving.
pub async fn check(
    conn: &mut Conn,
    project: &Project,
    target: &Target,
    pins: Option<&RoutinePins>,
    managed: &BTreeSet<ModuleId>,
    at: Check,
) -> anyhow::Result<()> {
    if conn.driver() != Driver::Postgres {
        return Ok(());
    }
    let read = pbps_pg::pins::read(conn, at.within())
        .await
        .with_context(|| {
            format!(
                "could not read the routines this plan pins, {} (DEC-319.1)",
                at.when()
            )
        })?;
    let now = unmanaged_by_schema(read.routines, managed);
    let refuse = |what: String| -> anyhow::Error {
        anyhow::anyhow!(
            "{what}, found {} on `{}`.\nA routine this plan can run was changed after the plan \
             was approved (DEC-319.1). Look at what changed, then plan again with \
             `pbps plan --db`.",
            at.when(),
            target.label
        )
    };
    let Some(pins) = pins else {
        if now.is_empty() {
            return Ok(());
        }
        return Err(refuse(format!(
            "the plan pinned no routine, and {} now holds routines a role short of a \
             superuser can replace",
            schema_names(&now)
        )));
    };
    if read.server_version != pins.server_version {
        return Err(refuse(format!(
            "the plan was made against server version {}, and the server is now {}",
            pins.server_version, read.server_version
        )));
    }
    let key = key(project, target).map_err(|why| {
        anyhow::anyhow!(
            "this plan pins routines under fingerprint key {}, and {why}.\n\
             Give `{}` its key, or plan again under the key it has.",
            pins.key_id,
            target.label
        )
    })?;
    if key.id().as_str() != pins.key_id {
        bail!(
            "this plan pins routines under fingerprint key {}, and `{}` has key {}.\n\
             A pin made under one key is no evidence under another (DEC-952.1). Plan again \
             under this environment's key.",
            pins.key_id,
            target.label,
            key.id()
        );
    }
    let recomputed = digests(&key, &now);
    let changed = differing(&pins.schemas, &recomputed);
    if changed.is_empty() {
        return Ok(());
    }
    Err(refuse(format!(
        "pinned routines changed in {}",
        changed.join(", ")
    )))
}

/// Each schema's routines that the plan does not manage, as the canonical
/// inputs of their pins.
fn unmanaged_by_schema(
    routines: Vec<pbps_pg::pins::Routine>,
    managed: &BTreeSet<ModuleId>,
) -> BTreeMap<i64, (String, Vec<String>)> {
    let mut schemas: BTreeMap<i64, (String, Vec<String>)> = BTreeMap::new();
    for routine in routines {
        if is_managed(&routine, managed) {
            continue;
        }
        schemas
            .entry(routine.namespace)
            .or_insert_with(|| (routine.schema.clone(), Vec::new()))
            .1
            .push(routine.input);
    }
    schemas
}

/// Whether the plan manages this routine, by the identity the pull gives it.
///
/// An argument list that does not parse as the model's routine identity cannot
/// be a managed routine's, so it is pinned: a failure to match errs towards
/// pinning, never towards leaving out.
fn is_managed(routine: &pbps_pg::pins::Routine, managed: &BTreeSet<ModuleId>) -> bool {
    let Ok(args) = routine
        .args
        .iter()
        .map(|a| a.parse::<RoutineArg>())
        .collect::<Result<Vec<_>, _>>()
    else {
        return false;
    };
    managed.contains(&ModuleId::Routine(RoutineId::new(
        TableName::new(&routine.schema, &routine.name),
        args,
    )))
}

fn digests(key: &FingerprintKey, schemas: &BTreeMap<i64, (String, Vec<String>)>) -> Vec<SchemaPin> {
    schemas
        .iter()
        .map(|(namespace, (schema, inputs))| {
            let component = namespace.to_string();
            let mut parts: Vec<&[u8]> = vec![RULE.as_bytes(), component.as_bytes()];
            parts.extend(inputs.iter().map(|i| i.as_bytes()));
            let mac = key.mac(&parts);
            SchemaPin {
                namespace: *namespace,
                schema: schema.clone(),
                routines: inputs.len(),
                digest: mac.iter().map(|b| format!("{b:02x}")).collect(),
            }
        })
        .collect()
}

/// The schemas whose pins differ, named as the plan recorded them or, for one
/// the plan never saw, as the catalog has it now.
fn differing(planned: &[SchemaPin], now: &[SchemaPin]) -> Vec<String> {
    let planned: BTreeMap<i64, &SchemaPin> = planned.iter().map(|p| (p.namespace, p)).collect();
    let now: BTreeMap<i64, &SchemaPin> = now.iter().map(|p| (p.namespace, p)).collect();
    let mut changed = Vec::new();
    for namespace in planned.keys().chain(now.keys()).collect::<BTreeSet<_>>() {
        match (planned.get(namespace), now.get(namespace)) {
            (Some(p), Some(n)) if p.digest == n.digest && p.routines == n.routines => {}
            (Some(p), Some(n)) => changed.push(format!(
                "`{}` ({} routine(s) pinned, {} now)",
                p.schema, p.routines, n.routines
            )),
            (Some(p), None) => changed.push(format!(
                "`{}` (its {} pinned routine(s) are gone from the set)",
                p.schema, p.routines
            )),
            (None, Some(n)) => changed.push(format!(
                "`{}` ({} routine(s) the plan never pinned)",
                n.schema, n.routines
            )),
            (None, None) => unreachable!("the key came from one of the two maps"),
        }
    }
    changed
}

fn schema_names(schemas: &BTreeMap<i64, (String, Vec<String>)>) -> String {
    schemas
        .values()
        .map(|(schema, inputs)| format!("`{schema}` ({})", inputs.len()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The environment's fingerprint key, or a sentence saying why there is none.
fn key(project: &Project, target: &Target) -> Result<FingerprintKey, String> {
    let Some(environment) = target.environment() else {
        return Err(
            "a bare `--db` target names no environment and so no fingerprint key; connect \
             with `--env`"
                .to_owned(),
        );
    };
    let source = project
        .fingerprint_key_source(environment)
        .map_err(|e| format!("environment `{environment}`'s fingerprint key: {e}"))?;
    let loaded = match source {
        None => {
            return Err(format!(
                "environment `{environment}` configures no fingerprint key"
            ));
        }
        Some(pbps_config::FingerprintKeySource::Env(var)) => FingerprintKey::from_env(&var),
        Some(pbps_config::FingerprintKeySource::File(path)) => FingerprintKey::from_file(&path),
    };
    loaded.map_err(|e| format!("environment `{environment}`'s fingerprint key: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routine(
        namespace: i64,
        schema: &str,
        name: &str,
        args: &[&str],
        input: &str,
    ) -> pbps_pg::pins::Routine {
        pbps_pg::pins::Routine {
            namespace,
            schema: schema.to_owned(),
            name: name.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            input: input.to_owned(),
        }
    }

    fn key(byte: u8) -> FingerprintKey {
        FingerprintKey::parse(&base64_of(&[byte; 32]), "test").unwrap()
    }

    fn base64_of(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn pins(key: &FingerprintKey, routines: Vec<pbps_pg::pins::Routine>) -> Vec<SchemaPin> {
        digests(key, &unmanaged_by_schema(routines, &BTreeSet::new()))
    }

    #[test]
    fn a_managed_routine_is_left_to_the_managed_comparison() {
        let managed: BTreeSet<ModuleId> = [ModuleId::Routine(RoutineId::new(
            TableName::new("app", "f"),
            vec!["integer".parse().unwrap()],
        ))]
        .into();
        let schemas = unmanaged_by_schema(
            vec![
                routine(1, "app", "f", &["integer"], "managed"),
                routine(
                    1,
                    "app",
                    "f",
                    &["text"],
                    "an overload the plan does not manage",
                ),
            ],
            &managed,
        );
        assert_eq!(schemas[&1].1, vec!["an overload the plan does not manage"]);
    }

    #[test]
    fn an_argument_list_that_does_not_parse_is_pinned_rather_than_taken_for_managed() {
        let managed: BTreeSet<ModuleId> = [ModuleId::Routine(RoutineId::new(
            TableName::new("app", "f"),
            vec![],
        ))]
        .into();
        let odd = routine(1, "app", "f", &[""], "unparseable");
        assert!(!is_managed(&odd, &managed));
    }

    #[test]
    fn any_change_to_a_routine_input_changes_its_schemas_pin_and_no_other() {
        let k = key(7);
        let before = pins(
            &k,
            vec![
                routine(1, "a", "f", &[], "one"),
                routine(2, "b", "g", &[], "two"),
            ],
        );
        let after = pins(
            &k,
            vec![
                routine(1, "a", "f", &[], "one"),
                routine(2, "b", "g", &[], "TWO"),
            ],
        );
        assert_eq!(
            differing(&before, &after),
            vec!["`b` (1 routine(s) pinned, 1 now)"]
        );
        assert!(differing(&before, &before).is_empty());
    }

    #[test]
    fn a_routine_arriving_or_leaving_is_a_change() {
        let k = key(7);
        let one = pins(&k, vec![routine(1, "a", "f", &[], "one")]);
        let two = pins(
            &k,
            vec![
                routine(1, "a", "f", &[], "one"),
                routine(1, "a", "g", &[], "two"),
            ],
        );
        assert_eq!(differing(&one, &two).len(), 1);
        let elsewhere = pins(
            &k,
            vec![
                routine(1, "a", "f", &[], "one"),
                routine(3, "c", "h", &[], "new"),
            ],
        );
        assert_eq!(
            differing(&one, &elsewhere),
            vec!["`c` (1 routine(s) the plan never pinned)"]
        );
        assert_eq!(
            differing(&one, &[]),
            vec!["`a` (its 1 pinned routine(s) are gone from the set)"]
        );
    }

    #[test]
    fn moving_the_boundary_between_two_routine_inputs_changes_the_pin() {
        let k = key(7);
        let split = pins(
            &k,
            vec![
                routine(1, "a", "f", &[], "ab"),
                routine(1, "a", "g", &[], "c"),
            ],
        );
        let moved = pins(
            &k,
            vec![
                routine(1, "a", "f", &[], "a"),
                routine(1, "a", "g", &[], "bc"),
            ],
        );
        assert_ne!(split[0].digest, moved[0].digest);
    }

    #[test]
    fn the_same_routines_under_another_key_give_another_pin() {
        let routines = || vec![routine(1, "a", "f", &[], "one")];
        assert_ne!(
            pins(&key(7), routines())[0].digest,
            pins(&key(8), routines())[0].digest
        );
        assert_eq!(pins(&key(7), routines()), pins(&key(7), routines()));
    }
}
