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
use pbps_model::escape::Escaped;
use pbps_model::{ModuleId, RoutineArg, RoutineId, RoutinePin, RoutinePins, TableName};

use crate::db::Target;
use pbps_config::Project;

/// The versioned rule every pin digest is made under. v2 is one digest per
/// routine (DEC-2333.1); a v1 digest covered a schema's routines together.
const RULE: &str = "pbps/external-routine-pin/v2";

/// Where a check runs, which decides how it reads and what its refusal says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check<'a> {
    /// Under the lock, before the pre-flight probes, with no transaction open.
    BeforeProbes,
    /// Inside the apply transaction, before a statement.
    BeforeStatements {
        executed: &'a [pbps_dialect::Statement],
    },
    /// Inside the apply transaction, after the read-back and before `record`.
    BeforeRecording {
        executed: &'a [pbps_dialect::Statement],
    },
    /// After recording, still inside the transaction, before COMMIT.
    BeforeCommit {
        executed: &'a [pbps_dialect::Statement],
    },
    /// A staged step, before its statement runs. No transaction is open.
    BeforeStep {
        step: usize,
        total: usize,
        executed: &'a [pbps_dialect::Statement],
    },
    /// A staged step, after its statement committed and before its checkpoint.
    AfterStep {
        step: usize,
        total: usize,
        executed: &'a [pbps_dialect::Statement],
    },
    /// A staged row write, inside its own transaction, after the statement
    /// and before the commit (#428).
    BeforeStepCommits {
        step: usize,
        total: usize,
        executed: &'a [pbps_dialect::Statement],
    },
    /// A staged run's closing read, before the ordinary entry that says it
    /// finished. A `--resume` with no step left reaches only this one.
    BeforeClosing {
        executed: &'a [pbps_dialect::Statement],
    },
}

impl<'a> Check<'a> {
    fn within(self) -> pbps_pg::pins::Within {
        match self {
            Check::BeforeStatements { .. }
            | Check::BeforeRecording { .. }
            | Check::BeforeCommit { .. }
            | Check::BeforeStepCommits { .. } => pbps_pg::pins::Within::CallersTransaction,
            Check::BeforeProbes
            | Check::BeforeStep { .. }
            | Check::AfterStep { .. }
            | Check::BeforeClosing { .. } => pbps_pg::pins::Within::OwnTransaction,
        }
    }

    fn executed(self) -> &'a [pbps_dialect::Statement] {
        match self {
            Check::BeforeProbes => &[],
            Check::BeforeStatements { executed }
            | Check::BeforeRecording { executed }
            | Check::BeforeCommit { executed }
            | Check::BeforeClosing { executed }
            | Check::BeforeStep { executed, .. }
            | Check::AfterStep { executed, .. }
            | Check::BeforeStepCommits { executed, .. } => executed,
        }
    }

    fn when(self) -> String {
        match self {
            Check::BeforeProbes => "before the pre-flight probes".to_owned(),
            Check::BeforeStatements { .. } => "before a statement".to_owned(),
            Check::BeforeRecording { .. } => "after the statements, before recording".to_owned(),
            Check::BeforeCommit { .. } => "before COMMIT".to_owned(),
            Check::BeforeStep { step, total, .. } => format!("before statement {step} of {total}"),
            Check::AfterStep { step, total, .. } => {
                format!("after statement {step} of {total} committed")
            }
            Check::BeforeClosing { .. } => "before the staged run's closing entry".to_owned(),
            Check::BeforeStepCommits { step, total, .. } => {
                format!("after statement {step} of {total} ran, before it committed")
            }
        }
    }
}

/// The recorded scope, independent of the saved plan's optional pin fields.
pub struct Managed {
    pub modules: BTreeSet<ModuleId>,
    pub references: BTreeSet<pbps_model::TypeRef>,
    pub declared_types: BTreeSet<pbps_model::Uid>,
}

/// Preserve external requirements independently of same-named lifecycle steps.
pub(crate) fn external_references(
    schema: &pbps_model::Schema,
    ids: &pbps_model::IdsFile,
) -> BTreeSet<pbps_model::TypeRef> {
    use pbps_diff::identity::type_refs::{TypeIdentity, TypeResolver, TypeSide};
    let resolver = TypeResolver::new(ids, ids);
    crate::engine::type_references(schema)
        .into_iter()
        .filter(|r| {
            matches!(
                resolver.resolve(TypeSide::Base, r),
                TypeIdentity::ExternalOrUnrecorded(_)
            )
        })
        .collect()
}

