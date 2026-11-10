//! The commands that need a database: recording state, adopting a database, and
//! building one from the declarations (SPEC §8, §9.2).
//!
//! # The three ways a state gets recorded, and why they are not one command
//!
//! | Command | Records | Guard |
//! |---|---|---|
//! | `snapshot` | the database as it stands | refuses when it differs from the last recorded state |
//! | `baseline` | the same thing, unconditionally | requires `--reason` |
//! | `bootstrap` | the database it just built | refuses unless the managed set is empty |
//!
//! `snapshot` is what CI runs after `apply`, where there is provably nothing to
//! skip; it refuses on a difference precisely because that case — someone
//! changed the schema by hand and the pipeline is about to bless it — is the one
//! this tool exists to catch. `baseline` is the deliberate override, and the
//! reason it demands is what an audit asks for later.

use crate::emission::Engine as _;
use anyhow::{Context as _, bail};
use pbps_diff::identity::type_refs::{TypeIdentity, TypeResolver, TypeSide};
use pbps_diff::order::PlanningPlan;

use std::collections::{BTreeMap, BTreeSet};

use pbps_config::Project;
use pbps_db::Conn;
use pbps_dialect::Dialect;
use pbps_model::{
    DataScopes, IdsFile, ModuleId, ObservedRows, RowScope, Schema, SessionZone, StateKind,
    StateSnapshot, TableName,
};

use crate::db::{self, Target};

mod checkout;
mod guard;
mod services;
pub(crate) mod test_run;

use services::{Attempt, apply_identified};
pub(crate) use services::{cmd_bootstrap, cmd_plan_db};

/// The command owns catalog diagnostics even when a later read fails. Keep
/// them beside errors, never inside an error or recorded schema (DEC-1739.1).
#[derive(Clone, Copy)]
enum CatalogWarnings<'a> {
    Human,
    Findings(&'a std::cell::RefCell<Vec<crate::output::Finding>>),
}

impl<'a> CatalogWarnings<'a> {
    fn for_output(
        json: bool,
        findings: &'a std::cell::RefCell<Vec<crate::output::Finding>>,
    ) -> Self {
        if json {
            Self::Findings(findings)
        } else {
            Self::Human
        }
    }

    fn for_progress(progress: Option<&'a services::Progress>) -> Self {
        progress.map_or(Self::Human, |p| Self::Findings(&p.findings))
    }

    fn emit(self, findings: impl IntoIterator<Item = crate::output::Finding>) {
        match self {
            Self::Human => {
                for finding in findings {
                    eprintln!("warning: {}", finding.message);
                }
            }
            Self::Findings(collected) => collected.borrow_mut().extend(findings),
        }
    }
}

/// A scoped live state, plus the declared modules the read could not express.
///
/// The second list cannot live in [`pbps_diff::Scoped`] — "pbps cannot read
/// this back" is a dialect fact, and the differ knows no dialect — but a caller
/// that has to decide whether an object is *there* needs both: a module the
/// catalog holds and introspection cannot reproduce is inside the managed set
/// by name and outside the scoped schema in fact.
pub struct Managed {
    /// The connected database's default collation, where the engine has
    /// column collations (#1175): the declarations are compared with a
    /// collation equal to it taken out ([`Schema::without_collation`]).
    pub database_collation: Option<String>,
    pub scoped: pbps_diff::Scoped,
    /// Catalog facts inside the managed set that the projection could not
    /// express: an unsupported feature on a managed table, or a module the set
    /// names that introspection cannot read back. They are drift findings,
    /// never ignorable warnings, and no recorder accepts a schema that has any
    /// (see [`managed_limitations`]).
    pub limitations: Vec<String>,
    /// Unsupported grants to unmanaged roles on managed targets. These are
    /// report-only context, never scoped state or saved-plan findings (DEC-2446.1).
    pub permission_advisories: Vec<String>,
    /// Report-only catalog objects; never attached to plan changes or state.
    pub catalog_notices: Vec<String>,
    /// The grants this read could see and this connection could not take
    /// away (#251): an entry a third role granted, where no `REVOKE` this
    /// connection runs would carry that grantor.
    ///
    /// Here rather than in [`pbps_diff::Scoped`] for the reason above — whose
    /// grantor a statement carries is a dialect fact, and the differ knows no
    /// dialect. They are **not** removed from the scoped schema: each is an
    /// ordinary grant, and a declaration that keeps one is satisfied by the
    /// database as it stands. Only a plan that revokes one is impossible, and
    /// that is what [`crate::engine::unrevocable_grants`] refuses.
    pub unrevocable: Vec<pbps_db::catalog::Unrevocable>,
    /// Every module or type in the database that introspection cannot express, by identity,
    /// with the reason already rendered.
    ///
    /// Returned in full rather than filtered to the managed set, because the
    /// caller that most needs it is asking about a name that has *never* been
    /// recorded: a newly declared module that collides with an encrypted one
    /// already standing there.
    pub unreadable: Vec<(pbps_db::catalog::LimitationTarget, String)>,
    /// The rows read back under the read scope the caller asked for. Not yet
    /// placed in the schema: `plan --db` projects two different views out of
    /// one read (see [`pbps_model::data::read_scopes`]).
    pub rows: ObservedRows,
    /// Names outside the managed set whose relation definitions were omitted.
    /// An unsupported temporal table still occupies its name (SPEC §7.6).
    pub unmanaged_relations: Vec<TableName>,
}

/// The scoped state alone, with the rows of every table in `scopes` read back
/// and placed under it — which is all any caller but `bootstrap` and
/// `plan --db` needs.
///
/// `reference` is the schema whose rows say how a cell at its default is
/// read ([`pbps_model::ObservedRow`]): the recorded snapshot for a drift
/// check, the declarations for a state recorded from them. A command that has
/// no rows of its own — `apply` records what it just wrote from the plan's
/// scope alone — passes an empty schema and gets the shortest true spelling.
/// The rows to read back for each table in `scopes`.
///
/// Its own function because three callers need it and one of them —
/// `verify` — cannot go through [`managed_state`]: it wants the raw
/// [`Managed`] so the unmanaged policy can be applied beside the report
/// rather than inside the read.
fn rows_to_read(scopes: &DataScopes) -> BTreeMap<TableName, RowScope> {
    scopes
        .iter()
        .map(|(n, s)| (n.clone(), s.rows_to_read()))
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn managed_state(
    conn: &mut Conn,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unmanaged: db::CatalogPolicy<'_>,
    scopes: &DataScopes,
    reference: &Schema,
    read: crate::engine::Read,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<pbps_diff::Scoped> {
    let request = ManagedRead {
        ids,
        modules,
        external,
        unmanaged,
        scopes,
        reference,
        read,
        warnings,
    };
    managed_state_then(conn, &request, || std::future::ready(Ok(()))).await
}

struct ManagedRead<'a> {
    ids: &'a IdsFile,
    modules: &'a BTreeSet<ModuleId>,
    external: &'a BTreeSet<TableName>,
    unmanaged: db::CatalogPolicy<'a>,
    scopes: &'a DataScopes,
    reference: &'a Schema,
    read: crate::engine::Read,
    warnings: CatalogWarnings<'a>,
}

impl ManagedRead<'_> {
    async fn capture(&self, conn: &mut Conn) -> anyhow::Result<pbps_diff::Scoped> {
        managed_state_once(
            conn,
            self.ids,
            self.modules,
            self.external,
            self.unmanaged,
            self.scopes,
            self.reference,
            self.read,
            self.warnings,
        )
        .await
    }
}

// An internal test callback places a second connection's DDL between the two
// real reads. The production caller supplies no work and no user hook exists.
async fn managed_state_then<F, Fut>(
    conn: &mut Conn,
    request: &ManagedRead<'_>,
    after_capture: F,
) -> anyhow::Result<pbps_diff::Scoped>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let captured = request.capture(conn).await?;
    if crate::engine::needs_readback_revalidation(conn.driver(), request.read) {
        after_capture().await?;
        let checked = request.capture(conn).await?;
        // An unrelated unmanaged object is outside SPEC §8.2. Compare the
        // facts that can enter this recording, not the ambient inventory.
        if captured.schema != checked.schema
            || captured.missing != checked.missing
            || captured.missing_roles != checked.missing_roles
            || captured.missing_types != checked.missing_types
            || captured.unexpressible != checked.unexpressible
        {
            bail!(
                "the managed state changed during the transactional read-back; refusing to record an unstable schema. Retry after the concurrent change settles"
            );
        }
        return Ok(checked);
    }
    Ok(captured)
}

#[allow(clippy::too_many_arguments)]
async fn managed_state_once(
    conn: &mut Conn,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unmanaged: db::CatalogPolicy<'_>,
    scopes: &DataScopes,
    reference: &Schema,
    read: crate::engine::Read,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<pbps_diff::Scoped> {
    let managed = managed_state_full(
        conn,
        ids,
        modules,
        external,
        unmanaged,
        &rows_to_read(scopes),
        read,
        warnings,
    )
    .await?;
    refuse_managed_limitations(&managed.limitations)?;
    let mut scoped = managed.scoped;
    // Definition projection must not manufacture absence. Keep the same
    // read's omitted relation names for final endpoint checks, without
    // recording an unsupported definition in the schema (SPEC §7.6).
    scoped.unmanaged.extend(managed.unmanaged_relations);
    scoped.unmanaged.sort();
    scoped.unmanaged.dedup();
    scoped.schema = scoped
        .schema
        .with_observed_rows(&managed.rows, scopes, reference)?;
    Ok(scoped)
}

/// Refuses a PostgreSQL plan that creates a name the database already uses
/// outside the recorded scope (#316, #320, #323, DEC-316.1).
///
/// The recorded scope is the baseline's, and it has to stay that way (SPEC
/// §8.2, DECISIONS 417): a table or module this project never recorded is
/// outside it whether or not a declaration now names it. So a declaration
/// that newly names an object already standing in the database plans a
/// `CREATE`, and this engine has no `CREATE OR ALTER`: the statement is
/// refused at apply, after everything ordered before it has run inside the
/// transaction. Refused here instead, where the remedy is a decision:
/// remove the declaration, move the object out of the way, or adopt it.
///
/// Three kinds of occupant, named as such, because the remedies differ: a
/// readable table (#323) or module (#320) can be adopted as it stands, and an
/// object pbps cannot read (#316) cannot. Tables, views and materialized
/// views share one namespace per schema on this engine, so a table's name is
/// occupied by a view of that name and the other way round; a routine or a
/// trigger is occupied only by itself.
///
/// A `CreateModule` after a `DropModule` of the same id is a rebuild, not a
/// creation. An unmanaged occupant is never dropped by a plan, so no other
/// change in the plan can clear the name first.
// The complement is every change that does not create a name.
#[allow(clippy::wildcard_enum_match_arm)]
fn refuse_occupied_names(
    cs: &pbps_model::ChangeSet,
    scoped: &pbps_diff::Scoped,
    unreadable: &[(pbps_db::catalog::LimitationTarget, String)],
    unmanaged_relations: &[TableName],
    label: &str,
) -> anyhow::Result<()> {
    use pbps_model::Change;
    let relation = |name: &TableName| -> Option<String> {
        if scoped.unmanaged.contains(name) && !unmanaged_relations.contains(name) {
            return Some(format!("table `{name}`", name = name.shown()));
        }
        // Only a relation shares this namespace: an unreadable aggregate
        // `app.x(integer)` or a trigger named `x` coexists with a table
        // `app.x`, and refusing it would refuse a plan the engine takes.
        let a_relation = |t: &pbps_db::catalog::LimitationTarget| matches!(t, pbps_db::catalog::LimitationTarget::Relation(n) if n == name);
        if let Some((_, why)) = unreadable.iter().find(|(t, _)| a_relation(t)) {
            return Some(format!(
                "`{name}`, which pbps cannot read ({why})",
                name = name.shown()
            ));
        }
        if unmanaged_relations.contains(name) {
            return Some(format!(
                "`{name}`, a relation pbps cannot read",
                name = name.shown()
            ));
        }
        scoped
            .unmanaged_modules
            .contains(&ModuleId::Named(name.clone()))
            .then(|| format!("view `{name}`", name = name.shown()))
    };
    let dropped: BTreeSet<&ModuleId> = cs
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::DropModule { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    let mut taken: Vec<String> = Vec::new();
    for p in &cs.changes {
        let (declared, occupant) = match &p.change {
            Change::CreateTable { name, .. } => (
                format!("table `{name}`", name = name.shown()),
                relation(name),
            ),
            Change::CreateModule { id, module } if !dropped.contains(id) => {
                let occupant = match id {
                    ModuleId::Named(name) => relation(name),
                    ModuleId::Routine(_) | ModuleId::Trigger { .. } => scoped
                        .unmanaged_modules
                        .contains(id)
                        .then(|| format!("{} `{id}`", module.kind, id = id.shown())),
                };
                (format!("{} `{id}`", module.kind, id = id.shown()), occupant)
            }
            _ => continue,
        };
        if let Some(occupant) = occupant {
            taken.push(format!("{declared}: the database already has {occupant}"));
        }
    }
    // An index shares the relation namespace with all of these (#1355).
    for name in created_index_names(cs) {
        if let Some(occupant) = relation(&name) {
            taken.push(format!(
                "index `{name}`: the database already has {occupant}",
                name = name.shown()
            ));
        }
    }
    if taken.is_empty() {
        return Ok(());
    }
    bail!(
        "`{label}` already uses {} name(s) this plan would create, outside what this project \
         records:\n  {}\nThis engine has no `CREATE OR ALTER`, so each `CREATE` would be \
         refused at apply. Remove or rename the declaration, drop or rename the object in the \
         database, or — for a table or module pbps can read — adopt it as it stands with \
         `pbps baseline --reason ...`, after which the declaration is compared with it rather \
         than created.",
        taken.len(),
        taken.join("\n  "),
        label = pbps_model::escape::Escaped(label),
    );
}

/// The packages a plan installs, by name.
pub(crate) fn created_extensions(
    cs: &pbps_model::ChangeSet,
) -> impl Iterator<Item = &pbps_model::ObjectLocalName> {
    cs.changes.iter().filter_map(|p| {
        if let pbps_model::Change::CreateExtension { name, .. } = &p.change {
            Some(name)
        } else {
            None
        }
    })
}

/// The packages a plan installs or updates, by name and what is done: each
/// runs a script pbps cannot read, so each needs a rehearsal to pin (DEC-2447.6,
/// DEC-2447.10).
fn changed_extensions(
    cs: &pbps_model::ChangeSet,
) -> impl Iterator<Item = (&pbps_model::ObjectLocalName, &'static str)> {
    cs.changes.iter().filter_map(|p| {
        if let pbps_model::Change::CreateExtension { name, .. } = &p.change {
            Some((name, "created"))
        } else if let pbps_model::Change::UpdateExtension { name, .. } = &p.change {
            Some((name, "updated"))
        } else if let pbps_model::Change::SetExtensionSchema { name, .. } = &p.change {
            Some((name, "moved"))
        } else {
            None
        }
    })
}

/// What the target says about the packages the declaration holds and it lacks,
/// judged before any plan is written (DEC-2447.6): every refusal names the
/// package and the fact. The answer is each package's own prerequisites, which
/// the differ turns into the order among the plan's creates.
async fn extension_creates(
    conn: &mut pbps_db::Conn,
    base: &Schema,
    declared: &Schema,
    ids: &IdsFile,
) -> anyhow::Result<BTreeMap<pbps_model::ObjectLocalName, BTreeSet<pbps_model::ObjectLocalName>>> {
    let wanted: BTreeMap<_, _> = declared
        .extensions
        .iter()
        .filter(|(name, _)| {
            !base.extensions.contains_key(*name) && ids.extension_uid(name).is_some()
        })
        .collect();
    if wanted.is_empty() {
        return Ok(BTreeMap::new());
    }
    let target = pbps_pg::extension_provider::target(conn).await?;
    let pinned: BTreeMap<String, pbps_model::Extension> = wanted
        .iter()
        .map(|(name, definition)| (name.raw().to_owned(), (*definition).clone()))
        .collect();
    let problems: Vec<String> = wanted
        .iter()
        .flat_map(|(name, definition)| {
            pbps_pg::extension_provider::create_problems(&target, name.raw(), definition, &pinned)
        })
        .collect();
    anyhow::ensure!(problems.is_empty(), "{}", problems.join("\n"));
    Ok(wanted
        .iter()
        .filter_map(|(name, definition)| {
            let control = target
                .available
                .get(&(name.raw().to_owned(), definition.version.clone()))?;
            Some((
                (*name).clone(),
                control.requires.iter().map(|r| r.as_str().into()).collect(),
            ))
        })
        .collect())
}

/// The packages the declarations hold at another version than the target, in
/// the same schema, under one recorded identity: each becomes an update, judged
/// before any plan is written (DEC-2447.10). Every refusal names the package;
/// the facts are what the differ turns into the update's order.
async fn extension_update_facts(
    conn: &mut pbps_db::Conn,
    base: &Schema,
    declared: &Schema,
    recorded: &IdsFile,
    ids: &IdsFile,
) -> anyhow::Result<BTreeMap<pbps_model::ObjectLocalName, pbps_model::ExtensionUpdateFacts>> {
    let updates: BTreeMap<_, _> = declared
        .extensions
        .iter()
        .filter_map(|(name, to)| {
            let from = base.extensions.get(name)?;
            let same = recorded.extension_uid(name).is_some()
                && recorded.extension_uid(name) == ids.extension_uid(name);
            // An update runs in the schema the package holds; a move that
            // follows it is [`extension_move_facts`]'s (DEC-2447.11).
            (same && from.version != to.version).then(|| {
                let stays = pbps_model::Extension {
                    schema: from.schema.clone(),
                    version: to.version.clone(),
                };
                (name.clone(), (from.clone(), stays))
            })
        })
        .collect();
    if updates.is_empty() {
        return Ok(BTreeMap::new());
    }
    let creating: BTreeMap<String, pbps_model::Extension> = declared
        .extensions
        .iter()
        .filter(|(name, _)| {
            !base.extensions.contains_key(*name) && ids.extension_uid(name).is_some()
        })
        .map(|(name, definition)| (name.raw().to_owned(), definition.clone()))
        .collect();
    let named: BTreeMap<String, _> = updates
        .iter()
        .map(|(name, pair)| (name.raw().to_owned(), pair.clone()))
        .collect();
    let problems = pbps_pg::extension_provider::update_problems(conn, &named, &creating).await?;
    anyhow::ensure!(problems.is_empty(), "{}", problems.join("\n"));
    let available = pbps_pg::extension_provider::available(conn).await?;
    let all = pbps_pg::extension_provider::providers(conn).await?;
    let mut reach = pbps_pg::extension_provider::drop_facts(conn, &all).await?;
    let mut facts = BTreeMap::new();
    for (name, (from, to)) in updates {
        // The prerequisites of every version on the path, which the same
        // function that judged them above names (DEC-2447.10).
        let path =
            pbps_pg::extension_provider::update_path(conn, name.raw(), &from.version, &to.version)
                .await?
                .unwrap_or_default();
        let requires = pbps_pg::extension_provider::path_requires(&available, name.raw(), &path)
            .iter()
            .map(|r| r.as_str().into())
            .collect();
        let reach = reach.remove(name.raw()).unwrap_or_default();
        facts.insert(name, pbps_model::ExtensionUpdateFacts { requires, reach });
    }
    Ok(facts)
}

/// The packages the declarations hold in another schema than the target, under
/// one recorded identity: each becomes a move, judged before any plan is written
/// (DEC-2447.11). Every refusal names the package; the answer is each package's
/// members as the destination would name them, which the differ turns into the
/// modules the move waits for.
async fn extension_move_facts(
    conn: &mut pbps_db::Conn,
    base: &Schema,
    declared: &Schema,
    recorded: &IdsFile,
    ids: &IdsFile,
) -> anyhow::Result<BTreeMap<pbps_model::ObjectLocalName, BTreeSet<pbps_model::ProviderMember>>> {
    let (mut moves, mut updating) = (BTreeMap::new(), BTreeSet::new());
    for (name, to) in &declared.extensions {
        let Some(from) = base.extensions.get(name) else {
            continue;
        };
        let same = recorded.extension_uid(name).is_some()
            && recorded.extension_uid(name) == ids.extension_uid(name);
        if same && from.schema != to.schema {
            // The move starts from the version this plan's update leaves.
            let starts = pbps_model::Extension {
                schema: from.schema.clone(),
                version: to.version.clone(),
            };
            moves.insert(name.raw().to_owned(), (starts, to.clone()));
            if from.version != to.version {
                updating.insert(name.raw().to_owned());
            }
        }
    }
    if moves.is_empty() {
        return Ok(BTreeMap::new());
    }
    let problems = pbps_pg::extension_provider::move_problems(conn, &moves, &updating).await?;
    anyhow::ensure!(problems.is_empty(), "{}", problems.join("\n"));
    let all = pbps_pg::extension_provider::providers(conn).await?;
    Ok(pbps_pg::extension_provider::relocated_members(&all, &moves)
        .into_iter()
        .map(|(name, members)| (name.as_str().into(), members))
        .collect())
}

/// An update that would leave a member the declarations also manage as their
/// own, or leave the package at another version than declared, is refused
/// before it is sealed: one object cannot have two owners (DEC-2447.7).
fn refuse_two_owners(
    name: &pbps_model::ObjectLocalName,
    after: &pbps_model::ExtensionProvider,
    to: &pbps_model::Extension,
    declared: &Schema,
) -> anyhow::Result<()> {
    let collisions = after.collisions(declared);
    anyhow::ensure!(
        collisions.is_empty() && after.definition == *to,
        "extension `{}` would leave {} that the declarations also manage as their own, or not at the declared version; one object cannot have two owners",
        name.shown(),
        collisions
            .iter()
            .map(|m| format!("`{m}`"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

/// The sealed evidence of each package change: the manifest the target holds
/// now, the one the rehearsal read after running the plan's statements, and the
/// versions the engine reports an update would walk. One pin covers the package's
/// whole chain, an update and then a move. A package the rehearsal read no
/// manifest for, that the declarations also manage a member of, or that a move
/// would put on a name an unmanaged object holds in the destination, is refused,
/// never sealed.
async fn extension_change_pins(
    conn: &mut pbps_db::Conn,
    cs: &pbps_model::ChangeSet,
    manifests: &crate::rehearsal::connected::Manifests,
    declared: &Schema,
) -> anyhow::Result<Vec<pbps_model::ExtensionChangePin>> {
    use pbps_model::{Change, Extension, Uid};
    type Step<'a> = (&'a Extension, &'a Extension);
    type Chain<'a> = (
        &'a pbps_model::ObjectLocalName,
        Option<Step<'a>>,
        Option<(Step<'a>, &'a BTreeSet<pbps_model::ModuleId>)>,
    );
    let mut chains: BTreeMap<&Uid, Chain<'_>> = BTreeMap::new();
    for planned in &cs.changes {
        if let Change::UpdateExtension {
            uid,
            name,
            from,
            to,
            ..
        } = &planned.change
        {
            chains.entry(uid).or_insert((name, None, None)).1 = Some((from, to));
        } else if let Change::SetExtensionSchema {
            uid,
            name,
            from,
            to,
            releases,
        } = &planned.change
        {
            chains.entry(uid).or_insert((name, None, None)).2 = Some(((from, to), releases));
        }
    }
    let mut pins = Vec::new();
    let mut held = None;
    for (uid, (name, update, movement)) in chains {
        let (from, to) = (
            update
                .or(movement.map(|(step, _)| step))
                .map(|(from, _)| from),
            movement.map(|(step, _)| step).or(update).map(|(_, to)| to),
        );
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        let after = manifests.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "the rehearsal read no members for extension `{}`, so the plan cannot pin its change",
                name.shown()
            )
        })?;
        refuse_two_owners(name, after, to, declared)?;
        if held.is_none() {
            held = Some(pbps_pg::extension_provider::providers(conn).await?);
        }
        let before = held
            .as_ref()
            .and_then(|all| all.get(name.raw()))
            .filter(|m| m.definition == *from)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "extension `{}` is not installed as the plan assumed, so the plan cannot pin its change",
                    name.shown()
                )
            })?;
        if let Some((_, releases)) = movement {
            // What the destination holds now that a moved member would take: the
            // modules this plan drops are the handover, anything else is refused
            // here, where the rehearsal's scratch cannot see an unmanaged object.
            let held = pbps_pg::extension_provider::schema_occupants(conn, &to.schema).await?;
            let taken: Vec<String> = after
                .members
                .iter()
                .filter(|m| held.contains(*m) && m.module(releases).is_none())
                .map(|m| format!("`{m}`"))
                .collect();
            anyhow::ensure!(
                taken.is_empty(),
                "extension `{}` cannot move to schema `{}`: it already holds {}, which this plan does not drop, so the engine would refuse",
                name.shown(),
                pbps_model::escape::Escaped(&to.schema),
                taken.join(", ")
            );
        }
        let path = match update {
            Some((from, to)) => pbps_pg::extension_provider::update_path(
                conn,
                name.raw(),
                &from.version,
                &to.version,
            )
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("extension `{}` has no update path to pin", name.shown())
            })?,
            None => Vec::new(),
        };
        pins.push(pbps_model::ExtensionChangePin {
            uid: uid.clone(),
            name: name.clone(),
            from: from.clone(),
            to: to.clone(),
            path,
            before: before.clone(),
            after: after.clone(),
        });
    }
    Ok(pins)
}

/// The packages a recorded intent retires, by name: held by the target, no
/// longer declared, and tombstoned in the identity this plan will carry.
fn retired_extensions(
    base: &Schema,
    declared: &Schema,
    recorded: &IdsFile,
    ids: &IdsFile,
) -> BTreeSet<pbps_model::ObjectLocalName> {
    base.extensions
        .keys()
        .filter(|name| !declared.extensions.contains_key(*name))
        .filter(|name| {
            recorded
                .extension_uid(name)
                .is_some_and(|uid| ids.tombstones.contains_key(uid))
        })
        .cloned()
        .collect()
}

/// What the target says dropping each retired package would reach, judged
/// before any plan is written (DEC-2447.8): the connected role must be allowed
/// to drop it, and the facts become the differ's input, which carries what the
/// plan also removes and refuses the rest by name.
async fn extension_drop_facts(
    conn: &mut pbps_db::Conn,
    retired: &BTreeSet<pbps_model::ObjectLocalName>,
) -> anyhow::Result<BTreeMap<pbps_model::ObjectLocalName, pbps_model::ExtensionDropFacts>> {
    if retired.is_empty() {
        return Ok(BTreeMap::new());
    }
    let names: BTreeSet<String> = retired.iter().map(|n| n.raw().to_owned()).collect();
    let problems = pbps_pg::extension_provider::drop_problems(conn, &names).await?;
    anyhow::ensure!(problems.is_empty(), "{}", problems.join("\n"));
    let all = pbps_pg::extension_provider::providers(conn).await?;
    let mut facts = pbps_pg::extension_provider::drop_facts(conn, &all).await?;
    Ok(retired
        .iter()
        .filter_map(|name| Some((name.clone(), facts.remove(name.raw())?)))
        .collect())
}

/// The sealed evidence of each drop: the manifest the target holds now, and
/// what the drop reaches beyond it that the plan accepted. A package the
/// target no longer holds as the plan assumed is refused, never sealed.
async fn extension_drop_pins(
    conn: &mut pbps_db::Conn,
    cs: &pbps_model::ChangeSet,
) -> anyhow::Result<Vec<pbps_model::ExtensionDropPin>> {
    let drops: Vec<_> = cs
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::DropExtension {
                uid,
                name,
                definition,
                ..
            } = &p.change
            {
                Some((uid, name, definition))
            } else {
                None
            }
        })
        .collect();
    if drops.is_empty() {
        return Ok(Vec::new());
    }
    let all = pbps_pg::extension_provider::providers(conn).await?;
    let mut facts = pbps_pg::extension_provider::drop_facts(conn, &all).await?;
    drops
        .into_iter()
        .map(|(uid, name, definition)| {
            let manifest = all.get(name.raw()).filter(|m| m.definition == *definition);
            let manifest = manifest.ok_or_else(|| {
                anyhow::anyhow!(
                    "extension `{}` is not installed as the plan assumed, so the plan cannot pin its drop",
                    name.shown()
                )
            })?;
            Ok(pbps_model::ExtensionDropPin {
                uid: uid.clone(),
                name: name.clone(),
                definition: definition.clone(),
                manifest: manifest.clone(),
                outside: facts.remove(name.raw()).unwrap_or_default().outside,
            })
        })
        .collect()
}

/// The sealed evidence of each create: the manifest the rehearsal read, after
/// the check that no member is also an object the declaration manages. A
/// create the rehearsal did not capture is refused, never sealed empty.
fn extension_pins(
    cs: &pbps_model::ChangeSet,
    mut manifests: crate::rehearsal::connected::Manifests,
    declared: &Schema,
    ids: &IdsFile,
) -> anyhow::Result<Vec<pbps_model::ExtensionPin>> {
    let mut pins = Vec::new();
    for planned in &cs.changes {
        let pbps_model::Change::CreateExtension {
            uid,
            name,
            definition,
            ..
        } = &planned.change
        else {
            continue;
        };
        let manifest = manifests.remove(name).ok_or_else(|| {
            anyhow::anyhow!(
                "the rehearsal read no members for extension `{}`, so the plan cannot pin it",
                name.shown()
            )
        })?;
        let collisions = manifest.collisions(declared);
        anyhow::ensure!(
            collisions.is_empty() && ids.extension_uid(name) == Some(uid),
            "extension `{}` brings {} that the declarations also manage as their own; one object cannot have two owners",
            name.shown(),
            collisions
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        pins.push(pbps_model::ExtensionPin {
            uid: uid.clone(),
            name: name.clone(),
            definition: definition.clone(),
            manifest,
        });
    }
    Ok(pins)
}

/// The relation names this plan creates: its tables, the views it creates
/// that it does not also drop, the indexes it creates by name, and fixed
/// sequence targets (DEC-1899.1). A view dropped and created in one plan is
/// a rebuild, and its name is already its own.
// The complement is every change that creates or frees no relation name.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn created_relation_names(cs: &pbps_model::ChangeSet) -> Vec<TableName> {
    use pbps_model::Change;
    let dropped: BTreeSet<&ModuleId> = cs
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::DropModule { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    cs.changes
        .iter()
        .filter(|p| !matches!(&p.change, Change::CreateModule { id, .. } if dropped.contains(id)))
        .flat_map(|p| {
            relation_names_created(&p.change).into_iter().chain(
                sequence_names_claimed(&p.change)
                    .into_iter()
                    .map(|(name, _)| name),
            )
        })
        .collect()
}

/// Fixed sequence targets, with their columns for diagnostics. Keep these
/// out of `relation_names_created`: its identity-release and detach callers
/// select ordering edges, which this namespace check does not add (DEC-1899.1).
/// The dialect calls the emitter's `owned_sequence`, including truncation.
// The complement is every change that creates or renames no owned sequence.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn sequence_names_claimed(
    change: &pbps_model::Change,
) -> Vec<(TableName, pbps_model::ColumnRef)> {
    use pbps_model::{Change, ColumnRef, ValueSource};
    let named = |column: ColumnRef, source: &ValueSource| {
        let ValueSource::Sequence(sequence) = source else {
            return None;
        };
        let name = pbps_pg::Postgres::new()
            .owned_sequence_name(&column.table, column.name.raw(), sequence)
            .expect("PostgreSQL always names an owned sequence");
        Some((TableName::new(&column.table.schema, name), column))
    };
    match change {
        Change::CreateTable { name, table, .. } => table
            .columns
            .iter()
            .filter_map(|(column, declared)| {
                named(
                    ColumnRef::new(name.clone(), column.clone()),
                    &declared.value_source,
                )
            })
            .collect(),
        Change::AddColumn {
            table,
            name,
            column,
            ..
        } => named(
            ColumnRef::new(table.clone(), name.clone()),
            &column.value_source,
        )
        .into_iter()
        .collect(),
        Change::AlterColumnNumbering {
            column, from, to, ..
        } => {
            if let (ValueSource::Sequence(was), ValueSource::Sequence(now)) = (from, to)
                && !(was.name.is_some() && now.name.is_some() && was.name != now.name)
            {
                // A retune, or an omitted name on either side, keeps the
                // catalog name; the emitter does not rename it.
                return Vec::new();
            }
            named(column.clone(), to).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

/// The relation names one change creates: a table, a view, and the index
/// names of [`index_names_created`].
// The complement is every change that creates no relation.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn relation_names_created(change: &pbps_model::Change) -> Vec<TableName> {
    use pbps_model::Change;
    let mut out = match change {
        Change::CreateTable { name, .. } => vec![name.clone()],
        Change::CreateModule {
            id: ModuleId::Named(name),
            module,
        } if module.kind == pbps_model::ModuleKind::View => vec![name.clone()],
        _ => Vec::new(),
    };
    out.extend(index_names_created(change));
    out
}

/// The index names this plan creates ([`index_names_created`]).
fn created_index_names(cs: &pbps_model::ChangeSet) -> Vec<TableName> {
    cs.changes
        .iter()
        .flat_map(|p| index_names_created(&p.change))
        .collect()
}

/// The index names a change creates, each in its table's schema (#1355):
/// an added index, the index behind an added unique constraint or a named
/// primary key, which PostgreSQL names after the constraint, and those a
/// created table declares. An index shares the relation namespace with
/// tables, views and sequences, so `CREATE INDEX` at a held name fails with
/// 42P07. An unnamed key's `<table>_pkey` is not here: PostgreSQL gives it a
/// fallback when that is held (DEC-465.1). Changes on a renamed table carry
/// its new name, which is the schema the index is created in.
// The complement is every change that creates no index.
#[allow(clippy::wildcard_enum_match_arm)]
fn index_names_created(change: &pbps_model::Change) -> Vec<TableName> {
    use pbps_model::Change;
    let in_schema = |t: &TableName, name: &str| TableName::new(t.schema.clone(), name);
    match change {
        Change::AddIndex { table, name, .. } | Change::AddUnique { table, name, .. } => {
            vec![in_schema(table, name.raw())]
        }
        Change::SetPrimaryKey {
            table,
            to: Some(key),
            ..
        } => key
            .name
            .iter()
            .map(|name| in_schema(table, name.raw()))
            .collect(),
        Change::DetachPartition { table, names, .. } => names
            .iter()
            .filter(|n| {
                matches!(
                    n.kind,
                    pbps_model::DetachedKind::PrimaryKey
                        | pbps_model::DetachedKind::Unique
                        | pbps_model::DetachedKind::Index
                )
            })
            .filter_map(|n| n.name.as_ref().map(pbps_model::ObjectLocalName::raw))
            .map(|name| in_schema(table, name))
            .collect(),
        Change::CreateTable { name, table, .. } => table
            .primary_key
            .iter()
            .filter_map(|k| k.name.as_ref().map(pbps_model::ObjectLocalName::raw))
            .chain(table.unique.keys().map(pbps_model::ObjectLocalName::raw))
            .chain(table.indexes.keys().map(pbps_model::ObjectLocalName::raw))
            .map(|n| in_schema(name, n))
            .collect(),
        _ => Vec::new(),
    }
}

/// Refuses a name this plan creates that a sequence, an index or a composite
/// type already holds (#951), the relation-namespace entries the catalog
/// inventory behind [`refuse_occupied_names`] does not report. One this plan
/// frees first is not an occupant: an index it drops, and an index or owned
/// sequence of a table it drops, which goes with its table.
///
/// The occupants are replayed through the plan in its order ([`Occupant`]),
/// and each name a change creates is compared with them as they are at that
/// change, after its own drops (#1723). A rename moves what its table owns,
/// and a cross-schema one carries it into the destination schema (#1084).
/// There it also meets the tables and views at its landing name (`relations`,
/// managed or not), which a drop or a rename away frees first and a rename
/// into it holds from then on (#1765).
pub(crate) fn refuse_uninventoried_occupants(
    cs: &pbps_model::ChangeSet,
    occupants: &[pbps_pg::catalog::NameOccupant],
    relations: &[(TableName, &'static str)],
    label: &str,
    catalog: &pbps_dialect::slots::CatalogFacts,
) -> anyhow::Result<()> {
    let taken: Vec<String> = name_clashes(&cs.changes, occupants, relations, catalog)
        .into_iter()
        .map(|(_, clash)| clash)
        .collect();
    if taken.is_empty() {
        return Ok(());
    }
    // A detach's clone-name claim needs a separate-deployment remedy,
    // naming the detach.
    let remedy = if cs
        .changes
        .iter()
        .any(|p| matches!(p.change, pbps_model::Change::DetachPartition { .. }))
    {
        "Each creation, schema move, or detach name change above would be refused at apply. \
         Free that name in a plan of its own first (another partition's clone: detach that \
         partition first), or declare another name (DEC-1565.1)."
    } else {
        "Each `CREATE`, sequence rename, or the `SET SCHEMA` of each cross-schema rename would be \
         refused at apply. \
         Rename the declaration, or drop or rename the object in the database. Where a later \
         rename in this plan moves the holder away, apply that rename in a plan of its own \
         first."
    };
    bail!(
        "`{label}` already uses {} name(s) this plan would create or move an object onto, for \
         objects that share PostgreSQL's relation namespace with tables and views:\n  {}\n{remedy}",
        taken.len(),
        taken.join("\n  "),
        label = pbps_model::escape::Escaped(label),
    );
}

/// Orders a cross-schema rename's carried objects clear of a holder that a
/// rename of this plan moves away (#1767, #1957, DEC-1957.2). Where the
/// replay ([`name_clashes`]) finds a move landing on a held name, the plan is
/// settled over its name handovers: the plan's own names and the catalog's
/// occupants as the first holders of theirs (`order::slots::handovers`), so
/// the move runs after what moves its holder away, and a chain of such moves
/// is ordered by the same evidence. The order settled is kept only where
/// table renames alone pass one another, and the replay finds fewer clashes
/// in it: a release build computes no other edge, and a table rename meets
/// another only through the names both see, which the edges and the replay
/// hold. Anything else between them, a cycle, or names that cannot be read
/// keep the order, and [`refuse_uninventoried_occupants`] refuses it. A
/// creation taking an identity sequence's catalog name also follows its
/// release, with the effects holding its dependents after it (DEC-1986.1).
/// Detached partitions hand over their exact catalog clone names the same
/// way, retaining each detach's own block and guards (DEC-1558.1).
pub(crate) fn order_carried_moves<S: Clone>(
    cs: &mut PlanningPlan<S>,
    occupants: &[pbps_pg::catalog::NameOccupant],
    relations: &[(TableName, &'static str)],
    sides: (pbps_diff::Side<'_>, pbps_diff::Side<'_>),
    dialect: &dyn pbps_dialect::Dialect,
    catalog: &pbps_dialect::slots::CatalogFacts,
) {
    use pbps_model::{Change, ValueSource};

    let clashes = name_clashes(&cs.changes, occupants, relations, catalog);
    if clashes.is_empty() {
        return;
    }
    let mut facts = occupant_slots(occupants);
    facts.detach_releases.clone_from(&catalog.detach_releases);
    let Ok(mut schedule) =
        pbps_diff::order::slots::handovers(&cs.changes, sides.0, sides.1, dialect, &facts)
    else {
        return;
    };
    // A retype and a numbering change are both possible leavers to the
    // generic slots. The PostgreSQL identity release is exact, so state its
    // own handover instead of requiring a unique generic edge (DEC-1986.2).
    let mut moving = vec![false; cs.changes.len()];
    let mut detach_steps = vec![false; cs.changes.len()];
    let mut present = Occupant::with_detaches(occupants, catalog);
    for (before, release) in cs.changes.iter().enumerate() {
        detach_steps[before] = matches!(
            &release.change,
            Change::DetachPartition { .. }
                | Change::DropTable {
                    detach_from: Some(_),
                    ..
                }
        ) && present.iter().any(|o| o.freed_by(&release.change));
        if matches!(&release.change, Change::AlterColumnNumbering {
            from: ValueSource::Identity(_), to, ..
        } if !matches!(to, ValueSource::Identity(_)))
        {
            for (after, claim) in cs.changes.iter().enumerate() {
                if relation_names_created(&claim.change).iter().any(|name| {
                    present.iter().any(|o| {
                        o.found.kind == "sequence" && o.name == *name && o.freed_by(&release.change)
                    })
                }) {
                    schedule.edge(
                        before,
                        after,
                        pbps_diff::order::Source::Catalog,
                        pbps_model::resolver::OrderReason::Identity,
                    );
                    moving[after] = true;
                }
            }
        }
        present.retain(|o| !o.freed_by(&release.change));
        for o in &mut present {
            if let Change::RenameTable { from, to, .. } = &release.change {
                o.moved(from, to);
            }
            if let Change::RenameColumn {
                table, from, to, ..
            } = &release.change
            {
                o.column_renamed(table, from, to);
            }
        }
    }
    // Detach handovers already have exact catalog edges. Renames can claim
    // the clone's name directly or carry an index onto it; neither is a
    // creation, but both need the same effect closure (DEC-1558.2).
    for edge in schedule.constraints() {
        if edge.source == pbps_diff::order::Source::Catalog
            && detach_steps[edge.before]
            && (matches!(cs.changes[edge.after].change, Change::RenameTable { .. })
                || !relation_names_created(&cs.changes[edge.after].change).is_empty())
        {
            moving[edge.after] = true;
        }
    }
    let Ok(mut settled) = schedule.settle(cs.clone()) else {
        return;
    };
    let renames: Vec<bool> = cs
        .changes
        .iter()
        .map(|p| matches!(p.change, pbps_model::Change::RenameTable { .. }))
        .collect();
    if !only_crossed_by(&settled.sequence, &renames) {
        use pbps_diff::order::{Baseline, Checks, Schedule, analyse_with_catalog, graph};
        use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
        if !moving.iter().any(|&moves| moves) {
            return;
        }
        let Ok(names) = pbps_model::plan_names::PlanNames::of(&cs.changes, sides.1.ids) else {
            return;
        };
        let owners = PlanOwners::new(names, sides.0.ids, sides.1.ids);
        let operations = Operations::of_plan(&cs.changes, ModuleEdits::Rebuilt);
        let before = Snapshot {
            schema: sides.0.schema,
            ids: sides.0.ids,
        };
        let after = Snapshot {
            schema: sides.1.schema,
            ids: sides.1.ids,
        };
        let effects = analyse_with_catalog(&operations, &owners, before, after, dialect, None);
        if !effects.unowned.is_empty() {
            return;
        }
        let graph = graph(
            operations.as_slice(),
            &effects.effects,
            &Baseline::of(before, &owners),
            &pbps_model::ModuleDeps::new(),
            Checks::ALL,
        );
        let effects = Schedule::of_graph(&graph, operations.as_slice());
        // A delayed claimant carries its consumers with it. Other steps
        // retain their relative order: this is not permission to reorder
        // the rest of the plan using an incomplete set of catalog facts.
        loop {
            let mut changed = false;
            for edge in effects.constraints() {
                if moving[edge.before] && !moving[edge.after] {
                    moving[edge.after] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut previous = None;
        for (at, &moves) in moving.iter().enumerate() {
            if !moves {
                if let Some(previous) = previous {
                    schedule.keep(
                        previous,
                        at,
                        "only a catalog name's new holder and its dependents may move".into(),
                    );
                }
                previous = Some(at);
            }
        }
        for edge in effects.constraints() {
            schedule.edge(edge.before, edge.after, edge.source, edge.reason);
        }
        let Ok(ordered) = schedule.settle(cs.clone()) else {
            return;
        };
        settled = ordered;
    }
    if name_clashes(&settled.changes.changes, occupants, relations, catalog).len() < clashes.len() {
        *cs = settled.changes;
    }
}

/// Whether every step `sequence` moves past another, or another moves past,
/// is one `allowed` marks: an inverted pair of positions has both ends
/// marked. A step has passed another exactly where an earlier step of the
/// order came from a later position, or a later one from an earlier.
fn only_crossed_by(sequence: &[usize], allowed: &[bool]) -> bool {
    let mut later_min = vec![usize::MAX; sequence.len() + 1];
    for (at, &from) in sequence.iter().enumerate().rev() {
        later_min[at] = later_min[at + 1].min(from);
    }
    let mut earlier_max: Option<usize> = None;
    for (at, &from) in sequence.iter().enumerate() {
        let crossed = earlier_max.is_some_and(|m| m > from) || later_min[at + 1] < from;
        if crossed && !allowed[from] {
            return false;
        }
        earlier_max = Some(earlier_max.map_or(from, |m| m.max(from)));
    }
    true
}

/// The occupants as name slots, for the handover edges and the slot replay
/// (#1964, #1957). The tables and views at the carried names are not
/// occupants: the plan manages them or refuses them already
/// (`refuse_uninventoried_occupants`).
pub(crate) fn occupant_slots(
    occupants: &[pbps_pg::catalog::NameOccupant],
) -> pbps_dialect::slots::CatalogFacts {
    use pbps_dialect::slots::{CatalogFacts, Namespace, Occupant, SlotKey};
    CatalogFacts {
        occupants: occupants
            .iter()
            .flat_map(|o| {
                let mut slots = vec![Occupant {
                    slot: SlotKey {
                        namespace: Namespace::Relations(o.name.schema.clone()),
                        name: o.name.name.clone(),
                    },
                    kind: o.kind.to_owned(),
                    owner: o.owner.clone(),
                    column: o.owner_column.clone(),
                }];
                if o.kind == "composite type" {
                    let mut row_type = slots[0].clone();
                    row_type.slot.namespace = Namespace::Types(o.name.schema.clone());
                    slots.push(row_type);
                }
                slots
            })
            .collect(),
        ..CatalogFacts::default()
    }
}

/// Names a detach certainly vacates or retains, not every index on its
/// table. An unnamed key keeps its engine name, and ambiguity keeps the DO
/// guard's existing refusal instead of inventing a release (DEC-1558.1).
pub(crate) fn detach_clone_slots(
    uid: &pbps_model::Uid,
    names: &[pbps_model::DetachedName],
    clones: &[pbps_pg::catalog::DetachClone],
) -> pbps_dialect::slots::CatalogFacts {
    use pbps_dialect::slots::{CatalogFacts, Namespace, SlotKey};
    let mut facts = CatalogFacts::default();
    for wanted in names {
        let Some(to) = &wanted.name else { continue };
        let matching: Vec<_> = clones
            .iter()
            .filter(|c| {
                c.kind == wanted.kind
                    && (c.kind == pbps_model::DetachedKind::PrimaryKey || c.parent == wanted.parent)
            })
            .collect();
        let [clone] = matching[..] else { continue };
        let slots = if clone.name.name != to.raw() {
            &mut facts.detach_releases
        } else {
            &mut facts.detach_retained
        };
        slots.insert(
            SlotKey {
                namespace: Namespace::Relations(clone.name.schema.clone()),
                name: clone.name.name.clone(),
            },
            uid.clone(),
        );
    }
    facts
}

/// The names a table rename takes as it runs: its target and, on its way
/// across schemas under a new name, its old name in the target schema, which
/// `SET SCHEMA` lands it on before `RENAME TO` (as `rename_order` claims).
fn claims_of(from: &TableName, to: &TableName) -> Vec<TableName> {
    let mut out = vec![to.clone()];
    if from.schema != to.schema && from.name != to.name {
        out.push(TableName::new(&to.schema, &from.name));
    }
    out
}

/// The names this plan's table renames take as they run ([`claims_of`]), for
/// the catalog reads behind [`name_clashes`] (#1785 review).
pub(crate) fn rename_claims(cs: &pbps_model::ChangeSet) -> Vec<TableName> {
    let mut out: Vec<TableName> = cs
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::RenameTable { from, to, .. } = &p.change {
                Some(claims_of(from, to))
            } else {
                None
            }
        })
        .flatten()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Each name [`refuse_uninventoried_occupants`] refuses, described, with the
/// position of the cross-schema rename whose carried object meets a holder,
/// or `None` for a name a change creates.
fn name_clashes(
    changes: &[pbps_model::PlannedChange],
    occupants: &[pbps_pg::catalog::NameOccupant],
    relations: &[(TableName, &'static str)],
    catalog: &pbps_dialect::slots::CatalogFacts,
) -> Vec<(Option<usize>, String)> {
    let mut present = Occupant::with_detaches(occupants, catalog);
    let mut held: Vec<(TableName, String)> = relations
        .iter()
        .map(|(name, kind)| {
            (
                name.clone(),
                format!("{kind} `{name}` already is", name = name.shown()),
            )
        })
        .collect();
    let mut created: BTreeSet<TableName> = BTreeSet::new();
    let mut sequences = BTreeMap::<TableName, pbps_model::ColumnRef>::new();
    let mut taken: Vec<(Option<usize>, String)> = Vec::new();
    for (at, p) in changes.iter().enumerate() {
        present.retain(|o| !o.freed_by(&p.change));
        held.retain(|(name, _)| !relation_freed_by(&p.change, name));
        if let pbps_model::Change::RenameColumn {
            table, from, to, ..
        } = &p.change
        {
            for o in &mut present {
                o.column_renamed(table, from, to);
            }
        }
        if let pbps_model::Change::RenameTable { from, to, .. } = &p.change {
            let moved: Vec<usize> = (0..present.len())
                .filter(|&k| present[k].moved(from, to))
                .collect();
            for &k in &moved {
                let o = &present[k];
                // `SET SCHEMA` refuses to carry one onto a name the plan
                // already created there, or that another relation holds
                // there, one an earlier rename carried in included
                // (measured on 16 and 18, #1749).
                if created.contains(&o.name) {
                    taken.push((None, o.taken()));
                }
                let holder = present
                    .iter()
                    .enumerate()
                    .find(|&(j, q)| j != k && q.name == o.name);
                let there = match holder {
                    Some((_, q)) => Some(match &q.carried {
                        Some((f, t)) => format!(
                            "{} `{}`, which the rename of `{f}` to `{t}` moves there first",
                            q.found.kind,
                            q.found.name.shown(),
                            f = f.shown(),
                            t = t.shown()
                        ),
                        None => format!("{} `{}` already is", q.found.kind, q.name.shown()),
                    }),
                    None => held
                        .iter()
                        .find(|(name, _)| name == &o.name)
                        .map(|(_, there)| there.clone()),
                };
                if let Some(there) = there {
                    taken.push((
                        Some(at),
                        format!(
                            "`{}`: this plan moves {} `{}` on `{from}` there, with the rename of \
                             `{from}` to `{to}`, where {there}",
                            o.name.shown(),
                            o.found.kind,
                            o.found.name.shown(),
                            from = from.shown(),
                            to = to.shown()
                        ),
                    ));
                }
            }
            // What the table takes on its way, with what it carried there:
            // a name an occupant or another table or view still holds is
            // refused by the engine. The ordering by handovers relies on
            // this: an order that runs a rename before what frees its name
            // has a clash here (#1785 review).
            for name in claims_of(from, to) {
                let there = present
                    .iter()
                    .find(|q| q.name == name)
                    .map(|q| match &q.carried {
                        Some((f, t)) => format!(
                            "{} `{}`, which the rename of `{f}` to `{t}` moves there first",
                            q.found.kind,
                            q.found.name.shown(),
                            f = f.shown(),
                            t = t.shown()
                        ),
                        None => format!("{} `{}` already is", q.found.kind, q.name.shown()),
                    })
                    .or_else(|| {
                        held.iter()
                            .find(|(held, _)| held == &name)
                            .map(|(_, there)| there.clone())
                    });
                if let Some(there) = there {
                    let how = if &name == to {
                        String::new()
                    } else {
                        " on its way, `SET SCHEMA` before `RENAME TO`".to_owned()
                    };
                    taken.push((
                        Some(at),
                        format!(
                            "`{name}`: this plan renames `{from}` to `{to}`, which takes \
                             `{name}`{how}, where {there}",
                            name = name.shown(),
                            from = from.shown(),
                            to = to.shown()
                        ),
                    ));
                }
            }
            // The table itself holds its new name from here on.
            held.push((
                to.clone(),
                format!(
                    "table `{to}`, which the rename of `{from}` to `{to}` moves there first",
                    to = to.shown(),
                    from = from.shown()
                ),
            ));
        }
        for name in relation_names_created(&p.change) {
            if let Some(column) = sequences.get(&name) {
                taken.push((
                    None,
                    format!(
                        "relation `{}` this plan creates meets sequence `{}` for column `{}`",
                        name.shown(),
                        name.shown(),
                        column.shown()
                    ),
                ));
            }
            let detaching =
                if let pbps_model::Change::DetachPartition { uid, table, .. } = &p.change {
                    Some((uid, table))
                } else {
                    None
                };
            // Moving clones were released above. Only an exact retained
            // clone may still hold its batch's name: ownership alone also
            // includes sequences and indexes the detach never renames
            // (DEC-2230.1). The fact uses the original catalog identity even
            // after an earlier change moves its owner.
            taken.extend(
                present
                    .iter()
                    .filter(|o| {
                        use pbps_dialect::slots::{Namespace, SlotKey};
                        o.name == name
                            && !detaching.is_some_and(|(uid, table)| {
                                o.owner.as_ref() == Some(table)
                                    && catalog.detach_retained.get(&SlotKey {
                                        namespace: Namespace::Relations(
                                            o.found.name.schema.clone(),
                                        ),
                                        name: o.found.name.name.clone(),
                                    }) == Some(uid)
                            })
                    })
                    .map(|o| {
                        (
                            None,
                            match detaching {
                                Some((_, table)) => format!(
                                    "detach of `{}` claims `{}`: {}",
                                    table.shown(),
                                    name.shown(),
                                    o.taken()
                                ),
                                None => o.taken(),
                            },
                        )
                    }),
            );
            if let Some((_, table)) = detaching {
                taken.extend(
                    held.iter()
                        .filter(|(at, _)| at == &name)
                        .map(|(_, holder)| {
                            (
                                None,
                                format!(
                                    "detach of `{}` claims `{}` where {holder}",
                                    table.shown(),
                                    name.shown()
                                ),
                            )
                        }),
                );
            }
            created.insert(name);
        }
        for (name, column) in sequence_names_claimed(&p.change) {
            let claim = format!(
                "sequence `{}` for column `{}`",
                name.shown(),
                column.shown()
            );
            for occupant in present.iter().filter(|o| o.name == name) {
                taken.push((None, format!("{claim}: {}", occupant.taken())));
            }
            if let Some((_, holder)) = held.iter().find(|(at, _)| at == &name) {
                taken.push((
                    None,
                    format!("{claim} claims `{}` where {holder}", name.shown()),
                ));
            } else if created.contains(&name) {
                taken.push((
                    None,
                    format!(
                        "{claim} meets relation `{}` this plan creates",
                        name.shown()
                    ),
                ));
            }
            held.push((
                name.clone(),
                format!("{claim}, which this plan claims first"),
            ));
            sequences.insert(name.clone(), column);
            created.insert(name);
        }
    }
    taken
}

/// Whether `change` frees the table or view name `name` (#1765): its table
/// dropped or renamed away, or its view dropped.
// The complement is every change that frees no table or view name.
#[allow(clippy::wildcard_enum_match_arm)]
fn relation_freed_by(change: &pbps_model::Change, name: &TableName) -> bool {
    use pbps_model::Change;
    match change {
        Change::DropTable { name: table, .. } | Change::RenameTable { from: table, .. } => {
            table == name
        }
        Change::DropModule {
            id: ModuleId::Named(view),
            ..
        } => view == name,
        _ => false,
    }
}

/// The names this plan's cross-schema renames carry `occupants` onto, in
/// their destination schemas (#1749). What holds one of them now is outside
/// what the plan creates and what the moved tables own, so the caller reads
/// it in a second pass before [`refuse_uninventoried_occupants`].
pub(crate) fn carried_names(
    cs: &pbps_model::ChangeSet,
    occupants: &[pbps_pg::catalog::NameOccupant],
) -> Vec<TableName> {
    let mut present = Occupant::all(occupants);
    let mut out = Vec::new();
    for p in &cs.changes {
        present.retain(|o| !o.freed_by(&p.change));
        if let pbps_model::Change::RenameTable { from, to, .. } = &p.change {
            for o in &mut present {
                if o.moved(from, to) {
                    out.push(o.name.clone());
                }
            }
        }
    }
    out
}

/// A [`pbps_pg::catalog::NameOccupant`] as the plan has left it so far: its
/// name and its owner's as they are at that point. A change matches it by
/// the name the change carries, so one emitted before a rename names the
/// table as it was, and one after it as it is (`rename_order` emits either,
/// #1723).
struct Occupant<'a> {
    found: &'a pbps_pg::catalog::NameOccupant,
    name: TableName,
    owner: Option<TableName>,
    /// The column of its owner it belongs to, an owned sequence's, under
    /// the name the changes replayed so far give it (#1930 review).
    owner_column: Option<pbps_model::ColumnName>,
    /// The cross-schema rename that last carried it, by its two names.
    carried: Option<(TableName, TableName)>,
    detached_by: Option<pbps_model::Uid>,
}

impl<'a> Occupant<'a> {
    /// Each occupant as the catalog read it, before any change.
    fn all(occupants: &'a [pbps_pg::catalog::NameOccupant]) -> Vec<Self> {
        occupants
            .iter()
            .map(|found| Occupant {
                found,
                name: found.name.clone(),
                owner: found.owner.clone(),
                owner_column: found.owner_column.clone(),
                carried: None,
                detached_by: None,
            })
            .collect()
    }

    fn with_detaches(
        occupants: &'a [pbps_pg::catalog::NameOccupant],
        facts: &pbps_dialect::slots::CatalogFacts,
    ) -> Vec<Self> {
        use pbps_dialect::slots::{Namespace, SlotKey};
        let mut present = Self::all(occupants);
        for o in &mut present {
            o.detached_by = facts
                .detach_releases
                .get(&SlotKey {
                    namespace: Namespace::Relations(o.name.schema.clone()),
                    name: o.name.name.clone(),
                })
                .cloned();
        }
        present
    }

    /// Whether `change` takes it away: an index it drops, a unique
    /// constraint's or a primary key's it drops, and what a table or a
    /// column it drops owns. Anything else, a dropped column's indexes among
    /// them, is kept, which can refuse a plan the engine would take and
    /// never takes one it would refuse.
    // The complement is every change that frees no occupant.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn freed_by(&self, change: &pbps_model::Change) -> bool {
        use pbps_model::Change;
        let owned_by = |t: &TableName| self.owner.as_ref() == Some(t);
        let at = |t: &TableName, name: &str| self.name.schema == t.schema && self.name.name == name;
        match change {
            Change::DropIndex { table, name } | Change::DropUnique { table, name } => {
                at(table, name.raw())
            }
            // A connected plan reads each key with the name its index has,
            // a numbered `<table>_pkey1` included (measured, #1723). An
            // unnamed one's is named after its table as the catalog read it,
            // which a rename does not change (#1082 review).
            Change::SetPrimaryKey {
                table,
                from: Some(key),
                ..
            } => match &key.name {
                Some(name) => at(table, name.raw()),
                None => {
                    owned_by(table)
                        && self.found.owner.as_ref().is_some_and(|catalog| {
                            self.found.name.name
                                == pbps_pg::implicit_primary_key_name(&catalog.name)
                        })
                }
            },
            Change::DetachPartition { uid, table, .. } => {
                owned_by(table) && self.detached_by.as_ref() == Some(uid)
            }
            Change::DropTable { name, .. } => owned_by(name),
            Change::DropColumn { column, .. } => {
                owned_by(&column.table) && self.owner_column.as_ref() == Some(&column.name)
            }
            // A numbering change that drops its column's sequence, or renames
            // it, frees the name it had (#1930 review).
            Change::AlterColumnNumbering {
                column, from, to, ..
            } => {
                use pbps_model::ValueSource;
                let frees = match (from, to) {
                    (ValueSource::Identity(_), ValueSource::Identity(_)) => false,
                    (ValueSource::Sequence(was), ValueSource::Sequence(now)) => {
                        was.name.is_some() && now.name.is_some() && was.name != now.name
                    }
                    (ValueSource::Identity(_) | ValueSource::Sequence(_), _) => true,
                    (
                        ValueSource::None | ValueSource::Default(_) | ValueSource::Generated(_),
                        _,
                    ) => false,
                };
                frees && owned_by(&column.table) && self.owner_column.as_ref() == Some(&column.name)
            }
            _ => false,
        }
    }

    /// Follows the rename of its owning column.
    fn column_renamed(
        &mut self,
        table: &TableName,
        from: &pbps_model::ColumnName,
        to: &pbps_model::ColumnName,
    ) {
        if self.owner.as_ref() == Some(table) && self.owner_column.as_ref() == Some(from) {
            self.owner_column = Some(to.to_owned());
        }
    }

    /// Follows the rename of its owner: the owner takes the new name, and a
    /// move to another schema takes it along under its own name. Whether
    /// this rename carried it into another schema.
    fn moved(&mut self, from: &TableName, to: &TableName) -> bool {
        if self.owner.as_ref() != Some(from) {
            return false;
        }
        self.owner = Some(to.clone());
        if from.schema == to.schema {
            return false;
        }
        self.name.schema.clone_from(&to.schema);
        self.carried = Some((from.clone(), to.clone()));
        true
    }

    /// The refusal for a name it holds.
    fn taken(&self) -> String {
        let (name, kind, found) = (&self.name, self.found.kind, &self.found.name);
        match (&self.carried, &self.found.owner) {
            (Some((from, to)), _) => format!(
                "`{name}`: this plan moves {kind} `{found}` on `{from}` there first, with the \
                 rename of `{from}` to `{to}`",
                name = name.shown(),
                found = found.shown(),
                from = from.shown(),
                to = to.shown()
            ),
            (None, Some(owner)) => {
                format!(
                    "`{name}`: the database already has {kind} `{name}` on `{owner}`",
                    name = name.shown(),
                    owner = owner.shown()
                )
            }
            (None, None) => format!(
                "`{name}`: the database already has {kind} `{name}`",
                name = name.shown()
            ),
        }
    }
}

/// The partitions this plan creates `UNLOGGED`, each with its parent (#1595).
fn unlogged_partitions_created(
    changes: &[pbps_model::PlannedChange],
) -> Vec<(usize, &TableName, &TableName)> {
    changes
        .iter()
        .enumerate()
        .filter_map(|(at, p)| {
            unlogged_partition(&p.change).map(|(partition, parent)| (at, partition, parent))
        })
        .collect()
}

/// The same statement-time parents the refusal compares, spelled for the
/// catalog read that supplies its referencers.
pub(crate) fn stored_unlogged_parents(
    names: &pbps_model::plan_names::PlanNames<'_>,
) -> Vec<TableName> {
    use pbps_model::plan_names::{At, Holder};
    unlogged_partitions_created(names.changes())
        .into_iter()
        .filter_map(
            |(at, _, parent)| match names.holder(parent, At::Before(at)) {
                Holder::Unmanaged => Some(parent.clone()),
                Holder::Tracked(_) | Holder::Vacant => names.stored(parent, At::Before(at)),
            },
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The partition `change` leaves `UNLOGGED`, with its parent: one it creates
/// so, or a table it attaches that the plan leaves so, which is a new
/// partition under the parent's referencing keys all the same (#1545).
fn unlogged_partition(change: &pbps_model::Change) -> Option<(&TableName, &TableName)> {
    if let pbps_model::Change::CreateTable { name, table, .. } = change {
        return table
            .partition_of
            .as_ref()
            .filter(|_| table.unlogged)
            .map(|of| (name, &of.parent));
    }
    if let pbps_model::Change::AttachPartition {
        table,
        parent,
        shape,
        ..
    } = change
    {
        return shape.unlogged.then_some((table, parent));
    }
    None
}

/// Refuses an `UNLOGGED` partition this plan creates under a parent that a
/// permanent table's foreign key references (#1595, DEC-1595.1). The engine
/// takes it, and a crash would empty the partition under rows that still
/// reference it. A declared referencing table is refused before planning
/// (DEC-1580.1); this is the one the declarations do not hold, read from the
/// catalog. A key this plan takes away is not one: a table it drops or makes
/// unlogged, or a key it drops, anywhere in a transactional plan and before
/// the partition's `CREATE` in a staged one.
// The complement is every change that leaves a referencing key in place.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn refuse_permanent_referencers(
    cs: &pbps_model::ChangeSet,
    names: &pbps_model::plan_names::PlanNames<'_>,
    referencers: &[pbps_pg::catalog::PermanentReferencer],
    staged: bool,
) -> anyhow::Result<()> {
    use pbps_model::{
        Change,
        plan_names::{At, Holder},
    };
    // The catalog names each referencing table as the database does now, and
    // the plan's changes on a table it also renames carry the new name, so
    // each is read back to the catalog's before it is compared: by uid where
    // the change has one, since a rename's destination can be a name the
    // plan drops first, and that drop names the table that leaves (#1612
    // review). A key drop carries its execution address, which can precede
    // its owner's rename (DECISIONS 496); resolve it during the walk below.
    // Each removal with its place in the plan, and a whole table's with no
    // key. In a staged plan only one that runs before the partition's
    // `CREATE` counts: a staged apply commits between statements, and a
    // removal after it would leave the unlogged partition under a permanent
    // key until it ran. The persistence switch sorts after table creation, so
    // there making the referencing table unlogged takes a plan of its own
    // first. A transactional plan commits both or neither (#1612 review).
    let mut removals: Vec<(usize, TableName, Option<&str>)> = Vec::new();
    for (at, p) in cs.changes.iter().enumerate() {
        match &p.change {
            Change::DropTable { uid, .. } => {
                if let Some(table) = names.table_at(uid, At::Start) {
                    removals.push((at, table.clone(), None));
                }
            }
            Change::SetTablePersistence {
                uid,
                unlogged: true,
                ..
            } => {
                if let Some(table) = names.table_at(uid, At::Start) {
                    removals.push((at, table.clone(), None));
                }
            }
            Change::DropForeignKey { table, name } => {
                // Only earlier renames decide which stored table owns this
                // removal; a later name handoff must not change it (DEC-1770.1).
                let stored = match names.holder(table, At::Before(at)) {
                    Holder::Unmanaged => Some(table.clone()),
                    Holder::Tracked(_) | Holder::Vacant => names.stored(table, At::Before(at)),
                };
                if let Some(stored) = stored {
                    removals.push((at, stored, Some(name.raw())));
                }
            }
            _ => {}
        }
    }
    let removed_before = |at: usize, table: &TableName, key: &str| {
        removals
            .iter()
            .any(|(j, t, k)| (!staged || *j < at) && t == table && k.is_none_or(|k| k == key))
    };
    let mut lines = Vec::new();
    for (at, p) in cs.changes.iter().enumerate() {
        let Some((partition, parent)) = unlogged_partition(&p.change) else {
            continue;
        };
        // The parent too, which the plan can rename while it adds the
        // partition under the new name (#1613, #1690); the message names it
        // as declared.
        let in_catalog = match names.holder(parent, At::Before(at)) {
            Holder::Unmanaged => Some(parent.clone()),
            Holder::Tracked(_) | Holder::Vacant => names.stored(parent, At::Before(at)),
        };
        let Some(in_catalog) = in_catalog else {
            continue;
        };
        for r in referencers.iter().filter(|r| r.parent == in_catalog) {
            // A leaf's copy goes with the key its partitioned table declares,
            // which is the one a plan names.
            if removed_before(at, &r.table, &r.key)
                || removed_before(at, &r.root_table, &r.root_key)
            {
                continue;
            }
            let through = if r.table == r.root_table {
                String::new()
            } else {
                format!(" (on its partition {} as `{}`)", r.table.shown(), r.key)
            };
            lines.push(format!(
                "{partition} would be {} unlogged under {parent}, which {}'s foreign key \
                 `{}`{through} references; a crash would empty {partition} and leave rows of {} \
                 referencing nothing",
                if matches!(p.change, Change::AttachPartition { .. }) {
                    "attached"
                } else {
                    "created"
                },
                r.root_table.shown(),
                r.root_key,
                r.table.shown(),
                partition = partition.shown(),
                parent = parent.shown()
            ));
        }
    }
    if lines.is_empty() {
        return Ok(());
    }
    bail!(
        "{} unlogged partition(s) under a permanent table's foreign key:\n  {}\n\
         PostgreSQL refuses a permanent table's key to an unlogged table, but not through a \
         partitioned one. Declare the partition permanent, or first drop that key or make its \
         table unlogged (in a plan before this one, when this one is staged); a table the \
         declarations do not hold is the operator's to change (DEC-1595.1).",
        lines.len(),
        lines.join("\n  ")
    );
}

/// Each table this plan makes unlogged, as declared and as the catalog names
/// it when the plan starts, which is when the publications are read (#1489).
pub(crate) fn tables_made_unlogged(
    names: &pbps_model::plan_names::PlanNames<'_>,
) -> Vec<(TableName, TableName)> {
    let mut out = Vec::new();
    for p in names.changes() {
        if let pbps_model::Change::SetTablePersistence {
            uid,
            table,
            unlogged: true,
        } = &p.change
            && let Some(stored) = names.table_at(uid, pbps_model::plan_names::At::Start)
        {
            out.push((table.clone(), stored.clone()));
        }
    }
    out
}

/// Refuses a plan that makes a table unlogged while a publication names it
/// (#1489, DEC-1489.1). The engine refuses that `SET UNLOGGED` at apply; asked
/// here, the refusal names the publication before anything runs.
pub(crate) fn refuse_published_unlogged(
    made_unlogged: &[(TableName, TableName)],
    naming: &[pbps_pg::catalog::NamingPublication],
) -> anyhow::Result<()> {
    use pbps_model::escape::Escaped;
    let mut lines = Vec::new();
    for (declared, stored) in made_unlogged {
        for found in naming.iter().filter(|n| n.table == *stored) {
            // The statement is shown only where it runs exactly as shown: a
            // name whose display escaping would change the statement's
            // spelling (a backslash, a control character) gets the words
            // instead, since a quoted identifier takes those literally.
            let remove = found
                .drop_statement()
                .filter(|statement| Escaped(statement).to_string() == *statement)
                .map_or_else(
                    || "remove it from that publication".to_owned(),
                    |statement| format!("remove it there (`{statement}`)"),
                );
            lines.push(format!(
                "{} is listed by the publication `{}`; {remove}, or keep the table logged",
                declared.shown(),
                Escaped(&found.publication),
            ));
        }
    }
    if lines.is_empty() {
        return Ok(());
    }
    bail!(
        "{} table(s) this plan makes unlogged are published:\n  {}\n\
         PostgreSQL refuses `SET UNLOGGED` on a table a publication lists, because an \
         unlogged table cannot be replicated. A publication is outside the declarations, so \
         it is the operator's to change (DEC-1489.1).",
        lines.len(),
        lines.join("\n  ")
    );
}

/// The tables this plan moves to another schema, by the catalog's names for
/// them: `SET SCHEMA` carries their indexes and owned sequences along
/// (#1084), so the occupant read asks what each of them owns.
// The complement is every change that moves no table between schemas.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn transferred_tables(cs: &pbps_model::ChangeSet) -> Vec<TableName> {
    use pbps_model::Change;
    cs.changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::RenameTable { from, to, .. } if from.schema != to.schema => Some(from.clone()),
            _ => None,
        })
        .collect()
}

/// The `sys.objects` names this plan creates on SQL Server: its tables, a
/// system-versioned table's history (#1176), and the modules it creates that
/// it does not also drop. A module dropped and created in one plan is a
/// rebuild, and its name is already its own. A trigger's object is in its
/// table's schema.
// The complement is every change that creates no object name.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn created_object_names(cs: &pbps_model::ChangeSet) -> Vec<TableName> {
    use pbps_model::Change;
    let dropped: BTreeSet<&ModuleId> = cs
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::DropModule { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    cs.changes
        .iter()
        .flat_map(|p| match &p.change {
            Change::CreateTable { name, table, .. } => {
                let history = table
                    .system_time
                    .as_ref()
                    .and_then(|st| st.versioning.as_ref())
                    .map(|v| v.history.clone());
                std::iter::once(name.clone()).chain(history).collect()
            }
            Change::CreateModule { id, .. } if !dropped.contains(id) => vec![module_object(id)],
            _ => Vec::new(),
        })
        .collect()
}

/// The `sys.objects` entry a module is, on SQL Server.
fn module_object(id: &ModuleId) -> TableName {
    match id {
        ModuleId::Named(name) => name.clone(),
        ModuleId::Routine(routine) => routine.name.clone(),
        ModuleId::Trigger { on, name } => TableName::new(on.schema.clone(), name.clone()),
    }
}

/// What the SQL Server occupant read has to cover beyond the names the plan
/// creates: the constraints and generated defaults it adds, which the walk
/// checks like a created table (#1201) and which only this read can find
/// already held in the database (#1214); the names its renames may move a
/// generated default to (#975),
/// the generated one and the digested fallback taken when that is held
/// (DEC-981.1), and the tables whose constraints, defaults and triggers its
/// changes move or remove, by the catalog's names for them.
// The complement is every change that moves or removes no object.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn object_reads(
    cs: &pbps_model::ChangeSet,
    index: &pbps_model::plan_names::PlanNames<'_>,
) -> (Vec<TableName>, Vec<TableName>) {
    use pbps_model::Change;
    use pbps_mssql::emit::default_constraint_name as generated;
    use pbps_mssql::emit::fallback_default_constraint_name as fallback_name;
    let in_schema = |t: &TableName, name: pbps_model::ObjectLocalName| {
        TableName::new(t.schema.clone(), name.into_raw())
    };
    let mut names = created_object_names(cs);
    let mut parents = Vec::new();
    for (at, p) in cs.changes.iter().enumerate() {
        let now = |t: &TableName| {
            index
                .stored(t, pbps_model::plan_names::At::Before(at))
                .unwrap_or_else(|| t.clone())
        };
        match &p.change {
            Change::RenameTable {
                from, to, defaults, ..
            } => {
                parents.push(now(from));
                // The target itself, so the walk sees an object the project
                // does not record holding it (DEC-981.2).
                names.push(to.clone());
                // And the name a transfer gives it in the new schema before
                // `sp_rename` runs, which such an object can hold too (#1362).
                if from.schema != to.schema {
                    names.push(in_schema(to, from.name.clone().into()));
                }
                for c in defaults {
                    names.push(in_schema(to, generated(to, c.raw())));
                    names.push(in_schema(to, fallback_name(to, c.raw())));
                }
            }
            Change::RenameColumn { table, to, .. } => {
                parents.push(now(table));
                names.push(in_schema(table, generated(table, to.raw())));
                names.push(in_schema(table, fallback_name(table, to.raw())));
            }
            Change::DropTable { name, .. } => parents.push(now(name)),
            Change::DropColumn { column, .. } => parents.push(now(&column.table)),
            Change::AlterColumnDefault { column, to, .. } => {
                parents.push(now(&column.table));
                if to.is_some() {
                    names.push(in_schema(
                        &column.table,
                        generated(&column.table, column.name.raw()),
                    ));
                }
            }
            Change::DropUnique { table, .. }
            | Change::DropForeignKey { table, .. }
            | Change::DropCheck { table, .. } => parents.push(now(table)),
            Change::SetPrimaryKey { table, to, .. } => {
                parents.push(now(table));
                if let Some(name) = to
                    .as_ref()
                    .and_then(|k| k.name.as_ref().map(pbps_model::ObjectLocalName::raw))
                {
                    names.push(in_schema(table, name.into()));
                }
            }
            Change::AddUnique { table, name, .. }
            | Change::AddForeignKey { table, name, .. }
            | Change::AddCheck { table, name, .. } => {
                names.push(in_schema(table, name.clone()));
            }
            Change::AddColumn {
                table,
                name,
                column,
                ..
            } if (match column.value_source {
                pbps_model::ValueSource::Default(_) => true,
                pbps_model::ValueSource::None
                | pbps_model::ValueSource::Identity(_)
                | pbps_model::ValueSource::Sequence(_)
                | pbps_model::ValueSource::Generated(_) => false,
            }) =>
            {
                names.push(in_schema(table, generated(table, name.raw())))
            }
            Change::CreateTable { name, table, .. } => {
                for (column, spec) in &table.columns {
                    if match spec.value_source {
                        pbps_model::ValueSource::Default(_) => true,
                        pbps_model::ValueSource::None
                        | pbps_model::ValueSource::Identity(_)
                        | pbps_model::ValueSource::Sequence(_)
                        | pbps_model::ValueSource::Generated(_) => false,
                    } {
                        names.push(in_schema(name, generated(name, column.raw())));
                    }
                }
                let named = table
                    .primary_key
                    .iter()
                    .filter_map(|k| k.name.clone())
                    .chain(table.unique.keys().cloned())
                    .chain(table.foreign_keys.keys().cloned())
                    .chain(table.checks.keys().cloned());
                names.extend(named.map(|n| in_schema(name, n)));
            }
            _ => {}
        }
    }
    for list in [&mut names, &mut parents] {
        list.sort();
        list.dedup();
    }
    (names, parents)
}

/// Every name the walk may hold that the collation question has to see
/// (#1215): the names the plan reads (`object_reads`), the names the catalog
/// found, each renamed table's new name, each module it creates (a rebuilt
/// one too), and the destination of each occupant a cross-schema
/// `RenameTable` carries. The walk moves a carried constraint into the new schema under
/// its own name, so `archive.Ck_Name` arrives there without ever being read,
/// and a later `archive.ck_name` must still meet it (review of #1240).
// The complement is every change that moves no table between schemas.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn alike_candidates(
    cs: &pbps_model::ChangeSet,
    names: &[TableName],
    occupants: &[pbps_mssql::catalog::NameOccupant],
) -> Vec<TableName> {
    use pbps_model::Change;
    let mut out: Vec<TableName> = names.to_vec();
    out.extend(occupants.iter().map(|o| o.name.clone()));
    for p in &cs.changes {
        // A renamed table's new name, which the plan uses without reading,
        // and every module the plan creates, a rebuilt one included, which
        // `created_object_names` leaves out of the read (review of #1240).
        match &p.change {
            Change::RenameTable { to, .. } => out.push(to.clone()),
            Change::CreateModule { id, .. } => out.push(module_object(id)),
            _ => {}
        }
    }
    out.extend(carried_destinations(cs, occupants));
    out.sort();
    out.dedup();
    out
}

/// Where each cross-schema `RenameTable` carries the children the catalog
/// read found on its table: the same name, in the new schema. A transfer is
/// refused while another object holds one of them there (Msg 15530), so
/// each is read like a name the plan creates (#1366).
// The complement is every change that moves no table between schemas.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn carried_destinations(
    cs: &pbps_model::ChangeSet,
    occupants: &[pbps_mssql::catalog::NameOccupant],
) -> Vec<TableName> {
    use pbps_model::Change;
    let mut out = Vec::new();
    for p in &cs.changes {
        if let Change::RenameTable { from, to, .. } = &p.change
            && from.schema != to.schema
        {
            out.extend(
                occupants
                    .iter()
                    .filter(|o| o.parent.as_ref() == Some(from))
                    .map(|o| TableName::new(to.schema.clone(), o.name.name.clone())),
            );
        }
    }
    out.sort();
    out.dedup();
    out
}

/// One entry of a schema's `sys.objects` namespace while a plan is walked:
/// read from the catalog, or put there by an earlier change of the plan.
#[derive(Debug, Clone)]
struct Held {
    /// The same entry across every move the walk makes of it, so a refusal
    /// can name the later change that would have freed it (#1366).
    id: usize,
    name: TableName,
    kind: String,
    parent: Option<TableName>,
    column: Option<pbps_model::ColumnName>,
    /// Put where it is by the plan, rather than found there in the database.
    planned: bool,
}

/// A name a change claimed while another entry held it.
#[derive(Debug, Clone)]
struct Taken {
    /// The claiming change, by its position in the walk.
    step: usize,
    /// The holder's [`Held::id`].
    holder: usize,
    line: String,
}

/// What the catalog read and the collation question answered: fixed for one
/// connected plan, and shared by every order of it that is tried (#1366).
pub(crate) struct NameFacts {
    occupants: Vec<pbps_mssql::catalog::NameOccupant>,
    /// The catalog's spelling of each name the plan asked about. The plan
    /// spells a created name its own way, and the database's collation may
    /// read it as an object the catalog spells differently.
    spelling: BTreeMap<TableName, TableName>,
    /// Each name the database reads as another, pointing at the first
    /// spelling of it, so two names meet exactly when they point at the same
    /// one.
    first: BTreeMap<TableName, TableName>,
}

impl NameFacts {
    /// `alike` is the pairs of names the plan uses that the database reads as
    /// one name under its collation, as `catalog::object_names_alike` answers
    /// (#1215); none is how a case-sensitive database answers.
    pub(crate) fn new(
        occupants: &[pbps_mssql::catalog::NameOccupant],
        alike: &[(TableName, TableName)],
    ) -> Self {
        let mut first: BTreeMap<TableName, TableName> = BTreeMap::new();
        for (earlier, later) in alike {
            let root = first.get(earlier).unwrap_or(earlier).clone();
            first.insert(later.clone(), root);
        }
        NameFacts {
            occupants: occupants.to_vec(),
            spelling: occupants
                .iter()
                .map(|o| (o.wanted.clone(), o.name.clone()))
                .collect(),
            first,
        }
    }

    fn one<'n>(&'n self, name: &'n TableName) -> &'n TableName {
        self.first.get(name).unwrap_or(name)
    }

    /// Whether where the drop of module `id` runs among `changes`' table
    /// renames can change what the walk finds (#1680 review). Only then may
    /// the rename search move it. Two things must hold:
    /// - The catalog read found an object under its name. Otherwise its drop
    ///   frees nothing (`Walk::step`).
    /// - A table rename can claim that name, under the collation. The renames
    ///   are the only changes among the drops that claim a name. Every claim
    ///   after them runs after every module drop whatever the order, and every
    ///   claim before them runs before it.
    ///
    /// A rename can claim its target, the stop a transfer passes through, the
    /// generated and fallback names of each default it moves, and, across
    /// schemas, the name of each child the read found that it carries
    /// (`Walk::rename_table`). Missing one leaves the drop where it is, as
    /// before #1680.
    // The complement is every change that is not a table rename.
    #[allow(clippy::wildcard_enum_match_arm)]
    pub(crate) fn module_drop_matters(
        &self,
        changes: &[pbps_model::PlannedChange],
        id: &ModuleId,
    ) -> bool {
        use pbps_mssql::emit::default_constraint_name as generated;
        use pbps_mssql::emit::fallback_default_constraint_name as fallback_name;
        let name = module_object(id);
        if !self.occupants.iter().any(|o| o.name == name) {
            return false;
        }
        let module = self.one(&name);
        let in_schema = |t: &TableName, n: &str| TableName::new(t.schema.clone(), n);
        changes.iter().any(|p| {
            let pbps_model::Change::RenameTable {
                from, to, defaults, ..
            } = &p.change
            else {
                return false;
            };
            let mut claims = vec![to.clone()];
            for c in defaults {
                claims.push(in_schema(to, generated(to, c.raw()).raw()));
                claims.push(in_schema(to, fallback_name(to, c.raw()).raw()));
            }
            if from.schema != to.schema {
                claims.push(in_schema(to, &from.name));
                // The children the read found on this table, as
                // `carried_destinations` reads them.
                claims.extend(
                    self.occupants
                        .iter()
                        .filter(|o| o.parent.as_ref() == Some(from))
                        .map(|o| in_schema(to, &o.name.name)),
                );
            }
            claims.iter().any(|c| self.one(c) == module)
        })
    }
}

/// An entry a change puts into the namespace.
struct Entry {
    name: TableName,
    kind: String,
    parent: Option<TableName>,
    column: Option<pbps_model::ColumnName>,
}

/// SQL Server's `sys.objects` namespace as a plan's changes move it, from
/// what the catalog read found (#1077). Each change moves it as the emitter's
/// statements do:
/// - drops remove an object, and a dropped table takes its children along;
/// - a rename moves a table; a transfer moves its children to the new schema;
/// - a table or column rename moves a generated default, found under either
///   name `pbps` may have left it under, to its new generated name when
///   nothing holds that name at that point (#975), and otherwise to the
///   digested fallback when nothing holds that (DEC-981.1);
/// - a dropped column takes its default, and a changed default is replaced;
/// - added constraints, defaults, tables and modules take their names, each
///   refused if the walk holds that name when it is added (#1201).
///
/// So a name freed before the `CREATE` runs is free, and a name something
/// moves into first is taken, whatever order the changes that do it come in.
/// Cloned to try one order of a plan without committing to it
/// (`object_order`).
#[derive(Clone)]
pub(crate) struct Walk<'f> {
    facts: &'f NameFacts,
    held: Vec<Held>,
    ids: usize,
    taken: Vec<Taken>,
    /// For each entry a change moved away or removed, the first change that
    /// did.
    freed: BTreeMap<usize, usize>,
}

impl<'f> Walk<'f> {
    pub(crate) fn new(facts: &'f NameFacts) -> Self {
        let mut walk = Walk {
            facts,
            held: Vec::new(),
            ids: 0,
            taken: Vec::new(),
            freed: BTreeMap::new(),
        };
        for o in &facts.occupants {
            if !walk.held.iter().any(|h| h.name == o.name) {
                walk.push(
                    Entry {
                        name: o.name.clone(),
                        kind: o.kind.clone(),
                        parent: o.parent.clone(),
                        column: o.parent_column.clone(),
                    },
                    false,
                );
            }
        }
        walk
    }

    /// Every change, in the order given.
    pub(crate) fn run(&mut self, changes: &[pbps_model::PlannedChange]) {
        for (step, p) in changes.iter().enumerate() {
            self.step(step, &p.change);
        }
    }

    /// Whether no change so far claimed a name another entry held.
    pub(crate) fn is_clear(&self) -> bool {
        self.taken.is_empty()
    }

    /// The namespace as it stands, independent of how each entry got there
    /// and of which order put it there: two trial orders reaching the same
    /// state have the same future.
    pub(crate) fn state(
        &self,
    ) -> Vec<(
        TableName,
        String,
        Option<TableName>,
        Option<pbps_model::ColumnName>,
    )> {
        let mut out: Vec<_> = self
            .held
            .iter()
            .map(|h| {
                (
                    h.name.clone(),
                    h.kind.clone(),
                    h.parent.clone(),
                    h.column.clone(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn push(&mut self, entry: Entry, planned: bool) -> usize {
        let id = self.ids;
        self.ids += 1;
        self.held.push(Held {
            id,
            name: entry.name,
            kind: entry.kind,
            parent: entry.parent,
            column: entry.column,
            planned,
        });
        id
    }

    /// The entry holding `name`: the same name, the catalog's spelling of it,
    /// or one the database reads as the same, other than the entries in
    /// `except`.
    fn holder(&self, name: &TableName, except: &[usize]) -> Option<usize> {
        let spelled = self.facts.spelling.get(name);
        self.held.iter().position(|h| {
            !except.contains(&h.id)
                && (&h.name == name
                    || Some(&h.name) == spelled
                    || self.facts.one(&h.name) == self.facts.one(name))
        })
    }

    fn refuse(&mut self, step: usize, name: &TableName, at: usize) {
        let h = &self.held[at];
        let what = match &h.parent {
            Some(parent) => format!(
                "{} `{}` on `{parent}`",
                h.kind,
                h.name.shown(),
                parent = parent.shown()
            ),
            None => format!("{} `{}`", h.kind, h.name.shown()),
        };
        let line = if h.planned {
            format!(
                "`{name}`: this plan puts {what} there first",
                name = name.shown()
            )
        } else {
            format!(
                "`{name}`: the database already has {what}",
                name = name.shown()
            )
        };
        self.taken.push(Taken {
            step,
            holder: h.id,
            line,
        });
    }

    /// Every name a change adds is checked against what the walk holds at
    /// that point, not only a created table's or module's: a constraint or
    /// default the plan adds under a name it or the database already holds
    /// fails with Msg 2714 just the same (#1201).
    fn claim(&mut self, step: usize, entry: Entry) {
        if let Some(at) = self.holder(&entry.name, &[]) {
            self.refuse(step, &entry.name, at);
        }
        self.push(entry, true);
    }

    /// Removes every entry `gone` names.
    fn release(&mut self, step: usize, gone: impl Fn(&Held) -> bool) {
        for h in &self.held {
            if gone(h) {
                self.freed.entry(h.id).or_insert(step);
            }
        }
        self.held.retain(|h| !gone(h));
    }

    /// `sp_rename` of a generated default, taken only when it is at `old` on
    /// `column` of `table`: to `new` when nothing holds it, else to
    /// `fallback` when nothing holds that (the emitter's own guards, #991,
    /// DEC-981.1). A hand-named default of another column under that name is
    /// not moved.
    fn move_default(
        &mut self,
        step: usize,
        table: &TableName,
        column: &pbps_model::ColumnName,
        old: TableName,
        new: TableName,
        fallback: TableName,
    ) {
        if old == new {
            return;
        }
        let new = if self.holder(&new, &[]).is_none() {
            new
        } else if fallback != new && fallback != old && self.holder(&fallback, &[]).is_none() {
            fallback
        } else {
            return;
        };
        if let Some(h) = self.held.iter_mut().find(|h| {
            h.name == old
                && h.kind == "default constraint"
                && h.parent.as_ref() == Some(table)
                && h.column.as_ref() == Some(column)
        }) {
            h.name = new;
            h.planned = true;
            self.freed.entry(h.id).or_insert(step);
        }
    }

    fn rename_table(
        &mut self,
        step: usize,
        from: &TableName,
        to: &TableName,
        defaults: &[pbps_model::ColumnName],
    ) {
        use pbps_mssql::emit::default_constraint_name as generated;
        use pbps_mssql::emit::fallback_default_constraint_name as fallback_name;
        use pbps_mssql::emit::generated_default_names as left_under;
        let in_schema = |t: &TableName, name: &str| TableName::new(t.schema.clone(), name);
        // The table's own entry, where the read found it.
        let table: Vec<usize> = self
            .held
            .iter()
            .filter(|h| &h.name == from)
            .map(|h| h.id)
            .collect();
        // `ALTER SCHEMA ... TRANSFER` first, where the schema changes: the
        // table to its name in the new schema, which is in neither the
        // baseline nor the plan, and its constraints and defaults along with
        // it. Each is refused there (Msg 15530) while another object holds
        // its name, so each is a claim (#1362, #1366). A carried default
        // keeps the old table's generated name until the rename below moves
        // it, and holds that name meanwhile.
        let stop = in_schema(to, &from.name);
        let mut carried = Vec::new();
        for h in &mut self.held {
            if table.contains(&h.id) && h.name != stop {
                h.name = stop.clone();
                self.freed.entry(h.id).or_insert(step);
            }
            if h.parent.as_ref() == Some(from) {
                h.parent = Some(to.clone());
                if h.name.schema != to.schema {
                    h.name.schema = to.schema.clone();
                    h.planned = true;
                    self.freed.entry(h.id).or_insert(step);
                    carried.push(h.id);
                }
            }
        }
        if from.schema != to.schema {
            // Where the basename stays, the stop is the target, asked below.
            if from.name != to.name
                && let Some(at) = self.holder(&stop, &table)
            {
                self.refuse(step, &stop, at);
            }
            for id in carried {
                let Some(name) = self
                    .held
                    .iter()
                    .find(|h| h.id == id)
                    .map(|h| h.name.clone())
                else {
                    continue;
                };
                if let Some(at) = self.holder(&name, &[id]) {
                    self.refuse(step, &name, at);
                }
            }
        }
        // The rename's own target, held when it runs by an object other than
        // the table: the same name, or one the database reads as the same
        // (DEC-981.2). Asked after the table's constraints have moved with
        // it: a transfer runs before the `sp_rename`, so a carried check
        // `New` lands in the target schema first and holds `new` there
        // (review of #1346).
        let collides = self.holder(to, &table);
        if let Some(at) = collides {
            self.refuse(step, to, at);
        }
        // The table itself holds its new name from here on, whether or not
        // the read found it: a later change adding a name the database reads
        // as the same one fails (review of #1240).
        let mut found = false;
        for h in &mut self.held {
            if table.contains(&h.id) {
                if &h.name != to {
                    h.name = to.clone();
                    self.freed.entry(h.id).or_insert(step);
                }
                h.planned |= collides.is_some();
                found = true;
            }
        }
        if !found && !self.held.iter().any(|h| &h.name == to) {
            self.push(
                Entry {
                    name: to.clone(),
                    kind: "user table".into(),
                    parent: None,
                    column: None,
                },
                true,
            );
        }
        for column in defaults {
            for old in left_under(from, column.raw()) {
                self.move_default(
                    step,
                    to,
                    column,
                    in_schema(to, old.raw()),
                    in_schema(to, generated(to, column.raw()).raw()),
                    in_schema(to, fallback_name(to, column.raw()).raw()),
                );
            }
        }
    }

    /// One change, at position `step` of the order being walked.
    // The complement is every change that touches no object name.
    #[allow(clippy::wildcard_enum_match_arm)]
    pub(crate) fn step(&mut self, step: usize, change: &pbps_model::Change) {
        use pbps_model::Change;
        use pbps_mssql::emit::default_constraint_name as generated;
        use pbps_mssql::emit::fallback_default_constraint_name as fallback_name;
        use pbps_mssql::emit::generated_default_names as left_under;
        let in_schema = |t: &TableName, name: &str| TableName::new(t.schema.clone(), name);
        let default_of = |table: &TableName, column: &str| Entry {
            name: in_schema(table, generated(table, column).raw()),
            kind: "default constraint".into(),
            parent: Some(table.clone()),
            column: Some(column.into()),
        };
        let constraint = |table: &TableName, name: &str, kind: &str| Entry {
            name: in_schema(table, name),
            kind: kind.into(),
            parent: Some(table.clone()),
            column: None,
        };
        let object = |name: &TableName, kind: &str| Entry {
            name: name.clone(),
            kind: kind.into(),
            parent: None,
            column: None,
        };
        match change {
            Change::DropModule { id, .. } => {
                let name = module_object(id);
                self.release(step, |h| h.name == name);
            }
            Change::DropTable { name, .. } => {
                self.release(step, |h| &h.name == name || h.parent.as_ref() == Some(name));
            }
            Change::RenameTable {
                from, to, defaults, ..
            } => self.rename_table(step, from, to, defaults),
            Change::RenameColumn {
                table,
                from,
                to,
                table_was,
                ..
            } => {
                let new = in_schema(table, generated(table, to.raw()).raw());
                let fallback = in_schema(table, fallback_name(table, to.raw()).raw());
                for was in std::iter::once(table).chain(table_was) {
                    // Still under `from`: the walk renames the column's
                    // entries below, after the default has moved.
                    for old in left_under(was, from.raw()) {
                        self.move_default(
                            step,
                            table,
                            from,
                            in_schema(table, old.raw()),
                            new.clone(),
                            fallback.clone(),
                        );
                    }
                }
                for h in &mut self.held {
                    if h.parent.as_ref() == Some(table) && h.column.as_ref() == Some(from) {
                        h.column = Some(to.clone());
                    }
                }
            }
            Change::DropColumn { column, .. } => self.release(step, |h| {
                h.parent.as_ref() == Some(&column.table) && h.column.as_ref() == Some(&column.name)
            }),
            Change::AlterColumnDefault {
                column, from, to, ..
            } => {
                if from.is_some() {
                    self.release(step, |h| {
                        h.parent.as_ref() == Some(&column.table)
                            && h.column.as_ref() == Some(&column.name)
                    });
                }
                if to.is_some() {
                    self.claim(step, default_of(&column.table, column.name.raw()));
                }
            }
            Change::AddColumn {
                table,
                name,
                column,
                ..
            } if (match column.value_source {
                pbps_model::ValueSource::Default(_) => true,
                pbps_model::ValueSource::None
                | pbps_model::ValueSource::Identity(_)
                | pbps_model::ValueSource::Sequence(_)
                | pbps_model::ValueSource::Generated(_) => false,
            }) =>
            {
                self.claim(step, default_of(table, name.raw()))
            }
            Change::DropUnique { table, name }
            | Change::DropForeignKey { table, name }
            | Change::DropCheck { table, name } => {
                let name = in_schema(table, name.raw());
                self.release(step, |h| h.name == name);
            }
            Change::AddUnique { table, name, .. } => {
                self.claim(step, constraint(table, name.raw(), "unique constraint"));
            }
            Change::AddForeignKey { table, name, .. } => {
                self.claim(
                    step,
                    constraint(table, name.raw(), "foreign key constraint"),
                );
            }
            Change::AddCheck { table, name, .. } => {
                self.claim(step, constraint(table, name.raw(), "check constraint"));
            }
            Change::SetPrimaryKey {
                table, from, to, ..
            } => {
                match from
                    .as_ref()
                    .map(|k| k.name.as_ref().map(pbps_model::ObjectLocalName::raw))
                {
                    Some(Some(name)) => {
                        let name = in_schema(table, name);
                        self.release(step, |h| h.name == name);
                    }
                    // An unnamed key has the name the server gave it, which
                    // the emitter looks up by table; so does the walk.
                    Some(None) => self.release(step, |h| {
                        h.kind == "primary key constraint" && h.parent.as_ref() == Some(table)
                    }),
                    None => {}
                }
                if let Some(name) = to
                    .as_ref()
                    .and_then(|k| k.name.as_ref().map(pbps_model::ObjectLocalName::raw))
                {
                    self.claim(step, constraint(table, name, "primary key constraint"));
                }
            }
            Change::CreateTable { name, table, .. } => {
                self.claim(step, object(name, "user table"));
                // The history is created by the same statement (#1176).
                if let Some(v) = table
                    .system_time
                    .as_ref()
                    .and_then(|st| st.versioning.as_ref())
                {
                    self.claim(step, object(&v.history, "user table"));
                }
                // Its own constraints and generated defaults take their names
                // with it, for whatever the plan creates after.
                for (column, spec) in &table.columns {
                    if match spec.value_source {
                        pbps_model::ValueSource::Default(_) => true,
                        pbps_model::ValueSource::None
                        | pbps_model::ValueSource::Identity(_)
                        | pbps_model::ValueSource::Sequence(_)
                        | pbps_model::ValueSource::Generated(_) => false,
                    } {
                        self.claim(step, default_of(name, column.raw()));
                    }
                }
                let named = table
                    .primary_key
                    .iter()
                    .filter_map(|k| k.name.as_ref().map(pbps_model::ObjectLocalName::raw))
                    .map(|n| (n, "primary key constraint"))
                    .chain(table.unique.keys().map(|n| (n.raw(), "unique constraint")))
                    .chain(
                        table
                            .foreign_keys
                            .keys()
                            .map(|n| (n.raw(), "foreign key constraint")),
                    )
                    .chain(table.checks.keys().map(|n| (n.raw(), "check constraint")));
                for (n, kind) in named {
                    self.claim(step, constraint(name, n, kind));
                }
            }
            Change::CreateModule { id, module } => {
                self.claim(step, object(&module_object(id), &module.kind.to_string()));
            }
            _ => {}
        }
    }

    /// The refusal for the names this walk met held, over the `changes` it
    /// walked. A holder a later change of the plan moves away or removes is
    /// named with that change: the plan could not run it first (#1366).
    pub(crate) fn refusal(
        &self,
        changes: &[pbps_model::PlannedChange],
        label: &str,
    ) -> anyhow::Error {
        let mut later = false;
        let lines: Vec<String> = self
            .taken
            .iter()
            .map(|t| match self.freed.get(&t.holder) {
                Some(&at) if at > t.step => {
                    later = true;
                    let change = &changes[at].change;
                    format!(
                        "{}, which a later change frees: `{}` {}",
                        t.line,
                        pbps_model::escape::Escaped(&change.subject()),
                        crate::report::describe(change)
                    )
                }
                _ => t.line.clone(),
            })
            .collect();
        let mut message = format!(
            "`{label}` already uses {} name(s) this plan would create, for objects that share \
             one namespace per schema on SQL Server:\n  {}\nEach `CREATE` would fail at apply, \
             or replace an object this project does not record. Rename the declaration, drop or \
             rename the object in the database, or — for a table or module pbps can read — \
             adopt it as it stands with `pbps baseline --reason ...`.",
            self.taken.len(),
            lines.join("\n  "),
            label = pbps_model::escape::Escaped(label),
        );
        if later {
            message.push_str(
                "\nA name a later change frees is taken first only where no order of the plan \
                 frees it in time: the change that frees it runs after table renames and drops \
                 (DECISIONS 496), or two changes each wait for the other. Deploy the change \
                 that frees the name first, in a plan of its own, then this one (DEC-1366.2).",
            );
        }
        anyhow::anyhow!(message)
    }
}

/// Refuses a name this plan creates on SQL Server that another object in
/// the schema's `sys.objects` namespace holds when the `CREATE` runs (#1077):
/// a sequence, a synonym, a constraint, a default, or a table or module this
/// project does not record. `CREATE TABLE` there fails with Msg 2714, and
/// `CREATE OR ALTER` fails or replaces what is there.
///
/// The plan is walked in its own order, which is the order it runs in
/// (`order_key`), over what the catalog read found ([`Walk`]).
///
/// The connected check calls [`refuse_occupied_objects_under`] with the
/// database's answer on which names are one; this is that call with none,
/// which is how a case-sensitive database answers.
#[cfg(test)]
pub(crate) fn refuse_occupied_objects(
    cs: &pbps_model::ChangeSet,
    occupants: &[pbps_mssql::catalog::NameOccupant],
    label: &str,
) -> anyhow::Result<()> {
    refuse_occupied_objects_under(cs, occupants, &[], label)
}

/// [`refuse_occupied_objects`], with `alike`: the pairs of names the plan
/// uses that the database reads as one name under its collation, as
/// `catalog::object_names_alike` answers (#1215). Two names the plan itself
/// adds have no catalog spelling to meet at, so without this `Ck_Name` and
/// `ck_name` passed as two names and the second failed at apply.
#[cfg(test)]
pub(crate) fn refuse_occupied_objects_under(
    cs: &pbps_model::ChangeSet,
    occupants: &[pbps_mssql::catalog::NameOccupant],
    alike: &[(TableName, TableName)],
    label: &str,
) -> anyhow::Result<()> {
    let facts = NameFacts::new(occupants, alike);
    let mut walk = Walk::new(&facts);
    walk.run(&cs.changes);
    if walk.is_clear() {
        return Ok(());
    }
    Err(walk.refusal(&cs.changes, label))
}

/// Refuses to use a catalog projection that omitted facts inside the managed
/// set. Both recording and connected planning need the same guard: a partial
/// schema is neither an honest snapshot nor a safe baseline for an artifact.
fn refuse_managed_limitations(limitations: &[String]) -> anyhow::Result<()> {
    if !limitations.is_empty() {
        bail!(
            "{} fact(s) inside the managed set cannot be represented:\n  {}\n\
             Refusing to compare, plan from, or record a partial schema.",
            limitations.len(),
            limitations.join("\n  ")
        );
    }
    Ok(())
}

/// The baseline an `apply` measures itself against, projected twice out of one
/// read.
///
/// The two projections answer two different questions and cannot be one.
/// The **checksum** has to be taken under the recorded state's spelling of a
/// cell at its default ([`pbps_model::ObservedRow`]) and under the union of
/// the recorded and declared scopes, because that is what the plan pinned
/// (DECISIONS 98). The **comparison** made after the statements is against a
/// read-back that has neither: `apply` records what it just wrote, from the
/// plan's scope and with no recorded state to spell it. So the second view is
/// projected exactly the way that read-back will be — the plan's scopes, no
/// reference — and any difference between the two is then a change, not a
/// difference of question.
///
/// One read and two projections, never two reads: two reads would ask the
/// engine the same thing twice and could get two answers, which is the very
/// thing the comparison exists to detect.
#[allow(clippy::too_many_arguments)]
async fn baseline_state(
    conn: &mut Conn,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unmanaged: db::CatalogPolicy<'_>,
    scopes: &DataScopes,
    recorded: &Schema,
    as_the_apply_reads_it: &DataScopes,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<(
    pbps_diff::Scoped,
    Schema,
    Vec<pbps_db::catalog::Unrevocable>,
)> {
    // Before the plan's transaction opens: the baseline is the database as
    // it stands, not as this apply is about to leave it.
    // One read for both projections: the rows either one keeps.
    let managed = managed_state_full(
        conn,
        ids,
        modules,
        external,
        unmanaged,
        &rows_to_read(&pinned_scopes(scopes, as_the_apply_reads_it)),
        crate::engine::Read::Snapshot,
        warnings,
    )
    .await?;
    refuse_managed_limitations(&managed.limitations)?;
    let comparable = managed.scoped.schema.clone().with_observed_rows(
        &managed.rows,
        as_the_apply_reads_it,
        &Schema::default(),
    )?;
    let mut scoped = managed.scoped;
    scoped.schema = scoped
        .schema
        .with_observed_rows(&managed.rows, scopes, recorded)?;
    // The grants this connection could not revoke come from the same read:
    // like ownership, they are beside the checksum, and the apply rechecks
    // them against the connection that runs the statements (#1057).
    Ok((scoped, comparable, managed.unrevocable))
}

/// The same two projections for a staged apply, which needs a third
/// difference: a checkpoint watches every module the plan *names*, including
/// ones it has yet to create (DECISIONS 164), while the checksum has to be
/// taken over exactly the managed set the plan was pinned to or no plan would
/// validate at all. Two cuts of one read, not two reads.
///
/// It was two reads, with the preflight probes and the resume checks running
/// between them. Anything another session changed in that window was already
/// in the second read, so it became the baseline every later checkpoint was
/// measured against — never reported, and finally recorded as clean by the
/// closing ordinary snapshot, which is the state `verify` compares against
/// ever after. [`baseline_state`] states the rule this broke: two reads ask
/// the engine the same thing twice and can get two answers, which is the very
/// thing the comparison exists to detect (DECISIONS 174).
#[allow(clippy::too_many_arguments)]
async fn staged_baseline(
    conn: &mut Conn,
    ids: &IdsFile,
    checked: &BTreeSet<ModuleId>,
    watched: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unmanaged: db::CatalogPolicy<'_>,
    checked_scopes: &DataScopes,
    recorded: &Schema,
    watched_scopes: &DataScopes,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<(
    pbps_diff::Scoped,
    Schema,
    Vec<pbps_db::catalog::Unrevocable>,
)> {
    let pulled = pull(conn, crate::engine::Read::Snapshot).await?;
    warnings.emit(
        pulled
            .warnings
            .iter()
            .map(|message| crate::output::Finding::warning("catalog.warning", message)),
    );
    let unreadable = unreadable_objects(&pulled);
    // Over the *watched* set, which contains the checked one: a module this
    // plan is about to write that the catalog cannot read back is a
    // limitation, and it is only in the wider set. The two reads this
    // replaces each refused over their own set, so refusing over the union is
    // what keeps that (491edd9, DECISIONS 174).
    debug_assert!(checked.is_subset(watched));
    refuse_managed_limitations(&managed_limitations(&pulled, ids, watched, external))?;

    let mut scoped = cut_report(
        &pulled,
        ids,
        checked,
        external,
        &unreadable,
        unmanaged,
        warnings,
    )?;
    // The watch cut is the one the row read is taken against, because it is
    // the wider of the two: a module the plan creates is in it and not in the
    // checksum's, and no table is in either alone. `Unmanaged::Ignore`
    // because the policy has already been applied, by the cut above — running
    // it twice would report the same object twice.
    let watching = cut_report(
        &pulled,
        ids,
        watched,
        external,
        &unreadable,
        db::CatalogPolicy {
            unmanaged: pbps_config::Unmanaged::Ignore,
            ..unmanaged
        },
        warnings,
    )?;
    let rows = crate::engine::read_rows(
        conn,
        &watching.schema,
        &rows_to_read(&pinned_scopes(checked_scopes, watched_scopes)),
        crate::engine::Read::Snapshot,
    )
    .await
    .context("cannot read the declared rows back")?;

    // Spelled two ways for the reason `baseline_state` gives: the checksum
    // under the recorded state's spelling of a cell at its default, the
    // comparison the way the checkpoint read that follows it will be.
    let previous = watching
        .schema
        .with_observed_rows(&rows, watched_scopes, &Schema::default())?;
    scoped.schema = scoped
        .schema
        .with_observed_rows(&rows, checked_scopes, recorded)?;
    Ok((scoped, previous, pulled.unrevocable.clone()))
}

/// Introspects, cuts the result down to the managed set, reads the rows of
/// every table in `read`, and reports whatever the read itself could not
/// express.
///
/// Every connected command starts here. Its catalog diagnostics go to the
/// command's collector before fallible projection and row reads, so a failure
/// cannot discard warnings that belong in its JSON envelope (DEC-1739.1).
///
/// The rows come from the catalog under a scope the caller supplies, because
/// which of a table's rows are declared is not something the database knows
/// (ADR-0004). A row read that fails is an error, not an empty answer: a
/// state recorded without the rows would say the table declares none.
#[allow(clippy::too_many_arguments)]
async fn managed_state_full(
    conn: &mut Conn,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unmanaged: db::CatalogPolicy<'_>,
    rows: &BTreeMap<TableName, RowScope>,
    read: crate::engine::Read,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<Managed> {
    let pulled = pull(conn, read).await?;
    warnings.emit(
        pulled
            .warnings
            .iter()
            .map(|message| crate::output::Finding::warning("catalog.warning", message)),
    );
    let unreadable = unreadable_objects(&pulled);
    let limitations = managed_limitations(&pulled, ids, modules, external);
    let permission_advisories = permission_advisories(&pulled.unexpressible, ids, modules)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let unmanaged_relations = pulled
        .limitations
        .iter()
        .filter_map(|limitation| match &limitation.target {
            pbps_db::catalog::LimitationTarget::Relation(name)
                if !ids.tables.values().any(|managed| managed == name)
                    && !modules.contains(&ModuleId::Named(name.clone())) =>
            {
                Some(name.clone())
            }
            pbps_db::catalog::LimitationTarget::Relation(_)
            | pbps_db::catalog::LimitationTarget::Type(_)
            | pbps_db::catalog::LimitationTarget::SharedModule(_)
            | pbps_db::catalog::LimitationTarget::Search(_)
            | pbps_db::catalog::LimitationTarget::Module(_)
            | pbps_db::catalog::LimitationTarget::UnnameableModule(_) => None,
        })
        .collect();
    let scoped = cut_report(
        &pulled,
        ids,
        modules,
        external,
        &unreadable,
        unmanaged,
        warnings,
    )?;
    let rows = crate::engine::read_rows(conn, &scoped.schema, rows, read)
        .await
        .context("cannot read the declared rows back")?;
    Ok(Managed {
        database_collation: pulled.database_collation.clone(),
        scoped,
        limitations,
        permission_advisories,
        catalog_notices: pulled
            .catalog_notices
            .iter()
            .cloned()
            .chain(
                pulled
                    .extension_notices
                    .iter()
                    .filter(|(name, _)| ids.extension_uid(name).is_none())
                    .map(|(_, message)| message.clone()),
            )
            .collect(),
        unrevocable: pulled.unrevocable.clone(),
        unreadable,
        rows,
        unmanaged_relations,
    })
}

/// The declarations as a connected plan compares them, on a database whose
/// default collation is `database_collation` (#1175): a declared collation
/// equal to it is a column with none, which is how the engine reads it back.
///
/// One function because three diffs depend on it agreeing: `plan --db`, the
/// `--dev` rehearsal, and `doctor`'s question of which keys survive the
/// deletes. A normalization one of them skipped would rebuild a column the
/// others leave alone, and in `doctor` that phantom rebuild withholds a demand
/// the apply needs (#1511, DEC-1511.1).
pub fn declarations_as_compared(declared: &Schema, database_collation: Option<&str>) -> Schema {
    match database_collation {
        Some(default) => declared.without_collation(default),
        None => declared.clone(),
    }
}

/// Refuses a plan whose target's default collation has changed since it was
/// computed (#1175). What a column declared with no collation means is that
/// default, and the checksum cannot see it move when no managed character
/// column sits under it (see [`pbps_model::PlanBaseline::database_collation`]).
/// A plan from an engine without column collations carries none and is not
/// asked.
async fn refuse_moved_database_collation(
    conn: &mut Conn,
    plan: &pbps_model::SavedPlan,
    label: &str,
) -> anyhow::Result<()> {
    let Some(planned) = &plan.baseline.database_collation else {
        return Ok(());
    };
    let now = crate::engine::database_collation(conn)
        .await
        .context("cannot read the database's default collation")?;
    if now.as_deref() != Some(planned.as_str()) {
        bail!(
            "`{label}` has default collation {} now, and this plan was computed against `{planned}`.\n\
             A column it creates or alters without a collation would take the new default, \
             which is not what was approved. Recompute the plan with `pbps plan --db`.",
            now.map_or_else(|| "none readable".to_owned(), |n| format!("`{n}`")),
            label = pbps_model::escape::Escaped(label),
        );
    }
    Ok(())
}

/// Refuses a plan whose target reads offset-less date/time values in another
/// zone than the one it was computed in (DEC-1916.1). Every such value the plan
/// writes, and every pin it runs under, is the recorded zone's; in another
/// zone the same statements store other instants, and nothing in them looks
/// different (see [`pbps_model::PlanBaseline::time_zone`]).
fn refuse_moved_time_zone(
    conn: &Conn,
    plan: &pbps_model::SavedPlan,
    label: &str,
) -> anyhow::Result<()> {
    match moved_time_zone(plan.baseline.time_zone.as_ref(), conn.session_zone(), label) {
        Some(refusal) => Err(anyhow::anyhow!(refusal)),
        None => Ok(()),
    }
}

/// [`refuse_moved_time_zone`]'s verdict, apart from the connection.
///
/// The zones compare by their exact text, as the engine reported them: two
/// spellings of one zone (`UTC`, `Etc/UTC`) are refused too, because which
/// spellings agree is a fact about the engine's tz database that this side
/// cannot see, and a spelling costs one `ALTER` where a wrong instant costs
/// every value written. Absent on one side only is refused as well: a plan
/// that records no zone for an engine that has one was not computed by this
/// build's `plan --db`.
fn moved_time_zone(
    planned: Option<&SessionZone>,
    now: Option<&SessionZone>,
    label: &str,
) -> Option<String> {
    let remedy = "Recompute the plan with `pbps plan --db`, or have the zone set back \
                  (`ALTER DATABASE … SET timezone` or `ALTER ROLE … SET timezone`).";
    match (planned, now) {
        (None, None) => None,
        (Some(planned), Some(now)) if planned == now => None,
        (Some(planned), Some(now)) => Some(format!(
            "`{label}` reads offset-less date/time values in `{now}` now, and this plan \
             was computed in `{planned}`.\nDifferent spellings are treated as different zones; \
             their texts must match exactly (DEC-1916.1). A different zone could store any \
             `timestamptz` or `timetz` written without an offset at another instant than \
             the one approved. \
             {remedy}",
            label = pbps_model::escape::Escaped(label),
        )),
        (Some(planned), None) => Some(format!(
            "`{label}` reports no session time zone, and this plan was computed in \
             `{planned}`. {remedy}",
            label = pbps_model::escape::Escaped(label),
        )),
        (None, Some(now)) => Some(format!(
            "`{label}` reads offset-less date/time values in `{now}`, and this plan records \
             no time zone, so what it writes was not read in one. {remedy}",
            label = pbps_model::escape::Escaped(label),
        )),
    }
}

/// Text search objects have no identity yet, so a drift comparison cannot tell a project's
/// object from another's: one the database holds that the recorded state does not is not
/// drift (as an unmanaged type is not), and the differ would otherwise read it as a creation
/// it cannot express. Only the objects the record holds are compared, so one of them that
/// changed is still reported by name (DEC-2728.1).
fn only_recorded_search_objects(recorded: &Schema, live: &mut Schema) {
    live.search
        .retain(|name, _| recorded.search.contains_key(name));
}

/// The informational line that ends a plan when the database holds comments no declaration
/// manages (DEC-2729.1): a note, not a change.
fn undeclared_comments_note(count: usize) -> String {
    let (are, comments) = if count == 1 {
        ("is", "comment in the database")
    } else {
        ("are", "comments in the database")
    };
    format!("{count} {comments} {are} not declared; pbps leaves them as they are.")
}

/// Both sides of a drift comparison without the comments of their tables and columns: a
/// comment is reconciled by the plan, never drift, and the differ would otherwise read one the
/// database holds and the record does not as a creation to plan (DEC-2729.1).
fn without_comments(recorded: &mut Schema, live: &mut Schema) {
    recorded.clear_descriptions();
    live.clear_descriptions();
}

/// The catalog, read once.
///
/// Split out so a caller that needs two *cuts* of one database state can take
/// them from one read. Two reads would ask the engine the same thing twice
/// and could get two answers, which is the very thing a movement comparison
/// exists to detect (DECISIONS 174).
async fn pull(
    conn: &mut Conn,
    read: crate::engine::Read,
) -> anyhow::Result<pbps_db::catalog::Pulled> {
    crate::engine::introspect(conn, read)
        .await
        .context("cannot read the database catalog")
}

/// Report the cut's diagnostics before its unmanaged policy can refuse it.
fn cut_report(
    pulled: &pbps_db::catalog::Pulled,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unreadable: &[(pbps_db::catalog::LimitationTarget, String)],
    unmanaged: db::CatalogPolicy<'_>,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<pbps_diff::Scoped> {
    let (scoped, permissions) = cut(pulled, ids, modules, external, unmanaged);
    warnings.emit(permissions.into_iter().map(|message| {
        crate::output::Finding::warning("catalog.unexpressible-permission", message)
    }));
    report_unmanaged(&scoped, unreadable, ids, modules, unmanaged.unmanaged)?;
    Ok(scoped)
}

/// One cut of a pulled catalog down to a managed set. Pure.
/// Returns the same permission subset added to the scoped reasons, so warnings
/// cannot label partition limitations as permissions (DEC-1739.1).
fn cut<'a>(
    pulled: &'a pbps_db::catalog::Pulled,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unmanaged: db::CatalogPolicy<'_>,
) -> (pbps_diff::Scoped, Vec<&'a str>) {
    let mut scoped = pbps_diff::scope(&pulled.schema, ids, modules, external);
    // These exact roles belong to the verified owned run, not its declarations.
    // A similarly named role remains subject to the project's ordinary policy.
    scoped
        .unmanaged_roles
        .retain(|role| !unmanaged.infrastructure_roles.contains(role));
    // A managed role's grant WITH GRANT OPTION is wider than the plain grant
    // the declarations can spell, and was left out of the role's set rather
    // than folded in. Carried beside the comparison so `verify` reports it
    // as drift and `plan --db` refuses to plan over it; on an unmanaged role,
    // or on somebody else's object, it is not ours (DECISIONS 95, 176).
    let permissions = unexpressible_permissions(pulled, ids, modules);
    scoped
        .unexpressible
        .extend(permissions.iter().map(|what| (*what).to_owned()));
    // Carried beside the comparison, never into it, exactly as the line above
    // carries a `WITH GRANT OPTION` (DECISIONS 95, 371). Cut to the managed
    // modules for the same reason every other field here is: what `PUBLIC`
    // may run on somebody else's routine is not ours to have an opinion on.
    scoped.public_execute = pulled
        .public_execute
        .iter()
        .filter(|id| modules.contains(*id))
        .cloned()
        .collect();
    // Kept whole rather than cut: a declared grant's target is the project's
    // by construction, and the caller looks each one up by name. An owner
    // this map lacks is an object the read did not see, which is a different
    // finding from one whose owner is somebody else.
    scoped.owners = pulled.owners.clone();
    scoped.session_role = pulled.session_role.clone();
    scoped.baseline_evidence = pulled
        .baseline_evidence
        .as_ref()
        .map(|evidence| evidence.for_managed(&scoped.schema));
    (scoped, permissions)
}

/// The modules the declarations name and the rows they declare, for the
/// commands that record a state.
///
/// `snapshot` and `baseline` write down what the environment is; a module the
/// declarations manage, or a table whose rows they declare, has to be in that
/// record, or the next `verify` would not be watching it. The ids file is
/// already required by both, so requiring the declarations to parse as well
/// changes nothing about when they can run.
/// The whole `Loaded` travels back, not just its schema: the state these two
/// commands record carries the module dependency annotations beside it, and
/// those live in the hints rather than in the model (constraint 8).
fn declared_scope(
    project: &Project,
    dialect: &dyn pbps_dialect::Dialect,
) -> anyhow::Result<(BTreeSet<ModuleId>, DataScopes, pbps_load::Loaded)> {
    let loaded = crate::load(project, dialect)?;
    Ok((
        managed_modules(None, Some(&loaded.schema)),
        loaded.schema.data_scopes(),
        loaded,
    ))
}

/// The scopes a plan's baseline is pinned under: the recorded ones and the
/// plan's own, table by table, each the union of the two where both cover it.
///
/// A table gaining its first `data:` block has rows the differ measured
/// against and the recorded state knows nothing of; a table whose `ensure`
/// block gains a key, or switches to `exact`, has rows the differ measured
/// that the recorded scope alone would not look at again. The baseline
/// checksum and `apply`'s check both read this union, so a row that appears
/// or changes in such a table between plan and apply is a mismatch, not a
/// row the approved changes silently missed or overwrote (DECISIONS 98).
fn pinned_scopes(recorded: &DataScopes, planned: &DataScopes) -> DataScopes {
    let mut out = recorded.clone();
    for (name, scope) in planned {
        let pinned = match out.remove(name) {
            Some(recorded) => recorded.union(scope.clone()),
            None => scope.clone(),
        };
        out.insert(name.clone(), pinned);
    }
    out
}

/// The history names a plan's `CREATE TABLE`s take that something in the
/// database still holds when that `CREATE` runs (#1176): each table, its
/// history, and the occupant. Ordered as the `sys.objects` walk orders the
/// plan, so an object the plan drops or renames away first frees the name, as
/// it does at plan time (#1501 review).
fn blocking_history_occupants<'a>(
    cs: &pbps_model::ChangeSet,
    held: &'a [pbps_mssql::catalog::NameOccupant],
) -> Vec<(TableName, TableName, &'a pbps_mssql::catalog::NameOccupant)> {
    let histories: BTreeMap<&TableName, &TableName> = cs
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::CreateTable { name, table, .. } = &p.change {
                let v = table.system_time.as_ref()?.versioning.as_ref()?;
                Some((&v.history, name))
            } else {
                None
            }
        })
        .collect();
    held.iter()
        .filter_map(|o| {
            let table = histories.get(&o.wanted)?;
            let facts = NameFacts::new(std::slice::from_ref(o), &[]);
            let mut walk = Walk::new(&facts);
            walk.run(&cs.changes);
            (!walk.is_clear()).then(|| ((*table).clone(), o.wanted.clone(), o))
        })
        .collect()
}

/// Refuses a plan whose `CREATE TABLE` names, as a system-versioned table's
/// history, a name the database still holds when it runs (#1176). Not left
/// to the engine: given an existing table whose columns match, SQL Server
/// takes it as the history, rows, layout and all, where any other `CREATE`
/// would be refused (measured on 17.0).
async fn refuse_taken_history_names(
    conn: &mut Conn,
    cs: &pbps_model::ChangeSet,
    label: &str,
) -> anyhow::Result<()> {
    if conn.driver() != pbps_db::Driver::Mssql {
        return Ok(());
    }
    let names: Vec<TableName> = cs
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::CreateTable { table, .. } = &p.change {
                Some(
                    table
                        .system_time
                        .as_ref()?
                        .versioning
                        .as_ref()?
                        .history
                        .clone(),
                )
            } else {
                None
            }
        })
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    let held = pbps_mssql::catalog::object_name_occupants(conn, &names, &[]).await?;
    let taken: Vec<String> = blocking_history_occupants(cs, &held)
        .into_iter()
        .map(|(table, history, o)| {
            format!(
                "{table} names {history} as its history table, and the database already has {} \
                 `{}`",
                o.kind,
                o.name.shown(),
                table = table.shown(),
                history = history.shown()
            )
        })
        .collect();
    if taken.is_empty() {
        return Ok(());
    }
    bail!(
        "`{label}` already holds {} history table name(s) this plan would create:\n  {}\n\
         SQL Server would take an existing table whose columns match as the history, with its \
         rows, rather than refuse. Drop or rename that object, or name another history table.",
        taken.len(),
        taken.join("\n  "),
        label = pbps_model::escape::Escaped(label),
    );
}

/// Refuses, before anything runs, a role name another database principal
/// holds: users, roles and application roles share one namespace, and a
/// `CREATE ROLE` or `ALTER ROLE ... WITH NAME` onto a taken name fails
/// after everything ordered before it has run (DECISIONS 118).
///
/// `wanted` are the names the plan's remaining statements create or rename
/// to; `vacated` the ones they drop or rename away, which are free for
/// this purpose. The engine decides which names are the same, under the
/// database's collation — `Shadow` is `shadow` to most databases and not
/// to a string comparison here (DECISIONS 119).
async fn refuse_taken_role_names(
    conn: &mut Conn,
    wanted: &[pbps_model::RoleName],
    vacated: &[pbps_model::RoleName],
) -> anyhow::Result<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    let wanted: Vec<&str> = wanted.iter().map(pbps_model::RoleName::raw).collect();
    let vacated: Vec<&str> = vacated.iter().map(pbps_model::RoleName::raw).collect();
    // The wanted names against one another first: two declared roles the
    // database reads as one name pass every check against the catalog, and
    // the second `CREATE ROLE` fails after everything before it has run
    // (DECISIONS 123).
    let alike = crate::engine::names_alike(conn, &wanted)
        .await
        .context("cannot compare the declared role names")?;
    if !alike.is_empty() {
        let pairs: Vec<String> = alike
            .iter()
            .map(|(earlier, later)| format!("`{earlier}` and `{later}`"))
            .collect();
        bail!(
            "two declared roles are one name to this database: {}.\n\
             Names are compared the way this database compares them, and the second \
             `CREATE ROLE` or `ALTER ROLE ... WITH NAME` would be refused after everything \
             before it had run. Rename one of them.",
            pairs.join(", ")
        );
    }
    let held = crate::engine::principals_holding(conn, &wanted, &vacated)
        .await
        .context("cannot read the database principals")?;
    if held.is_empty() {
        return Ok(());
    }
    let taken: Vec<String> = held
        .iter()
        .map(|(declared, held, kind)| {
            if declared == held {
                format!("`{declared}` is a {kind}")
            } else {
                format!("`{declared}` is `{held}` to this database, a {kind}")
            }
        })
        .collect();
    bail!(
        "a declared role's name is already held in this database: {}.\n\
         Users, roles and application roles share one namespace, compared the way this \
         database compares names, and `CREATE ROLE` or `ALTER ROLE ... WITH NAME` would be \
         refused after everything before it had run. Rename the role, or rename or drop the \
         principal by hand.",
        taken.join(", ")
    );
}

/// `schema` without the temporary CHECKs the plan's staged attach writes and
/// drops itself (#1638, DEC-1638.1), for the comparison of one checkpoint
/// with the last. Neither the declarations nor the plan's changes hold it,
/// so to the comparison it is a check that moved for no reason the plan
/// gives; the emitter names it, and the checkpoint still records it, as the
/// database holds it.
fn without_scan_checks<'a>(
    schema: &'a Schema,
    statements: &[pbps_dialect::Statement],
) -> std::borrow::Cow<'a, Schema> {
    let mut schema = std::borrow::Cow::Borrowed(schema);
    for check in statements.iter().filter_map(|s| s.scan_check.as_ref()) {
        if schema
            .tables
            .get(&check.table)
            .is_some_and(|t| t.checks.contains_key(&check.name))
            && let Some(table) = schema.to_mut().tables.get_mut(&check.table)
        {
            table.checks.remove(&check.name);
        }
    }
    schema
}

/// Whether `stmt` is the `step` of a staged attach's temporary CHECK.
fn is_scan_check(stmt: &pbps_dialect::Statement, step: pbps_dialect::ScanCheckStep) -> bool {
    stmt.scan_check.as_ref().is_some_and(|c| c.step == step)
}

/// The mode `plan --db` makes a plan in.
fn plan_mode(staged: bool) -> pbps_model::PlanMode {
    if staged {
        pbps_model::PlanMode::Staged
    } else {
        pbps_model::PlanMode::Transactional
    }
}

/// The role names the statements after `completed` still have to find free
/// (`CREATE ROLE`, the new name of a rename) and the ones they free first
/// (`DROP ROLE`, the old name of a rename): `(wanted, vacated)`. Counted per
/// statement the way `role_drop_expectations` counts, so a staged resume
/// asks only about the names its remaining statements touch (DECISIONS 119).
fn role_name_expectations<D: crate::emission::Engine>(
    plan: &(dyn pbps_diff::order::PlanView<D> + 'static),
    dialect: &D,
    mode: pbps_model::PlanMode,
    completed: usize,
) -> anyhow::Result<(Vec<pbps_model::RoleName>, Vec<pbps_model::RoleName>)> {
    let cs = pbps_dialect::PlanPayload::changes(plan);
    let mut wanted = Vec::new();
    let mut vacated = Vec::new();
    let mut at = 0usize;
    let emit = crate::emitter(dialect, plan, mode);
    for (position, p) in cs.changes.iter().enumerate() {
        let n = emit(position)?.len();
        // A change is pending while any of its statements is: a half-done
        // role drop has removed members and still holds the name.
        if completed < at + n {
            if let pbps_model::Change::CreateRole { name, .. } = &p.change {
                wanted.push(name.clone());
            } else if let pbps_model::Change::RenameRole { from, to, .. } = &p.change {
                wanted.push(to.clone());
                vacated.push(from.clone());
            } else if let pbps_model::Change::DropRole { name, .. } = &p.change {
                vacated.push(name.clone());
            }
        }
        at += n;
    }
    Ok((wanted, vacated))
}

/// What the catalog calls each declared table and its key column *now*: the
/// names `live_ids` gives the uids that `final_ids` gives the declarations.
///
/// The spelling checks run before the plan does, so a table or key column
/// this revision renames is still under its old name — and the collation read
/// inside the collision query, the one part of those checks that names an
/// object rather than converting a literal, silently found nothing and fell
/// back to the database's default collation (DECISIONS 148).
pub(crate) fn catalogued_as(
    schema: &Schema,
    final_ids: &IdsFile,
    live_ids: &IdsFile,
) -> pbps_db::catalog::CatalogNames {
    let mut out = pbps_db::catalog::CatalogNames::new();
    // Once, not a scan per table and key (#1870).
    let final_index = final_ids.index();
    for (name, table) in &schema.tables {
        let live_table = final_index.resolved_in(name, live_ids);
        // Only the key column is named to the catalog; every other column
        // reaches it as a converted literal, under no name at all.
        let live_key = table
            .primary_key
            .as_ref()
            .filter(|pk| pk.columns.len() == 1)
            .map(|pk| pbps_model::ColumnRef::new(name.clone(), pk.columns[0].clone()))
            .and_then(|r| final_index.column_uid(&r))
            .and_then(|uid| live_ids.columns.get(uid))
            .map(|r| r.name.clone());
        let at = pbps_db::catalog::Catalogued {
            table: (live_table != *name).then_some(live_table),
            key_column: live_key.filter(|c| {
                table
                    .primary_key
                    .as_ref()
                    .is_none_or(|pk| pk.columns.first() != Some(c))
            }),
            // Read from the catalog by the engine whose question it is, under
            // the two names above; a declaration cannot know it.
            key_collation: None,
        };
        if at != pbps_db::catalog::Catalogued::default() {
            out.insert(name.clone(), at);
        }
    }
    out
}

/// Prints what [`refuse_misspelt`] could not ask, for a command whose
/// warnings go to stderr.
/// What this command's sweeps found (#2410): a note for what was removed, a
/// warning with the removing statement for what was not.
pub(crate) fn recovery_findings() -> Vec<crate::output::Finding> {
    recovery_report_findings(pbps_cli::resolver::server::scratch::recovery::take())
}

fn recovery_report_findings(
    collected: pbps_cli::resolver::server::scratch::recovery::Collected,
) -> Vec<crate::output::Finding> {
    let mut findings = Vec::new();
    for names in collected.cancelled {
        findings.push(crate::output::Finding::warning(
            "scratch.cleanup-unconfirmed",
            format!(
                "a cancelled resolver run could not confirm cleanup; remove exactly: {}",
                names
                    .iter()
                    .map(|name| pbps_model::escape::Escaped(name).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    for why in collected.unread {
        findings.push(crate::output::Finding::warning(
            "scratch.leftovers-unread",
            format!("scratch left by earlier runs could not be looked for: {why}"),
        ));
    }
    for group in collected.groups {
        for resource in &group.removed {
            findings.push(crate::output::Finding::note(
                "scratch.recovered",
                format!(
                    "confirmed removal of {} {}, left by run {}",
                    resource.kind,
                    resource.names.join(", "),
                    group.token
                ),
            ));
        }
        for kept in group.kept {
            let mut finding = crate::output::Finding::warning(
                "scratch.leftover",
                format!(
                    "{} {} left by run {} was not removed: {}",
                    kept.resource.kind,
                    kept.resource.names.join(", "),
                    group.token,
                    kept.why
                ),
            );
            finding.remedy = Some(kept.statement);
            findings.push(finding);
        }
    }
    findings
}

pub(crate) fn warn_unasked(unasked: &[String]) {
    for w in unasked {
        eprintln!("warning: {}", w);
    }
}

/// Refuses a declaration whose text the engine would not read back as
/// written, before anything is written (DECISIONS 101).
///
/// Asked of the engine rather than reasoned about: `"1.5"` in a
/// `decimal(5,2)` comes back `1.50`, `"ab "` in a `char(5)` comes back `ab`,
/// and a text the type cannot read at all comes back as a failed insert —
/// each a plan that never converges or never applies, and each a question
/// only the engine answers the same way it will answer at read time.
///
/// Hands back, as warnings, what it could not ask: a partition's own default it
/// could not deparse (DEC-1609.1). Not a finding and not a pass — the apply's
/// closing check still refuses one stored as the parent's, after its
/// statement — so each caller reports them where its other warnings go: a
/// connected plan in its JSON envelope (#1660), the rest on stderr through
/// [`warn_unasked`]. They go to `unasked` whether or not it refuses: beside
/// the error, never inside it, so a refusal is the plain error every caller
/// and every downcast already knows, and the caller writes the warnings
/// wherever it writes the refusal (#1702, DEC-1726.1).
///
/// What it hands back, when it does not refuse, is `schema` as the engine
/// reads it: every date or time value it read one way only in the engine's
/// own spelling, cells, keys and bounds alike (DEC-1883.1). Every caller
/// plans, compares and records that schema from here on, never the one it
/// passed in — a plan written from the declared text would restate, on every
/// plan after, a value the table already holds.
pub(crate) async fn refuse_misspelt(
    conn: &mut Conn,
    schema: &Schema,
    at: &pbps_db::catalog::CatalogNames,
    unasked: &mut Vec<String>,
    type_routines_need_review: bool,
) -> anyhow::Result<Schema> {
    refuse_misspelt_with_probe(conn, schema, schema, at, unasked, type_routines_need_review).await
}

async fn refuse_misspelt_with_probe(
    conn: &mut Conn,
    schema: &Schema,
    probe_schema: &Schema,
    at: &pbps_db::catalog::CatalogNames,
    unasked: &mut Vec<String>,
    type_routines_need_review: bool,
) -> anyhow::Result<Schema> {
    let mut found = crate::engine::misspelt(conn, probe_schema, at, type_routines_need_review)
        .await
        .context("cannot ask the engine how it reads the declared rows")?;
    unasked.extend(found.defaults_unasked.iter().map(|why| {
        format!(
            "not checked before the plan whether the engine stores it as the parent's \
             default: {why}"
        )
    }));
    // A date or time the engine reads, and reads one way only — the probe
    // has already moved every text with two readings to `ambiguous` — is the
    // value it reads (DEC-1883.1). Every other type keeps DECISIONS 101.
    let (cells, misspelt): (Vec<_>, Vec<_>) = std::mem::take(&mut found.misspelt)
        .into_iter()
        .partition(|m| m.temporal && m.canonical.is_some());
    found.misspelt = misspelt;
    let (bounds, misspelt_bounds): (Vec<_>, Vec<_>) = std::mem::take(&mut found.bounds)
        .into_iter()
        .partition(|b| b.temporal && b.canonical.is_some());
    found.bounds = misspelt_bounds;
    if found.misspelt.is_empty()
        && found.conflicts.is_empty()
        && found.bounds.is_empty()
        && found.list_collisions.is_empty()
        && found.defaults_as_parents.is_empty()
        && found.ambiguous.is_empty()
    {
        return Ok(as_read(schema, &cells, &bounds));
    }
    // Two keys the engine reads as one row would insert twice and fail on
    // the second; the alias check (74) cannot see them on a table that holds
    // neither yet (DECISIONS 106).
    let mut lines: Vec<String> = found.conflicts.iter().map(ToString::to_string).collect();
    lines.extend(
        found
            .misspelt
            .iter()
            .map(|m| match (&m.column, &m.canonical) {
                (None, None) => format!(
                    "{} row key `{}` cannot be read as {} by the engine",
                    m.table.shown(),
                    m.key,
                    m.ty
                ),
                (None, Some(canonical)) => format!(
                    "{} row key `{}` is read back by the engine as {:?}; write it that way",
                    m.table.shown(),
                    m.key,
                    canonical
                ),
                (Some(c), None) => format!(
                    "{} row `{}`: `{c}` = {:?} cannot be read as {} by the engine",
                    m.table.shown(),
                    m.key,
                    m.declared,
                    m.ty,
                    c = c.shown(),
                ),
                (Some(c), Some(canonical)) => format!(
                    "{} row `{}`: `{c}` is written {:?}, and the engine reads it back as {:?}; \
                 write it that way",
                    m.table.shown(),
                    m.key,
                    m.declared,
                    canonical,
                    c = c.shown(),
                ),
            }),
    );
    // A bound the engine stores in its own spelling would differ from the
    // declaration on every plan after (#1170).
    lines.extend(found.bounds.iter().map(|b| match &b.canonical {
        None => format!(
            "partition {} bound on `{}`: {:?} cannot be read as {} by the engine",
            b.partition.shown(),
            b.column.shown(),
            b.declared,
            b.ty
        ),
        Some(canonical) => format!(
            "partition {} bound on `{}` is written {:?}, and the engine reads it back as \
             {canonical:?}; write it that way",
            b.partition.shown(),
            b.column.shown(),
            b.declared
        ),
    }));
    // A partition's own default stored as its parent's reads back as none of
    // its own, so every plan would set it again (#1609).
    lines.extend(found.defaults_as_parents.iter().map(|d| {
        format!(
            "partition {} column `{}`: its own default {:?} is stored as {:?}, which is its \
             parent's; drop the partition's own default, it is the parent's",
            d.partition.shown(),
            d.column.shown(),
            d.declared,
            d.stored
        )
    }));
    let mut sections = Vec::new();
    if !lines.is_empty() {
        sections.push(format!(
            "{} declared value(s) would not come back as written:\n  {}\n\
             A declaration that disagrees with its own database on every plan is worse \
             than none; the engine's spelling is the one to write (DECISIONS 101, 106).",
            lines.len(),
            lines.join("\n  ")
        ));
    }
    // Two partitions taking one value by the key type's own equality, which
    // validation's text comparison cannot see: the engine refuses the second
    // as overlapping the first. Its own section, because the remedy is not a
    // spelling: both texts may be the engine's own (#1632).
    if !found.list_collisions.is_empty() {
        let lines: Vec<String> = found
            .list_collisions
            .iter()
            .map(|c| {
                format!(
                    "`{}` takes {:?} and `{}` takes {:?} of `{}`",
                    c.first.0.shown(),
                    c.first.1,
                    c.second.0.shown(),
                    c.second.1,
                    c.parent.shown()
                )
            })
            .collect();
        sections.push(format!(
            "{} pair(s) of list values are one value of the partition key's type:\n  {}\n\
             A value routes to one partition, so the engine refuses the second; list it in \
             one partition only.",
            lines.len(),
            lines.join("\n  ")
        ));
    }
    // A section of its own, because its remedy is the opposite of the one
    // above: not "write what the engine reads", which would name the pinned
    // reading as the meant one, but "write it so that there is one reading"
    // (#1756, DEC-1756.1).
    if !found.ambiguous.is_empty() {
        let lines: Vec<String> = found.ambiguous.iter().map(ambiguous_line).collect();
        sections.push(format!(
            "{} declared date/time value(s) can be read more than one way:\n  {}\n\
             What such a text means is decided by the settings of the session that reads \
             it; this tool's would store one reading without saying which (DEC-1756.1).",
            lines.len(),
            lines.join("\n  ")
        ));
    }
    bail!("{}", sections.join("\n\n"))
}

/// `schema` with each date or time the engine read in place of the text that
/// declared it (DEC-1883.1): a cell's value, a row's key, a partition's bound.
///
/// The cells go first, found under the keys as declared, and only then are
/// the keys themselves replaced. Two declared keys the engine reads as one are
/// never here: they are a conflict, refused before this is reached (DECISIONS
/// 106), so a replaced key cannot land on another row.
fn as_read(
    schema: &Schema,
    cells: &[pbps_db::catalog::Misspelt],
    bounds: &[pbps_db::catalog::MisspeltBound],
) -> Schema {
    let mut out = schema.clone();
    for m in cells {
        let (Some(column), Some(canonical)) = (&m.column, &m.canonical) else {
            continue;
        };
        if let Some(row) = out
            .tables
            .get_mut(&m.table)
            .and_then(|t| t.data.as_mut())
            .and_then(|d| d.rows.get_mut(&m.key))
        {
            row.0
                .insert(column.clone(), pbps_model::Value::Text(canonical.clone()));
        }
    }
    for m in cells {
        let (None, Some(canonical)) = (&m.column, &m.canonical) else {
            continue;
        };
        if let Some(data) = out.tables.get_mut(&m.table).and_then(|t| t.data.as_mut())
            && let Some(row) = data.rows.remove(&m.key)
        {
            data.rows
                .insert(pbps_model::RowKey::from(canonical.as_str()), row);
        }
    }
    for b in bounds {
        let Some(canonical) = &b.canonical else {
            continue;
        };
        let Some(of) = out
            .tables
            .get(&b.partition)
            .and_then(|t| t.partition_of.as_ref())
        else {
            continue;
        };
        let Some(position) = schema
            .tables
            .get(&of.parent)
            .and_then(|p| p.partition_by.as_ref())
            .and_then(|by| by.columns.iter().position(|c| *c == b.column))
        else {
            continue;
        };
        match out
            .tables
            .get_mut(&b.partition)
            .and_then(|t| t.partition_of.as_mut())
            .map(|of| &mut of.bound)
        {
            // Both ends, wherever the declared text stands: the same text is
            // the same reading.
            Some(pbps_model::PartitionBound::Range { from, to }) => {
                for datum in [from.get_mut(position), to.get_mut(position)]
                    .into_iter()
                    .flatten()
                {
                    if *datum == pbps_model::BoundDatum::Value(b.declared.clone()) {
                        *datum = pbps_model::BoundDatum::Value(canonical.clone());
                    }
                }
            }
            // Every value of a list is its one key column's (#1632).
            Some(pbps_model::PartitionBound::List { values }) if position == 0 => {
                if values.remove(&pbps_model::ListDatum::Value(b.declared.clone())) {
                    values.insert(pbps_model::ListDatum::Value(canonical.clone()));
                }
            }
            _ => {}
        }
    }
    out
}

/// One ambiguous literal, where it is and what it can mean.
fn ambiguous_line(a: &pbps_db::catalog::Ambiguous) -> String {
    use pbps_db::catalog::LiteralAt;
    let at = match &a.at {
        LiteralAt::Cell {
            table,
            key,
            column: None,
        } => format!("{table} row key `{key}`", table = table.shown()),
        LiteralAt::Cell {
            table,
            key,
            column: Some(c),
        } => format!(
            "{table} row `{key}`: `{c}`",
            table = table.shown(),
            c = c.shown(),
        ),
        LiteralAt::Bound { partition, column } => {
            format!(
                "partition {partition} bound on `{column}`",
                partition = partition.shown(),
                column = column.shown(),
            )
        }
        LiteralAt::Default { table, column } => {
            format!(
                "{table} column `{column}` default",
                table = table.shown(),
                column = column.shown(),
            )
        }
    };
    match &a.pinned {
        None => format!(
            "{at} {:?} is a {} decided by the moment the statement runs, not by its text; {}",
            a.declared, a.ty, a.remedy
        ),
        Some(pinned) => {
            let otherwise: Vec<String> = a
                .otherwise
                .iter()
                .map(|(reading, under)| format!("{reading} under {under}"))
                .collect();
            format!(
                "{at} {:?} is read as {pinned} under this tool's settings, but as {}; {}",
                a.declared,
                otherwise.join(", and as "),
                a.remedy
            )
        }
    }
}

/// Every schema name a declaration spells, and one declaration that spells it
/// that way — for the message, so the refusal names something the author can
/// find in a file rather than a bare schema name.
pub(crate) fn schemas_declared(schema: &Schema) -> BTreeMap<String, String> {
    let Schema {
        tables,
        types,
        search,
        modules,
        roles,
        extensions,
    } = schema;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for name in search.keys() {
        out.entry(name.name.schema().to_owned())
            .or_insert_with(|| format!("the schema of {}", name.shown()));
    }
    for (name, definition) in extensions {
        out.entry(definition.schema.clone())
            .or_insert_with(|| format!("the schema of extension `{}`", name.shown()));
    }
    for (name, role) in roles {
        for target in role.grants.keys() {
            if let pbps_model::GrantTarget::Schema(s) = target {
                out.entry(s.clone())
                    .or_insert_with(|| format!("granted to role `{name}`", name = name.shown(),));
            }
        }
    }
    // The schema half of a qualified name is text on both sides too, and the
    // uid that matches the *table* does not match it: the managed set is
    // scoped by name, so `DBO.customer` on a database whose schema is `dbo`
    // is bootstrapped, recorded as a state holding no tables at all, and
    // reported as drift by the `verify` immediately after.
    for name in tables.keys() {
        out.entry(name.schema.clone())
            .or_insert_with(|| format!("the schema of `{name}`", name = name.shown()));
    }
    for id in modules.keys() {
        out.entry(id.schema().to_owned())
            .or_insert_with(|| format!("the schema of `{id}`", id = id.shown()));
    }
    for name in types.keys() {
        out.entry(name.schema().to_owned())
            .or_insert_with(|| format!("the schema of type `{}`", name.shown()));
    }
    // A history is created in its own schema, spelled as declared, and read
    // back as the database spells it (#1176, #1501 review).
    for (name, table) in tables {
        if let Some(v) = table
            .system_time
            .as_ref()
            .and_then(|st| st.versioning.as_ref())
        {
            out.entry(v.history.schema.clone()).or_insert_with(|| {
                format!(
                    "the schema of `{name}`'s history table",
                    name = name.shown()
                )
            });
        }
    }
    out
}

/// The declared schema names the database spells differently, one line each.
///
/// Absent is not misspelt: `schema::legacy` on a database that has no such
/// schema is the pre-flight probe's refusal, and that one says to create it.
fn wrongly_spelt(
    spelled: &BTreeMap<String, Option<String>>,
    declared_by: &BTreeMap<String, String>,
) -> Vec<String> {
    spelled
        .iter()
        .filter_map(|(declared, found)| match found {
            Some(found) if found != declared => Some(format!(
                "`{declared}` ({}) is written `{found}` by the database",
                declared_by.get(declared).map_or("declared", String::as_str)
            )),
            _ => None,
        })
        .collect()
}

/// Refuses a schema name the database spells differently, before a plan is
/// written that could never converge (DECISIONS 142).
///
/// The sibling of `refuse_misspelt`, for the names in a declaration with no
/// identity behind them. A table is matched by uid, so its own case is
/// whatever the ids file recorded; the schema it lives in, and the schema a
/// `schema::` grant names, are text on both sides. On a case-insensitive
/// database `schema::DBO` grants successfully, reads back as `dbo`, and is
/// revoked and granted again by every plan after; `DBO.customer` is created
/// as `dbo.customer` and then falls outside the managed set the state
/// records by name.
pub(crate) async fn refuse_wrongly_spelt_schemas(
    conn: &mut Conn,
    schema: &Schema,
) -> anyhow::Result<()> {
    let declared_by = schemas_declared(schema);
    let wanted: std::collections::BTreeSet<String> = declared_by.keys().cloned().collect();
    let spelled = crate::engine::schema_spellings(conn, &wanted)
        .await
        .context("cannot ask the engine how it spells the declared schemas")?;
    let wrong = wrongly_spelt(&spelled, &declared_by);
    if wrong.is_empty() {
        return Ok(());
    }
    bail!(
        "{} declared schema name(s) the database spells differently:\n  {}\n\
         The objects would be created, granted and read back under the database's spelling, \
         so every plan after would disagree with the declaration that produced it; write the \
         name the way the database does (DECISIONS 142).",
        wrong.len(),
        wrong.join("\n  ")
    );
}

/// The key-column type changes in `cs` on tables whose declared keys were
/// matched to their rows under a different spelling (`01` for a stored `1`
/// under `int`), one line each. The alias mapping holds under the type the
/// column has now; changed to `varchar`, the stored `1` is not `01`, the
/// plan emits no row change, and an `ensure` block inserts a duplicate on
/// the next run (DECISIONS 108).
fn key_type_changes_over_aliases(
    cs: &pbps_model::ChangeSet,
    declared_live: &Schema,
    rows: &pbps_model::ObservedRows,
    final_ids: &IdsFile,
    live_ids: &IdsFile,
) -> Vec<String> {
    let mut out = Vec::new();
    for p in &cs.changes {
        let pbps_model::Change::AlterColumnType {
            column, from, to, ..
        } = &p.change
        else {
            continue;
        };
        // The change names the table as this plan leaves it; both collections
        // below are keyed by the name the database has now. A table renamed
        // in the same revision was looked up under its new name in neither,
        // and the guard passed a key-type change it exists to refuse.
        let live = final_ids.resolved_in(&column.table, live_ids);
        let Some(table) = declared_live.tables.get(&live) else {
            continue;
        };
        let is_key = table
            .primary_key
            .as_ref()
            .is_some_and(|pk| pk.columns.as_slice() == [column.name.clone()]);
        if !is_key {
            continue;
        }
        let aliased: Vec<String> = rows
            .get(&live)
            .map(|t| {
                t.aliases
                    .iter()
                    .filter(|(requested, canonical)| requested != canonical)
                    .map(|(requested, canonical)| {
                        format!("`{requested}` (stored as `{canonical}`)")
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !aliased.is_empty() {
            out.push(format!(
                "{}: this plan changes the type of its key column `{}` ({from} -> {to}), and \
                 these declared keys are spelled differently from the way the engine spells \
                 them: {}",
                column.table.shown(),
                column.name.shown(),
                aliased.join(", "),
                from = from.shown(),
                to = to.shown(),
            ));
        }
    }
    out
}

/// Brings the objects a committed statement created into the live identities,
/// under the uids the plan gave them, so the checkpoint taken next scopes
/// them in. Before this, a role or a table the plan had just created stood
/// outside every checkpoint until the closing entry, and a grant or a row it
/// gained while the deployment was paused was recorded as clean without a
/// resume ever comparing it (DECISIONS 100). A name the plan's ids do not
/// know is left alone: there is no uid to adopt it under.
fn adopt_created(live_ids: &mut IdsFile, plan_ids: &IdsFile, created: &[pbps_dialect::Created]) {
    for c in created {
        match c {
            pbps_dialect::Created::Table(name) => {
                if let Some(uid) = plan_ids.table_uid(name) {
                    live_ids.tables.insert(uid.clone(), name.clone());
                }
                // Its columns come with it: the CREATE TABLE made them all.
                for (uid, r) in &plan_ids.columns {
                    if &r.table == name {
                        live_ids.columns.insert(uid.clone(), r.clone());
                    }
                }
            }
            pbps_dialect::Created::Column(table, column) => {
                let r = pbps_model::ColumnRef::new(table.clone(), column.clone());
                if let Some(uid) = plan_ids.column_uid(&r) {
                    live_ids.columns.insert(uid.clone(), r);
                }
            }
            pbps_dialect::Created::Role(name) => {
                if let Some(uid) = plan_ids.role_uid(name) {
                    live_ids.roles.insert(uid.clone(), name.clone());
                }
            }
        }
    }
}

/// The plan's data scopes under the names the catalog has *now*.
///
/// `plan.data` is keyed by each table's final name. Halfway through a staged
/// rename that moves both the schema and the name, the table stands at an
/// intermediate name, and a scope keyed by the final one would make the
/// checkpoint read no rows for it — so a hand-made change to those rows while
/// the deployment is paused would go unseen by `--resume`. The scopes follow
/// the same mapping the checkpoint's ids follow: final name -> uid -> the
/// name `live_ids` has for it. A table the plan has not created yet keeps its
/// final name and is simply not there to read.
fn scopes_at(plan: &pbps_model::SavedPlan, live_ids: &IdsFile) -> DataScopes {
    scopes_under(&plan.data, &plan.ids, live_ids)
}

/// The state read under the widened scope, kept to the rows the plan still
/// manages once its renames have run (DEC-2346.2): see
/// [`pbps_diff::enum_labels::trim_to_the_final_scope`].
fn trim_to_the_planned_scope(
    plan: &pbps_model::SavedPlan,
    ids: &IdsFile,
    scopes: &DataScopes,
    state: &mut Schema,
) {
    pbps_diff::enum_labels::trim_to_the_final_scope(
        state,
        scopes,
        (&TypeResolver::new(ids, &plan.ids), TypeSide::Base),
        &pbps_diff::enum_labels::label_renames(plan.changes.changes.iter().map(|p| &p.change)),
    );
}

/// `scopes` with the keys of the tables an enum keys also named by the labels
/// they hold before this plan's label renames (DEC-2346.2). The engine renames
/// a label in the rows that hold it, so a read taken before the rename finds
/// the row only under the label it still has, and one taken after finds it
/// under the new one; the baseline of every apply, staged or not, asks both.
/// `schema` and `ids` are the observed side's, whose type references resolve
/// against the plan's.
fn held_before_the_plan(
    plan: &pbps_model::SavedPlan,
    schema: &Schema,
    ids: &IdsFile,
    scopes: &DataScopes,
) -> DataScopes {
    held_before(&plan.changes, &plan.ids, schema, ids, scopes)
}

/// [`held_before_the_plan`] on its parts, for the side that has the changes
/// and the ids but not yet a saved plan.
fn held_before(
    changes: &pbps_model::ChangeSet,
    plan_ids: &IdsFile,
    schema: &Schema,
    ids: &IdsFile,
    scopes: &DataScopes,
) -> DataScopes {
    pbps_diff::enum_labels::scopes_before_the_plan(
        scopes,
        schema,
        (&TypeResolver::new(ids, plan_ids), TypeSide::Base),
        &pbps_diff::enum_labels::label_renames(changes.changes.iter().map(|p| &p.change)),
    )
}

/// The scopes a plan's baseline checksum is taken over, on both sides of the
/// pin: [`pinned_scopes`] of the recorded scopes and the planned ones, with the
/// planned ones widened by the labels their keys hold before the plan's renames
/// (DEC-2346.2). A row the plan depends on only under the label it has now,
/// the takeover of an unmanaged `sad` renamed to `blue`, is in what the
/// checksum pins, so a change to it between `plan` and `apply` is drift. The
/// plan computes it and the apply recomputes it from the same inputs through
/// this one function; with no rename of a label a key holds it is
/// [`pinned_scopes`] exactly.
fn pinned_baseline_scopes(
    recorded: &DataScopes,
    planned: &DataScopes,
    changes: &pbps_model::ChangeSet,
    plan_ids: &IdsFile,
    (schema, ids): (&Schema, &IdsFile),
) -> DataScopes {
    pinned_scopes(
        recorded,
        &held_before(changes, plan_ids, schema, ids, planned),
    )
}

/// The declared schema with its tables keyed by the names `live_ids` gives
/// their uids — the names the database has before this plan runs — so that
/// a scope, a row reference or a row spelling looked up by the live name
/// finds the declaration of the same table. Everything inside a table is
/// left as declared; only the map keys move.
fn tables_under(schema: &Schema, final_ids: &IdsFile, live_ids: &IdsFile) -> Schema {
    let mut out = schema.clone();
    out.tables = schema
        .tables
        .iter()
        .map(|(final_name, table)| (final_ids.resolved_in(final_name, live_ids), table.clone()))
        .collect();
    out
}

/// [`scopes_at`] on its parts: `data` keyed by the names `final_ids` gives
/// each table, re-keyed by the names `live_ids` gives the same uids.
///
/// The lookup itself is [`IdsFile::resolved_in`] — shared with `doctor`'s
/// readiness questions (DECISIONS 439), which resolve a declared object to
/// the name an environment currently has it under for the same reason this
/// does: a name only the declarations have moved to is not yet a name the
/// database answers to.
fn scopes_under(data: &DataScopes, final_ids: &IdsFile, live_ids: &IdsFile) -> DataScopes {
    data.iter()
        .map(|(final_name, scope)| (final_ids.resolved_in(final_name, live_ids), scope.clone()))
        .collect()
}

/// Every fact inside the managed set that the catalog projection could not
/// express: an unsupported feature on a managed table, or a module the set
/// names whose definition introspection cannot read back.
///
/// The second half is what keeps a recorded scope honest. [`managed_modules`]
/// rebuilds the set from the recorded schema's keys, so a name that schema does
/// not hold is a name every later command forgets. Left as a warning, a
/// declared encrypted procedure was exempted from `unmanaged: error` by the
/// `baseline` that recorded it and refused as an undeclared object by the
/// first `verify` after — a policy violation on a database nothing had touched.
/// A module pbps cannot read is inside the managed set by name and outside it
/// in fact; that is a partial schema, and the recorders already refuse one.
pub(crate) fn managed_limitations(
    pulled: &pbps_db::catalog::Pulled,
    ids: &IdsFile,
    modules: &std::collections::BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
) -> Vec<String> {
    let mut managed_tables: std::collections::BTreeSet<_> = ids.tables.values().collect();
    // A partition of a managed parent whose partitions are external (#1533)
    // is held only as its parent makes it: what the catalog read could not
    // take in, a hash index of its own say, is a part of its own the scope
    // cannot see, and is refused as one (#1833 review).
    managed_tables.extend(pulled.schema.tables.iter().filter_map(|(name, table)| {
        let of = table.partition_of.as_ref()?;
        (external.contains(&of.parent) && ids.tables.values().any(|t| t == &of.parent))
            .then_some(name)
    }));
    pulled
        .limitations
        .iter()
        .filter(|limitation| match &limitation.target {
            pbps_db::catalog::LimitationTarget::Type(name) => ids
                .types
                .values()
                .any(|t| t.schema() == name.schema && t.name() == name.name),
            // A text search object has no identity yet (#2728), so none is inside the
            // managed set: its omission is named by the census and not reported here.
            pbps_db::catalog::LimitationTarget::Search(_) => false,
            pbps_db::catalog::LimitationTarget::Relation(_)
            | pbps_db::catalog::LimitationTarget::SharedModule(_)
            | pbps_db::catalog::LimitationTarget::Module(_)
            | pbps_db::catalog::LimitationTarget::UnnameableModule(_) => {
                omission_is_managed(&limitation.target, &managed_tables, modules)
            }
        })
        .map(|limitation| limitation.detail.clone())
        .chain(
            pulled
                .unmanaged_modules
                .iter()
                .filter(|m| omission_is_managed(&m.target, &managed_tables, modules))
                // The catalog keeps both inventories for different consumers
                // (DECISIONS 426). Only this exact fact supersedes the fallback;
                // matching by target alone would discard another limitation.
                .filter(|m| {
                    !pulled.limitations.iter().any(|limitation| {
                        limitation.target == m.target && limitation.detail == m.why
                    })
                })
                .map(|m| {
                    format!(
                        "{} {} is in the managed set, but {}",
                        m.kind,
                        pbps_model::escape::Escaped(&m.target.to_string()),
                        m.why
                    )
                }),
        )
        .collect()
}

/// Omitted relations still belong to identity-file tables even when absent
/// from the pulled schema. Routine arguments and trigger parents retain their
/// own namespaces; a table identity must not capture either (DECISIONS 425).
fn omission_is_managed(
    target: &pbps_db::catalog::LimitationTarget,
    tables: &BTreeSet<&TableName>,
    modules: &BTreeSet<ModuleId>,
) -> bool {
    matches!(target, pbps_db::catalog::LimitationTarget::Relation(name) if tables.contains(name))
        || modules.iter().any(|id| target.matches_module(id))
}

/// The unexpressible permissions that are this project's business.
///
/// A managed role's, first — an unmanaged role's grants are its own
/// (DECISIONS 95, 125). And then, on the securable: a `DENY`, a column-level
/// grant or a `WITH GRANT OPTION` on an object **outside the managed set** is
/// that object's business too, exactly as `pbps_diff::scope` says of the
/// *plain* grant beside it — "a grant on somebody else's table is that table's
/// business, and comparing it would have the next plan revoke a permission the
/// declarations were never allowed to name". Filtered by role alone, the plain
/// grant was dropped and the unsupported one stopped every command
/// (DECISIONS 176).
///
/// Membership is tested against the managed set as *declared*, not against the
/// cut schema: a managed module the catalog could not read back is absent from
/// the second and still ours (491edd9).
///
/// Schema targets and the targetless ones stay. A schema grant is declarable,
/// so a `DENY` on one is a difference the declarations cannot hold; a
/// permission on the database itself belongs to no object at all, and a role
/// that gained one has changed (DECISIONS 105).
pub(crate) fn unexpressible_permissions<'a>(
    pulled: &'a pbps_db::catalog::Pulled,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
) -> Vec<&'a str> {
    permissions_the_managed_set_keeps(&pulled.unexpressible, ids, modules)
}

/// The same cut, asked of a list on its own — by `init`, which has findings
/// and a generated identity file but no `Pulled` left to ask.
///
/// One function rather than two that have to agree: `init` tells the operator
/// which of these `baseline` will refuse over, and `baseline` is this filter
/// (#308).
pub(crate) fn permissions_the_managed_set_keeps<'a>(
    found: &'a [pbps_db::catalog::Unexpressible],
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
) -> Vec<&'a str> {
    let managed_tables: BTreeSet<&TableName> = ids.tables.values().collect();
    found
        .iter()
        .filter(|u| ids.roles.values().any(|managed| managed == &u.role))
        .filter(|u| {
            u.target
                .as_ref()
                .is_none_or(|target| permission_target_is_managed(target, &managed_tables, modules))
        })
        .map(|u| u.what.as_str())
        .collect()
}

/// One target cut for fatal permissions and report-only advisories. A routine
/// signature must not capture a relation of the same name (DECISIONS 176, 384).
fn permission_target_is_managed(
    target: &pbps_model::GrantTarget,
    tables: &BTreeSet<&TableName>,
    modules: &BTreeSet<ModuleId>,
) -> bool {
    match target {
        pbps_model::GrantTarget::Object(o) => {
            tables.contains(o)
                || modules.iter().any(|id| {
                    !matches!(id, ModuleId::Routine(_)) && id.referenced_name().as_ref() == Some(o)
                })
        }
        pbps_model::GrantTarget::Routine(r) => modules
            .iter()
            .any(|id| matches!(id, ModuleId::Routine(other) if other == r)),
        pbps_model::GrantTarget::Sequence(c) => tables.contains(&c.table),
        // Schema grants are declarable independently of object identities;
        // preserve the fatal filter's existing schema scope (DEC-2446.1).
        pbps_model::GrantTarget::Schema(_) => true,
    }
}

fn permission_advisories<'a>(
    found: &'a [pbps_db::catalog::Unexpressible],
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
) -> Vec<&'a str> {
    let tables = ids.tables.values().collect();
    found
        .iter()
        .filter(|u| !ids.roles.values().any(|managed| managed == &u.role))
        // A targetless finding cannot establish managed-object membership.
        .filter(|u| {
            u.target
                .as_ref()
                .is_some_and(|target| permission_target_is_managed(target, &tables, modules))
        })
        .map(|u| u.what.as_str())
        .collect()
}

fn permission_advisory_findings(
    advisories: &[String],
    project: &Project,
) -> Vec<crate::output::Finding> {
    let id = pbps_policy::rules::UNMANAGED_GRANT;
    let policies = project.config.policies();
    let today = pbps_policy::civil::at(crate::unix_seconds(), 0).date();
    policies
        .effective(id)
        .severity
        .filter(|_| !policies.suppressed(id, None, &today))
        .into_iter()
        .flat_map(|severity| {
            advisories.iter().map(move |what| {
                crate::policy_finding(&pbps_model::Finding::new(
                    id,
                    severity,
                    format!("{what}; the grantee is unmanaged, so this permission is outside drift comparison"),
                    None,
                ))
            })
        })
        .collect()
}

/// The tables whose partitions are external (#1533), by identity: the
/// recorded state's and each the plan creates so. Built as
/// [`modules_after`] is, from the state and the plan alone. A uid's setting
/// cannot change within a plan (the differ refuses the switch), so the one
/// set holds for every read the apply takes. Each read names them through
/// the ids it scopes with, by [`externals_under`]: by name, a rename chain
/// that hands a declared parent's name to an external one would mark the
/// declared parent external before its rename ran (#1833 review).
fn external_uids(
    recorded: &pbps_model::StateSnapshot,
    changes: &pbps_model::ChangeSet,
) -> BTreeSet<pbps_model::Uid> {
    let names = pbps_diff::external_parents(&recorded.schema);
    let mut set: BTreeSet<_> = recorded
        .ids
        .tables
        .iter()
        .filter(|(_, name)| names.contains(*name))
        .map(|(uid, _)| uid.clone())
        .collect();
    for p in &changes.changes {
        if let pbps_model::Change::CreateTable { uid, table, .. } = &p.change
            && table.partition_by.as_ref().is_some_and(|by| by.external)
        {
            set.insert(uid.clone());
        }
    }
    set
}

/// The names `ids` gives the tables of [`external_uids`].
fn externals_under(uids: &BTreeSet<pbps_model::Uid>, ids: &IdsFile) -> BTreeSet<TableName> {
    ids.tables
        .iter()
        .filter(|(uid, _)| uids.contains(*uid))
        .map(|(_, name)| name.clone())
        .collect()
}

/// The modules a plan leaves the environment holding.
///
/// Built from the recorded state plus the plan's own changes rather than from
/// the declarations, so that `apply` still needs nothing but the plan file — the
/// same reason the plan carries its ids (SPEC §7.3, and constraint 23).
fn modules_after(
    recorded: &pbps_model::StateSnapshot,
    changes: &pbps_model::ChangeSet,
    settled: Settled,
) -> BTreeSet<ModuleId> {
    let mut set: BTreeSet<_> = recorded.schema.modules.keys().cloned().collect();
    for p in &changes.changes {
        // Every module change names its module and nothing else does, so the
        // accessor is the whole classification; only the direction is left.
        let Some(id) = p.change.module_id() else {
            continue;
        };
        // A module the plan drops leaves the set only once the plan has run.
        // Removed from every mid-run read as well, a module whose `DROP` had
        // not happened yet was already outside the managed set at each
        // checkpoint — absent from the checkpoint's schema, and invisible to
        // `--resume`, which scopes the live side the same way. The remaining
        // `DROP` then ran against an object nobody had looked at since the
        // plan was approved (DECISIONS 164).
        //
        // Keeping it needs no knowledge of which statements have run: one
        // already dropped is simply absent from the catalog, which the read
        // records truthfully, and one still standing stays watched.
        if settled.whole() && matches!(p.change, pbps_model::Change::DropModule { .. }) {
            set.remove(id);
        } else {
            set.insert(id.clone());
        }
    }
    set
}

/// The dependency annotations that describe an intermediate staged state.
///
/// `plan.module_deps` is the complete post-plan annotation set, including the
/// meaningful absence of an edge that was removed. It is therefore right for
/// every module that survives the plan. A module scheduled for deletion may
/// still exist at an early checkpoint, though, and its declaration has already
/// disappeared; until its DROP runs, preserve the dependency information from
/// the recorded state. Edges to modules no longer present at this checkpoint
/// are discarded so the snapshot never claims an impossible dependency.
fn dependency_hints_for_schema(
    recorded: &pbps_model::StateSnapshot,
    plan: &pbps_model::SavedPlan,
    schema: &Schema,
) -> pbps_model::ModuleDeps {
    let dropped: std::collections::BTreeSet<_> = plan
        .changes
        .changes
        .iter()
        .filter(|planned| matches!(planned.change, pbps_model::Change::DropModule { .. }))
        .filter_map(|planned| planned.change.module_id())
        .collect();
    let present: std::collections::BTreeSet<_> = schema.modules.keys().collect();
    let mut dependencies = pbps_model::ModuleDeps::new();

    for name in schema.modules.keys() {
        let source = if dropped.contains(name) {
            recorded.module_deps.get(name)
        } else {
            plan.module_deps.get(name)
        };
        let Some(source) = source else {
            continue;
        };
        let filtered = source
            .iter()
            .filter(|dependency| present.contains(*dependency))
            .cloned()
            .collect();
        dependencies.insert(name.clone(), filtered);
    }
    dependencies.retain(|_, required| !required.is_empty());
    dependencies
}

/// The modules a command is answerable for.
///
/// Two callers, two questions (see [`pbps_diff::scope`]): `verify` asks whether
/// this environment has moved since it was recorded, so the recorded state's
/// modules are the set; everything that plans or records asks what the state
/// should be, so the declarations count too.
///
/// The recorded set is the recorded schema's keys and nothing more. That is
/// sound only because no recorder writes a snapshot whose managed set names a
/// module the schema does not hold — [`managed_limitations`] turns such a
/// module into a refusal before `record` is reached.
pub(crate) fn managed_modules(
    recorded: Option<&pbps_model::StateSnapshot>,
    declared: Option<&pbps_model::Schema>,
) -> BTreeSet<ModuleId> {
    let mut set = BTreeSet::new();
    if let Some(s) = recorded {
        set.extend(s.schema.modules.keys().cloned());
    }
    if let Some(s) = declared {
        set.extend(s.modules.keys().cloned());
    }
    set
}

/// Says so when the deployment lock could not be released after a command that
/// had already failed.
///
/// The command's own error is what the caller returns — a cleanup failure must
/// not replace the reason the deployment stopped — but dropping the unlock
/// result on that path left `__pbps_lock` held with no word about it, and the
/// retry that should have fixed the environment failed as "locked" instead.
fn warn_unreleased(label: &str, released: &Result<bool, pbps_db::DbError>) {
    if let Err(e) = released {
        eprintln!(
            "warning: the deployment lock on `{label}` was not released: {e}\n\
             Release it with `pbps unlock` before the next attempt.",
            label = pbps_model::escape::Escaped(label),
        );
    }
}

/// The `unmanaged: error` policy, refused.
///
/// A distinct type because `verify` has to tell it apart from every other way
/// its work can fail. The database was reached, the catalog was read, and the
/// command found something the project's own policy calls a problem — that is
/// an answer (exit 2, the schema owner's), not a failure to answer (exit 1,
/// CI's). Folded into the catch-all it came back as `environment.unreachable`
/// about a database that had just been read successfully.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UnmanagedPolicy(String);

/// Converts unreadable modules and type limitations into the common identity
/// and description form used by connected commands.
pub(crate) fn unreadable_objects(
    pulled: &pbps_db::catalog::Pulled,
) -> Vec<(pbps_db::catalog::LimitationTarget, String)> {
    pulled
        .unmanaged_modules
        .iter()
        .map(|module| {
            (
                module.target.clone(),
                format!(
                    "{} {} ({})",
                    module.kind,
                    pbps_model::escape::Escaped(&module.target.to_string()),
                    module.why
                ),
            )
        })
        // An omitted type has no definition to scope, but remains a catalog
        // object subject to the unmanaged policy (SPEC §4.8, DEC-1635.1).
        .chain(
            pulled
                .limitations
                .iter()
                .filter(|limitation| {
                    matches!(
                        limitation.target,
                        pbps_db::catalog::LimitationTarget::Type(_)
                    )
                })
                .map(|limitation| {
                    (
                        limitation.target.clone(),
                        format!(
                            "type {} ({})",
                            pbps_model::escape::Escaped(&limitation.target.to_string()),
                            limitation.detail
                        ),
                    )
                }),
        )
        .collect()
}

/// Names every representable or unreadable catalog object outside the managed
/// set — tables, types, modules and roles alike — for applying the `unmanaged:`
/// policy of SPEC §8.2.
pub(crate) fn unmanaged_objects(
    scoped: &pbps_diff::Scoped,
    unreadable: &[(pbps_db::catalog::LimitationTarget, String)],
    ids: &IdsFile,
    managed_modules: &std::collections::BTreeSet<ModuleId>,
) -> Vec<String> {
    let managed_tables = ids.tables.values().collect();
    scoped
        .unmanaged
        .iter()
        .map(|name| name.shown().to_string())
        .chain(
            scoped
                .unmanaged_types
                .iter()
                .map(|name| format!("type {}", name.shown())),
        )
        .chain(
            scoped
                .unmanaged_modules
                .iter()
                .map(|id| id.shown().to_string()),
        )
        // A role is a principal rather than an object, so it has no
        // `TableName` to be listed under and needs its own spelling — but the
        // policy is about everything in the database this project does not
        // declare, and an undeclared role is one of those (ADR-0005).
        .chain(
            scoped
                .unmanaged_roles
                .iter()
                .map(|r| format!("role {}", r.shown())),
        )
        // Unreadable modules and types are absent from `scoped.schema`, but they are
        // still catalog objects. If their target is outside the supplied managed
        // set, the unmanaged policy applies exactly as it does to a readable
        // module. Without this half, encryption was an accidental escape from
        // `unmanaged: error`.
        .chain(
            unreadable
                .iter()
                .filter(|(target, _)| match target {
                    pbps_db::catalog::LimitationTarget::Type(name) => !ids
                        .types
                        .values()
                        .any(|t| t.schema() == name.schema && t.name() == name.name),
                    // Unreadable, and with no identity to be managed by (#2728): outside.
                    pbps_db::catalog::LimitationTarget::Search(_) => true,
                    pbps_db::catalog::LimitationTarget::Relation(_)
                    | pbps_db::catalog::LimitationTarget::SharedModule(_)
                    | pbps_db::catalog::LimitationTarget::Module(_)
                    | pbps_db::catalog::LimitationTarget::UnnameableModule(_) => {
                        !omission_is_managed(target, &managed_tables, managed_modules)
                    }
                })
                .map(|(_, description)| description.clone()),
        )
        .collect()
}

/// Applies the `unmanaged:` policy to both representable and unreadable
/// catalog objects outside the managed set.
fn report_unmanaged(
    scoped: &pbps_diff::Scoped,
    unreadable: &[(pbps_db::catalog::LimitationTarget, String)],
    ids: &IdsFile,
    managed_modules: &std::collections::BTreeSet<ModuleId>,
    policy: pbps_config::Unmanaged,
) -> anyhow::Result<()> {
    let names = unmanaged_objects(scoped, unreadable, ids, managed_modules);
    apply_unmanaged_policy(&names, policy)
}

fn apply_unmanaged_policy(names: &[String], policy: pbps_config::Unmanaged) -> anyhow::Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let names_shown = names.join(", ");
    match policy {
        pbps_config::Unmanaged::Ignore => {}
        pbps_config::Unmanaged::Warn => eprintln!(
            "warning: {} object(s) in this database are not declared and are left alone: {}",
            names.len(),
            names_shown
        ),
        pbps_config::Unmanaged::Error => {
            return Err(UnmanagedPolicy(format!(
                "`unmanaged: error` in pbps.yml, and {} object(s) here are not declared: {}.\n\
                 Declare them (`pbps pull` reverse-generates them) or relax the setting.",
                names.len(),
                names_shown
            ))
            .into());
        }
    }
    Ok(())
}

/// A declared table the database does not have is worth saying out loud
/// wherever it turns up: it is either a hand-dropped table or an identity file
/// that describes a different database.
fn report_missing(scoped: &pbps_diff::Scoped) {
    if !scoped.missing.is_empty() {
        let names: Vec<String> = scoped
            .missing
            .iter()
            .map(|name| name.shown().to_string())
            .collect();
        eprintln!(
            "warning: the identity file names {} table(s) this database does not have: {}",
            names.len(),
            names.join(", ")
        );
    }
    if !scoped.missing_types.is_empty() {
        eprintln!(
            "warning: the identity file names {} type(s) this database does not have: {}",
            scoped.missing_types.len(),
            scoped
                .missing_types
                .iter()
                .map(|name| name.shown().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !scoped.missing_roles.is_empty() {
        eprintln!(
            "warning: the identity file names {} role(s) this database does not have: {}",
            scoped.missing_roles.len(),
            scoped
                .missing_roles
                .iter()
                .map(|name| name.shown().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// Refuses a command that would record or plan over a live permission the
/// declarations cannot hold.
///
/// A managed role's grant `WITH GRANT OPTION`, a column-level grant, a `DENY`:
/// `managed_state` leaves each out of `scoped.schema` and names it here
/// instead (DECISIONS 95, 97, 105). A state recorded from that schema is one
/// `verify` reports as drift the moment it is written, and a plan built on it
/// restates the plain `GRANT` on every run while the engine keeps the option
/// — so `snapshot`, `baseline` and `plan --db` all stop here rather than
/// write down a state that can never be clean (DECISIONS 110).
///
/// `snapshot --force` is not an escape. It answers "record a state that
/// differs from the recorded one", which is a different question from
/// "record a state pbps cannot express at all"; the second has no right
/// answer to force.
fn refuse_unexpressible(scoped: &pbps_diff::Scoped, label: &str, then: &str) -> anyhow::Result<()> {
    if scoped.unexpressible.is_empty() {
        return Ok(());
    }
    bail!(
        "`{label}` holds what the declarations cannot express:\n  {}\n\
         Resolve it by hand, then {then}.",
        scoped.unexpressible.join("\n  "),
        label = pbps_model::escape::Escaped(label),
    );
}

/// Refuses a state that moved while the plan was running, and not by the plan.
///
/// `apply` reads the baseline before the statements and reads the state back
/// after them, and it is the second read it records (SPEC §8.2, DECISIONS
/// 147). Between the two, another session can change something this plan
/// never mentions — revoke a grant the declarations still hold, edit a
/// declared row of a table the plan does not touch — and the read-back takes
/// it in as if the plan had produced it. `apply` then reports success,
/// `verify` is clean against the newly blessed change, and only the next
/// connected plan proposes the declaration back. The pinned checksum does not
/// reach it: it is read before the statements, and this happens after.
///
/// Locking every managed object for the length of an apply is not on offer,
/// and moving the checksum later only moves the window. What is exact is the
/// half of the question the tool can answer with no dialect knowledge at all:
/// **for every object this plan does not touch, the state after is the state
/// before.** The objects it does touch are held by the plan's own
/// preconditions and postconditions (132, 136, 143) and by the locks its own
/// statements take (DECISIONS 150).
///
/// Both ends of a rename count as touched, since the same object is one name
/// before and another after; a created object is absent on one side and a
/// dropped one on the other, and both are named by the change that does it.
///
/// A touched *table* is not exempt down to its rows, though — only down to
/// the rows this plan names. An `AFTER` trigger on a declared table reaches
/// the table's *other* rows from inside the very statement that writes the
/// one the plan asked for, and the statement's own postcondition speaks for
/// that row alone (132, 136, 143). Exempting the whole table let the trigger's
/// collateral write be recorded as the plan's result (DECISIONS 153).
///
/// The table's *shape* stays exempt where the plan names it: a plan that
/// alters a column is meant to change the table, and a concurrent DDL on the
/// same table has to wait for the schema lock this plan's own statements
/// hold. Rows are what a trigger can move while the apply is running.
/// How much of the plan has run by the time the comparison is made.
///
/// The two halves of this check need different amounts of it. Asking whether
/// something the plan does *not* touch moved is fair at any point; asking
/// whether the plan got what it wanted is only fair once every statement has
/// run, and putting the whole plan's postconditions to a staged checkpoint
/// demanded changes that had not executed yet — the run then stopped at its
/// first checkpoint (DECISIONS 161).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Settled {
    /// Every statement of the plan has run.
    Whole,
    /// Every statement has run, *and* the read was taken from the plan's
    /// scope alone — no recorded row said how to spell a cell at its default,
    /// so one the engine confirmed at its default is omitted, and one that is
    /// there is not at it. That is the one read a cell the plan leaves to its
    /// default can be held to (DECISIONS 191). A checkpoint read is spelled
    /// against the checkpoint before it and cannot say the same.
    Closing,
    /// Some of them have. Only movement is comparable.
    SoFar,
}

impl Settled {
    /// Whether the plan's postconditions can be asked.
    fn whole(self) -> bool {
        self != Settled::SoFar
    }
}

/// Whether two types are one type to the dialect.
///
/// SQL Server fills in a type's defaulted arguments, so a declared `decimal`
/// is stored `decimal(18,0)`, `char` as `char(1)`, `float` as `float(53)` and
/// `nvarchar` as `nvarchar(1)`; compared raw, a valid apply is refused.
/// `normalize_type` expands exactly those. A type it cannot normalize is one
/// nothing here can say anything about, and gets no answer rather than a wrong
/// one (DECISIONS 186).
fn same_type(
    dialect: &dyn pbps_dialect::Dialect,
    a: &pbps_model::ColumnType,
    b: &pbps_model::ColumnType,
) -> bool {
    match (dialect.normalize_type(a), dialect.normalize_type(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => true,
    }
}

/// Whether an index read back is the one declared. Structure only: the
/// filter's text is the engine's to rewrite, and whether there is one at all
/// decides which rows the index covers, and is not (DECISIONS 185). The
/// access method is read back exactly, and a GIN index rebuilt as a B-tree
/// answers other queries (DEC-1169.1).
fn index_as_declared(declared: &pbps_model::Index, now: &pbps_model::Index) -> bool {
    declared.columns.len() == now.columns.len()
        && declared
            .columns
            .iter()
            .zip(&now.columns)
            .all(|(d, n)| key_as_declared(d, n))
        && declared.include == now.include
        && declared.unique == now.unique
        && declared.filter.is_some() == now.filter.is_some()
        && declared.method == now.method
        // Both canonical (#1442), so compared outright: another session's
        // `ALTER INDEX … SET` after the creation is not this plan's (#1483
        // review).
        && declared.storage_parameters == now.storage_parameters
}

/// Whether an exclusion constraint read back is the one declared (DEC-2448.3):
/// the method, timing, payload, storage parameters, and each key's direction,
/// class and operator, with an expression by being one, as an index key is, and
/// the predicate by being there or not.
fn exclusion_as_declared(
    declared: &pbps_model::ExclusionConstraint,
    now: &pbps_model::ExclusionConstraint,
) -> bool {
    declared.method == now.method
        && declared.timing == now.timing
        && declared.include == now.include
        && declared.filter.is_some() == now.filter.is_some()
        && declared.storage_parameters == now.storage_parameters
        && declared.keys.len() == now.keys.len()
        && declared.keys.iter().zip(&now.keys).all(|(d, n)| {
            key_as_declared(&d.key, &n.key)
                && d.key.collation == n.key.collation
                && d.operator == n.operator
        })
}

/// Whether an index key read back is the one declared: a column by its name,
/// and an expression by being one — its text is the engine's to respell, as a
/// filter's is (DEC-1169.2) — each with the same direction and class.
fn key_as_declared(declared: &pbps_model::IndexColumn, now: &pbps_model::IndexColumn) -> bool {
    declared.descending == now.descending
        && declared.opclass == now.opclass
        && match (&declared.key, &now.key) {
            (pbps_model::IndexKey::Column(a), pbps_model::IndexKey::Column(b)) => a == b,
            (pbps_model::IndexKey::Expression(_), pbps_model::IndexKey::Expression(_)) => true,
            (pbps_model::IndexKey::Column(_), pbps_model::IndexKey::Expression(_))
            | (pbps_model::IndexKey::Expression(_), pbps_model::IndexKey::Column(_)) => false,
        }
}

/// Whether a primary key read back is the one declared. The name only where
/// the declaration gives one: `name: None` leaves it to the database, and the
/// engine's generated `PK__t__3213E83F` is not movement.
fn primary_key_as_declared(
    declared: &pbps_model::PrimaryKey,
    now: &pbps_model::PrimaryKey,
) -> bool {
    declared.columns == now.columns
        && !declared
            .name
            .as_ref()
            .is_some_and(|n| Some(n) != now.name.as_ref())
        // As an index's (#1442, #1483 review).
        && declared.storage_parameters == now.storage_parameters
}

/// `schema` with each owned sequence's name set to the one it has
/// ([`pbps_dialect::Dialect::owned_sequence_name`]), so that two reads compare
/// the sequence and not the name its column's or table's name would generate.
fn sequences_named<'a>(
    dialect: &dyn pbps_dialect::Dialect,
    schema: &'a Schema,
) -> std::borrow::Cow<'a, Schema> {
    let any = schema.tables.values().any(|t| {
        t.columns
            .values()
            .any(|c| matches!(c.value_source, pbps_model::ValueSource::Sequence(_)))
    });
    if !any {
        return std::borrow::Cow::Borrowed(schema);
    }
    let mut out = schema.clone();
    for (name, table) in &mut out.tables {
        for (column, spec) in &mut table.columns {
            if let pbps_model::ValueSource::Sequence(sequence) = &mut spec.value_source {
                sequence.name = dialect.owned_sequence_name(name, column.raw(), sequence);
            }
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
fn refuse_unplanned_movement(
    dialect: &dyn pbps_dialect::Dialect,
    changes: &pbps_model::ChangeSet,
    before: &Schema,
    after: &Schema,
    label: &str,
    settled: Settled,
) -> anyhow::Result<()> {
    let ids = guard::fixture_ids(changes, before);
    let plan = guard::Plan { changes, ids: &ids };
    let read = match settled {
        // Final-shape fixtures use the transactional plan-scoped read.
        // Staged terminal-checkpoint policy is tested through staged_movement.
        Settled::Whole | Settled::Closing => guard::Read::transactional(plan),
        Settled::SoFar => guard::Staged::new(plan)?.read(StagedRead::Checkpoint {
            completed: 1,
            total: 2,
        }),
    };
    refuse_pinned_movement(dialect, read, before, after, label)
}

#[cfg(test)]
fn staged_comparison(
    dialect: &dyn pbps_dialect::Dialect,
    changes: &pbps_model::ChangeSet,
    before: &Schema,
    after: &Schema,
    label: &str,
    settled: Settled,
) -> anyhow::Result<()> {
    let read = match settled {
        Settled::SoFar => StagedRead::Checkpoint {
            completed: 1,
            total: 2,
        },
        Settled::Whole => StagedRead::Checkpoint {
            completed: 2,
            total: 2,
        },
        Settled::Closing => StagedRead::Closing { total: 2 },
    };
    staged_movement(dialect, changes, before, after, label, None, read)
}

#[cfg(test)]
fn staged_movement(
    dialect: &dyn pbps_dialect::Dialect,
    changes: &pbps_model::ChangeSet,
    before: &Schema,
    after: &Schema,
    label: &str,
    environment: Option<&str>,
    read: StagedRead,
) -> anyhow::Result<()> {
    let ids = guard::fixture_ids(changes, before);
    staged_pinned_movement(
        dialect,
        guard::Staged::new(guard::Plan { changes, ids: &ids })?,
        before,
        after,
        label,
        environment,
        read,
        None,
        None,
    )
}

#[allow(clippy::wildcard_enum_match_arm)] // Type deltas are checked apart from table/module effects.
fn refuse_pinned_movement(
    dialect: &dyn pbps_dialect::Dialect,
    read: guard::Read<'_>,
    before: &Schema,
    after: &Schema,
    label: &str,
) -> anyhow::Result<()> {
    let verified = read.retained();
    let defaults = read.domain_defaults();
    let (plan, settled) = read.parts();
    let changes = plan.changes;
    let mut named_before = sequences_named(dialect, before);
    use pbps_diff::identity::type_refs::{TypeResolver, TypeSide, after_changes, before_changes};
    let base_ids = before_changes(plan.ids, changes);
    let physical_types = match defaults {
        Some(defaults) => std::borrow::Cow::Borrowed(defaults),
        None => std::borrow::Cow::Owned(pbps_pg::type_refs::declared::completed(
            before, &base_ids, changes,
        )),
    };
    let realized = |uid, definition: &pbps_model::ManagedType| {
        let mut expected = definition.clone();
        if let (
            pbps_model::ManagedType::Domain { default, .. },
            Some(pbps_model::ManagedType::Domain {
                default: physical, ..
            }),
        ) = (&mut expected, physical_types.get(uid))
        {
            *default = physical.clone();
        }
        expected
    };
    let desired_ids = after_changes(&base_ids, changes);
    let resolver = TypeResolver::new(&base_ids, &desired_ids);
    let renames = pbps_pg::type_refs::renames::TypeRenames::resolved(&base_ids, &desired_ids);
    let mut named_after = sequences_named(dialect, after);
    if !renames.0.is_empty() {
        renames.schema(named_before.to_mut());
        pbps_pg::type_refs::renames::TypeRenames::default().schema(named_after.to_mut());
    }
    if let Some(verified) = verified {
        verified.comparison(named_before.to_mut(), named_after.as_ref());
    }
    let after = named_after.as_ref();
    let before = named_before.as_ref();
    let mut expected_types = BTreeMap::new();
    for step in &changes.changes {
        match &step.change {
            pbps_model::Change::AddEnumValue {
                uid, definition, ..
            }
            | pbps_model::Change::AlterDomain {
                uid,
                to: definition,
                ..
            }
            | pbps_model::Change::RenameEnumValue {
                uid, definition, ..
            }
            | pbps_model::Change::CreateType {
                uid, definition, ..
            }
            | pbps_model::Change::RenameType {
                uid, definition, ..
            } => {
                if let Some(name) = resolver.name(TypeSide::Desired, uid) {
                    expected_types.insert(name.clone(), Some(realized(uid, definition)));
                }
            }
            // The replacement may stand anywhere between its creation and the
            // swap, and no longer after it; the swap leaves the final labels
            // under the type's own name (DEC-1635.7).
            pbps_model::Change::EnumRebuild { uid, tmp, step, .. } => {
                expected_types.entry(tmp.clone()).or_insert(None);
                if let pbps_model::EnumRebuildStep::Swap { definition, .. } = step
                    && let Some(name) = resolver.name(TypeSide::Desired, uid)
                {
                    expected_types.insert(name.clone(), Some(realized(uid, definition)));
                }
            }
            _ => {}
        }
    }
    // Retired names and intermediate addresses may be reused by another uid.
    // Only the final identity map decides which names must be absent.
    for step in &changes.changes {
        let old = match &step.change {
            pbps_model::Change::DropType { name, .. } => Some(name.clone()),
            pbps_model::Change::RenameType { from, to, .. } => {
                if let Ok(intermediate) = pbps_model::TypeRef::new(to.schema(), from.name()) {
                    expected_types.entry(intermediate).or_insert(None);
                }
                Some(from.clone())
            }
            _ => None,
        };
        if let Some(name) = old {
            expected_types.entry(name).or_insert(None);
        }
    }
    // Both reads are already scoped to held identities. A plan admits only
    // creates (DEC-2447.1, DEC-2447.6): each package it installs may appear
    // between its statement and the close, in exactly the pinned definition,
    // and every other package keeps its exact definition throughout.
    let installs: BTreeMap<&pbps_model::ObjectLocalName, &pbps_model::Extension> = changes
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            pbps_model::Change::CreateExtension {
                name, definition, ..
            } => Some((name, definition)),
            _ => None,
        })
        .collect();
    // The packages a drop removes: standing until its statement, absent from
    // it on, and in exactly the pinned definition while they stand.
    let removals: BTreeMap<&pbps_model::ObjectLocalName, &pbps_model::Extension> = changes
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            pbps_model::Change::DropExtension {
                name, definition, ..
            } => Some((name, definition)),
            _ => None,
        })
        .collect();
    // The packages an update or a move changes: at one of the definitions its
    // chain passes through while the plan runs, and at the end of it nowhere
    // else (DEC-2447.10, DEC-2447.11).
    let mut updates: BTreeMap<
        &pbps_model::ObjectLocalName,
        (Vec<&pbps_model::Extension>, &pbps_model::Extension),
    > = BTreeMap::new();
    for p in &changes.changes {
        if let pbps_model::Change::UpdateExtension { name, from, to, .. } = &p.change {
            let entry = updates.entry(name).or_insert((Vec::new(), to));
            entry.0.extend([from, to]);
        }
    }
    for p in &changes.changes {
        if let pbps_model::Change::SetExtensionSchema { name, from, to, .. } = &p.change {
            let entry = updates.entry(name).or_insert((Vec::new(), to));
            entry.0.extend([from, to]);
            entry.1 = to;
        }
    }
    let extensions: BTreeSet<_> = before
        .extensions
        .keys()
        .chain(after.extensions.keys())
        .chain(installs.keys().copied())
        .chain(removals.keys().copied())
        .chain(updates.keys().copied())
        .collect();
    for name in extensions {
        let (was, now) = (before.extensions.get(name), after.extensions.get(name));
        if let Some((passes, to)) = updates.get(name) {
            let stands =
                |held: Option<&pbps_model::Extension>| held.is_some_and(|d| passes.contains(&d));
            if !stands(was)
                || !stands(now)
                || (now != Some(*to) && !matches!(settled, Settled::SoFar))
            {
                bail!(
                    "extension `{}` does not match what its plan promised during {label}",
                    name.shown()
                )
            }
            continue;
        }
        if let Some(pinned) = removals.get(name) {
            if was.is_some_and(|was| was != *pinned)
                || now.is_some_and(|now| now != *pinned)
                || (now.is_some() && !matches!(settled, Settled::SoFar))
            {
                bail!(
                    "extension `{}` does not match what its plan promised during {label}",
                    name.shown()
                )
            }
            continue;
        }
        match installs.get(name) {
            None if was != now => bail!("extension `{}` changed during {label}", name.shown()),
            // A resumed run starts from a checkpoint that may already hold
            // the package, so `was` may be the pinned definition too.
            Some(wrote)
                if was.is_some_and(|was| was != *wrote)
                    || now.is_some_and(|now| now != *wrote)
                    || (now.is_none() && !matches!(settled, Settled::SoFar)) =>
            {
                bail!(
                    "extension `{}` does not match what its plan promised during {label}",
                    name.shown()
                )
            }
            _ => {}
        }
    }
    // A managed domain over a type a moved package provides reads back with
    // its base in the new schema and no change of the plan on it: the same
    // carrying as for a column of the table (DEC-2447.12).
    let package_moves = pbps_diff::package_moves(changes);
    let mut types_before = before.types.clone();
    pbps_diff::forward_domain_moves(&mut types_before, &after.types, &package_moves);
    let types: BTreeSet<_> = types_before.keys().chain(after.types.keys()).collect();
    for name in types {
        if types_before.get(name) != after.types.get(name) && !expected_types.contains_key(name) {
            bail!("type `{}` changed during {label}", name.shown());
        }
    }
    if !matches!(settled, Settled::SoFar) {
        for (name, expected) in &expected_types {
            let matches = match (expected, after.types.get(name)) {
                (Some(wrote), Some(now)) => {
                    pbps_pg::type_refs::declared::matches_declaration(wrote, now)
                }
                (None, None) => true,
                _ => false,
            };
            if !matches {
                bail!(
                    "type `{}` does not match what its plan promised during {label}",
                    name.shown()
                );
            }
        }
    }
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, plan.ids)?;
    // Each owned sequence under the name it has, before anything below maps
    // a column or table across a rename: a held name is `None` where it is
    // the generated one, and a rename changes that without touching the
    // sequence (#1631).
    let (before, after) = (&named_before, &named_after);
    let mut objects: BTreeSet<&TableName> = BTreeSet::new();
    // Modules are counted in their own set, under their own identity: a
    // module is no longer a name in the tables namespace (ADR-0009 §1), and
    // asking whether the plan touched `app.f` would exempt every overload of
    // it from the comparison below.
    let mut touched_modules: BTreeSet<&pbps_model::ModuleId> = BTreeSet::new();
    let mut roles: BTreeSet<&pbps_model::RoleName> = BTreeSet::new();
    for p in &changes.changes {
        objects.extend(p.change.objects());
        touched_modules.extend(p.change.module_id());
        roles.extend(p.change.roles());
    }
    let table_changes = guard::TableChanges::of(changes, &names);
    let column_changes = guard::ColumnChanges::of(changes, &names);
    let part_changes = guard::PartChanges::of(changes, &names);
    let created = guard::CreatedChanges::of(changes, &names, &table_changes, &part_changes);
    let implied = table_changes.implied(before, after, &objects);
    objects.extend(implied.names());

    // Row scope remains keyed by each statement's table spelling. The table
    // expectations below carry that scope to the surviving identity.
    let mut written: BTreeMap<&TableName, BTreeSet<&pbps_model::RowKey>> = BTreeMap::new();
    // The permissions this plan moves, keyed by the role it moves them on and
    // the target they sit on. A role can be both granted and revoked on one
    // target in one plan, so the sets are unioned rather than replaced.
    // What this plan leaves each role holding on each target it touches: the
    // permissions it adds and the ones it takes away, kept apart because the
    // comparison below reconstructs the set rather than excusing it.
    type Permissions = BTreeSet<pbps_model::Permission>;
    let mut granting: BTreeMap<(pbps_model::RoleName, pbps_model::GrantTarget), Permissions> =
        BTreeMap::new();
    let mut revoking: BTreeMap<(pbps_model::RoleName, pbps_model::GrantTarget), Permissions> =
        BTreeMap::new();
    // The columns this plan changes, under the table they are on, and the
    // objects it removes outright. Both are things the plan does to a
    // *container* that show up somewhere else without any change of its own
    // saying so: a renamed column re-keys every row of its table, and a
    // dropped table takes its grants with it (DECISIONS 158).
    let mut columns: BTreeMap<TableName, BTreeSet<pbps_model::ColumnName>> = BTreeMap::new();
    let mut gone: BTreeSet<pbps_model::Dropped> = BTreeSet::new();
    for p in &changes.changes {
        if let Some((table, key, _)) = p.change.row() {
            written.entry(table).or_default().insert(key);
        }
        if let Some((role, target, permissions)) = p.change.grant() {
            let (side, permissions) = match permissions {
                pbps_model::PermissionChange::Granted(p) => (&mut granting, p),
                pbps_model::PermissionChange::Revoked(p) => (&mut revoking, p),
            };
            let one = pbps_model::Role {
                grants: [(target.clone(), permissions.clone())].into(),
                ..Default::default()
            };
            // Statements carrying the marker operate on the sequences present
            // when they run; grants follow creations, revokes precede drops.
            let grant_tables = if matches!(p.change, pbps_model::Change::Grant { .. }) {
                &after.tables
            } else {
                &before.tables
            };
            for (target, permissions) in pbps_model::role::effective_grants(&one, grant_tables) {
                side.entry((role.clone(), target))
                    .or_default()
                    .extend(permissions);
            }
        }
        for column in p.change.columns() {
            columns.entry(column.table).or_default().insert(column.name);
        }
        if let Some(dropped) = p.change.drops() {
            gone.insert(dropped);
        }
    }

    let tables = guard::Tables::of(&names, before, after, &column_changes);
    let undo_before = &tables.undo_before;
    let undo_after = &tables.undo_after;
    let partition_hands = &column_changes.partition_handoffs;

    let mut moved = Vec::new();
    let named = |n: &TableName| objects.contains(n);
    // A child's foreign key follows the parent it references through
    // `sp_rename` — the referenced table and its columns are both rewritten by
    // the engine — so the child's shape moves with no change of this plan on
    // it and nothing to excuse it with.
    // The engine renames a label where a partition's bound holds it, so a
    // standing partition's bound follows this plan's label renames without a
    // change of its own (DEC-2346.1).
    let label_renames =
        pbps_diff::enum_labels::label_renames(changes.changes.iter().map(|p| &p.change));
    // The same for a column of a type a moved package provides: the type keeps
    // its OID and its name, and only its schema follows the package, so a table
    // this plan does not touch reads back with the new schema in its column
    // types and no change of the plan on it (DEC-2447.12).
    let label_forwarded = |table: &pbps_model::Table, now: Option<&pbps_model::Table>| {
        let mut table = table.clone();
        if let Some(now) = now {
            pbps_diff::forward_schema_moves(&mut table, now, &package_moves);
        }
        if let Some(of) = &mut table.partition_of
            && let Some(parent) = before.tables.get(&of.parent)
        {
            of.bound = pbps_diff::enum_labels::forwarded_bound(
                &of.bound,
                parent,
                (&resolver, TypeSide::Desired),
                &label_renames,
            );
        }
        // And so do its rows (DEC-2346.2).
        pbps_diff::enum_labels::forwarded_rows(
            &table,
            (&resolver, TypeSide::Desired),
            &label_renames,
        )
        .unwrap_or(table)
    };
    compare(
        "",
        &before.tables,
        &after.tables,
        named,
        |name: &TableName, was: &pbps_model::Table, now: &pbps_model::Table| {
            let read = undo_after.apply(now, name);
            label_forwarded(&undo_before.apply(was, name), Some(&read)) == *read
        },
        &mut moved,
        |n| n.shown().to_string(),
    );
    guard::implied(&implied, after, &mut moved);
    // Every touched table, under the name it ends with — including the ones
    // this plan creates, which have no `before` entry to be found under. A
    // loop over the baseline alone never visited a new table, so an
    // undeclared row that arrived in one (a DDL trigger, or another session
    // between a staged `CREATE TABLE` and its checkpoint) was recorded into an
    // `exact` snapshot and read as clean ever after (DECISIONS 163). A
    // created table's baseline is simply no rows, which is what it had.
    let carried: BTreeSet<&TableName> = before
        .tables
        .keys()
        .filter(|name| !tables.retired(name))
        .map(|name| tables.final_name(name))
        .collect();
    let no_rows = pbps_model::TableData {
        mode: pbps_model::DataMode::Exact,
        rows: Default::default(),
    };
    // The rows the engine renames a label in follow this plan's label renames
    // without a change of their own: the baseline's keys and cells are brought
    // forward as the differ brought them (DEC-2346.2).
    let forwarded_rows: BTreeMap<&TableName, pbps_model::TableData> = before
        .tables
        .iter()
        .filter_map(|(name, was)| {
            let forwarded = pbps_diff::enum_labels::forwarded_rows(
                was,
                (&resolver, TypeSide::Desired),
                &label_renames,
            )?;
            Some((name, forwarded.data?))
        })
        .collect();
    let touched_tables = before
        .tables
        .iter()
        .filter(|(name, _)| !tables.retired(name))
        .map(|(name, was)| {
            (
                name,
                forwarded_rows
                    .get(name)
                    .or_else(|| guard::Fields::of(was).data),
            )
        })
        .chain(
            after
                .tables
                .keys()
                .filter(|name| named(name) && !carried.contains(*name))
                .map(|name| (name, Some(&no_rows))),
        );
    for (name, was_rows) in touched_tables {
        if !named(name) {
            // Already compared whole, rows included.
            continue;
        }
        let now_name = tables.final_name(name);
        // The table's *shape*, entry by entry, over everything this plan does
        // not move. Its own columns and constraints are compared between two
        // read-backs — never against the declaration — so nothing here depends
        // on predicting the engine's stored form, which is why the whole table
        // could stop being exempt (DECISIONS 166). What the plan's own
        // alterations achieved is checked once every statement has run,
        // below: against the plan's own values, in the fields the catalog
        // reads back unchanged (DECISIONS 189).
        //
        // At every read, not only the settled one. Everything this plan will
        // move is excluded below whether or not its statement has run, so
        // nothing here needs the plan to be finished — and gating it on the
        // last read meant a change that landed before an earlier checkpoint
        // went into `previous`, after which the final comparison measured the
        // contaminated shape against itself (DECISIONS 167).
        // A table this plan creates has no `before` to be compared against, and
        // `CreateTable` names no column and no part of its own, so nothing
        // answered for its shape: a DDL trigger, or another session between a
        // staged `CREATE TABLE` and its checkpoint, could add a column or an
        // index to it and have that recorded as this plan's result. 163 gave
        // such a table a synthetic baseline of *no rows* for the same reason;
        // this is the other half of it (DECISIONS 181).
        //
        // Keep asking after the table enters the checkpoint: its constraints
        // and indexes can be separate later statements, replaced by a trigger
        // before their first read. Comparing only with that checkpoint would
        // certify the replacement against itself (decision 458). Comparable
        // structure is held to the declaration; engine-rewritten expressions
        // still use presence, as SPEC 7.6 requires.
        // Only at a read taken while statements remain: once every one has
        // run, an attached table that reads as an ordinary one is movement.
        let not_yet_attached = !settled.whole()
            && created.get(now_name).is_some_and(|table| table.attached())
            && after
                .tables
                .get(now_name)
                .is_some_and(|t| t.partition_of.is_none());
        if let (Some(declared), Some(now)) = (created.get(now_name), after.tables.get(now_name))
            && !not_yet_attached
        {
            declared.check(
                dialect,
                now_name,
                &guard::Fields::of(now),
                settled,
                &mut moved,
            );
        }
        // Held to its declared shape above: a detached table's partition held
        // nothing to compare it with, and an attached table's columns are its
        // parent's now (#1545).
        if created.get(now_name).is_some_and(|table| table.detached())
            || (created.get(now_name).is_some_and(|table| table.attached()) && !not_yet_attached)
        {
            continue;
        }
        if let (Some(was), Some(now)) = (before.tables.get(name), after.tables.get(now_name))
            && carried.contains(now_name)
        {
            // A rename carries the constraints that name the column or the
            // table with it — a primary key, a unique, an index's key and
            // `INCLUDE` columns, a child's `references_table` — so no change
            // of this plan restates them and there is nothing here to excuse
            // them with. The before side is brought forward to the spelling
            // the rename leaves, exactly as the differ does when it decides
            // not to emit the restatement in the first place. What may be
            // rewritten and what may not is `Renames::apply`'s business.
            // Both reads back in the spelling that precedes this plan, so
            // the constraints its renames carry compare equal without any
            // change of its own restating them.
            //
            // Both, not the later one alone: a staged run's closing check
            // compares two checkpoints, and by then the earlier of them
            // already carries the rename. Undoing is idempotent on a name the
            // plan does not rename, so a read from either side of a statement
            // lands in the same spelling.
            let was = &undo_before.apply(was, name);
            let was = &label_forwarded(was, after.tables.get(now_name));
            let now = &undo_after.apply(now, now_name);
            let expected_columns = column_changes.expect(now_name, was, now);
            let was_fields = guard::Fields::of(was);
            let now_fields = guard::Fields::of(now);
            guard::columns(&expected_columns, &now_fields, now_name, &mut moved);
            let pending = (settled != Settled::Closing)
                .then(|| created.get(now_name).map(|created| created.table()))
                .flatten();
            part_changes.kept(now_name, &was_fields, &now_fields, pending, &mut moved);
            let key_handoff = partition_hands.get(now_name);
            let scalar_expected =
                table_changes.expect(name, now_name, was, settled.whole(), key_handoff);
            let placed = if key_handoff.is_some() {
                after.tables.get(now_name).unwrap_or(now)
            } else {
                now
            };
            guard::table(
                &scalar_expected,
                &now_fields,
                &guard::Fields::of(placed),
                now_name,
                &mut moved,
            );
            let parent = was.partition_of.as_ref().and_then(|of| {
                use pbps_model::plan_names::{At, Holder};
                match names.holder(&of.parent, At::Start) {
                    Holder::Tracked(uid) => names.table_at(uid, At::End),
                    Holder::Unmanaged => Some(&of.parent),
                    Holder::Vacant => None,
                }
            });
            let own_handoff = parent.and_then(|parent| partition_hands.get(parent));
            let own_expected = table_changes.own(
                now_name,
                was,
                settled.whole(),
                guard::OwnNames {
                    parent,
                    handoff: own_handoff,
                    undo: undo_after,
                    index: &names,
                },
            );
            let own_now = if own_handoff.is_some() {
                after.tables.get(now_name).unwrap_or(now)
            } else {
                now
            };
            guard::own(
                &own_expected,
                &guard::Fields::of(own_now),
                now_name,
                &mut moved,
            );
            guard::system_time(&scalar_expected, &now_fields, now_name, &mut moved);
            part_changes.storage(
                now_name,
                &was_fields,
                &now_fields,
                settled.whole(),
                &mut moved,
            );
            part_changes.layout(now_name, &was_fields, &now_fields, &mut moved);
        }
        // Dropped, or outside the row scope on one side: there is no pair of
        // row sets to compare, and "absent" is not "empty".
        let (Some(was_rows), Some(now_rows)) = (
            was_rows,
            after
                .tables
                .get(now_name)
                .and_then(|t| guard::Fields::of(t).data),
        ) else {
            continue;
        };
        let empty = BTreeSet::new();
        let plans = written.get(now_name).unwrap_or(&empty);
        // A row is compared on the columns this plan leaves alone. One it
        // renames is under one name in the baseline and another in the
        // read-back; one it adds is in neither; one it retypes reads back in a
        // different rendering. All three are the plan's own doing, and
        // comparing the whole row called every one of them somebody else's
        // (DECISIONS 158).
        let no_columns = BTreeSet::new();
        let touched_columns = columns.get(now_name).unwrap_or(&no_columns);
        // A free function rather than a closure: the borrow of the row has to
        // outlive the call, and a closure cannot say so.
        fn held<'a>(
            row: &'a pbps_model::Row,
            skip: &BTreeSet<pbps_model::ColumnName>,
        ) -> BTreeMap<&'a pbps_model::ColumnName, &'a pbps_model::Value> {
            row.0
                .iter()
                .filter(|(column, _)| !skip.contains(*column))
                .collect()
        }
        for (key, row) in &was_rows.rows {
            if plans.contains(key) {
                continue;
            }
            match now_rows.rows.get(key) {
                Some(after_row)
                    if held(after_row, touched_columns) == held(row, touched_columns) => {}
                Some(_) => moved.push(format!(
                    "{now_name} row `{key}` is not what the plan was approved over, \
                     and no change of this plan writes it",
                    now_name = now_name.shown()
                )),
                None => moved.push(format!(
                    "{now_name} row `{key}` is gone, and no change of this plan deletes it",
                    now_name = now_name.shown()
                )),
            }
        }
        for key in now_rows.rows.keys() {
            if plans.contains(key) || was_rows.rows.contains_key(key) {
                continue;
            }
            moved.push(format!(
                "{now_name} row `{key}` is there, and no change of this plan inserts it",
                now_name = now_name.shown()
            ));
        }
    }
    compare(
        "",
        &before.modules,
        &after.modules,
        |id: &pbps_model::ModuleId| touched_modules.contains(id),
        |_: &pbps_model::ModuleId, was: &pbps_model::Module, now: &pbps_model::Module| was == now,
        &mut moved,
        |id| id.shown().to_string(),
    );
    // A module the plan names is held to the definition the plan wrote, not
    // exempted. `CREATE OR ALTER` reports success and says nothing about what
    // is now stored, and no statement carries a postcondition for one — so a
    // session, or a DDL trigger inside the statement itself, that altered or
    // dropped the module straight afterwards was read back and recorded as
    // this plan's own result (DECISIONS 160).
    // By identity, not by change: a module that changes kind is a
    // `DropModule` *and* a `CreateModule` for one identity (`diff_modules`).
    // Checked separately the drop always failed — the create had put the
    // module back — and every replacement was refused (DECISIONS 161). The
    // plan is in `order_key` order, which puts the drop first, so the last
    // word on an identity is the net one. A trigger moved to another table is
    // no longer such a pair: it is two identities, one dropped and one
    // created, and each answers for itself.
    let mut modules_after_plan: BTreeMap<&pbps_model::ModuleId, pbps_model::ModuleAfter<'_>> =
        BTreeMap::new();
    for p in &changes.changes {
        if let Some((name, expected)) = p.change.module() {
            modules_after_plan.insert(name, expected);
        }
    }
    for (name, expected) in modules_after_plan {
        if settled == Settled::SoFar {
            break;
        }
        match (expected, after.modules.get(name)) {
            (pbps_model::ModuleAfter::Standing(wrote), Some(now))
                if dialect.module_matches_declaration(wrote, now) => {}
            (pbps_model::ModuleAfter::Standing(_), Some(_)) => moved.push(format!(
                "{name} does not hold the definition this plan wrote",
                name = name.shown()
            )),
            (pbps_model::ModuleAfter::Standing(_), None) => moved.push(format!(
                "{name} is not there, and this plan writes it",
                name = name.shown()
            )),
            (pbps_model::ModuleAfter::Gone, None) => {}
            (pbps_model::ModuleAfter::Gone, Some(_)) => moved.push(format!(
                "{name} is still there, and this plan drops it",
                name = name.shown()
            )),
        }
    }

    // The rows the plan writes, held to being there or gone once every
    // statement has run. Their *contents* are held by the statement itself
    // (132, 136, 143) — but only until it commits. In one transaction that is
    // the whole story, because the row stays locked until the commit and
    // nothing else can reach it; a staged run commits each statement, and a
    // row it inserted can be deleted before the checkpoint read
    // (DECISIONS 162).
    if settled.whole() {
        for p in &changes.changes {
            let Some((table, key, expected)) = p.change.row() else {
                continue;
            };
            // Out of the read's row scope, or the table itself is gone: the
            // table checks below answer for that, and "absent" is not "empty".
            let Some(rows) = after.tables.get(table).and_then(|t| t.data.as_ref()) else {
                continue;
            };
            match (expected, rows.rows.get(key)) {
                // Present, and holding what the plan spelled. Only the cells
                // it spells, and only where the read-back carries them: a cell
                // at its column's default is omitted from the read-back, so
                // its absence proves nothing and demanding it would refuse a
                // valid apply (DECISIONS 165).
                (pbps_model::RowAfter::Holding(cells), Some(row)) => {
                    for (column, wrote) in cells {
                        match (wrote, row.get(column.raw())) {
                            (pbps_model::CellAfter::Spelled(wrote), Some(now)) if now != wrote => {
                                moved.push(format!(
                                    "{table} row `{key}` does not hold in `{column}` what this \
                                     plan wrote",
                                    table = table.shown(),
                                    column = column.shown(),
                                ));
                            }
                            (pbps_model::CellAfter::Spelled(_), _) => {}
                            // A cell the plan leaves to its default is held
                            // there only where a present cell *means* not at
                            // it: at the closing read, whose spelling omits a
                            // confirmed default, and on a column whose default
                            // the engine confirms at all. A `NEWID()` cell
                            // comes back with its value on every read, and a
                            // checkpoint read spells a cell the way the
                            // checkpoint before it did (DECISIONS 191).
                            (pbps_model::CellAfter::AtDefault, Some(_))
                                if settled == Settled::Closing
                                    && after
                                        .tables
                                        .get(table)
                                        .and_then(|t| t.columns.get(column))
                                        .is_some_and(|c| dialect.reads_back_at_default(c)) =>
                            {
                                moved.push(format!(
                                    "{table} row `{key}` holds a value in `{column}`, and this \
                                     plan leaves it at its default",
                                    table = table.shown(),
                                    column = column.shown(),
                                ));
                            }
                            (pbps_model::CellAfter::AtDefault, _) => {}
                        }
                    }
                }
                (pbps_model::RowAfter::Holding(_), None) => moved.push(format!(
                    "{table} row `{key}` is not there, and this plan writes it",
                    table = table.shown()
                )),
                (pbps_model::RowAfter::Gone, Some(_)) => moved.push(format!(
                    "{table} row `{key}` is still there, and this plan deletes it",
                    table = table.shown()
                )),
                (pbps_model::RowAfter::Gone, None) => {}
            }
        }
    }

    tables.kept(after, &mut moved);

    // And the names the plan leaves standing or empty. Existence, for the
    // tables and roles themselves: a table's shape is answered for above,
    // entry by entry, and below for the entries this plan writes. A role
    // with no grants has nothing *but* its name, so without this a
    // `CREATE ROLE` another session undid was recorded as success
    // (DECISIONS 161).
    if settled.whole() {
        let mut expected_roles: BTreeMap<&pbps_model::RoleName, pbps_model::Presence> =
            BTreeMap::new();
        for p in &changes.changes {
            expected_roles.extend(p.change.roles_after());
        }
        tables.after(after, &mut moved);
        guard::columns_after(dialect, &column_changes, after, &mut moved);
        part_changes.after(after, &mut moved);
        for (name, expected) in expected_roles {
            match (expected, after.roles.contains_key(name)) {
                (pbps_model::Presence::Present, false) => moved.push(format!(
                    "role {name} is not there, and this plan creates it",
                    name = name.shown(),
                )),
                (pbps_model::Presence::Absent, true) => moved.push(format!(
                    "role {name} is still there, and this plan removes it",
                    name = name.shown(),
                )),
                _ => {}
            }
        }
    }
    // The baseline's grants, spelled the way the read-back will spell them.
    // Measured: SQL Server carries a grant across `sp_rename`, so a plan that
    // renames a granted table changes no permission and the differ emits no
    // grant change at all — the role is untouched. Compared by name alone,
    // the one grant read as two and every such rename was refused
    // (DECISIONS 157).
    let before_roles: BTreeMap<pbps_model::RoleName, pbps_model::Role> = before
        .roles
        .iter()
        .map(|(name, role)| {
            let effective = pbps_model::role::effective_grants(role, &before.tables);
            let grants = effective
                .iter()
                // A grant on an object this plan drops goes with it: the engine
                // removes a permission with its securable, so `diff_roles`
                // emits no `REVOKE` and there is nothing left for the read-back
                // to hold (DECISIONS 158).
                // By whole identity: dropping one overload leaves the
                // sibling's grants standing, and standing grants are compared.
                .filter(|(target, _)| !gone.iter().any(|d| d.takes(target)))
                .filter(|(target, _)| {
                    let pbps_model::GrantTarget::Sequence(column) = target else {
                        return true;
                    };
                    use pbps_model::plan_names::{At, Holder};
                    let Holder::Tracked(column_uid) = names.column_holder(column, At::Start) else {
                        return true;
                    };
                    // The baseline uses starting names; a drop or numbering
                    // change uses the names at its step. Only the UID survives
                    // both renames and another column taking the vacated name.
                    !changes.changes.iter().any(|p| {
                        if let pbps_model::Change::DropColumn { uid, .. } = &p.change {
                            return uid == column_uid;
                        }
                        if let pbps_model::Change::AlterColumnNumbering { uid, to, .. } = &p.change
                        {
                            return uid == column_uid
                                && !matches!(to, pbps_model::ValueSource::Sequence(_));
                        }
                        false
                    })
                })
                .map(|(target, held)| {
                    let target = match target {
                        pbps_model::GrantTarget::Object(object) => {
                            pbps_model::GrantTarget::Object(tables.final_name(object).clone())
                        }
                        // Only tables are renamed, and a routine is not a
                        // table — as `diff_roles` says. Where routines have a
                        // namespace of their own, `app.f(integer)` stands
                        // beside a table `app.f`, and forwarding the routine's
                        // grant through the table's rename refused the apply
                        // for a grant that had, rightly, not moved.
                        pbps_model::GrantTarget::Sequence(column) => {
                            use pbps_model::plan_names::{At, Holder};
                            let final_column = match names.column_holder(column, At::Start) {
                                Holder::Tracked(uid) => names
                                    .column_at(uid, At::End)
                                    .and_then(|(table, name)| {
                                        names
                                            .table_at(table, At::End)
                                            .map(|table| table.column(name))
                                    })
                                    .unwrap_or_else(|| column.clone()),
                                Holder::Vacant | Holder::Unmanaged => column.clone(),
                            };
                            pbps_model::GrantTarget::Sequence(final_column)
                        }
                        routine @ pbps_model::GrantTarget::Routine(_) => routine.clone(),
                        schema @ pbps_model::GrantTarget::Schema(_) => schema.clone(),
                    };
                    (target, held.clone())
                })
                .collect();
            (
                name.clone(),
                pbps_model::Role {
                    description: role.description.clone(),
                    grants,
                },
            )
        })
        .collect();
    let after_roles: BTreeMap<_, _> = after
        .roles
        .iter()
        .map(|(name, role)| {
            (
                name.clone(),
                pbps_model::Role {
                    description: role.description.clone(),
                    grants: pbps_model::role::effective_grants(role, &after.tables),
                },
            )
        })
        .collect();
    let named_role = |n: &pbps_model::RoleName| roles.contains(n);
    compare(
        "role ",
        &before_roles,
        &after_roles,
        named_role,
        |_: &pbps_model::RoleName, was: &pbps_model::Role, now: &pbps_model::Role| was == now,
        &mut moved,
        |name| name.shown().to_string(),
    );
    // And the same for a role the plan does name: exempt down to the
    // permissions it moves, and no further. A plan that adds one grant is not
    // answerable for the rest of the role's set, and nothing else in the run
    // speaks for them — the statements grant and revoke what they were asked
    // to and say nothing about what they left alone (DECISIONS 156).
    // Created roles included, with no grants before them — for the reason the
    // tables above are: a loop over the baseline never visits a name that was
    // not there, so a grant that arrived on a role this plan had just created
    // was checked by nothing at all (DECISIONS 163). Its existence alone was.
    use pbps_model::plan_names::{At, Holder};
    let carried: Vec<_> = before_roles
        .iter()
        .filter_map(|(name, was)| {
            let now_name = match names.role_holder(name, At::Start) {
                Holder::Tracked(uid) => match names.role_at(uid, At::End) {
                    Some(to) => to,
                    // The retired principal's grants are not the incoming
                    // holder's baseline. An unrefilled name stays compared:
                    // staged membership removal can leave DROP ROLE pending.
                    None if matches!(names.role_holder(name, At::End), Holder::Tracked(_)) => {
                        return None;
                    }
                    None => name,
                },
                // A staged closing baseline may already use the final name.
                // Its observed grants are still held, not reset to empty.
                Holder::Vacant | Holder::Unmanaged => name,
            };
            Some((now_name, was))
        })
        .collect();
    // Keep the pairs, not a map keyed by their destination: two observed
    // baseline names must not silently overwrite one grant comparison.
    let carried_roles: BTreeSet<&pbps_model::RoleName> =
        carried.iter().map(|(name, _)| *name).collect();
    let no_grants = pbps_model::Role {
        description: None,
        grants: BTreeMap::new(),
    };
    let touched_roles = carried.iter().copied().chain(
        after_roles
            .keys()
            .filter(|name| named_role(name) && !carried_roles.contains(*name))
            .map(|name| (name, &no_grants)),
    );
    for (now_name, was) in touched_roles {
        if !roles.contains(now_name) {
            // Already compared whole, grants included.
            continue;
        }
        // Every pair already uses its destination. A new role under a vacated
        // source name must not follow the old holder's rename (DEC-1988.1).
        let Some(now) = after_roles.get(now_name) else {
            // Created or dropped: named by the change that does it, and there
            // is no pair of grant sets to compare.
            continue;
        };
        let empty = Permissions::new();
        // Every target either side holds permissions on, *and* every target
        // this plan names. A plan that adds the first permission a role has on
        // a target puts it in neither set when the grant is reversed before
        // the read — so the one thing being checked was the one thing the loop
        // never visited (DECISIONS 162).
        let targets: BTreeSet<_> = was
            .grants
            .keys()
            .chain(now.grants.keys())
            .chain(
                granting
                    .keys()
                    .chain(revoking.keys())
                    .filter(|(role, _)| role == now_name)
                    .map(|(_, target)| target),
            )
            .collect();
        for target in targets {
            // What the plan says the role will hold here: what it held, less
            // what this plan revokes, plus what it grants. Excusing the moved
            // permissions from both sides instead left the plan's own grant
            // checked by nothing — no statement has a postcondition on a
            // permission, so a session (or a DDL trigger) that reversed it
            // straight away was recorded as the plan's result (DECISIONS 160).
            let mut expected: Permissions = was.grants.get(target).cloned().unwrap_or_default();
            let mut held = now.grants.get(target).cloned().unwrap_or_default();
            let adds = granting
                .get(&(now_name.clone(), target.clone()))
                .unwrap_or(&empty);
            let removes = revoking
                .get(&(now_name.clone(), target.clone()))
                .unwrap_or(&empty);
            match settled {
                // Every statement has run, so what the plan asked for is part
                // of the answer.
                Settled::Whole | Settled::Closing => {
                    for revoked in removes {
                        expected.remove(revoked);
                    }
                    expected.extend(adds);
                }
                // Mid-run: the grant may simply not have happened yet, so only
                // the permissions this plan does not move can be compared.
                Settled::SoFar => {
                    expected.retain(|p| !adds.contains(p) && !removes.contains(p));
                    held.retain(|p| !adds.contains(p) && !removes.contains(p));
                }
            }
            if expected != held {
                // Which permission, both ways: a revoke that did not take and a
                // grant that was taken away read the same without it (#700).
                let listed = |set: &Permissions| {
                    set.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let mut detail = Vec::new();
                // A surplus the plan revoked is a revoke that did not take;
                // any other was neither held nor granted by the plan, so
                // another session put it there, and "still" would misdescribe
                // it (#1052).
                let (survived, arrived): (Permissions, Permissions) = held
                    .difference(&expected)
                    .cloned()
                    .partition(|p| removes.contains(p));
                let surplus = listed(&survived);
                if !surplus.is_empty() {
                    detail.push(format!("it still holds {surplus}"));
                }
                let unplanned = listed(&arrived);
                if !unplanned.is_empty() {
                    detail.push(format!("it unexpectedly holds {unplanned}"));
                }
                let missing = listed(&expected.difference(&held).cloned().collect());
                if !missing.is_empty() {
                    detail.push(format!("it lacks {missing}"));
                }
                moved.push(format!(
                    "role {now_name} does not hold on {target} what this plan leaves it holding: {}",
                    detail.join("; "),
                    now_name = now_name.shown(),
                    target = target.shown(),
                ));
            }
        }
    }
    if moved.is_empty() {
        return Ok(());
    }
    // The finding and the reason, and no remedy: what to do about it depends
    // on the caller. Inside a transaction nothing has been applied and the
    // plan can simply be run again; a staged run has committed, and whether
    // a `--resume` can go on depends on which read found the change. Naming
    // the transactional remedy here put "the transaction was rolled back"
    // into every staged refusal, one line above "nothing was rolled back"
    // (DECISIONS 190).
    bail!(
        "`{label}` moved while this plan was running, and not because of it:\n  {}\n\
         The state `apply` records is the database read back, so another session's change \
         committed during the run would have been written down as this plan's own result, \
         and every later `verify` would call it clean.",
        moved.join("\n  "),
        label = pbps_model::escape::Escaped(label),
    );
}

/// One kind of named thing on a table — its unique constraints, foreign keys,
/// checks or indexes — compared by definition over the names the plan leaves
/// alone.
///
/// A free function because it is generic over the value, which a closure
/// cannot be, and because comparing by name alone was the bug: a constraint
/// dropped and recreated under one name with a different body is present on
/// both sides (DECISIONS 167).
/// One namespace of the two states, compared over the names the plan leaves
/// alone. Generic over the key so tables, modules and roles get the same
/// answer from the same code: three near-identical loops is three places for
/// one of them to stop being written.
fn compare<K, V>(
    kind: &str,
    before: &BTreeMap<K, V>,
    after: &BTreeMap<K, V>,
    touched: impl Fn(&K) -> bool,
    // Equality, supplied because one namespace's is not plain `==`: a table's
    // constraints follow this plan's renames without a change of its own, and
    // in a staged run a checkpoint falls between the renames, so both
    // spellings have to be accepted. Modules and roles pass `==`.
    unchanged: impl Fn(&K, &V, &V) -> bool,
    moved: &mut Vec<String>,
    shown: impl Fn(&K) -> String,
) where
    K: Ord,
{
    for (name, was) in before {
        if touched(name) {
            continue;
        }
        let display = shown(name);
        match after.get(name) {
            Some(now) if unchanged(name, was, now) => {}
            Some(_) => moved.push(format!(
                "{kind}{display} is not what the plan was approved over"
            )),
            None => moved.push(format!(
                "{kind}{display} is gone, and no change of this plan drops it"
            )),
        }
    }
    for name in after.keys() {
        if touched(name) || before.contains_key(name) {
            continue;
        }
        let display = shown(name);
        moved.push(format!(
            "{kind}{display} is there, and no change of this plan creates it"
        ));
    }
}

/// No `From` conversion: every fallible verify step must choose its own ID,
/// so a successful connection cannot relabel a later read failure (DEC-1804.1).
#[derive(Debug)]
struct VerifyFailure {
    id: &'static str,
    error: anyhow::Error,
}

impl VerifyFailure {
    fn at(id: &'static str, error: impl Into<anyhow::Error>) -> Self {
        Self {
            id,
            error: error.into(),
        }
    }
}

fn recorded_verify_state(
    found: Result<Option<pbps_db::LedgerEntry>, pbps_db::LedgerError>,
    label: &str,
) -> Result<pbps_db::LedgerEntry, VerifyFailure> {
    match found {
        Ok(Some(entry)) => Ok(entry),
        Ok(None) => Err(VerifyFailure::at(
            "state.no-entries",
            anyhow::anyhow!(
                "`{label}` has a ledger but no entries; there is nothing to compare against.\n\
                 Record one with `pbps snapshot` or `pbps baseline --reason ...`.",
                label = pbps_model::escape::Escaped(label),
            ),
        )),
        Err(error) => {
            let id = match &error {
                pbps_db::LedgerError::NotInitialized => "state.uninitialized",
                pbps_db::LedgerError::BadEntry { .. }
                | pbps_db::LedgerError::Db(_)
                | pbps_db::LedgerError::Locked(_) => "state.unreadable",
            };
            Err(VerifyFailure::at(id, error))
        }
    }
}

/// `pbps verify` — the drift check (SPEC §8.2).
///
/// The comparison is scoped and identified by the **recorded** state's identity
/// file, not the working tree's. The question this command answers is "has this
/// environment moved since pbps last recorded it", and answering it with a
/// mapping the environment has never seen would report every uncommitted local
/// rename as drift in production.
pub fn cmd_verify(project: &Project, target: &Target, json: bool) -> anyhow::Result<()> {
    let catalog = std::cell::RefCell::new(Vec::new());
    let dialect = crate::output::or_unanswerable(
        "verify",
        json,
        "project.unsupported-dialect",
        crate::dialect(project),
    )?;
    let checked_at = crate::now();

    let rt = crate::output::or_unanswerable("verify", json, "runtime.unavailable", db::runtime())?;
    let (report, unmanaged_refusal, unmanaged_inventory) = match rt.block_on(async {
        let mut conn = db::connect(target)
            .await
            .map_err(|error| VerifyFailure::at("environment.unreachable", error))?;
        let baseline =
            recorded_verify_state(crate::engine::latest(&mut conn).await, &target.label)?;

        let recorded_ids = baseline.snapshot.ids.clone();
        let recorded_modules = managed_modules(Some(&baseline.snapshot), None);
        // The rows, like the modules, under the *recorded* scope: the question
        // is whether this environment moved since pbps last recorded it, and a
        // table whose rows were never recorded has nothing to have moved from.
        let recorded_scopes = baseline.snapshot.schema.data_scopes();
        let managed = managed_state_full(
            &mut conn,
            &recorded_ids,
            &recorded_modules,
            &pbps_diff::external_parents(&baseline.snapshot.schema),
            // The unmanaged policy is applied below instead, beside the
            // finished report, so that neither verdict can erase the other.
            target.catalog_policy(pbps_config::Unmanaged::Ignore),
            &rows_to_read(&recorded_scopes),
            crate::engine::Read::Snapshot,
            CatalogWarnings::for_output(json, &catalog),
        )
        .await
        .map_err(|error| VerifyFailure::at("environment.unreadable", error))?;
        let mut scoped = managed.scoped;
        scoped.schema = scoped
            .schema
            .with_observed_rows(&managed.rows, &recorded_scopes, &baseline.snapshot.schema)
            .map_err(|error| VerifyFailure::at("verify.failed", error))?;

        // The live side is identified by what is actually there, not by the
        // recorded mapping. Comparing two sides that share one identity file
        // can only surface attribute changes on objects present in both — so a
        // hand-added or hand-dropped column, the two things a drift check most
        // needs to catch, would be exactly what it missed.
        let observed = pbps_diff::observed_ids(&scoped.schema, &recorded_ids);

        // `diff_partial`, not `diff`: a drift report wants both halves. The
        // `Result` form returns only the errors, and taking that branch meant
        // one table's altered `IDENTITY` silently deleted every expressible
        // difference the same comparison had found — an undercount in the
        // report, in the finding count, and in the hook's payload.
        // Both sides are reads, which know the sequence names they leave
        // out; compared unnamed, a renamed sequence would take the other
        // side's name and read as no change (#1631). The checksums below are
        // the reads' own.
        let mut recorded_read = baseline.snapshot.schema.clone();
        pbps_dialect::name_read_sequences(&mut recorded_read, dialect.as_ref());
        let mut live_read = scoped.schema.clone();
        pbps_dialect::name_read_sequences(&mut live_read, dialect.as_ref());
        only_recorded_search_objects(&recorded_read, &mut live_read);
        without_comments(&mut recorded_read, &mut live_read);
        let diffed = pbps_diff::diff_partial(
            pbps_diff::Side {
                schema: &recorded_read,
                ids: &recorded_ids,
            },
            pbps_diff::Side {
                schema: &live_read,
                ids: &observed,
            },
            dialect.as_ref(),
            // Drift emits no SQL, so there is no execution to give a hint
            // about; passing the declarations' strategies here would put a
            // hint nobody can act on into a report about what already happened.
            &pbps_model::Hints::default(),
        );
        // Carried *in* the report, not raised as a separate outcome. The
        // database was reached and a difference was established — the differ
        // simply has no `Change` for it — so this is drift, and everything
        // downstream (findings, the envelope, the `on_drift` hook, exit 2)
        // must treat it as such. An earlier fix made it exit 2 but returned
        // early, which skipped the hook: right verdict, and the alert that
        // exists to carry that verdict never fired.
        let changes = diffed.changes;
        let mut unexpressible: Vec<String> =
            diffed.errors.iter().map(ToString::to_string).collect();
        unexpressible.extend(managed.limitations);
        // A managed role's permission the declarations cannot spell is the
        // same kind of difference as a catalog fact they cannot: established,
        // and with no `Change` to carry it (DECISIONS 95).
        unexpressible.extend(scoped.unexpressible.iter().cloned());
        // Unmanaged objects are outside the drift scope by definition. Keep
        // their policy verdict beside the completed drift report: neither can
        // erase the other when both are present. `report_unmanaged` currently
        // has only this one typed failure, but preserve any future operational
        // error as an inability to answer rather than misclassifying it as a
        // policy finding.
        let unmanaged_inventory = unmanaged_objects(
            &scoped,
            &managed.unreadable,
            &recorded_ids,
            &recorded_modules,
        );
        let unmanaged_refusal =
            match apply_unmanaged_policy(&unmanaged_inventory, project.config.unmanaged) {
                Ok(()) => None,
                Err(error) => match error.downcast::<UnmanagedPolicy>() {
                    Ok(policy) => Some(policy),
                    Err(error) => return Err(VerifyFailure::at("verify.failed", error)),
                },
            };

        Ok((
            pbps_model::DriftReport {
                version: pbps_model::drift::CURRENT_VERSION,
                environment: target.label.clone(),
                checked_at,
                baseline: pbps_model::DriftBaseline {
                    entry_id: baseline.id,
                    applied_at: baseline.applied_at.clone(),
                    checksum: pbps_model::drift_checksum(&baseline.snapshot.schema, &recorded_ids),
                },
                // The checksum compares like with like: both sides fingerprinted
                // with the recorded mapping, so a derived identity for a hand-added
                // column cannot by itself make the two differ.
                live_checksum: pbps_model::drift_checksum(&scoped.schema, &recorded_ids),
                changes,
                unmanaged: scoped.unmanaged,
                unexpressible,
            },
            unmanaged_refusal,
            unmanaged_inventory,
        ))
    }) {
        Ok(outcome) => outcome,
        Err(VerifyFailure { id, error }) => {
            // Unanswerable: `verify` was asked whether this database still
            // matches its recorded state, and it could not look. Without
            // this the JSON path returned before its own branch, leaving
            // stdout empty — so a consumer got the converter's generic
            // "produced no output" instead of a report naming the target
            // (SPEC §9.8).
            if json {
                let mut findings = vec![crate::output::Finding::error(
                    id,
                    format!("{}: {error:#}", pbps_model::escape::Escaped(&target.label)),
                )];
                findings.extend(catalog.take());
                crate::output::unanswerable("verify", findings);
            }
            return Err(error);
        }
    };

    // The hook always receives the bare report, whatever the human asked for on
    // stdout: a script's payload should not change shape because someone added
    // a flag for their own eyes, and it must not gain an envelope because
    // stdout did.
    let payload = format!("{}\n", serde_json::to_string_pretty(&report)?);

    let mut findings = catalog.into_inner();
    if report.has_drift() {
        findings.push(
            crate::output::Finding::error(
                "state.drift",
                format!(
                    // Both counted: a difference the differ cannot phrase is
                    // still a difference, and "0 difference(s)" beside a
                    // drift verdict reads as a bug in the tool rather than a
                    // fact about the database.
                    "`{}` no longer matches its recorded state ({} difference(s))",
                    pbps_model::escape::Escaped(&target.label),
                    report.changes.changes.len() + report.unexpressible.len()
                ),
            )
            .remedy(crate::report::DRIFT_REMEDY),
        );
    }
    // One finding per difference, because the differ returns one per
    // unexpressible change and the column name in it is the remedy.
    for e in &report.unexpressible {
        findings.push(
            crate::output::Finding::error(
                "state.drift-unexpressible",
                format!("{}: {e}", pbps_model::escape::Escaped(&target.label)),
            )
            .remedy(crate::report::DRIFT_REMEDY),
        );
    }
    // The typed report predates managed modules and carries tables only. JSON
    // findings use the complete policy inventory so readable and unreadable
    // modules cannot disappear from the machine view while stderr names them.
    for object in &unmanaged_inventory {
        findings.push(crate::output::Finding::note(
            "state.unmanaged",
            format!("{object} is in the database and outside the managed set; it was not compared"),
        ));
    }
    if let Some(ref refusal) = unmanaged_refusal {
        findings.push(
            crate::output::Finding::error(
                "state.unmanaged-refused",
                format!("{}: {refusal}", pbps_model::escape::Escaped(&target.label)),
            )
            .remedy("pbps pull, or relax `unmanaged:` in pbps.yml"),
        );
    }

    if json {
        // Preserve the established policy-only shape (`data` absent), while a
        // real drift still carries its complete report even when the unmanaged
        // policy also failed.
        let data = if unmanaged_refusal.is_some() && !report.has_drift() {
            None
        } else {
            Some(&report)
        };
        // The envelope, not the bare report: a consumer reading `verify` beside
        // `validate` should not need a second parser for one of them (SPEC
        // §14.1). The report itself is unchanged, one level down in `data`.
        println!(
            "{}",
            serde_json::to_string_pretty(&crate::output::Report::new("verify", findings, data))?
        );
    } else {
        if unmanaged_refusal.is_none() || report.has_drift() {
            print!("{}", crate::report::drift(&report));
        }
        if let Some(ref refusal) = unmanaged_refusal {
            eprintln!("{refusal}");
        }
    }

    if report.has_drift() {
        if let Some(hook) = &project.config.hooks.on_drift {
            // After the envelope under `--format json`: the hook's own output
            // must not become trailing bytes of that document (#493).
            let stdout = if json {
                crate::hooks::HookStdout::Stderr
            } else {
                crate::hooks::HookStdout::Shared
            };
            crate::hooks::run(hook, &payload, "on_drift", stdout);
        }
        // A distinct exit code so a scheduled pipeline can tell "the database moved"
        // from "the tool could not run" — the two need different people woken up.
        return Err(crate::Found::reported().into());
    }
    if unmanaged_refusal.is_some() {
        // The project's own policy was refused on a database that was read
        // successfully. `verify` answered — exit 2 and the schema owner's, not
        // exit 1 and CI's.
        return Err(crate::Found::reported().into());
    }
    Ok(())
}

/// `pbps snapshot` — record the current state, refusing to bless a difference.
pub fn cmd_snapshot(project: &Project, target: &Target, force: bool) -> anyhow::Result<()> {
    let operator = crate::operator(project.root())?;
    let ids = crate::read_ids(project)?;
    let (declared_modules, declared_data, loaded) =
        declared_scope(project, crate::dialect(project)?.as_ref())?;

    db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;
        crate::engine::lock(&mut conn, &operator).await?;
        let result = async {
            let ids = crate::engine::installation_ids(&mut conn, &ids).await?;
            let scoped = managed_state(
                &mut conn,
                &ids,
                &declared_modules,
                &pbps_diff::external_parents(&loaded.schema),
                target.catalog_policy(project.config.unmanaged),
                &declared_data,
                &loaded.schema,
                crate::engine::Read::Snapshot,
                CatalogWarnings::Human,
            )
            .await?;
            report_missing(&scoped);
            refuse_unexpressible(&scoped, &target.label, "snapshot again")?;

            // Comparing against the recorded state is the whole guard. A snapshot
            // that overwrites a state it differs from is exactly "somebody SSHed in
            // and changed the schema" being quietly adopted by a pipeline.
            match crate::engine::latest(&mut conn).await {
                Ok(Some(previous)) if !previous.snapshot.matches(&scoped.schema) && !force => {
                    bail!(
                        "`{}` differs from the state recorded at {} (entry #{}).\n\
                         `pbps verify` shows what changed. Then either put the database back and \
                         re-run, or accept it with `pbps baseline --reason ...`.\n\
                         `--force` records it anyway.",
                        pbps_model::escape::Escaped(&target.label),
                        previous.applied_at,
                        previous.id
                    );
                }
                Ok(Some(_)) => {}
                // Taking the lock initializes the tables, so a first snapshot
                // is now represented by an empty ledger rather than
                // `NotInitialized`. It remains an adoption decision.
                Ok(None) if !force => bail!(
                    "pbps has never recorded a state for `{}`.\n\
                     Adopt it deliberately with `pbps baseline --reason ...`, or `--force` to \
                     record it as it stands.",
                    pbps_model::escape::Escaped(&target.label)
                ),
                Ok(None) => {}
                Err(e) => return Err(e.into()),
            }

            // What the database holds, not every uid the ids file names: a
            // missing table or role recorded as held is one the next plan
            // never creates (#1600, #1606).
            crate::engine::prove_tables_absent(&mut conn, &scoped.missing).await?;
            let gone = crate::engine::absent_roles(&mut conn, &scoped.missing_roles).await?;
            let held = ids
                .without(&scoped.missing)
                .without_roles(&gone)
                .without_types(&scoped.missing_types);
            let mut snapshot = with_provenance(
                project.root(),
                StateSnapshot::new(StateKind::Apply, scoped.schema, held, &operator),
            );
            snapshot.baseline_evidence = scoped.baseline_evidence;
            snapshot.module_deps = loaded.hints.module_deps.clone();
            let tables = snapshot.schema.tables.len();
            let id = crate::engine::record(&mut conn, &snapshot).await?;
            Ok::<_, anyhow::Error>((id, tables))
        }
        .await;
        let released = crate::engine::unlock(&mut conn).await;
        let (id, tables) = match result {
            Ok(recorded) => recorded,
            Err(error) => {
                warn_unreleased(&target.label, &released);
                return Err(error);
            }
        };
        println!(
            "Recorded the state of `{}` as entry #{id} ({} table(s)).",
            pbps_model::escape::Escaped(&target.label),
            tables
        );
        // The ledger row is durable even when deleting the lock row fails.
        // Report that before surfacing cleanup so retrying cannot look safe.
        released?;
        Ok(())
    })
}

/// The table uids a drop intent in the working identity file names (#1852):
/// a tombstone's uid is the table's identity whatever it was renamed to
/// before the drop, so it is matched against the recorded state by uid.
fn tables_a_drop_intent_names(ids: &IdsFile) -> Vec<&pbps_model::Uid> {
    ids.tombstones
        .keys()
        .filter(|uid| uid.kind() == pbps_model::UidKind::Table)
        .collect()
}

/// Refuses a baseline that would hand over a held partition with pending drop
/// intent (DEC-1852.1). Uids still decide what is held. ADR-0018's opaque
/// evidence can narrow that refusal to one live partition, or prove its drop;
/// missing/mismatched evidence retains the parent-and-bound refusal below.
/// An unreadable ledger cannot establish what is held and refuses the handover.
fn refuse_handing_over_held_partitions(
    dropped: &[&pbps_model::Uid],
    working: &IdsFile,
    scoped: &pbps_diff::Scoped,
    previous: Result<Option<pbps_db::LedgerEntry>, pbps_db::LedgerError>,
    label: &str,
    catalog: Option<&pbps_model::baseline_evidence::BaselineCatalog>,
) -> anyhow::Result<()> {
    let lines: Vec<String> = match &previous {
        Ok(None) => return Ok(()),
        Ok(Some(previous)) => {
            let recorded = &previous.snapshot;
            dropped
                .iter()
                .filter_map(|uid| {
                    let name = recorded.ids.tables.get(*uid)?;
                    let of = recorded.schema.tables.get(name)?.partition_of.as_ref()?;
                    let parent = recorded
                        .ids
                        .tables
                        .iter()
                        .find(|(_, held)| **held == of.parent)
                        .map(|(parent, _)| parent)?;
                    let parent = working.tables.get(parent)?;
                    let siblings: Vec<(&TableName, &pbps_model::PartitionOf)> = scoped
                        .external
                        .iter()
                        .filter(|(_, live)| live.parent == *parent)
                        .collect();
                    if siblings.is_empty() {
                        return None;
                    }
                    let entry = previous.id;
                    if let Some(catalog) = catalog {
                        use pbps_model::baseline_evidence::HeldPartition;
                        match catalog.held_partition(
                            recorded.baseline_evidence.as_ref(),
                            name,
                            &of.parent,
                            parent,
                        ) {
                            HeldPartition::Dropped => return None,
                            HeldPartition::Present(live) => {
                                if !scoped.external.contains_key(&live) {
                                    return None;
                                }
                                let was = if name == &live {
                                    String::new()
                                } else {
                                    format!(", recorded as {}", name.shown())
                                };
                                return Some(format!(
                                    "{} is named by a drop intent and held by the recorded state \
                                     (entry #{entry}{was}); this baseline would leave it to its \
                                     external parent undropped",
                                    live.shown()
                                ));
                            }
                            HeldPartition::ParentRefusal => {}
                        }
                    }
                    Some(
                        match siblings.iter().find(|(_, live)| live.bound == of.bound) {
                            Some((live, _)) => {
                                let was = if name == *live {
                                    String::new()
                                } else {
                                    format!(", recorded as {}", name.shown())
                                };
                                format!(
                                    "{} is named by a drop intent and held by the recorded state \
                                     (entry #{entry}{was}); this baseline would leave it to its \
                                     external parent undropped",
                                    live.shown()
                                )
                            }
                            None => format!(
                                "{} is named by a drop intent and held by the recorded state \
                                 (entry #{entry}) as a partition of {}, whose partitions this \
                                 baseline would leave external ({}); none has its bound, and \
                                 which of them it is now cannot be told",
                                name.shown(),
                                parent.shown(),
                                siblings
                                    .iter()
                                    .map(|(live, _)| live.shown().to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        },
                    )
                })
                .collect()
        }
        Err(e) => scoped
            .external
            .keys()
            .map(|name| {
                format!(
                    "{} is left to its external parent, and whether a drop intent names a table \
                     the recorded state holds there cannot be read ({e})",
                    name.shown()
                )
            })
            .collect(),
    };
    if lines.is_empty() {
        return Ok(());
    }
    bail!(
        "`{label}` holds {} partition(s) this baseline would hand over:\n  {}\n\
         A drop intent leaves a partition out of the identity file before it is dropped. \
         Finish the pending drop first with `pbps plan` then `pbps apply --plan <plan>` \
         (keeping the parent's partitions declared), then baseline; if it was dropped by hand, \
         revert the drop intent with `git revert <drop-intent-commit>` so the baseline records \
         it missing (DEC-1852.1).",
        lines.len(),
        lines.join("\n  "),
        label = pbps_model::escape::Escaped(label),
    );
}

/// `pbps baseline` — take the database as it stands as the new starting point.
pub fn cmd_baseline(
    project: &Project,
    target: &Target,
    reason: &pbps_model::Reason,
) -> anyhow::Result<()> {
    let operator = crate::operator(project.root())?;
    let ids = crate::read_ids(project)?;
    let dialect = crate::dialect(project)?;
    let (declared_modules, declared_data, loaded) = declared_scope(project, dialect.as_ref())?;

    db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;
        crate::engine::lock(&mut conn, &operator).await?;
        let result = async {
            let ids = crate::engine::installation_ids(&mut conn, &ids).await?;
            // A staged attach's temporary CHECK is taken out in the same
            // transaction as the read and the record. Dropped on its own, a
            // baseline that then refused left the database without it while
            // the newest staged checkpoint still recorded it, and `apply
            // --resume` refused that drift (#2127). Without one to remove,
            // the read is the snapshot it always was.
            let checks = crate::engine::standing_scan_checks(&mut conn, &ids).await?;
            let read = if checks.is_empty() {
                crate::engine::Read::Snapshot
            } else {
                conn.begin(&dialect.transaction_framing(&pin_zone(&conn)))
                    .await
                    .context("cannot open a transaction")?;
                crate::engine::Read::InsideOwnTransaction
            };
            let body = async {
                let removed = crate::engine::drop_scan_checks(&mut conn, &checks).await?;
                let scoped = managed_state(
                    &mut conn,
                    &ids,
                    &declared_modules,
                    &pbps_diff::external_parents(&loaded.schema),
                    target.catalog_policy(project.config.unmanaged),
                    &declared_data,
                    &loaded.schema,
                    read,
                    CatalogWarnings::Human,
                )
                .await?;
                // Adoption certifies only the selected exact packages, never
                // a different installed version or an unheld discovery (DEC-2447.3).
                refuse_extension_prerequisite_drift(
                    &scoped.schema,
                    &loaded.schema,
                    &ids,
                    dialect.as_ref(),
                    "baseline",
                )?;
                report_missing(&scoped);
                refuse_unexpressible(&scoped, &target.label, "baseline again")?;
                // A partition a drop intent names is out of the working identity
                // file before it is dropped, and the scope then reads it as one of
                // its external parent's (#1852). Asked only while an intent names a
                // table and a partition is left external, so a baseline that is the
                // remedy for an unreadable ledger, or a deliberate handover with no
                // intent, never reads the ledger here.
                let dropped = tables_a_drop_intent_names(&ids);
                if !dropped.is_empty() && !scoped.external.is_empty() {
                    let previous = crate::engine::latest(&mut conn).await;
                    let catalog = crate::engine::baseline_catalog(&mut conn).await?;
                    refuse_handing_over_held_partitions(
                        &dropped,
                        &ids,
                        &scoped,
                        previous,
                        &target.label,
                        catalog.as_ref(),
                    )?;
                }

                // What the database holds, as `snapshot` records it (#1600, #1606).
                crate::engine::prove_tables_absent(&mut conn, &scoped.missing).await?;
                let gone = crate::engine::absent_roles(&mut conn, &scoped.missing_roles).await?;
                let held = ids
                    .without(&scoped.missing)
                    .without_roles(&gone)
                    .without_types(&scoped.missing_types);
                let mut snapshot = with_provenance(
                    project.root(),
                    StateSnapshot::new(StateKind::Baseline, scoped.schema, held, &operator),
                );
                snapshot.baseline_evidence = scoped.baseline_evidence;
                snapshot.module_deps = loaded.hints.module_deps.clone();
                // The note on what was removed rides on the reason only where
                // the bounded column has room for both: it never turns a reason
                // that fits into a record that fails. The removal is printed
                // either way, once it has committed.
                let noted = format!("{reason} (first removed {})", removed.join(", "));
                snapshot.reason = Some(
                    if removed.is_empty()
                        || crate::engine::truncate_reason(conn.driver(), &noted) != noted
                    {
                        reason.to_string()
                    } else {
                        noted
                    }
                    .into(),
                );

                let tables = snapshot.schema.tables.len();
                let id = crate::engine::record(&mut conn, &snapshot).await?;
                Ok::<_, anyhow::Error>((id, tables, removed))
            }
            .await;
            let (id, tables, removed) = if checks.is_empty() {
                body
            } else {
                finish_transaction(&mut conn, dialect.as_ref(), body).await
            }?;
            for said in &removed {
                println!("Removed {}.", pbps_model::escape::Escaped(said));
            }
            Ok::<_, anyhow::Error>((id, tables))
        }
        .await;
        let released = crate::engine::unlock(&mut conn).await;
        let (id, tables) = match result {
            Ok(recorded) => recorded,
            Err(error) => {
                warn_unreleased(&target.label, &released);
                return Err(error);
            }
        };
        println!(
            "Baselined `{}` as entry #{id}: {} table(s) are now the starting point.",
            pbps_model::escape::Escaped(&target.label),
            tables
        );
        println!("Reason recorded: {reason}");
        // The baseline was recorded even if cleanup now reports an error.
        released?;
        Ok(())
    })
}

/// `pbps bootstrap` — build the whole schema from the declarations.
///
/// Two halves that are useful separately: `--sql` writes the CREATE script for a
/// DR runbook or an air-gapped host, and a target executes it. Neither implies
/// the other — the script is worth having on a machine that cannot reach the
/// database at all.
/// Refuses declarations `validate` would reject, before a single statement is
/// written for a real database (DECISIONS 141).
///
/// The commands below hand SQL to a database that is not a rehearsal. A
/// declaration the dialect refuses — `execute` granted on a table, say — is
/// not a plan that fails to convert; it is a plan whose *last* statements
/// fail, and in staged mode the ones before them have already committed.
///
/// With `json`, the refusal is the findings `validate` reports for the same
/// declarations, as one findings document: a refusal the operator can act on
/// is not an operational failure, and `plan.failed` would hide which
/// declaration to fix (#573).
fn refuse_invalid_declarations(
    loaded: &pbps_load::Loaded,
    dialect: &dyn Dialect,
    json: bool,
) -> anyhow::Result<()> {
    let problems = crate::declaration_problems(loaded, dialect);
    if problems.is_empty() {
        return Ok(());
    }
    if json {
        return crate::output::Report::plain(
            "plan",
            problems
                .into_iter()
                .map(|(id, problem)| crate::output::Finding::error(id, problem))
                .collect(),
        )
        .emit_json();
    }
    bail!(
        "the declarations have {} problem(s) that would reach the database:\n  {}\n\
         `pbps validate` reports these with their source lines.",
        problems.len(),
        problems
            .iter()
            .map(|(_, problem)| problem.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

// Immediate bootstrap has no reviewed source artifact, including before its
// spelling probes. Recheck before execution too (DEC-2181.2).
async fn bootstrap_type_pins(
    conn: &mut Conn,
    schema: &Schema,
    changes: &pbps_model::ChangeSet,
    project: &Project,
) -> anyhow::Result<crate::engine::Prerequisites> {
    let pins = crate::engine::seal_type_pins(
        conn,
        &[schema],
        changes,
        &project.config.trusted_roles,
        project.config.pin_type_routines,
    )
    .await?;
    if !crate::engine::type_input_routines(
        conn,
        &pins.types,
        &project.config.trusted_roles,
        project.config.pin_type_routines,
        false,
    )
    .await?
    .is_empty()
    {
        bail!(
            "immediate bootstrap cannot run type routines without a reviewed saved plan; baseline an empty target, then plan, review and apply (DEC-2181.2)"
        );
    }
    Ok(pins)
}

/// A missing uid makes the differ skip an object that the declaration record
/// still claims. Modules use names rather than uids (ADR-0009 §1).
fn refuse_unidentified_bootstrap_objects(schema: &Schema, ids: &IdsFile) -> anyhow::Result<()> {
    // No `..`: a new object family must choose its identity rule here.
    let Schema {
        tables,
        types,
        // Text search objects take identities with the next part (#2728).
        search: _,
        modules: _,
        roles,
        extensions,
    } = schema;
    let unidentified: Vec<String> = extensions
        .keys()
        .filter(|name| ids.extension_uid(name).is_none())
        .map(|name| format!("extension {}", name.shown()))
        .chain(
            tables
                .keys()
                .filter(|name| ids.table_uid(name).is_none())
                .map(|name| name.shown().to_string()),
        )
        .chain(
            roles
                .keys()
                .filter(|name| ids.role_uid(name).is_none())
                .map(|name| format!("role {}", name.shown())),
        )
        .chain(
            types
                .keys()
                .filter(|name| {
                    matches!(
                        TypeResolver::new(ids, ids).resolve(TypeSide::Desired, name),
                        TypeIdentity::ExternalOrUnrecorded(_)
                    )
                })
                .map(|name| format!("type {}", name.shown())),
        )
        .collect();
    if !unidentified.is_empty() {
        bail!(
            "the identity file does not know {}: {}.\n\
             Run `pbps plan` first to mint identities for the declarations.",
            if unidentified.len() == 1 {
                "a declared object"
            } else {
                "these declared objects"
            },
            unidentified.join(", ")
        );
    }
    Ok(())
}

/// Refuse before attempting a transaction: only a proven empty managed set
/// justifies the empty snapshot recorded after a rolled-back bootstrap.
fn refuse_nonempty_bootstrap(
    schema: &Schema,
    limitations: &[String],
    dialect: &dyn Dialect,
    label: &str,
) -> anyhow::Result<()> {
    // Derive emptiness from the diagnostic inventory so the two cannot differ.
    let Schema {
        tables,
        types,
        search,
        modules,
        roles,
        extensions,
    } = schema;
    let names: Vec<String> = tables
        .keys()
        .map(|name| name.shown().to_string())
        .chain(search.keys().map(|name| name.shown().to_string()))
        .chain(modules.keys().map(|id| id.shown().to_string()))
        // PostgreSQL cluster roles with no managed grants are prerequisites,
        // rather than objects bootstrap creates (DECISIONS 211).
        .chain(
            roles
                .iter()
                .filter(|(_, role)| dialect.manages_roles() || !role.grants.is_empty())
                .map(|(name, _)| format!("role {}", name.shown())),
        )
        .chain(types.keys().map(|name| format!("type {}", name.shown())))
        .chain(
            extensions
                .keys()
                // Exact PostgreSQL packages are separately checked prerequisites,
                // like standing cluster roles, not objects we create (DEC-2447.2).
                .filter(|_| dialect.name() != "postgres")
                .map(|name| format!("extension {}", name.shown())),
        )
        // Omitted definitions still occupy names in the managed set.
        .chain(limitations.iter().cloned())
        .collect();
    if !names.is_empty() {
        bail!(
            "`{label}` already has {} of the declared object(s): {}.\n\
             Bootstrap builds into an empty database; use `pbps plan --db` and `pbps \
             apply` to migrate it instead.",
            names.len(),
            names.join(", "),
            label = pbps_model::escape::Escaped(label),
        );
    }
    Ok(())
}

/// Bootstrap may adopt packages it verifies, but cannot install or change them.
fn refuse_extension_prerequisite_drift(
    installed: &Schema,
    desired: &Schema,
    ids: &IdsFile,
    dialect: &dyn Dialect,
    operation: &str,
) -> anyhow::Result<()> {
    for name in desired.extensions.keys() {
        anyhow::ensure!(
            ids.extension_uid(name).is_some(),
            "extension `{}` has no identity; select it with `pbps pull --extension` before {operation}",
            name.shown()
        );
    }
    let errors = pbps_diff::prerequisite_differences(
        pbps_diff::Side {
            schema: installed,
            ids,
        },
        pbps_diff::Side {
            schema: desired,
            ids,
        },
        dialect.name(),
    );
    if !errors.is_empty() {
        bail!(
            "{operation} extension prerequisites differ:\n{}",
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    Ok(())
}

/// Keep the read-back and authored definitions together: the former proves
/// drift, while the latter preserves expressions the catalog respells.
fn bootstrap_record(
    built: pbps_diff::Scoped,
    held: IdsFile,
    loaded: &pbps_load::Loaded,
    operator: &str,
) -> StateSnapshot {
    let mut snapshot = StateSnapshot::new(StateKind::Bootstrap, built.schema, held, operator);
    snapshot.baseline_evidence = built.baseline_evidence;
    snapshot.module_deps = loaded.hints.module_deps.clone();
    // Every object was created from these declarations (ADR-0009 §2.2).
    snapshot.declared = pbps_model::Declared::from_schema(&loaded.schema);
    snapshot
}

fn bootstrap(
    project: &Project,
    target: Option<&Target>,
    sql_out: Option<&std::path::Path>,
    declarations: services::Declarations,
    dialect: crate::emission::SelectedDialect,
    operator: &str,
) -> anyhow::Result<Option<services::Bootstrapped>> {
    match db::runtime()?.block_on(bootstrap_async(
        project,
        target,
        sql_out,
        declarations,
        dialect,
        operator,
        CatalogWarnings::Human,
    ))? {
        services::Bootstrap::Script => Ok(None),
        services::Bootstrap::Built(built) => Ok(Some(built)),
        services::Bootstrap::Failed { error, released } => {
            // The command's own error is returned; the owned test run reports
            // the same outcome as a finding instead of this warning.
            if let Some(target) = target {
                warn_unreleased(&target.label, &released);
            }
            Err(error)
        }
    }
}

async fn bootstrap_async(
    project: &Project,
    target: Option<&Target>,
    sql_out: Option<&std::path::Path>,
    declarations: services::Declarations,
    dialect: crate::emission::SelectedDialect,
    operator: &str,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<services::Bootstrap> {
    pbps_cli::with_plan_dialect!(dialect, |mut dialect| {
        let services::Declarations { mut loaded, ids } = declarations;
        refuse_invalid_declarations(&loaded, dialect.as_ref(), false)?;
        refuse_unidentified_bootstrap_objects(&loaded.schema, &ids)?;

        // A connected generation proves nothing about a later replay target.
        // Keep the input-path proof in connected execution (DEC-2181.1).
        if sql_out.is_some() && (
            crate::engine::type_references(&loaded.schema).iter().any(|r| !loaded.schema.types.contains_key(r))
            || loaded.schema.types.values().any(|t| matches!(t, pbps_model::ManagedType::Domain { default, checks, .. } if default.is_some() || !checks.is_empty()))
        ) {
            bail!(
                "bootstrap --sql cannot validate referenced type input paths for its replay target; run connected bootstrap without --sql, or use plan/apply against the target (DEC-2181.1)"
            );
        }

        // Every declared text, as the engine reads it, before anything is built
        // from them: a spelling it would read back differently would be recorded
        // as the engine spells it and drift from the declaration on the next plan
        // (DECISIONS 101), and a date or time it reads one way only is built,
        // scripted and recorded as that reading (DEC-1883.1) — so this is asked
        // ahead of the diff, not inside the build where it used to be, since the
        // statements and the `--sql` script are made from what it answers. Into
        // an empty database, so nothing is under an older name: the declared
        // names are the only ones the catalog could have. Not under the lock: the
        // engine's reading of a text does not turn on what the database holds.
        // Without a target nothing can be asked, and the script carries the
        // declared text, as it always has.
        if let Some(target) = target {
            loaded.schema = Box::pin(async {
                let mut conn = db::connect(target).await?;
                dialect = crate::engine::planning_dialect_for(&mut conn, &dialect).await?;
                bootstrap_type_pins(&mut conn, &loaded.schema, &Default::default(), project)
                    .await?;
                let mut unasked = Vec::new();
                let refused = refuse_misspelt(
                    &mut conn,
                    &loaded.schema,
                    &Default::default(),
                    &mut unasked,
                    false,
                )
                .await;
                // Printed before the refusal, beside it, as the plan prints them.
                warn_unasked(&unasked);
                refused
            })
            .await?;
        }
        for notice in crate::row_security::binding_notices(&loaded.schema) {
            eprintln!("warning: {notice}");
        }
        let declared_modules = managed_modules(None, Some(&loaded.schema));

        // Every ordinary object is created; exact preinstalled packages are
        // prerequisites, checked under the lock and at read-back (DEC-2447.2).
        let (prerequisites, prerequisite_ids) = pbps_diff::bootstrap_base(pbps_diff::Side {
            schema: &loaded.schema,
            ids: &ids,
        });
        let cs = pbps_diff::diff(
            pbps_diff::Side {
                schema: &prerequisites,
                ids: &prerequisite_ids,
            },
            pbps_diff::Side {
                schema: &loaded.schema,
                ids: &ids,
            },
            dialect.as_ref(),
            // Bootstrap builds into an empty database, so there are no rows for an
            // online operation to spare and the strategies are dropped. The module
            // dependencies are not: `depends_on:` exists for the edges the
            // identifier scan cannot see, and discarding them here would make
            // bootstrap emit a dependent module first and fail on declarations that
            // plan perfectly well.
            &pbps_model::Hints {
                row_guard_defaults: Default::default(),
                retained_expressions: Vec::new(),
                enum_value_renames: loaded.hints.enum_value_renames.clone(),
                enum_comments: Default::default(),
                domain_array_holders: Default::default(),
                enum_dependents: Default::default(),
                // Nothing is created here: the packages are the prerequisites.
                extension_requires: Default::default(),
                extension_drops: Default::default(),
                extension_updates: Default::default(),
                extension_moves: Default::default(),
                strategies: Default::default(),
                module_deps: loaded.hints.module_deps.clone(),
                // Carried for the same reason, and one more: it is what decides
                // whether the routines this bootstrap creates arrive executable
                // by every principal in the cluster (ADR-0010 §5). Dropping it
                // here would make the first apply into an empty database the one
                // apply that leaves the engine default standing.
                public_execute: loaded.hints.public_execute.clone(),
            },
        )
        .map_err(|errs| {
            let errors = crate::Unexpressible(errs);
            errors.print_human();
            errors
        })?;
        // What a new table calls goes ahead of it, by the same reordering passes
        // `plan --db` runs (#1585, DEC-942.1, DEC-1364.1): a default or generated
        // column calling a declared function. Into an empty database there is no
        // catalog to read, so the facts are none: nothing is dropped or rebuilt,
        // no column is released, and `weave` has nothing to add. The passes that
        // read nothing run, so the `--sql` script gets them too (#1955).
        let cs = crate::dependents::schedule::schedule(
            cs,
            &crate::dependents::schedule::Declarations {
                schema: &loaded.schema,
                sources: crate::dependents::Sources {
                    before: pbps_diff::Side {
                        schema: &Schema::default(),
                        ids: &IdsFile::default(),
                    },
                    after: &ids,
                },
                module_deps: &loaded.hints.module_deps,
            },
            &crate::dependents::schedule::Facts::default(),
            dialect.as_ref(),
        )?
        .changes;
        // And the check `plan --db` runs on that final order (DEC-1576.1): an
        // expression naming, in a literal, a relation created only after it is
        // refused with the two-plan remedy, before any script or DDL, rather
        // than rolled back by the engine. A qualified name is refused offline.
        // An unqualified one may be found ahead on its path, and which name an
        // unnamed key's index takes depends on what holds its candidates now
        // (#1619, #1645), which only the target can say: asked of it with
        // `--db`, before any script is written, and refused without one. One
        // refusal names them all.
        let mut refused = Vec::new();
        if dialect.name() == "postgres" {
            let offline = crate::dependents::names_a_later_relation(
                &cs,
                dialect.as_ref(),
                None,
                &Schema::default(),
            );
            let prefixes = crate::dependents::key_name_prefixes(&cs);
            match target {
                Some(target)
                    if !prefixes.is_empty() || offline.iter().any(|n| !n.searched.is_empty()) =>
                {
                    Box::pin(async {
                        let mut conn = db::connect(target).await?;
                        let holders = pbps_pg::catalog::key_name_holders(&mut conn, &prefixes)
                            .await
                            .map_err(|e| anyhow::anyhow!("reading what holds key names: {e}"))?;
                        for name in crate::dependents::names_a_later_relation(
                            &cs,
                            dialect.as_ref(),
                            Some(&holders),
                            &Schema::default(),
                        ) {
                            if name.searched.is_empty()
                                || !crate::engine::resolves_now(&mut conn, &name).await?
                            {
                                refused.push(name);
                            }
                        }
                        Ok::<_, anyhow::Error>(())
                    })
                    .await?;
                }
                Some(_) | None => refused = offline,
            }
        }
        crate::dependents::later_relation_refusal(&refused)
            .map_err(|why| anyhow::anyhow!("module_dependents (PostgreSQL): {why}"))?;

        // Rendered now; with a target, written as the build's last step inside
        // its transaction, after the ledger row: a script written first
        // overwrote the file with one the same command then refused (#1684).
        // Refusals come before the lock, under it (two roles the collation reads
        // as one name, DECISIONS 123) and after the build (a definer routine
        // without a pinned path, #322), so only the end of the build is past all
        // of them. A write that fails there rolls the build back with it.
        //
        // Emitted now and rendered at the write: the pins carry the zone the
        // build ran in, which a target tells only once connected (DEC-1916.1).
        let script = sql_out
            .map(|_| {
                crate::statements(
                    &cs.clone()
                        .ordered(pbps_model::PlanMode::Transactional, dialect.as_ref()),
                    &dialect,
                )?
                .bootstrap_script(&loaded.schema, &project.config.trusted_roles)
            })
            .transpose()?;
        let write_script = |zone: &SessionZone| -> anyhow::Result<()> {
            if let (Some(path), Some(statements)) = (sql_out, &script) {
                let script = crate::sql_script(statements, "an empty database", zone)?;
                std::fs::write(path, script)
                    .with_context(|| format!("cannot write `{}`", path.display()))?;
                println!("wrote {}", path.display());
            }
            Ok(())
        };

        let Some(target) = target else {
            if sql_out.is_none() {
                bail!(
                    "bootstrap needs somewhere to go: pass --sql {}, --db or --env",
                    crate::report::placeholder("file")
                );
            }
            // No target to ask: UTC until #1918 (DEC-1916.1).
            return write_script(&SessionZone::offline_utc()).map(|()| services::Bootstrap::Script);
        };

        let statements = crate::statements(
            &cs.clone()
                .ordered(pbps_model::PlanMode::Transactional, dialect.as_ref()),
            &dialect,
        )?;

        Box::pin(async {
            let mut conn = db::connect(target).await?;
            let bootstrap_pins =
                bootstrap_type_pins(&mut conn, &loaded.schema, &cs, project).await?;
            // A collation the server lacks is refused here, by name, rather than
            // by the `CREATE TABLE` that names it after the ones before it ran
            // (#1175).
            crate::engine::refuse_unknown_collations(&mut conn, &loaded.schema).await?;
            // A key to a declared history under another spelling (#1625).
            crate::engine::refuse_history_references_alike(
                &mut conn,
                &loaded.schema,
                dialect.as_ref(),
            )
            .await?;
            // A history retention or a cascading key the server lacks (#1502).
            crate::engine::refuse_unsupported_temporal(&mut conn, &cs).await?;
            crate::engine::permission_support(&mut conn, &cs, &ids).await?;
            crate::engine::lock(&mut conn, operator).await?;
            let mut transaction_attempted = false;
            let result = async {
                refuse_wrongly_spelt_schemas(&mut conn, &loaded.schema).await?;

                // The empty-target check is protected by the same lock as the
                // build. Checking before it would let two bootstraps both observe
                // emptiness, then let the second continue after the first had
                // committed and released the lock.
                //
                // No rows are read here: the question is whether any declared
                // object stands, and a table that does is refused before its rows
                // matter.
                let existing = managed_state_full(
                    &mut conn,
                    &ids,
                    &declared_modules,
                    &pbps_diff::external_parents(&loaded.schema),
                    target.catalog_policy(project.config.unmanaged),
                    &BTreeMap::new(),
                    crate::engine::Read::Snapshot,
                    warnings,
                )
                .await?;

                // Unsupported grants are outside role.grants, so an apparently
                // empty role is not proof that the database is empty (110).
                refuse_unexpressible(&existing.scoped, &target.label, "bootstrap again")?;
                crate::engine::refuse_missing_cluster_roles(
                    conn.driver(),
                    &existing.scoped.missing_roles,
                )?;
                // This command builds what it grants on, so the role running it
                // owns every object the declarations name. A grant to that role
                // is the impossible line a connected plan is refused for, and
                // here there is nothing downstream to catch it: the engine writes
                // only the owner's default ACL entry, the pull reads that entry
                // as the zero point (DECISIONS 371), and this command compares no
                // declared grant against what it built. Unrefused it reports
                // success and records a snapshot that does not hold the grant
                // (#261).
                crate::engine::owned_targets(
                    conn.driver(),
                    &cs,
                    &ids,
                    &existing.scoped.owners,
                    &existing.scoped.session_role,
                )?;

                // Only names the plan creates need to be free (DECISIONS 118).
                // PostgreSQL grants use existing cluster roles; their existence
                // is outside this database's ownership (DECISIONS 211).
                let (wanted, vacated) = role_name_expectations(
                    &cs.clone()
                        .ordered(pbps_model::PlanMode::Transactional, dialect.as_ref()),
                    dialect.as_ref(),
                    pbps_model::PlanMode::Transactional,
                    0,
                )?;
                refuse_taken_role_names(&mut conn, &wanted, &vacated).await?;
                // A history name already taken: SQL Server takes an existing
                // table whose columns match as the history, with its rows,
                // instead of refusing (measured on 17.0, #1176).
                refuse_taken_history_names(&mut conn, &cs, &target.label).await?;

                refuse_extension_prerequisite_drift(
                    &existing.scoped.schema,
                    &loaded.schema,
                    &ids,
                    dialect.as_ref(),
                    "bootstrap",
                )?;
                refuse_nonempty_bootstrap(
                    &existing.scoped.schema,
                    &existing.limitations,
                    dialect.as_ref(),
                    &target.label,
                )?;

                crate::engine::recheck_type_prerequisites(
                    &mut conn,
                    &bootstrap_pins,
                    &cs,
                    &project.config.trusted_roles,
                    false,
                )
                .await?;
                transaction_attempted = true;
                execute_transaction_body(
                    &mut conn,
                    dialect.as_ref(),
                    &statements,
                    &bootstrap_pins,
                    &cs,
                    &project.config.trusted_roles,
                )
                .await?;
                // Bootstrap creates routines as `apply` does, so the same form is
                // required of a definer among them (#322).
                crate::engine::refuse_unsafe_definers(
                    &mut conn,
                    &crate::engine::written_routines(&cs),
                    crate::engine::Absent::Refused,
                )
                .await?;

                // Read-back and the success ledger row are part of the same
                // transaction as the DDL. If either fails, the database is still
                // empty and bootstrap can be retried honestly.
                //
                // The state recorded is what the engine actually built, read back —
                // not what was declared. Expressions come back in the engine's
                // stored form, and only that form compares equal on the next drift
                // check (SPEC §8.2). The rows too, under the declared scope:
                // bootstrap inserted them, and the engine's spelling of each value
                // is what the record has to hold.
                let built = managed_state(
                    &mut conn,
                    &ids,
                    &declared_modules,
                    &pbps_diff::external_parents(&loaded.schema),
                    target.catalog_policy(project.config.unmanaged),
                    &loaded.schema.data_scopes(),
                    &loaded.schema,
                    crate::engine::Read::InsideOwnTransaction,
                    warnings,
                )
                .await?;
                // A database-side trigger can add a privilege during the build.
                // Never commit a snapshot that silently omits it (110, 147).
                refuse_unexpressible(&built, &target.label, "bootstrap again")?;
                refuse_extension_prerequisite_drift(
                    &built.schema,
                    &loaded.schema,
                    &ids,
                    dialect.as_ref(),
                    "bootstrap",
                )?;
                crate::engine::refuse_missing_cluster_roles(conn.driver(), &built.missing_roles)?;
                // And it can take one back. `ddl_command_end` fires on `GRANT`
                // and on `REVOKE` (measured), so a trigger already in the
                // database can reverse what this build just settled about
                // `PUBLIC` before this read — and no other check here would see
                // it, because what `PUBLIC` holds is in neither the schema nor
                // the unexpressible list (DECISIONS 371, 517). The same
                // postcondition the apply paths ask, at the read this command
                // records.
                refuse_unmet_public_execution(&cs, &built, &target.label)?;
                // Only the uids of what this build made. The ids file may name
                // tables, types and roles these declarations do not hold; recording
                // those as held would make the next plan skip them (#1600, #1606).
                crate::engine::prove_tables_absent(&mut conn, &built.missing).await?;
                let gone = crate::engine::absent_roles(&mut conn, &built.missing_roles).await?;
                let mut held = ids
                    .without(&built.missing)
                    .without_roles(&gone)
                    .without_types(&built.missing_types);
                held.extensions.clone_from(&prerequisite_ids.extensions);
                crate::engine::recheck_type_prerequisites(
                    &mut conn,
                    &bootstrap_pins,
                    &cs,
                    &project.config.trusted_roles,
                    true,
                )
                .await?;
                let snapshot = with_provenance(
                    project.root(),
                    bootstrap_record(built, held, &loaded, operator),
                );
                let id = crate::engine::record(&mut conn, &snapshot).await?;
                write_script(&pin_zone(&conn))?;
                Ok::<_, anyhow::Error>((id, snapshot))
            }
            .await;
            let result = if transaction_attempted {
                finish_transaction(&mut conn, dialect.as_ref(), result).await
            } else {
                result
            };
            if transaction_attempted && let Err(error) = &result {
                record_failed_bootstrap(&mut conn, project.root(), operator, error).await;
            }
            let unlocked = crate::engine::unlock(&mut conn).await;
            let (id, snapshot) = match result {
                Ok(built) => built,
                Err(error) => {
                    return Ok(services::Bootstrap::Failed {
                        error,
                        released: unlocked,
                    });
                }
            };
            Ok(services::Bootstrap::Built(services::Bootstrapped {
                entry: id,
                tables: snapshot.schema.tables.len(),
                target: target.label.clone(),
                released: unlocked,
            }))
        })
        .await
    })
}

/// Best-effort audit row for a bootstrap whose transactional work was rolled
/// back. The empty state was established under the lock before the build, so it is
/// the last trustworthy state to carry forward; the declared identity mapping
/// has not become an environment mapping until the build succeeds.
///
/// `reason` goes through [`crate::engine::ledger_safe_reason`] for the same
/// reason `failed_apply_snapshot`'s does: bootstrap fails the same way apply
/// does, through the same `execute_statements` (DECISIONS 455).
async fn record_failed_bootstrap(
    conn: &mut Conn,
    root: &std::path::Path,
    operator: &str,
    error: &anyhow::Error,
) {
    let mut failed = StateSnapshot::new(
        StateKind::Failed,
        Schema::default(),
        IdsFile::default(),
        operator,
    );
    failed.git_sha = db::git_sha(root);
    failed.reason = Some(
        crate::engine::truncate_reason(conn.driver(), &crate::engine::ledger_safe_reason(error))
            .into(),
    );
    match crate::engine::record(conn, &failed).await {
        Ok(id) => eprintln!("Bootstrap failure recorded as ledger entry #{id}."),
        Err(audit_error) => eprintln!(
            "warning: the failed bootstrap could not be added to the ledger: {audit_error}"
        ),
    }
}

/// `pbps state prune` — drop old snapshots.
pub fn cmd_prune(target: &Target, keep: u32) -> anyhow::Result<()> {
    db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;
        let removed = crate::engine::prune(&mut conn, keep).await?;
        // The effective figure, not the one asked for: `--keep 0` still keeps
        // the newest entry, and reporting "0 remain" would describe an
        // environment with no baseline — which is not what happened.
        let kept = keep.max(1);
        println!(
            "Pruned {removed} old snapshot(s) from `{}`; the {kept} newest remain.",
            pbps_model::escape::Escaped(&target.label)
        );
        if keep == 0 {
            println!(
                "(--keep 0 still keeps the current baseline: dropping it would switch drift detection off.)"
            );
        }
        Ok(())
    })
}

/// `pbps unlock` — clear a lock left behind by a process that died.
pub fn cmd_unlock(target: &Target) -> anyhow::Result<()> {
    db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;
        let holder = crate::engine::lock_holder(&mut conn).await?;
        match crate::engine::unlock(&mut conn).await? {
            true => {
                let who = holder
                    .as_ref()
                    .map(|h| format!("`{}` since {}", h.locked_by, h.locked_at))
                    .unwrap_or_else(|| "an unnamed holder".to_owned());
                println!(
                    "Released the lock on `{}`, held by {who}.",
                    pbps_model::escape::Escaped(&target.label)
                );
                // Printed after the release as well as in every "locked by"
                // refusal before it: if the holder is in fact alive, this is
                // how to find it and stop it before it records over whatever
                // runs next (#1188).
                if let Some(lookup) = holder.and_then(|h| h.session_lookup) {
                    println!("{lookup}");
                }
                // Nothing about a released lock proves the operation it guarded
                // finished, and a half-finished apply is exactly what drift
                // detection is for.
                println!("Run `pbps verify` before applying anything else.");
            }
            false => println!(
                "`{}` was not locked.",
                pbps_model::escape::Escaped(&target.label)
            ),
        }
        Ok(())
    })
}

/// Advisory only: a reused name supplies no identity intent (SPEC 14.3).
pub(crate) fn type_replacement_warnings(
    base: &IdsFile,
    desired: &IdsFile,
    changes: &pbps_model::ChangeSet,
    json: bool,
) -> Vec<crate::output::Finding> {
    let resolver = TypeResolver::new(base, desired);
    changes.changes.iter().filter_map(|step| {
        let pbps_model::Change::DropType { uid: old, name, .. } = &step.change else { return None };
        let TypeIdentity::Declared(new) = resolver.resolve(TypeSide::Desired, name) else { return None };
        if old == new || !changes.changes.iter().any(|s| matches!(&s.change, pbps_model::Change::CreateType { uid, .. } if uid == new)) {
            return None;
        }
        let message = format!(
            "Type `{}` is replaced: old uid `{old}` and new uid `{new}` are different types to pbps, so the old uid's history does not carry over to the new one. To change the type while keeping its uid, keep the original declaration file and its uid and edit the definition there; rename with `renamed_from`. Enum labels (#1635 slice 3) and a domain's NOT NULL, default and checks (slice 4a) change in place under the same uid; a domain's base type or collation is rebuilt under its uid (slice 4b). This is a suggestion only; pbps never infers identity from the name.",
            name.shown(),
        );
        if !json { eprintln!("warning: {message}"); }
        Some(crate::output::Finding::warning("plan.type-replacement", message))
    }).collect()
}

/// A standing `NOT VALID` check of a declared domain compares equal to its
/// declaration, as the model holds no validation (DECISIONS 251, DEC-1635.4);
/// the plan says so and names the statement that validates it (DEC-1635.8).
async fn domain_check_not_valid_warnings(
    conn: &mut Conn,
    recorded: &Schema,
    json: bool,
) -> anyhow::Result<Vec<crate::output::Finding>> {
    if conn.driver() != pbps_db::Driver::Postgres
        || !recorded
            .types
            .values()
            .any(|t| matches!(t, pbps_model::ManagedType::Domain { .. }))
    {
        return Ok(Vec::new());
    }
    let standing = pbps_pg::domain_alter::not_valid(conn)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(standing
        .into_iter()
        .filter(|found| recorded.types.contains_key(&found.domain))
        .map(|found| {
            let message = format!(
                "Check `{}` of domain `{}` is NOT VALID: the engine has not confirmed the rows that were there when it was added, and pbps compares it equal to its declaration because a declaration holds no validation.",
                pbps_model::escape::Escaped(&found.check),
                found.domain.shown(),
            );
            if !json {
                eprintln!("warning: {message}\n  Run `{}` by hand.", found.remedy);
            }
            crate::output::Finding::warning("plan.domain-check-not-valid", message)
                .remedy(format!("run `{}`", found.remedy))
        })
        .collect())
}

/// `pbps plan --db` — the applyable plan, computed against the target
/// environment as queried (SPEC §7.3).
///
/// This is the deployment layer, so it changes no files. The identity file was
/// settled when the MR was reviewed; a `plan --db` that quietly rewrote it
/// would mean the artifact the gate approves was computed against a mapping
/// nobody read.
///
/// `carried` receives the warnings the plan had gathered when it failed, in
/// JSON mode, for `main` to put beside the failure in the one envelope; human
/// mode printed them when they were found (#1703, DEC-1726.1).
#[allow(clippy::too_many_arguments)]
fn plan_db(
    project: &Project,
    target: &Target,
    declarations: services::Declarations,
    dialect: crate::emission::SelectedDialect,
    script: bool,
    staged: bool,
    json: bool,
    resolver_selection: Option<pbps_config::resolver::ResolverSelection>,
    no_rehearse: bool,
    assertions: Option<&crate::plan_assert::Selection>,
    carried: &mut Vec<crate::output::Finding>,
) -> anyhow::Result<services::PlanResult> {
    db::runtime()?.block_on(plan_db_selected(
        project,
        target,
        declarations,
        dialect,
        script,
        staged,
        json,
        resolver_selection,
        no_rehearse,
        assertions,
        carried,
        None,
    ))
}

/// A plan with no target assertions: the scratch test runner plans this way
/// and runs its own assertions on its own database. Not an `async fn`: an
/// extra async layer would hold the whole planning future twice, which
/// overflows a test thread's stack in a debug build.
#[allow(clippy::too_many_arguments)]
fn plan_db_async<'a>(
    project: &'a Project,
    target: &'a Target,
    declarations: services::Declarations,
    dialect: crate::emission::SelectedDialect,
    script: bool,
    staged: bool,
    json: bool,
    resolver_selection: Option<pbps_config::resolver::ResolverSelection>,
    no_rehearse: bool,
    carried: &'a mut Vec<crate::output::Finding>,
    owned: Option<&'a services::Progress>,
) -> impl std::future::Future<Output = anyhow::Result<services::PlanResult>> + 'a {
    plan_db_selected(
        project,
        target,
        declarations,
        dialect,
        script,
        staged,
        json,
        resolver_selection,
        no_rehearse,
        None,
        carried,
        owned,
    )
}

#[allow(clippy::too_many_arguments)]
async fn plan_db_selected(
    project: &Project,
    target: &Target,
    declarations: services::Declarations,
    dialect: crate::emission::SelectedDialect,
    script: bool,
    staged: bool,
    json: bool,
    resolver_selection: Option<pbps_config::resolver::ResolverSelection>,
    no_rehearse: bool,
    assertions: Option<&crate::plan_assert::Selection>,
    carried: &mut Vec<crate::output::Finding>,
    // An owned test run lends the collector its caller drains after the phase:
    // a cancelled or expired phase drops this future, and a collector local to
    // it would take the observed findings along (#2694). `None` is the command.
    owned: Option<&services::Progress>,
) -> anyhow::Result<services::PlanResult> {
    let quiet = owned.is_some();
    pbps_cli::with_plan_dialect!(dialect, |mut dialect| {
        let services::Declarations { mut loaded, ids } = declarations;
        refuse_invalid_declarations(&loaded, dialect.as_ref(), json)?;
        let created_at = crate::now();
        // Keep the runtime alive for this connection across identity resolution.
        // Target provenance must be checked before a stale local mapping can
        // obscure the deletions it would plan (DEC-2397.1).
        let checked_target = async {
            let mut conn = db::connect(target).await?;
            let Some(entry) = crate::engine::latest(&mut conn).await? else {
                bail!(
                    "`{}` has a ledger but no entries; there is nothing to plan against.\n\
                     Record one with `pbps baseline --reason ...`.",
                    pbps_model::escape::Escaped(&target.label)
                );
            };
            refuse_mid_deployment(&entry, &target.label)?;
            let revision = if !entry.snapshot.kind.establishes_schema() {
                crate::engine::deployed_revision(&mut conn, entry.id).await?
            } else {
                Some(pbps_db::ledger::StateRevision {
                    id: entry.id,
                    git_sha: entry.snapshot.git_sha.clone(),
                })
            };
            let removals =
                checkout::inventory(&entry.snapshot, &ids, &loaded.schema, &loaded.intents);
            let findings: Vec<_> =
                checkout::assess(project.root(), &target.label, revision.as_ref(), &removals)?
                    .into_iter()
                    .collect();
            Ok::<_, anyhow::Error>((conn, entry, findings, revision.and_then(|r| r.git_sha)))
        }
        .await;
        // A failed target read cannot produce a plan, but must not replace the
        // established local identity refusals with an environment failure
        // (SPEC 9.8). Retain the error until those checks finish; it is never
        // missing evidence that would permit planning to continue.
        let (checked_target, checkout_findings, retirement_revision) = match checked_target {
            Ok((conn, entry, findings, revision)) => (Ok((conn, entry)), findings, revision),
            Err(error) => (Err(error), Vec::new(), None),
        };
        if !json {
            eprint!("{}", crate::output::human(&checkout_findings));
        }
        if checkout_findings
            .iter()
            .any(|f| f.severity == crate::output::Severity::Error)
        {
            let report = crate::output::Report::plain("plan", checkout_findings);
            return Ok(services::PlanResult::Refused(report));
        }
        if json {
            carried.clone_from(&checkout_findings);
        }

        if let Ok((_, entry)) = &checked_target {
            let reversed = pbps_diff::rebuild::rebuilt_since(&ids, &entry.snapshot.ids);
            anyhow::ensure!(
                reversed.is_empty(),
                "this environment already applied an approved rewrite of {}; restore the approval before planning; neither a drop and create nor a reverse rebuild is planned",
                reversed
                    .iter()
                    .map(|t| t.shown().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        // Identity and the eventual diff use the same recorded snapshot: a
        // missing checkout UID supplies no drop reason (DEC-2414.1).
        let context = crate::context(project.root())?;
        let resolution = match checked_target.as_ref() {
            Ok((_, entry)) => pbps_diff::identity::resolve_with_baseline(
                &loaded.schema,
                &ids,
                &loaded.intents,
                loaded.intents.len(),
                pbps_diff::Side {
                    schema: &entry.snapshot.schema,
                    ids: &entry.snapshot.ids,
                },
                &context,
            ),
            // The target read's saved error still prevents a plan below.
            Err(_) => pbps_diff::resolve_with_annotations(
                &loaded.schema,
                &ids,
                &loaded.intents,
                loaded.intents.len(),
                &context,
            ),
        };
        let mut resolved = match resolution {
            Ok(r) => r,
            Err(blockers) => {
                if json {
                    return Ok(services::PlanResult::Refused(crate::output::Report::plain(
                        "plan",
                        checkout_findings
                            .iter()
                            .cloned()
                            .chain(crate::report::blocker_findings(&blockers))
                            .collect(),
                    )));
                }
                eprintln!("{}", crate::report::blockers(&blockers));
                bail!("some changes could not be decided automatically");
            }
        };
        // As `plan` prunes: a record whose type has moved on is stale
        // identity, and this plan must be computed against what was reviewed
        // (DEC-2331.1).
        pbps_diff::conversion::prune(&mut resolved.ids, &loaded.schema, dialect.as_ref());
        // Diagnose an unsupported package action before the generic stale-ids
        // gate: CREATE/DROP themselves change the tentative identity section.
        if let Ok((_, entry)) = &checked_target {
            let errors = pbps_diff::extension_differences(
                pbps_diff::Side {
                    schema: &entry.snapshot.schema,
                    ids: &entry.snapshot.ids,
                },
                pbps_diff::Side {
                    schema: &loaded.schema,
                    ids: &resolved.ids,
                },
                dialect.name(),
            );
            if !errors.is_empty() {
                bail!(
                    "{}",
                    errors
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
        }
        if resolved.ids != ids {
            let message = format!(
                "the identity file is out of date; run `pbps plan` locally and commit `{}`.\n\
             A deployment plan must be computed against the identity its reviewers read.",
                project.ids_file().display()
            );
            // The same finding offline `plan --check` reports, so a pipeline
            // reads one id for one refusal whichever command met it (#573).
            if json {
                return Ok(services::PlanResult::Refused(crate::output::Report::plain(
                    "plan",
                    checkout_findings
                        .iter()
                        .cloned()
                        .chain([crate::output::Finding::error("identity.stale", message)
                            .at(project.ids_file(), None)
                            .remedy("pbps plan")])
                        .collect(),
                )));
            }
            bail!("{message}");
        }

        let (mut conn, mut entry) = checked_target?;
        let mut dependencies = pbps_model::dependency::unexamined(&loaded.dependencies);
        let mut target_assertions = Vec::new();
        let connected = Box::pin(async {
        // First, before the resolver and rehearsal: a violation stops cheaply,
        // and a refused login never reaches the expensive steps (DEC-2401.4).
        // Run on fresh assertion connections; the planning session only names
        // the target they must match. `run` returns a boxed future so the
        // engines' large runner futures stay out of this block's frame.
        if let Some(selection) = assertions {
            match selection.run(&mut conn).await {
                Ok(passed) => target_assertions = passed,
                Err(crate::plan_assert::Stop::Refused(finding)) => {
                    return Ok(Connected::Refused { finding, findings: checkout_findings.clone() });
                }
                Err(crate::plan_assert::Stop::Unanswerable(finding)) => {
                    return Err(crate::plan_assert::Unanswerable(finding).into());
                }
            }
        }
        // The selected resolver entry's scratch, acquired at most once for the
        // resolver and the rehearsal both (#2379).
        let mut scratch = resolver_selection
            .as_ref()
            .map(|selection| crate::resolution::Entry::new(selection, project, target));
        dependencies = crate::engine::compare_dependencies(&mut conn, &loaded.dependencies).await?;
        let initial_types = crate::engine::seal_type_pins(
            &mut conn,
            &[&loaded.schema],
            &Default::default(),
            &project.config.trusted_roles,
            project.config.pin_type_routines,
        )
        .await?;
        let type_routines_need_review = !crate::engine::type_input_routines(
            &mut conn, &initial_types.types, &project.config.trusted_roles,
            project.config.pin_type_routines, false,
        ).await?.is_empty();
        let mut findings = checkout_findings;

        let role_renames =
            crate::engine::external_role_renames(&mut conn, &entry.snapshot.ids, &resolved.ids)
                .await?
                .renames;
        let recorded_snapshot = entry.snapshot.clone();
        rename_snapshot_roles(&mut entry.snapshot, &role_renames);
        let recorded_ids = role_scope(&entry.snapshot.ids, &resolved.ids, dialect.as_ref());
        // The baseline's module scope is the **recorded** state's, never the
        // declarations': `apply` has only the plan file and the ledger, so a
        // scope that needed a checkout would make the two checksums disagree on
        // a host with none. A declared module that exists but was never
        // recorded is simply created again, which `CREATE OR ALTER` makes
        // harmless.
        let recorded_modules = managed_modules(Some(&entry.snapshot), None);
        // The policy is applied separately from the scope. The scope has to stay
        // the recorded set, or the two checksums below would be computed over
        // different object sets — but a module that this revision declares for
        // the first time and that already exists in the database is outside it,
        // and telling its author "this object is not declared" while planning
        // the `CREATE OR ALTER` for it would be both wrong and unactionable.
        //
        // The rows are read once, widely enough for both scopes that meet here
        // (ADR-0004): the recorded one, which the drift check below compares
        // against, and the declared one, which the differ measures the
        // declarations against — every row an `exact` declaration is about to
        // delete included.
        let recorded_data = entry.snapshot.schema.data_scopes();
        // Every declared text, as the engine reads it: a spelling it would
        // read back differently is refused before a plan is written that
        // could never converge (DECISIONS 101).
        for notice in crate::row_security::binding_notices(&loaded.schema) {
            if !json {
                eprintln!("warning: {notice}");
            }
            findings.push(crate::output::Finding::warning(
                "plan.policy-bindings-unchecked",
                notice,
            ));
        }
        let mut unasked = Vec::new();
        let probe_schema = pbps_diff::enum_labels::probe_schema(
            pbps_diff::Side { schema: &entry.snapshot.schema, ids: &recorded_ids },
            pbps_diff::Side { schema: &loaded.schema, ids: &resolved.ids },
            &loaded.hints,
        );
        let refused = refuse_misspelt_with_probe(
            &mut conn,
            &loaded.schema,
            &probe_schema,
            &catalogued_as(&loaded.schema, &resolved.ids, &recorded_ids),
            &mut unasked,
            type_routines_need_review,
        )
        .await;
        // In the envelope, not beside it: a warning-only result of a
        // read-only command stays in its one JSON report (SPEC §9.8; #1660).
        for w in unasked {
            if !json {
                eprintln!("warning: {}", w);
            }
            findings.push(crate::output::Finding::warning(
                "plan.partition-default-unasked",
                w,
            ));
        }
        // Printed already in human mode; in JSON they are carried to the
        // envelope of any failure from here on, the spelling refusal just
        // below included (#1702, #1703).
        if json {
            carried.clone_from(&findings);
        }
        // The declarations as the engine reads them, a date or time in its
        // own spelling (DEC-1883.1), from here to the plan's recorded scope.
        loaded.schema = refused?;
        // The declarations under the names the database has *now*: a table
        // this plan renames is read, scoped and measured under its old name,
        // and a declared scope keyed by the new one would find no table and
        // be skipped — an `ensure` -> `exact` switch in the same revision as
        // the rename then planned none of its deletes.
        let declared_live = tables_under(&loaded.schema, &resolved.ids, &recorded_ids);
        let declared_data = declared_live.data_scopes();
        refuse_wrongly_spelt_schemas(&mut conn, &loaded.schema).await?;
        // A key the declarations renamed a label to is held by the row under
        // the label it had (DEC-2346.2).
        let mut to_read = pbps_model::data::read_scopes(&recorded_data, &declared_data);
        pbps_diff::enum_labels::read_before_the_renames(&mut to_read, &declared_live, &loaded.hints);
        let local_catalog = std::cell::RefCell::new(Vec::new());
        let catalog = owned.map_or(&local_catalog, |p| &p.findings);
        let managed = managed_state_full(
            &mut conn,
            &recorded_ids,
            &recorded_modules,
            // The recorded setting, as `verify` reads it: the live side is
            // what the ledger says pbps holds, and a declared switch is the
            // differ's to refuse (DEC-1533.1).
            &pbps_diff::external_parents(&entry.snapshot.schema),
            target.catalog_policy(pbps_config::Unmanaged::Ignore),
            &to_read,
            crate::engine::Read::Snapshot,
            CatalogWarnings::for_output(json, catalog),
        )
        .await;
        findings.extend(catalog.take());
        if json {
            carried.clone_from(&findings);
        }
        let managed = managed?;
        refuse_unexpressible(&managed.scoped, &target.label, "plan again")?;
        let mut advisories = permission_advisory_findings(&managed.permission_advisories, project);
        // Both report-only inventories must reach the same policy gate.
        let policies = project.config.policies();
        let rule = pbps_policy::rules::UNHELD_OBJECT;
        let today = pbps_policy::civil::at(crate::unix_seconds(), 0).date();
        if let Some(severity) = policies.effective(rule).severity
            && !policies.suppressed(rule, None, &today)
        {
            advisories.extend(managed.catalog_notices.iter().map(|message| {
                crate::policy_finding(&pbps_model::Finding::new(rule, severity, message.clone(), None))
            }));
        }
        if !json {
            eprint!("{}", crate::output::human(&advisories));
        }
        let refused_by_policy = advisories
            .iter()
            .any(|f| f.severity == crate::output::Severity::Error);
        findings.extend(advisories);
        if json {
            carried.clone_from(&findings);
        }
        if refused_by_policy {
            if json {
                if quiet { carried.clone_from(&findings); } else {
                    crate::output::Report::plain("plan", findings).emit_json()?;
                }
            }
            bail!(
                "a policy set to `error` refuses this plan; fix the declarations, or suppress \
                 the rule in pbps.yml with a reason"
            );
        }
        // A connected plan becomes an applyable artifact. If introspection
        // omitted a fact on a recorded table, its scoped checksum can still
        // match while the real database does not; planning from that partial
        // projection would bless the omission and an empty apply would never
        // reconnect to catch it. The same argument as the line above, about
        // the other half of what a projection can fail to hold.
        refuse_managed_limitations(&managed.limitations)?;
        // The declarations as this database reads them back: a collation
        // equal to its default is a column with none (#1175). Compared as
        // written, an unchanged column was altered to the collation it
        // already has on every plan, and a created one read back without
        // it failed the apply's own postcondition.
        let declared =
            declarations_as_compared(&loaded.schema, managed.database_collation.as_deref());
        crate::engine::refuse_unknown_collations(&mut conn, &declared).await?;
        // A key to a declared history under another spelling of its name,
        // which only the database's collation can join (#1625).
        crate::engine::refuse_history_references_alike(&mut conn, &declared, dialect.as_ref())
            .await?;
        // And now that the default is known, an absent collation facing a
        // named one on a foreign key is decidable (#1247 review).
        let mismatched =
            declared.foreign_key_collation_problems(managed.database_collation.is_some());
        if !mismatched.is_empty() {
            bail!(
                "the declarations have a foreign key the engine refuses (1757):\n  {}",
                mismatched.join("\n  ")
            );
        }
        // A declared name standing on an object introspection cannot express is
        // not something to plan around. It is absent from the scoped schema, so
        // the diff would emit an ungated `CreateModule` and `CREATE OR ALTER`
        // would replace it — removing, silently, the very options that made it
        // unreadable (ADR-0002: what pbps cannot reproduce, it does not touch).
        let colliding: Vec<&str> = managed
            .unreadable
            .iter()
            .filter(|(n, _)| loaded.schema.modules.keys().any(|id| n.matches_module(id)))
            .map(|(_, why)| why.as_str())
            .collect();
        if !colliding.is_empty() {
            bail!(
                "`{}` already holds {} declared object(s) that pbps cannot read back: {}.\n\
                 Planning would propose creating them, and `CREATE OR ALTER` would replace what \
                 is there. Remove the declaration, or recreate the object in a form pbps can \
                 express.",
                pbps_model::escape::Escaped(&target.label),
                colliding.len(),
                colliding.join(", ")
            );
        }
        let scoped = managed.scoped;
        let mut for_policy = scoped.clone();
        for_policy
            .unmanaged_modules
            .retain(|m| !loaded.schema.modules.contains_key(m));
        report_unmanaged(
            &for_policy,
            &managed.unreadable,
            &recorded_ids,
            &recorded_modules,
            project.config.unmanaged,
        )?;

        // The plan is computed against the environment *as queried*, so the
        // queried state had better be the recorded one. When it is not, the
        // plan would be pinned to a checksum that describes neither — and the
        // difference is drift, which has its own command and its own three
        // remedies. The rows are compared under the recorded scope, exactly as
        // `verify` compares them.
        let as_recorded = scoped.schema.clone().with_observed_rows(
            &managed.rows,
            &recorded_data,
            &entry.snapshot.schema,
        )?;
        let live = pbps_model::drift_checksum(&as_recorded, &recorded_ids);
        let mut expected = entry.snapshot.schema.clone();
        // Done also permits drop-and-create (377). Grants lost with the old
        // principal must be planned onto the new one, not rejected as drift.
        // Newly managed roles also start from their actual grants. Both sets
        // remain in the pinned baseline and the differ's input (421).
        let missing_roles = crate::engine::reconcile_cluster_roles(
            conn.driver(),
            &recorded_snapshot.ids,
            &recorded_ids,
            &role_renames,
            &as_recorded,
            &mut expected,
        )?;
        // First declarations adopt the live definition, as newly managed
        // cluster roles do. Already recorded types still participate in drift.
        for (name, definition) in &as_recorded.types {
            if matches!(TypeResolver::new(&entry.snapshot.ids, &resolved.ids).resolve(TypeSide::Base, name), TypeIdentity::ExternalOrUnrecorded(_)) {
                expected.types.insert(name.clone(), definition.clone());
            }
        }
        let recorded = pbps_model::drift_checksum(&expected, &recorded_ids);
        if live != recorded {
            bail!(
                "`{}` has drifted from the state recorded at {} (entry #{}).\n\
                 Run `pbps verify --env/--db ...` to see how, and resolve it before planning.",
                pbps_model::escape::Escaped(&target.label),
                entry.applied_at,
                entry.id
            );
        }

        // A table whose rows the declarations cover for the first time is
        // measured against what it holds, not against nothing: the rows are
        // adopted, the ones that differ are updated, and — for `exact` — the
        // ones nobody declared are deleted behind the gate. The note names
        // the current row count beside the typed takeover below (DEC-1803.1).
        for (name, scope) in &declared_data {
            if recorded_data.contains_key(name) {
                continue;
            }
            let message = match managed.rows.get(name) {
                Some(observed) => format!(
                    "Reference data: the declarations take over the rows of {name} ({}); it holds \
                     {} row(s) now.",
                    scope.mode,
                    observed.rows.len(),
                    name = name.shown()
                ),
                None => format!(
                    "Reference data: {name} does not exist yet; its declared rows go in with it.",
                    name = name.shown()
                ),
            };
            if !json {
                println!("{}", message);
            }
            findings.push(crate::output::Finding::note("data.adoption", message));
        }
        // The comments the database holds that no declaration has a `description` for: left as
        // they are, and said once at the end of the plan (DEC-2729.1).
        if dialect.holds_descriptions() {
            let count = pbps_diff::undeclared_comments(&scoped.schema, &loaded.schema);
            if count > 0 {
                findings.push(crate::output::Finding::note(
                    "plan.undeclared-comments",
                    undeclared_comments_note(count),
                ));
            }
        }
        let read_back = pbps_model::data::plan_base(
            &scoped.schema,
            &managed.rows,
            &recorded_data,
            &declared_live,
        )?;
        // Observe the attaching identities under their current names before
        // the differ chooses their optional scan CHECK (DEC-2126.1). Its key
        // carries that choice through re-derivation, sealing, apply and resume.
        if staged {
            dialect = crate::engine::staged_attach_dialect(
                &mut conn,
                &dialect,
                pbps_diff::Side { schema: &read_back, ids: &recorded_ids },
                pbps_diff::Side { schema: &declared, ids: &resolved.ids },
            ).await?;
        }
        // The engine respells every expression and module body it stores —
        // measured on SQL Server, a check declared `n > 0` reads back
        // `([n]>(0))` — so compared against the read-back an unchanged check
        // was dropped and re-added, and an unchanged filtered index rebuilt,
        // on every connected plan. The differ compares the declarations
        // against what was *declared* when each object was last written,
        // where the ledger recorded it (ADR-0009 §2.2, ADR-0013 §4,
        // DECISIONS 207–208).
        let mut base = entry.snapshot.declared.overlay(&read_back);
        // A read knows the sequence names it leaves out (#1631).
        pbps_dialect::name_read_sequences(&mut base, dialect.as_ref());
        // A removed module no longer has a declaration carrying its
        // `depends_on:` edge. The newest snapshot keeps those baseline
        // annotations so connected planning can still drop dependents first.
        let mut hints = loaded.hints.clone();
        hints.record_row_defaults(&entry.snapshot.schema);
        for (name, dependencies) in &entry.snapshot.module_deps {
            if !loaded.schema.modules.contains_key(name) {
                hints.module_deps.insert(name.clone(), dependencies.clone());
            }
        }
        // The rehearsal rebuilds the base as the target holds it, so a kept
        // module is ordered by the annotation it was written under, not by
        // this revision's (see `rehearsal::connected::Plan::base_hints`).
        let recorded_module_deps = entry.snapshot.module_deps.clone();
        // A rebuilt enum or domain keeps its comment, which `CREATE TYPE` does
        // not copy: it is a fact about the target, read before the differ
        // writes the swap that carries it (DEC-1635.7, DEC-1635.9). So is
        // which domains an array holds, which PostgreSQL refuses a new check
        // on in place and a rebuild takes instead.
        if conn.driver() == pbps_db::Driver::Postgres {
            let types: Vec<_> = base.types.keys().cloned().collect();
            hints.enum_comments = pbps_pg::type_rebuild::comments(&mut conn, &types).await?;
            let domains: Vec<_> = base
                .types
                .iter()
                .filter(|(_, ty)| matches!(ty, pbps_model::ManagedType::Domain { .. }))
                .filter_map(|(name, _)| Some((recorded_ids.type_uid(name)?.clone(), name.clone())))
                .collect();
            hints.domain_array_holders =
                pbps_pg::domain_alter::array_holders(&mut conn, &domains).await?;
            hints.extension_requires = extension_creates(&mut conn, &base, &declared, &resolved.ids).await?;
            hints.extension_updates =
                extension_update_facts(&mut conn, &base, &declared, &recorded_ids, &resolved.ids).await?;
            hints.extension_moves =
                extension_move_facts(&mut conn, &base, &declared, &recorded_ids, &resolved.ids).await?;
            hints.extension_drops = extension_drop_facts(
                &mut conn,
                &retired_extensions(&base, &declared, &recorded_ids, &resolved.ids),
            )
            .await?;
        }
        // SQL Server's computed columns that stand throughout are judged by
        // the catalog's edges below (`order_computed_by_edges`), under the
        // database's collation, not by the text screen (#1460).
        let screen = if conn.driver() == pbps_db::Driver::Mssql {
            pbps_diff::Screen::Catalog
        } else {
            pbps_diff::Screen::Text
        };
        // plan_base uses the declared mode to compare newly managed rows.
        // Carry the takeover separately so matching rows still reach the
        // checksum/approval gate and the ordinary ledger write (DEC-1803.1).
        let takeovers: Vec<_> =
            pbps_model::data::takeovers(&recorded_data, &declared_data, &scoped.schema)
                .into_iter()
                .map(|(name, mode)| {
                    let change = pbps_model::Change::SetDataMode {
                        table: recorded_ids.resolved_in(&name, &resolved.ids),
                        from: None,
                        to: Some(mode),
                    };
                    let risks = dialect.change_risks(&change);
                    let mut planned = pbps_model::PlannedChange::new(change);
                    planned.risks = risks;
                    planned
                })
                .collect();
        // The union read is for row comparison, not evidence that a key was
        // already managed. Carry the recorded scope transition by identity
        // even when no DML is needed (DEC-1808.1).
        let ensure_keys: Vec<_> = pbps_model::data::ensure_key_changes(
            &recorded_data,
            &declared.data_scopes(),
            &recorded_ids,
            &resolved.ids,
        )
        .into_iter()
        .map(|change| {
            let risks = dialect.change_risks(&change);
            let mut planned = pbps_model::PlannedChange::new(change);
            planned.risks = risks;
            planned
        })
        .collect();
        // The zero-SQL changes the diff cannot see, carried by every plan this
        // read makes: the initial one, each re-derivation, and the resolver's
        // (#1516 review). One list, so a plan cannot carry one and not the
        // other.
        let zero_sql = pbps_diff::order::writebacks::ScopeInput::new(
            takeovers.into_iter().chain(ensure_keys).collect(),
        )
        .expect("connected scope inputs are data-mode and ensure-key transitions");
        // The read-back goes in beside the overlaid base, for the comparisons
        // the engine makes by parse tree (an attach's checks and generation
        // expressions, #1642 review).
        if conn.driver() == pbps_db::Driver::Postgres && let Some(entry) = scratch.as_mut() {
            let before = pbps_diff::Side { schema: &base, ids: &recorded_ids };
            let after = pbps_diff::Side { schema: &declared, ids: &resolved.ids };
            let requests = pbps_diff::retained::candidates(before,after,&hints);
            if !requests.is_empty() && pbps_pg::retained::available(&mut conn,&requests).await {
                // Paired binding carries the resolver lifecycle. Keep its future
                // separate from the connected planner's other binding phases.
                hints.retained_expressions = Box::pin(crate::resolution::retained::compare(
                    entry,&mut conn,target,before,after,&read_back,&hints,&requests,
                )).await?;
            }
        }
        // The modules a rebuilt enum's columns carry, found in the planning
        // transaction and kept for every plan that follows it.
        let enum_dependents = std::cell::RefCell::new(std::collections::BTreeSet::new());
        let diff_here = |also: &std::collections::BTreeSet<pbps_model::ModuleId>| {
            let mut planning = hints.clone();
            planning.enum_dependents = enum_dependents.borrow().clone();
            pbps_diff::diff_read_back(
                pbps_diff::Side {
                    schema: &base,
                    ids: &recorded_ids,
                },
                pbps_diff::Side {
                    schema: &declared,
                    ids: &resolved.ids,
                },
                dialect.as_ref(),
                &planning,
                also,
                screen,
                &read_back,
            )
            .map(|changes| {
                // Re-derivations and resolver planning must retain the same
                // zero-SQL changes as the initial connected plan.
                zero_sql.append_to(changes, dialect.as_ref())
            })
        };
        let mut cs = diff_here(&std::collections::BTreeSet::new()).map_err(crate::Unexpressible)?;
        findings.extend(type_replacement_warnings(&recorded_ids, &resolved.ids, cs.changes(), json));
        findings.extend(domain_check_not_valid_warnings(&mut conn, &base, json).await?);

        let prerequisite_types = crate::engine::seal_plan_type_pins(
            &mut conn,
            pbps_diff::Side { schema: &base, ids: &recorded_ids },
            pbps_diff::Side { schema: &declared, ids: &resolved.ids }, &cs,
            &project.config.trusted_roles, project.config.pin_type_routines,
        ).await?;
        let needs_review = !crate::engine::type_input_routines(
            &mut conn, &prerequisite_types.types, &project.config.trusted_roles,
            project.config.pin_type_routines, false,
        ).await?.is_empty();
        // DEC-1515.1's three cases, decided from this ordinary plan before any
        // pass asks the catalog about it: case 2 then opens nothing a
        // resolver owns, and case 3 hands the resolver the plan it re-derives.
        let base_side = pbps_diff::Side {
            schema: &base,
            ids: &recorded_ids,
        };
        let desired_side = pbps_diff::Side {
            schema: &declared,
            ids: &resolved.ids,
        };
        let mut resolver_assessment = None;
        // Case 3's answer, unsealed: the resolver's order goes through every
        // pass and check below, as an ordinary plan does, and is sealed in
        // the order they leave (#1516).
        let mut resolution = None;
        match crate::resolution::case(
            resolver_selection.as_ref(),
            base_side,
            desired_side,
            &cs,
            dialect.as_ref(),
        ) {
            crate::resolution::Case::Fallback => {}
            crate::resolution::Case::NotNeeded { assessment } => {
                resolver_assessment = Some(crate::resolution::ResolverAssessment::not_needed(
                    &assessment,
                ));
            }
            crate::resolution::Case::Required {
                selection,
                assessment,
            } => {
                if needs_review {
                    bail!("resolver execution would run unreviewed type routines; use a separate reviewed plan before resolver execution (DEC-2181.2)");
                }
                // A resolved plan is sealed transactional: its evidence
                // describes one atomic change (`SavedPlan::with_resolution`).
                // Refused before the cluster identity is read, so the gate
                // names itself to a role that may not read it.
                if staged {
                    let finding = crate::resolution::staged(selection, &assessment);
                    return Ok(Connected::Refused { finding, findings });
                }
                // The cluster this plan was read from: the resolver checks its
                // own target connection against it (#1685).
                let identity =
                    match crate::resolution::planning_identity(&mut conn, selection).await {
                        Ok(identity) => identity,
                        Err(crate::resolution::Refused::Finding(finding)) => {
                            return Ok(Connected::Refused { finding, findings });
                        }
                        Err(crate::resolution::Refused::Unanswerable(error)) => return Err(error),
                    };
                let request = crate::resolution::Request {
                    sources: Some(&loaded.sources),
                    dependencies: &loaded.dependencies,
                    selection,
                    assessment: &assessment,
                    target,
                    project,
                    base: base_side,
                    desired: desired_side,
                    hints: &hints,
                    target_identity: identity.as_deref(),
                };
                let asked = crate::resolution::named(&assessment);
                // Boxed like the retained comparison above: the resolver's
                // future is the largest in this block, and the bootstrap
                // prerequisites (DEC-2447.2) it now carries tipped the owned
                // lifecycle tests' native thread stacks over their limit.
                match Box::pin(crate::resolution::resolve(
                    &request,
                    scratch.as_mut().expect("a required resolution selected an entry"),
                )).await {
                    Ok(unsealed) => {
                        resolver_assessment =
                            Some(crate::resolution::ResolverAssessment::resolved(&assessment));
                        // The resolver's order, with the connected plan's
                        // zero-SQL changes after it, where `diff_here`
                        // keeps them.
                        cs = crate::resolution::Continuation::with_scope(
                            dialect.as_ref(), &zero_sql, &unsealed,
                        );
                        resolution = Some((selection.name.clone(), unsealed, asked));
                    }
                    Err(refused) => {
                        // The container the resolver used is confirmed gone
                        // before the refusal, as it was when the resolver
                        // owned it alone.
                        let released = scratch.as_mut().map(|entry| entry.finish());
                        if let Some(released) = released
                            && let Err(crate::resolution::Refused::Unanswerable(error)) =
                                released.await
                        {
                            return Err(error);
                        }
                        match refused {
                            crate::resolution::Refused::Finding(finding) => {
                                return Ok(Connected::Refused { finding, findings });
                            }
                            crate::resolution::Refused::Unanswerable(error) => return Err(error),
                        }
                    }
                }
            }
        }

        // Before anything is checked against the plan: a name it creates that
        // the database already uses, outside what this project records, is a
        // `CREATE` the engine will refuse (#316, #320, #323).
        if conn.driver() == pbps_db::Driver::Postgres {
            refuse_occupied_names(
                &cs,
                &scoped,
                &managed.unreadable,
                &managed.unmanaged_relations,
                &target.label,
            )?;
        }
        crate::engine::require_transactional_rebuilds(conn.driver(), &cs, staged)?;
        // The one admission rule of every site that runs code the plan holds
        // (DEC-2563.2). Review admits routines into it as the sites vet; the plan
        // shows and pins them when it seals.
        let site_admission = crate::pins::admission(project);
        conn.begin(&dialect.transaction_framing(&pin_zone(&conn)))
            .await?;
        let checks = async {
            // In the read-only planning transaction, with everything below:
            // the names this plan creates, asked of PostgreSQL's whole
            // relation namespace, not only of what the inventory reports
            // (#951). SQL Server's is asked after the passes below (#1077).
            let planned = crate::engine::Planned {
                base: pbps_diff::Side {
                    schema: &recorded_snapshot.schema,
                    ids: &recorded_ids,
                },
                desired: pbps_diff::Side {
                    schema: &loaded.schema,
                    ids: &resolved.ids,
                },
                dialect: dialect.as_ref(),
            };
            let name_facts = crate::engine::refuse_created_name_occupants(
                &mut conn,
                &mut cs,
                &target.label,
                planned,
            )
            .await?;
            crate::engine::refuse_unlogged_partition_referencers(
                &mut conn,
                &cs,
                &resolved.ids,
                staged,
            )
            .await?;
            crate::engine::refuse_published_unlogged(&mut conn, &cs, &resolved.ids).await?;
            let rename_evidence = crate::engine::external_role_renames(
                &mut conn,
                &recorded_snapshot.ids,
                &resolved.ids,
            )
            .await?
            .check;
            // A role this plan drops still has this environment's members,
            // which the engine will not drop it over. They are read here, for
            // the schedule to write into the plan by name, so the artifact the
            // gate approves lists exactly who is removed (ADR-0005) — never
            // found again at apply time, where nobody would have reviewed
            // them. Without a role drop there is nothing to ask.
            let role_members = if cs
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::DropRole { .. }))
            {
                Some(
                    crate::engine::role_members(&mut conn)
                        .await
                        .context("cannot read the role memberships")?,
                )
            } else {
                None
            };
            // The plan carries what the resolver's run bound at creation, so
            // every pass that orders it again reads the same bindings. Only the
            // observer below judges the finished `ChangeSet`, which does not
            // (DEC-2143.8).
            #[cfg(debug_assertions)]
            let captured = resolution
                .as_ref()
                .map(|(_, unsealed, _)| unsealed.captured().clone())
                .unwrap_or_default();
            // First, so that what it adds is held to every check after it: a
            // view this plan now drops and recreates takes `before_a_rebuild`
            // like one the declarations edit (#314, ADR-0009 §4).
            let rediff = |also: &std::collections::BTreeSet<pbps_model::ModuleId>| {
                // The resolver rebuilds what its evidence shows reaching a
                // replaced input. A module it did not rebuild that still
                // depends on one the plan drops is outside that evidence, and
                // planning again without the resolver would drop its answer
                // (#1516).
                if resolution.is_some() {
                    let names: Vec<String> = also.iter().map(|id| id.shown().to_string()).collect();
                    bail!(
                        "the resolver's plan drops a module that {} depend(s) on, and its \
                         evidence does not cover rebuilding them; declare the dependency, or \
                         change the module in a plan of its own",
                        names.join(", ")
                    );
                }
                diff_here(also).map_err(|errors| crate::Unexpressible(errors).into())
            };
            // What stands on an enum this plan rebuilds is read first: the views,
            // bodies and triggers over its columns come down and go back through
            // the module rebuild below, and anything else is refused by name
            // (DEC-1635.7).
            let cs = if conn.driver() == pbps_db::Driver::Postgres
                && cs.changes().changes.iter().any(|p| {
                    matches!(p.change, pbps_model::Change::EnumRebuild { .. })
                }) {
                let census = pbps_pg::type_rebuild::census(
                    &mut conn,
                    cs.changes(),
                    Some(&loaded.schema),
                    &recorded_ids,
                    false,
                )
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
                if census.rebuild.is_empty() {
                    cs
                } else {
                    enum_dependents.borrow_mut().extend(census.rebuild);
                    rediff(&std::collections::BTreeSet::new())?
                }
            } else {
                cs
            };
            let mut rebuild_verification = None;
            let (mut cs, dependents) = if cs.changes.iter().any(|p| matches!(p.change, pbps_model::Change::RebuildPartitionedTable(_))) {
                let (changes, check, verification) = crate::engine::expand_table_rebuild(&mut conn, &site_admission, cs,
                    pbps_diff::Side { schema: &base, ids: &recorded_ids },
                    pbps_diff::Side { schema: &declared, ids: &resolved.ids },
                    &hints, dialect.as_ref()).await?;
                rebuild_verification = Some(verification);
                (changes, check)
            } else {
                crate::engine::account_for_module_dependents(
                &mut conn,
                cs,
                &loaded.schema,
                crate::dependents::Sources {
                    before: pbps_diff::Side {
                        schema: &recorded_snapshot.schema,
                        ids: &recorded_ids,
                    },
                    after: &resolved.ids,
                },
                &hints.module_deps,
                role_members.as_ref(),
                dialect.as_ref(),
                &rediff,
            )
            .await?
            };
            // A retype or drop of a column a generated column reads goes
            // after the plan's change to that expression which stops reading
            // it (DEC-1316.1): after the module passes, which may move that
            // change, and before the checks that read the order.
            crate::engine::release_generated_inputs(
                &mut conn,
                &mut cs,
                pbps_diff::Side { schema: &recorded_snapshot.schema, ids: &recorded_ids },
                pbps_diff::Side { schema: &loaded.schema, ids: &resolved.ids },
                dialect.as_ref(),
                &hints.module_deps,
            ).await?;
            // A computed column's function drops after it, by the catalog's
            // edges (DEC-1431.1): before the rename walk, which moves a
            // module drop only between the drops these edges order it
            // among (#1680).
            let precedence =
                crate::engine::order_computed_by_edges(&mut conn, &mut cs, &resolved.ids).await?;
            // What an added computed column calls has no edge yet; its names
            // are compared under the collation instead (#1459).
            crate::engine::refuse_added_computed_calls(&mut conn, &cs, &declared).await?;
            // Last of the passes that order the plan, so the order it settles
            // from the catalog is the one checked below and saved (#1366).
            let object_facts = crate::engine::order_created_object_names(
                &mut conn,
                &mut cs,
                &target.label,
                &precedence,
                &crate::dependents::schedule::Declarations {
                    schema: &loaded.schema,
                    sources: crate::dependents::Sources {
                        before: pbps_diff::Side { schema: &recorded_snapshot.schema, ids: &recorded_ids },
                        after: &resolved.ids,
                    },
                    module_deps: &hints.module_deps,
                },
            )
            .await?;
            // The module passes may plan the revision again in the differ's
            // order (`rediff`), undoing what was ordered: the carried moves
            // are ordered and checked again, over the same reads, as the plan
            // now stands (#1767 review).
            if let Some(facts) = &name_facts {
                facts.order_and_refuse(&mut cs, &target.label, planned)?;
            }
            // Every pass that orders the plan has run: the verifier checks the
            // order that is saved and applied, in tests and debug builds
            // (#1954, DEC-1954.6).
            // Debug assertions alone, not `cfg(test)`: `pbps-diff` is compiled
            // without its own `cfg(test)` here, so a release test build has no
            // observer to call.
            if let Some(verification) = &rebuild_verification {
                verification.verify(pbps_diff::Side { schema: &base, ids: &recorded_ids }, pbps_diff::Side { schema: &declared, ids: &resolved.ids }, &hints, &cs, dialect.as_ref()).map_err(|why| anyhow::anyhow!("{why}"))?;
            }
            #[cfg(debug_assertions)]
            if rebuild_verification.is_none() { pbps_diff::order::shadow::observe_scheduled(
                pbps_diff::Side {
                    schema: &recorded_snapshot.schema,
                    ids: &recorded_ids,
                },
                pbps_diff::Side {
                    schema: &loaded.schema,
                    ids: &resolved.ids,
                },
                dialect.as_ref(),
                &hints.module_deps,
                &cs,
                pbps_diff::order::shadow::Stage::Final,
                Some(
                    &object_facts
                        .clone()
                        .or_else(|| name_facts.as_ref().map(crate::engine::NameFacts::slots))
                        .unwrap_or_default(),
                ),
                &captured,
            ); }
            // The observer's own gate: a release test build has `cfg(test)`
            // but no observer, so the facts are spent here too (#2089).
            #[cfg(not(debug_assertions))]
            let _ = object_facts;
            let rebuilds = crate::engine::check_module_rebuilds(&mut conn, &cs, false).await?;
            let drops = crate::engine::check_drop_blockers(
                &mut conn,
                &cs,
                &resolved.ids,
                &recorded_snapshot.schema,
                &recorded_ids,
            )
            .await?;
            let slots = crate::engine::logical_slots(&mut conn, &cs).await?;
            let fills = crate::engine::numbered_column_fills(&mut conn, &cs, &resolved.ids).await?;
            crate::engine::prepare_data_writes(
                &mut conn,
                &site_admission,
                &cs,
                &entry.snapshot,
                &resolved.ids,
            )
            .await?;
            Ok::<_, anyhow::Error>((
                cs,
                rename_evidence,
                dependents,
                rebuilds,
                drops,
                slots,
                fills,
            ))
        }
        .await;
        let rollback = conn
            .rollback(&dialect.transaction_framing(&pin_zone(&conn)))
            .await;
        let (mut cs, rename_evidence, dependents, rebuilds, drop_blockers, logical_slots, fills) =
            checks?;
        rollback?;
        let requirement_names = pbps_model::plan_names::PlanNames::of(&cs.changes().changes, &resolved.ids)?;
        crate::engine::vet_drop_requirements(&mut conn, &site_admission, &requirement_names)
            .await?;
        // A conversion is judged at apply, before its first evaluation
        // (DEC-2331.1). Here it only tells which routines the review admits, so
        // the plan can show and pin them; a refusal waits for apply, which
        // judges it again, and a routine missed here fails closed there.
        let _ = crate::engine::vet_conversions(&mut conn, &site_admission, requirement_names.changes())
            .await;
        // The expressions a table gains are bound the same way, to learn which
        // routines the review admits; one that uses an object this plan creates
        // cannot be bound yet and is said so, then judged at apply (DEC-2563.4).
        if conn.driver() == pbps_db::Driver::Postgres {
            for planned in requirement_names.changes() {
                let Ok(pbps_pg::table_expressions::Vetted::Unjudged(notes)) =
                    crate::engine::vet_table_expressions(
                        &mut conn,
                        &site_admission,
                        pbps_pg::pins::Within::OwnTransaction,
                        pbps_pg::table_expressions::Moment::Early,
                        &planned.change,
                    )
                    .await
                else {
                    continue;
                };
                for note in notes {
                    if !json {
                        eprintln!("warning: {note}");
                    }
                    findings.push(crate::output::Finding::warning(
                        "plan.table-expression-unbound",
                        &note,
                    ));
                }
            }
        }
        // Rows that hold a label an enum rebuild removes, or that a domain's
        // narrower base cannot hold, are counted now, by the same probes the
        // apply runs, so the plan is refused by name and with a count before
        // anything is written (DEC-1635.7, DEC-1635.9).
        if conn.driver() == pbps_db::Driver::Postgres
            && cs.changes().changes.iter().any(|p| {
                matches!(&p.change, pbps_model::Change::EnumRebuild {
                    step: pbps_model::EnumRebuildStep::Retype { values, .. }, ..
                } if match values.as_ref() {
                    pbps_model::ValueChange::Cast { from, to, .. } => from != to,
                    pbps_model::ValueChange::Relabel { removed } => !removed.is_empty(),
                })
            })
        {
            let names = pbps_model::plan_names::PlanNames::of(&cs.changes().changes, &resolved.ids)?;
            let result = run_probes_observed(
                &mut conn,
                &site_admission,
                dialect.as_ref(),
                &names,
                &probe_partitions(&recorded_snapshot.schema, &names),
                Some(pbps_model::operation::Snapshot {
                    schema: &recorded_snapshot.schema,
                    ids: &recorded_ids,
                }),
                None,
                owned,
            )
            .await;
            if let Some(owned) = owned {
                findings.extend(owned.findings.take());
            }
            if json { carried.clone_from(&findings); }
            result?;
        }
        // Again, over what the dependents added: a rebuild the plan now
        // synthesizes needs the transaction as much as one it was given.
        crate::engine::require_transactional_rebuilds(conn.driver(), &cs, staged)?;

        // The keys were matched to the rows under the type the key column has
        // now (71); a plan that changes that type would carry the mapping
        // into a type that does not make it (DECISIONS 108).
        let over_aliases = key_type_changes_over_aliases(
            &cs,
            &declared_live,
            &managed.rows,
            &resolved.ids,
            &recorded_ids,
        );
        if !over_aliases.is_empty() {
            bail!(
                "{}\n\
                 The declared keys were matched to the rows under the type the key column has \
                 now, and the new type may not read them the same way. Write each key as the \
                 engine spells it, apply that, then change the type.",
                over_aliases.join("\n")
            );
        }

        // A role this plan creates or renames to needs its name free of
        // every principal, not only of the roles the managed set knows:
        // users, roles and application roles share one namespace, and
        // `CREATE ROLE` on a name a user holds fails after everything
        // ordered before it has run (DECISIONS 118, 119).
        let (wanted, vacated) = role_name_expectations(
            &cs.clone().ordered(plan_mode(staged), dialect.as_ref()),
            dialect.as_ref(),
            plan_mode(staged),
            0,
        )?;
        refuse_taken_role_names(&mut conn, &wanted, &vacated).await?;

        // A role this plan drops, its members already written into it by the
        // schedule (above, in the planning transaction).
        if cs
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::DropRole { .. }))
        {
            // Ownership is refused, not planned around: the engine will not
            // drop an owning role, and moving ownership is a decision about
            // who owns a securable, not a consequence of a drop. Said here,
            // before anything runs — a staged apply would otherwise commit
            // every DROP MEMBER and then fail on the DROP ROLE.
            let owned = crate::engine::role_owned_securables(&mut conn)
                .await
                .context("cannot read what the roles own")?;
            for p in &cs.changes {
                if let pbps_model::Change::DropRole { name, .. } = &p.change
                    && let Some(securables) = owned.get(name.raw())
                    && !securables.is_empty()
                {
                    bail!(
                        "role `{name}` cannot be dropped in `{}`: it owns {}.\n\
                         Move the ownership first (`ALTER AUTHORIZATION ON {} TO dbo;`, \
                         by hand, since pbps does not decide who owns a securable), or keep \
                         the role.",
                        pbps_model::escape::Escaped(&target.label),
                        securables.join(", "),
                        securables[0],
                        name = name.shown(),
                    );
                }
            }
        }

        // The plan rules (ADR-0008), the connected ones included: this is the
        // one place a change window has a moment and a target to be measured
        // against. An `error` refuses the plan here, before anything is
        // written; `apply` never sees a policy.
        let policy = crate::attach_policy_findings(
            &mut cs, project, dialect.as_ref(), true,
            pbps_diff::Side { schema: &base, ids: &recorded_ids },
            pbps_diff::Side { schema: &declared, ids: &resolved.ids },
            retirement_revision.as_ref(),
        );
        if !json {
            for f in &policy {
                eprintln!("{}: {} — {}", f.severity_word(), f.id, f.message);
            }
        }
        if policy
            .iter()
            .any(|f| f.severity == crate::output::Severity::Error)
        {
            if json {
                // This is an answered policy question, before any artifact is
                // written. Keep the rule's typed fields and any earlier notes.
                findings.extend(policy);
                if quiet { carried.clone_from(&findings); } else {
                    crate::output::Report::plain("plan", findings).emit_json()?;
                }
            }
            bail!(
                "a policy set to `error` refuses this plan; fix the declarations, or suppress \
                 the rule in pbps.yml with a reason"
            );
        }

        findings.extend(policy);
        let permission_support = crate::engine::permission_support(&mut conn, &cs, &resolved.ids).await?;
        let generation_support =
            crate::engine::generation_support(&mut conn, &cs, &resolved.ids).await?;
        // Asked of the read this plan was built from, not of the connection
        // again: the owners came out of the same statement snapshot as the
        // schema, so the two cannot disagree (DECISIONS 174).
        let owned_targets = crate::engine::owned_targets(
            conn.driver(),
            &cs,
            &resolved.ids,
            &scoped.owners,
            &scoped.session_role,
        )?;
        // The other half of the same read, and the other direction: a grant
        // this connection could see and could not take away is refused only
        // when the plan actually revokes it (#251).
        let unrevocable_grants = crate::engine::unrevocable_grants(
            conn.driver(),
            &cs,
            &resolved.ids,
            &managed.unrevocable,
        )?;

        // The edition is a connection-time fact, and it is the only place the
        // two edition-dependent questions of ADR-0003 can be answered
        // honestly: whether ONLINE will be accepted at all, and whether an
        // addition that is metadata-only on Enterprise rewrites every row
        // here. An offline plan has to assume the conservative answer.
        let verdict = crate::engine::edition_verdict(&mut conn, &cs).await?;
        // A server fact like the edition: 2016 has no history retention and
        // no cascading key from a system-versioned table (#1502). Answered,
        // it is a finding, as the edition's below is; a failure to ask stays
        // an error (#1630).
        let temporal = crate::engine::unsupported_temporal(&mut conn, &cs).await?;
        crate::engine::refuse_answered_temporal(&temporal, json, &mut findings)?;
        if !verdict.refused_online.is_empty() {
            let refusal = format!(
                "`strategy: online` is declared for {}, and `{}` runs {}, which has no online \
                 index operations.\n\
                 The statement would fail partway through the apply. Remove the hint, or deploy \
                 this change to an edition that supports it.",
                verdict.refused_online.iter().map(|subject| pbps_model::escape::Escaped(subject).to_string()).collect::<Vec<_>>().join(", "),
                pbps_model::escape::Escaped(&target.label),
                verdict.runs
            );
            if json {
                // An answered question, like the policy refusal above: the
                // edition was read and it says no (DECISIONS 26, 485). Its own
                // id rather than `plan.edition` at error severity, which a
                // consumer could only tell from the advisories by the severity
                // (#574).
                findings.push(
                    crate::output::Finding::error("plan.online-unsupported", refusal.clone())
                        .remedy(
                            "remove `strategy: online` from these tables, or deploy to an edition \
                         with online index operations",
                        ),
                );
                if quiet { carried.clone_from(&findings); } else {
                    crate::output::Report::plain("plan", findings).emit_json()?;
                }
            }
            bail!("{refusal}");
        }
        for w in &verdict.warnings {
            if !json {
                eprintln!("warning: {w}");
            }
            findings.push(crate::output::Finding::warning("plan.edition", w));
        }

        // What the plan is pinned to is wider than what the drift check
        // compared: the rows of a table the declarations cover for the first
        // time were read (the differ measured against them) but are not in
        // the recorded scope, and a baseline that left them out would let a
        // row inserted between plan and apply slip past `apply`'s check —
        // and, for `exact`, survive the approved deletes. `apply` reads the
        // same union (`pinned_scopes`) under the same reference.
        let pinned = scoped.schema.with_observed_rows(
            &managed.rows,
            &pinned_baseline_scopes(
                &recorded_data,
                &declared_data,
                cs.changes(),
                &resolved.ids,
                (&entry.snapshot.schema, &entry.snapshot.ids),
            ),
            &entry.snapshot.schema,
        )?;
        let baseline = pbps_model::drift_checksum(&pinned, &recorded_ids);
        let cost = crate::engine::operational_cost(&mut conn, &cs, &resolved.ids).await;
        let crate::engine::Prerequisites {
            types: type_pins,
            providers: provider_pins,
        } = crate::engine::seal_plan_type_pins(
            &mut conn,
            pbps_diff::Side { schema: &entry.snapshot.schema, ids: &recorded_ids },
            pbps_diff::Side { schema: &loaded.schema, ids: &resolved.ids },
            &cs,
            &project.config.trusted_roles,
            project.config.pin_type_routines,
        )
        .await?;
        let reviewed =
            crate::pins::seal_selected(&mut conn, project, target, &type_pins, &site_admission)
                .await?;
        // Last, so that the routines are sealed as close to the gate as the
        // plan can get them. The managed set is the one `apply` rebuilds from
        // the ledger and the plan: what was recorded, and what is declared.
        let routine_pins = crate::pins::seal(
            &mut conn,
            project,
            target,
            &managed_modules(Some(&entry.snapshot), Some(&loaded.schema)),
        )
        .await?;
        let mut domain_vet_unavailable = false;
        let mut domain_code = None;
        if conn.driver() == pbps_db::Driver::Postgres
            && crate::resolution::domain::needed(cs.changes())
            && let Some(entry) = scratch.as_mut()
        {
            match crate::resolution::domain::vet(entry, &mut conn, target, project, desired_side).await {
                crate::resolution::domain::Verdict::Passed => {
                    domain_code = Some(crate::engine::ConnectedCheck {
                        name: "declared-domain-code", engine: "PostgreSQL", status: "checked",
                        message: "Declared domain CHECK and DEFAULT code passed the advisory scratch vet; apply repeats its own in-transaction vet.".into(),
                    });
                }
                crate::resolution::domain::Verdict::Refused(finding) => {
                    entry.finish().await.map_err(|refused| match refused {
                        crate::resolution::Refused::Unanswerable(error) => error,
                        crate::resolution::Refused::Finding(finding) => anyhow::anyhow!(finding.message),
                    })?;
                    return Ok(Connected::Refused { finding, findings });
                }
                crate::resolution::domain::Verdict::Unavailable(finding) => {
                    domain_vet_unavailable = true;
                    domain_code = Some(crate::engine::ConnectedCheck {
                        name: "declared-domain-code", engine: "PostgreSQL", status: "deferred",
                        message: "Scratch could not answer the advisory domain-code vet; apply vets CHECK and DEFAULT code in its transaction.".into(),
                    });
                    if let Some(finding) = finding {
                        // Keep the warning with the approved typed change as
                        // well as in the command's findings envelope.
                        pbps_diff::order::writebacks::findings(&mut cs, &[(0, finding.clone())]);
                        let finding = crate::policy_finding(&finding);
                        if finding.severity == crate::output::Severity::Error {
                            return Ok(Connected::Refused { finding, findings });
                        }
                        if !json { eprintln!("{}: {}", finding.severity_word(), finding.message); }
                        findings.push(finding);
                    }
                }
            }
        }
        // Case 3, last: the target is read again, after every read above,
        // and must still be what the resolver's run read; then the plan is
        // sealed in the order the passes above left (DEC-1516.1).
        let resolution = match resolution {
            None => None,
            Some((name, unsealed, asked)) => {
                // A session of its own, as the run's was: one target name
                // can reach another cluster on a new connection, which the
                // recheck must see and refuse (DEC-1685.1).
                let mut fresh = pbps_db::Conn::connect(conn.driver(), target.connection())
                    .await
                    .with_context(|| format!("cannot reach `{}` for the recheck", pbps_model::escape::Escaped(&target.label)))?;
                let rechecked = match unsealed.recheck(&mut fresh).await {
                    Ok(rechecked) => rechecked,
                    Err(refused) => {
                        let error = refused.error;
                        if !matches!(error, pbps_cli::resolver::server::Error::Changed(_)) {
                            return Err(error.into());
                        }
                        let finding =
                            crate::output::Finding::error("resolver.recheck", error.to_string())
                                .remedy("plan again");
                        return Ok(Connected::Refused { finding, findings });
                    }
                };
                let sealed = rechecked
                    .seal(cs.clone(), base_side, desired_side)
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "the resolver's evidence could not be sealed over the final plan: {e}; \
                         this is a bug in pbps, please report it"
                        )
                    })?;
                resolver_assessment
                    .as_mut()
                    .expect("a successful resolver run has an assessment")
                    .record_authorization(sealed.evidence.qualification().authorization);
                cs = sealed.changes;
                Some(Resolution {
                    name,
                    asked,
                    evidence: sealed.evidence,
                })
            }
        };
        let declared_type_pins = pbps_pg::type_refs::declared::seal(&read_back, &recorded_ids, &declared, &resolved.ids);
        let domain_validation =
            crate::engine::check_domain_alterations(&mut conn, &cs, &recorded_ids).await?;
        // The rehearsal, last: it runs the plan as it will be written, on the
        // scratch the resolver used or would have used (#2379).
        let course = crate::rehearsal::connected::course(
            // An unavailable advisory scratch must not become a fatal retry
            // through the independent rehearsal (DEC-2309.1).
            scratch.is_some() && !domain_vet_unavailable,
            crate::rehearsal::connected::switched_off(project, target.environment(), no_rehearse),
        );
        use crate::rehearsal::connected::Course;
        // A create runs code pbps cannot read, so its members are read from a
        // rehearsal and pinned: without one there is nothing to pin (DEC-2447.6).
        if course != Course::Rehearse && let Some((name, done)) = changed_extensions(&cs).next() {
            anyhow::bail!(
                "extension `{}` is {done} by this plan, which runs the package's script; its members are read from a rehearsal on the resolver scratch and pinned into the plan, and this plan is not rehearsed ({}). Select a resolver scratch entry and do not turn the rehearsal off (`rehearse: false`, `--no-rehearse`)",
                name.shown(),
                match course {
                    Course::Off => "the rehearsal is turned off",
                    Course::Skip | Course::Rehearse => "no scratch is available to rehearse on",
                }
            );
        }
        let mut manifests = crate::rehearsal::connected::Manifests::new();
        let rehearsal = match (course, scratch.as_mut()) {
            (Course::Off, _) => Some(pbps_model::Rehearsal::Off),
            (Course::Rehearse, Some(entry)) => {
                let ordered = cs.clone().ordered(plan_mode(staged), dialect.as_ref());
                let lowered = crate::statements(&ordered, &dialect)?;
                let mut base_hints = hints.clone();
                base_hints.module_deps.extend(recorded_module_deps.clone());
                // The same refusal the plan meets below, before a rehearsal
                // can meet the statement instead.
                if !staged {
                    reject_non_transactional(&lowered)?;
                }
                let verdict = crate::rehearsal::connected::run(
                    entry,
                    &mut conn,
                    target,
                    &crate::rehearsal::connected::Plan {
                        dialect: &dialect,
                        base: base_side,
                        desired: desired_side,
                        declared: &loaded.schema,
                        hints: &hints,
                        base_hints: &base_hints,
                        dependencies: &loaded.dependencies,
                        ordered: &ordered,
                        operations: &pbps_dialect::PlanPayload::operations(&ordered),
                        statements: &lowered,
                        staged,
                    },
                )
                .await;
                // Confirmed gone before the plan goes on or is refused.
                let released = entry.finish().await;
                let verdict = verdict?;
                if let Err(crate::resolution::Refused::Unanswerable(error)) = released {
                    return Err(error);
                }
                match verdict {
                    crate::rehearsal::connected::Verdict::Sealed(record, sealed) => {
                        manifests = sealed;
                        Some(record)
                    }
                    crate::rehearsal::connected::Verdict::Refused(finding) => {
                        return Ok(Connected::Refused { finding, findings });
                    }
                }
            }
            (Course::Rehearse | Course::Skip, _) => None,
        };
        if let Some(entry) = scratch.as_mut()
            && let Err(crate::resolution::Refused::Unanswerable(error)) = entry.finish().await
        {
            return Err(error);
        }
        let extension_changes = extension_change_pins(&mut conn, &cs, &manifests, &declared).await?;
        let extension_pins = extension_pins(&cs, manifests, &declared, &resolved.ids)?;
        let extension_drops = extension_drop_pins(&mut conn, &cs).await?;
        Ok(Connected::Ordinary(Box::new((
            cs,
            // The zone this session started in: what every offset-less
            // value above was read in, and what the apply pins (DEC-1916.1).
            (
                baseline,
                managed.database_collation.clone(),
                conn.session_zone().cloned(),
            ),
            format!("{} as queried (entry #{})", target.label, entry.id),
            vec![
                permission_support,
                generation_support,
                owned_targets,
                unrevocable_grants,
                drop_blockers,
                missing_roles,
                rename_evidence,
                dependents,
                rebuilds,
                logical_slots,
                fills,
            ]
            .into_iter()
            .chain(domain_validation)
            .chain(domain_code)
            .collect::<Vec<_>>(),
            findings,
            cost,
            routine_pins,
            type_pins,
            provider_pins,
            declared_type_pins,
            reviewed,
            resolver_assessment,
            resolution,
            (rehearsal, extension_pins, extension_drops, extension_changes),
        ))))
    }).await?;
        let (
            cs,
            (baseline_checksum, baseline_collation, baseline_zone),
            baseline_description,
            connected_checks,
            findings,
            cost,
            routine_pins,
            type_pins,
            provider_pins,
            declared_type_pins,
            reviewed,
            resolver_assessment,
            resolution,
            (rehearsal, extension_pins, extension_drops, extension_changes),
        ) = match connected {
            Connected::Ordinary(planned) => *planned,
            Connected::Refused {
                finding,
                mut findings,
            } => {
                if json {
                    findings.push(finding);
                    return Ok(services::PlanResult::Refused(crate::output::Report::plain(
                        "plan", findings,
                    )));
                }
                let report = crate::output::Report::plain("plan", vec![finding]);
                eprint!("{}", crate::output::human(&report.findings));
                return Ok(services::PlanResult::Refused(report));
            }
        };

        if staged && pbps_diff::order::row_security::requires_transaction(&cs) {
            bail!(
                "row-security changes require a transactional plan; --staged is unsupported (DEC-1173.3)"
            );
        }
        let statements = crate::statements(
            &cs.clone().ordered(plan_mode(staged), dialect.as_ref()),
            &dialect,
        )?;
        if staged {
            // A staged plan is one logical change isolated in a deployment of its
            // own (ADR-0003). The limit is the whole point: what cannot be rolled
            // back must not be able to take four unrelated changes down with it,
            // and a resume that had to reason about which of five changes were
            // half-done would be guessing.
            let logical = crate::logical_changes(&cs);
            if logical > 1 {
                bail!(
                    "--staged applies one logical change, and this plan has {logical}.\n\
                 Stage the change that needs it in a revision of its own; the rest can go \
                 through an ordinary transactional apply."
                );
            }
            if cs.is_empty() {
                bail!("there is nothing to stage: this plan is empty");
            }
        } else {
            // §7.5: a statement that cannot run inside a transaction fails **here**,
            // not halfway through an apply with no way back.
            reject_non_transactional(&statements)?;
        }

        let names = pbps_model::plan_names::PlanNames::of(&cs.changes, &resolved.ids)?;
        let (tables, modules) = crate::report::touched(&names);
        let data = crate::PlanData {
            dependencies: dependencies.clone(),
            baseline: baseline_description.clone(),
            time_zone: baseline_zone.as_ref().map(ToString::to_string),
            changes: cs.changes.len(),
            tables,
            modules,
            roles: crate::report::touched_roles(&names),
            risks: cs.risks().iter().map(|risk| risk.as_str()).collect(),
            connected_checks: {
                let mut checks = connected_checks;
                if !checks.iter().any(|check| check.name == "declared-domain-code") && declared_type_pins.iter().any(|pin| matches!(&pin.definition, pbps_model::ManagedType::Domain { default, checks, .. } if default.is_some() || !checks.is_empty())) {
                    checks.push(crate::engine::ConnectedCheck {
                        name: "declared-domain-code", engine: "PostgreSQL", status: "deferred",
                        message: "Declared domain checks and defaults are vetted inside apply, immediately after their type statements.".into(),
                    });
                }
                checks
            },
            type_routines: reviewed.0.clone(),
            type_routine_approval: (!reviewed.0.is_empty())
                .then(|| crate::pins::TYPE_ROUTINE_APPROVAL.to_owned()),
            origins: crate::report::origins(&cs),
            cost: Some(cost),
            resolver_selection,
            resolver_assessment,
            rehearsal: rehearsal.as_ref().map(crate::explain::rehearsal_detail),
            target_assertions: target_assertions.clone(),
        };
        if !json {
            print!("{}", crate::dependencies::render(&dependencies));
            println!("Baseline: {baseline_description}");
            if !target_assertions.is_empty() {
                // Said where the approver reads it: this is not a check apply repeats.
                println!(
                    "Target assertions: {} passed while planning; apply does not run them.",
                    target_assertions.len()
                );
            }
            if let Some(zone) = &baseline_zone {
                // Named because nothing in a statement shows it: the same
                // `2026-02-01` is another instant in another zone (DEC-1916.1).
                println!("Offset-less date/time values read in {zone}");
            }
            // Case 1 has no resolver to report and claims no verification.
            if let Some(resolution) = &resolution {
                println!(
                    "Resolver profile: {}; it answered the binding questions of {}, and the \
                 target was rechecked after its run.",
                    resolution.name, resolution.asked
                );
                if let Some(notice) = data
                    .resolver_assessment
                    .as_ref()
                    .and_then(|a| a.authorization_notice)
                {
                    println!("Notice: {notice}");
                }
            } else if let (Some(selection), Some(assessment)) =
                (&data.resolver_selection, &data.resolver_assessment)
            {
                println!(
                    "Resolver profile: {} ({:?}); not needed for binding, so the resolver did not run: this plan moves no name a kept binding could reach ({} unaffected, {} rebuilt from their declarations).",
                    selection.name, selection.source, assessment.unaffected, assessment.rebuilt,
                );
            }
            if let Some(detail) = &data.rehearsal {
                println!("Rehearsal: {}", crate::explain::rehearsal_line(detail));
                print!("{}", crate::explain::rehearsal_render(detail));
            }
            print!("{}", crate::report::plan(&cs, &names));
            if let Some(note) = findings.iter().find(|f| f.id == "plan.undeclared-comments") {
                println!("{}", note.message);
            }
            for check in &data.connected_checks {
                println!("{}: {}", check.name, check.message);
            }
            if let Some(cost) = &data.cost {
                print!("{}", crate::cost::render(cost));
            }
        }

        let mut plan = pbps_model::SavedPlan::new(
            pbps_model::PlanOrigin::Database,
            dialect.name(),
            created_at,
            pbps_model::PlanBaseline {
                description: baseline_description.as_str().into(),
                checksum: baseline_checksum,
                database_collation: baseline_collation,
                time_zone: baseline_zone,
            },
            cs.changes().clone(),
            resolved.ids,
        );
        plan.module_deps = loaded.hints.module_deps.clone();
        plan.dependencies = dependencies;
        plan.git_sha = db::git_sha(project.root());
        // The rows the state recorded after the apply has to cover: the
        // declarations' scope, carried so that `apply` needs no checkout.
        plan.data = loaded.schema.data_scopes();
        plan.routine_pins = routine_pins;
        plan.declared_type_pins = declared_type_pins;
        plan.type_pins = type_pins;
        plan.provider_pins = provider_pins;
        plan.pin_type_routines = project.config.pin_type_routines;
        (plan.type_routines, plan.type_routine_pins) = reviewed;
        if !json {
            print!("{}", crate::pins::reviewed_sources(&plan.type_routines));
        }
        plan.trusted_roles = project.config.trusted_roles.clone();
        plan.rehearsal = rehearsal;
        plan.extension_pins = extension_pins;
        plan.extension_drops = extension_drops;
        plan.target_assertions = target_assertions;
        plan.extension_changes = extension_changes;
        // A type the plan expects its own package to bring must be one the
        // rehearsal found in that package (DEC-2447.12).
        pbps_pg::type_refs::provider_proof(
            &plan.type_pins,
            &plan.extension_pins,
            &plan.extension_changes,
        )
        .map_err(anyhow::Error::msg)?;
        pbps_pg::class_pins::proof(
            &plan.provider_pins,
            &plan.extension_pins,
            &plan.extension_changes,
        )
        .map_err(anyhow::Error::msg)?;
        if let Some(resolution) = resolution {
            plan = plan.with_resolution(resolution.evidence).map_err(|e| {
            anyhow::anyhow!(
                "the resolver's evidence does not describe this plan: {e}; this is a bug in pbps, \
                 please report it"
            )
        })?;
        }
        if staged {
            plan = plan.staged();
            if !json {
                println!(
                    "\nThis is a staged plan: {} statement(s) will run outside a transaction, each \n\
             recorded in the ledger as it completes. Apply it with `pbps apply --staged \n\
             --checksum {}`, and continue an interrupted run with \n\
             `--staged --resume`.",
                    statements.len(),
                    crate::report::placeholder("approved-checksum")
                );
            }
        }

        let script = script.then(|| -> anyhow::Result<String> {
        Ok(format!(
            "-- Generated by pbps against {}\n-- Applyable only through `pbps apply --plan`; never hand-edit this file.\n\n{}",
            plan.baseline.description.shown(),
            statements.render(
                // The zone the plan recorded: the script is the plan's, and
                // runs where the plan was computed (DEC-1916.1).
                plan.baseline
                    .time_zone
                    .as_ref()
                    .unwrap_or(&SessionZone::utc()),
            )?
        ))
    }).transpose()?;
        let plan = pbps_diff::order::seal(cs.ordered(plan.mode, dialect.as_ref()), plan)?;
        Ok(services::PlanResult::Planned(Box::new(services::Planned {
            artifact: Box::new(plan),
            script,
            statements: statements.len(),
            data,
            findings,
        })))
    })
}

/// What the connected half of `plan` ended with (DEC-1515.1).
enum Connected<T> {
    /// The plan and everything checked against it: the ordinary plan
    /// (cases 1 and 2), or the resolver's, rechecked and sealed (case 3).
    Ordinary(Box<T>),
    /// Case 3, refused by an answer: unresolved, or measured incompatible.
    Refused {
        finding: crate::output::Finding,
        findings: Vec<crate::output::Finding>,
    },
}

/// Case 3's sealed answer: the resolver that gave it, the binding questions
/// it answered, and the evidence the plan carries (#1516).
struct Resolution {
    name: String,
    asked: String,
    evidence: pbps_model::resolver::ResolverEvidence,
}

/// What the operator asked `apply` to do, as typed.
pub struct ApplyRequest<'a> {
    pub plan_path: &'a std::path::Path,
    pub approved_checksum: &'a str,
    pub allow: &'a std::collections::BTreeSet<pbps_model::RiskClass>,
    pub staged: bool,
    pub resume: bool,
}

/// `pbps apply` — run an approved plan (SPEC §7.3, §7.5).
pub fn cmd_apply(
    project: &Project,
    target: &Target,
    request: &ApplyRequest<'_>,
) -> anyhow::Result<()> {
    let operator = crate::operator(project.root())?;
    let dialect = crate::dialect(project)?;
    let plan_path = request.plan_path;

    // Before this point there is no artifact to report on: the attempt event
    // carries a checksum, and a file that cannot be read or is not a plan has
    // none. From `plan_checksum` on, every outcome is an attempt on this
    // artifact against this environment, and the hook sees all of them.
    let raw = std::fs::read_to_string(plan_path)
        .with_context(|| format!("cannot read `{}`", plan_path.display()))?;
    let plan = crate::saved_plan::decode(&raw, plan_path)?;
    let plan_checksum = plan.wire_fingerprint();

    let attempt = apply_identified(
        project,
        target,
        request,
        &plan,
        &plan_checksum,
        &dialect,
        &operator,
    );
    let (error, released) = match attempt {
        Ok(Attempt::Empty) => {
            println!("The plan is empty; nothing to apply.");
            return Ok(());
        }
        Ok(Attempt::Applied { entry, released }) => {
            println!(
                "Applied {} change(s) to `{}`{}; recorded as entry #{entry}.",
                plan.changes.changes.len(),
                pbps_model::escape::Escaped(&target.label),
                if request.staged { " (staged)" } else { "" }
            );
            // Preserve the original public hook contract: successful applies
            // receive the exact approved plan JSON. Attempt events use their
            // own key so existing success-only integrations cannot be invoked
            // on a failure or misread an unrelated payload shape.
            if let Some(hook) = &project.config.hooks.on_apply {
                crate::hooks::run(hook, &raw, "on_apply", crate::hooks::HookStdout::Shared);
            }
            if let Some(hook) = &project.config.hooks.on_apply_attempt {
                crate::hooks::run_apply_attempt(
                    hook,
                    plan_path,
                    &plan_checksum,
                    &target.label,
                    Some(entry),
                    None,
                );
            }
            // Report the deployment before surfacing cleanup. The lock may
            // need an operator to clear it, but retrying this already-recorded
            // plan would be the wrong response.
            released?;
            return Ok(());
        }
        Ok(Attempt::Failed { error, released }) => (error, Some(released)),
        // Refused before the lock was taken, or the connection or the lock
        // itself failed: still an attempt on this artifact, with no lock to
        // clean up.
        Err(error) => (error, None),
    };
    if let Some(hook) = &project.config.hooks.on_apply_attempt {
        // `ledger_safe_reason`, not `error.to_string()`: the hook's own doc
        // comment calls this payload "stable input" a `pbps.yml` author wires
        // to an arbitrary shell command, which can relay it anywhere — a
        // wider and less accountable audience than the operator's own
        // terminal, and exactly the "emitted outward" case DECISIONS 455
        // exists for, beside the ledger it names directly.
        let message = crate::engine::ledger_safe_reason(&error);
        crate::hooks::run_apply_attempt(
            hook,
            plan_path,
            &plan_checksum,
            &target.label,
            None,
            Some(&message),
        );
    }
    // After the hook, so the payload it receives is the deployment's error
    // alone; before the return, because a lock this run could not clear is
    // the first thing the next run will hit.
    if let Some(released) = &released {
        warn_unreleased(&target.label, released);
    }
    Err(error)
}

/// Best-effort audit row for an apply that did not complete. The schema and ids
/// come from the newest trustworthy checkpoint; for a transactional apply that
/// is the unchanged pre-plan state, and for a staged apply it is the last
/// completed statement. A failure to write the audit must never replace the
/// original deployment error.
async fn record_failed_apply<D: crate::emission::Engine>(
    conn: &mut Conn,
    d: &Deployment<'_, D>,
    error: &anyhow::Error,
) {
    let current = match crate::engine::latest(conn).await {
        Ok(Some(entry)) => entry.snapshot,
        Ok(None) | Err(pbps_db::LedgerError::NotInitialized) => return,
        Err(audit_error) => {
            eprintln!("warning: the failed apply could not be added to the ledger: {audit_error}");
            return;
        }
    };
    let failed = failed_apply_snapshot(
        conn.driver(),
        current,
        d.operator,
        d.plan
            .git_sha
            .clone()
            .or_else(|| db::git_sha(d.project.root())),
        d.plan_checksum,
        error,
    );
    match crate::engine::record(conn, &failed).await {
        Ok(id) => eprintln!("Apply failure recorded as ledger entry #{id}."),
        Err(audit_error) => {
            eprintln!("warning: the failed apply could not be added to the ledger: {audit_error}")
        }
    }
}

/// Turns the newest trustworthy state into a failed-attempt audit row.
///
/// An unfinished staged state belongs to the plan that produced its progress.
/// A later attempt with another plan may fail before running anything, but it
/// must not relabel that checkpoint with the attempted plan's checksum or git
/// revision: doing so could let the next `--resume` skip statements from the
/// wrong artifact. The operator and reason still describe the failed attempt.
///
/// `reason` is [`crate::engine::ledger_safe_reason`], not `error.to_string()`:
/// this row is durable — SPEC's own audit trail — and a driver's own sentence
/// can name a value nobody declared to this tool (DECISIONS 455). The
/// operator still sees the whole thing; only what gets written down is cut.
fn failed_apply_snapshot(
    driver: pbps_db::Driver,
    mut current: StateSnapshot,
    operator: &str,
    attempted_git_sha: Option<pbps_model::Text>,
    attempted_plan_checksum: &str,
    error: &anyhow::Error,
) -> StateSnapshot {
    let is_staged_checkpoint = current.staged.is_some();
    current.kind = StateKind::Failed;
    current.data_work = None;
    // The newest row's assertions passed for the plan that wrote it; this row
    // records a failed attempt and must not attribute them to another plan.
    current.target_assertions.clear();
    current.operator = operator.to_owned();
    if !is_staged_checkpoint {
        current.git_sha = attempted_git_sha;
        current.plan_checksum = Some(attempted_plan_checksum.to_owned());
    }
    current.reason = Some(
        crate::engine::truncate_reason(driver, &crate::engine::ledger_safe_reason(error)).into(),
    );
    current
}

/// Everything one `apply` carries from its command body into the locked section.
///
/// Bundled rather than passed positionally because two of these are `&str` —
/// the plan's checksum and the operator's name — and they were adjacent
/// arguments to both functions below. Transposing them compiles, and what it
/// produces is a ledger entry whose `plan_checksum` is a username: the pin that
/// `apply --plan` exists to enforce, silently recording the wrong thing.
///
/// A struct does not make that *unrepresentable* — both fields are still
/// `&str`, and `plan_checksum: &operator` would still compile. What it does is
/// put the field name beside the value at the one call site, so the mistake has
/// to be written down rather than counted to. Nine positional arguments is a
/// place where it can be made by counting, which is what the two
/// `#[allow(clippy::too_many_arguments)]` this replaces were saying.
struct Deployment<'a, D: crate::emission::Engine> {
    project: &'a Project,
    target: &'a Target,
    plan: &'a pbps_diff::order::AdmittedPlan<D>,
    plan_checksum: &'a str,
    statements: &'a crate::emission::LoweredPlan,
    rename_targets: &'a [pbps_db::impact::RenameTarget],
    dialect: &'a D,
    operator: &'a str,
    progress: Option<&'a services::Progress>,
}

/// Everything between taking the lock and releasing it. Returns the ledger id.
/// `resolved` holds a resolved plan's checks, its key and cluster already
/// passed (#616).
async fn apply_under_lock<D: crate::emission::Engine>(
    conn: &mut Conn,
    d: &Deployment<'_, D>,
    resolved: Option<&pbps_cli::resolver::apply::Guard<'_>>,
) -> anyhow::Result<Option<i64>> {
    // Destructured straight back into the names the body below already uses:
    // every field is a shared reference, so this copies nothing.
    let Deployment {
        project,
        target,
        plan,
        plan_checksum,
        statements,
        rename_targets,
        dialect,
        operator,
        progress,
    } = *d;
    let Some(mut entry) = crate::engine::latest(conn).await? else {
        bail!(
            "`{}` has a ledger but no entries; a plan cannot be pinned to a state that was never recorded.",
            pbps_model::escape::Escaped(&target.label)
        );
    };
    // First, before anything is read: in another zone every offset-less
    // value reads back as other text, so the drift check below would report
    // the zone as a change nobody made (DEC-1916.1).
    refuse_moved_time_zone(conn, plan, &target.label)?;
    refuse_mid_deployment(&entry, &target.label)?;
    // Under the lock, before anything runs: a table made at a history's name
    // since the plan was computed would be adopted, not refused (#1176).
    refuse_taken_history_names(conn, &plan.changes, &target.label).await?;
    // Nothing binds a saved plan to a server: asked again here (#1502).
    crate::engine::refuse_unsupported_temporal(conn, &plan.changes).await?;
    let original_ids = entry.snapshot.ids.clone();
    pbps_diff::drop_requirements::validate(&plan.changes, &original_ids, &plan.ids)
        .map_err(anyhow::Error::msg)?;
    let role_renames = crate::engine::external_role_renames(conn, &original_ids, &plan.ids)
        .await?
        .renames;
    let pinned_managed = pinned_managed(&entry.snapshot, &plan.changes, &plan.ids);
    if plan.changes.is_empty()
        && original_ids.roles == plan.ids.roles
        && original_ids.types == plan.ids.types
        && original_ids.extensions == plan.ids.extensions
    {
        // No statements still means replay of the approved type prerequisites
        // (DEC-2181.1). Reference-free no-ops keep their existing fast path.
        if !pinned_managed.references.is_empty()
            || !pinned_managed.declared_types.is_empty()
            || !plan.type_pins.is_empty()
            || !plan.provider_pins.is_empty()
            || !plan.declared_type_pins.is_empty()
        {
            crate::pins::check(
                conn,
                project,
                target,
                plan,
                &pinned_managed,
                crate::pins::Check::BeforeProbes,
            )
            .await?;
        }
        return Ok(None);
    }
    rename_snapshot_roles(&mut entry.snapshot, &role_renames);
    let externals = external_uids(&entry.snapshot, &plan.changes);
    let recorded_ids = role_scope(&entry.snapshot.ids, &plan.ids, dialect);
    let recorded_modules = managed_modules(Some(&entry.snapshot), None);
    // The scopes the closing read-back will use, expressed in the names the
    // database has *now* — the second projection is compared against that
    // read, so it has to ask it the same question.
    let planned_scopes = scopes_under(&plan.data, &plan.ids, &entry.snapshot.ids);
    // The database holds the labels it has now, which a rename of this plan
    // changes: the baseline is read under both (DEC-2346.2).
    let held_scopes = held_before_the_plan(
        plan,
        &entry.snapshot.schema,
        &entry.snapshot.ids,
        &planned_scopes,
    );
    let (scoped, mut before, unrevocable) = baseline_state(
        conn,
        &recorded_ids,
        &recorded_modules,
        &externals_under(&externals, &recorded_ids),
        target.catalog_policy(project.config.unmanaged),
        &pinned_baseline_scopes(
            &entry.snapshot.schema.data_scopes(),
            &planned_scopes,
            &plan.changes,
            &plan.ids,
            (&entry.snapshot.schema, &entry.snapshot.ids),
        ),
        &entry.snapshot.schema,
        &held_scopes,
        CatalogWarnings::for_progress(d.progress),
    )
    .await?;
    retain_managed_roles(&mut before, &plan.ids, dialect);
    trim_to_the_planned_scope(plan, &entry.snapshot.ids, &planned_scopes, &mut before);

    // The drift check, and the whole reason a coarse `--allow` is safe: this
    // plan is only valid against the environment it was computed against, down
    // to the identity mapping.
    let live = pbps_model::drift_checksum(&scoped.schema, &recorded_ids);
    if live != plan.baseline.checksum {
        bail!(
            "`{}` is no longer the database this plan was computed against.\n\
             plan baseline: {}\n\
             database now:  {live}\n\
             Something changed since the plan was approved. `pbps verify` shows what; \
             then recompute the plan with `pbps plan --db`.",
            pbps_model::escape::Escaped(&target.label),
            plan.baseline.checksum
        );
    }
    refuse_moved_database_collation(conn, plan, &target.label).await?;
    crate::row_security::check_dropped_policies(&plan.changes, &plan.ids, &scoped.schema)?;

    // And the same guard the recording commands use (110): a permission the
    // declarations cannot hold is invisible to the checksum above, so it
    // reaches here as a clean baseline, and the closing snapshot would write
    // the environment down without it. Refused before statement one, where a
    // refusal still costs nothing.
    refuse_unexpressible(&scoped, &target.label, "apply again")?;
    // Ownership is beside the checksum too (DECISIONS 371), so an `ALTER ...
    // OWNER TO` the grantee after planning reaches here as a clean baseline.
    // The same question `plan` asked, of the database as it stands now
    // (#692): a grant to a target's owner is a no-op no `REVOKE` undoes.
    crate::engine::owned_targets(
        conn.driver(),
        &plan.changes,
        &plan.ids,
        &scoped.owners,
        &scoped.session_role,
    )?;
    // And whether *this* connection can take back what the plan revokes,
    // which `plan` asked of the connection it planned over (#1057). A plan
    // made as the owner and applied as a least-privilege deployer otherwise
    // passes the baseline and fails at its `REVOKE`, after earlier statements
    // of a staged run are already committed.
    crate::engine::unrevocable_grants(conn.driver(), &plan.changes, &plan.ids, &unrevocable)?;

    // Before the probes, which run declared expressions and so the routines
    // they call (DEC-319.1). Every module the plan holds at any point of its
    // run is managed here, so its own creates, rebuilds and drops are the
    // read-back's to compare, not the pins'.
    crate::pins::check(
        conn,
        project,
        target,
        plan,
        &pinned_managed,
        crate::pins::Check::BeforeProbes,
    )
    .await?;

    let approved = crate::pins::Approved {
        project,
        target,
        plan,
        managed: &pinned_managed,
    };
    preflight(
        conn,
        dialect,
        plan,
        rename_targets,
        &scoped.schema,
        &recorded_ids,
        &approved,
        progress,
    )
    .await?;

    // Recorded when the plan was made and never repeated here: apply
    // acquires no scratch (#2379).
    if let Some(record) = &plan.rehearsal {
        services::notice(
            progress,
            "apply.progress",
            format!(
                "Rehearsal, recorded in the plan: {}",
                crate::explain::rehearsal_line(&crate::explain::rehearsal_detail(record))
            ),
        );
    }
    services::notice(
        progress,
        "apply.progress",
        format!("Applying {} statement(s)...", statements.len()),
    );
    let result = async {
        conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
            .await?;
        let admission = approved.admission();
        crate::engine::rebuild_window(
            conn,
            &admission,
            pbps_diff::Side {
                schema: &entry.snapshot.schema,
                ids: &recorded_ids,
            },
            &plan.changes,
        )
        .await?;
        let retained = crate::engine::hold_retained(
            conn,
            &plan.changes,
            Some(&entry.snapshot.declared.overlay(&before)),
        )
        .await?;
        crate::engine::external_role_renames(conn, &original_ids, &plan.ids).await?;
        crate::engine::check_module_dependents(
            conn,
            plan,
            &scoped.schema,
            &plan.ids,
            &recorded_ids,
        )
        .await?;
        crate::engine::check_module_rebuilds(conn, &plan.changes, false).await?;
        recheck_enum_rebuilds(conn, &plan.changes, &recorded_ids, false).await?;
        recheck_domain_alterations(conn, &plan.changes, &recorded_ids).await?;
        recheck_extension_creates(conn, plan).await?;
        crate::engine::check_drop_blockers(
            conn,
            &plan.changes,
            &plan.ids,
            &scoped.schema,
            &recorded_ids,
        )
        .await?;
        let data_guard = crate::engine::prepare_data_writes(
            conn,
            &approved.admission(),
            &plan.changes,
            &entry.snapshot,
            &plan.ids,
        )
        .await?;
        // Again, because the probes ran in transactions of their own and a
        // routine could be replaced after them.
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            crate::pins::Check::BeforeStatements { executed: &[] },
        )
        .await?;
        // Last before the first statement, inside the transaction and under
        // the lock: the target is still the one the evidence was sealed
        // against (#616).
        if let Some(resolved) = resolved {
            resolved.before_statements(conn).await?;
        }
        let domain_defaults = pbps_pg::type_refs::declared::capture_defaults(
            conn,
            &plan.declared_type_pins,
            &plan.changes,
            pbps_pg::pins::Within::CallersTransaction,
        )
        .await?;
        execute_statements(conn, statements, &data_guard, &approved, progress).await?;
        crate::engine::check_module_rebuilds(conn, &plan.changes, true).await?;
        crate::engine::refuse_unsafe_definers(
            conn,
            &crate::engine::written_routines(&plan.changes),
            crate::engine::Absent::Refused,
        )
        .await?;
        crate::engine::external_role_renames(conn, &original_ids, &plan.ids).await?;

        // What gets recorded is the database read back, not the plan applied to
        // the old state. Expressions come back in the engine's stored form, and
        // only that form compares equal on the next drift check (SPEC §8.2).
        // The rows under the plan's own scope, for the same reason it carries
        // its ids.
        //
        // Read-back and ledger insertion both happen before the commit, so a
        // failure in either rolls the DDL back as well — and so that no commit
        // opens a window another session could write a declared row or a
        // managed role's grants into, for this read to take in and record as
        // the plan's own result (DECISIONS 147).
        let mut after = managed_state(
            conn,
            &plan.ids,
            &modules_after(&entry.snapshot, &plan.changes, Settled::Whole),
            &externals_under(&externals, &plan.ids),
            target.catalog_policy(project.config.unmanaged),
            &plan.data,
            &Schema::default(),
            crate::engine::Read::InsideOwnTransaction,
            CatalogWarnings::for_progress(d.progress),
        )
        .await?;
        // A deferred trigger runs at COMMIT unless explicitly settled. Compare
        // its effects inside this transaction, after every planned write: an
        // earlier flush can refuse a constraint that the full plan satisfies,
        // while a later one records rows that the commit then changes (473).
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            crate::pins::Check::BeforeRecording {
                executed: statements.statements(),
            },
        )
        .await
        .context("Nothing has been applied — the transaction was rolled back.")?;
        if crate::engine::settle_data_writes(conn).await? {
            let settled = managed_state(
                conn,
                &plan.ids,
                &modules_after(&entry.snapshot, &plan.changes, Settled::Whole),
                &externals_under(&externals, &plan.ids),
                target.catalog_policy(project.config.unmanaged),
                &plan.data,
                &Schema::default(),
                crate::engine::Read::InsideOwnTransaction,
                CatalogWarnings::for_progress(d.progress),
            )
            .await?;
            refuse_unexpressible(&settled, &target.label, "apply again")?;
            if settled.schema != after.schema {
                bail!(
                    "deferred constraint triggers changed the managed state after the plan's \
                     row checks. Nothing has been applied — the transaction was rolled back."
                );
            }
            after = settled;
        }
        refuse_recreated_tables(conn, &plan.changes, statements, &after.unmanaged)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "{e:#}\n\nNothing has been applied — the transaction was rolled back."
                )
            })?;
        // Everything this plan does not touch has to be what the baseline
        // held, down to the rows of a table it does touch that no change of
        // it names (DECISIONS 150, 153). The read above is what gets
        // recorded, so a change another session made between the two reads
        // would otherwise be blessed as this plan's result. And the same rule
        // the recording commands follow: a permission the declarations cannot
        // express stops a state being written down, whether it was there at
        // the start or arrived during the run (110).
        let verified =
            crate::engine::verify_retained(conn, retained.as_ref(), &after.schema).await?;
        refuse_pinned_movement(
            dialect,
            guard::Read::transactional(guard::Plan {
                changes: &plan.changes,
                ids: &plan.ids,
            })
            .with_retained(verified.as_ref())
            .with_domain_defaults(Some(&domain_defaults)),
            &before,
            &after.schema,
            &target.label,
        )
        .and_then(|()| refuse_unmet_public_execution(&plan.changes, &after, &target.label))
        .map_err(|e| {
            anyhow::anyhow!(
                "{e:#}\n\n\
                 Nothing has been applied — the transaction was rolled back. `pbps verify` \
                 shows what moved; then apply again."
            )
        })?;
        refuse_unexpressible(&after, &target.label, "apply again")?;
        // And a last time before recording: a routine replaced while the
        // statements ran was run by them, and the rollback is still available.
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            crate::pins::Check::BeforeRecording {
                executed: statements.statements(),
            },
        )
        .await
        .map_err(|e| {
            anyhow::anyhow!("{e:#}\n\nNothing has been applied — the transaction was rolled back.")
        })?;
        // And the closing bindings, after the managed read-back has held the
        // plan's own records to what it said (DEC-1274.1, #616).
        if let Some(resolved) = resolved {
            resolved
                .before_recording(conn, &after.schema, &plan.ids)
                .await?;
        }
        let mut snapshot = pbps_model::StateSnapshot::new(
            pbps_model::StateKind::Apply,
            after.schema,
            // The plan's mapping, not the baseline's: after a rename the two
            // disagree, and recording the old one would say the rename never
            // happened — leaving the next plan to propose it all over again.
            plan.ids.clone(),
            operator,
        );
        snapshot.baseline_evidence = after.baseline_evidence;
        snapshot.module_deps = plan.module_deps.clone();
        snapshot.git_sha = plan.git_sha.clone().or_else(|| db::git_sha(project.root()));
        snapshot.plan_checksum = Some(plan_checksum.to_owned());
        snapshot.target_assertions = plan.target_assertions.clone();
        // What the previous state declared, advanced by what this plan wrote:
        // from the plan alone, since `apply --plan` needs nothing else.
        snapshot.declared = entry.snapshot.declared.clone();
        snapshot
            .declared
            .advance_with_partitions(&plan.changes, &snapshot.schema);
        let id = crate::engine::record(conn, &snapshot).await?;
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            crate::pins::Check::BeforeCommit {
                executed: statements.statements(),
            },
        )
        .await
        .context("Nothing has been applied — the transaction was rolled back.")?;
        Ok::<_, anyhow::Error>(id)
    }
    .await;
    finish_transaction(conn, dialect, result).await.map(Some)
}

/// Every module the plan holds at some point of its run: what the ledger
/// recorded, and what the plan leaves or passes through.
///
/// The routine pins leave these to the drift check and the read-back
/// (DEC-319.1). The union is what makes the set stable across a run: a routine
/// the plan creates is in it before it exists, and one it drops stays in it
/// after it is gone, so neither reads as an unmanaged routine arriving or
/// leaving.
fn pinned_managed(
    recorded: &StateSnapshot,
    changes: &pbps_model::ChangeSet,
    desired: &IdsFile,
) -> crate::pins::Managed {
    let approved_base = pbps_diff::identity::type_refs::before_changes(desired, changes);
    let base_ids = type_scope(&recorded.ids, &approved_base);
    let mut set = managed_modules(Some(recorded), None);
    set.extend(modules_after(recorded, changes, Settled::SoFar));
    // Existing-column operations carry no type. A missing saved pin must
    // still refuse, rather than erase a baseline reference from admission.
    let mut references = crate::pins::external_references(&recorded.schema, &base_ids);
    // A column a staged rebuild has already moved holds the replacement, which
    // is the declared type's own until the swap names it back (DEC-1635.7).
    for planned in &changes.changes {
        if let pbps_model::Change::EnumRebuild { tmp, .. } = &planned.change {
            references.remove(tmp);
        }
    }
    crate::pins::Managed {
        modules: set,
        references,
        declared_types: crate::pins::declared_requirements(&recorded.schema, &base_ids),
    }
}

/// The cluster already moved these identities. Both the planner and the
/// checkout-free apply must scope and pin the same names (DECISIONS 377).
fn rename_snapshot_roles(
    snapshot: &mut StateSnapshot,
    renames: &BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
) {
    for (from, to) in renames {
        snapshot.ids.rename_role(from, to);
        snapshot.schema.rename_role(from, to);
    }
}

/// Incoming type declarations belong in the queried and pinned baseline even
/// before their UIDs have reached the ledger. Existing names keep their
/// baseline UID; only their post-apply identity changes when a UID is replaced.
fn type_scope(recorded: &IdsFile, declared: &IdsFile) -> IdsFile {
    let mut scoped = recorded.clone();
    for (uid, name) in &declared.types {
        if !scoped.types.contains_key(uid)
            && matches!(
                TypeResolver::new(&scoped, declared).resolve(TypeSide::Base, name),
                TypeIdentity::ExternalOrUnrecorded(_)
            )
        {
            scoped.types.insert(uid.clone(), name.clone());
        }
    }
    scoped
}

fn role_scope(recorded: &IdsFile, declared: &IdsFile, dialect: &dyn Dialect) -> IdsFile {
    let mut scoped = type_scope(recorded, declared);
    if !dialect.manages_roles() {
        for (uid, name) in &declared.roles {
            if !scoped.roles.values().any(|old| old == name) {
                scoped.roles.insert(uid.clone(), name.clone());
            }
        }
    }
    scoped
}

/// A role removed from a PostgreSQL declaration becomes unmanaged, rather
/// than being dropped from the cluster. Closing comparisons use that scope.
fn retain_managed_roles(schema: &mut Schema, ids: &IdsFile, dialect: &dyn Dialect) {
    if !dialect.manages_roles() {
        schema
            .roles
            .retain(|name, _| ids.roles.values().any(|kept| kept == name));
    }
}

/// A staged apply: one logical change, run statement by statement outside a
/// transaction, with each completion recorded (ADR-0003 decision 2).
///
/// # Why the ledger is written between statements
///
/// Nothing here rolls back — that is the whole reason the plan is staged. So
/// the only thing that can make a mid-way failure visible rather than
/// mysterious is a record written as each statement completes; and once that
/// record exists, `--resume` has somewhere honest to start from.
///
/// # Why resume re-checks the database
///
/// A checkpoint says what the database looked like when the run stopped. Half a
/// deployment sitting in an environment is exactly when somebody reaches in by
/// hand, so the drift discipline applies to a half-finished plan as much as to
/// a finished one: the live state has to still equal the checkpoint, or the
/// remaining statements are being run against something nobody planned for.
/// The census again, where the plan runs: a dependent that appeared on an
/// enum after the plan was made is refused by name before the first statement
/// (DEC-1635.7). Inside the apply's own transaction, after the lock.
async fn recheck_enum_rebuilds(
    conn: &mut Conn,
    changes: &pbps_model::ChangeSet,
    stored: &IdsFile,
    resuming: bool,
) -> anyhow::Result<()> {
    if conn.driver() != pbps_db::Driver::Postgres
        || !changes
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::EnumRebuild { .. }))
    {
        return Ok(());
    }
    pbps_pg::type_rebuild::census(conn, changes, None, stored, resuming)
        .await
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// The tightened domains again, where the plan runs: a column of an array or
/// a composite of the domain that appeared after the plan was made is refused
/// by name before the first statement (DEC-1635.8).
async fn recheck_domain_alterations(
    conn: &mut Conn,
    changes: &pbps_model::ChangeSet,
    stored: &IdsFile,
) -> anyhow::Result<()> {
    crate::engine::check_domain_alterations(conn, changes, stored)
        .await
        .map(|_| ())
}

async fn apply_staged_under_lock<D: crate::emission::Engine>(
    conn: &mut Conn,
    d: &Deployment<'_, D>,
    resume: bool,
) -> anyhow::Result<i64> {
    let Deployment {
        project,
        target,
        plan,
        plan_checksum,
        statements,
        rename_targets,
        dialect,
        operator,
        progress: _,
    } = *d;
    let staged = guard::Staged::new(guard::Plan {
        changes: &plan.changes,
        ids: &plan.ids,
    })?;
    let Some(mut entry) = crate::engine::latest(conn).await? else {
        bail!(
            "`{}` has a ledger but no entries; a plan cannot be pinned to a state that was never recorded.",
            pbps_model::escape::Escaped(&target.label)
        );
    };

    let original_ids = entry.snapshot.ids.clone();
    pbps_diff::drop_requirements::validate(&plan.changes, &original_ids, &plan.ids)
        .map_err(anyhow::Error::msg)?;
    let role_renames = crate::engine::external_role_renames(conn, &original_ids, &plan.ids)
        .await?
        .renames;
    rename_snapshot_roles(&mut entry.snapshot, &role_renames);
    let externals = external_uids(&entry.snapshot, &plan.changes);

    // Both branches read once and hand back what they validated: the
    // statement to start at, and the state the checkpoints are measured
    // against. Returned together so there is no way to reach the loop with a
    // baseline from some other read (DECISIONS 174).
    // A fresh run asks what a transactional one does; a resumed one created
    // the history itself, before its checkpoint (DEC-1176.1).
    // First, for a fresh run and a resumed one alike, as in
    // [`apply_under_lock`] (DEC-1916.1).
    refuse_moved_time_zone(conn, plan, &target.label)?;
    if !resume {
        refuse_taken_history_names(conn, &plan.changes, &target.label).await?;
        // Nothing binds a saved plan to a server: asked again here (#1502).
        crate::engine::refuse_unsupported_temporal(conn, &plan.changes).await?;
    }
    let (start, mut previous) = if resume {
        let progress = match (&entry.snapshot.staged, entry.snapshot.kind) {
            (Some(p), StateKind::Staged | StateKind::Failed) => p.clone(),
            _ => bail!(
                "`{}` has no staged apply in progress: its newest entry (#{}) is an ordinary \
                 {} state.\n\
                 Run `pbps apply --staged` without `--resume` to start this plan.",
                pbps_model::escape::Escaped(&target.label),
                entry.id,
                entry.snapshot.kind
            ),
        };
        if entry.snapshot.plan_checksum.as_deref() != Some(plan_checksum) {
            bail!(
                "entry #{} is a checkpoint of a different plan (checksum {}).\n\
                 Resume the plan that was interrupted, not this one.",
                entry.id,
                entry.snapshot.plan_checksum.as_deref().unwrap_or("none")
            );
        }
        // The drift check, applied to a half-finished plan. The mapping is the
        // checkpoint's own — which names every managed table as the catalog had
        // it at that moment, intermediate rename names included. Using the
        // plan's here instead would leave a table that is mid-rename out of both
        // sides, and a change someone made to it while the deployment was
        // paused would pass unseen into the closing entry.
        let after_modules = modules_after(&entry.snapshot, &plan.changes, Settled::SoFar);
        let at_checkpoint = &entry.snapshot.ids;
        // The state the checkpoints to come are measured against comes out of
        // *this* read — the one the comparison below validates — and not out
        // of a second one taken after these checks (DECISIONS 174).
        let (scoped, mut watching, unrevocable) = staged_baseline(
            conn,
            at_checkpoint,
            &after_modules,
            &after_modules,
            &externals_under(&externals, at_checkpoint),
            target.catalog_policy(project.config.unmanaged),
            &entry.snapshot.schema.data_scopes(),
            &entry.snapshot.schema,
            &held_before_the_plan(
                plan,
                &entry.snapshot.schema,
                at_checkpoint,
                &scopes_at(plan, at_checkpoint),
            ),
            CatalogWarnings::for_progress(d.progress),
        )
        .await?;

        trim_to_the_planned_scope(
            plan,
            at_checkpoint,
            &scopes_at(plan, at_checkpoint),
            &mut watching,
        );
        let live = pbps_model::drift_checksum(&scoped.schema, at_checkpoint);
        let checkpoint = pbps_model::drift_checksum(&entry.snapshot.schema, at_checkpoint);
        if live != checkpoint {
            bail!(
                "`{}` has moved since the checkpoint at entry #{}.\n\
                 checkpoint: {checkpoint}\n\
                 database:   {live}\n\
                 Either something changed while this deployment was half-finished, or the \
                 statement after the checkpoint committed and the checkpoint for it could not \
                 be written (a lost connection between the two). `pbps verify` shows which: if \
                 the difference is exactly the next statement of this plan, that statement is \
                 already done. Resuming cannot decide that for you — the remaining statements \
                 must not run against a database nobody planned for — so take the database as \
                 it stands with `pbps baseline --reason ...` and plan the rest from there.",
                pbps_model::escape::Escaped(&target.label),
                entry.id
            );
        }
        // Nor the database's default collation, which the plan pinned and
        // the checkpoint's checksum cannot see move (#1247 review): a
        // statement still to run that leaves a column with no collation
        // would take the new one.
        refuse_moved_database_collation(conn, plan, &target.label).await?;
        // Nor can it see a permission the declarations cannot hold, for the
        // same reason: it is beside the comparison, not in it (110). A
        // resume that ran on would close the deployment with a snapshot
        // written without it.
        refuse_unexpressible(&scoped, &target.label, "resume again")?;
        // And ownership, which is beside the checksum for the same reason
        // (#692), and whether this connection can revoke what is left to
        // revoke (#1057).
        crate::engine::owned_targets(
            conn.driver(),
            &plan.changes,
            &plan.ids,
            &scoped.owners,
            &scoped.session_role,
        )?;
        crate::engine::unrevocable_grants(conn.driver(), &plan.changes, &plan.ids, &unrevocable)?;
        // The role a `DROP ROLE` will meet is the role as the plan left it:
        // the members whose `DROP MEMBER` has not run yet, and nothing
        // owned. Membership and ownership are outside the checksum on
        // purpose, so the check above cannot see a member added while the
        // deployment was paused (DECISIONS 102).
        check_role_drops(
            conn,
            &role_drop_expectations(plan, dialect, plan.mode, progress.completed)?,
        )
        .await?;
        // And a principal created while it was paused, under a name a
        // remaining statement needs (119).
        let (wanted, vacated) =
            role_name_expectations(plan, dialect, plan.mode, progress.completed)?;
        refuse_taken_role_names(conn, &wanted, &vacated).await?;
        println!(
            "Resuming at statement {} of {} (checkpoint entry #{}).",
            progress.completed + 1,
            statements.len(),
            entry.id
        );
        if progress.total != statements.len() {
            bail!(
                "the checkpoint recorded {} statement(s) and this plan emits {}; they are not the \
                 same run.",
                progress.total,
                statements.len()
            );
        }
        (progress.completed, watching)
    } else {
        if entry.snapshot.staged.is_some() {
            bail!(
                "`{}` is mid-deployment: entry #{} is a staged checkpoint ({} of {} statements).\n\
                 Finish it with `pbps apply --staged --resume --plan ...`, or take the database \
                 as it stands with `pbps baseline --reason ...`.",
                pbps_model::escape::Escaped(&target.label),
                entry.id,
                entry.snapshot.staged.as_ref().map_or(0, |p| p.completed),
                entry.snapshot.staged.as_ref().map_or(0, |p| p.total)
            );
        }
        let recorded_ids = role_scope(&entry.snapshot.ids, &plan.ids, dialect);
        let recorded_modules = managed_modules(Some(&entry.snapshot), None);
        // Two cuts of one read: the checksum over the managed set the plan was
        // pinned to, and beside it the state the checkpoints are measured
        // against, which watches every module the plan names as well
        // (DECISIONS 174).
        let (scoped, mut watching, unrevocable) = staged_baseline(
            conn,
            &recorded_ids,
            &recorded_modules,
            &modules_after(&entry.snapshot, &plan.changes, Settled::SoFar),
            &externals_under(&externals, &recorded_ids),
            target.catalog_policy(project.config.unmanaged),
            &pinned_baseline_scopes(
                &entry.snapshot.schema.data_scopes(),
                &scopes_under(&plan.data, &plan.ids, &entry.snapshot.ids),
                &plan.changes,
                &plan.ids,
                (&entry.snapshot.schema, &entry.snapshot.ids),
            ),
            &entry.snapshot.schema,
            &held_before_the_plan(
                plan,
                &entry.snapshot.schema,
                &entry.snapshot.ids,
                &scopes_at(plan, &recorded_ids),
            ),
            CatalogWarnings::for_progress(d.progress),
        )
        .await?;
        retain_managed_roles(&mut watching, &plan.ids, dialect);
        trim_to_the_planned_scope(
            plan,
            &entry.snapshot.ids,
            &scopes_at(plan, &recorded_ids),
            &mut watching,
        );

        let live = pbps_model::drift_checksum(&scoped.schema, &recorded_ids);
        if live != plan.baseline.checksum {
            bail!(
                "`{}` is no longer the database this plan was computed against.\n\
                 plan baseline: {}\n\
                 database now:  {live}\n\
                 Something changed since the plan was approved. `pbps verify` shows what; \
                 then recompute the plan with `pbps plan --db --staged`.",
                pbps_model::escape::Escaped(&target.label),
                plan.baseline.checksum
            );
        }
        refuse_moved_database_collation(conn, plan, &target.label).await?;
        refuse_unexpressible(&scoped, &target.label, "apply again")?;
        // Ownership and revocability, as in the transactional apply (#692,
        // #1057).
        crate::engine::owned_targets(
            conn.driver(),
            &plan.changes,
            &plan.ids,
            &scoped.owners,
            &scoped.session_role,
        )?;
        crate::engine::unrevocable_grants(conn.driver(), &plan.changes, &plan.ids, &unrevocable)?;
        // Before the probes, as in the transactional apply (DEC-319.1).
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed(&entry.snapshot, &plan.changes, &plan.ids),
            crate::pins::Check::BeforeProbes,
        )
        .await?;
        // Only on a fresh start. The probes name objects as the catalog had
        // them before the first statement, and after a partial run some of
        // those names have already moved — a probe answered about the wrong
        // object is worse than one that was not asked.
        preflight(
            conn,
            dialect,
            plan,
            rename_targets,
            &scoped.schema,
            &recorded_ids,
            &crate::pins::Approved {
                project,
                target,
                plan,
                managed: &pinned_managed(&entry.snapshot, &plan.changes, &plan.ids),
            },
            None,
        )
        .await?;
        (0, watching)
    };

    // The settings this dialect's writes must run under, on the connection,
    // because this mode opens no transaction to carry them (`session_pins`).
    // Established here rather than once per statement: it covers a `--resume`
    // on a fresh connection, which starts partway through the plan, and every
    // statement after the one that fails.
    //
    // Kept even though `preflight` now pins as its first act: a `--resume`
    // skips `preflight` entirely, so this is the only place the resumed
    // statements get their settings from (DECISIONS 415).
    pin_session(conn, dialect).await?;

    // A staged run has no transaction of its own to carry the census: it reads
    // in one that it rolls back, at a start and at a resume alike.
    if start < statements.len() {
        conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
            .await?;
        let mut seen =
            recheck_enum_rebuilds(conn, &plan.changes, &entry.snapshot.ids, resume).await;
        if seen.is_ok() && !resume {
            seen = recheck_domain_alterations(conn, &plan.changes, &entry.snapshot.ids).await;
        }
        // Every statement starts from a target that still allows it: a resume
        // only re-asks for the packages whose statement has not run.
        if seen.is_ok() && !resume {
            seen = recheck_extension_creates(conn, plan).await;
        }
        let rolled = conn
            .rollback(&dialect.transaction_framing(&pin_zone(conn)))
            .await;
        seen?;
        rolled?;
    }

    let mut retained_verified = None;
    let total = statements.len();
    // Staged plans carry one logical change. A completed DROP has no catalog
    // target left; a closing-only resume must not ask for that old target.
    if start < total {
        drop_preflight(
            conn,
            dialect,
            &plan.changes,
            &plan.ids,
            &entry.snapshot.schema,
            &entry.snapshot.ids,
        )
        .await?;
    }
    // The names the catalog has right now. It starts at whatever the newest
    // entry recorded — the last ordinary state on a fresh run, the checkpoint
    // on a resume — and each statement moves it, using what the emitter said
    // that statement does (`Statement::renames`).
    let mut live_ids = entry.snapshot.ids.clone();
    // Include adopted types missing from the older state. Each lifecycle
    // statement updates its uid below before the checkpoint is read.
    live_ids.types = plan.ids.types.clone();
    if !dialect.manages_roles() {
        live_ids.roles = plan.ids.roles.clone();
    }
    // The state each checkpoint is measured against, read in the shape a
    // checkpoint is read in so the two compare like with like — the newest
    // entry's own schema is spelled by whichever command wrote it, and a cell
    // at its default has three spellings (`ObservedRow`).
    //
    // A staged run cannot roll back, so it cannot refuse its way out of a
    // change somebody else makes underneath it (147, 150). What it can do is
    // notice: nothing else in the run compares one checkpoint with the last,
    // so an edit that landed between two statements was carried into every
    // later read and finally into the closing ordinary snapshot, which is the
    // state `verify` measures against ever after (DECISIONS 159).
    println!(
        "Applying {} statement(s) with per-statement commits...",
        total - start
    );
    // A resume starts from a checkpoint, whose modules include what earlier
    // steps created; the union keeps those managed too.
    let pinned_managed = pinned_managed(&entry.snapshot, &plan.changes, &plan.ids);
    let written_routines = crate::engine::written_routines(&plan.changes);
    // Whether this run has a checkpoint to resume from: a resumed run starts
    // on one.
    let mut checkpointed = entry.snapshot.staged.is_some();
    let domain_defaults = pbps_pg::type_refs::declared::capture_defaults(
        conn,
        &plan.declared_type_pins,
        &plan.changes,
        pbps_pg::pins::Within::OwnTransaction,
    )
    .await?;
    for (i, stmt) in statements.iter().enumerate().skip(start) {
        // Before each step, including the first after a resume, which skips
        // the check before the probes along with the probes.
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            crate::pins::Check::BeforeStep {
                step: i + 1,
                total,
                executed: &statements[..i],
            },
        )
        .await?;
        let executed = if stmt.row_write.is_some() {
            // A staged row is still one atomic statement. Hold its trigger
            // locks through the write, then commit before the checkpoint;
            // non-transactional DDL retains the ordinary staged path.
            conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
                .await?;
            let result = async {
                // The plan's own admission, so a routine its review pinned is
                // admitted here as it was at the start (DEC-2563.9).
                let admission = crate::pins::applying(project, plan);
                let guard = crate::engine::prepare_data_writes(
                    conn,
                    &admission,
                    &plan.changes,
                    &entry.snapshot,
                    &plan.ids,
                )
                .await?;
                crate::engine::check_data_write(conn, &admission, stmt, &guard).await?;
                conn.execute(&stmt.sql).await?;
                // A deferred trigger would otherwise run at COMMIT, after the
                // check below, and a function replaced while an earlier one
                // held the commit would run unchecked. The step commits right
                // after, so settling here refuses nothing the commit would
                // have allowed (473).
                crate::engine::settle_data_writes(conn).await?;
                // Before this step commits, not after (#428). The trigger
                // check and the row statement are two server commands, and a
                // trigger function replaced between them has already run by
                // now. Its pin moved, and this transaction can still take the
                // write back with it. After the commit, the check below could
                // only record the step and stop.
                crate::pins::check(
                    conn,
                    project,
                    target,
                    plan,
                    &pinned_managed,
                    crate::pins::Check::BeforeStepCommits {
                        step: i + 1,
                        total,
                        executed: &statements[..=i],
                    },
                )
                .await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            finish_transaction(conn, dialect, result).await
        } else if stmt.transactional
            && (stmt.declared_type.is_some()
                || stmt.rewrites.is_some()
                || stmt.introduces.is_some()
                || stmt.extension.is_some()
                || !written_routines.is_empty()
                || !plan.type_pins.is_empty()
                || !plan.provider_pins.is_empty()
                || !plan.declared_type_pins.is_empty())
        {
            // A step of a plan that writes routines runs in a transaction of
            // its own, so that a SECURITY DEFINER routine outside the safe
            // form is refused before its step commits rather than after
            // (#322). A routine a later step creates is not there yet.
            conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
                .await?;
            let result = async {
                let retained = crate::engine::hold_retained(
                    conn,
                    &plan.changes,
                    Some(&entry.snapshot.declared.overlay(&previous)),
                )
                .await?;
                if let Some(extension) = &stmt.extension {
                    vet_before_extension(conn, extension, Some(plan)).await?;
                }
                if stmt.rewrites.is_some() {
                    // The expressions the rewrite re-evaluates, held still by
                    // the lock the guard takes, judged by the plan's own
                    // admission (DEC-2563.9).
                    let admission = crate::pins::applying(project, plan);
                    let guard = crate::engine::prepare_data_writes(
                        conn,
                        &admission,
                        &plan.changes,
                        &entry.snapshot,
                        &plan.ids,
                    )
                    .await?;
                    crate::engine::check_data_write(conn, &admission, stmt, &guard).await?;
                }
                if let Some(change) = &stmt.introduces {
                    // Judged by the plan's own admission before the statement
                    // runs, with what the earlier steps created in place.
                    let admission = crate::pins::applying(project, plan);
                    pbps_pg::table_expressions::vet_change(
                        conn,
                        &admission,
                        pbps_pg::pins::Within::CallersTransaction,
                        pbps_pg::table_expressions::Moment::Statement,
                        change,
                    )
                    .await
                    .and_then(pbps_pg::table_expressions::Vetted::admitted)
                    .map_err(anyhow::Error::msg)?;
                }
                conn.execute(&stmt.sql).await?;
                retained_verified =
                    crate::engine::verify_retained(conn, retained.as_ref(), &previous).await?;
                if let Some(type_statement) = &stmt.declared_type {
                    pbps_pg::type_refs::declared::vet(
                        conn,
                        type_statement,
                        &crate::pins::applying(project, plan),
                        &plan.declared_type_pins,
                        &plan.changes,
                        &statements[..=i],
                    )
                    .await?;
                }
                if let Some(extension) = &stmt.extension {
                    vet_after_extension(conn, extension, Some(plan)).await?;
                }
                crate::engine::refuse_unsafe_definers(
                    conn,
                    &written_routines,
                    crate::engine::Absent::NotYetCreated,
                )
                .await?;
                crate::pins::check(
                    conn,
                    project,
                    target,
                    plan,
                    &pinned_managed,
                    crate::pins::Check::BeforeStepCommits {
                        step: i + 1,
                        total,
                        executed: &statements[..=i],
                    },
                )
                .await
            }
            .await;
            let result = finish_transaction(conn, dialect, result).await;
            // The preceding ADD committed its NOT VALID CHECK. Recover it
            // after rollback, when catalog reads can run again (DEC-1638.1).
            match result {
                Err(error) => {
                    match crate::engine::recover_failed_staged_validation(conn, stmt).await {
                        Some(outcome) => Err(error.context(outcome)),
                        None => Err(error),
                    }
                }
                Ok(()) => Ok(()),
            }
        } else {
            crate::engine::execute_staged_statement(conn, stmt)
                .await
                .map_err(anyhow::Error::from)
        };
        if let Err(e) = executed {
            // `.context()` on the already-built `anyhow::Error`, not
            // `anyhow::anyhow!("...{e}")`: the latter would bake `e`'s
            // `Display` — which can carry the driver's own sentence — into an
            // opaque string with nothing left to downcast. This keeps
            // whatever `e` already carries (a `DbError::Driver`, most often)
            // as the source, so `crate::engine::ledger_safe_reason` can find
            // and redact just that frame later (DECISIONS 455).
            let recovery = if !stmt.transactional && i == 0 && entry.snapshot.staged.is_none() {
                "There is no staged checkpoint: the first non-transactional statement failed. \
                 Fix the cause, then start this plan again with `pbps apply --staged` without `--resume`."
            } else if is_scan_check(stmt, pbps_dialect::ScanCheckStep::Validate) && !checkpointed {
                // The `ADD` before it took no checkpoint, and the failed
                // `VALIDATE` took its check out again (#1638).
                "There is no staged checkpoint: the attach's temporary CHECK did not validate. \
                 Fix the rows it names, then start this plan again with `pbps apply --staged` \
                 without `--resume`."
            } else {
                "The ledger records everything that did complete. Fix the cause, then continue \
                 with `pbps apply --staged --resume`."
            };
            return Err(e.context(format!(
                "the database rejected statement {} of {total}; earlier committed statements were not rolled back:\n{}\n\n\
                 {recovery}",
                i + 1,
                stmt.sql
            )));
        }

        // Applied before the checkpoint is taken: this statement has committed,
        // so the names it moved are the names the catalog has now.
        for (from, to) in &stmt.renames {
            live_ids.rename_table(from, to);
        }
        for (from, to) in &stmt.role_renames {
            live_ids.rename_role(from, to);
        }
        if let Some(type_statement) = &stmt.declared_type {
            if type_statement.definition.is_some() {
                live_ids
                    .types
                    .insert(type_statement.uid.clone(), type_statement.name.clone());
            } else {
                live_ids.types.remove(&type_statement.uid);
            }
        }
        if let Some(installed) = &stmt.extension {
            match installed.phase {
                pbps_dialect::ExtensionPhase::Drop => {
                    live_ids.extensions.remove(&installed.uid);
                }
                pbps_dialect::ExtensionPhase::Create => {
                    live_ids
                        .extensions
                        .insert(installed.uid.clone(), installed.name.clone());
                }
                // The package keeps its identity.
                pbps_dialect::ExtensionPhase::Update { .. }
                | pbps_dialect::ExtensionPhase::SetSchema { .. } => {}
            }
        }
        adopt_created(&mut live_ids, &plan.ids, &stmt.creates);

        // No checkpoint after a staged attach's `ADD … NOT VALID`: measured,
        // every read of a table holding a `NOT VALID` check refuses as a fact
        // the model cannot hold, so this read would stop the run on a
        // statement that worked. The `VALIDATE` after it checkpoints both,
        // and takes the check out again if it fails (#1638, DEC-1638.1).
        if is_scan_check(stmt, pbps_dialect::ScanCheckStep::Add) {
            crate::pins::check(
                conn,
                project,
                target,
                plan,
                &pinned_managed,
                crate::pins::Check::AfterStep {
                    step: i + 1,
                    total,
                    executed: &statements[..=i],
                },
            )
            .await?;
            println!(
                "  statement {} of {total} done (checkpointed with the next)",
                i + 1
            );
            continue;
        }

        // The unmanaged policy is deliberately not enforced between two
        // committed statements. It is a hygiene gate for the start of a
        // command, where it already ran, and failing it here would abort
        // *after* the DDL committed and leave no checkpoint to resume from.
        // The rows under the plan's scope even mid-way: a declared row the
        // remaining statements have not inserted yet is simply not there, and
        // the checkpoint says so.
        let after = managed_state(
            conn,
            &live_ids,
            &modules_after(&entry.snapshot, &plan.changes, Settled::SoFar),
            &externals_under(&externals, &live_ids),
            target.catalog_policy(pbps_config::Unmanaged::Ignore),
            // The plan's own scope, not the widened one: what a statement
            // leaves is read under the keys the plan leaves, as the closing
            // read is, so a row the renames leave unmanaged is not compared
            // (DEC-2346.2).
            &scopes_at(plan, &live_ids),
            &Schema::default(),
            crate::engine::Read::Snapshot,
            CatalogWarnings::for_progress(d.progress),
        )
        .await?;
        // Scoped and identified by `live_ids`, not by the plan's mapping: a
        // checkpoint records the database as it stands, and halfway through a
        // rename that moves both the schema and the name, the table stands at
        // neither end. `--resume` reads this mapping back and compares against
        // it, so a hand-made change to that table while the deployment is
        // paused is seen rather than carried silently into the closing entry.
        let module_deps = dependency_hints_for_schema(&entry.snapshot, plan, &after.schema);
        let mut checkpoint = pbps_model::StateSnapshot::new(
            StateKind::Staged,
            after.schema,
            live_ids.clone(),
            operator,
        );
        checkpoint.baseline_evidence = after.baseline_evidence;
        checkpoint.module_deps = module_deps;
        // A checkpoint records the database as it stands, and what was
        // declared is what the previous state said until the plan finishes:
        // the closing entry advances it by the whole plan, and nothing plans
        // against a checkpoint (`refuse_mid_deployment`).
        checkpoint.declared = entry.snapshot.declared.clone();
        checkpoint.git_sha = plan.git_sha.clone().or_else(|| db::git_sha(project.root()));
        checkpoint.plan_checksum = Some(plan_checksum.to_owned());
        checkpoint.staged = Some(pbps_model::StagedProgress {
            completed: i + 1,
            total,
            last_statement: stmt.sql.clone(),
        });
        let recorded = checkpoint.schema.clone();
        // After the step, and before its checkpoint, so that a routine
        // replaced while the step ran is caught at that step — the last one
        // included. The statement has committed, so a mismatch is a
        // post-commit guard failure: the checkpoint is written first and the
        // run stops after it (DECISIONS 159).
        let pins_after_step = crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            crate::pins::Check::AfterStep {
                step: i + 1,
                total,
                executed: &statements[..=i],
            },
        )
        .await;
        let id = crate::engine::record(conn, &checkpoint).await?;
        checkpointed = true;
        println!("  statement {} of {total} done (checkpoint #{id})", i + 1);
        pins_after_step.map_err(|e| {
            e.context(format!(
                "statement {} of {total} committed, and checkpoint #{id} records it; nothing \
                 was rolled back. A resume checks the same pins, so either put the routine back \
                 as it was and run `pbps apply --staged --resume`, or accept the database with \
                 `pbps baseline --reason ...` and plan from there",
                i + 1
            ))
        })?;
        // The checkpoint is written *first*, and then the run stops. It says
        // what the database holds, which is the one thing a resume needs to be
        // true — refusing before writing it would lose the record of a
        // statement that has already committed, which is what checkpoints are
        // for. Stopping is the whole remedy a staged run has.
        let statement_plan = staged.statement(stmt, statements.get(i.wrapping_sub(1)));
        staged_pinned_movement(
            dialect,
            statement_plan.staged(),
            &without_scan_checks(&previous, statements),
            &without_scan_checks(&recorded, statements),
            &target.label,
            target.environment(),
            StagedRead::Checkpoint {
                completed: i + 1,
                total,
            },
            retained_verified.as_ref(),
            Some(&domain_defaults),
        )?;
        previous = recorded;
    }

    let retaining = pbps_model::retained::expressions(&plan.changes)
        .next()
        .is_some();
    if retaining {
        conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
            .await?;
    }
    // Keep the closing guard's large future off every caller's apply future,
    // including transactional applies that never enter this staged path.
    let closing = Box::pin(async {
        let retained = crate::engine::hold_retained(conn, &plan.changes, None).await?;
        // The closing entry is an ordinary apply with no staged marker: its absence
        // is what tells every later command this environment is no longer
        // mid-deployment.
        let after = managed_state(
            conn,
            &plan.ids,
            &modules_after(&entry.snapshot, &plan.changes, Settled::Whole),
            &externals_under(&externals, &plan.ids),
            target.catalog_policy(project.config.unmanaged),
            &plan.data,
            &Schema::default(),
            if retaining {
                crate::engine::Read::InsideOwnTransaction
            } else {
                crate::engine::Read::Snapshot
            },
            CatalogWarnings::for_progress(d.progress),
        )
        .await?;
        refuse_recreated_tables(conn, &plan.changes, statements, &after.unmanaged)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "{e:#}\n\n\
             All {total} statement(s) completed, but the staged deployment cannot close. \
             Nothing was rolled back; the ledger retains the last checkpoint. \
             Remove the recreated table and run `pbps apply --staged --resume`, \
             or accept the database with `pbps baseline --reason ...` and plan from there."
                )
            })?;
        let verified =
            crate::engine::verify_retained(conn, retained.as_ref(), &after.schema).await?;
        // And the last window of all: between the final checkpoint and this read.
        // Refused *before* the ordinary entry is written, because that entry is
        // what says the deployment finished — leaving the environment on its last
        // checkpoint is the honest answer, and `refuse_mid_deployment` then makes
        // every other command say so.
        staged_pinned_movement(
            dialect,
            staged,
            &previous,
            &after.schema,
            &target.label,
            target.environment(),
            StagedRead::Closing { total },
            verified.as_ref(),
            Some(&domain_defaults),
        )?;
        // The same read, and the half of it no `Schema` holds. Refused here for
        // the reason the movement check above is: this is the last window, and
        // the ordinary entry that follows is what says the deployment finished.
        refuse_unmet_public_execution(&plan.changes, &after, &target.label).map_err(|e| {
            anyhow::anyhow!(
                "{e:#}\n\n\
             All {total} statement(s) completed, but the staged deployment cannot close. \
             Nothing was rolled back; the ledger retains the last checkpoint. Settle what \
             moved and run `pbps apply --staged --resume`, or accept the database with \
             `pbps baseline --reason ...` and plan from there."
            )
        })?;
        // The pins once more, because a `--resume` after the last checkpoint runs
        // no step and so reaches no other check (DEC-319.1).
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            if retaining {
                crate::pins::Check::BeforeRecording {
                    executed: statements.statements(),
                }
            } else {
                crate::pins::Check::BeforeClosing {
                    executed: statements.statements(),
                }
            },
        )
        .await
        .map_err(|e| {
            e.context(format!(
                "all {total} statement(s) completed, but the staged deployment cannot close; \
             nothing was rolled back and the ledger retains the last checkpoint. Either put \
             the routine back as it was and run `pbps apply --staged --resume`, or accept the \
             database with `pbps baseline --reason ...` and plan from there"
            ))
        })?;
        let mut snapshot = pbps_model::StateSnapshot::new(
            pbps_model::StateKind::Apply,
            after.schema,
            plan.ids.clone(),
            operator,
        );
        snapshot.baseline_evidence = after.baseline_evidence;
        snapshot.module_deps = plan.module_deps.clone();
        snapshot.git_sha = plan.git_sha.clone().or_else(|| db::git_sha(project.root()));
        snapshot.plan_checksum = Some(plan_checksum.to_owned());
        snapshot.target_assertions = plan.target_assertions.clone();
        // What the previous state declared, advanced by what this plan wrote: from
        // the plan alone, since `apply --plan` needs nothing else (SPEC §7.3).
        snapshot.declared = entry.snapshot.declared.clone();
        snapshot
            .declared
            .advance_with_partitions(&plan.changes, &snapshot.schema);
        let id = crate::engine::record(conn, &snapshot).await?;
        if retaining {
            crate::pins::check(
                conn,
                project,
                target,
                plan,
                &pinned_managed,
                crate::pins::Check::BeforeCommit {
                    executed: statements.statements(),
                },
            )
            .await?;
        }
        Ok(id)
    })
    .await;
    if retaining {
        finish_transaction(conn, dialect, closing).await
    } else {
        closing
    }
}

/// The movement check a staged run can make: each read compared with the one
/// before it, over everything this plan does not touch.
///
/// A split table rename uses the current statement's exact endpoints at each
/// checkpoint (DECISIONS 436). Its intermediate name is still compared for
/// unchanged shape, rows and incoming references. Other change shapes retain
/// the logical plan's exemptions: a later statement of that change must not
/// be demanded at an earlier checkpoint (152 to 158, 161). The closing read
/// retains the full logical postconditions and checks the vacated rename
/// endpoints, including intermediate names, under SPEC §7.6.
///
/// Which read of a staged run is being compared with the one before it.
///
/// The two are told apart because the remedy differs. A change found at a
/// checkpoint is *in* that checkpoint — the read is what the checkpoint
/// records — so a `--resume` measures the live database against a record that
/// already holds it, and goes on. A change found at the closing read landed
/// after the last checkpoint was written, and no record holds it: the same
/// `--resume` finds the database moved since that checkpoint and refuses. One
/// message told the operator to resume in both cases (DECISIONS 190).
#[derive(Clone, Copy)]
enum StagedRead {
    /// The read a checkpoint records, once `completed` of `total` statements
    /// have run.
    Checkpoint { completed: usize, total: usize },
    /// The read after the last checkpoint, which becomes the closing entry.
    Closing { total: usize },
}

/// Ids follow a renamed table and omit a dropped one, so a recreated name
/// falls outside the managed schema. Its absence remains a plan promise
/// (SPEC §7.6). Use the same read's inventory without adopting unrelated
/// unmanaged tables into the guard (SPEC §8.2); the net promise also permits
/// a plan that deliberately reuses a name.
async fn refuse_recreated_tables(
    conn: &mut Conn,
    changes: &pbps_model::ChangeSet,
    statements: &[pbps_dialect::Statement],
    unmanaged: &[TableName],
) -> anyhow::Result<()> {
    let absent = removed_table_names(changes, statements);
    if let Some(name) = crate::engine::matching_table_names(conn, &absent, unmanaged)
        .await?
        .first()
    {
        anyhow::bail!(
            "{name} is still there, and this plan removes it",
            name = name.shown()
        );
    }
    Ok(())
}

/// The logical rename omits its intermediate names (DECISIONS 436). Their
/// emitted moves still promise absence at closing (SPEC §7.6), unless a
/// later move or the logical plan deliberately gives the name to a table.
fn removed_table_names(
    changes: &pbps_model::ChangeSet,
    statements: &[pbps_dialect::Statement],
) -> Vec<TableName> {
    let mut expected = BTreeMap::new();
    for statement in statements {
        for (from, to) in &statement.renames {
            expected.insert(from, pbps_model::Presence::Absent);
            expected.insert(to, pbps_model::Presence::Present);
        }
    }
    expected.extend(
        changes
            .changes
            .iter()
            .flat_map(|change| change.change.tables_after()),
    );
    expected
        .into_iter()
        .filter(|(_, presence)| *presence == pbps_model::Presence::Absent)
        .map(|(name, _)| name.clone())
        .collect()
}

/// What the plan promised about `PUBLIC`, asked of the read that closes the
/// run.
///
/// # Why this is not part of the movement comparison
///
/// `refuse_pinned_movement` compares two `Schema`s, and what `PUBLIC` holds
/// is never in one: it is the engine's default rather than a grant, so putting
/// it there would make every routine read back as drift (DECISIONS 371, 95).
/// The decision this plan carries is therefore checked against the read's own
/// `public_execute` context instead — which is what SPEC §7.6 requires of it.
///
/// # Why the closing read in particular
///
/// SPEC §7.6 does not promise that a checkpoint catches a concurrent change
/// to **the field the plan is itself changing**: the field is expected to move
/// there, and the checkpoint cannot tell the plan's statement from the other
/// session's. It does promise the closing read catches it, by holding the
/// field to the value the plan promised. Without this the promise was not
/// kept: a staged run's `GRANT` and its `CREATE` commit separately, another
/// session revoking `PUBLIC` in between would be invisible to the checkpoint
/// by design and to the closing read because nothing looked, and the run
/// would record a routine the declaration asked to leave open as a success.
///
/// A decision this engine does not make is not asked about — the check reads
/// the plan's own decisions, and a dialect that emits none has none to fail.
fn refuse_unmet_public_execution(
    changes: &pbps_model::ChangeSet,
    after: &pbps_diff::Scoped,
    label: &str,
) -> anyhow::Result<()> {
    let mut unmet = Vec::new();
    for planned in &changes.changes {
        let pbps_model::Change::PublicExecution {
            routine, access, ..
        } = &planned.change
        else {
            continue;
        };
        let id = pbps_model::ModuleId::Routine(routine.clone());
        let open = after.public_execute.contains(&id);
        match access {
            pbps_model::PublicAccess::Kept if !open => unmet.push(format!(
                "`{routine}` is not executable by PUBLIC, which this plan declared it would stay",
                routine = routine.shown()
            )),
            pbps_model::PublicAccess::Revoked if open => unmet.push(format!(
                "`{routine}` is executable by PUBLIC, which this plan closed",
                routine = routine.shown()
            )),
            pbps_model::PublicAccess::Kept | pbps_model::PublicAccess::Revoked => {}
        }
    }
    if !unmet.is_empty() {
        bail!(
            "{label} does not hold what this plan settled about PUBLIC execution:\n  {}",
            unmet.join("\n  "),
            label = pbps_model::escape::Escaped(label),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn staged_pinned_movement<'a>(
    dialect: &dyn pbps_dialect::Dialect,
    plan: guard::Staged<'a>,
    before: &Schema,
    after: &Schema,
    label: &str,
    environment: Option<&str>,
    read: StagedRead,
    verified: Option<&'a pbps_pg::retained::Verified>,
    defaults: Option<&'a pbps_pg::type_refs::declared::DomainDefaults>,
) -> anyhow::Result<()> {
    refuse_pinned_movement(
        dialect,
        plan.read(read)
            .with_retained(verified)
            .with_domain_defaults(defaults),
        before,
        after,
        label,
    )
    .map_err(|e| {
        match read {
            StagedRead::Checkpoint { completed, total } => anyhow::anyhow!(
                "{e:#}\n\n\
                 {completed} of {total} statement(s) completed and the ledger records them; a \
                 staged apply runs outside a transaction, so nothing was rolled back. The \
                 checkpoint holds the database as it stands, this change included — resuming \
                 accepts it. The list above is the record of what moved: the checkpoint \
                 already holds it, so `pbps verify` reads clean, and this refusal is recorded \
                 as the failed entry's reason{}.",
                // Named only to a caller who can run it. `status` takes no
                // target and reports on the environments `pbps.yml` configures
                // (SPEC 9.2), so for a `--db` run it answers about other
                // databases or says none are configured — a pointer that cannot
                // be followed, in a message whose whole job is to say where the
                // record is. Without one the sentence is already complete: the
                // list above is that record (DECISIONS 196).
                match environment {
                    Some(_) => ", which `pbps status` shows",
                    None => "",
                }
            ),
            StagedRead::Closing { total } => anyhow::anyhow!(
                "{e:#}\n\n\
                 All {total} statement(s) completed and the ledger records them; a staged \
                 apply runs outside a transaction, so nothing was rolled back. This change \
                 landed after the last checkpoint was written, so no checkpoint holds it, and \
                 `--resume` will refuse the database as having moved since that checkpoint. \
                 `pbps verify` shows what it is: undo it and resume, or take the database as \
                 it stands with `pbps baseline --reason ...` and plan from there."
            ),
        }
    })
}

/// Refuses to act on an environment that is half-way through a staged apply.
///
/// Planning or applying anything else on top of an unfinished staged plan
/// builds on a state nobody approved: the recorded baseline is a checkpoint,
/// not a deployment anybody signed off.
fn refuse_mid_deployment(entry: &pbps_db::LedgerEntry, label: &str) -> anyhow::Result<()> {
    let Some(progress) = &entry.snapshot.staged else {
        return Ok(());
    };
    bail!(
        "`{label}` is mid-deployment: entry #{} is a staged checkpoint ({} of {} statements).\n\
         Finish it with `pbps apply --staged --resume --plan ...`, or take the database as it \
         stands with `pbps baseline --reason ...`.",
        entry.id,
        progress.completed,
        progress.total,
        label = pbps_model::escape::Escaped(label),
    )
}

/// The pre-flight of §7.5: dependency impact, then probes against the data.
/// The members each dropped role is expected to still have when the plan's
/// next statement runs: every listed member before statement one, fewer
/// once some of its `DROP MEMBER` statements have committed, and no
/// expectation at all once the `DROP ROLE` has. Counted off the emitter's
/// own statements, in the emitter's order (one `DROP MEMBER` per listed
/// member, then the `DROP ROLE`), so a resume asks about the role as the
/// plan left it and not as the plan first saw it (DECISIONS 102).
fn role_drop_expectations<D: crate::emission::Engine>(
    plan: &(dyn pbps_diff::order::PlanView<D> + 'static),
    dialect: &D,
    mode: pbps_model::PlanMode,
    completed: usize,
) -> anyhow::Result<Vec<(pbps_model::RoleName, Vec<pbps_model::RoleName>)>> {
    let cs = pbps_dialect::PlanPayload::changes(plan);
    let mut out = Vec::new();
    let mut at = 0usize;
    let emit = crate::emitter(dialect, plan, mode);
    for (position, p) in cs.changes.iter().enumerate() {
        let n = emit(position)?.len();
        if let pbps_model::Change::DropRole { name, members, .. } = &p.change {
            let done = completed.saturating_sub(at).min(n);
            if done <= members.len() {
                out.push((name.clone(), members[done..].to_vec()));
            }
        }
        at += n;
    }
    Ok(out)
}

/// Refuses, before anything runs, a role drop whose role is not the one the
/// plan was made against: a member the plan did not list, a listed member
/// no longer there, or a securable it has come to own. A staged apply would
/// otherwise commit every `DROP MEMBER` the reviewer saw and then fail on
/// the one nobody did, leaving the reviewed users without access and the
/// role in place (DECISIONS 92, 102).
async fn check_role_drops(
    conn: &mut Conn,
    expected: &[(pbps_model::RoleName, Vec<pbps_model::RoleName>)],
) -> anyhow::Result<()> {
    if expected.is_empty() {
        return Ok(());
    }
    let members_now = crate::engine::role_members(conn)
        .await
        .context("cannot read the role memberships")?;
    let owned_now = crate::engine::role_owned_securables(conn)
        .await
        .context("cannot read what the roles own")?;
    for (name, listed) in expected {
        let listed: std::collections::BTreeSet<&pbps_model::RoleName> = listed.iter().collect();
        let now: std::collections::BTreeSet<&pbps_model::RoleName> = members_now
            .get(name)
            .map(|m| m.iter().collect())
            .unwrap_or_default();
        let added: Vec<&pbps_model::RoleName> = now.difference(&listed).copied().collect();
        let gone: Vec<&pbps_model::RoleName> = listed.difference(&now).copied().collect();
        if !added.is_empty() || !gone.is_empty() {
            let mut detail = Vec::new();
            if !added.is_empty() {
                detail.push(format!(
                    "member(s) the plan did not list: {}",
                    added
                        .iter()
                        .map(|name| name.shown().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !gone.is_empty() {
                detail.push(format!(
                    "listed member(s) no longer in it: {}",
                    gone.iter()
                        .map(|name| name.shown().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            bail!(
                "role `{name}` is not the role this plan was made against: {}.\n\
                 Its membership changed after `plan --db`, and the plan lists who loses \
                 the role so a reviewer can see it. Recompute it with `pbps plan --db`.",
                detail.join("; "),
                name = name.shown(),
            );
        }
        if let Some(securables) = owned_now.get(name)
            && !securables.is_empty()
        {
            bail!(
                "role `{name}` cannot be dropped: it owns {}, which it did not when this \
                 plan was made.\n\
                 Move the ownership first (`ALTER AUTHORIZATION ON {} TO dbo;`, by hand, \
                 since pbps does not decide who owns a securable), then recompute the plan \
                 with `pbps plan --db`.",
                securables.join(", "),
                securables[0],
                name = name.shown(),
            );
        }
    }
    Ok(())
}

/// The zone a deployment over `conn` pins (DEC-1916.1): the one the session
/// started in.
///
/// At `plan --db` that is the zone the plan records. At `apply` it is the
/// zone the plan recorded, because [`refuse_moved_time_zone`] has refused
/// before the first statement unless the two are one. An engine with no
/// session zone ignores the argument, which is why UTC is no guess there.
fn pin_zone(conn: &Conn) -> SessionZone {
    conn.session_zone()
        .cloned()
        .unwrap_or_else(SessionZone::utc)
}

/// The settings this dialect's statements depend on, established on the
/// connection.
///
/// Idempotent by construction — it is a list of `SET`s — and issued from more
/// than one place on purpose. See [`run_probes`], which does not trust its
/// caller to have called this.
async fn pin_session(conn: &mut Conn, dialect: &dyn pbps_dialect::Dialect) -> anyhow::Result<()> {
    if let Some(pins) = dialect.session_pins(&pin_zone(conn)) {
        conn.execute(&pins)
            .await
            .context("the session settings this dialect's statements depend on could not be set")?;
    }
    Ok(())
}

async fn drop_preflight(
    conn: &mut Conn,
    dialect: &dyn pbps_dialect::Dialect,
    changes: &pbps_model::ChangeSet,
    ids: &IdsFile,
    stored: &Schema,
    stored_ids: &IdsFile,
) -> anyhow::Result<()> {
    conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
        .await?;
    let result = crate::engine::check_drop_blockers(conn, changes, ids, stored, stored_ids).await;
    let rollback = conn
        .rollback(&dialect.transaction_framing(&pin_zone(conn)))
        .await;
    result?;
    rollback?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn preflight<D: crate::emission::Engine>(
    conn: &mut Conn,
    dialect: &D,
    plan: &pbps_diff::order::AdmittedPlan<D>,
    rename_targets: &[pbps_db::impact::RenameTarget],
    stored: &pbps_model::Schema,
    stored_ids: &IdsFile,
    approved: &crate::pins::Approved<'_>,
    progress: Option<&services::Progress>,
) -> anyhow::Result<()> {
    // **The pins before the questions.** Every read below — the edition, the
    // role checks, the rename impact scans and the probes — used to run under
    // whatever settings the operator's own session happened to carry, while
    // the statements they clear ran under the pinned ones, because the framing
    // that pins them is opened *after* this function returns and the staged
    // path sets them after this call. Measured on 18.6, that gap refuses valid
    // plans in three separate ways (DECISIONS 415).
    //
    // Here rather than at the two call sites: a caller that forgets is a
    // caller that gets the old bug back, and this function is the one whose
    // answers depend on it.
    pin_session(conn, dialect).await?;
    drop_preflight(conn, dialect, &plan.changes, &plan.ids, stored, stored_ids).await?;
    pbps_diff::drop_requirements::validate(&plan.changes, stored_ids, &plan.ids)
        .map_err(anyhow::Error::msg)?;
    // Asked again, as the edition is below: a table outside the declarations
    // can gain a key to the parent between the plan and this apply, and the
    // engine takes the unlogged partition under it all the same (#1595).
    crate::engine::refuse_unlogged_partition_referencers(
        conn,
        &plan.changes,
        &plan.ids,
        plan.mode.is_staged(),
    )
    .await?;
    // A publication can list the table between the plan and this apply (#1489).
    crate::engine::refuse_published_unlogged(conn, &plan.changes, &plan.ids).await?;
    // The edition, asked again. `plan --db` checked it, but nothing binds a
    // saved plan to an environment: the same file can be applied to a different
    // server, or to the same one after an edition change, and an `ONLINE = ON`
    // statement rejected halfway through an apply is precisely what the check
    // at plan time exists to prevent (ADR-0003).
    let verdict = crate::engine::edition_verdict(conn, &plan.changes).await?;
    if !verdict.refused_online.is_empty() {
        bail!(
            "this plan carries `strategy: online` for {}, and this server runs {}, which has no \
             online index operations.\n\
             The plan was approved against an edition that supports it. Recompute it against \
             this environment with `pbps plan --db`.",
            verdict
                .refused_online
                .iter()
                .map(|subject| pbps_model::escape::Escaped(subject).to_string())
                .collect::<Vec<_>>()
                .join(", "),
            verdict.runs
        );
    }

    // A dropped role's members were listed at plan time, and the ledger's
    // checksum cannot see a membership change — membership is each
    // environment's own and outside the managed state on purpose. Read
    // again here, before statement one (DECISIONS 92), and again on a
    // resume, for the members whose statements have not run yet (102).
    check_role_drops(conn, &role_drop_expectations(plan, dialect, plan.mode, 0)?).await?;
    // The same for a name this plan needs free: a user created since the
    // plan is outside the managed state, so the checksum above cannot see
    // it either, and `CREATE ROLE` would meet it after everything ordered
    // before had run (119).
    let (wanted, vacated) = role_name_expectations(plan, dialect, plan.mode, 0)?;
    refuse_taken_role_names(conn, &wanted, &vacated).await?;

    // A SCHEMABINDING referrer this plan is about to drop is not a blocker: the
    // module changes sort before the table changes precisely so that the drop
    // runs first (ADR-0002). The catalog is queried before anything executes, so
    // it still sees the dependency — and reporting it would refuse a plan whose
    // own first statement removes the obstacle.
    let dropped = dropped_referrer_names(&plan.changes);
    let dropped_modules = dropped_modules(&plan.changes);

    let mut blocked = Vec::new();
    for target in rename_targets {
        let mut report = crate::engine::rename_impact(conn, target).await?;
        excuse_dropped_referrers(&mut report, &dropped, &dropped_modules);
        if report.is_empty() {
            continue;
        }
        services::notice(
            progress,
            "apply.preflight",
            format!("\n{} {} affects:", target.verb(), report.target),
        );
        for line in impact_lines(
            &report,
            &crate::engine::impact_notes(conn.driver(), &report),
        ) {
            services::notice(progress, "apply.preflight", line);
        }
        for r in &report.blocking {
            blocked.push(format!("{} {}", r.kind, r.name));
        }
    }
    if !blocked.is_empty() {
        bail!(
            "the engine will refuse this rename while these exist: {}.\n\
             Drop or recreate them without SCHEMABINDING first.",
            blocked.join(", ")
        );
    }

    // The probes take the plan's changes, which name a parent it renames by
    // its new name, so the stored relation is brought under those names
    // (#1751 review).
    let names = pbps_model::plan_names::PlanNames::of(&plan.changes.changes, &plan.ids)?;
    let admission = approved.admission();
    run_probes_observed(
        conn,
        &admission,
        dialect,
        &names,
        &probe_partitions(stored, &names),
        Some(pbps_model::operation::Snapshot {
            schema: stored,
            ids: stored_ids,
        }),
        Some(approved),
        progress,
    )
    .await
}

/// The partitions the probes read the plan against: the relation as the
/// database stands before the plan, under the plan's final parent names. Every
/// caller of [`run_probes`] that probes a plan builds it here, so the plan-time
/// removed-label count and the apply-time probes see the same relation
/// (#2406).
fn probe_partitions(
    stored: &pbps_model::Schema,
    names: &pbps_model::plan_names::PlanNames<'_>,
) -> pbps_model::Partitions {
    pbps_model::Partitions::of(stored).renamed(names)
}

/// The body of one rename's impact report: what breaks, what blocks, and what
/// the rename is carried into, each under its own wording.
///
/// `carried` used to be left out, so a PostgreSQL report holding only carried
/// objects printed an "affects:" heading with nothing under it, and one that
/// mixed them hid the half the engine handles (#307, DECISIONS 395). A carried
/// object is never described as breaking or blocking. The notes are the
/// engine's (`engine::impact_notes`), printed last: the list is of what the
/// catalog can see, and applications and downstream ELT are invisible to any
/// query.
fn impact_lines(report: &pbps_db::impact::ImpactReport, notes: &[String]) -> Vec<String> {
    let detail = |r: &pbps_db::impact::Referrer| {
        r.detail
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for r in &report.advisory {
        out.push(format!("  {} {}{}", r.kind, r.name, detail(r)));
    }
    for r in &report.blocking {
        out.push(format!(
            "  {} {} — SCHEMABINDING, which blocks the rename",
            r.kind, r.name
        ));
    }
    for r in &report.carried {
        out.push(format!(
            "  {} — carried into the new name, keeps working{}",
            pbps_model::escape::Escaped(&r.name),
            detail(r)
        ));
    }
    out.extend(notes.iter().map(|n| format!("  {n}")));
    out
}

/// What this plan implies about the data, asked of the engine (SPEC §7.5).
///
/// **Pins its own session**, and does not take the caller's word for it. A
/// probe reads a rendering, parses a plan literal and parses the operator's own
/// declared expression, and all three are decided by settings the framing pins
/// — which used to be established only *after* this ran (DECISIONS 415). A
/// second `SET` batch costs nothing; a probe answered under the wrong settings
/// refuses a plan this engine takes.
/// `partitions` is the relation as the database stands before the plan: the
/// read whose checksum matched the plan's baseline (DEC-1699.1).
#[cfg(test)]
async fn run_probes(
    conn: &mut Conn,
    dialect: &dyn pbps_dialect::Dialect,
    names: &pbps_model::plan_names::PlanNames<'_>,
    partitions: &pbps_model::Partitions,
    sources: Option<pbps_model::operation::Snapshot<'_>>,
    approved: Option<&crate::pins::Approved<'_>>,
) -> anyhow::Result<()> {
    run_probes_observed(
        conn,
        &pbps_pg::admission::Admission::default(),
        dialect,
        names,
        partitions,
        sources,
        approved,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_probes_observed(
    conn: &mut Conn,
    admission: &pbps_pg::admission::Admission,
    dialect: &dyn pbps_dialect::Dialect,
    names: &pbps_model::plan_names::PlanNames<'_>,
    partitions: &pbps_model::Partitions,
    sources: Option<pbps_model::operation::Snapshot<'_>>,
    approved: Option<&crate::pins::Approved<'_>>,
    progress: Option<&services::Progress>,
) -> anyhow::Result<()> {
    pin_session(conn, dialect).await?;
    // A recorded conversion's expression is judged before any probe evaluates
    // it: the probe would run non-built-in code as the deployer, unvetted.
    let mark = admission.mark();
    let immutable = crate::engine::vet_conversions(conn, admission, names.changes()).await?;
    crate::engine::vet_drop_requirements(conn, admission, names).await?;
    // The expressions the plan gives a table are evaluated by the probes too, so
    // they are judged first. One that is not admitted yet, or cannot be bound
    // before the plan creates what it uses, is left to the check just before its
    // statement; the probes that would run it are skipped, not run unjudged.
    let mut table_unjudged = false;
    if conn.driver() == pbps_db::Driver::Postgres {
        for planned in names.changes() {
            match crate::engine::vet_table_expressions(
                conn,
                admission,
                pbps_pg::pins::Within::OwnTransaction,
                pbps_pg::table_expressions::Moment::Early,
                &planned.change,
            )
            .await
            {
                Ok(pbps_pg::table_expressions::Vetted::Admitted) => {}
                // Not judged, however it came about: refused, or unbound here.
                Ok(pbps_pg::table_expressions::Vetted::Unjudged(_)) | Err(_) => {
                    table_unjudged |= pbps_pg::table_expressions::probed(&planned.change);
                }
            }
        }
    }
    if table_unjudged || admission.relied_since(mark) {
        // Plan time, and a conversion or requirement is admitted only by the
        // review: no probe evaluates it before the human has read the source.
        // Apply runs every probe after the approval (DEC-2563.2).
        services::notice(
            progress,
            "apply.preflight",
            "Pre-flight: the probes are unchecked until source review, because an expression calls a routine that only `pin_type_routines` admits, or that is not admitted yet; apply judges it before its statement and checks them."
                .to_owned(),
        );
        return Ok(());
    }
    let mut failures = Vec::new();
    let mut passed = 0usize;
    let mut unchecked = 0usize;
    let report = dialect.preflight_with_conversions(names, partitions, sources, &immutable);
    for skipped in &report.unchecked {
        unchecked += 1;
        eprintln!(
            "warning: could not check {} ({}); the engine will enforce it during the apply",
            skipped.description, skipped.reason
        );
    }
    for probe in &report.probes {
        // A probe can still legitimately fail to run — a check whose expression
        // names a column this plan renames, say, since expression text is never
        // rewritten by substitution. That is not a violation and it is not
        // silence either: the engine enforces the constraint inside the
        // transaction, where a failure rolls everything back.
        if let Some(approved) = approved {
            approved
                .check(conn, crate::pins::Check::BeforeProbes)
                .await?;
        }
        let answer = probe_query(conn, dialect, &probe.sql).await?;
        if let Some(approved) = approved {
            approved
                .check(conn, crate::pins::Check::BeforeProbes)
                .await?;
        }
        let rows = match answer {
            Ok(rows) => rows,
            // The server's text is left out: it quotes the value it could not
            // convert, and this failure may be recorded (DECISIONS 455).
            Err(e) if probe.refuses_when_it_fails => {
                let code = e.server_error_code();
                failures.push(format!(
                    "{} fails on a stored row (SQLSTATE {})",
                    probe.description,
                    code.as_deref().unwrap_or("unknown")
                ));
                continue;
            }
            Err(e) => {
                unchecked += 1;
                eprintln!(
                    "warning: could not check {} ({e}); the engine will enforce it during the apply",
                    probe.description
                );
                continue;
            }
        };
        // A query that answered but whose count cannot be read is in exactly
        // the same position as one that failed: nobody looked. Defaulting to
        // zero read that silence as "no rows violate this" — the one answer a
        // gate must never give by accident — and, worse, the probe was then
        // counted among those that *passed*.
        let count = match rows
            .first()
            .and_then(|r| r.try_get_at::<i32>(0).ok().flatten())
        {
            Some(c) => c,
            None => {
                unchecked += 1;
                eprintln!(
                    "warning: {} returned no readable count; the engine will enforce it during \
                     the apply",
                    probe.description
                );
                continue;
            }
        };
        passed += 1;
        if count > 0 {
            failures.push(format!("{count} {}", probe.description));
        }
    }
    if !failures.is_empty() {
        bail!(
            "the data will not accept this plan, and nothing has been changed:\n  {}",
            failures.join("\n  ")
        );
    }
    if passed > 0 {
        services::notice(
            progress,
            "apply.preflight",
            format!("Pre-flight: {passed} probe(s) passed against the live data."),
        );
    }
    if unchecked > 0 {
        // Said on stdout as well as in the warnings above: an operator reading
        // "3 probes passed" while a fourth went unchecked has been told
        // something true and something misleading in the same breath.
        services::notice(
            progress,
            "apply.preflight",
            format!(
                "Pre-flight: {unchecked} probe(s) could not be checked here; the engine enforces \
             them inside the transaction."
            ),
        );
    }

    // "No probe" is not "no risk". Saying so keeps the operator's attention
    // where the approval already put it, rather than letting a clean pre-flight
    // read as a clean bill of health.
    let unprobed: Vec<&str> = names
        .changes()
        .iter()
        .flat_map(|change| change.risks.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|r| {
            matches!(
                r,
                pbps_model::RiskClass::Rename | pbps_model::RiskClass::Destructive
            )
        })
        .map(|r| r.as_str())
        .collect();
    if !unprobed.is_empty() {
        services::notice(
            progress,
            "apply.preflight",
            format!(
                "Pre-flight: {} risk(s) cannot be probed and rest on the approval alone.",
                unprobed.join(", ")
            ),
        );
    }
    Ok(())
}

/// One probe, in the dialect's probe transaction when it has one.
///
/// The outer `Result` is the framing, the inner one the probe. A probe the
/// engine refuses — a `nextval` in a `CHECK`, under `READ ONLY` — is an
/// unchecked probe and the caller says so. A framing that cannot open or
/// close is not: a transaction left open would carry the plan's own `BEGIN`
/// inside it, where PostgreSQL only warns, so it stops the apply instead
/// (DECISIONS 537).
async fn probe_query(
    conn: &mut Conn,
    dialect: &dyn pbps_dialect::Dialect,
    sql: &str,
) -> anyhow::Result<Result<Vec<pbps_db::Row>, pbps_db::DbError>> {
    let Some(framing) = dialect.probe_framing() else {
        return Ok(conn.query(sql).await);
    };
    conn.begin(&framing)
        .await
        .context("could not open the transaction a pre-flight probe runs in")?;
    let rows = conn.query(sql).await;
    conn.rollback(&framing)
        .await
        .context("could not close the transaction a pre-flight probe ran in")?;
    Ok(rows)
}

/// §7.5: a plan containing a non-transactional statement fails before it runs.
fn reject_non_transactional(statements: &[pbps_dialect::Statement]) -> anyhow::Result<()> {
    // The statement is SQL out of the plan's names, and a quoted identifier may
    // hold a line break or a terminal sequence; it is shown escaped, with the
    // layout around it the tool's own (DEC-2032.1, #2035).
    let offenders: Vec<String> = statements
        .iter()
        .filter(|s| !s.transactional)
        .map(|s| pbps_model::escape::Escaped(&s.sql).to_string())
        .collect();
    if offenders.is_empty() {
        return Ok(());
    }
    bail!(
        "{} statement(s) in this plan cannot run inside a transaction, and \"one plan, one \
         transaction\" is not negotiable:\n  {}\n\
         Isolate the change in a revision of its own and plan it with `pbps plan --db --staged`, \n\
         which runs it statement by statement with a checkpoint in the ledger (ADR-0003).",
        offenders.len(),
        offenders.join("\n  ")
    )
}

/// The statements as one transaction: all or nothing (SPEC §7.5), left
/// **open** for the caller.
///
/// The rollback is attempted on every failure path and its own error is
/// deliberately not allowed to replace the original one — the statement that
/// broke is what the operator needs to see.
///
/// What follows an apply is the read-back that becomes the recorded state and
/// the ledger entry that records it, and both belong inside this transaction.
/// Committing first left a window another session could write in — a declared
/// row edited, a managed role's grant changed — and the read-back would take
/// that in and record it as this plan's own result: `apply` reporting success,
/// `verify` clean against the newly blessed state, and only the next connected
/// plan proposing the declaration back (DECISIONS 147). It also makes the
/// ledger entry as atomic as the change it describes, where before a failure
/// to write it left the environment changed with nothing saying so.
///
/// [`finish_transaction`] is what closes it, on both paths.
async fn execute_transaction_body(
    conn: &mut Conn,
    dialect: &dyn Dialect,
    statements: &crate::emission::LoweredPlan,
    type_pins: &crate::engine::Prerequisites,
    changes: &pbps_model::ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
) -> anyhow::Result<()> {
    conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
        .await
        .context("cannot open a transaction")?;
    crate::engine::recheck_type_prerequisites(conn, type_pins, changes, trusted, true).await?;
    for position in 0..statements.len() {
        crate::engine::recheck_type_prerequisites(conn, type_pins, changes, trusted, true).await?;
        execute_statement(
            conn,
            statements,
            position,
            &crate::engine::DataWriteGuard::default(),
            None,
            changes,
            trusted,
            None,
        )
        .await?;
    }
    Ok(())
}

async fn execute_statements(
    conn: &mut Conn,
    statements: &crate::emission::LoweredPlan,
    data_guard: &crate::engine::DataWriteGuard,
    reviewed: &crate::pins::Approved<'_>,
    progress: Option<&services::Progress>,
) -> anyhow::Result<()> {
    for position in 0..statements.len() {
        execute_statement(
            conn,
            statements,
            position,
            data_guard,
            Some(reviewed),
            &reviewed.plan.changes,
            &reviewed.project.config.trusted_roles,
            progress,
        )
        .await?;
    }
    Ok(())
}

// Bootstrap checks prerequisites between statements without turning the
// certified collection into a raw slice that another caller could execute.
#[allow(clippy::too_many_arguments)]
async fn execute_statement(
    conn: &mut Conn,
    statements: &crate::emission::LoweredPlan,
    position: usize,
    data_guard: &crate::engine::DataWriteGuard,
    reviewed: Option<&crate::pins::Approved<'_>>,
    changes: &pbps_model::ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
    progress: Option<&services::Progress>,
) -> anyhow::Result<()> {
    let stmt = &statements[position];
    if let Some(approved) = reviewed {
        approved
            .check(
                conn,
                crate::pins::Check::BeforeStatements {
                    executed: &statements[..position],
                },
            )
            .await
            .context("Nothing has been applied — the transaction was rolled back.")?;
    }
    let admission = reviewed.map_or_else(
        || pbps_pg::admission::Admission::without_review(trusted.iter().cloned()),
        crate::pins::Approved::admission,
    );
    crate::engine::check_data_write(conn, &admission, stmt, data_guard).await?;
    crate::engine::check_drop_requirement(conn, &admission, stmt).await?;
    // The routines a table expression calls are judged just before the statement
    // that introduces it, with what the plan created earlier in place (DEC-2563.4).
    if let Some(change) = &stmt.introduces {
        pbps_pg::table_expressions::vet_change(
            conn,
            &admission,
            pbps_pg::pins::Within::CallersTransaction,
            pbps_pg::table_expressions::Moment::Statement,
            change,
        )
        .await
        .and_then(pbps_pg::table_expressions::Vetted::admitted)
        .map_err(anyhow::Error::msg)?;
    }
    // A drop is vetted before it runs: what it would delete is judged while
    // it can still be refused, not after (DEC-2447.8).
    if let Some(extension) = &stmt.extension {
        vet_before_extension(conn, extension, reviewed.map(|a| a.plan)).await?;
    }
    if let Some(progress) = progress {
        progress.started.set(true);
    }
    if let Err(e) = conn.execute(&stmt.sql).await {
        // `.context()`, not `anyhow::anyhow!("...{e}")`: interpolating the
        // driver error into the message text would bake its `Display`
        // into an opaque string with no error left inside to find. Kept
        // as the source instead, `e` survives in this anyhow::Error's
        // chain, which is what lets `crate::engine::ledger_safe_reason`
        // downcast to `DbError::Driver` and redact only that frame later —
        // the server's sentence still reaches the operator through the
        // top-level context here plus `{:#}` at the CLI's own top level
        // (DECISIONS 455).
        return Err(anyhow::Error::new(e).context(format!(
            "the database rejected this statement, and the whole plan was rolled back:\n{}",
            stmt.sql
        )));
    }
    if let Some(type_statement) = &stmt.declared_type {
        pbps_pg::type_refs::declared::vet(
            conn,
            type_statement,
            &admission,
            reviewed.map_or(&[][..], |a| a.plan.declared_type_pins.as_slice()),
            changes,
            &statements[..=position],
        )
        .await?;
    }
    if let Some(extension) = &stmt.extension {
        vet_after_extension(conn, extension, reviewed.map(|a| a.plan)).await?;
    }
    Ok(())
}

/// What an extension statement is held to before it runs, by what it does:
/// a drop's manifest and reach (DEC-2447.8), an update's manifest, versions
/// and dependents (DEC-2447.10). An install is vetted after only.
async fn vet_before_extension(
    conn: &mut Conn,
    extension: &pbps_dialect::ExtensionStatement,
    plan: Option<&pbps_model::SavedPlan>,
) -> anyhow::Result<()> {
    use pbps_dialect::ExtensionPhase;
    match &extension.phase {
        ExtensionPhase::Create => Ok(()),
        ExtensionPhase::Drop => {
            let pins = plan.map_or(&[][..], |p| p.extension_drops.as_slice());
            vet_before_extension_drop(conn, extension, pins).await
        }
        ExtensionPhase::Update { from } | ExtensionPhase::SetSchema { from } => {
            let pin = sealed_change(plan, extension, "vetted")?;
            let moving = matches!(extension.phase, ExtensionPhase::SetSchema { .. });
            pbps_pg::extension_provider::vet_before_change(
                conn,
                extension.name.raw(),
                pin,
                from,
                moving,
            )
            .await
            .map_err(|error| {
                anyhow::Error::new(error).context("nothing was changed and nothing was recorded")
            })
        }
    }
}

/// The pin a package change was sealed with, which every update and move of the
/// plan has (admission demands it, so a miss here is a plan that did not come
/// through it).
fn sealed_change<'a>(
    plan: Option<&'a pbps_model::SavedPlan>,
    extension: &pbps_dialect::ExtensionStatement,
    what: &str,
) -> anyhow::Result<&'a pbps_model::ExtensionChangePin> {
    plan.into_iter()
        .flat_map(|p| p.extension_changes.iter())
        .find(|p| p.uid == extension.uid && p.name == extension.name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "extension `{}` has no pinned manifest in the plan, so its change cannot be {what}",
                extension.name.shown()
            )
        })
}

/// What it is held to right after, before its transaction commits.
async fn vet_after_extension(
    conn: &mut Conn,
    extension: &pbps_dialect::ExtensionStatement,
    plan: Option<&pbps_model::SavedPlan>,
) -> anyhow::Result<()> {
    use pbps_dialect::ExtensionPhase;
    match &extension.phase {
        ExtensionPhase::Create => {
            let pins = plan.map_or(&[][..], |p| p.extension_pins.as_slice());
            vet_installed_extension(conn, extension, pins).await
        }
        ExtensionPhase::Drop => confirm_extension_dropped(conn, extension).await,
        ExtensionPhase::Update { .. } | ExtensionPhase::SetSchema { .. } => {
            let pin = sealed_change(plan, extension, "confirmed")?;
            let moving = matches!(extension.phase, ExtensionPhase::SetSchema { .. });
            pbps_pg::extension_provider::confirm_changed(conn, extension.name.raw(), pin, moving)
                .await
                .map_err(|error| {
                    anyhow::Error::new(error)
                        .context("the change was rolled back and nothing was recorded")
                })
        }
    }
}

/// Holds what a `CREATE EXTENSION` just installed to the manifest the plan was
/// approved over. Called before the transaction that ran it commits, so any
/// difference, unreadable read or missing pin takes the install back with it
/// and records nothing (DEC-2447.6).
async fn vet_installed_extension(
    conn: &mut Conn,
    installed: &pbps_dialect::ExtensionStatement,
    pins: &[pbps_model::ExtensionPin],
) -> anyhow::Result<()> {
    let pin = pins
        .iter()
        .find(|p| p.uid == installed.uid && p.name == installed.name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "extension `{}` has no pinned manifest in the plan, so what it installed cannot be vetted",
                installed.name.shown()
            )
        })?;
    pbps_pg::extension_provider::vet(conn, installed.name.raw(), &pin.manifest)
        .await
        .map_err(|error| {
            anyhow::Error::new(error)
                .context("the install was rolled back and nothing was recorded")
        })
}

/// The check before a `DROP EXTENSION` runs, in the statement's own
/// transaction: the manifest and the objects outside it must be what the plan
/// was approved over, and a pin must exist (DEC-2447.8).
async fn vet_before_extension_drop(
    conn: &mut Conn,
    dropped: &pbps_dialect::ExtensionStatement,
    pins: &[pbps_model::ExtensionDropPin],
) -> anyhow::Result<()> {
    let pin = pins
        .iter()
        .find(|p| p.uid == dropped.uid && p.name == dropped.name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "extension `{}` has no pinned manifest in the plan, so what its drop reaches cannot be vetted",
                dropped.name.shown()
            )
        })?;
    pbps_pg::extension_provider::vet_before_drop(conn, dropped.name.raw(), pin)
        .await
        .map_err(|error| {
            anyhow::Error::new(error).context("nothing was dropped and nothing was recorded")
        })
}

/// The check right after a `DROP EXTENSION`, before its transaction commits:
/// the package and everything reachable from it are gone, or the drop is taken
/// back with the rest (DEC-2447.8).
async fn confirm_extension_dropped(
    conn: &mut Conn,
    dropped: &pbps_dialect::ExtensionStatement,
) -> anyhow::Result<()> {
    pbps_pg::extension_provider::confirm_dropped(conn, dropped.name.raw())
        .await
        .map_err(|error| {
            anyhow::Error::new(error).context("the drop was rolled back and nothing was recorded")
        })
}

/// The opening check of an apply that installs packages, under the lock: the
/// target must still be one the plan was made against, with the same sentences
/// the planner gives. Nothing has run when it refuses.
async fn recheck_extension_creates(
    conn: &mut Conn,
    plan: &pbps_model::SavedPlan,
) -> anyhow::Result<()> {
    if !plan.extension_drops.is_empty() {
        // Each package this plan drops is still installed as it was pinned and
        // the role may still drop it: the planner's sentences, before a
        // statement runs.
        let names: BTreeSet<String> = plan
            .extension_drops
            .iter()
            .map(|p| p.name.raw().to_owned())
            .collect();
        let mut problems = pbps_pg::extension_provider::drop_problems(conn, &names).await?;
        let held = pbps_pg::extension_provider::installed(conn).await?;
        for pin in &plan.extension_drops {
            if held
                .get(pin.name.raw())
                .is_some_and(|d| *d != pin.definition)
            {
                problems.push(format!(
                    "extension `{}` is no longer installed as it was planned",
                    pin.name.shown()
                ));
            }
        }
        anyhow::ensure!(
            problems.is_empty(),
            "nothing was applied: {}",
            problems.join("; ")
        );
    }
    if !plan.extension_changes.is_empty() {
        // Each package this plan updates or moves is still installed as it was
        // pinned, the engine still has the path, and the role may do each, with
        // the planner's sentences.
        let (mut updates, mut moves) = (BTreeMap::new(), BTreeMap::new());
        for planned in &plan.changes.changes {
            if let pbps_model::Change::UpdateExtension { name, from, to, .. } = &planned.change {
                updates.insert(name.raw().to_owned(), (from.clone(), to.clone()));
            } else if let pbps_model::Change::SetExtensionSchema { name, from, to, .. } =
                &planned.change
            {
                moves.insert(name.raw().to_owned(), (from.clone(), to.clone()));
            }
        }
        let creating: BTreeMap<String, pbps_model::Extension> = plan
            .extension_pins
            .iter()
            .map(|p| (p.name.raw().to_owned(), p.definition.clone()))
            .collect();
        let mut problems =
            pbps_pg::extension_provider::update_problems(conn, &updates, &creating).await?;
        problems.extend(
            pbps_pg::extension_provider::move_problems(
                conn,
                &moves,
                &updates.keys().cloned().collect(),
            )
            .await?,
        );
        anyhow::ensure!(
            problems.is_empty(),
            "nothing was applied: {}",
            problems.join("; ")
        );
    }
    if plan.extension_pins.is_empty() {
        return Ok(());
    }
    let target = pbps_pg::extension_provider::target(conn).await?;
    let pinned: BTreeMap<String, pbps_model::Extension> = plan
        .extension_pins
        .iter()
        .map(|p| (p.name.raw().to_owned(), p.definition.clone()))
        .collect();
    let problems: Vec<String> = plan
        .extension_pins
        .iter()
        .flat_map(|p| {
            pbps_pg::extension_provider::create_problems(
                &target,
                p.name.raw(),
                &p.definition,
                &pinned,
            )
        })
        .collect();
    anyhow::ensure!(
        problems.is_empty(),
        "nothing was applied: {}",
        problems.join("; ")
    );
    Ok(())
}

/// Commits only after every post-DDL operation succeeded; otherwise rolls back
/// while preserving the original error as the useful one.
///
/// Everything between the statements and the commit is inside the transaction,
/// so a failure there has to undo the statements too — an apply whose
/// read-back or ledger entry failed has changed nothing (147).
async fn finish_transaction<T>(
    conn: &mut Conn,
    dialect: &dyn Dialect,
    result: anyhow::Result<T>,
) -> anyhow::Result<T> {
    let framing = dialect.transaction_framing(&pin_zone(conn));
    match result {
        Ok(value) => {
            if let Err(error) = conn.commit(&framing).await {
                let _ = conn.rollback(&framing).await;
                return Err(
                    anyhow::Error::new(error).context("the transaction could not be committed")
                );
            }
            Ok(value)
        }
        Err(error) => {
            let _ = conn.rollback(&framing).await;
            Err(error)
        }
    }
}

/// Stamps a snapshot with where it came from.
fn with_provenance(root: &std::path::Path, mut snapshot: StateSnapshot) -> StateSnapshot {
    snapshot.git_sha = db::git_sha(root);
    snapshot
}

/// The modules this plan drops, spelled as the catalog spells a referrer.
///
/// `rename_impact` names a referrer `schema.name` from `sys.objects`, and a
/// trigger's `ModuleId` is `schema.table.name` — so the id's own string form
/// matched nothing, a trigger this plan drops before the rename stayed in the
/// report, and a schema-bound one refused the plan whose first statement
/// removes it. The object name is what the catalog has (ADR-0009 §1).
/// Leaves out of a rename's impact report the referrers this plan drops
/// first, before anything is counted or printed — so an empty remainder
/// prints no heading and the notes are computed from what is left.
///
/// Blocking and advisory referrers are matched by name, as they always were.
/// A carried one is matched only by the typed module the engine proved its
/// removal with: its display is the engine's description, so a name match
/// would never fire on it, and a looser one would excuse a same-named
/// survivor (#823).
fn excuse_dropped_referrers(
    report: &mut pbps_db::impact::ImpactReport,
    dropped: &BTreeSet<String>,
    dropped_modules: &BTreeSet<(pbps_model::ModuleKind, pbps_model::ModuleId)>,
) {
    report.blocking.retain(|r| !dropped.contains(&r.name));
    report.advisory.retain(|r| !dropped.contains(&r.name));
    report.carried.retain(|r| {
        r.removed_with
            .as_ref()
            .is_none_or(|key| !dropped_modules.contains(key))
    });
}

/// Every module this plan drops outright, by its full typed identity. A
/// rebuild (`AlterModule`) keeps the module, so it is not here.
fn dropped_modules(
    changes: &pbps_model::ChangeSet,
) -> BTreeSet<(pbps_model::ModuleKind, pbps_model::ModuleId)> {
    changes
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::DropModule { id, kind } = &p.change {
                Some((*kind, id.clone()))
            } else {
                None
            }
        })
        .collect()
}

fn dropped_referrer_names(changes: &pbps_model::ChangeSet) -> BTreeSet<String> {
    changes
        .changes
        .iter()
        .filter(|p| matches!(p.change, pbps_model::Change::DropModule { .. }))
        .filter_map(|p| p.change.module_id())
        .map(|id| id.object_name().raw().to_string())
        .collect()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod plan_name_tests;

#[cfg(test)]
mod local_name_key_tests;

#[cfg(test)]
mod held_before_the_plan_tests;

#[cfg(test)]
mod data_work_failure_tests;

#[cfg(test)]
mod extension_guard_tests;

#[cfg(test)]
mod search_drift_tests;

#[cfg(test)]
mod comment_drift_tests;