pub(crate) fn declared_requirements(
    schema: &pbps_model::Schema,
    ids: &pbps_model::IdsFile,
) -> BTreeSet<pbps_model::Uid> {
    use pbps_diff::identity::type_refs::{TypeIdentity, TypeResolver, TypeSide};
    let resolver = TypeResolver::new(ids, ids);
    crate::engine::type_references(schema)
        .iter()
        .chain(schema.types.keys())
        .filter_map(|r| match resolver.resolve(TypeSide::Base, r) {
            TypeIdentity::Declared(uid) => Some(uid.clone()),
            TypeIdentity::ExternalOrUnrecorded(_) => None,
        })
        .collect()
}

fn require_declared_types(
    required: &BTreeSet<pbps_model::Uid>,
    pins: &[pbps_model::DeclaredTypePin],
    changes: &pbps_model::ChangeSet,
) -> anyhow::Result<()> {
    for uid in required {
        if !pins.iter().any(|p| &p.uid == uid)
            && !changes.changes.iter().any(|s| matches!(&s.change, pbps_model::Change::DropType { uid: dropped, .. } if dropped == uid))
        {
            bail!("plan has no declared type pin for uid `{uid}`; plan again against the database");
        }
    }
    Ok(())
}

/// The approved artifact and its environment, borrowed at execution boundaries.
pub struct Approved<'a> {
    pub project: &'a Project,
    pub target: &'a Target,
    pub plan: &'a pbps_model::SavedPlan,
    pub managed: &'a Managed,
}

impl Approved<'_> {
    pub async fn check(&self, conn: &mut Conn, at: Check<'_>) -> anyhow::Result<()> {
        check(conn, self.project, self.target, self.plan, self.managed, at).await
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
    let routines = unmanaged(read.routines, managed);
    if routines.is_empty() {
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
            schema_names(&routines)
        )
    })?;
    Ok(Some(RoutinePins {
        key_id: key.id().as_str().to_owned(),
        server_version: read.server_version,
        routines: digests(&key, &routines),
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
    plan: &pbps_model::SavedPlan,
    managed: &Managed,
    at: Check<'_>,
) -> anyhow::Result<()> {
    if conn.driver() == Driver::Postgres {
        require_declared_types(
            &managed.declared_types,
            &plan.declared_type_pins,
            &plan.changes,
        )?;
        pbps_pg::type_refs::declared::check(
            conn,
            &plan.declared_type_pins,
            &project.config.trusted_roles,
            at.within(),
            matches!(
                at,
                Check::BeforeRecording { .. }
                    | Check::BeforeCommit { .. }
                    | Check::BeforeClosing { .. }
            ),
            &plan.changes,
            at.executed(),
        )
        .await?;
    }
    crate::engine::check_type_pins(
        conn,
        plan,
        &project.config.trusted_roles,
        &managed.references,
        matches!(at.within(), pbps_pg::pins::Within::CallersTransaction),
    )
    .await?;
    check_selected(conn, project, target, plan, at).await?;
    let pins = plan.routine_pins.as_ref();
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
    let now = unmanaged(read.routines, &managed.modules);
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
             superuser can replace: {}",
            schema_names(&now),
            Differences {
                added: now.values().map(|r| r.signature.clone()).collect(),
                ..Differences::default()
            }
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
    let changed = differing(&pins.routines, &digests(&key, &now));
    if changed.is_empty() {
        return Ok(());
    }
    Err(refuse(format!("pinned routines changed: {changed}")))
}

/// Seal reviewed roots with the same canonical catalog rows and HMAC as
/// DEC-319.1, including roots excluded from that rule's broad replaceable set.
pub async fn seal_selected(
    conn: &mut Conn,
    project: &Project,
    target: &Target,
    types: &[pbps_model::TypePin],
) -> anyhow::Result<(Vec<pbps_model::TypeRoutine>, Option<RoutinePins>)> {
    let roots = crate::engine::type_input_routines(
        conn,
        types,
        &project.config.trusted_roles,
        project.config.pin_type_routines,
        false,
    )
    .await?;
    if roots.is_empty() {
        return Ok((Vec::new(), None));
    }
    let (read, sources) = pbps_pg::pins::selected(
        conn,
        pbps_pg::pins::Within::OwnTransaction,
        &roots.into_iter().collect::<Vec<_>>(),
    )
    .await?;
    let key = key(project, target).map_err(anyhow::Error::msg)?;
    let pins = RoutinePins {
        key_id: key.id().as_str().to_owned(),
        server_version: read.server_version,
        routines: digests(&key, &unmanaged(read.routines, &BTreeSet::new())),
    };
    Ok((sources, Some(pins)))
}

async fn check_selected(
    conn: &mut Conn,
    project: &Project,
    target: &Target,
    plan: &pbps_model::SavedPlan,
    at: Check<'_>,
) -> anyhow::Result<()> {
    if project.config.pin_type_routines != plan.pin_type_routines {
        bail!("pin_type_routines changed since planning; plan again against the database");
    }
    let roots = crate::engine::type_input_routines(
        conn,
        &plan.type_pins,
        &project.config.trusted_roles,
        plan.pin_type_routines,
        matches!(at.within(), pbps_pg::pins::Within::CallersTransaction),
    )
    .await?;
    let expected: BTreeSet<_> = plan.type_routines.iter().map(|r| r.oid).collect();
    if roots != expected || expected.len() != plan.type_routines.len() {
        bail!("the referenced types' reviewed routine set changed; plan again (DEC-2181.2)");
    }
    let Some(pins) = &plan.type_routine_pins else {
        if roots.is_empty() {
            return Ok(());
        }
        bail!("the plan has no reviewed type routine pins; plan again");
    };
    let (read, sources) =
        pbps_pg::pins::selected(conn, at.within(), &roots.into_iter().collect::<Vec<_>>()).await?;
    let key = key(project, target).map_err(anyhow::Error::msg)?;
    let now = digests(&key, &unmanaged(read.routines, &BTreeSet::new()));
    let changed = differing(&pins.routines, &now);
    if pins.key_id != key.id().as_str()
        || pins.server_version != read.server_version
        || !changed.is_empty()
        || sources != plan.type_routines
    {
        let which = if changed.is_empty() {
            String::new()
        } else {
            format!(": {changed}")
        };
        bail!(
            "reviewed type routines changed{which}, found {} on `{}`; plan again (DEC-2181.2)",
            at.when(),
            target.label
        );
    }
    Ok(())
}

/// Full sources are review data; terminal rendering escapes control characters.
pub const TYPE_ROUTINE_APPROVAL: &str = "Approving this plan approves the shown routines, whatever they call as those routines stand at plan time, and any SQL they build from data that pbps cannot show.";

pub fn reviewed_sources(routines: &[pbps_model::TypeRoutine]) -> String {
    if routines.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "{TYPE_ROUTINE_APPROVAL}\nPlan-time probes requiring these routines are unchecked until source review; apply checks them.\n"
    );
    for routine in routines {
        out.push_str(&format!(
            "{}\n",
            pbps_model::escape::Escaped(&routine.definition)
        ));
    }
    out
}

/// One routine in the pin set, ready to be digested.
struct Unmanaged {
    schema: String,
    /// Schema-qualified name and argument types, for messages (see
    /// [`signature`]).
    signature: String,
    /// The canonical catalog row the digest is made over.
    input: String,
}

/// The routines that the plan does not manage, keyed by `pg_proc.oid`.
fn unmanaged(
    routines: Vec<pbps_pg::pins::Routine>,
    managed: &BTreeSet<ModuleId>,
) -> BTreeMap<i64, Unmanaged> {
    routines
        .into_iter()
        .filter(|routine| !is_managed(routine, managed))
        .map(|routine| {
            (
                routine.oid,
                Unmanaged {
                    signature: signature(&routine),
                    schema: routine.schema,
                    input: routine.input,
                },
            )
        })
        .collect()
}

/// The routine's identity as the model spells it, so that overloads differ and
/// a name is escaped once, where it is shown. An argument list the model's
/// identity cannot hold still gets a signature, joined as the catalog gave it.
fn signature(routine: &pbps_pg::pins::Routine) -> String {
    match routine_id(routine) {
        Some(id) => id.raw().to_string(),
        None => format!(
            "{}.{}({})",
            routine.schema,
            routine.name,
            routine.args.join(",")
        ),
    }
}

fn routine_id(routine: &pbps_pg::pins::Routine) -> Option<RoutineId> {
    let args = routine
        .args
        .iter()
        .map(|a| a.parse::<RoutineArg>())
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some(RoutineId::new(
        TableName::new(&routine.schema, &routine.name),
        args,
    ))
}

/// Whether the plan manages this routine, by the identity the pull gives it.
///
/// An argument list that does not parse as the model's routine identity cannot
/// be a managed routine's, so it is pinned: a failure to match errs towards
/// pinning, never towards leaving out.
fn is_managed(routine: &pbps_pg::pins::Routine, managed: &BTreeSet<ModuleId>) -> bool {
    routine_id(routine).is_some_and(|id| managed.contains(&ModuleId::Routine(id)))
}

/// One digest per routine. The OID is a component beside the input, which
/// carries it too, so a digest cannot be moved to another entry.
fn digests(key: &FingerprintKey, routines: &BTreeMap<i64, Unmanaged>) -> BTreeMap<i64, RoutinePin> {
    routines
        .iter()
        .map(|(oid, routine)| {
            let component = oid.to_string();
            let mac = key.mac(&[
                RULE.as_bytes(),
                component.as_bytes(),
                routine.input.as_bytes(),
            ]);
            (
                *oid,
                RoutinePin {
                    signature: routine.signature.clone(),
                    digest: mac.iter().map(|b| format!("{b:02x}")).collect(),
                },
            )
        })
        .collect()
}

/// What differs between the pinned routines and the ones found now.
#[derive(Debug, Default, PartialEq, Eq)]
struct Differences {
    /// Pinned, present, and digested differently.
    changed: Vec<String>,
    /// Present now and never pinned.
    added: Vec<String>,
    /// Pinned and gone from the set.
    removed: Vec<String>,
}

impl Differences {
    fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }
}

impl std::fmt::Display for Differences {
    /// Every routine, named as the plan recorded it or, for one the plan never
    /// saw, as the catalog has it now. A signature is shown escaped.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        for (what, names) in [
            ("changed", &self.changed),
            ("added", &self.added),
            ("removed", &self.removed),
        ] {
            if !names.is_empty() {
                let names: Vec<_> = names.iter().map(|n| format!("`{}`", Escaped(n))).collect();
                parts.push(format!("{what} {}", names.join(", ")));
            }
        }
        f.write_str(&parts.join("; "))
    }
}

fn differing(planned: &BTreeMap<i64, RoutinePin>, now: &BTreeMap<i64, RoutinePin>) -> Differences {
    let mut found = Differences::default();
    for (oid, pin) in planned {
        match now.get(oid) {
            Some(current) if current.digest == pin.digest => {}
            Some(_) => found.changed.push(pin.signature.clone()),
            None => found.removed.push(pin.signature.clone()),
        }
    }
    for (oid, current) in now {
        if !planned.contains_key(oid) {
            found.added.push(current.signature.clone());
        }
    }
    found
}

/// The schemas holding pinned routines, with a count each.
fn schema_names(routines: &BTreeMap<i64, Unmanaged>) -> String {
    let mut schemas: BTreeMap<&str, usize> = BTreeMap::new();
    for routine in routines.values() {
        *schemas.entry(&routine.schema).or_default() += 1;
    }
    schemas
        .into_iter()
        .map(|(schema, n)| format!("`{}` ({n})", Escaped(schema)))
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
        oid: i64,
        schema: &str,
        name: &str,
        args: &[&str],
        input: &str,
    ) -> pbps_pg::pins::Routine {
        pbps_pg::pins::Routine {
            oid,
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

    fn pins(
        key: &FingerprintKey,
        routines: Vec<pbps_pg::pins::Routine>,
    ) -> BTreeMap<i64, RoutinePin> {
        digests(key, &unmanaged(routines, &BTreeSet::new()))
    }

    fn named(planned: &BTreeMap<i64, RoutinePin>, now: &BTreeMap<i64, RoutinePin>) -> String {
        differing(planned, now).to_string()
    }

    #[test]
    fn a_managed_routine_is_left_to_the_managed_comparison() {
        let managed: BTreeSet<ModuleId> = [ModuleId::Routine(RoutineId::new(
            TableName::new("app", "f"),
            vec!["integer".parse().unwrap()],
        ))]
        .into();
        let found = unmanaged(
            vec![
                routine(1, "app", "f", &["integer"], "managed"),
                routine(
                    2,
                    "app",
                    "f",
                    &["text"],
                    "an overload the plan does not manage",
                ),
            ],
            &managed,
        );
        assert_eq!(found.keys().collect::<Vec<_>>(), [&2]);
        assert_eq!(found[&2].input, "an overload the plan does not manage");
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
        assert_eq!(unmanaged(vec![odd], &managed)[&1].signature, "app.f()");
    }

    #[test]
    fn one_changed_routine_among_several_is_the_only_one_named() {
        let k = key(7);
        let before = pins(
            &k,
            vec![
                routine(1, "ext", "f", &[], "one"),
                routine(2, "ext", "g", &[], "two"),
                routine(3, "other", "h", &[], "three"),
            ],
        );
        let after = pins(
            &k,
            vec![
                routine(1, "ext", "f", &[], "one"),
                routine(2, "ext", "g", &[], "TWO"),
                routine(3, "other", "h", &[], "three"),
            ],
        );
        assert_eq!(named(&before, &after), "changed `ext.g()`");
        assert!(differing(&before, &before).is_empty());
    }

    #[test]
    fn an_added_and_a_removed_routine_are_each_named() {
        let k = key(7);
        let one = pins(&k, vec![routine(1, "a", "f", &[], "one")]);
        let two = pins(
            &k,
            vec![
                routine(1, "a", "f", &[], "one"),
                routine(2, "a", "g", &[], "two"),
            ],
        );
        assert_eq!(named(&one, &two), "added `a.g()`");
        assert_eq!(named(&two, &one), "removed `a.g()`");
        assert_eq!(
            named(&one, &BTreeMap::new()),
            "removed `a.f()`",
            "a schema that lost its last routine still names it"
        );
        let elsewhere = pins(
            &k,
            vec![
                routine(1, "a", "f", &[], "one"),
                routine(3, "c", "h", &[], "new"),
            ],
        );
        assert_eq!(named(&one, &elsewhere), "added `c.h()`");
    }

    #[test]
    fn every_kind_of_difference_is_listed_in_one_sentence() {
        let k = key(7);
        let before = pins(
            &k,
            vec![
                routine(1, "a", "f", &[], "one"),
                routine(2, "a", "g", &[], "two"),
                routine(3, "a", "h", &[], "three"),
            ],
        );
        let after = pins(
            &k,
            vec![
                routine(1, "a", "f", &[], "ONE"),
                routine(3, "a", "h", &[], "three"),
                routine(4, "a", "i", &[], "four"),
            ],
        );
        assert_eq!(
            named(&before, &after),
            "changed `a.f()`; added `a.i()`; removed `a.g()`"
        );
    }

    #[test]
    fn an_overload_changing_is_named_by_its_own_signature() {
        let k = key(7);
        let rows = |text_body: &str| {
            vec![
                routine(1, "app", "f", &["integer"], "int body"),
                routine(2, "app", "f", &["text"], text_body),
                routine(3, "app", "f", &["integer", "text"], "pair body"),
            ]
        };
        assert_eq!(
            named(&pins(&k, rows("old")), &pins(&k, rows("new"))),
            "changed `app.f(text)`"
        );
    }

    #[test]
    fn a_routine_replaced_under_the_same_signature_reads_as_removed_and_added() {
        let k = key(7);
        let before = pins(&k, vec![routine(1, "a", "f", &["integer"], "one")]);
        let after = pins(&k, vec![routine(9, "a", "f", &["integer"], "one")]);
        assert_eq!(
            named(&before, &after),
            "added `a.f(integer)`; removed `a.f(integer)`"
        );
    }

    #[test]
    fn a_signature_that_carries_a_line_break_is_shown_escaped() {
        let k = key(7);
        let before = pins(&k, vec![routine(1, "a", "f\nforged: ok", &[], "one")]);
        let shown = named(&before, &BTreeMap::new());
        assert!(!shown.contains('\n'), "{shown}");
        assert!(shown.contains("\\n"), "{shown}");
    }

    #[test]
    fn moving_the_boundary_between_two_routine_inputs_changes_neither_pin_into_the_other() {
        let k = key(7);
        let split = pins(
            &k,
            vec![
                routine(1, "a", "f", &[], "ab"),
                routine(2, "a", "g", &[], "c"),
            ],
        );
        let moved = pins(
            &k,
            vec![
                routine(1, "a", "f", &[], "a"),
                routine(2, "a", "g", &[], "bc"),
            ],
        );
        assert_eq!(named(&split, &moved), "changed `a.f()`, `a.g()`");
    }

    #[test]
    fn a_digest_made_for_one_routine_does_not_pass_for_another() {
        let k = key(7);
        let first = pins(&k, vec![routine(1, "a", "f", &[], "same")]);
        let second = pins(&k, vec![routine(2, "a", "f", &[], "same")]);
        assert_ne!(first[&1].digest, second[&2].digest);
    }

    #[test]
    fn the_same_routines_under_another_key_give_another_pin() {
        let routines = || vec![routine(1, "a", "f", &[], "one")];
        assert_ne!(
            pins(&key(7), routines())[&1].digest,
            pins(&key(8), routines())[&1].digest
        );
        assert_eq!(pins(&key(7), routines()), pins(&key(7), routines()));
    }
}

#[cfg(test)]
mod type_identity_tests {
    use super::*;

    #[test]
    fn creating_a_type_does_not_waive_an_existing_uids_reviewed_evidence() {
        let old: pbps_model::Uid = "y_000001".parse().unwrap();
        let new: pbps_model::Uid = "y_000002".parse().unwrap();
        let name: pbps_model::TypeRef = "app.mood".parse().unwrap();
        let definition = pbps_model::ManagedType::Enum {
            values: vec!["ok".into()],
        };
        let created = pbps_model::PlannedChange::new(pbps_model::Change::CreateType {
            uid: new,
            name: name.clone(),
            definition: Box::new(definition.clone()),
        });
        let pin = pbps_model::DeclaredTypePin {
            uid: old.clone(),
            name: name.clone(),
            definition: definition.clone(),
            before: Some(pbps_model::DeclaredTypeBefore {
                name: name.clone(),
                definition: definition.clone(),
            }),
        };
        for rename in [false, true] {
            let mut changes = pbps_model::ChangeSet {
                changes: vec![created.clone()],
            };
            if rename {
                changes.changes.push(pbps_model::PlannedChange::new(
                    pbps_model::Change::RenameType {
                        uid: old.clone(),
                        from: name.clone(),
                        to: "app.kept".parse().unwrap(),
                        definition: Box::new(definition.clone()),
                    },
                ));
            }
            assert!(require_declared_types(&[old.clone()].into(), &[], &changes).is_err());
            require_declared_types(&[old.clone()].into(), std::slice::from_ref(&pin), &changes)
                .unwrap();
        }
        // A drop carries its reviewed before-definition in the UID-bearing step.
        let changes = pbps_model::ChangeSet {
            changes: vec![
                created,
                pbps_model::PlannedChange::new(pbps_model::Change::DropType {
                    uid: old.clone(),
                    name,
                    definition: Box::new(definition),
                }),
            ],
        };
        require_declared_types(&[old].into(), &[], &changes).unwrap();
    }

    #[test]
    fn a_declared_requirement_needs_its_own_uid_even_when_a_pin_reuses_its_name() {
        let old: pbps_model::Uid = "y_000001".parse().unwrap();
        let new: pbps_model::Uid = "y_000002".parse().unwrap();
        let name: pbps_model::TypeRef = "app.mood".parse().unwrap();
        let definition = pbps_model::ManagedType::Enum {
            values: vec!["ok".into()],
        };
        let pin = pbps_model::DeclaredTypePin {
            uid: new,
            name: name.clone(),
            definition: definition.clone(),
            before: None,
        };
        assert!(
            require_declared_types(
                &[old.clone()].into(),
                std::slice::from_ref(&pin),
                &Default::default()
            )
            .is_err()
        );
        let changes = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::DropType {
                    uid: old.clone(),
                    name,
                    definition: Box::new(definition),
                },
            )],
        };
        require_declared_types(&[old.clone()].into(), &[pin], &changes).unwrap();
        assert!(require_declared_types(&[old].into(), &[], &Default::default()).is_err());
    }

    #[test]
    fn an_external_reference_cannot_borrow_a_same_named_declared_identity() {
        let name: pbps_model::TypeRef = "app.mood".parse().unwrap();
        let schema = pbps_model::Schema {
            tables: [(
                "app.t".parse().unwrap(),
                pbps_model::Table {
                    columns: [(
                        "v".into(),
                        pbps_model::Column::new("app.mood[]".parse().unwrap()),
                    )]
                    .into(),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        let external = pbps_model::IdsFile::default();
        let declared = pbps_model::IdsFile {
            types: [("y_000001".parse().unwrap(), name.clone())].into(),
            ..Default::default()
        };
        assert_eq!(external_references(&schema, &external), [name].into());
        assert!(external_references(&schema, &declared).is_empty());
    }
}
