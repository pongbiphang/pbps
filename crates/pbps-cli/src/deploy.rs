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

use anyhow::{Context as _, bail};

use std::collections::{BTreeMap, BTreeSet};

use pbps_config::Project;
use pbps_db::Conn;
use pbps_dialect::Dialect;
use pbps_model::{
    DataScopes, IdsFile, ModuleId, ObjectName, ObservedRows, RowScope, Schema, StateKind,
    StateSnapshot, TableName,
};

use crate::db::{self, Target};

/// A scoped live state, plus the declared modules the read could not express.
///
/// The second list cannot live in [`pbps_diff::Scoped`] — "pbps cannot read
/// this back" is a dialect fact, and the differ knows no dialect — but a caller
/// that has to decide whether an object is *there* needs both: a module the
/// catalog holds and introspection cannot reproduce is inside the managed set
/// by name and outside the scoped schema in fact.
pub struct Managed {
    pub scoped: pbps_diff::Scoped,
    /// Catalog facts inside the managed set that the projection could not
    /// express: an unsupported feature on a managed table, or a module the set
    /// names that introspection cannot read back. They are drift findings,
    /// never ignorable warnings, and no recorder accepts a schema that has any
    /// (see [`managed_limitations`]).
    pub limitations: Vec<String>,
    /// Every module in the database that introspection cannot express, by name,
    /// with the reason already rendered.
    ///
    /// Returned in full rather than filtered to the managed set, because the
    /// caller that most needs it is asking about a name that has *never* been
    /// recorded: a newly declared module that collides with an encrypted one
    /// already standing there.
    pub unreadable: Vec<(ObjectName, String)>,
    /// The rows read back under the read scope the caller asked for. Not yet
    /// placed in the schema: `plan --db` projects two different views out of
    /// one read (see [`pbps_model::data::read_scopes`]).
    pub rows: ObservedRows,
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

async fn managed_state(
    conn: &mut Conn,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    unmanaged: pbps_config::Unmanaged,
    scopes: &DataScopes,
    reference: &Schema,
    read: crate::engine::Read,
) -> anyhow::Result<pbps_diff::Scoped> {
    let request = ManagedRead {
        ids,
        modules,
        unmanaged,
        scopes,
        reference,
        read,
    };
    managed_state_then(conn, &request, || std::future::ready(Ok(()))).await
}

struct ManagedRead<'a> {
    ids: &'a IdsFile,
    modules: &'a BTreeSet<ModuleId>,
    unmanaged: pbps_config::Unmanaged,
    scopes: &'a DataScopes,
    reference: &'a Schema,
    read: crate::engine::Read,
}

impl ManagedRead<'_> {
    async fn capture(&self, conn: &mut Conn) -> anyhow::Result<pbps_diff::Scoped> {
        managed_state_once(
            conn,
            self.ids,
            self.modules,
            self.unmanaged,
            self.scopes,
            self.reference,
            self.read,
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

async fn managed_state_once(
    conn: &mut Conn,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    unmanaged: pbps_config::Unmanaged,
    scopes: &DataScopes,
    reference: &Schema,
    read: crate::engine::Read,
) -> anyhow::Result<pbps_diff::Scoped> {
    let managed =
        managed_state_full(conn, ids, modules, unmanaged, &rows_to_read(scopes), read).await?;
    refuse_managed_limitations(&managed.limitations)?;
    let mut scoped = managed.scoped;
    scoped.schema = scoped
        .schema
        .with_observed_rows(&managed.rows, scopes, reference)?;
    Ok(scoped)
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
async fn baseline_state(
    conn: &mut Conn,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    unmanaged: pbps_config::Unmanaged,
    scopes: &DataScopes,
    recorded: &Schema,
    as_the_apply_reads_it: &DataScopes,
) -> anyhow::Result<(pbps_diff::Scoped, Schema)> {
    // Before the plan's transaction opens: the baseline is the database as
    // it stands, not as this apply is about to leave it.
    let managed = managed_state_full(
        conn,
        ids,
        modules,
        unmanaged,
        &rows_to_read(scopes),
        crate::engine::Read::Snapshot,
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
    Ok((scoped, comparable))
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
    unmanaged: pbps_config::Unmanaged,
    checked_scopes: &DataScopes,
    recorded: &Schema,
    watched_scopes: &DataScopes,
) -> anyhow::Result<(pbps_diff::Scoped, Schema)> {
    let pulled = pull(conn, crate::engine::Read::Snapshot).await?;
    let unreadable = unreadable_modules(&pulled.unmanaged_modules);
    // Over the *watched* set, which contains the checked one: a module this
    // plan is about to write that the catalog cannot read back is a
    // limitation, and it is only in the wider set. The two reads this
    // replaces each refused over their own set, so refusing over the union is
    // what keeps that (491edd9, DECISIONS 174).
    debug_assert!(checked.is_subset(watched));
    refuse_managed_limitations(&managed_limitations(&pulled, ids, watched))?;

    let mut scoped = cut(&pulled, ids, checked, &unreadable, unmanaged)?;
    // The watch cut is the one the row read is taken against, because it is
    // the wider of the two: a module the plan creates is in it and not in the
    // checksum's, and no table is in either alone. `Unmanaged::Ignore`
    // because the policy has already been applied, by the cut above — running
    // it twice would report the same object twice.
    let watching = cut(
        &pulled,
        ids,
        watched,
        &unreadable,
        pbps_config::Unmanaged::Ignore,
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
    Ok((scoped, previous))
}

/// Introspects, cuts the result down to the managed set, reads the rows of
/// every table in `read`, and reports whatever the read itself could not
/// express.
///
/// Every connected command starts here. The warnings are printed rather than
/// returned because ignoring them is never right: a drift check that silently
/// skipped a computed column would report "no drift" about a database it only
/// half read.
///
/// The rows come from the catalog under a scope the caller supplies, because
/// which of a table's rows are declared is not something the database knows
/// (ADR-0004). A row read that fails is an error, not an empty answer: a
/// state recorded without the rows would say the table declares none.
async fn managed_state_full(
    conn: &mut Conn,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    unmanaged: pbps_config::Unmanaged,
    rows: &BTreeMap<TableName, RowScope>,
    read: crate::engine::Read,
) -> anyhow::Result<Managed> {
    let pulled = pull(conn, read).await?;
    let unreadable = unreadable_modules(&pulled.unmanaged_modules);
    let limitations = managed_limitations(&pulled, ids, modules);
    let scoped = cut(&pulled, ids, modules, &unreadable, unmanaged)?;
    let rows = crate::engine::read_rows(conn, &scoped.schema, rows, read)
        .await
        .context("cannot read the declared rows back")?;
    Ok(Managed {
        scoped,
        limitations,
        unreadable,
        rows,
    })
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
    let pulled = crate::engine::introspect(conn, read)
        .await
        .context("cannot read the database catalog")?;
    for w in &pulled.warnings {
        eprintln!("warning: {w}");
    }
    Ok(pulled)
}

/// One cut of a pulled catalog down to a managed set. Pure.
fn cut(
    pulled: &pbps_db::catalog::Pulled,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    unreadable: &[(ObjectName, String)],
    unmanaged: pbps_config::Unmanaged,
) -> anyhow::Result<pbps_diff::Scoped> {
    let mut scoped = pbps_diff::scope(&pulled.schema, ids, modules);
    // A managed role's grant WITH GRANT OPTION is wider than the plain grant
    // the declarations can spell, and was left out of the role's set rather
    // than folded in. Carried beside the comparison so `verify` reports it
    // as drift and `plan --db` refuses to plan over it; on an unmanaged role,
    // or on somebody else's object, it is not ours (DECISIONS 95, 176).
    for what in unexpressible_permissions(pulled, ids, modules) {
        eprintln!("warning: {what}");
        scoped.unexpressible.push(what.to_owned());
    }
    report_unmanaged(&scoped, unreadable, modules, unmanaged)?;
    Ok(scoped)
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
    wanted: &[String],
    vacated: &[String],
) -> anyhow::Result<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    let wanted: Vec<&str> = wanted.iter().map(String::as_str).collect();
    let vacated: Vec<&str> = vacated.iter().map(String::as_str).collect();
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

/// The role names the statements after `completed` still have to find free
/// (`CREATE ROLE`, the new name of a rename) and the ones they free first
/// (`DROP ROLE`, the old name of a rename): `(wanted, vacated)`. Counted per
/// statement the way `role_drop_expectations` counts, so a staged resume
/// asks only about the names its remaining statements touch (DECISIONS 119).
fn role_name_expectations(
    cs: &pbps_model::ChangeSet,
    dialect: &dyn pbps_dialect::Dialect,
    completed: usize,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let mut wanted = Vec::new();
    let mut vacated = Vec::new();
    let mut at = 0usize;
    for p in &cs.changes {
        let n = dialect
            .emit(&p.change, p.strategy)
            .map_err(|e| anyhow::anyhow!("cannot render a change as SQL: {e}"))?
            .len();
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
    for (name, table) in &schema.tables {
        let live_table = live_name(name, final_ids, live_ids);
        // Only the key column is named to the catalog; every other column
        // reaches it as a converted literal, under no name at all.
        let live_key = table
            .primary_key
            .as_ref()
            .filter(|pk| pk.columns.len() == 1)
            .map(|pk| pbps_model::ColumnRef::new(name.clone(), pk.columns[0].clone()))
            .and_then(|r| final_ids.column_uid(&r).cloned())
            .and_then(|uid| live_ids.columns.get(&uid))
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

/// Refuses a declaration whose text the engine would not read back as
/// written, before anything is written (DECISIONS 101).
///
/// Asked of the engine rather than reasoned about: `"1.5"` in a
/// `decimal(5,2)` comes back `1.50`, `"ab "` in a `char(5)` comes back `ab`,
/// and a text the type cannot read at all comes back as a failed insert —
/// each a plan that never converges or never applies, and each a question
/// only the engine answers the same way it will answer at read time.
pub(crate) async fn refuse_misspelt(
    conn: &mut Conn,
    schema: &Schema,
    at: &pbps_db::catalog::CatalogNames,
) -> anyhow::Result<()> {
    let found = crate::engine::misspelt(conn, schema, at)
        .await
        .context("cannot ask the engine how it reads the declared rows")?;
    if found.misspelt.is_empty() && found.conflicts.is_empty() {
        return Ok(());
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
                (None, _) => format!(
                    "{} row key `{}` cannot be read as {} by the engine",
                    m.table, m.key, m.ty
                ),
                (Some(c), None) => format!(
                    "{} row `{}`: `{c}` = {:?} cannot be read as {} by the engine",
                    m.table, m.key, m.declared, m.ty
                ),
                (Some(c), Some(canonical)) => format!(
                    "{} row `{}`: `{c}` is written {:?}, and the engine reads it back as {:?}; \
                 write it that way",
                    m.table, m.key, m.declared, canonical
                ),
            }),
    );
    bail!(
        "{} declared value(s) would not come back as written:\n  {}\n\
         A declaration that disagrees with its own database on every plan is worse than \
         none; the engine's spelling is the one to write (DECISIONS 101, 106).",
        lines.len(),
        lines.join("\n  ")
    );
}

/// Every schema name a declaration spells, and one declaration that spells it
/// that way — for the message, so the refusal names something the author can
/// find in a file rather than a bare schema name.
fn schemas_declared(schema: &Schema) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (name, role) in &schema.roles {
        for target in role.grants.keys() {
            if let pbps_model::GrantTarget::Schema(s) = target {
                out.entry(s.clone())
                    .or_insert_with(|| format!("granted to role `{name}`"));
            }
        }
    }
    // The schema half of a qualified name is text on both sides too, and the
    // uid that matches the *table* does not match it: the managed set is
    // scoped by name, so `DBO.customer` on a database whose schema is `dbo`
    // is bootstrapped, recorded as a state holding no tables at all, and
    // reported as drift by the `verify` immediately after.
    for name in schema.tables.keys() {
        out.entry(name.schema.clone())
            .or_insert_with(|| format!("the schema of `{name}`"));
    }
    for id in schema.modules.keys() {
        out.entry(id.schema().to_owned())
            .or_insert_with(|| format!("the schema of `{id}`"));
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
        let live = live_name(&column.table, final_ids, live_ids);
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
                column.table,
                column.name,
                aliased.join(", ")
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
        .map(|(final_name, table)| (live_name(final_name, final_ids, live_ids), table.clone()))
        .collect();
    out
}

/// The name the database has for the table this plan calls `final_name` —
/// the same one for everything this plan does not rename.
///
/// Everything keyed by the live names (the declarations re-keyed by
/// [`tables_under`], the rows read back, the scopes) has to be looked up
/// through this, and a change's own table name is always the *final* one.
fn live_name(final_name: &TableName, final_ids: &IdsFile, live_ids: &IdsFile) -> TableName {
    final_ids
        .table_uid(final_name)
        .and_then(|uid| live_ids.tables.get(uid))
        .cloned()
        .unwrap_or_else(|| final_name.clone())
}

/// [`scopes_at`] on its parts: `data` keyed by the names `final_ids` gives
/// each table, re-keyed by the names `live_ids` gives the same uids.
fn scopes_under(data: &DataScopes, final_ids: &IdsFile, live_ids: &IdsFile) -> DataScopes {
    data.iter()
        .map(|(final_name, scope)| (live_name(final_name, final_ids, live_ids), scope.clone()))
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
) -> Vec<String> {
    let managed_tables: std::collections::BTreeSet<_> = ids.tables.values().collect();
    pulled
        .limitations
        .iter()
        .filter(|limitation| match &limitation.target {
            pbps_db::catalog::LimitationTarget::Relation(name) => {
                managed_tables.contains(name) || modules.contains(&ModuleId::Named(name.clone()))
            }
            pbps_db::catalog::LimitationTarget::Module(id) => modules.contains(id),
            pbps_db::catalog::LimitationTarget::UnnameableModule(_) => false,
        })
        .map(|limitation| limitation.detail.clone())
        .chain(
            pulled
                .unmanaged_modules
                .iter()
                // An unmanageable module is known by the name the catalog
                // gave it, so the managed set is asked under the same name:
                // whatever a module's identity holds, that is what it is
                // called (ADR-0009 §1).
                .filter(|m| modules.iter().any(|id| id.object_name() == m.name))
                .map(|m| format!("{} {} is in the managed set, but {}", m.kind, m.name, m.why)),
        )
        .collect()
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
    let managed_tables: BTreeSet<&TableName> = ids.tables.values().collect();
    pulled
        .unexpressible
        .iter()
        .filter(|u| ids.roles.values().any(|managed| managed == &u.role))
        .filter(|u| match &u.target {
            // In the relation namespace, the same question `pbps_diff::scope`
            // asks of the plain grant beside it: an id carrying a signature is
            // a routine on an engine that overloads, and there a table of that
            // name is a different object. Counted, a limitation on somebody
            // else's table was kept — and refused every plan — because a
            // managed routine happened to share its name (DECISIONS 176, 384).
            Some(pbps_model::GrantTarget::Object(o)) => {
                managed_tables.contains(o)
                    || modules.iter().any(|id| {
                        !matches!(id, ModuleId::Routine(_))
                            && id.referenced_name().as_ref() == Some(o)
                    })
            }
            Some(pbps_model::GrantTarget::Routine(r)) => modules
                .iter()
                .any(|id| matches!(id, ModuleId::Routine(other) if other == r)),
            Some(pbps_model::GrantTarget::Schema(_)) | None => true,
        })
        .map(|u| u.what.as_str())
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
fn managed_modules(
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
             Release it with `pbps unlock` before the next attempt."
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

/// Converts the dialect's unreadable-module inventory into the common name and
/// description form used by connected commands.
pub(crate) fn unreadable_modules(
    modules: &[pbps_db::catalog::UnmanagedModule],
) -> Vec<(pbps_model::ObjectName, String)> {
    modules
        .iter()
        .map(|module| {
            (
                module.name.clone(),
                format!("{} {} ({})", module.kind, module.name, module.why),
            )
        })
        .collect()
}

/// Names every representable or unreadable catalog object outside the managed
/// set — tables, modules and roles alike — for applying the `unmanaged:`
/// policy of SPEC §8.2.
pub(crate) fn unmanaged_objects(
    scoped: &pbps_diff::Scoped,
    unreadable: &[(pbps_model::ObjectName, String)],
    managed_modules: &std::collections::BTreeSet<ModuleId>,
) -> Vec<String> {
    scoped
        .unmanaged
        .iter()
        .map(ToString::to_string)
        .chain(scoped.unmanaged_modules.iter().map(ToString::to_string))
        // A role is a principal rather than an object, so it has no
        // `TableName` to be listed under and needs its own spelling — but the
        // policy is about everything in the database this project does not
        // declare, and an undeclared role is one of those (ADR-0005).
        .chain(scoped.unmanaged_roles.iter().map(|r| format!("role {r}")))
        // Unreadable modules are absent from `scoped.schema`, but they are
        // still catalog objects. If their name is outside the supplied module
        // set, the unmanaged policy applies exactly as it does to a readable
        // module. Without this half, encryption was an accidental escape from
        // `unmanaged: error`.
        .chain(
            unreadable
                .iter()
                .filter(|(name, _)| !managed_modules.iter().any(|id| id.object_name() == *name))
                .map(|(_, description)| description.clone()),
        )
        .collect()
}

/// Applies the `unmanaged:` policy to both representable and unreadable
/// catalog objects outside the managed set.
fn report_unmanaged(
    scoped: &pbps_diff::Scoped,
    unreadable: &[(pbps_model::ObjectName, String)],
    managed_modules: &std::collections::BTreeSet<ModuleId>,
    policy: pbps_config::Unmanaged,
) -> anyhow::Result<()> {
    let names = unmanaged_objects(scoped, unreadable, managed_modules);
    apply_unmanaged_policy(&names, policy)
}

fn apply_unmanaged_policy(names: &[String], policy: pbps_config::Unmanaged) -> anyhow::Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    match policy {
        pbps_config::Unmanaged::Ignore => {}
        pbps_config::Unmanaged::Warn => eprintln!(
            "warning: {} object(s) in this database are not declared and are left alone: {}",
            names.len(),
            names.join(", ")
        ),
        pbps_config::Unmanaged::Error => {
            return Err(UnmanagedPolicy(format!(
                "`unmanaged: error` in pbps.yml, and {} object(s) here are not declared: {}.\n\
                 Declare them (`pbps pull` reverse-generates them) or relax the setting.",
                names.len(),
                names.join(", ")
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
        let names: Vec<String> = scoped.missing.iter().map(ToString::to_string).collect();
        eprintln!(
            "warning: the identity file names {} table(s) this database does not have: {}",
            names.len(),
            names.join(", ")
        );
    }
    if !scoped.missing_roles.is_empty() {
        eprintln!(
            "warning: the identity file names {} role(s) this database does not have: {}",
            scoped.missing_roles.len(),
            scoped.missing_roles.join(", ")
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
        scoped.unexpressible.join("\n  ")
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

/// Whether a column read back is the one a declaration asked for, in the
/// fields the catalog reads back unchanged: the normalized type, the
/// nullability, the identity, and whether there is a default at all — its
/// **text** is the engine's (`0` comes back `((0))`), so only its presence is
/// comparable (DECISIONS 185, 186).
fn column_as_declared(
    dialect: &dyn pbps_dialect::Dialect,
    declared: &pbps_model::Column,
    now: &pbps_model::Column,
) -> bool {
    same_type(dialect, &declared.ty, &now.ty)
        && declared.nullable == now.nullable
        && declared.identity == now.identity
        && declared.default.is_some() == now.default.is_some()
}

/// Whether an index read back is the one declared. Structure only: the
/// filter's text is the engine's to rewrite, and whether there is one at all
/// decides which rows the index covers, and is not (DECISIONS 185).
fn index_as_declared(declared: &pbps_model::Index, now: &pbps_model::Index) -> bool {
    declared.columns == now.columns
        && declared.include == now.include
        && declared.unique == now.unique
        && declared.filter.is_some() == now.filter.is_some()
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
}

/// Whether a part read back is the one the plan adds it as, or `None` where
/// the definition has nothing a read-back can be held to: a check is nothing
/// but an expression and SQL Server rewrites it (167), so its name is all
/// that is comparable (DECISIONS 183).
///
/// `None` as well where the part is not there at all: absent and wrong are
/// two different findings, and the presence check answers for the first.
fn part_as_planned(
    planned: pbps_model::PartDefinition<'_>,
    table: &pbps_model::Table,
    name: &str,
) -> Option<bool> {
    use pbps_model::PartDefinition;
    Some(match planned {
        PartDefinition::PrimaryKey(was) => {
            primary_key_as_declared(was, table.primary_key.as_ref()?)
        }
        // A unique constraint is nothing but its columns.
        PartDefinition::Unique(was) => table.unique.get(name)? == was,
        // And a foreign key nothing but structure — the columns, the parent
        // and the two referential actions — so all of it is comparable (182).
        PartDefinition::ForeignKey(was) => table.foreign_keys.get(name)? == was,
        PartDefinition::Check(_) => return None,
        PartDefinition::Index(was) => index_as_declared(was, table.indexes.get(name)?),
    })
}

/// Every field of a column two reads disagree on, with the word to call it.
///
/// Field by field because the exclusion above is: a plan that retypes a column
/// cannot be held to what the type became — only the engine's stored form says
/// that — while the same column's default, identity and nullability came back
/// from two reads like anything else (DECISIONS 173).
///
/// `description` and `deprecated` are compared too, and are always equal today
/// because no catalog reads them back. That is the right way round: if one
/// ever does, a description another session changed is drift, and no change of
/// this model moves it.
fn differing(
    was: &pbps_model::Column,
    now: &pbps_model::Column,
) -> Vec<(Option<pbps_model::ColumnField>, &'static str)> {
    use pbps_model::ColumnField;
    let mut out = Vec::new();
    if was.ty != now.ty {
        out.push((Some(ColumnField::Type), "type"));
    }
    if was.nullable != now.nullable {
        out.push((Some(ColumnField::Nullable), "nullability"));
    }
    if was.default != now.default {
        out.push((Some(ColumnField::Default), "default"));
    }
    if was.deprecated != now.deprecated {
        out.push((Some(ColumnField::Deprecated), "deprecation"));
    }
    // `None` is "no change of this model moves this", so nothing can excuse
    // it: an identity or a description that differs across an apply is
    // somebody else's work, and the whole-column exclusion used to hide it.
    if was.identity != now.identity {
        out.push((None, "identity"));
    }
    if was.description != now.description {
        out.push((None, "description"));
    }
    out
}

fn refuse_unplanned_movement(
    dialect: &dyn pbps_dialect::Dialect,
    changes: &pbps_model::ChangeSet,
    before: &Schema,
    after: &Schema,
    label: &str,
    settled: Settled,
) -> anyhow::Result<()> {
    let mut objects: BTreeSet<&TableName> = BTreeSet::new();
    // Modules are counted in their own set, under their own identity: a
    // module is no longer a name in the tables namespace (ADR-0009 §1), and
    // asking whether the plan touched `app.f` would exempt every overload of
    // it from the comparison below.
    let mut touched_modules: BTreeSet<&pbps_model::ModuleId> = BTreeSet::new();
    let mut roles: BTreeSet<&str> = BTreeSet::new();
    for p in &changes.changes {
        objects.extend(p.change.objects());
        touched_modules.extend(p.change.module_id());
        roles.extend(p.change.roles());
    }

    // The rows this plan writes, under the name it gives their table, and the
    // old name too where it renames one — the two states are keyed by
    // different names across a rename.
    let mut renamed: BTreeMap<&TableName, &TableName> = BTreeMap::new();
    let mut renamed_roles: BTreeMap<&str, &str> = BTreeMap::new();
    // And the columns, under the name each ends with: across the read that
    // spans the rename statement, the column is under one name before and
    // another after, and the shape comparison follows it rather than
    // excusing both ends (DECISIONS 189).
    let mut renamed_columns: BTreeMap<(&TableName, &str), &str> = BTreeMap::new();
    let mut written: BTreeMap<&TableName, BTreeSet<&pbps_model::RowKey>> = BTreeMap::new();
    // The permissions this plan moves, keyed by the role it moves them on and
    // the target they sit on. A role can be both granted and revoked on one
    // target in one plan, so the sets are unioned rather than replaced.
    // What this plan leaves each role holding on each target it touches: the
    // permissions it adds and the ones it takes away, kept apart because the
    // comparison below reconstructs the set rather than excusing it.
    type Permissions = BTreeSet<pbps_model::Permission>;
    let mut granting: BTreeMap<(&str, &pbps_model::GrantTarget), Permissions> = BTreeMap::new();
    let mut revoking: BTreeMap<(&str, &pbps_model::GrantTarget), Permissions> = BTreeMap::new();
    // The columns this plan changes, under the table they are on, and the
    // objects it removes outright. Both are things the plan does to a
    // *container* that show up somewhere else without any change of its own
    // saying so: a renamed column re-keys every row of its table, and a
    // dropped table takes its grants with it (DECISIONS 158).
    let mut columns: BTreeMap<TableName, BTreeSet<String>> = BTreeMap::new();
    // Per column *and* field: the reason for excluding anything here is
    // field-sized. Only the engine's stored form says what a retyped column
    // became, but that column's default, identity and nullability still came
    // back from two reads, and a whole-column exclusion meant nothing
    // compared them (DECISIONS 173).
    // The tables this plan creates, with the shape the `CREATE` asks for.
    // They have no baseline entry, so the comparison below cannot reach them
    // any other way (DECISIONS 181).
    let mut created: BTreeMap<&TableName, &pbps_model::Table> = BTreeMap::new();
    // The parts this plan puts on a table by a change of its own. A created
    // table's `CREATE` payload is *not* everything it will hold: the differ
    // takes the foreign keys out of it (`std::mem::take`) and emits each as
    // its own change, because they sort after every create (DECISIONS 182).
    // And the foreign keys' own definitions, which is where they live once
    // the differ has taken them out of the payload: a name restored without
    // one leaves what the key points at unchecked (DECISIONS 184).
    let mut added_fks: BTreeMap<(&TableName, &str), &pbps_model::ForeignKey> = BTreeMap::new();
    let mut added_parts: BTreeMap<&TableName, BTreeSet<(pbps_model::Part, &str)>> = BTreeMap::new();
    let mut redefined: BTreeMap<TableName, BTreeMap<String, BTreeSet<pbps_model::ColumnField>>> =
        BTreeMap::new();
    let mut gone: BTreeSet<pbps_model::Dropped> = BTreeSet::new();
    // The constraints and indexes this plan moves, and the tables whose
    // primary key it sets. Everything else on a touched table then answers for
    // itself, instead of one index change exempting the whole shape
    // (DECISIONS 166).
    // Keyed by the *kind* as well as the name: indexes and constraints are
    // separate namespaces to the engine, so a table can hold an index `x` and
    // a check `x`, and a set of bare names let a planned change to one exempt
    // the other from every comparison (DECISIONS 168).
    let mut constraints: BTreeMap<&TableName, BTreeSet<(pbps_model::Part, &str)>> = BTreeMap::new();
    let mut keys: BTreeSet<&TableName> = BTreeSet::new();
    for p in &changes.changes {
        if let pbps_model::Change::RenameTable { from, to, .. } = &p.change {
            renamed.insert(from, to);
        }
        if let pbps_model::Change::RenameRole { from, to, .. } = &p.change {
            renamed_roles.insert(from, to);
        }
        if let pbps_model::Change::RenameColumn {
            table, from, to, ..
        } = &p.change
        {
            renamed_columns.insert((table, to), from);
        }
        if let Some((table, key, _)) = p.change.row() {
            written.entry(table).or_default().insert(key);
        }
        if let Some((role, target, permissions)) = p.change.grant() {
            let (side, permissions) = match permissions {
                pbps_model::PermissionChange::Granted(p) => (&mut granting, p),
                pbps_model::PermissionChange::Revoked(p) => (&mut revoking, p),
            };
            side.entry((role, target)).or_default().extend(permissions);
        }
        for column in p.change.columns() {
            columns.entry(column.table).or_default().insert(column.name);
        }
        // A second set, because the two comparisons ask different questions of
        // it: `columns` is whose *reading* moved, for the rows; this is whose
        // *definition* moved, for the shape (DECISIONS 170).
        for (column, field) in p.change.columns_redefined() {
            redefined
                .entry(column.table)
                .or_default()
                .entry(column.name)
                .or_default()
                .insert(field);
        }
        if let Some(part) = p.change.constraints() {
            if part.after.presence() == pbps_model::Presence::Present
                && let Some(n) = part.name
            {
                added_parts
                    .entry(part.table)
                    .or_default()
                    .insert((part.part(), n));
            }
            match part.name {
                Some(name) => {
                    constraints
                        .entry(part.table)
                        .or_default()
                        .insert((part.part(), name));
                }
                None => {
                    keys.insert(part.table);
                }
            }
        }
        if let pbps_model::Change::CreateTable { name, table, .. } = &p.change {
            created.insert(name, table.as_ref());
        }
        if let pbps_model::Change::AddForeignKey {
            table,
            name,
            constraint,
        } = &p.change
        {
            added_fks.insert((table, name.as_str()), constraint.as_ref());
        }
        if let Some(dropped) = p.change.drops() {
            gone.insert(dropped);
        }
    }

    // This plan's renames, backwards: the name each renamed thing had, keyed
    // by the name it ends with. The later of the two reads is brought back to
    // the earlier spelling with it, and then compared as it always was.
    //
    // Backwards rather than forwards, and part by part rather than whole,
    // because a staged run is checked after every statement: at a checkpoint
    // some of the renames have happened and some have not, so neither read is
    // in one spelling. A child with two foreign keys to two separately renamed
    // parents is carried for one and not the other. Undoing what *has*
    // happened needs no knowledge of what has not (the same reason `settled`
    // exists, DECISIONS 161).
    //
    // A rename is undone only where the two names cannot be confused. If a
    // read holds *both* spellings, the vacated name has been taken by
    // something else — another session creating a table under it, or adding a
    // column back — and rewinding would collapse two identities into one:
    // a foreign key repointed at the impostor would compare equal to one
    // still on the original, and SPEC 7.6's promise that an untouched object's
    // change is caught at the first checkpoint after it lands would be broken.
    // Left un-rewound, the two spellings simply differ and the read is
    // refused, which is the safe direction for an ambiguity that a
    // name-keyed read cannot resolve. The cost is a plan that renames a table
    // and creates another under the vacated name in one revision: its
    // untouched children report as moved. Splitting that revision is the
    // answer, and refusing beats recording another session's write as this
    // plan's own.
    let holds = |s: &Schema, a: &TableName, b: &TableName| {
        s.tables.contains_key(a) && s.tables.contains_key(b)
    };
    let mut undo = pbps_model::Renames::default();
    for (from, to) in &renamed {
        if holds(before, from, to) || holds(after, from, to) {
            continue;
        }
        undo.rename_table((*to).clone(), (*from).clone());
    }
    // A `RenameColumn` carries its table's *declared* name — the one the
    // rename leaves it under — which is already the key this map wants. And
    // `order_key` runs the table renames (class 1) before the column renames
    // (class 3), so at every read a table whose column has been renamed is
    // already under its new name: the key is never a spelling that did not
    // exist yet.
    let holds_column = |s: &Schema, table: &TableName, a: &str, b: &str| {
        s.tables
            .get(table)
            .is_some_and(|t| t.columns.contains_key(a) && t.columns.contains_key(b))
    };
    for ((table, to), from) in &renamed_columns {
        if holds_column(before, table, from, to) || holds_column(after, table, from, to) {
            continue;
        }
        undo.rename_column(pbps_model::ColumnRef::new((*table).clone(), *to), *from);
    }

    let mut moved = Vec::new();
    let named = |n: &TableName| objects.contains(n);
    // A child's foreign key follows the parent it references through
    // `sp_rename` — the referenced table and its columns are both rewritten by
    // the engine — so the child's shape moves with no change of this plan on
    // it and nothing to excuse it with.
    compare(
        "",
        &before.tables,
        &after.tables,
        named,
        |name: &TableName, was: &pbps_model::Table, now: &pbps_model::Table| {
            undo.apply(was, name) == undo.apply(now, name)
        },
        &mut moved,
    );
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
        .map(|name| renamed.get(name).copied().unwrap_or(name))
        .collect();
    let no_rows = pbps_model::TableData {
        mode: pbps_model::DataMode::Exact,
        rows: Default::default(),
    };
    let touched_tables = before
        .tables
        .iter()
        .map(|(name, was)| (name, was.data.as_ref()))
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
        let now_name = renamed.get(name).copied().unwrap_or(name);
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
        // By name, never by value. What a created column *is* comes back in
        // the engine's spelling, and holding it to the declaration would
        // refuse valid applies — which is why the whole table was exempt
        // before 166. A name nobody declared is not ambiguous that way.
        if let (None, Some(declared), Some(now)) = (
            before.tables.get(name),
            created.get(now_name),
            after.tables.get(now_name),
        ) {
            let no_parts = BTreeSet::new();
            let planned = added_parts.get(now_name).unwrap_or(&no_parts);
            let mut named = |kind: &str,
                             part: Option<pbps_model::Part>,
                             was: BTreeSet<&str>,
                             now: BTreeSet<&str>| {
                // Plus whatever a change of this plan's own adds to it: the
                // payload alone is not what the table will hold.
                let mut was = was;
                if let Some(part) = part {
                    was.extend(planned.iter().filter(|(k, _)| *k == part).map(|(_, n)| *n));
                }
                for extra in now.difference(&was) {
                    moved.push(format!(
                        "{now_name} {kind} `{extra}` is there, and this plan declares no such \
                         {kind}"
                    ));
                }
                // Only once every statement has run: a foreign key is split
                // out of the `CREATE` into a change of its own, and at a
                // checkpoint it may not have been added yet.
                if settled.whole() {
                    for missing in was.difference(&now) {
                        moved.push(format!(
                            "{now_name} {kind} `{missing}` is not there, and this plan's \
                             `CREATE TABLE` declares it"
                        ));
                    }
                }
            };
            // No `Part` for the columns: the differ never splits one out of a
            // `CREATE`, and `AddColumn` is not emitted for a table this plan
            // also creates.
            named(
                "column",
                None,
                declared.columns.keys().map(String::as_str).collect(),
                now.columns.keys().map(String::as_str).collect(),
            );
            named(
                "unique",
                Some(pbps_model::Part::Unique),
                declared.unique.keys().map(String::as_str).collect(),
                now.unique.keys().map(String::as_str).collect(),
            );
            named(
                "foreign key",
                Some(pbps_model::Part::ForeignKey),
                declared.foreign_keys.keys().map(String::as_str).collect(),
                now.foreign_keys.keys().map(String::as_str).collect(),
            );
            named(
                "check",
                Some(pbps_model::Part::Check),
                declared.checks.keys().map(String::as_str).collect(),
                now.checks.keys().map(String::as_str).collect(),
            );
            named(
                "index",
                Some(pbps_model::Part::Index),
                declared.indexes.keys().map(String::as_str).collect(),
                now.indexes.keys().map(String::as_str).collect(),
            );
            // A column's own promise, in the fields the catalog reads back
            // unchanged — the type once normalized, measured: the live
            // created-table test declares bare `decimal`, `char`, `float` and
            // `nvarchar` so a raw comparison would refuse it again
            // (DECISIONS 185, 186).
            for (n, was) in &declared.columns {
                let Some(now) = now.columns.get(n) else {
                    continue;
                };
                if !column_as_declared(dialect, was, now) {
                    moved.push(format!(
                        "{now_name} column `{n}` is not the one this plan's `CREATE TABLE` \
                         declares"
                    ));
                }
            }
            // And what those parts *are*, where the declaration says it
            // without the engine's help. Only structure: a check is nothing
            // but an expression and SQL Server rewrites it (167), and an
            // index's filter is one too, so those two fields are left to the
            // name comparison above (DECISIONS 183).
            for (n, was) in &declared.unique {
                if let Some(now) = now.unique.get(n)
                    && was != now
                {
                    moved.push(format!(
                        "{now_name} unique `{n}` is not the one this plan's `CREATE TABLE` \
                         declares"
                    ));
                }
            }
            // A foreign key is nothing but structure — the columns, the
            // parent and the two referential actions — so all of it is
            // comparable, and its definition is on the change rather than in
            // the payload (182).
            for (n, now) in &now.foreign_keys {
                if let Some(was) = added_fks.get(&(now_name, n.as_str()))
                    && *was != now
                {
                    moved.push(format!(
                        "{now_name} foreign key `{n}` is not the one this plan adds"
                    ));
                }
            }
            for (n, was) in &declared.indexes {
                if let Some(now) = now.indexes.get(n)
                    && !index_as_declared(was, now)
                {
                    moved.push(format!(
                        "{now_name} index `{n}` is not the one this plan's `CREATE TABLE` \
                         declares"
                    ));
                }
            }
            if let (Some(was), Some(now)) = (&declared.primary_key, &now.primary_key)
                && !primary_key_as_declared(was, now)
            {
                moved.push(format!(
                    "{now_name} primary key is not the one this plan's `CREATE TABLE` declares"
                ));
            }
            if declared.primary_key.is_none() && now.primary_key.is_some() {
                moved.push(format!(
                    "{now_name} has a primary key, and this plan declares none"
                ));
            }
            if settled.whole() && declared.primary_key.is_some() && now.primary_key.is_none() {
                moved.push(format!(
                    "{now_name} has no primary key, and this plan's `CREATE TABLE` declares one"
                ));
            }
        }
        if let (Some(was), Some(now)) = (before.tables.get(name), after.tables.get(now_name)) {
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
            let was = &undo.apply(was, name);
            let now = &undo.apply(now, now_name);
            let no_fields = BTreeMap::new();
            let moved_columns = redefined.get(now_name).unwrap_or(&no_fields);
            let no_names = BTreeSet::new();
            let moved_parts = constraints.get(now_name).unwrap_or(&no_names);
            let empty = BTreeSet::new();
            for column in was.columns.keys().chain(now.columns.keys()) {
                let mut moves = moved_columns.get(column).unwrap_or(&empty).clone();
                // A renamed column is under its old name in a read taken
                // before the rename statement and its new one after, so the
                // read that spans the statement finds it under `from` on one
                // side and `to` on the other. It is one column, and it is
                // compared as one — excusing both names left it exempt at
                // every later read of a staged run as well (DECISIONS 189).
                // The old name itself is then the one-sided entry below.
                let from = renamed_columns.get(&(now_name, column.as_str())).copied();
                let was_c = was.columns.get(column).or_else(|| {
                    let from = from?;
                    if now.columns.contains_key(from) {
                        return None;
                    }
                    moves.extend(moved_columns.get(from).into_iter().flatten().copied());
                    was.columns.get(from)
                });
                // Added, dropped or renamed: the column is on one side only,
                // and its presence is what `columns_after` answers for. Only
                // where it *is* one-sided, though — a column this plan adds
                // that is on both sides of a later read of a staged run is
                // two read-backs like any other, and excusing it there is
                // what let a concurrent redefinition through (DECISIONS 189).
                let (Some(was_c), Some(now_c)) = (was_c, now.columns.get(column)) else {
                    if !moves.contains(&pbps_model::ColumnField::Whole) {
                        moved.push(format!(
                            "{now_name} column `{column}` is not on both sides, and no change of \
                             this plan adds or removes it"
                        ));
                    }
                    continue;
                };
                for (field, what) in differing(was_c, now_c) {
                    if field.is_some_and(|f| moves.contains(&f)) {
                        continue;
                    }
                    moved.push(format!(
                        "{now_name} column `{column}` has a different {what} from the one the \
                         plan was approved over, and no change of this plan moves it"
                    ));
                }
            }
            if !keys.contains(now_name) && was.primary_key != now.primary_key {
                moved.push(format!("{now_name} has a different primary key"));
            }
            // By definition, not by name. A constraint dropped and recreated
            // under the same name with a different body is present on both
            // sides, and a membership test called that unchanged
            // (DECISIONS 167). Both values come from a read-back, so comparing
            // them predicts nothing — the same reason the columns above can be
            // compared outright.
            // One call per kind: the value is what has to be compared, and a
            // closure cannot be generic over it (DECISIONS 167). The kind
            // travels with the name because the engine keeps indexes and
            // constraints in separate namespaces (DECISIONS 168).
            use pbps_model::Part;
            let (n, s, m) = (now_name, moved_parts, &mut moved);
            named_alike(n, Part::Unique, "unique", &was.unique, &now.unique, s, m);
            named_alike(
                n,
                Part::ForeignKey,
                "foreign key",
                &was.foreign_keys,
                &now.foreign_keys,
                s,
                m,
            );
            named_alike(n, Part::Check, "check", &was.checks, &now.checks, s, m);
            named_alike(n, Part::Index, "index", &was.indexes, &now.indexes, s, m);
        }
        // Dropped, or outside the row scope on one side: there is no pair of
        // row sets to compare, and "absent" is not "empty".
        let (Some(was_rows), Some(now_rows)) = (
            was_rows,
            after.tables.get(now_name).and_then(|t| t.data.as_ref()),
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
            skip: &BTreeSet<String>,
        ) -> BTreeMap<&'a String, &'a pbps_model::Value> {
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
                     and no change of this plan writes it"
                )),
                None => moved.push(format!(
                    "{now_name} row `{key}` is gone, and no change of this plan deletes it"
                )),
            }
        }
        for key in now_rows.rows.keys() {
            if plans.contains(key) || was_rows.rows.contains_key(key) {
                continue;
            }
            moved.push(format!(
                "{now_name} row `{key}` is there, and no change of this plan inserts it"
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
                "{name} does not hold the definition this plan wrote"
            )),
            (pbps_model::ModuleAfter::Standing(_), None) => {
                moved.push(format!("{name} is not there, and this plan writes it"))
            }
            (pbps_model::ModuleAfter::Gone, None) => {}
            (pbps_model::ModuleAfter::Gone, Some(_)) => {
                moved.push(format!("{name} is still there, and this plan drops it"))
            }
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
                        match (wrote, row.get(column)) {
                            (pbps_model::CellAfter::Spelled(wrote), Some(now)) if now != wrote => {
                                moved.push(format!(
                                    "{table} row `{key}` does not hold in `{column}` what this \
                                     plan wrote"
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
                                     plan leaves it at its default"
                                ));
                            }
                            (pbps_model::CellAfter::AtDefault, _) => {}
                        }
                    }
                }
                (pbps_model::RowAfter::Holding(_), None) => moved.push(format!(
                    "{table} row `{key}` is not there, and this plan writes it"
                )),
                (pbps_model::RowAfter::Gone, Some(_)) => moved.push(format!(
                    "{table} row `{key}` is still there, and this plan deletes it"
                )),
                (pbps_model::RowAfter::Gone, None) => {}
            }
        }
    }

    // And the names the plan leaves standing or empty. Existence, for the
    // tables and roles themselves: a table's shape is answered for above,
    // entry by entry, and below for the entries this plan writes. A role
    // with no grants has nothing *but* its name, so without this a
    // `CREATE ROLE` another session undid was recorded as success
    // (DECISIONS 161).
    if settled.whole() {
        let mut expected_tables: BTreeMap<&TableName, pbps_model::Presence> = BTreeMap::new();
        let mut expected_roles: BTreeMap<&str, pbps_model::Presence> = BTreeMap::new();
        for p in &changes.changes {
            expected_tables.extend(p.change.tables_after());
            expected_roles.extend(p.change.roles_after());
        }
        for (name, expected) in expected_tables {
            match (expected, after.tables.contains_key(name)) {
                (pbps_model::Presence::Present, false) => {
                    moved.push(format!("{name} is not there, and this plan creates it"))
                }
                (pbps_model::Presence::Absent, true) => {
                    moved.push(format!("{name} is still there, and this plan removes it"))
                }
                _ => {}
            }
        }
        // And the parts of a table this plan changes. Nothing else says what
        // became of them: the shape comparison excludes exactly these, and the
        // table check above answers only for the table itself — so a column
        // added and dropped again before the checkpoint read was recorded as
        // the plan's own result (DECISIONS 168). Presence here, and the
        // definition next: a presence check is the wrong size for an
        // exclusion of a definition (DECISIONS 189).
        let mut expected_columns: BTreeMap<pbps_model::ColumnRef, pbps_model::Presence> =
            BTreeMap::new();
        // Keyed, not collected: a constraint or index whose *definition*
        // changes is a drop and an add under one name (`by_name!` in the
        // differ), and holding both outcomes meant the add satisfied
        // `Present` while the drop then failed `Absent` — every redefinition
        // refused (DECISIONS 169). The plan is in `order_key` order, which
        // puts the drops first, so the last word on a name is the net one.
        // The same collapsing the columns beside it get from being a map, and
        // the same 161 gave the modules.
        let mut expected_parts: BTreeMap<
            (&TableName, pbps_model::Part, Option<&str>),
            pbps_model::PartAfter<'_>,
        > = BTreeMap::new();
        // And the *definition* of each column field the plan moves, for the
        // same reason: the shape comparison excludes exactly those fields,
        // so nothing but this says what became of them. Presence alone let a
        // column another session retyped between the plan's `ALTER` and the
        // checkpoint read be recorded as the plan's own result — while the
        // created-table block above already held its columns to the
        // declaration, in the same fields (DECISIONS 189).
        //
        // Keyed by field, and for the reason the parts above are keyed by
        // name: a column that changes type and replaces its default gives up
        // the old default, changes type and takes the new one (DECISIONS 271),
        // so one field carries `Default(false)` and then `Default(true)` and
        // no read can satisfy both. The plan is in `order_key` order, so the
        // last promise about a field is the net one (DECISIONS 280).
        let mut expected_promises: BTreeMap<
            (pbps_model::ColumnRef, pbps_model::ColumnField),
            pbps_model::ColumnPromise<'_>,
        > = BTreeMap::new();
        for p in &changes.changes {
            expected_columns.extend(p.change.columns_after());
            for (column, promise) in p.change.columns_promised() {
                expected_promises.insert((column, promise.field()), promise);
            }
            if let Some(part) = p.change.constraints() {
                expected_parts.insert((part.table, part.part(), part.name), part.after);
            }
        }
        for (column, expected) in expected_columns {
            let Some(table) = after.tables.get(&column.table) else {
                // The table itself is gone or renamed; its own check answers.
                continue;
            };
            match (expected, table.columns.contains_key(&column.name)) {
                (pbps_model::Presence::Present, false) => moved.push(format!(
                    "{} column `{}` is not there, and this plan writes it",
                    column.table, column.name
                )),
                (pbps_model::Presence::Absent, true) => moved.push(format!(
                    "{} column `{}` is still there, and this plan removes it",
                    column.table, column.name
                )),
                _ => {}
            }
        }
        for ((column, _), promise) in expected_promises {
            let Some(now) = after
                .tables
                .get(&column.table)
                .and_then(|t| t.columns.get(&column.name))
            else {
                // Reported above, or the table's own check answers.
                continue;
            };
            use pbps_model::ColumnPromise;
            let (kept, what) = match promise {
                ColumnPromise::Whole(declared) => {
                    (column_as_declared(dialect, declared, now), "definition")
                }
                ColumnPromise::Type(to) => (same_type(dialect, to, &now.ty), "type"),
                ColumnPromise::Nullable(to) => (now.nullable == to, "nullability"),
                ColumnPromise::Default(has) => (now.default.is_some() == has, "default"),
            };
            if !kept {
                moved.push(format!(
                    "{} column `{}` does not have the {what} this plan gives it",
                    column.table, column.name
                ));
            }
        }
        for ((table_name, part, part_name), after_it) in expected_parts {
            let Some(table) = after.tables.get(table_name) else {
                continue;
            };
            // Held to what the plan adds, not to being there. A part is
            // dropped and recreated under one name by anyone who redefines
            // it, so a read that finds the name says nothing about which
            // definition is behind it (DECISIONS 189).
            if let pbps_model::PartAfter::Standing(definition) = after_it
                && part_as_planned(definition, table, part_name.unwrap_or("")) == Some(false)
            {
                let kind = match part {
                    pbps_model::Part::PrimaryKey => "primary key",
                    pbps_model::Part::Unique => "unique",
                    pbps_model::Part::ForeignKey => "foreign key",
                    pbps_model::Part::Check => "check",
                    pbps_model::Part::Index => "index",
                };
                let name = part_name.unwrap_or("");
                moved.push(format!(
                    "{table_name} {kind} `{name}` is there, and is not the one this plan adds"
                ));
                continue;
            }
            let after_it = after_it.presence();
            let (kind, there) = match (part, part_name) {
                (pbps_model::Part::PrimaryKey, _) => ("primary key", table.primary_key.is_some()),
                (pbps_model::Part::Unique, Some(n)) => ("unique", table.unique.contains_key(n)),
                (pbps_model::Part::ForeignKey, Some(n)) => {
                    ("foreign key", table.foreign_keys.contains_key(n))
                }
                (pbps_model::Part::Check, Some(n)) => ("check", table.checks.contains_key(n)),
                (pbps_model::Part::Index, Some(n)) => ("index", table.indexes.contains_key(n)),
                // Only the primary key is nameless, and it is matched above.
                (_, None) => continue,
            };
            let name = part_name.unwrap_or("");
            match (after_it, there) {
                (pbps_model::Presence::Present, false) => moved.push(format!(
                    "{table_name} {kind} `{name}` is not there, and this plan writes it"
                )),
                (pbps_model::Presence::Absent, true) => moved.push(format!(
                    "{table_name} {kind} `{name}` is still there, and this plan removes it"
                )),
                _ => {}
            }
        }
        for (name, expected) in expected_roles {
            match (expected, after.roles.contains_key(name)) {
                (pbps_model::Presence::Present, false) => moved.push(format!(
                    "role {name} is not there, and this plan creates it"
                )),
                (pbps_model::Presence::Absent, true) => moved.push(format!(
                    "role {name} is still there, and this plan removes it"
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
    let before_roles: BTreeMap<String, pbps_model::Role> = before
        .roles
        .iter()
        .map(|(name, role)| {
            let grants = role
                .grants
                .iter()
                // A grant on an object this plan drops goes with it: the engine
                // removes a permission with its securable, so `diff_roles`
                // emits no `REVOKE` and there is nothing left for the read-back
                // to hold (DECISIONS 158).
                // By whole identity: dropping one overload leaves the
                // sibling's grants standing, and standing grants are compared.
                .filter(|(target, _)| !gone.iter().any(|d| d.takes(target)))
                .map(|(target, held)| {
                    let target = match target {
                        pbps_model::GrantTarget::Object(object) => pbps_model::GrantTarget::Object(
                            renamed
                                .get(object)
                                .copied()
                                .cloned()
                                .unwrap_or_else(|| object.clone()),
                        ),
                        // Only tables are renamed, and a routine is not a
                        // table — as `diff_roles` says. Where routines have a
                        // namespace of their own, `app.f(integer)` stands
                        // beside a table `app.f`, and forwarding the routine's
                        // grant through the table's rename refused the apply
                        // for a grant that had, rightly, not moved.
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
    let named_role = |n: &String| roles.contains(n.as_str());
    compare(
        "role ",
        &before_roles,
        &after.roles,
        named_role,
        |_: &String, was: &pbps_model::Role, now: &pbps_model::Role| was == now,
        &mut moved,
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
    let carried_roles: BTreeSet<&str> = before_roles
        .keys()
        .map(|name| renamed_roles.get(name.as_str()).copied().unwrap_or(name))
        .collect();
    let no_grants = pbps_model::Role {
        description: None,
        grants: BTreeMap::new(),
    };
    let touched_roles = before_roles.iter().chain(
        after
            .roles
            .keys()
            .filter(|name| named_role(name) && !carried_roles.contains(name.as_str()))
            .map(|name| (name, &no_grants)),
    );
    for (name, was) in touched_roles {
        if !named_role(name) {
            // Already compared whole, grants included.
            continue;
        }
        // `ALTER ROLE ... WITH NAME` keeps the membership and the grants, so
        // the two ends of a rename are one role's before and after; the
        // grant changes beside it name the role as it will be (`order_key`).
        let now_name = renamed_roles.get(name.as_str()).copied().unwrap_or(name);
        let Some(now) = after.roles.get(now_name) else {
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
                    .filter(|(role, _)| *role == now_name)
                    .map(|(_, target)| *target),
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
            let adds = granting.get(&(now_name, target)).unwrap_or(&empty);
            let removes = revoking.get(&(now_name, target)).unwrap_or(&empty);
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
                moved.push(format!(
                    "role {now_name} does not hold on {target} what this plan leaves it holding"
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
        moved.join("\n  ")
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
fn named_alike<V: PartialEq>(
    table: &TableName,
    part: pbps_model::Part,
    kind: &str,
    was: &BTreeMap<String, V>,
    now: &BTreeMap<String, V>,
    skip: &BTreeSet<(pbps_model::Part, &str)>,
    moved: &mut Vec<String>,
) {
    let names: BTreeSet<&String> = was.keys().chain(now.keys()).collect();
    for name in names {
        if skip.contains(&(part, name.as_str())) {
            continue;
        }
        if was.get(name) != now.get(name) {
            moved.push(format!(
                "{table} {kind} `{name}` is not as the plan left it"
            ));
        }
    }
}

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
) where
    K: Ord + std::fmt::Display,
{
    for (name, was) in before {
        if touched(name) {
            continue;
        }
        match after.get(name) {
            Some(now) if unchanged(name, was, now) => {}
            Some(_) => moved.push(format!(
                "{kind}{name} is not what the plan was approved over"
            )),
            None => moved.push(format!(
                "{kind}{name} is gone, and no change of this plan drops it"
            )),
        }
    }
    for name in after.keys() {
        if touched(name) || before.contains_key(name) {
            continue;
        }
        moved.push(format!(
            "{kind}{name} is there, and no change of this plan creates it"
        ));
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
    let dialect = crate::output::or_unanswerable(
        "verify",
        json,
        "project.unsupported-dialect",
        crate::dialect(project),
    )?;
    let checked_at = crate::now();

    let rt = crate::output::or_unanswerable("verify", json, "runtime.unavailable", db::runtime())?;
    let (report, unmanaged_refusal, unmanaged_inventory) = match rt.block_on(async {
        let mut conn = db::connect(target).await?;

        let Some(baseline) = crate::engine::latest(&mut conn).await? else {
            bail!(
                "`{}` has a ledger but no entries; there is nothing to compare against.\n\
                 Record one with `pbps snapshot` or `pbps baseline --reason ...`.",
                target.label
            );
        };

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
            // The unmanaged policy is applied below instead, beside the
            // finished report, so that neither verdict can erase the other.
            pbps_config::Unmanaged::Ignore,
            &rows_to_read(&recorded_scopes),
            crate::engine::Read::Snapshot,
        )
        .await?;
        let mut scoped = managed.scoped;
        scoped.schema = scoped.schema.with_observed_rows(
            &managed.rows,
            &recorded_scopes,
            &baseline.snapshot.schema,
        )?;

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
        let diffed = pbps_diff::diff_partial(
            pbps_diff::Side {
                schema: &baseline.snapshot.schema,
                ids: &recorded_ids,
            },
            pbps_diff::Side {
                schema: &scoped.schema,
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
        let unmanaged_inventory =
            unmanaged_objects(&scoped, &managed.unreadable, &recorded_modules);
        let unmanaged_refusal =
            match apply_unmanaged_policy(&unmanaged_inventory, project.config.unmanaged) {
                Ok(()) => None,
                Err(error) => match error.downcast::<UnmanagedPolicy>() {
                    Ok(policy) => Some(policy),
                    Err(error) => return Err(error),
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
                    checksum: pbps_model::state_checksum(&baseline.snapshot.schema, &recorded_ids),
                },
                // The checksum compares like with like: both sides fingerprinted
                // with the recorded mapping, so a derived identity for a hand-added
                // column cannot by itself make the two differ.
                live_checksum: pbps_model::state_checksum(&scoped.schema, &recorded_ids),
                changes,
                unmanaged: scoped.unmanaged,
                unexpressible,
            },
            unmanaged_refusal,
            unmanaged_inventory,
        ))
    }) {
        Ok(outcome) => outcome,
        Err(e) => {
            // Unanswerable: `verify` was asked whether this database still
            // matches its recorded state, and it could not look. Without
            // this the JSON path returned before its own branch, leaving
            // stdout empty — so a consumer got the converter's generic
            // "produced no output" instead of a report naming the target
            // (SPEC §9.8).
            if json {
                crate::output::unanswerable(
                    "verify",
                    vec![crate::output::Finding::error(
                        "environment.unreachable",
                        format!("{}: {e:#}", target.label),
                    )],
                );
            }
            return Err(e);
        }
    };

    // The hook always receives the bare report, whatever the human asked for on
    // stdout: a script's payload should not change shape because someone added
    // a flag for their own eyes, and it must not gain an envelope because
    // stdout did.
    let payload = format!("{}\n", serde_json::to_string_pretty(&report)?);

    let mut findings = Vec::new();
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
                    target.label,
                    report.changes.changes.len() + report.unexpressible.len()
                ),
            )
            .remedy("pbps pull | pbps plan --db … && pbps apply | pbps baseline --reason \"…\""),
        );
    }
    // One finding per difference, because the differ returns one per
    // unexpressible change and the column name in it is the remedy.
    for e in &report.unexpressible {
        findings.push(crate::output::Finding::error(
            "state.drift-unexpressible",
            format!("{}: {e}", target.label),
        ));
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
                format!("{}: {refusal}", target.label),
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
            crate::hooks::run(hook, &payload, "on_drift");
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
    let ids = crate::read_ids(project)?;
    let (declared_modules, declared_data, loaded) =
        declared_scope(project, crate::dialect(project)?.as_ref())?;
    let operator = crate::operator(project.root());

    db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;
        crate::engine::lock(&mut conn, &operator).await?;
        let result = async {
            let scoped = managed_state(
                &mut conn,
                &ids,
                &declared_modules,
                project.config.unmanaged,
                &declared_data,
                &loaded.schema,
                crate::engine::Read::Snapshot,
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
                        target.label,
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
                    target.label
                ),
                Ok(None) => {}
                Err(e) => return Err(e.into()),
            }

            let mut snapshot = with_provenance(
                project.root(),
                StateSnapshot::new(StateKind::Apply, scoped.schema, ids.clone(), &operator),
            );
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
            target.label, tables
        );
        // The ledger row is durable even when deleting the lock row fails.
        // Report that before surfacing cleanup so retrying cannot look safe.
        released?;
        Ok(())
    })
}

/// `pbps baseline` — take the database as it stands as the new starting point.
pub fn cmd_baseline(project: &Project, target: &Target, reason: &str) -> anyhow::Result<()> {
    let ids = crate::read_ids(project)?;
    let (declared_modules, declared_data, loaded) =
        declared_scope(project, crate::dialect(project)?.as_ref())?;
    let operator = crate::operator(project.root());

    db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;
        crate::engine::lock(&mut conn, &operator).await?;
        let result = async {
            let scoped = managed_state(
                &mut conn,
                &ids,
                &declared_modules,
                project.config.unmanaged,
                &declared_data,
                &loaded.schema,
                crate::engine::Read::Snapshot,
            )
            .await?;
            report_missing(&scoped);
            refuse_unexpressible(&scoped, &target.label, "baseline again")?;

            let mut snapshot = with_provenance(
                project.root(),
                StateSnapshot::new(StateKind::Baseline, scoped.schema, ids.clone(), &operator),
            );
            snapshot.module_deps = loaded.hints.module_deps.clone();
            snapshot.reason = Some(reason.to_owned());

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
            "Baselined `{}` as entry #{id}: {} table(s) are now the starting point.",
            target.label, tables
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
fn refuse_invalid_declarations(
    loaded: &pbps_load::Loaded,
    dialect: &dyn Dialect,
) -> anyhow::Result<()> {
    let problems = crate::declaration_problems(loaded, dialect);
    if problems.is_empty() {
        return Ok(());
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

pub fn cmd_bootstrap(
    project: &Project,
    target: Option<&Target>,
    sql_out: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let dialect = crate::dialect(project)?;
    let loaded = crate::load(project, dialect.as_ref())?;
    let ids = crate::read_ids(project)?;
    refuse_invalid_declarations(&loaded, dialect.as_ref())?;
    let declared_modules = managed_modules(None, Some(&loaded.schema));

    // Every declared object needs its identity, not just "some": a role the
    // ids file does not know is skipped by the differ, and a bootstrap that
    // skipped it built less than the declarations say and recorded that as
    // the whole state. `pbps plan` mints what is missing (DECISIONS 109).
    let unidentified: Vec<String> = loaded
        .schema
        .tables
        .keys()
        .filter(|t| ids.table_uid(t).is_none())
        .map(ToString::to_string)
        .chain(
            loaded
                .schema
                .roles
                .keys()
                .filter(|r| ids.role_uid(r).is_none())
                .map(|r| format!("role {r}")),
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

    // Bootstrap is the plan from nothing: every declared table is created.
    let cs = pbps_diff::diff(
        pbps_diff::Side {
            schema: &Schema::default(),
            ids: &IdsFile::default(),
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
            strategies: Default::default(),
            module_deps: loaded.hints.module_deps.clone(),
        },
    )
    .map_err(|errs| {
        for e in &errs {
            eprintln!("  {e}");
        }
        anyhow::anyhow!("{} change(s) cannot be expressed", errs.len())
    })?;

    if let Some(path) = sql_out {
        let script = crate::render_sql(&cs, dialect.as_ref(), "an empty database")?;
        std::fs::write(path, script)
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        println!("wrote {}", path.display());
    }

    let Some(target) = target else {
        if sql_out.is_none() {
            bail!(
                "bootstrap needs somewhere to go: pass --sql {}, --db or --env",
                crate::report::placeholder("file")
            );
        }
        return Ok(());
    };

    let statements = crate::statements(&cs, dialect.as_ref())?;
    let operator = crate::operator(project.root());

    db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;
        crate::engine::lock(&mut conn, &operator).await?;
        let mut transaction_attempted = false;
        let result = async {
            // Before the declared rows go in: a spelling the engine reads back
            // differently would be recorded as the engine spells it and drift
            // from the declaration on the next plan (DECISIONS 101).
            // Into an empty database, so nothing is under an older name: the
            // declared names are the only ones the catalog could have.
            refuse_misspelt(&mut conn, &loaded.schema, &Default::default()).await?;
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
                project.config.unmanaged,
                &BTreeMap::new(),
                crate::engine::Read::Snapshot,
            )
            .await?;

            // Unsupported grants are outside role.grants, so an apparently
            // empty role is not proof that the database is empty (110).
            refuse_unexpressible(&existing.scoped, &target.label, "bootstrap again")?;
            crate::engine::refuse_missing_cluster_roles(
                conn.driver(),
                &existing.scoped.missing_roles,
            )?;

            // Only names the plan creates need to be free (DECISIONS 118).
            // PostgreSQL grants use existing cluster roles; their existence
            // is outside this database's ownership (DECISIONS 211).
            let (wanted, vacated) = role_name_expectations(&cs, dialect.as_ref(), 0)?;
            refuse_taken_role_names(&mut conn, &wanted, &vacated).await?;

            // A cluster role with no managed grants is not an object this
            // bootstrap creates. Existing managed grants still make the
            // target nonempty: bootstrap must not silently adopt them.
            let existing_roles: Vec<_> = existing
                .scoped
                .schema
                .roles
                .iter()
                .filter(|(_, role)| dialect.manages_roles() || !role.grants.is_empty())
                .map(|(name, _)| format!("role {name}"))
                .collect();

            // Bootstrap means "into an empty database". Running it over an
            // existing managed set would fail halfway through on the first
            // CREATE and leave a partly built schema unless the transaction
            // catches it. Modules count too: `CREATE OR ALTER` would otherwise
            // replace one without a plan or approval. And so does a role
            // (ADR-0005): `CREATE ROLE` fails on one already there, after
            // everything before it in the batch has run. And so does
            // everything in the managed set that the projection could not
            // express — a declared module the catalog cannot read back, or a
            // declared table whose every column is unsupported and which
            // introspection therefore left out whole. That table is absent
            // from the scoped schema and present in the database; counting
            // only the scoped schema called the target empty, the CREATE
            // failed on it, and the failure audit recorded an empty state as
            // the newest one, which the next `verify` under `unmanaged:
            // ignore` believed.
            if !existing.scoped.schema.tables.is_empty()
                || !existing.scoped.schema.modules.is_empty()
                || !existing_roles.is_empty()
                || !existing.limitations.is_empty()
            {
                let names: Vec<String> = existing
                    .scoped
                    .schema
                    .tables
                    .keys()
                    .map(ToString::to_string)
                    .chain(
                        existing
                            .scoped
                            .schema
                            .modules
                            .keys()
                            .map(ToString::to_string),
                    )
                    .chain(existing_roles)
                    .chain(existing.limitations.iter().cloned())
                    .collect();
                bail!(
                    "`{}` already has {} of the declared object(s): {}.\n\
                     Bootstrap builds into an empty database; use `pbps plan --db` and `pbps \
                     apply` to migrate it instead.",
                    target.label,
                    names.len(),
                    names.join(", ")
                );
            }

            transaction_attempted = true;
            execute_transaction_body(&mut conn, dialect.as_ref(), &statements).await?;

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
                project.config.unmanaged,
                &loaded.schema.data_scopes(),
                &loaded.schema,
                crate::engine::Read::InsideOwnTransaction,
            )
            .await?;
            // A database-side trigger can add a privilege during the build.
            // Never commit a snapshot that silently omits it (110, 147).
            refuse_unexpressible(&built, &target.label, "bootstrap again")?;
            crate::engine::refuse_missing_cluster_roles(conn.driver(), &built.missing_roles)?;
            let mut snapshot = with_provenance(
                project.root(),
                StateSnapshot::new(StateKind::Bootstrap, built.schema, ids.clone(), &operator),
            );
            snapshot.module_deps = loaded.hints.module_deps.clone();
            // Every object was created from these declarations, so what was
            // declared is exactly what bootstrap holds (ADR-0009 §2.2).
            snapshot.declared = pbps_model::Declared::from_schema(&loaded.schema);
            let id = crate::engine::record(&mut conn, &snapshot).await?;
            Ok::<_, anyhow::Error>((id, snapshot))
        }
        .await;
        let result = if transaction_attempted {
            finish_transaction(&mut conn, dialect.as_ref(), result).await
        } else {
            result
        };
        if transaction_attempted && let Err(error) = &result {
            record_failed_bootstrap(&mut conn, project.root(), &operator, error).await;
        }
        let unlocked = crate::engine::unlock(&mut conn).await;
        let (id, snapshot) = match result {
            Ok(built) => built,
            Err(error) => {
                warn_unreleased(&target.label, &unlocked);
                return Err(error);
            }
        };
        println!(
            "Bootstrapped `{}`: {} table(s) created, recorded as entry #{id}.",
            target.label,
            snapshot.schema.tables.len()
        );
        // Both the DDL and ledger row committed before lock cleanup began.
        unlocked?;
        Ok(())
    })
}

/// Best-effort audit row for a bootstrap whose transactional work was rolled
/// back. The empty state was established before the lock was taken, so it is
/// the last trustworthy state to carry forward; the declared identity mapping
/// has not become an environment mapping until the build succeeds.
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
    failed.reason = Some(crate::engine::truncate_reason(
        conn.driver(),
        &error.to_string(),
    ));
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
            target.label
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
                    .map(|h| format!("`{}` since {}", h.locked_by, h.locked_at))
                    .unwrap_or_else(|| "an unnamed holder".to_owned());
                println!("Released the lock on `{}`, held by {who}.", target.label);
                // Nothing about a released lock proves the operation it guarded
                // finished, and a half-finished apply is exactly what drift
                // detection is for.
                println!("Run `pbps verify` before applying anything else.");
            }
            false => println!("`{}` was not locked.", target.label),
        }
        Ok(())
    })
}

/// `pbps plan --db` — the applyable plan, computed against the target
/// environment as queried (SPEC §7.3).
///
/// This is the deployment layer, so it changes no files. The identity file was
/// settled when the MR was reviewed; a `plan --db` that quietly rewrote it
/// would mean the artifact the gate approves was computed against a mapping
/// nobody read.
pub fn cmd_plan_db(
    project: &Project,
    target: &Target,
    out: Option<&std::path::Path>,
    sql_out: Option<&std::path::Path>,
    staged: bool,
) -> anyhow::Result<()> {
    let dialect = crate::dialect(project)?;
    let loaded = crate::load(project, dialect.as_ref())?;
    let ids = crate::read_ids(project)?;
    refuse_invalid_declarations(&loaded, dialect.as_ref())?;
    let created_at = crate::now();

    let resolved = match pbps_diff::resolve_with_annotations(
        &loaded.schema,
        &ids,
        &loaded.intents,
        loaded.intents.len(),
        &crate::context(project.root()),
    ) {
        Ok(r) => r,
        Err(blockers) => {
            eprintln!("{}", crate::report::blockers(&blockers));
            bail!("some changes could not be decided automatically");
        }
    };
    if resolved.ids != ids {
        bail!(
            "the identity file is out of date; run `pbps plan` locally and commit `{}`.\n\
             A deployment plan must be computed against the identity its reviewers read.",
            project.ids_file().display()
        );
    }

    let (cs, baseline_checksum, baseline_description) = db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;

        let Some(mut entry) = crate::engine::latest(&mut conn).await? else {
            bail!(
                "`{}` has a ledger but no entries; there is nothing to plan against.\n\
                 Record one with `pbps baseline --reason ...`.",
                target.label
            );
        };
        refuse_mid_deployment(&entry, &target.label)?;
        let role_renames =
            crate::engine::external_role_renames(&mut conn, &entry.snapshot.ids, &resolved.ids)
                .await?;
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
        // The declarations under the names the database has *now*: a table
        // this plan renames is read, scoped and measured under its old name,
        // and a declared scope keyed by the new one would find no table and
        // be skipped — an `ensure` -> `exact` switch in the same revision as
        // the rename then planned none of its deletes.
        let declared_live = tables_under(&loaded.schema, &resolved.ids, &recorded_ids);
        let declared_data = declared_live.data_scopes();
        // Every declared text, as the engine reads it: a spelling it would
        // read back differently is refused before a plan is written that
        // could never converge (DECISIONS 101).
        refuse_misspelt(
            &mut conn,
            &loaded.schema,
            &catalogued_as(&loaded.schema, &resolved.ids, &recorded_ids),
        )
        .await?;
        refuse_wrongly_spelt_schemas(&mut conn, &loaded.schema).await?;
        let managed = managed_state_full(
            &mut conn,
            &recorded_ids,
            &recorded_modules,
            pbps_config::Unmanaged::Ignore,
            &pbps_model::data::read_scopes(&recorded_data, &declared_data),
            crate::engine::Read::Snapshot,
        )
        .await?;
        refuse_unexpressible(&managed.scoped, &target.label, "plan again")?;
        // A connected plan becomes an applyable artifact. If introspection
        // omitted a fact on a recorded table, its scoped checksum can still
        // match while the real database does not; planning from that partial
        // projection would bless the omission and an empty apply would never
        // reconnect to catch it. The same argument as the line above, about
        // the other half of what a projection can fail to hold.
        refuse_managed_limitations(&managed.limitations)?;
        // A declared name standing on an object introspection cannot express is
        // not something to plan around. It is absent from the scoped schema, so
        // the diff would emit an ungated `CreateModule` and `CREATE OR ALTER`
        // would replace it — removing, silently, the very options that made it
        // unreadable (ADR-0002: what pbps cannot reproduce, it does not touch).
        let colliding: Vec<&str> = managed
            .unreadable
            .iter()
            .filter(|(n, _)| {
                loaded
                    .schema
                    .modules
                    .keys()
                    .any(|id| &id.object_name() == n)
            })
            .map(|(_, why)| why.as_str())
            .collect();
        if !colliding.is_empty() {
            bail!(
                "`{}` already holds {} declared object(s) that pbps cannot read back: {}.\n\
                 Planning would propose creating them, and `CREATE OR ALTER` would replace what \
                 is there. Remove the declaration, or recreate the object in a form pbps can \
                 express.",
                target.label,
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
        let live = pbps_model::state_checksum(&as_recorded, &recorded_ids);
        let mut expected = entry.snapshot.schema.clone();
        // Done also permits drop-and-create (377). Grants lost with the old
        // principal must be planned onto the new one, not rejected as drift.
        // Newly managed roles also start from their actual grants. Both sets
        // remain in the pinned baseline and the differ's input (421).
        for to in recorded_ids.roles.values().filter(|name| {
            role_renames.values().any(|to| to == *name)
                || !recorded_snapshot.ids.roles.values().any(|old| old == *name)
        }) {
            let Some(role) = as_recorded.roles.get(to) else {
                bail!("declared cluster role `{to}` is missing; create or rename it first, then plan again");
            };
            expected.roles.insert(to.clone(), role.clone());
        }
        let recorded = pbps_model::state_checksum(&expected, &recorded_ids);
        if live != recorded {
            bail!(
                "`{}` has drifted from the state recorded at {} (entry #{}).\n\
                 Run `pbps verify --env/--db ...` to see how, and resolve it before planning.",
                target.label,
                entry.applied_at,
                entry.id
            );
        }

        // A table whose rows the declarations cover for the first time is
        // measured against what it holds, not against nothing: the rows are
        // adopted, the ones that differ are updated, and — for `exact` — the
        // ones nobody declared are deleted behind the gate. Said out loud,
        // because the plan shows the consequences and not the takeover.
        for (name, scope) in &declared_data {
            if recorded_data.contains_key(name) {
                continue;
            }
            match managed.rows.get(name) {
                Some(observed) => println!(
                    "Reference data: the declarations take over the rows of {name} ({}); it holds \
                     {} row(s) now.",
                    scope.mode,
                    observed.rows.len()
                ),
                None => println!(
                    "Reference data: {name} does not exist yet; its declared rows go in with it."
                ),
            }
        }
        let base = pbps_model::data::plan_base(
            &scoped.schema,
            &managed.rows,
            &recorded_data,
            &declared_live,
        )?;
        // The engine respells every expression and module body it stores —
        // measured on SQL Server, a check declared `n > 0` reads back
        // `([n]>(0))` — so compared against the read-back an unchanged check
        // was dropped and re-added, and an unchanged filtered index rebuilt,
        // on every connected plan. The differ compares the declarations
        // against what was *declared* when each object was last written,
        // where the ledger recorded it (ADR-0009 §2.2, ADR-0013 §4,
        // DECISIONS 207–208).
        let base = entry.snapshot.declared.overlay(&base);
        // A removed module no longer has a declaration carrying its
        // `depends_on:` edge. The newest snapshot keeps those baseline
        // annotations so connected planning can still drop dependents first.
        let mut hints = loaded.hints.clone();
        for (name, dependencies) in &entry.snapshot.module_deps {
            if !loaded.schema.modules.contains_key(name) {
                hints.module_deps.insert(name.clone(), dependencies.clone());
            }
        }
        let mut cs = pbps_diff::diff(
            pbps_diff::Side {
                schema: &base,
                ids: &recorded_ids,
            },
            pbps_diff::Side {
                schema: &loaded.schema,
                ids: &resolved.ids,
            },
            dialect.as_ref(),
            &hints,
        )
        .map_err(|errs| {
            for e in &errs {
                eprintln!("  {e}");
            }
            anyhow::anyhow!("{} change(s) cannot be expressed", errs.len())
        })?;

        crate::engine::require_transactional_rebuilds(conn.driver(), &cs, staged)?;
        conn.begin(dialect.transaction_framing()).await?;
        let checks = async {
            crate::engine::external_role_renames(&mut conn, &recorded_snapshot.ids, &resolved.ids)
                .await?;
            crate::engine::check_module_rebuilds(&mut conn, &cs, false).await
        }
        .await;
        let rollback = conn.rollback(dialect.transaction_framing()).await;
        checks?;
        rollback?;

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
        let (wanted, vacated) = role_name_expectations(&cs, dialect.as_ref(), 0)?;
        refuse_taken_role_names(&mut conn, &wanted, &vacated).await?;

        // A role this plan drops still has this environment's members, which
        // the engine will not drop it over. They are read here and written
        // into the plan by name, so the artifact the gate approves lists
        // exactly who is removed (ADR-0005) — never found again at apply
        // time, where nobody would have reviewed them.
        if cs
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::DropRole { .. }))
        {
            let members = crate::engine::role_members(&mut conn)
                .await
                .context("cannot read the role memberships")?;
            // Ownership is refused, not planned around: the engine will not
            // drop an owning role, and moving ownership is a decision about
            // who owns a securable, not a consequence of a drop. Said here,
            // before anything runs — a staged apply would otherwise commit
            // every DROP MEMBER and then fail on the DROP ROLE.
            let owned = crate::engine::role_owned_securables(&mut conn)
                .await
                .context("cannot read what the roles own")?;
            for p in &mut cs.changes {
                if let pbps_model::Change::DropRole {
                    name, members: m, ..
                } = &mut p.change
                {
                    if let Some(securables) = owned.get(name.as_str())
                        && !securables.is_empty()
                    {
                        bail!(
                            "role `{name}` cannot be dropped in `{}`: it owns {}.\n\
                             Move the ownership first (`ALTER AUTHORIZATION ON {} TO dbo;`, \
                             by hand, since pbps does not decide who owns a securable), or keep \
                             the role.",
                            target.label,
                            securables.join(", "),
                            securables[0]
                        );
                    }
                    *m = members.get(name).cloned().unwrap_or_default();
                }
            }
            // The differ sorted the drops before it could know who holds
            // whom; now that the members are in, a role holding another
            // dropped role goes first, or its `DROP MEMBER` names a principal
            // already gone (DECISIONS 127, 139).
            pbps_diff::order_role_drops(&mut cs);
        }

        // The plan rules (ADR-0008), the connected ones included: this is the
        // one place a change window has a moment and a target to be measured
        // against. An `error` refuses the plan here, before anything is
        // written; `apply` never sees a policy.
        let policy = crate::attach_policy_findings(&mut cs, project, true);
        for f in &policy {
            eprintln!("{}: {} — {}", f.severity_word(), f.id, f.message);
        }
        if policy
            .iter()
            .any(|f| f.severity == crate::output::Severity::Error)
        {
            bail!(
                "a policy set to `error` refuses this plan; fix the declarations, or suppress \
                 the rule in pbps.yml with a reason"
            );
        }

        // The edition is a connection-time fact, and it is the only place the
        // two edition-dependent questions of ADR-0003 can be answered
        // honestly: whether ONLINE will be accepted at all, and whether an
        // addition that is metadata-only on Enterprise rewrites every row
        // here. An offline plan has to assume the conservative answer.
        let verdict = crate::engine::edition_verdict(&mut conn, &cs).await?;
        if !verdict.refused_online.is_empty() {
            bail!(
                "`strategy: online` is declared for {}, and `{}` runs {}, which has no online \
                 index operations.\n\
                 The statement would fail partway through the apply. Remove the hint, or deploy \
                 this change to an edition that supports it.",
                verdict.refused_online.join(", "),
                target.label,
                verdict.runs
            );
        }
        for w in &verdict.warnings {
            eprintln!("warning: {w}");
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
            &pinned_scopes(&recorded_data, &declared_data),
            &entry.snapshot.schema,
        )?;
        let baseline = pbps_model::state_checksum(&pinned, &recorded_ids);
        Ok((
            cs,
            baseline,
            format!("{} as queried (entry #{})", target.label, entry.id),
        ))
    })?;

    let statements = crate::statements(&cs, dialect.as_ref())?;
    if staged {
        // A staged plan is one logical change isolated in a deployment of its
        // own (ADR-0003). The limit is the whole point: what cannot be rolled
        // back must not be able to take four unrelated changes down with it,
        // and a resume that had to reason about which of five changes were
        // half-done would be guessing.
        if cs.changes.len() > 1 {
            bail!(
                "--staged applies one logical change, and this plan has {}.\n\
                 Stage the change that needs it in a revision of its own; the rest can go \
                 through an ordinary transactional apply.",
                cs.changes.len()
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

    println!("Baseline: {baseline_description}");
    print!("{}", crate::report::plan(&cs));

    let mut plan = pbps_model::SavedPlan::new(
        pbps_model::PlanOrigin::Database,
        dialect.name(),
        created_at,
        pbps_model::PlanBaseline {
            description: baseline_description,
            checksum: baseline_checksum,
        },
        cs,
        resolved.ids,
    );
    plan.module_deps = loaded.hints.module_deps.clone();
    plan.git_sha = db::git_sha(project.root());
    // The rows the state recorded after the apply has to cover: the
    // declarations' scope, carried so that `apply` needs no checkout.
    plan.data = loaded.schema.data_scopes();
    if staged {
        plan = plan.staged();
        println!(
            "\nThis is a staged plan: {} statement(s) will run outside a transaction, each \n\
             recorded in the ledger as it completes. Apply it with `pbps apply --staged \n\
             --checksum {}`, and continue an interrupted run with \n\
             `--staged --resume`.",
            statements.len(),
            crate::report::placeholder("approved-checksum")
        );
    }

    if let Some(path) = out {
        crate::write_plan(path, &plan)?;
        println!("\nwrote {} (checksum {})", path.display(), plan.checksum());
    }
    if let Some(path) = sql_out {
        let script = format!(
            "-- Generated by pbps against {}\n-- Applyable only through `pbps apply --plan`; never hand-edit this file.\n\n{}",
            plan.baseline.description,
            pbps_dialect::render_script(&statements, dialect.batch_separator())
        );
        std::fs::write(path, script)
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        println!("wrote {}", path.display());
    }
    if out.is_none() {
        println!(
            "\nThis plan was not saved. `--out plan.json` writes the artifact `pbps apply` takes."
        );
    }
    Ok(())
}

/// What the operator asked `apply` to do, as typed.
pub struct ApplyRequest<'a> {
    pub plan_path: &'a std::path::Path,
    pub approved_checksum: &'a str,
    pub allow: &'a std::collections::BTreeSet<pbps_model::RiskClass>,
    pub staged: bool,
    pub resume: bool,
}

/// How an attempt on an identified artifact ended.
///
/// The three outcomes are one type so that [`cmd_apply`] has one place to
/// report them: every path out of [`apply_identified`] arrives here, and the
/// `on_apply_attempt` hook fires from that one place. Earlier the failure hook
/// was called at each site that could fail after the lock, and the refusals
/// before it — a stale `--checksum`, an unapproved risk — returned past the
/// hook entirely, so the audit sink advertised as seeing every attempt never
/// saw the rejected artifact.
enum Attempt {
    /// Nothing to do; not an attempt in the hook's sense.
    Empty,
    /// The plan ran and its ledger entry is durable. `released` is the lock
    /// cleanup, reported separately because a failed DELETE of the lock row
    /// cannot turn committed DDL plus its success row into a failed deployment.
    Applied {
        entry: i64,
        released: Result<bool, pbps_db::DbError>,
    },
    /// The plan was run under the lock and did not complete. The lock cleanup
    /// travels with the error so the caller can warn about it.
    Failed {
        error: anyhow::Error,
        released: Result<bool, pbps_db::DbError>,
    },
}

/// `pbps apply` — run an approved plan (SPEC §7.3, §7.5).
pub fn cmd_apply(
    project: &Project,
    target: &Target,
    request: &ApplyRequest<'_>,
) -> anyhow::Result<()> {
    let dialect = crate::dialect(project)?;
    let operator = crate::operator(project.root());
    let plan_path = request.plan_path;

    // Before this point there is no artifact to report on: the attempt event
    // carries a checksum, and a file that cannot be read or is not a plan has
    // none. From `plan_checksum` on, every outcome is an attempt on this
    // artifact against this environment, and the hook sees all of them.
    let raw = std::fs::read_to_string(plan_path)
        .with_context(|| format!("cannot read `{}`", plan_path.display()))?;
    let plan: pbps_model::SavedPlan = serde_json::from_str(&raw)
        .with_context(|| format!("`{}` is not a pbps plan", plan_path.display()))?;
    let plan_checksum = plan.checksum();

    let attempt = apply_identified(
        project,
        target,
        request,
        &plan,
        &plan_checksum,
        dialect.as_ref(),
        &operator,
    );
    let (error, released) = match attempt {
        Ok(Attempt::Empty) => return Ok(()),
        Ok(Attempt::Applied { entry, released }) => {
            println!(
                "Applied {} change(s) to `{}`{}; recorded as entry #{entry}.",
                plan.changes.changes.len(),
                target.label,
                if request.staged { " (staged)" } else { "" }
            );
            // Preserve the original public hook contract: successful applies
            // receive the exact approved plan JSON. Attempt events use their
            // own key so existing success-only integrations cannot be invoked
            // on a failure or misread an unrelated payload shape.
            if let Some(hook) = &project.config.hooks.on_apply {
                crate::hooks::run(hook, &raw, "on_apply");
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
        let message = error.to_string();
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

/// Everything `apply` does once it knows which artifact it is holding.
///
/// Returns through [`Attempt`] or through `Err`, and nothing else: the caller
/// turns both into the attempt event, so a refusal added here later cannot
/// bypass the hook by construction.
fn apply_identified(
    project: &Project,
    target: &Target,
    request: &ApplyRequest<'_>,
    plan: &pbps_model::SavedPlan,
    plan_checksum: &str,
    dialect: &dyn pbps_dialect::Dialect,
    operator: &str,
) -> anyhow::Result<Attempt> {
    let ApplyRequest {
        plan_path,
        approved_checksum,
        allow,
        staged,
        resume,
    } = *request;

    if plan.version != pbps_model::plan::CURRENT_VERSION {
        bail!(
            "`{}` is a version {} plan and this tool understands version {}",
            plan_path.display(),
            plan.version,
            pbps_model::plan::CURRENT_VERSION
        );
    }
    if plan_checksum != approved_checksum {
        bail!(
            "`{}` no longer matches the artifact approved at the deployment gate.\n\
             approved checksum: {approved_checksum}\n\
             plan checksum now: {plan_checksum}\n\
             Do not approve the new value implicitly: retrieve the reviewed artifact or run \
             `pbps explain --plan ...` and send it through approval again.",
            plan_path.display()
        );
    }
    // A preview's baseline is a git revision, so its checksum describes
    // something that is not this environment at all. There is no flag for this:
    // comparing it would be theatre.
    if !plan.origin.is_applyable() {
        bail!(
            "`{}` is an offline preview, not an applyable plan.\n\
             Recompute it against the target with `pbps plan --db ... --out {}`.",
            plan_path.display(),
            plan_path.display()
        );
    }
    if plan.dialect != dialect.name() {
        bail!(
            "`{}` was computed for {} and this project is {}",
            plan_path.display(),
            plan.dialect,
            dialect.name()
        );
    }
    crate::validate_saved_plan(plan, dialect)?;
    // The mode lives in the file because that is what the gate approved; the
    // flag exists so that the CI configuration says out loud which kind of
    // deployment this is. A disagreement between them is somebody's mistake,
    // and running whichever was typed would be the tool choosing the loser.
    if plan.mode.is_staged() != staged {
        bail!(
            "`{}` is a {} plan and `--staged` was {}.\n\
             A staged plan runs outside a transaction and is applied with `--staged`; a \
             transactional one is not.",
            plan_path.display(),
            plan.mode,
            if staged { "given" } else { "not given" }
        );
    }
    if resume && !staged {
        bail!("--resume continues a staged apply; pass --staged as well");
    }

    // A cluster role's rename has no SQL, but its approved identity mapping
    // still has to be checked and recorded. Additions and removal of the last
    // role need the same check (421). SQL Server keeps its connection-free
    // empty path; PostgreSQL decides whether the role map moved under the lock.
    if plan.changes.is_empty() && dialect.manages_roles() {
        println!("The plan is empty; nothing to apply.");
        return Ok(Attempt::Empty);
    }

    // The gate. It can be this coarse precisely because the checksum pins the
    // plan: what `--allow destructive` approves is this plan's destruction and
    // no other (SPEC §7.3).
    let unapproved = plan.changes.unapproved_risks(allow);
    if !unapproved.is_empty() {
        let names: Vec<&str> = unapproved.iter().map(|r| r.as_str()).collect();
        bail!(
            "this plan carries risks that were not approved: {}.\n\
             Read {} and, if it is what you intend, re-run with --allow {}",
            names.join(", "),
            plan_path.display(),
            plan.changes
                .gated_risks()
                .iter()
                .map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
    }

    let statements = crate::statements(&plan.changes, dialect)?;
    crate::engine::require_transactional_rebuilds(target.driver(), &plan.changes, staged)?;
    // Only for a transactional plan. A staged one exists *because* its
    // statement cannot run inside a transaction (ADR-0003): `plan --db
    // --staged` accepts it deliberately, and rejecting it here would leave
    // staged execution refusing the only kind of change it is for.
    //
    // Nothing the T-SQL emitter writes is marked non-transactional yet, so this
    // is latent — which is the reason to fix it now rather than when the first
    // such statement arrives and the guard swallows every staged apply.
    if !staged {
        reject_non_transactional(&statements)?;
    }
    let targets = crate::engine::rename_targets(target.driver(), &plan.changes);
    let deployment = Deployment {
        project,
        target,
        plan,
        plan_checksum,
        statements: &statements,
        rename_targets: &targets,
        dialect,
        operator,
    };

    let (result, released) = db::runtime()?.block_on(async {
        let mut conn = db::connect(target).await?;

        // The lock comes first, before the checks and not after them: a
        // pre-flight that passed while another pipeline was mid-apply would
        // have been answered about a database that is already moving.
        crate::engine::lock(&mut conn, operator).await?;
        let result = if staged {
            apply_staged_under_lock(&mut conn, &deployment, resume)
                .await
                .map(Some)
        } else {
            apply_under_lock(&mut conn, &deployment).await
        };
        if let Err(error) = &result {
            record_failed_apply(&mut conn, &deployment, error).await;
        }
        // Released whatever happened. A lock left behind by a failed apply
        // blocks the very pipeline that would fix it.
        let released = crate::engine::unlock(&mut conn).await;
        Ok::<_, anyhow::Error>((result, released))
    })?;

    Ok(match result {
        Ok(Some(entry)) => Attempt::Applied { entry, released },
        Ok(None) => {
            released.context("the empty plan's deployment lock could not be released")?;
            println!("The plan is empty; nothing to apply.");
            Attempt::Empty
        }
        Err(error) => Attempt::Failed { error, released },
    })
}

/// Best-effort audit row for an apply that did not complete. The schema and ids
/// come from the newest trustworthy checkpoint; for a transactional apply that
/// is the unchanged pre-plan state, and for a staged apply it is the last
/// completed statement. A failure to write the audit must never replace the
/// original deployment error.
async fn record_failed_apply(conn: &mut Conn, d: &Deployment<'_>, error: &anyhow::Error) {
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
fn failed_apply_snapshot(
    driver: pbps_db::Driver,
    mut current: StateSnapshot,
    operator: &str,
    attempted_git_sha: Option<String>,
    attempted_plan_checksum: &str,
    error: &anyhow::Error,
) -> StateSnapshot {
    let is_staged_checkpoint = current.staged.is_some();
    current.kind = StateKind::Failed;
    current.operator = operator.to_owned();
    if !is_staged_checkpoint {
        current.git_sha = attempted_git_sha;
        current.plan_checksum = Some(attempted_plan_checksum.to_owned());
    }
    current.reason = Some(crate::engine::truncate_reason(driver, &error.to_string()));
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
struct Deployment<'a> {
    project: &'a Project,
    target: &'a Target,
    plan: &'a pbps_model::SavedPlan,
    plan_checksum: &'a str,
    statements: &'a [pbps_dialect::Statement],
    rename_targets: &'a [pbps_db::impact::RenameTarget],
    dialect: &'a dyn pbps_dialect::Dialect,
    operator: &'a str,
}

/// Everything between taking the lock and releasing it. Returns the ledger id.
async fn apply_under_lock(conn: &mut Conn, d: &Deployment<'_>) -> anyhow::Result<Option<i64>> {
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
    } = *d;
    let Some(mut entry) = crate::engine::latest(conn).await? else {
        bail!(
            "`{}` has a ledger but no entries; a plan cannot be pinned to a state that was never recorded.",
            target.label
        );
    };
    refuse_mid_deployment(&entry, &target.label)?;
    let original_ids = entry.snapshot.ids.clone();
    let role_renames = crate::engine::external_role_renames(conn, &original_ids, &plan.ids).await?;
    if plan.changes.is_empty() && original_ids.roles == plan.ids.roles {
        return Ok(None);
    }
    rename_snapshot_roles(&mut entry.snapshot, &role_renames);
    let recorded_ids = role_scope(&entry.snapshot.ids, &plan.ids, dialect);
    let recorded_modules = managed_modules(Some(&entry.snapshot), None);
    // The scopes the closing read-back will use, expressed in the names the
    // database has *now* — the second projection is compared against that
    // read, so it has to ask it the same question.
    let planned_scopes = scopes_under(&plan.data, &plan.ids, &entry.snapshot.ids);
    let (scoped, mut before) = baseline_state(
        conn,
        &recorded_ids,
        &recorded_modules,
        project.config.unmanaged,
        &pinned_scopes(&entry.snapshot.schema.data_scopes(), &planned_scopes),
        &entry.snapshot.schema,
        &planned_scopes,
    )
    .await?;
    retain_managed_roles(&mut before, &plan.ids, dialect);

    // The drift check, and the whole reason a coarse `--allow` is safe: this
    // plan is only valid against the environment it was computed against, down
    // to the identity mapping.
    let live = pbps_model::state_checksum(&scoped.schema, &recorded_ids);
    if live != plan.baseline.checksum {
        bail!(
            "`{}` is no longer the database this plan was computed against.\n\
             plan baseline: {}\n\
             database now:  {live}\n\
             Something changed since the plan was approved. `pbps verify` shows what; \
             then recompute the plan with `pbps plan --db`.",
            target.label,
            plan.baseline.checksum
        );
    }

    // And the same guard the recording commands use (110): a permission the
    // declarations cannot hold is invisible to the checksum above, so it
    // reaches here as a clean baseline, and the closing snapshot would write
    // the environment down without it. Refused before statement one, where a
    // refusal still costs nothing.
    refuse_unexpressible(&scoped, &target.label, "apply again")?;

    preflight(conn, dialect, plan, rename_targets).await?;

    println!("Applying {} statement(s)...", statements.len());
    let result = async {
        conn.begin(dialect.transaction_framing()).await?;
        crate::engine::external_role_renames(conn, &original_ids, &plan.ids).await?;
        crate::engine::check_module_rebuilds(conn, &plan.changes, false).await?;
        execute_statements(conn, statements).await?;
        crate::engine::check_module_rebuilds(conn, &plan.changes, true).await?;
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
        let after = managed_state(
            conn,
            &plan.ids,
            &modules_after(&entry.snapshot, &plan.changes, Settled::Whole),
            project.config.unmanaged,
            &plan.data,
            &Schema::default(),
            crate::engine::Read::InsideOwnTransaction,
        )
        .await?;
        // Everything this plan does not touch has to be what the baseline
        // held, down to the rows of a table it does touch that no change of
        // it names (DECISIONS 150, 153). The read above is what gets
        // recorded, so a change another session made between the two reads
        // would otherwise be blessed as this plan's result. And the same rule
        // the recording commands follow: a permission the declarations cannot
        // express stops a state being written down, whether it was there at
        // the start or arrived during the run (110).
        refuse_unplanned_movement(
            dialect,
            &plan.changes,
            &before,
            &after.schema,
            &target.label,
            Settled::Closing,
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "{e:#}\n\n\
                 Nothing has been applied — the transaction was rolled back. `pbps verify` \
                 shows what moved; then apply again."
            )
        })?;
        refuse_unexpressible(&after, &target.label, "apply again")?;
        let mut snapshot = pbps_model::StateSnapshot::new(
            pbps_model::StateKind::Apply,
            after.schema,
            // The plan's mapping, not the baseline's: after a rename the two
            // disagree, and recording the old one would say the rename never
            // happened — leaving the next plan to propose it all over again.
            plan.ids.clone(),
            operator,
        );
        snapshot.module_deps = plan.module_deps.clone();
        snapshot.git_sha = plan.git_sha.clone().or_else(|| db::git_sha(project.root()));
        snapshot.plan_checksum = Some(plan_checksum.to_owned());
        // What the previous state declared, advanced by what this plan wrote:
        // from the plan alone, since `apply --plan` needs nothing else.
        snapshot.declared = entry.snapshot.declared.clone();
        snapshot.declared.advance(&plan.changes);
        Ok::<_, anyhow::Error>(crate::engine::record(conn, &snapshot).await?)
    }
    .await;
    finish_transaction(conn, dialect, result).await.map(Some)
}

/// The cluster already moved these identities. Both the planner and the
/// checkout-free apply must scope and pin the same names (DECISIONS 377).
fn rename_snapshot_roles(snapshot: &mut StateSnapshot, renames: &BTreeMap<String, String>) {
    for (from, to) in renames {
        snapshot.ids.rename_role(from, to);
        if let Some(role) = snapshot.schema.roles.remove(from) {
            snapshot.schema.roles.insert(to.clone(), role);
        }
    }
}

/// Incoming cluster roles belong in the queried and pinned baseline even
/// before their UIDs have reached the ledger. Existing names keep their
/// baseline UID; only their post-apply identity changes when a UID is replaced.
fn role_scope(recorded: &IdsFile, declared: &IdsFile, dialect: &dyn Dialect) -> IdsFile {
    let mut scoped = recorded.clone();
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
async fn apply_staged_under_lock(
    conn: &mut Conn,
    d: &Deployment<'_>,
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
    } = *d;
    let Some(mut entry) = crate::engine::latest(conn).await? else {
        bail!(
            "`{}` has a ledger but no entries; a plan cannot be pinned to a state that was never recorded.",
            target.label
        );
    };

    let original_ids = entry.snapshot.ids.clone();
    let role_renames = crate::engine::external_role_renames(conn, &original_ids, &plan.ids).await?;
    rename_snapshot_roles(&mut entry.snapshot, &role_renames);

    // Both branches read once and hand back what they validated: the
    // statement to start at, and the state the checkpoints are measured
    // against. Returned together so there is no way to reach the loop with a
    // baseline from some other read (DECISIONS 174).
    let (start, mut previous) = if resume {
        let progress = match (&entry.snapshot.staged, entry.snapshot.kind) {
            (Some(p), StateKind::Staged | StateKind::Failed) => p.clone(),
            _ => bail!(
                "`{}` has no staged apply in progress: its newest entry (#{}) is an ordinary \
                 {} state.\n\
                 Run `pbps apply --staged` without `--resume` to start this plan.",
                target.label,
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
        let (scoped, watching) = staged_baseline(
            conn,
            at_checkpoint,
            &after_modules,
            &after_modules,
            project.config.unmanaged,
            &entry.snapshot.schema.data_scopes(),
            &entry.snapshot.schema,
            &scopes_at(plan, at_checkpoint),
        )
        .await?;

        let live = pbps_model::state_checksum(&scoped.schema, at_checkpoint);
        let checkpoint = pbps_model::state_checksum(&entry.snapshot.schema, at_checkpoint);
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
                target.label,
                entry.id
            );
        }
        // Nor can it see a permission the declarations cannot hold, for the
        // same reason: it is beside the comparison, not in it (110). A
        // resume that ran on would close the deployment with a snapshot
        // written without it.
        refuse_unexpressible(&scoped, &target.label, "resume again")?;
        // The role a `DROP ROLE` will meet is the role as the plan left it:
        // the members whose `DROP MEMBER` has not run yet, and nothing
        // owned. Membership and ownership are outside the checksum on
        // purpose, so the check above cannot see a member added while the
        // deployment was paused (DECISIONS 102).
        check_role_drops(
            conn,
            &role_drop_expectations(&plan.changes, dialect, progress.completed)?,
        )
        .await?;
        // And a principal created while it was paused, under a name a
        // remaining statement needs (119).
        let (wanted, vacated) = role_name_expectations(&plan.changes, dialect, progress.completed)?;
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
                target.label,
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
        let (scoped, mut watching) = staged_baseline(
            conn,
            &recorded_ids,
            &recorded_modules,
            &modules_after(&entry.snapshot, &plan.changes, Settled::SoFar),
            project.config.unmanaged,
            &pinned_scopes(
                &entry.snapshot.schema.data_scopes(),
                &scopes_under(&plan.data, &plan.ids, &entry.snapshot.ids),
            ),
            &entry.snapshot.schema,
            &scopes_at(plan, &recorded_ids),
        )
        .await?;
        retain_managed_roles(&mut watching, &plan.ids, dialect);

        let live = pbps_model::state_checksum(&scoped.schema, &recorded_ids);
        if live != plan.baseline.checksum {
            bail!(
                "`{}` is no longer the database this plan was computed against.\n\
                 plan baseline: {}\n\
                 database now:  {live}\n\
                 Something changed since the plan was approved. `pbps verify` shows what; \
                 then recompute the plan with `pbps plan --db --staged`.",
                target.label,
                plan.baseline.checksum
            );
        }
        refuse_unexpressible(&scoped, &target.label, "apply again")?;
        // Only on a fresh start. The probes name objects as the catalog had
        // them before the first statement, and after a partial run some of
        // those names have already moved — a probe answered about the wrong
        // object is worse than one that was not asked.
        preflight(conn, dialect, plan, rename_targets).await?;
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

    let total = statements.len();
    // The names the catalog has right now. It starts at whatever the newest
    // entry recorded — the last ordinary state on a fresh run, the checkpoint
    // on a resume — and each statement moves it, using what the emitter said
    // that statement does (`Statement::renames`).
    let mut live_ids = entry.snapshot.ids.clone();
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
        "Applying {} statement(s) without a transaction...",
        total - start
    );
    for (i, stmt) in statements.iter().enumerate().skip(start) {
        if let Err(e) = conn.execute(&stmt.sql).await {
            return Err(anyhow::anyhow!(
                "the database rejected statement {} of {total}, and nothing was rolled back \
                 (a staged apply runs outside a transaction):\n{}\n\n{e}\n\n\
                 The ledger records everything that did complete. Fix the cause, then continue \
                 with `pbps apply --staged --resume`.",
                i + 1,
                stmt.sql
            ));
        }

        // Applied before the checkpoint is taken: this statement has committed,
        // so the names it moved are the names the catalog has now.
        for (from, to) in &stmt.renames {
            live_ids.rename_table(from, to);
        }
        for (from, to) in &stmt.role_renames {
            live_ids.rename_role(from, to);
        }
        adopt_created(&mut live_ids, &plan.ids, &stmt.creates);

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
            pbps_config::Unmanaged::Ignore,
            &scopes_at(plan, &live_ids),
            &Schema::default(),
            crate::engine::Read::Snapshot,
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
        let id = crate::engine::record(conn, &checkpoint).await?;
        println!("  statement {} of {total} done (checkpoint #{id})", i + 1);
        // The checkpoint is written *first*, and then the run stops. It says
        // what the database holds, which is the one thing a resume needs to be
        // true — refusing before writing it would lose the record of a
        // statement that has already committed, which is what checkpoints are
        // for. Stopping is the whole remedy a staged run has.
        staged_movement(
            dialect,
            &plan.changes,
            &previous,
            &recorded,
            &target.label,
            target.environment(),
            StagedRead::Checkpoint {
                completed: i + 1,
                total,
            },
        )?;
        previous = recorded;
    }

    // The closing entry is an ordinary apply with no staged marker: its absence
    // is what tells every later command this environment is no longer
    // mid-deployment.
    let after = managed_state(
        conn,
        &plan.ids,
        &modules_after(&entry.snapshot, &plan.changes, Settled::Whole),
        project.config.unmanaged,
        &plan.data,
        &Schema::default(),
        crate::engine::Read::Snapshot,
    )
    .await?;
    // And the last window of all: between the final checkpoint and this read.
    // Refused *before* the ordinary entry is written, because that entry is
    // what says the deployment finished — leaving the environment on its last
    // checkpoint is the honest answer, and `refuse_mid_deployment` then makes
    // every other command say so.
    staged_movement(
        dialect,
        &plan.changes,
        &previous,
        &after.schema,
        &target.label,
        target.environment(),
        StagedRead::Closing { total },
    )?;
    let mut snapshot = pbps_model::StateSnapshot::new(
        pbps_model::StateKind::Apply,
        after.schema,
        plan.ids.clone(),
        operator,
    );
    snapshot.module_deps = plan.module_deps.clone();
    snapshot.git_sha = plan.git_sha.clone().or_else(|| db::git_sha(project.root()));
    snapshot.plan_checksum = Some(plan_checksum.to_owned());
    // What the previous state declared, advanced by what this plan wrote: from
    // the plan alone, since `apply --plan` needs nothing else (SPEC §7.3).
    snapshot.declared = entry.snapshot.declared.clone();
    snapshot.declared.advance(&plan.changes);
    Ok(crate::engine::record(conn, &snapshot).await?)
}

/// The movement check a staged run can make: each read compared with the one
/// before it, over everything this plan does not touch.
///
/// Exempting what the *whole* plan touches rather than only the statement just
/// run is deliberate. It is the conservative direction — an object a later
/// statement will change is simply not compared yet — and this guard has been
/// wrong four times in the other one, inventing movement rather than missing
/// it (152 to 158). What it costs is a change to an object the plan touches
/// later; what it buys is that no correct staged apply is ever stopped by it.
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

fn staged_movement(
    dialect: &dyn pbps_dialect::Dialect,
    changes: &pbps_model::ChangeSet,
    before: &Schema,
    after: &Schema,
    label: &str,
    environment: Option<&str>,
    read: StagedRead,
) -> anyhow::Result<()> {
    // Only the last read of a staged run can be asked what the plan achieved:
    // at a checkpoint most of the plan has not happened, and putting its
    // postconditions to one demanded changes that were still to come
    // (DECISIONS 161).
    let settled = match read {
        StagedRead::Checkpoint { completed, total } if completed < total => Settled::SoFar,
        StagedRead::Checkpoint { .. } => Settled::Whole,
        StagedRead::Closing { .. } => Settled::Closing,
    };
    refuse_unplanned_movement(dialect, changes, before, after, label, settled).map_err(|e| {
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
        progress.total
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
fn role_drop_expectations(
    cs: &pbps_model::ChangeSet,
    dialect: &dyn pbps_dialect::Dialect,
    completed: usize,
) -> anyhow::Result<Vec<(String, Vec<String>)>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    for p in &cs.changes {
        let n = dialect
            .emit(&p.change, p.strategy)
            .map_err(|e| anyhow::anyhow!("cannot render a change as SQL: {e}"))?
            .len();
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
    expected: &[(String, Vec<String>)],
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
        let listed: std::collections::BTreeSet<&str> = listed.iter().map(String::as_str).collect();
        let now: std::collections::BTreeSet<&str> = members_now
            .get(name.as_str())
            .map(|m| m.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let added: Vec<&str> = now.difference(&listed).copied().collect();
        let gone: Vec<&str> = listed.difference(&now).copied().collect();
        if !added.is_empty() || !gone.is_empty() {
            let mut detail = Vec::new();
            if !added.is_empty() {
                detail.push(format!(
                    "member(s) the plan did not list: {}",
                    added.join(", ")
                ));
            }
            if !gone.is_empty() {
                detail.push(format!(
                    "listed member(s) no longer in it: {}",
                    gone.join(", ")
                ));
            }
            bail!(
                "role `{name}` is not the role this plan was made against: {}.\n\
                 Its membership changed after `plan --db`, and the plan lists who loses \
                 the role so a reviewer can see it. Recompute it with `pbps plan --db`.",
                detail.join("; ")
            );
        }
        if let Some(securables) = owned_now.get(name.as_str())
            && !securables.is_empty()
        {
            bail!(
                "role `{name}` cannot be dropped: it owns {}, which it did not when this \
                 plan was made.\n\
                 Move the ownership first (`ALTER AUTHORIZATION ON {} TO dbo;`, by hand, \
                 since pbps does not decide who owns a securable), then recompute the plan \
                 with `pbps plan --db`.",
                securables.join(", "),
                securables[0]
            );
        }
    }
    Ok(())
}

/// The settings this dialect's statements depend on, established on the
/// connection.
///
/// Idempotent by construction — it is a list of `SET`s — and issued from more
/// than one place on purpose. See [`run_probes`], which does not trust its
/// caller to have called this.
async fn pin_session(conn: &mut Conn, dialect: &dyn pbps_dialect::Dialect) -> anyhow::Result<()> {
    if let Some(pins) = dialect.session_pins() {
        conn.execute(pins)
            .await
            .context("the session settings this dialect's statements depend on could not be set")?;
    }
    Ok(())
}

async fn preflight(
    conn: &mut Conn,
    dialect: &dyn pbps_dialect::Dialect,
    plan: &pbps_model::SavedPlan,
    rename_targets: &[pbps_db::impact::RenameTarget],
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
            verdict.refused_online.join(", "),
            verdict.runs
        );
    }

    // A dropped role's members were listed at plan time, and the ledger's
    // checksum cannot see a membership change — membership is each
    // environment's own and outside the managed state on purpose. Read
    // again here, before statement one (DECISIONS 92), and again on a
    // resume, for the members whose statements have not run yet (102).
    check_role_drops(conn, &role_drop_expectations(&plan.changes, dialect, 0)?).await?;
    // The same for a name this plan needs free: a user created since the
    // plan is outside the managed state, so the checksum above cannot see
    // it either, and `CREATE ROLE` would meet it after everything ordered
    // before had run (119).
    let (wanted, vacated) = role_name_expectations(&plan.changes, dialect, 0)?;
    refuse_taken_role_names(conn, &wanted, &vacated).await?;

    // A SCHEMABINDING referrer this plan is about to drop is not a blocker: the
    // module changes sort before the table changes precisely so that the drop
    // runs first (ADR-0002). The catalog is queried before anything executes, so
    // it still sees the dependency — and reporting it would refuse a plan whose
    // own first statement removes the obstacle.
    let dropped = dropped_referrer_names(&plan.changes);

    let mut blocked = Vec::new();
    for target in rename_targets {
        let mut report = crate::engine::rename_impact(conn, target).await?;
        report.blocking.retain(|r| !dropped.contains(&r.name));
        report.advisory.retain(|r| !dropped.contains(&r.name));
        if report.is_empty() {
            continue;
        }
        println!("\n{} {} affects:", target.verb(), report.target);
        for r in &report.advisory {
            let detail = r
                .detail
                .as_deref()
                .map(|d| format!(" — {d}"))
                .unwrap_or_default();
            println!("  {} {}{detail}", r.kind, r.name);
        }
        for r in &report.blocking {
            println!(
                "  {} {} — SCHEMABINDING, which blocks the rename",
                r.kind, r.name
            );
            blocked.push(format!("{} {}", r.kind, r.name));
        }
        if !report.advisory.is_empty() {
            // The list is of what the catalog can see. Applications, reports
            // and downstream ELT are invisible to any query, and implying
            // otherwise is worse than saying nothing.
            println!(
                "  Nothing outside the database is visible here: applications and downstream \
                 consumers need a human's checklist."
            );
        }
    }
    if !blocked.is_empty() {
        bail!(
            "the engine will refuse this rename while these exist: {}.\n\
             Drop or recreate them without SCHEMABINDING first.",
            blocked.join(", ")
        );
    }

    run_probes(conn, dialect, &plan.changes).await
}

/// What this plan implies about the data, asked of the engine (SPEC §7.5).
///
/// **Pins its own session**, and does not take the caller's word for it. A
/// probe reads a rendering, parses a plan literal and parses the operator's own
/// declared expression, and all three are decided by settings the framing pins
/// — which used to be established only *after* this ran (DECISIONS 415). A
/// second `SET` batch costs nothing; a probe answered under the wrong settings
/// refuses a plan this engine takes.
async fn run_probes(
    conn: &mut Conn,
    dialect: &dyn pbps_dialect::Dialect,
    changes: &pbps_model::ChangeSet,
) -> anyhow::Result<()> {
    pin_session(conn, dialect).await?;
    let mut failures = Vec::new();
    let mut passed = 0usize;
    let mut unchecked = 0usize;
    let probes = dialect.preflight(changes);
    for probe in &probes {
        // A probe can still legitimately fail to run — a check whose expression
        // names a column this plan renames, say, since expression text is never
        // rewritten by substitution. That is not a violation and it is not
        // silence either: the engine enforces the constraint inside the
        // transaction, where a failure rolls everything back.
        let rows = match conn.query(&probe.sql).await {
            Ok(rows) => rows,
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
        println!("Pre-flight: {passed} probe(s) passed against the live data.");
    }
    if unchecked > 0 {
        // Said on stdout as well as in the warnings above: an operator reading
        // "3 probes passed" while a fourth went unchecked has been told
        // something true and something misleading in the same breath.
        println!(
            "Pre-flight: {unchecked} probe(s) could not be checked here; the engine enforces \
             them inside the transaction."
        );
    }

    // "No probe" is not "no risk". Saying so keeps the operator's attention
    // where the approval already put it, rather than letting a clean pre-flight
    // read as a clean bill of health.
    let unprobed: Vec<&str> = changes
        .risks()
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
        println!(
            "Pre-flight: {} risk(s) cannot be probed and rest on the approval alone.",
            unprobed.join(", ")
        );
    }
    Ok(())
}

/// §7.5: a plan containing a non-transactional statement fails before it runs.
fn reject_non_transactional(statements: &[pbps_dialect::Statement]) -> anyhow::Result<()> {
    let offenders: Vec<&str> = statements
        .iter()
        .filter(|s| !s.transactional)
        .map(|s| s.sql.as_str())
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
    statements: &[pbps_dialect::Statement],
) -> anyhow::Result<()> {
    conn.begin(dialect.transaction_framing())
        .await
        .context("cannot open a transaction")?;
    execute_statements(conn, statements).await
}

async fn execute_statements(
    conn: &mut Conn,
    statements: &[pbps_dialect::Statement],
) -> anyhow::Result<()> {
    for stmt in statements {
        if let Err(e) = conn.execute(&stmt.sql).await {
            return Err(anyhow::anyhow!(
                "the database rejected this statement, and the whole plan was rolled back:\n\
                 {}\n\n{e}",
                stmt.sql
            ));
        }
    }
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
    let framing = dialect.transaction_framing();
    match result {
        Ok(value) => {
            if let Err(error) = conn.commit(framing).await {
                let _ = conn.rollback(framing).await;
                return Err(
                    anyhow::Error::new(error).context("the transaction could not be committed")
                );
            }
            Ok(value)
        }
        Err(error) => {
            let _ = conn.rollback(framing).await;
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
fn dropped_referrer_names(changes: &pbps_model::ChangeSet) -> BTreeSet<String> {
    changes
        .changes
        .iter()
        .filter(|p| matches!(p.change, pbps_model::Change::DropModule { .. }))
        .filter_map(|p| p.change.module_id())
        .map(|id| id.object_name().to_string())
        .collect()
}

#[cfg(test)]
mod tests {

    /// DECISIONS 415: a probe is answered under the settings the statement it
    /// clears will run under, because [`run_probes`] pins them itself.
    ///
    /// The operator's own session is the hostile one here, and it is hostile in
    /// the way a real one is: `DateStyle` set on the connection, as a
    /// `ALTER ROLE … SET` or a `PGOPTIONS` would. One stored row `2026-01-15`
    /// and the declared `CHECK (d < '02/01/2026')` — 1 February under `DMY`,
    /// 2 January under the pinned `MDY`. The engine takes the constraint; a
    /// probe parsed under `DMY` counts the row and refuses it.
    ///
    /// Asserted against the engine's own verdict on the very statement, not
    /// against a remembered count: the two have to agree, and either one alone
    /// can be wrong for its own reasons.
    #[test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    fn a_probe_is_answered_under_the_settings_the_statement_will_run_under() {
        // A runtime built by hand, as the flow suite does: this workspace's
        // `tokio` carries `net`, `rt` and `time` and not `macros`.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        rt.block_on(a_probe_is_answered_under_the_pins());
    }

    async fn a_probe_is_answered_under_the_pins() {
        use pbps_model::{Change, ChangeSet, CheckConstraint, PlannedChange};

        let url = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &url)
            .await
            .expect("connect");

        let schema = format!("pbps_pins_{}", std::process::id());
        conn.execute(&format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE;
             CREATE SCHEMA {schema};
             CREATE TABLE {schema}.t (d date);
             INSERT INTO {schema}.t VALUES ('2026-01-15');"
        ))
        .await
        .expect("the fixture");

        // The operator's session, before pbps says anything.
        conn.execute("SET DateStyle = 'ISO, DMY';")
            .await
            .expect("the operator's own setting");

        let table: pbps_model::TableName = format!("{schema}.t").parse().expect("a table name");
        let changes = ChangeSet {
            changes: vec![PlannedChange::new(Change::AddCheck {
                table: table.clone(),
                name: "t_ck".to_owned(),
                constraint: CheckConstraint {
                    expression: "d < '02/01/2026'".to_owned(),
                },
            })],
        };

        let dialect = pbps_pg::Postgres::new();
        let probed = run_probes(&mut conn, &dialect, &changes).await;

        // And what the engine does with the statement the probe just judged,
        // under the pins, which is where it will run. Established here rather
        // than relied on from `run_probes`: the engine's verdict is this
        // test's ground truth and must not move with the thing under test, or
        // a regression fails on the wrong assertion.
        conn.execute(
            pbps_dialect::Dialect::session_pins(&dialect).expect("this dialect pins its session"),
        )
        .await
        .expect("the pins the framing would establish");
        let applied = conn
            .execute(&format!(
                "ALTER TABLE {schema}.t ADD CONSTRAINT t_ck CHECK (d < '02/01/2026');"
            ))
            .await;

        let cleanup = conn
            .execute(&format!("DROP SCHEMA {schema} CASCADE;"))
            .await;
        assert!(
            applied.is_ok(),
            "the engine takes this constraint under the pins: {applied:?}"
        );
        assert!(
            probed.is_ok(),
            "the probe refused a plan this engine takes: {probed:?}"
        );
        cleanup.expect("drop");
    }

    #[test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
    fn transactional_read_back_refuses_concurrent_managed_ddl_but_ignores_unmanaged_ddl() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(read_back_under_concurrent_ddl());
    }

    async fn read_back_under_concurrent_ddl() {
        let connection = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
        let mut reader = Conn::connect(pbps_db::Driver::Postgres, &connection)
            .await
            .unwrap();
        let mut writer = Conn::connect(pbps_db::Driver::Postgres, &connection)
            .await
            .unwrap();
        let schema = format!("pbps_capture_{}", std::process::id());
        writer.execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}; CREATE TABLE {schema}.untouched (id integer)")).await.unwrap();
        reader.execute("BEGIN").await.unwrap();
        reader
            .execute(&format!("CREATE TABLE {schema}.own_write (id integer)"))
            .await
            .unwrap();
        let read = crate::engine::Read::InsideOwnTransaction;
        let mut watched = crate::engine::introspect(&mut reader, read)
            .await
            .unwrap()
            .schema;
        watched.tables.retain(|name, _| name.schema == schema);
        watched.modules.clear();
        watched.roles.clear();
        let ids = pbps_diff::observed_ids(&watched, &IdsFile::default());
        let modules = BTreeSet::new();
        let scopes = DataScopes::default();
        let reference = Schema::default();
        let request = ManagedRead {
            ids: &ids,
            modules: &modules,
            unmanaged: pbps_config::Unmanaged::Ignore,
            scopes: &scopes,
            reference: &reference,
            read,
        };
        let result = managed_state_then(&mut reader, &request, || async {
            writer
                .execute(&format!(
                    "ALTER TABLE {schema}.untouched ADD CONSTRAINT concurrent_check CHECK (id > 0)"
                ))
                .await?;
            Ok(())
        })
        .await;
        // The second capture now includes the independently committed check;
        // unrelated objects remain outside the comparison's managed scope.
        let stable = managed_state_then(&mut reader, &request, || async {
            writer
                .execute(&format!("CREATE TABLE {schema}.unmanaged (id integer)"))
                .await?;
            Ok(())
        })
        .await;
        let still_open = pbps_pg::catalog::in_transaction(&mut reader).await.unwrap();
        reader.execute("ROLLBACK").await.unwrap();
        let after = pbps_pg::catalog::introspect(&mut writer).await.unwrap();
        writer
            .execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.to_string().contains("managed state changed during")),
            "{result:?}"
        );
        let stable = stable.expect("unmanaged DDL does not invalidate this recording");
        assert!(
            stable
                .schema
                .tables
                .contains_key(&TableName::new(&schema, "own_write"))
        );
        assert!(
            stable.schema.tables[&TableName::new(&schema, "untouched")]
                .checks
                .contains_key("concurrent_check")
        );
        assert!(still_open, "read-back must preserve the caller transaction");
        assert!(
            after.schema.tables[&TableName::new(&schema, "untouched")]
                .checks
                .contains_key("concurrent_check")
        );
        assert!(
            !after
                .schema
                .tables
                .contains_key(&TableName::new(&schema, "own_write"))
        );
    }

    /// `scope` drops a managed role's *plain* grant on an object nobody
    /// manages, with its reason recorded: that is the object's business. The
    /// unsupported permission beside it — a DENY, a column-level grant, a
    /// WITH GRANT OPTION — was filtered by role alone, so it stopped every
    /// command over a securable outside the managed set (DECISIONS 176).
    #[test]
    fn an_unsupported_permission_on_somebody_elses_object_is_not_this_projects_drift() {
        use pbps_db::catalog::{Pulled, Unexpressible};

        let mine: TableName = "dbo.mine".parse().unwrap();
        let theirs: TableName = "dbo.theirs".parse().unwrap();
        let mut ids = IdsFile::default();
        ids.tables.insert("t_aaaaaa".parse().unwrap(), mine.clone());
        ids.roles
            .insert("r_aaaaaa".parse().unwrap(), "app".to_owned());
        let module: ModuleId = "dbo.v".parse().unwrap();
        let modules: BTreeSet<ModuleId> = [module.clone()].into_iter().collect();

        let entry =
            |role: &str, target: Option<pbps_model::GrantTarget>, what: &str| Unexpressible {
                role: role.to_owned(),
                target,
                what: what.to_owned(),
            };
        let object = |t: &TableName| Some(pbps_model::GrantTarget::Object(t.clone()));
        let pulled = Pulled {
            schema: Schema::default(),
            warnings: Vec::new(),
            limitations: Vec::new(),
            unmanaged_modules: Vec::new(),
            unexpressible: vec![
                entry("app", object(&mine), "on a table this project manages"),
                entry(
                    "app",
                    object(&module.object_name()),
                    "on a module this project manages",
                ),
                entry("app", object(&theirs), "on somebody else's table"),
                entry("other", object(&mine), "an unmanaged role's business"),
                // No object to belong to: a permission on the database itself,
                // and a class the model cannot even name (105).
                entry("app", None, "on the database"),
                // Declarable, so a DENY on one is a difference we must hold.
                entry(
                    "app",
                    Some(pbps_model::GrantTarget::Schema("dbo".to_owned())),
                    "on a schema",
                ),
            ],
        };

        let kept = unexpressible_permissions(&pulled, &ids, &modules);
        assert_eq!(
            kept,
            [
                "on a table this project manages",
                "on a module this project manages",
                "on the database",
                "on a schema",
            ],
            "{kept:?}"
        );
    }

    use super::*;
    use pbps_db::catalog::{Limitation, Pulled, UnmanagedModule};
    use pbps_model::{DataMode, DataScope};

    /// A permission the declarations cannot hold stops every command that
    /// would write the state down, not just the one that plans over it: a
    /// state recorded without it is one `verify` calls drift on sight.
    #[test]
    fn a_state_that_cannot_be_expressed_is_not_recorded_by_any_command() {
        // The real constructor, so the empty case is the one a clean
        // database actually produces.
        let empty = || pbps_diff::scope(&Schema::default(), &IdsFile::default(), &BTreeSet::new());
        for then in ["plan again", "snapshot again", "baseline again"] {
            refuse_unexpressible(&empty(), "prod", then).expect("nothing to refuse");
        }

        let mut held = empty();
        held.unexpressible
            .push("role app_reader holds SELECT ON dbo.t(secret)".to_owned());
        for then in ["plan again", "snapshot again", "baseline again"] {
            let e = refuse_unexpressible(&held, "prod", then).expect_err(then);
            let msg = format!("{e:#}");
            assert!(msg.contains("app_reader"), "{msg}");
            // The remedy names the command the operator actually ran.
            assert!(msg.contains(then), "{msg}");
        }
    }

    /// The guard that keeps `apply` from recording somebody else's change as
    /// its own: everything the plan does not touch has to come back the way
    /// the baseline had it, and everything it does touch is exempt because
    /// changing it is the whole point (DECISIONS 150).
    #[test]
    fn a_change_no_statement_of_this_plan_makes_refuses_the_apply() {
        fn table(ty: &str) -> pbps_model::Table {
            let mut t = pbps_model::Table::default();
            t.columns
                .insert("c".to_owned(), pbps_model::Column::new(ty.parse().unwrap()));
            t
        }
        let dbo = || pbps_model::GrantTarget::Schema("dbo".to_owned());
        fn role(held: &[pbps_model::Permission]) -> pbps_model::Role {
            let mut grants = BTreeMap::new();
            if !held.is_empty() {
                grants.insert(
                    pbps_model::GrantTarget::Schema("dbo".to_owned()),
                    held.iter().copied().collect(),
                );
            }
            pbps_model::Role {
                description: None,
                grants,
            }
        }
        let schema = |cols: &str, held: &[pbps_model::Permission]| {
            let mut s = Schema::default();
            s.tables.insert("dbo.t".parse().unwrap(), table(cols));
            s.roles.insert("app".to_owned(), role(held));
            s
        };
        let changes = |c: Vec<pbps_model::Change>| pbps_model::ChangeSet {
            changes: c.into_iter().map(pbps_model::PlannedChange::new).collect(),
        };
        let refuse = |cs: &pbps_model::ChangeSet, before: &Schema, after: &Schema| {
            refuse_unplanned_movement(
                &pbps_mssql::Mssql,
                cs,
                before,
                after,
                "prod",
                Settled::Whole,
            )
            .map_err(|e| format!("{e:#}"))
        };

        let before = schema("int", &[pbps_model::Permission::Select]);
        // Nothing moved: the plan may be empty and the apply still records.
        refuse(&changes(vec![]), &before, &before).expect("nothing moved");

        // A grant revoked by another session while the plan ran. The plan
        // never mentions the role, so nothing else would ever notice: the
        // read-back records the revocation and the next `verify` is clean.
        let robbed = schema("int", &[]);
        let e = refuse(&changes(vec![]), &before, &robbed).expect_err("the role moved");
        assert!(e.contains("role app"), "{e}");
        // The reason, and no remedy: that is the caller's to name (190).
        assert!(e.contains("written down as this plan's own result"), "{e}");
        assert!(!e.contains("rolled back"), "{e}");

        // The same change, when it *is* this plan's: a plan that revokes the
        // grant must not be refused for having revoked it.
        let planned = changes(vec![pbps_model::Change::Revoke {
            role: "app".to_owned(),
            target: dbo(),
            permissions: [pbps_model::Permission::Select].into_iter().collect(),
        }]);
        refuse(&planned, &before, &robbed).expect("the plan's own change");

        // A table retyped behind the plan's back, and the same retype planned.
        let retyped = schema("bigint", &[pbps_model::Permission::Select]);
        let e = refuse(&changes(vec![]), &before, &retyped).expect_err("the table moved");
        assert!(e.contains("dbo.t"), "{e}");
        let planned = changes(vec![pbps_model::Change::AlterColumnType {
            uid: "c_aaaaaa".parse().unwrap(),
            column: pbps_model::ColumnRef::new("dbo.t".parse().unwrap(), "c"),
            from: "int".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
        }]);
        refuse(&planned, &before, &retyped).expect("the plan's own change");

        // Appearing and vanishing are movements too, and neither reads as
        // "nothing there": a table dropped by somebody else is not the same
        // answer as a table this plan drops.
        let mut gone = before.clone();
        gone.tables.remove(&"dbo.t".parse::<TableName>().unwrap());
        let e = refuse(&changes(vec![]), &before, &gone).expect_err("the table went");
        assert!(e.contains("dbo.t is gone"), "{e}");
        let e = refuse(&changes(vec![]), &gone, &before).expect_err("the table arrived");
        assert!(e.contains("dbo.t is there"), "{e}");
    }

    /// A role the plan touches is exempt down to the permissions it moves, and
    /// no further. A plan that adds one grant says nothing about the rest of
    /// the role's set, and neither does any statement it runs — so exempting
    /// the whole role let a concurrent revoke of an unchanged grant be
    /// recorded as this plan's own result (DECISIONS 156).
    #[test]
    fn a_grant_this_plan_does_not_move_is_still_compared() {
        use pbps_model::{GrantTarget, Permission};
        let dbo = || GrantTarget::Schema("dbo".to_owned());
        let role = |held: &[Permission]| {
            let mut grants = BTreeMap::new();
            if !held.is_empty() {
                grants.insert(dbo(), held.iter().copied().collect());
            }
            pbps_model::Role {
                description: None,
                grants,
            }
        };
        let schema = |held: &[Permission]| {
            let mut s = Schema::default();
            s.roles.insert("app".to_owned(), role(held));
            s
        };
        let plan_granting = |permission: Permission| pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Grant {
                role: "app".to_owned(),
                target: dbo(),
                permissions: [permission].into_iter().collect(),
            })],
        };
        let refuse = |cs: &pbps_model::ChangeSet, before: &Schema, after: &Schema| {
            refuse_unplanned_movement(
                &pbps_mssql::Mssql,
                cs,
                before,
                after,
                "prod",
                Settled::Whole,
            )
            .map_err(|e| format!("{e:#}"))
        };

        let before = schema(&[Permission::Select]);
        let after = schema(&[Permission::Select, Permission::Insert]);
        // The plan's own grant, and nothing else moved.
        refuse(&plan_granting(Permission::Insert), &before, &after).expect("the plan's own change");

        // The same plan, with `SELECT` revoked underneath it by somebody else.
        // The role is named by the plan, so a whole-role exemption saw nothing.
        let robbed = schema(&[Permission::Insert]);
        let e = refuse(&plan_granting(Permission::Insert), &before, &robbed)
            .expect_err("the untouched grant moved");
        assert!(e.contains("role app"), "{e}");
        assert!(e.contains("schema::dbo"), "{e}");

        // And a revoke the plan *did* ask for is not reported as movement.
        let revoking = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Revoke {
                role: "app".to_owned(),
                target: dbo(),
                permissions: [Permission::Select].into_iter().collect(),
            })],
        };
        refuse(&revoking, &before, &schema(&[])).expect("the plan's own revoke");

        // The other half, and the reason the two directions are kept apart:
        // the plan's own change is *verified*, not excused. Nothing else in a
        // run has a postcondition on a permission, so a grant reversed before
        // the read-back — by another session, or by a DDL trigger inside the
        // statement itself — was recorded as the plan's result (DECISIONS 160).
        let e = refuse(&plan_granting(Permission::Insert), &before, &before)
            .expect_err("the grant this plan asked for did not take");
        assert!(e.contains("role app"), "{e}");
        let e = refuse(&revoking, &before, &before)
            .expect_err("the revoke this plan asked for did not take");
        assert!(e.contains("role app"), "{e}");
    }

    /// A table's rename forwards the grants on the table and nothing else.
    /// Where routines have their own namespace a routine may share the
    /// table's name, and its grant stays where it is when the table moves;
    /// forwarding it through the rename refused an apply that had done
    /// exactly what the plan said.
    #[test]
    fn a_table_rename_does_not_forward_the_grant_on_a_routine_of_that_name() {
        use pbps_model::{GrantTarget, Permission};
        let routine: GrantTarget = "dbo.f(integer)".parse().unwrap();
        let with = |table: &str, on_routine: &GrantTarget| {
            let mut grants = BTreeMap::new();
            grants.insert(
                GrantTarget::Object(table.parse().unwrap()),
                [Permission::Select].into_iter().collect::<BTreeSet<_>>(),
            );
            grants.insert(
                on_routine.clone(),
                [Permission::Execute].into_iter().collect::<BTreeSet<_>>(),
            );
            let mut s = Schema::default();
            s.tables
                .insert(table.parse().unwrap(), pbps_model::Table::default());
            s.modules.insert(
                "dbo.f(integer)".parse().unwrap(),
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::Function,
                    description: None,
                    definition: "RETURN 1".to_owned(),
                },
            );
            s.roles.insert(
                "app".to_owned(),
                pbps_model::Role {
                    description: None,
                    grants,
                },
            );
            s
        };
        let rename = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::RenameTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    from: "dbo.f".parse().unwrap(),
                    to: "dbo.g".parse().unwrap(),
                },
            )],
        };
        let refuse = |after: &Schema| {
            refuse_unplanned_movement(
                &pbps_mssql::Mssql,
                &rename,
                &with("dbo.f", &routine),
                after,
                "prod",
                Settled::Whole,
            )
            .map_err(|e| format!("{e:#}"))
        };
        // The table's grant moved with the table; the routine's stayed.
        refuse(&with("dbo.g", &routine)).expect("the routine's grant did not move");
        // A routine grant that *did* move to the new name is movement.
        let moved: GrantTarget = "dbo.g(integer)".parse().unwrap();
        let e = refuse(&with("dbo.g", &moved)).expect_err("the routine's grant moved");
        assert!(e.contains("role app"), "{e}");
    }

    /// The rename impact report names a referrer as the catalog does,
    /// `schema.name`; a trigger this plan drops has to be looked up under that
    /// spelling, not under its id's, or the drop excuses nothing.
    #[test]
    fn a_dropped_trigger_is_excused_from_the_rename_impact_under_its_catalog_name() {
        let changes = pbps_model::ChangeSet {
            changes: vec![
                pbps_model::PlannedChange::new(pbps_model::Change::DropModule {
                    id: "dbo.t.audit".parse().unwrap(),
                    kind: pbps_model::ModuleKind::Trigger,
                }),
                pbps_model::PlannedChange::new(pbps_model::Change::DropModule {
                    id: "dbo.v".parse().unwrap(),
                    kind: pbps_model::ModuleKind::View,
                }),
                pbps_model::PlannedChange::new(pbps_model::Change::AlterModule {
                    id: "dbo.kept".parse().unwrap(),
                    module: Box::new(pbps_model::Module {
                        kind: pbps_model::ModuleKind::View,
                        description: None,
                        definition: "SELECT 1".to_owned(),
                    }),
                }),
            ],
        };
        let names: Vec<String> = dropped_referrer_names(&changes).into_iter().collect();
        assert_eq!(names, ["dbo.audit", "dbo.v"]);
    }

    /// A dropped routine takes its own grants and no other's: where routines
    /// overload, a plan dropping `dbo.f(integer)` says nothing about the
    /// grants on `dbo.f(text)`, and one of those revoked underneath the apply
    /// is movement to refuse, not a permission gone with the drop
    /// (ADR-0009 §1, DECISIONS 158).
    #[test]
    fn a_dropped_overload_does_not_excuse_the_siblings_grants() {
        use pbps_model::{GrantTarget, Permission};
        let f = |body: &str| pbps_model::Module {
            kind: pbps_model::ModuleKind::Function,
            description: None,
            definition: body.to_owned(),
        };
        let int: ModuleId = "dbo.f(integer)".parse().unwrap();
        let text: ModuleId = "dbo.f(text)".parse().unwrap();
        let on = |id: &ModuleId| match id {
            ModuleId::Routine(r) => GrantTarget::Routine(r.clone()),
            ModuleId::Named(_) | ModuleId::Trigger { .. } => unreachable!(),
        };
        let schema = |with_int: bool, text_granted: bool| {
            let mut s = Schema::default();
            if with_int {
                s.modules.insert(int.clone(), f("RETURN 1"));
            }
            s.modules.insert(text.clone(), f("RETURN 'a'"));
            let mut grants = BTreeMap::new();
            if with_int {
                grants.insert(on(&int), [Permission::Execute].into_iter().collect());
            }
            if text_granted {
                grants.insert(on(&text), [Permission::Execute].into_iter().collect());
            }
            s.roles.insert(
                "app".to_owned(),
                pbps_model::Role {
                    description: None,
                    grants,
                },
            );
            s
        };
        let dropping_int = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::DropModule {
                    id: int.clone(),
                    kind: pbps_model::ModuleKind::Function,
                },
            )],
        };
        let refuse = |after: &Schema| {
            refuse_unplanned_movement(
                &pbps_mssql::Mssql,
                &dropping_int,
                &schema(true, true),
                after,
                "prod",
                Settled::Whole,
            )
            .map_err(|e| format!("{e:#}"))
        };
        // The drop took its own grant with it, and the sibling's stands.
        refuse(&schema(false, true)).expect("only the dropped overload's grant went");
        // The sibling's grant went too: not this plan's doing.
        // Named at role granularity: the plan does not name the role, so the
        // whole-role compare answers, as it does for any untouched role.
        let e = refuse(&schema(false, false)).expect_err("the sibling's grant moved");
        assert!(
            e.contains("role app is not what the plan was approved over"),
            "{e}"
        );
    }

    /// A table the plan never mentions can be a hybrid at a checkpoint too,
    /// and it is not enough for the *table* to match one spelling or the
    /// other.
    ///
    /// A child with two foreign keys to two separately renamed parents has one
    /// of them carried and one not after the first statement. Compared whole,
    /// it matches neither the read before it nor that read with every rename
    /// applied, and the checkpoint reported a table the plan does not touch as
    /// unplanned movement.
    ///
    /// The comparison therefore brings the *later* read back to the earlier
    /// spelling part by part, which has no whole-table state to be wrong
    /// about.
    #[test]
    fn a_checkpoint_accepts_a_child_carried_only_halfway() {
        use pbps_model::{Change, ChangeSet, ForeignKey, PlannedChange, Table};

        let child = |first: &str, second: &str| {
            let mut t = Table::default();
            for (n, parent) in [("fk_one", first), ("fk_two", second)] {
                t.foreign_keys.insert(
                    n.into(),
                    ForeignKey {
                        columns: vec!["pid".into()],
                        references_table: parent.parse().unwrap(),
                        references_columns: vec!["id".into()],
                        on_delete: Default::default(),
                        on_update: Default::default(),
                    },
                );
            }
            let mut s = Schema::default();
            s.tables.insert("dbo.child".parse().unwrap(), t);
            s
        };

        let renames = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::RenameTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    from: "dbo.one".parse().unwrap(),
                    to: "dbo.one_x".parse().unwrap(),
                }),
                PlannedChange::new(Change::RenameTable {
                    uid: "t_bbbbbb".parse().unwrap(),
                    from: "dbo.two".parse().unwrap(),
                    to: "dbo.two_x".parse().unwrap(),
                }),
            ],
        };

        // The parents themselves, so the reads are the shape a real one has:
        // whether a name has been reused is decided by what each read holds.
        let with = |first: &str, second: &str, parents: &[&str]| {
            let mut s = child(first, second);
            for t in parents {
                s.tables.insert(t.parse().unwrap(), Table::default());
            }
            s
        };

        // The first parent has been renamed and the second has not: the child
        // is spelled one way for one key and the other way for the other.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renames,
            &with("dbo.one", "dbo.two", &["dbo.one", "dbo.two"]),
            &with("dbo.one_x", "dbo.two", &["dbo.one_x", "dbo.two"]),
            "prod",
            Settled::SoFar,
        )
        .expect("a child carried only halfway");

        // The alias: another session creates a *different* table under the
        // name the rename vacated, and repoints the untouched child at it.
        // Undoing collapses the two identities — the checkpoint's `dbo.one_x`
        // and the closing read's `dbo.one` both rewind to `dbo.one` — and a
        // real retarget of a table the plan does not touch would be recorded
        // as the plan's own result, which SPEC 7.6 promises to catch. Holding
        // both spellings in one read is what says the name has been reused, so
        // that rename is not undone at all and the difference stands.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renames,
            // The checkpoint the last statement left: both renames done.
            &with("dbo.one_x", "dbo.two_x", &["dbo.one_x", "dbo.two_x"]),
            // The read after it: a new `dbo.one` exists and the child points
            // at it.
            &with(
                "dbo.one",
                "dbo.two_x",
                &["dbo.one", "dbo.one_x", "dbo.two_x"],
            ),
            "prod",
            Settled::SoFar,
        )
        .expect_err("a child repointed at a new table under the vacated name");
        assert!(format!("{e:#}").contains("dbo.child"), "{e:#}");

        // The negative case: a parent this plan never names is movement.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renames,
            &child("dbo.one", "dbo.two"),
            &child("dbo.one_x", "dbo.elsewhere"),
            "prod",
            Settled::SoFar,
        )
        .expect_err("a parent no rename of this plan produces");
        assert!(format!("{e:#}").contains("dbo.child"), "{e:#}");
    }

    /// A staged run is checked after **every** statement, with the whole plan
    /// in hand, so a read taken between two renames finds one of them done and
    /// the other not.
    ///
    /// Bringing the previous checkpoint forward through every rename the plan
    /// contains would expect both new spellings and report the half that has
    /// not happened as unplanned movement — a valid staged deployment stopped
    /// at its own checkpoint. The comparison therefore accepts a constraint
    /// under either spelling: the one the read before it had, or the one the
    /// rename leaves.
    #[test]
    fn a_checkpoint_between_two_renames_accepts_both_spellings() {
        use pbps_model::{Change, ChangeSet, Column, PlannedChange, PrimaryKey, Table};

        // The columns and the constraints that name them move together: a
        // read finds both spelled as the statements so far have left them.
        let table = |pk: &str, ix: &str| {
            let mut t = Table {
                columns: [pk, ix]
                    .into_iter()
                    .map(|c| (c.to_string(), Column::new("int".parse().unwrap())))
                    .collect(),
                primary_key: Some(PrimaryKey {
                    name: Some("pk_t".into()),
                    columns: vec![pk.into()],
                }),
                ..Default::default()
            };
            t.unique.insert(
                "uq_t".into(),
                pbps_model::UniqueConstraint {
                    columns: vec![ix.into()],
                },
            );
            let mut s = Schema::default();
            s.tables.insert("dbo.t".parse().unwrap(), t);
            s
        };

        let renames = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::RenameColumn {
                    uid: "c_aaaaaa".parse().unwrap(),
                    table: "dbo.t".parse().unwrap(),
                    from: "a".into(),
                    to: "a2".into(),
                }),
                PlannedChange::new(Change::RenameColumn {
                    uid: "c_bbbbbb".parse().unwrap(),
                    table: "dbo.t".parse().unwrap(),
                    from: "b".into(),
                    to: "b2".into(),
                }),
            ],
        };

        // The first rename has run; the second has not. Both constraints are
        // in a state this plan is passing through.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renames,
            &table("a", "b"),
            &table("a2", "b"),
            "prod",
            Settled::SoFar,
        )
        .expect("a checkpoint between the two renames");

        // And at the end, both.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renames,
            &table("a", "b"),
            &table("a2", "b2"),
            "prod",
            Settled::Whole,
        )
        .expect("both renames done");

        // The negative case: a spelling this plan never produces is movement,
        // at a checkpoint as much as at the end.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renames,
            &table("a", "b"),
            &table("a2", "b"),
            "prod",
            Settled::SoFar,
        );
        e.expect("the control: the same read with nothing else moved");

        // The closing read of a staged run compares two checkpoints, and by
        // then *both* carry the rename: `previous` is the checkpoint the last
        // statement left. Rewinding only the later one would leave the earlier
        // under the new spelling and refuse to close a run that has committed
        // every statement it was asked to.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renames,
            &table("a2", "b2"),
            &table("a2", "b2"),
            "prod",
            Settled::Closing,
        )
        .expect("two reads that both carry the renames");

        // The negative case: a constraint spelled a way no rename of this plan
        // produces is movement, at a checkpoint as much as at the end.
        let mut drifted = table("a2", "b");
        drifted
            .tables
            .get_mut(&"dbo.t".parse::<TableName>().unwrap())
            .unwrap()
            .unique
            .insert(
                "uq_t".into(),
                pbps_model::UniqueConstraint {
                    columns: vec!["elsewhere".into()],
                },
            );
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renames,
            &table("a", "b"),
            &drifted,
            "prod",
            Settled::SoFar,
        )
        .expect_err("a spelling no rename of this plan leaves");
        assert!(format!("{e:#}").contains("uq_t"), "{e:#}");
    }

    /// A module the plan writes is held to the definition it wrote. Nothing
    /// else in a run is: `CREATE OR ALTER` reports success and says nothing
    /// about what is now stored (DECISIONS 160).
    #[test]
    fn a_module_this_plan_writes_is_held_to_what_it_wrote() {
        let view = |definition: &str| pbps_model::Module {
            kind: pbps_model::ModuleKind::View,
            description: None,
            definition: definition.to_owned(),
        };
        let schema_with = |module: Option<pbps_model::Module>| {
            let mut s = Schema::default();
            if let Some(m) = module {
                s.modules.insert("dbo.v".parse().unwrap(), m);
            }
            s
        };
        let writing = |definition: &str| pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::AlterModule {
                    id: "dbo.v".parse().unwrap(),
                    module: Box::new(view(definition)),
                },
            )],
        };
        let before = schema_with(Some(view("SELECT 1")));

        // What the plan wrote is what is there.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &writing("SELECT 2"),
            &before,
            &schema_with(Some(view("SELECT 2"))),
            "prod",
            Settled::Whole,
        )
        .expect("the plan's own definition");

        // Something else rewrote it straight afterwards.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &writing("SELECT 2"),
            &before,
            &schema_with(Some(view("SELECT 3"))),
            "prod",
            Settled::Whole,
        )
        .expect_err("not the definition this plan wrote");
        assert!(format!("{e:#}").contains("dbo.v"), "{e:#}");

        // Or dropped it.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &writing("SELECT 2"),
            &before,
            &schema_with(None),
            "prod",
            Settled::Whole,
        )
        .expect_err("the module is gone");
        assert!(format!("{e:#}").contains("is not there"), "{e:#}");

        // And the other direction: a module this plan drops must be gone.
        let dropping = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::DropModule {
                    id: "dbo.v".parse().unwrap(),
                    kind: pbps_model::ModuleKind::View,
                },
            )],
        };
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &dropping,
            &before,
            &schema_with(None),
            "prod",
            Settled::Whole,
        )
        .expect("the plan's own drop");
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &dropping,
            &before,
            &before,
            "prod",
            Settled::Whole,
        )
        .expect_err("the module is still there");
        assert!(format!("{e:#}").contains("is still there"), "{e:#}");
    }

    /// A touched table is exempt down to the columns and constraints the plan
    /// moves, and no further — the same narrowing its rows got, one level up
    /// (DECISIONS 166).
    #[test]
    fn a_touched_table_answers_for_the_shape_the_plan_leaves_alone() {
        let table = |note: &str, index: Option<&str>| {
            let mut t = pbps_model::Table::default();
            t.columns.insert(
                "note".to_owned(),
                pbps_model::Column::new(note.parse().unwrap()),
            );
            t.columns.insert(
                "other".to_owned(),
                pbps_model::Column::new("int".parse().unwrap()),
            );
            if let Some(name) = index {
                t.indexes.insert(
                    name.to_owned(),
                    pbps_model::Index {
                        columns: vec![pbps_model::IndexColumn {
                            name: "note".to_owned(),
                            descending: false,
                        }],
                        include: Vec::new(),
                        unique: false,
                        filter: None,
                    },
                );
            }
            let mut s = Schema::default();
            s.tables.insert("dbo.t".parse().unwrap(), t);
            s
        };
        // The plan widens `note`, and nothing else.
        let widening = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::AlterColumnType {
                    uid: "c_aaaaaa".parse().unwrap(),
                    column: "dbo.t.note".parse().unwrap(),
                    from: "nvarchar(50)".parse().unwrap(),
                    to: "nvarchar(100)".parse().unwrap(),
                    from_nullable: true,
                    to_nullable: true,
                },
            )],
        };
        let before = table("nvarchar(50)", None);
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &widening,
            &before,
            &table("nvarchar(100)", None),
            "prod",
            Settled::Whole,
        )
        .expect("the column this plan retypes is its own business");

        // An index that arrived on the same table is not.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &widening,
            &before,
            &table("nvarchar(100)", Some("ix_rogue")),
            "prod",
            Settled::Whole,
        )
        .expect_err("an index nobody planned");
        assert!(format!("{e:#}").contains("ix_rogue"), "{e:#}");

        // And so is another column changing underneath it.
        let mut retyped_other = table("nvarchar(100)", None);
        retyped_other
            .tables
            .get_mut(&"dbo.t".parse::<TableName>().unwrap())
            .unwrap()
            .columns
            .insert(
                "other".to_owned(),
                pbps_model::Column::new("bigint".parse().unwrap()),
            );
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &widening,
            &before,
            &retyped_other,
            "prod",
            Settled::Whole,
        )
        .expect_err("a column nobody planned");
        assert!(format!("{e:#}").contains("`other`"), "{e:#}");

        // A constraint dropped and recreated under the same name with a
        // different body is present on both sides; only its definition says
        // so (DECISIONS 167).
        let mut rebuilt = table("nvarchar(50)", Some("ix_note"));
        rebuilt
            .tables
            .get_mut(&"dbo.t".parse::<TableName>().unwrap())
            .unwrap()
            .indexes
            .get_mut("ix_note")
            .unwrap()
            .unique = true;
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &widening,
            &table("nvarchar(50)", Some("ix_note")),
            &rebuilt,
            "prod",
            Settled::Whole,
        )
        .expect_err("the index came back different");
        assert!(format!("{e:#}").contains("ix_note"), "{e:#}");

        // And the shape is compared at every read, not only the settled one:
        // a checkpoint that blessed a change became `previous`, after which
        // the final comparison measured the contaminated shape against itself.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &widening,
            &before,
            &table("nvarchar(50)", Some("ix_rogue")),
            "prod",
            Settled::SoFar,
        )
        .expect_err("mid-run is still a read");
        assert!(format!("{e:#}").contains("ix_rogue"), "{e:#}");

        // The plan's own index change is exempt, both ways round.
        let adding = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::AddIndex {
                    table: "dbo.t".parse().unwrap(),
                    name: "ix_note".to_owned(),
                    index: Box::new(pbps_model::Index {
                        columns: vec![pbps_model::IndexColumn {
                            name: "note".to_owned(),
                            descending: false,
                        }],
                        include: Vec::new(),
                        unique: false,
                        filter: None,
                    }),
                },
            )],
        };
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &adding,
            &table("nvarchar(50)", None),
            &table("nvarchar(50)", Some("ix_note")),
            "prod",
            Settled::Whole,
        )
        .expect("the index this plan adds");

        // What the plan does to a table's parts is checked too. Nothing else
        // says so: the shape comparison excludes exactly these, and the table
        // check answers only for the table (DECISIONS 168).
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &adding,
            &table("nvarchar(50)", None),
            &table("nvarchar(50)", None),
            "prod",
            Settled::Whole,
        )
        .expect_err("the index this plan adds is not there");
        assert!(format!("{e:#}").contains("ix_note"), "{e:#}");

        // And an index and a check may share a name — separate namespaces to
        // the engine — so a planned change to one must not exempt the other.
        let mut with_check = table("nvarchar(50)", Some("ix_note"));
        let put_check = |s: &mut Schema, expression: &str| {
            s.tables
                .get_mut(&"dbo.t".parse::<TableName>().unwrap())
                .unwrap()
                .checks
                .insert(
                    "ix_note".to_owned(),
                    pbps_model::CheckConstraint {
                        expression: expression.to_owned(),
                    },
                );
        };
        put_check(&mut with_check, "note IS NOT NULL");
        let mut check_moved = table("nvarchar(50)", Some("ix_note"));
        put_check(&mut check_moved, "note <> N''");
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &adding,
            &with_check,
            &check_moved,
            "prod",
            Settled::Whole,
        )
        .expect_err("the check shares a name with the planned index, and is not it");
        assert!(format!("{e:#}").contains("check `ix_note`"), "{e:#}");
    }
    /// A row the plan writes is held to the cells it spelled, once every
    /// statement has run. Its own statement stops speaking at its commit, and
    /// a staged run leaves the row loose from then until the checkpoint read
    /// (DECISIONS 165).
    #[test]
    fn a_row_this_plan_writes_holds_the_cells_it_spelled() {
        use pbps_model::{DataMode, RowKey, TableData, Value};
        let holding = |cells: &[(&str, &str)]| {
            let t = pbps_model::Table {
                data: Some(TableData {
                    mode: DataMode::Exact,
                    rows: [(
                        RowKey::from("k"),
                        cells
                            .iter()
                            .map(|(c, v)| ((*c).to_owned(), Value::Text((*v).to_owned())))
                            .collect::<pbps_model::Row>(),
                    )]
                    .into_iter()
                    .collect(),
                }),
                ..Default::default()
            };
            let mut s = Schema::default();
            s.tables.insert("dbo.t".parse().unwrap(), t);
            s
        };
        let inserting = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::InsertRow {
                    table: "dbo.t".parse().unwrap(),
                    key_column: "code".to_owned(),
                    identity_key: false,
                    key: RowKey::from("k"),
                    row: [("note".to_owned(), Value::Text("wrote".to_owned()))]
                        .into_iter()
                        .collect(),
                    defaults: Default::default(),
                    types: Default::default(),
                },
            )],
        };
        let empty = Schema::default();
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &inserting,
            &empty,
            &holding(&[("note", "wrote")]),
            "prod",
            Settled::Whole,
        )
        .expect("the row holds what the plan wrote");
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &inserting,
            &empty,
            &holding(&[("note", "rewritten")]),
            "prod",
            Settled::Whole,
        )
        .expect_err("something rewrote the cell after the statement committed");
        assert!(format!("{e:#}").contains("`note`"), "{e:#}");

        // A cell the read-back does not carry proves nothing: it is at its
        // column's default, which is where a spelled value equal to that
        // default also lands. Saying anything here would refuse valid applies.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &inserting,
            &empty,
            &holding(&[]),
            "prod",
            Settled::Whole,
        )
        .expect("an omitted cell is a cell at its default");
    }

    /// A module the plan drops leaves the managed set only once the plan has
    /// run. Out of it from the first checkpoint, it was absent from the
    /// checkpoint's schema and invisible to the `--resume` that scopes the
    /// live side the same way — so the remaining `DROP` ran against an object
    /// nobody had looked at since the plan was approved (DECISIONS 164).
    #[test]
    fn a_module_still_to_be_dropped_stays_in_the_staged_scope() {
        let module = |name: &str| {
            (
                name.parse::<pbps_model::ModuleId>().unwrap(),
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::View,
                    description: None,
                    definition: "SELECT 1".to_owned(),
                },
            )
        };
        let mut schema = Schema::default();
        schema.modules.extend([module("dbo.a"), module("dbo.b")]);
        let recorded = StateSnapshot::new(StateKind::Apply, schema, IdsFile::default(), "leon");
        let dropping = pbps_model::ChangeSet {
            changes: ["dbo.a", "dbo.b"]
                .into_iter()
                .map(|name| {
                    pbps_model::PlannedChange::new(pbps_model::Change::DropModule {
                        id: name.parse().unwrap(),
                        kind: pbps_model::ModuleKind::View,
                    })
                })
                .collect(),
        };
        // Once the plan has run they are gone, which is what the closing entry
        // records.
        assert!(
            modules_after(&recorded, &dropping, Settled::Whole).is_empty(),
            "the finished plan drops both"
        );
        // Until then both are still watched — including the one whose own
        // `DROP` has already run, which the read simply finds absent.
        let during = modules_after(&recorded, &dropping, Settled::SoFar);
        assert_eq!(during.len(), 2, "{during:?}");

        // And a module the plan creates is watched from the start either way.
        let creating = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::CreateModule {
                    id: "dbo.c".parse().unwrap(),
                    module: Box::new(module("dbo.c").1),
                },
            )],
        };
        for settled in [Settled::Whole, Settled::SoFar] {
            assert!(
                modules_after(&recorded, &creating, settled)
                    .contains(&"dbo.c".parse::<pbps_model::ModuleId>().unwrap()),
                "a module the plan adds is in the set from the start"
            );
        }
    }

    /// An object this plan creates has no baseline entry, so a loop over the
    /// baseline never visits it — and everything that arrived on it while it
    /// was new went unchecked (DECISIONS 163).
    #[test]
    fn what_this_plan_creates_is_compared_too() {
        use pbps_model::{DataMode, GrantTarget, Permission, RowKey, TableData};

        // A rogue row in a table this plan creates `exact`.
        let created = |keys: &[&str]| {
            let t = pbps_model::Table {
                data: Some(TableData {
                    mode: DataMode::Exact,
                    rows: keys
                        .iter()
                        .map(|k| (RowKey::from(*k), pbps_model::Row::default()))
                        .collect(),
                }),
                ..Default::default()
            };
            let mut s = Schema::default();
            s.tables.insert("dbo.new".parse().unwrap(), t);
            s
        };
        let creating = pbps_model::ChangeSet {
            changes: vec![
                pbps_model::PlannedChange::new(pbps_model::Change::CreateTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    name: "dbo.new".parse().unwrap(),
                    table: Box::new(pbps_model::Table::default()),
                }),
                pbps_model::PlannedChange::new(pbps_model::Change::InsertRow {
                    table: "dbo.new".parse().unwrap(),
                    key_column: "code".to_owned(),
                    identity_key: false,
                    key: RowKey::from("declared"),
                    row: pbps_model::Row::default(),
                    defaults: Default::default(),
                    types: Default::default(),
                }),
            ],
        };
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creating,
            &Schema::default(),
            &created(&["declared"]),
            "prod",
            Settled::Whole,
        )
        .expect("the row this plan inserts into the table it creates");
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creating,
            &Schema::default(),
            &created(&["declared", "rogue"]),
            "prod",
            Settled::Whole,
        )
        .expect_err("a row nobody declared, in a table one statement old");
        assert!(format!("{e:#}").contains("row `rogue`"), "{e:#}");

        // And a grant on a role this plan creates.
        let with_grants = |held: &[Permission]| {
            let mut grants = BTreeMap::new();
            if !held.is_empty() {
                grants.insert(
                    GrantTarget::Schema("dbo".to_owned()),
                    held.iter().copied().collect::<BTreeSet<_>>(),
                );
            }
            let mut s = Schema::default();
            s.roles.insert(
                "app".to_owned(),
                pbps_model::Role {
                    description: None,
                    grants,
                },
            );
            s
        };
        let creating_role = pbps_model::ChangeSet {
            changes: vec![
                pbps_model::PlannedChange::new(pbps_model::Change::CreateRole {
                    uid: "r_aaaaaa".parse().unwrap(),
                    name: "app".to_owned(),
                }),
                pbps_model::PlannedChange::new(pbps_model::Change::Grant {
                    role: "app".to_owned(),
                    target: GrantTarget::Schema("dbo".to_owned()),
                    permissions: [Permission::Select].into_iter().collect(),
                }),
            ],
        };
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creating_role,
            &Schema::default(),
            &with_grants(&[Permission::Select]),
            "prod",
            Settled::Whole,
        )
        .expect("the grant this plan gives the role it creates");
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creating_role,
            &Schema::default(),
            &with_grants(&[Permission::Select, Permission::Delete]),
            "prod",
            Settled::Whole,
        )
        .expect_err("a permission nobody planned, on a role one statement old");
        assert!(format!("{e:#}").contains("role app"), "{e:#}");
    }

    /// Three things the guard could not see, each because of where it looked
    /// rather than what it compared (DECISIONS 162).
    #[test]
    fn the_guard_looks_where_the_plan_reaches() {
        use pbps_model::{DataMode, GrantTarget, Permission, RowKey, TableData, Value};

        // 1. A target only the plan names. Reversed before the read, it is in
        //    neither side's grants and the loop never visited it.
        let role = |grants: BTreeMap<GrantTarget, BTreeSet<Permission>>| {
            let mut s = Schema::default();
            s.roles.insert(
                "app".to_owned(),
                pbps_model::Role {
                    description: None,
                    grants,
                },
            );
            s
        };
        let granting = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Grant {
                role: "app".to_owned(),
                target: GrantTarget::Schema("dbo".to_owned()),
                permissions: [Permission::Select].into_iter().collect(),
            })],
        };
        let none = role(BTreeMap::new());
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &granting,
            &none,
            &none,
            "prod",
            Settled::Whole,
        )
        .expect_err("the first permission on a target is still a permission");
        assert!(format!("{e:#}").contains("role app"), "{e:#}");

        // 2. A row the plan writes, gone before the read. Its statement's own
        //    postcondition stopped speaking at the commit.
        let table = |keys: &[&str]| {
            let t = pbps_model::Table {
                data: Some(TableData {
                    mode: DataMode::Exact,
                    rows: keys
                        .iter()
                        .map(|k| (RowKey::from(*k), pbps_model::Row::default()))
                        .collect(),
                }),
                ..Default::default()
            };
            let mut s = Schema::default();
            s.tables.insert("dbo.t".parse().unwrap(), t);
            s
        };
        let inserting = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::InsertRow {
                    table: "dbo.t".parse().unwrap(),
                    key_column: "code".to_owned(),
                    identity_key: false,
                    key: RowKey::from("new"),
                    row: pbps_model::Row::default(),
                    defaults: Default::default(),
                    types: Default::default(),
                },
            )],
        };
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &inserting,
            &table(&[]),
            &table(&[]),
            "prod",
            Settled::Whole,
        )
        .expect_err("the row this plan inserts is not there");
        assert!(format!("{e:#}").contains("row `new`"), "{e:#}");
        // Mid-run it simply has not happened yet.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &inserting,
            &table(&[]),
            &table(&[]),
            "prod",
            Settled::SoFar,
        )
        .expect("the insert has not run yet");

        // 3. A column change that moves no reading leaves its cells compared.
        let with_note = |note: &str| {
            let t = pbps_model::Table {
                data: Some(TableData {
                    mode: DataMode::Exact,
                    rows: [(
                        RowKey::from("k"),
                        [("note".to_owned(), Value::Text(note.to_owned()))]
                            .into_iter()
                            .collect::<pbps_model::Row>(),
                    )]
                    .into_iter()
                    .collect(),
                }),
                ..Default::default()
            };
            let mut s = Schema::default();
            s.tables.insert("dbo.t".parse().unwrap(), t);
            s
        };
        let tightening = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::AlterColumnNullability {
                    uid: "c_aaaaaa".parse().unwrap(),
                    column: "dbo.t.note".parse().unwrap(),
                    ty: "nvarchar(50)".parse().unwrap(),
                    to_nullable: false,
                },
            )],
        };
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &tightening,
            &with_note("kept"),
            &with_note("rewritten"),
            "prod",
            Settled::Whole,
        )
        .expect_err("nullability rewrites no value, so the cell is still compared");
        assert!(format!("{e:#}").contains("row `k`"), "{e:#}");
    }

    /// A module that changes kind is a drop *and* a create for one name, and
    /// the net of the two is what has to hold: checked separately, the drop
    /// always failed because the create had put the module back, and every
    /// replacement was refused (DECISIONS 161).
    #[test]
    fn a_module_replacement_is_judged_by_its_net_result() {
        let module = |kind: pbps_model::ModuleKind| pbps_model::Module {
            kind,
            description: None,
            definition: "SELECT 1".to_owned(),
        };
        let schema_with = |kind: pbps_model::ModuleKind| {
            let mut s = Schema::default();
            s.modules.insert("dbo.v".parse().unwrap(), module(kind));
            s
        };
        // `order_key` puts the drop first, which is the order a plan holds.
        let replacing = pbps_model::ChangeSet {
            changes: vec![
                pbps_model::PlannedChange::new(pbps_model::Change::DropModule {
                    id: "dbo.v".parse().unwrap(),
                    kind: pbps_model::ModuleKind::View,
                }),
                pbps_model::PlannedChange::new(pbps_model::Change::CreateModule {
                    id: "dbo.v".parse().unwrap(),
                    module: Box::new(module(pbps_model::ModuleKind::Function)),
                }),
            ],
        };
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &replacing,
            &schema_with(pbps_model::ModuleKind::View),
            &schema_with(pbps_model::ModuleKind::Function),
            "prod",
            Settled::Whole,
        )
        .expect("a replacement is a drop and a create, and the create is the net");
    }

    /// A table this plan creates has no `before` entry, so the shape
    /// comparison never ran for one — and `CreateTable` names no column and
    /// no part of its own, so the final checks answered only for the table's
    /// existence. A DDL trigger, or another session between a staged
    /// `CREATE TABLE` and its checkpoint, could add a column or an index to
    /// it and have that recorded as this plan's own result (DECISIONS 181).
    ///
    /// By name here, and by value in the fields the catalog reads back
    /// unchanged (185, 186); a created column's type is compared normalized
    /// because the engine fills in its defaulted arguments.
    #[test]
    fn a_table_this_plan_creates_answers_for_the_shape_it_was_given() {
        let declared = || {
            let mut t = pbps_model::Table::default();
            t.columns.insert(
                "id".to_owned(),
                pbps_model::Column::new("int".parse().unwrap()),
            );
            t
        };
        let creating = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::CreateTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    name: "dbo.new".parse().unwrap(),
                    table: Box::new(declared()),
                },
            )],
        };
        let after_with = |t: pbps_model::Table| {
            let mut s = Schema::default();
            s.tables.insert("dbo.new".parse().unwrap(), t);
            s
        };
        let before = Schema::default();

        // The engine's own spelling of a type is not movement: it fills in
        // the arguments a declaration omits, and `normalize_type` expands the
        // same ones, so the two meet (DECISIONS 186).
        let mut spelled = pbps_model::Table::default();
        spelled.columns.insert(
            "id".to_owned(),
            pbps_model::Column::new("decimal(18,0)".parse().unwrap()),
        );
        let bare = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::CreateTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    name: "dbo.new".parse().unwrap(),
                    table: Box::new({
                        let mut t = pbps_model::Table::default();
                        t.columns.insert(
                            "id".to_owned(),
                            pbps_model::Column::new("decimal".parse().unwrap()),
                        );
                        t
                    }),
                },
            )],
        };
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &bare,
            &before,
            &after_with(spelled),
            "prod",
            Settled::Whole,
        )
        .expect("`decimal` and `decimal(18,0)` are one type");

        // A type that really changed is not that.
        let mut widened = declared();
        widened.columns.insert(
            "id".to_owned(),
            pbps_model::Column::new("bigint".parse().unwrap()),
        );
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creating,
            &before,
            &after_with(widened),
            "prod",
            Settled::Whole,
        )
        .expect_err("a type nobody planned");
        assert!(format!("{e:#}").contains("`id`"), "{e:#}");

        // A column nobody declared.
        let mut extra = declared();
        extra.columns.insert(
            "sneaky".to_owned(),
            pbps_model::Column::new("int".parse().unwrap()),
        );
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creating,
            &before,
            &after_with(extra),
            "prod",
            Settled::Whole,
        )
        .expect_err("a column nobody planned");
        assert!(format!("{e:#}").contains("sneaky"), "{e:#}");

        // An index nobody declared.
        let mut indexed = declared();
        indexed.indexes.insert(
            "ix_rogue".to_owned(),
            pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    name: "id".to_owned(),
                    descending: false,
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
            },
        );
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creating,
            &before,
            &after_with(indexed),
            "prod",
            Settled::Whole,
        )
        .expect_err("an index nobody planned");
        assert!(format!("{e:#}").contains("ix_rogue"), "{e:#}");

        // A foreign key is *not* in the `CreateTable` payload: `diff_partial`
        // takes it out with `std::mem::take` and emits an `AddForeignKey` of
        // its own, because it sorts after every create. Read off the payload
        // alone the expectation is empty, and the key the plan itself adds
        // reads as movement — every created table with a foreign key refused
        // (DECISIONS 182).
        let fk = pbps_model::ForeignKey {
            columns: vec!["id".to_owned()],
            references_table: "dbo.other".parse().unwrap(),
            references_columns: vec!["id".to_owned()],
            on_delete: Default::default(),
            on_update: Default::default(),
        };
        let with_fk = pbps_model::ChangeSet {
            changes: vec![
                pbps_model::PlannedChange::new(pbps_model::Change::CreateTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    name: "dbo.new".parse().unwrap(),
                    table: Box::new(declared()),
                }),
                pbps_model::PlannedChange::new(pbps_model::Change::AddForeignKey {
                    table: "dbo.new".parse().unwrap(),
                    name: "fk_new".to_owned(),
                    constraint: Box::new(fk.clone()),
                }),
            ],
        };
        let mut keyed = declared();
        keyed.foreign_keys.insert("fk_new".to_owned(), fk);
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &with_fk,
            &before,
            &after_with(keyed),
            "prod",
            Settled::Whole,
        )
        .expect("the foreign key this plan adds to the table it creates");

        // Nor is a column just a name. Its catalog-stable fields are its own
        // promise: nullability, identity, and whether it has a default at all
        // (the default's *text* is rewritten by the engine, so only its
        // presence is comparable) (DECISIONS 185).
        let column = |f: &dyn Fn(&mut pbps_model::Column)| {
            let mut c = pbps_model::Column::new("int".parse().unwrap());
            f(&mut c);
            let mut t = pbps_model::Table::default();
            t.columns.insert("id".to_owned(), c);
            t
        };
        let creates = |t: pbps_model::Table| pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::CreateTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    name: "dbo.new".parse().unwrap(),
                    table: Box::new(t),
                },
            )],
        };
        let plain = creates(column(&|_| {}));
        for tamper in [
            (&|c: &mut pbps_model::Column| c.nullable = false) as &dyn Fn(&mut pbps_model::Column),
            &|c: &mut pbps_model::Column| {
                c.identity = Some(pbps_model::Identity {
                    seed: 1,
                    increment: 1,
                })
            },
            &|c: &mut pbps_model::Column| c.default = Some("((0))".to_owned()),
        ] {
            let e = refuse_unplanned_movement(
                &pbps_mssql::Mssql,
                &plain,
                &before,
                &after_with(column(tamper)),
                "prod",
                Settled::Whole,
            )
            .expect_err("a column altered underneath the CREATE");
            assert!(format!("{e:#}").contains("`id`"), "{e:#}");
        }

        // A filtered index replaced by an unfiltered one covers different
        // rows. The predicate's text is the engine's to rewrite; whether
        // there is one at all is not.
        let filtered = |filter: Option<&str>| {
            let mut t = declared();
            t.indexes.insert(
                "ix_new".to_owned(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        name: "id".to_owned(),
                        descending: false,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: filter.map(str::to_owned),
                },
            );
            t
        };
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creates(filtered(Some("[id] > 0"))),
            &before,
            &after_with(filtered(None)),
            "prod",
            Settled::Whole,
        )
        .expect_err("a filter this plan declared, gone");
        assert!(format!("{e:#}").contains("ix_new"), "{e:#}");
        // But the engine's rewriting of the predicate is not movement.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creates(filtered(Some("[id] > 0"))),
            &before,
            &after_with(filtered(Some("([id]>(0))"))),
            "prod",
            Settled::Whole,
        )
        .expect("the engine rewrites a filter's text");

        // A part is not just a name. A primary key put back on different
        // columns, or under a different declared name, is `Some` on both
        // sides and the presence check accepted it (DECISIONS 183).
        let keyed = |name: Option<&str>, columns: &[&str]| {
            let mut t = declared();
            t.primary_key = Some(pbps_model::PrimaryKey {
                name: name.map(str::to_owned),
                columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            });
            t
        };
        let plans = |t: pbps_model::Table| pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::CreateTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    name: "dbo.new".parse().unwrap(),
                    table: Box::new(t),
                },
            )],
        };
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &plans(keyed(Some("pk_new"), &["id"])),
            &before,
            &after_with(keyed(Some("pk_new"), &["other"])),
            "prod",
            Settled::Whole,
        )
        .expect_err("a primary key on columns nobody planned");
        assert!(format!("{e:#}").contains("primary key"), "{e:#}");
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &plans(keyed(Some("pk_new"), &["id"])),
            &before,
            &after_with(keyed(Some("pk_other"), &["id"])),
            "prod",
            Settled::Whole,
        )
        .expect_err("a primary key under a name nobody planned");
        assert!(format!("{e:#}").contains("primary key"), "{e:#}");

        // A declaration that leaves the naming to the database gets whatever
        // the engine generates, and that is not movement.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &plans(keyed(None, &["id"])),
            &before,
            &after_with(keyed(Some("PK__new__3213E83F"), &["id"])),
            "prod",
            Settled::Whole,
        )
        .expect("an unnamed key is named by the database");

        // A unique constraint is nothing but its columns, so the same holds.
        let uniqued = |columns: &[&str]| {
            let mut t = declared();
            t.unique.insert(
                "uq_new".to_owned(),
                pbps_model::UniqueConstraint {
                    columns: columns.iter().map(|c| (*c).to_owned()).collect(),
                },
            );
            t
        };
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &plans(uniqued(&["id"])),
            &before,
            &after_with(uniqued(&["other"])),
            "prod",
            Settled::Whole,
        )
        .expect_err("a unique constraint nobody planned");
        assert!(format!("{e:#}").contains("uq_new"), "{e:#}");

        // The foreign key's own definition, which lives on the
        // `AddForeignKey` change rather than in the payload (182): restoring
        // only its *name* left what it points at unchecked (DECISIONS 184).
        let fk_to = |columns: &[&str], on_delete| pbps_model::ForeignKey {
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            references_table: "dbo.other".parse().unwrap(),
            references_columns: vec!["id".to_owned()],
            on_delete,
            on_update: Default::default(),
        };
        let adding = |fk: pbps_model::ForeignKey| pbps_model::ChangeSet {
            changes: vec![
                pbps_model::PlannedChange::new(pbps_model::Change::CreateTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    name: "dbo.new".parse().unwrap(),
                    table: Box::new(declared()),
                }),
                pbps_model::PlannedChange::new(pbps_model::Change::AddForeignKey {
                    table: "dbo.new".parse().unwrap(),
                    name: "fk_new".to_owned(),
                    constraint: Box::new(fk),
                }),
            ],
        };
        let read_back = |fk: pbps_model::ForeignKey| {
            let mut t = declared();
            t.foreign_keys.insert("fk_new".to_owned(), fk);
            t
        };
        let planned = fk_to(&["id"], pbps_model::ReferentialAction::NoAction);

        // Different columns under the planned name.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &adding(planned.clone()),
            &before,
            &after_with(read_back(fk_to(
                &["other"],
                pbps_model::ReferentialAction::NoAction,
            ))),
            "prod",
            Settled::Whole,
        )
        .expect_err("a foreign key on columns nobody planned");
        assert!(format!("{e:#}").contains("fk_new"), "{e:#}");

        // Same columns, different referential action: the engine will delete
        // rows this plan never said it could.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &adding(planned.clone()),
            &before,
            &after_with(read_back(fk_to(
                &["id"],
                pbps_model::ReferentialAction::Cascade,
            ))),
            "prod",
            Settled::Whole,
        )
        .expect_err("a cascade nobody planned");
        assert!(format!("{e:#}").contains("fk_new"), "{e:#}");

        // And the one the plan actually asked for.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &adding(planned.clone()),
            &before,
            &after_with(read_back(planned)),
            "prod",
            Settled::Whole,
        )
        .expect("the foreign key this plan adds");

        // And one the CREATE asked for that is not there once it has run.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creating,
            &before,
            &after_with(pbps_model::Table::default()),
            "prod",
            Settled::Whole,
        )
        .expect_err("the column the CREATE declared");
        assert!(format!("{e:#}").contains("`id`"), "{e:#}");
    }

    /// A cell the plan writes as an explicit NULL was dropped from the
    /// expectation entirely, on the argument that the read-back omits a NULL
    /// in a column with no default. It does — but the argument only excuses
    /// *absence*. A value that arrived in its place is present, and nothing
    /// looked (DECISIONS 179).
    #[test]
    fn a_cell_this_plan_writes_as_null_is_still_answered_for() {
        let schema_with = |cell: Option<pbps_model::Value>| {
            let mut row = pbps_model::Row::default();
            row.0
                .insert("code".to_owned(), pbps_model::Value::Text("k".into()));
            if let Some(v) = cell {
                row.0.insert("note".to_owned(), v);
            }
            let mut t = pbps_model::Table::default();
            t.columns.insert(
                "code".to_owned(),
                pbps_model::Column::new("varchar(20)".parse().unwrap()),
            );
            t.columns.insert(
                "note".to_owned(),
                pbps_model::Column::new("nvarchar(50)".parse().unwrap()),
            );
            t.data = Some(pbps_model::TableData {
                mode: pbps_model::DataMode::Exact,
                rows: [(pbps_model::RowKey::from("k"), row)].into_iter().collect(),
            });
            let mut s = Schema::default();
            s.tables.insert("dbo.t".parse().unwrap(), t);
            s
        };
        let mut written = pbps_model::Row::default();
        written
            .0
            .insert("code".to_owned(), pbps_model::Value::Text("k".into()));
        written.0.insert("note".to_owned(), pbps_model::Value::Null);
        let inserting = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::InsertRow {
                    table: "dbo.t".parse().unwrap(),
                    key_column: "code".to_owned(),
                    identity_key: false,
                    key: pbps_model::RowKey::from("k"),
                    row: written,
                    defaults: Default::default(),
                    types: Default::default(),
                },
            )],
        };
        let mut before = schema_with(None);
        before
            .tables
            .get_mut(&"dbo.t".parse::<TableName>().unwrap())
            .unwrap()
            .data
            .as_mut()
            .unwrap()
            .rows
            .clear();

        // Absent: a NULL in a column with no default is omitted from the
        // read-back, so absence still proves nothing.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &inserting,
            &before,
            &schema_with(None),
            "prod",
            Settled::Whole,
        )
        .expect("an omitted cell is the NULL this plan wrote");

        // Present and NULL: a column *with* a default reads one back
        // explicitly, and that is the plan's own result too.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &inserting,
            &before,
            &schema_with(Some(pbps_model::Value::Null)),
            "prod",
            Settled::Whole,
        )
        .expect("an explicit NULL is the NULL this plan wrote");

        // Present and something else: somebody wrote it.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &inserting,
            &before,
            &schema_with(Some(pbps_model::Value::Text("rogue".into()))),
            "prod",
            Settled::Whole,
        )
        .expect_err("a value nobody planned");
        assert!(format!("{e:#}").contains("note"), "{e:#}");
    }

    /// The exclusion was per *column*, and its reason covers one *field*. A
    /// plan that retypes a column could not be held to what its type became —
    /// only the engine's stored form says that — but everything else about
    /// that column still came back from two reads, and nothing compared them.
    /// So a default, an identity or a nullability another session changed
    /// while the plan ran was recorded as this plan's own result
    /// (DECISIONS 173).
    #[test]
    fn a_column_this_plan_retypes_still_answers_for_its_other_fields() {
        let schema_with = |ty: &str, default: Option<&str>, nullable: bool| {
            let mut c = pbps_model::Column::new(ty.parse().unwrap());
            c.default = default.map(str::to_owned);
            c.nullable = nullable;
            let mut t = pbps_model::Table::default();
            t.columns.insert("note".to_owned(), c);
            let mut s = Schema::default();
            s.tables.insert("dbo.t".parse().unwrap(), t);
            s
        };
        let widening = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::AlterColumnType {
                    uid: "c_aaaaaa".parse().unwrap(),
                    column: "dbo.t.note".parse().unwrap(),
                    from: "nvarchar(50)".parse().unwrap(),
                    to: "nvarchar(100)".parse().unwrap(),
                    from_nullable: true,
                    to_nullable: true,
                },
            )],
        };
        let before = schema_with("nvarchar(50)", None, true);

        // The type is the plan's own business: only the stored form says what
        // it became, which is why the field is excluded at all.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &widening,
            &before,
            &schema_with("nvarchar(100)", None, true),
            "prod",
            Settled::Whole,
        )
        .expect("the column this plan retypes");

        // A default that arrived beside it is not.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &widening,
            &before,
            &schema_with("nvarchar(100)", Some("N'x'"), true),
            "prod",
            Settled::Whole,
        )
        .expect_err("a default nobody planned");
        assert!(format!("{e:#}").contains("default"), "{e:#}");

        // Nor is a nullability this plan's own statement does not move: the
        // change carries `from_nullable == to_nullable`, so the restatement
        // leaves a value two reads still agree on.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &widening,
            &before,
            &schema_with("nvarchar(100)", None, false),
            "prod",
            Settled::Whole,
        )
        .expect_err("a nullability nobody planned");
        assert!(format!("{e:#}").contains("nullability"), "{e:#}");

        // Where the plan does move it, it is the plan's own business again.
        let tightening = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::AlterColumnType {
                    uid: "c_aaaaaa".parse().unwrap(),
                    column: "dbo.t.note".parse().unwrap(),
                    from: "nvarchar(50)".parse().unwrap(),
                    to: "nvarchar(100)".parse().unwrap(),
                    from_nullable: true,
                    to_nullable: false,
                },
            )],
        };
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &tightening,
            &before,
            &schema_with("nvarchar(100)", None, false),
            "prod",
            Settled::Whole,
        )
        .expect("a type change folds the nullability into itself");

        // And an identity nothing in this model can move is never excused.
        let mut with_identity = schema_with("nvarchar(100)", None, true);
        with_identity
            .tables
            .get_mut(&"dbo.t".parse::<TableName>().unwrap())
            .unwrap()
            .columns
            .get_mut("note")
            .unwrap()
            .identity = Some(pbps_model::Identity {
            seed: 1,
            increment: 1,
        });
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &widening,
            &before,
            &with_identity,
            "prod",
            Settled::Whole,
        )
        .expect_err("an identity nobody planned");
        assert!(format!("{e:#}").contains("identity"), "{e:#}");
    }

    /// `Change::columns` answers "whose *reading* did this move", which 162
    /// narrowed for the row comparison; 166 then reused the same set to
    /// exclude the plan's own shape edits. Nullability rewrites no cell and so
    /// is rightly absent there, but the catalog reads `is_nullable` back — so
    /// the shape comparison saw the plan's own `ALTER COLUMN ... NOT NULL` as
    /// somebody else's work and refused every nullability-only plan
    /// (DECISIONS 170).
    #[test]
    fn a_column_this_plan_makes_not_null_is_its_own_business() {
        let schema_with = |nullable: bool| {
            let mut t = pbps_model::Table::default();
            let mut c = pbps_model::Column::new("int".parse().unwrap());
            c.nullable = nullable;
            t.columns.insert("note".to_owned(), c);
            t.columns.insert(
                "other".to_owned(),
                pbps_model::Column::new("int".parse().unwrap()),
            );
            let mut s = Schema::default();
            s.tables.insert("dbo.t".parse().unwrap(), t);
            s
        };
        let tightening = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::AlterColumnNullability {
                    uid: "c_aaaaaa".parse().unwrap(),
                    column: "dbo.t.note".parse().unwrap(),
                    ty: "int".parse().unwrap(),
                    to_nullable: false,
                },
            )],
        };
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &tightening,
            &schema_with(true),
            &schema_with(false),
            "prod",
            Settled::Whole,
        )
        .expect("the column this plan makes NOT NULL is its own business");

        // Its own, and no wider: another column changing underneath it is
        // still movement.
        let mut other_moved = schema_with(false);
        let mut c = pbps_model::Column::new("int".parse().unwrap());
        c.nullable = false;
        other_moved
            .tables
            .get_mut(&"dbo.t".parse::<TableName>().unwrap())
            .unwrap()
            .columns
            .insert("other".to_owned(), c);
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &tightening,
            &schema_with(true),
            &other_moved,
            "prod",
            Settled::Whole,
        )
        .expect_err("a column nobody planned");
        assert!(format!("{e:#}").contains("`other`"), "{e:#}");
    }

    /// The same for a table's parts, one field over. A constraint or an index
    /// whose *definition* changes is a drop and an add under one name — the
    /// differ's `by_name!` emits both — so holding both outcomes made the add
    /// satisfy `Present` and the drop then fail `Absent`, refusing every
    /// redefinition (DECISIONS 169).
    #[test]
    fn a_redefined_index_is_judged_by_its_net_result() {
        let index = |unique: bool| pbps_model::Index {
            columns: vec![pbps_model::IndexColumn {
                name: "note".to_owned(),
                descending: false,
            }],
            include: Vec::new(),
            unique,
            filter: None,
        };
        let schema_with = |unique: bool| {
            let mut t = pbps_model::Table::default();
            t.indexes.insert("ix_note".to_owned(), index(unique));
            let mut s = Schema::default();
            s.tables.insert("dbo.t".parse().unwrap(), t);
            s
        };
        // `order_key` puts the drop first, which is the order a plan holds.
        let redefining = pbps_model::ChangeSet {
            changes: vec![
                pbps_model::PlannedChange::new(pbps_model::Change::DropIndex {
                    table: "dbo.t".parse().unwrap(),
                    name: "ix_note".to_owned(),
                }),
                pbps_model::PlannedChange::new(pbps_model::Change::AddIndex {
                    table: "dbo.t".parse().unwrap(),
                    name: "ix_note".to_owned(),
                    index: Box::new(index(true)),
                }),
            ],
        };
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &redefining,
            &schema_with(false),
            &schema_with(true),
            "prod",
            Settled::Whole,
        )
        .expect("a redefinition is a drop and an add, and the add is the net");

        // Net, not blanket: a plan that only drops still has to see it gone.
        let dropping = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::DropIndex {
                    table: "dbo.t".parse().unwrap(),
                    name: "ix_note".to_owned(),
                },
            )],
        };
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &dropping,
            &schema_with(false),
            &schema_with(false),
            "prod",
            Settled::Whole,
        )
        .expect_err("the drop did not take");
        assert!(format!("{e:#}").contains("ix_note"), "{e:#}");
    }

    /// A staged checkpoint cannot be asked what the plan achieved: most of it
    /// has not run. Only movement is comparable there (DECISIONS 161).
    #[test]
    fn a_half_run_plan_is_compared_on_movement_alone() {
        let mut before = Schema::default();
        before
            .tables
            .insert("dbo.kept".parse().unwrap(), pbps_model::Table::default());
        let creating = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::CreateTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    name: "dbo.new".parse().unwrap(),
                    table: Box::new(pbps_model::Table::default()),
                },
            )],
        };
        // Mid-run the table is not there yet, and that is not a failure.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creating,
            &before,
            &before,
            "prod",
            Settled::SoFar,
        )
        .expect("the statement has not run yet");
        // Once every statement has run it is one: `CREATE TABLE` reporting
        // success is not the table being there.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creating,
            &before,
            &before,
            "prod",
            Settled::Whole,
        )
        .expect_err("the table this plan creates is not there");
        assert!(format!("{e:#}").contains("dbo.new"), "{e:#}");
        // And movement is compared either way.
        let mut moved = before.clone();
        moved
            .tables
            .remove(&"dbo.kept".parse::<TableName>().unwrap());
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creating,
            &before,
            &moved,
            "prod",
            Settled::SoFar,
        )
        .expect_err("an untouched table went");
        assert!(format!("{e:#}").contains("dbo.kept"), "{e:#}");
    }

    /// And the wiring, not just the function: a staged run asks for movement
    /// at every checkpoint and for the whole answer only at the last read.
    ///
    /// Its own test because the one above calls `refuse_unplanned_movement`
    /// directly — reverting `staged_movement`'s choice of mode left that one
    /// green, which is a revert check that proves nothing (DECISIONS 161).
    #[test]
    fn a_staged_checkpoint_is_not_asked_what_the_plan_achieved() {
        let before = Schema::default();
        let creating = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::CreateTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    name: "dbo.new".parse().unwrap(),
                    table: Box::new(pbps_model::Table::default()),
                },
            )],
        };
        staged_movement(
            &pbps_mssql::Mssql,
            &creating,
            &before,
            &before,
            "prod",
            Some("prod"),
            StagedRead::Checkpoint {
                completed: 1,
                total: 2,
            },
        )
        .expect("statement 1 of 2: the create has not run yet");
        let e = staged_movement(
            &pbps_mssql::Mssql,
            &creating,
            &before,
            &before,
            "prod",
            Some("prod"),
            StagedRead::Checkpoint {
                completed: 2,
                total: 2,
            },
        )
        .expect_err("the last read is asked what the plan achieved");
        let e = format!("{e:#}");
        assert!(e.contains("dbo.new"), "{e}");
        // And the message says what a staged run cannot do about it.
        assert!(e.contains("nothing was rolled back"), "{e}");
    }

    /// A role with no grants has nothing but its name, so nothing else here
    /// would notice it being dropped straight after it was created.
    #[test]
    fn a_role_this_plan_creates_has_to_be_there() {
        let empty = Schema::default();
        let creating = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::CreateRole {
                    uid: "r_aaaaaa".parse().unwrap(),
                    name: "app".to_owned(),
                },
            )],
        };
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creating,
            &empty,
            &empty,
            "prod",
            Settled::Whole,
        )
        .expect_err("the role this plan creates is not there");
        assert!(format!("{e:#}").contains("role app"), "{e:#}");

        let mut made = Schema::default();
        made.roles.insert(
            "app".to_owned(),
            pbps_model::Role {
                description: None,
                grants: BTreeMap::new(),
            },
        );
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &creating,
            &empty,
            &made,
            "prod",
            Settled::Whole,
        )
        .expect("the plan's own role");

        // And a dropped role that came back.
        let dropping = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::DropRole {
                    uid: "r_aaaaaa".parse().unwrap(),
                    name: "app".to_owned(),
                    members: Vec::new(),
                },
            )],
        };
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &dropping,
            &made,
            &made,
            "prod",
            Settled::Whole,
        )
        .expect_err("the role this plan drops is still there");
        assert!(format!("{e:#}").contains("role app"), "{e:#}");
    }

    /// A grant on a table this plan renames is the same grant afterwards, under
    /// the name the table now has. Measured on SQL Server 2025: `sp_rename`
    /// carries the permission to the new name, so nothing about the role
    /// changed and the differ emits no grant change — leaving the role
    /// untouched, and the comparison reading one grant as two (DECISIONS 157).
    #[test]
    fn a_grant_follows_the_table_this_plan_renames() {
        use pbps_model::{GrantTarget, Permission};
        let granted_on = |table: &str| {
            let mut grants = BTreeMap::new();
            grants.insert(
                GrantTarget::Object(table.parse().unwrap()),
                [Permission::Select].into_iter().collect::<BTreeSet<_>>(),
            );
            let mut s = Schema::default();
            s.tables
                .insert(table.parse().unwrap(), pbps_model::Table::default());
            s.roles.insert(
                "app".to_owned(),
                pbps_model::Role {
                    description: None,
                    grants,
                },
            );
            s
        };
        let rename = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::RenameTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    from: "dbo.old".parse().unwrap(),
                    to: "dbo.new".parse().unwrap(),
                },
            )],
        };
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &rename,
            &granted_on("dbo.old"),
            &granted_on("dbo.new"),
            "prod",
            Settled::Whole,
        )
        .expect("the grant moved with the table, which is not movement");

        // And the guard still bites underneath the forwarding: the same rename
        // with the grant actually gone is movement, not bookkeeping.
        let mut robbed = granted_on("dbo.new");
        robbed.roles.get_mut("app").unwrap().grants.clear();
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &rename,
            &granted_on("dbo.old"),
            &robbed,
            "prod",
            Settled::Whole,
        )
        .expect_err("the grant is gone");
        assert!(format!("{e:#}").contains("role app"), "{e:#}");
    }

    /// A column change re-shapes every row of its table without any row change
    /// saying so, and none of that is somebody else's work (DECISIONS 158).
    #[test]
    fn a_column_this_plan_changes_is_not_compared_row_by_row() {
        use pbps_model::{Cell, DataMode, RowKey, TableData, Value};
        let table = |cells: &[(&str, &str)]| {
            let mut t = pbps_model::Table {
                data: Some(TableData {
                    mode: DataMode::Exact,
                    rows: [(
                        RowKey::from("k"),
                        cells
                            .iter()
                            .map(|(c, v)| ((*c).to_owned(), Value::Text((*v).to_owned())))
                            .collect::<pbps_model::Row>(),
                    )]
                    .into_iter()
                    .collect(),
                }),
                ..Default::default()
            };
            // The columns too: a rename is checked for having happened as
            // well as for re-keying the row (DECISIONS 168), and a table
            // whose rows name columns it does not have is not a state any
            // read produces.
            for (column, _) in cells {
                t.columns.insert(
                    (*column).to_owned(),
                    pbps_model::Column::new("nvarchar(50)".parse().unwrap()),
                );
            }
            let mut s = Schema::default();
            s.tables.insert("dbo.t".parse().unwrap(), t);
            s
        };
        let renaming = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::RenameColumn {
                    uid: "c_aaaaaa".parse().unwrap(),
                    table: "dbo.t".parse().unwrap(),
                    from: "old".to_owned(),
                    to: "new".to_owned(),
                },
            )],
        };
        // The one column is renamed, so the row is keyed differently on each
        // side. That is the plan's own doing.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renaming,
            &table(&[("old", "kept"), ("note", "same")]),
            &table(&[("new", "kept"), ("note", "same")]),
            "prod",
            Settled::Whole,
        )
        .expect("a renamed column re-keys the row, which is not movement");

        // And the columns it leaves alone are still compared underneath.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renaming,
            &table(&[("old", "kept"), ("note", "same")]),
            &table(&[("new", "kept"), ("note", "rewritten")]),
            "prod",
            Settled::Whole,
        )
        .expect_err("the untouched column moved");
        assert!(format!("{e:#}").contains("row `k`"), "{e:#}");
        let _ = Cell::Value(Value::Null);
    }

    /// A securable takes its permissions with it, so a plan that drops a
    /// granted table emits no `REVOKE` and the grant is simply gone
    /// (DECISIONS 158).
    #[test]
    fn a_grant_on_a_table_this_plan_drops_goes_with_it() {
        use pbps_model::{GrantTarget, Permission};
        let mut before = Schema::default();
        before
            .tables
            .insert("dbo.t".parse().unwrap(), pbps_model::Table::default());
        before.roles.insert(
            "app".to_owned(),
            pbps_model::Role {
                description: None,
                grants: [(
                    GrantTarget::Object("dbo.t".parse().unwrap()),
                    [Permission::Select].into_iter().collect::<BTreeSet<_>>(),
                )]
                .into_iter()
                .collect(),
            },
        );
        let mut after = Schema::default();
        after.roles.insert(
            "app".to_owned(),
            pbps_model::Role {
                description: None,
                grants: BTreeMap::new(),
            },
        );
        let dropping = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::DropTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    name: "dbo.t".parse().unwrap(),
                },
            )],
        };
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &dropping,
            &before,
            &after,
            "prod",
            Settled::Whole,
        )
        .expect("the grant went with the table it was on");

        // A grant on a table the plan does *not* drop still has to be there.
        let mut before_two = before.clone();
        before_two.roles.get_mut("app").unwrap().grants.insert(
            GrantTarget::Schema("dbo".to_owned()),
            [Permission::Select].into_iter().collect(),
        );
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &dropping,
            &before_two,
            &after,
            "prod",
            Settled::Whole,
        )
        .expect_err("the schema grant did not go anywhere");
        assert!(format!("{e:#}").contains("role app"), "{e:#}");
    }

    /// Both ends of a rename are the plan's business. The recorded state knows
    /// the object by its old name and the read-back by its new one, so a
    /// comparison that took only one end would see the rename itself as a
    /// table vanishing and another appearing — and refuse every rename.
    #[test]
    fn a_rename_exempts_the_name_at_each_end() {
        let mut before = Schema::default();
        before
            .tables
            .insert("dbo.old".parse().unwrap(), pbps_model::Table::default());
        before.roles.insert(
            "was".to_owned(),
            pbps_model::Role {
                description: None,
                grants: Default::default(),
            },
        );
        let mut after = Schema::default();
        after
            .tables
            .insert("dbo.new".parse().unwrap(), pbps_model::Table::default());
        after.roles.insert(
            "now".to_owned(),
            pbps_model::Role {
                description: None,
                grants: Default::default(),
            },
        );
        let uid: pbps_model::Uid = "t_aaaaaa".parse().unwrap();
        let cs = pbps_model::ChangeSet {
            changes: [
                pbps_model::Change::RenameTable {
                    uid: uid.clone(),
                    from: "dbo.old".parse().unwrap(),
                    to: "dbo.new".parse().unwrap(),
                },
                pbps_model::Change::RenameRole {
                    uid,
                    from: "was".to_owned(),
                    to: "now".to_owned(),
                },
            ]
            .into_iter()
            .map(pbps_model::PlannedChange::new)
            .collect(),
        };
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &cs,
            &before,
            &after,
            "prod",
            Settled::Whole,
        )
        .expect("a rename is not movement");
    }

    /// A renamed table's declaration is found under the name the database
    /// still has; an unrenamed one, and one the plan creates, stay put.
    #[test]
    fn the_declarations_are_keyed_by_the_names_the_database_has_now() {
        let uid: pbps_model::Uid = "t_aaaaaa".parse().unwrap();
        let mut final_ids = IdsFile::default();
        final_ids
            .tables
            .insert(uid.clone(), "dbo.t2".parse().unwrap());
        let mut live_ids = IdsFile::default();
        live_ids.tables.insert(uid, "dbo.t".parse().unwrap());
        let mut declared = Schema::default();
        let t = pbps_model::Table {
            data: Some(pbps_model::TableData {
                mode: DataMode::Exact,
                rows: BTreeMap::new(),
            }),
            ..Default::default()
        };
        declared.tables.insert("dbo.t2".parse().unwrap(), t);
        declared
            .tables
            .insert("dbo.new".parse().unwrap(), pbps_model::Table::default());

        let live = tables_under(&declared, &final_ids, &live_ids);
        let names: Vec<String> = live.tables.keys().map(ToString::to_string).collect();
        assert_eq!(names, ["dbo.new", "dbo.t"]);
        assert!(
            live.tables[&"dbo.t".parse::<TableName>().unwrap()]
                .data
                .is_some()
        );
        let scopes = live.data_scopes();
        assert!(scopes.contains_key(&"dbo.t".parse::<TableName>().unwrap()));
    }

    /// The recorded scope wins where there is one; a table only the plan
    /// covers is pinned under the plan's.
    #[test]
    fn a_baseline_is_pinned_under_the_recorded_scopes_plus_the_newly_covered() {
        let t: TableName = "dbo.t".parse().unwrap();
        let u: TableName = "dbo.u".parse().unwrap();
        let recorded: DataScopes = [(
            t.clone(),
            DataScope {
                mode: DataMode::Ensure,
                keys: ["a"].into_iter().map(pbps_model::RowKey::from).collect(),
            },
        )]
        .into_iter()
        .collect();
        let exact = DataScope {
            mode: DataMode::Exact,
            keys: BTreeSet::new(),
        };
        let planned: DataScopes = [(t.clone(), exact.clone()), (u.clone(), exact.clone())]
            .into_iter()
            .collect();
        let pinned = pinned_scopes(&recorded, &planned);
        // Both cover `t`: the plan's `exact` widens the read, and the recorded
        // key is still spelled so the engine can say which row it names.
        assert_eq!(pinned[&t].mode, DataMode::Exact, "{:?}", pinned[&t]);
        assert_eq!(
            pinned[&t].keys,
            ["a"].into_iter().map(pbps_model::RowKey::from).collect()
        );
        assert_eq!(pinned[&u], exact, "newly covered: the plan's");
        assert_eq!(pinned.len(), 2);

        // A key added to an `ensure` block is pinned too: read at plan time,
        // it has to be checked again before apply, or a change to it in
        // between is overwritten by an approved update nobody measured.
        let wider: DataScopes = [(
            t.clone(),
            DataScope {
                mode: DataMode::Ensure,
                keys: ["a", "b"]
                    .into_iter()
                    .map(pbps_model::RowKey::from)
                    .collect(),
            },
        )]
        .into_iter()
        .collect();
        let pinned = pinned_scopes(&recorded, &wider);
        assert_eq!(pinned[&t].mode, DataMode::Ensure);
        assert_eq!(
            pinned[&t].keys,
            ["a", "b"]
                .into_iter()
                .map(pbps_model::RowKey::from)
                .collect()
        );
    }

    /// The mapping from `01` to the stored `1` holds under `int`; a plan
    /// that makes the column `varchar` is refused while it stands, and a
    /// plan on a table whose keys are spelled the engine's way is not.
    #[test]
    fn a_key_type_change_over_an_aliased_key_is_named_and_one_without_is_not() {
        use pbps_model::{Change, ChangeSet, ColumnRef, ColumnType, PlannedChange};
        use std::str::FromStr;
        let table: TableName = "dbo.t".parse().unwrap();
        let mut declared = Schema::default();
        let mut t = pbps_model::Table::default();
        t.columns.insert(
            "id".into(),
            pbps_model::Column::new(ColumnType::from_str("varchar(10)").unwrap()).not_null(),
        );
        t.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["id".into()],
        });
        declared.tables.insert(table.clone(), t);
        let change = |name: &str| {
            PlannedChange::new(Change::AlterColumnType {
                uid: "c_aaaaaa".parse().unwrap(),
                column: ColumnRef::new(table.clone(), name),
                from: ColumnType::from_str("int").unwrap(),
                to: ColumnType::from_str("varchar(10)").unwrap(),
                from_nullable: false,
                to_nullable: false,
            })
        };
        let cs = ChangeSet {
            changes: vec![change("id")],
        };
        let mut rows = pbps_model::ObservedRows::new();
        let mut observed = pbps_model::ObservedTable::default();
        observed.aliases.insert(
            pbps_model::RowKey::from("01"),
            pbps_model::RowKey::from("1"),
        );
        observed
            .aliases
            .insert(pbps_model::RowKey::from("2"), pbps_model::RowKey::from("2"));
        rows.insert(table.clone(), observed);

        // No rename in play: the plan's name is the database's name.
        let same = IdsFile::default();
        let found = key_type_changes_over_aliases(&cs, &declared, &rows, &same, &same);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("`01` (stored as `1`)"), "{}", found[0]);
        assert!(
            !found[0].contains("`2`"),
            "a key spelled the engine's way: {}",
            found[0]
        );

        // Not the key column: nothing to say. Keys all spelled the engine's
        // way: nothing to say either.
        let other = ChangeSet {
            changes: vec![change("label")],
        };
        assert!(key_type_changes_over_aliases(&other, &declared, &rows, &same, &same).is_empty());
        rows.get_mut(&table)
            .unwrap()
            .aliases
            .remove(&pbps_model::RowKey::from("01"));
        assert!(key_type_changes_over_aliases(&cs, &declared, &rows, &same, &same).is_empty());
    }

    /// The change names the table as the plan leaves it; the declarations and
    /// the rows are keyed by the name the database has now. A rename in the
    /// same revision made both lookups miss, and the guard let the key-type
    /// change through.
    #[test]
    fn a_table_renamed_by_the_same_plan_is_still_checked_for_aliased_keys() {
        use pbps_model::{Change, ChangeSet, ColumnRef, ColumnType, PlannedChange};
        use std::str::FromStr;
        let uid: pbps_model::Uid = "t_aaaaaa".parse().unwrap();
        let after: TableName = "dbo.customer".parse().unwrap();
        let before: TableName = "dbo.client".parse().unwrap();
        let mut final_ids = IdsFile::default();
        final_ids.tables.insert(uid.clone(), after.clone());
        let mut live_ids = IdsFile::default();
        live_ids.tables.insert(uid, before.clone());

        let mut t = pbps_model::Table::default();
        t.columns.insert(
            "id".into(),
            pbps_model::Column::new(ColumnType::from_str("varchar(10)").unwrap()).not_null(),
        );
        t.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["id".into()],
        });
        // Keyed by the live name, as `tables_under` leaves it.
        let mut declared = Schema::default();
        declared.tables.insert(before.clone(), t);

        let mut rows = pbps_model::ObservedRows::new();
        let mut observed = pbps_model::ObservedTable::default();
        observed.aliases.insert(
            pbps_model::RowKey::from("01"),
            pbps_model::RowKey::from("1"),
        );
        rows.insert(before, observed);

        let cs = ChangeSet {
            changes: vec![PlannedChange::new(Change::AlterColumnType {
                uid: "c_aaaaaa".parse().unwrap(),
                // The plan's own name for it: the one after the rename.
                column: ColumnRef::new(after, "id"),
                from: ColumnType::from_str("int").unwrap(),
                to: ColumnType::from_str("varchar(10)").unwrap(),
                from_nullable: false,
                to_nullable: false,
            })],
        };
        let found = key_type_changes_over_aliases(&cs, &declared, &rows, &final_ids, &live_ids);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("`01` (stored as `1`)"), "{}", found[0]);
    }

    /// Counted off the emitter's statements: every member before statement
    /// one, fewer as the `DROP MEMBER`s commit, none once the role is gone.
    #[test]
    fn a_resumed_role_drop_expects_only_the_members_whose_statements_remain() {
        use pbps_model::{Change, ChangeSet, PlannedChange};
        let uid: pbps_model::Uid = "r_aaaaaa".parse().unwrap();
        let cs = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::CreateRole {
                    uid: uid.clone(),
                    name: "auditors".into(),
                }),
                PlannedChange::new(Change::DropRole {
                    uid,
                    name: "reporting".into(),
                    members: vec!["a".into(), "b".into()],
                }),
            ],
        };
        let expect =
            |completed: usize| role_drop_expectations(&cs, &pbps_mssql::Mssql, completed).unwrap();
        let both = vec![("reporting".to_owned(), vec!["a".to_owned(), "b".to_owned()])];
        assert_eq!(expect(0), both, "before statement one");
        assert_eq!(
            expect(1),
            both,
            "the CREATE ROLE is done; the drop has not started"
        );
        assert_eq!(
            expect(2),
            vec![("reporting".to_owned(), vec!["b".to_owned()])],
            "one DROP MEMBER committed"
        );
        assert_eq!(
            expect(3),
            vec![("reporting".to_owned(), vec![])],
            "every member gone; the DROP ROLE still to run"
        );
        assert!(
            expect(4).is_empty(),
            "the role is dropped; nothing to expect"
        );
    }

    /// The names the remaining statements need free and the ones they free
    /// first, counted off the emitter's statements the same way: a rename
    /// wants its new name and vacates its old one until its statement runs,
    /// and a half-done role drop still holds the name it is vacating.
    #[test]
    fn a_resume_asks_only_about_the_role_names_its_remaining_statements_touch() {
        use pbps_model::{Change, ChangeSet, PlannedChange};
        let r1: pbps_model::Uid = "r_aaaaaa".parse().unwrap();
        let r2: pbps_model::Uid = "r_bbbbbb".parse().unwrap();
        let r3: pbps_model::Uid = "r_cccccc".parse().unwrap();
        let cs = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::RenameRole {
                    uid: r1,
                    from: "reader".into(),
                    to: "app_reader".into(),
                }),
                PlannedChange::new(Change::CreateRole {
                    uid: r2,
                    name: "auditors".into(),
                }),
                PlannedChange::new(Change::DropRole {
                    uid: r3,
                    name: "reporting".into(),
                    members: vec!["a".into()],
                }),
            ],
        };
        let expect =
            |completed: usize| role_name_expectations(&cs, &pbps_mssql::Mssql, completed).unwrap();
        let s = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            expect(0),
            (s(&["app_reader", "auditors"]), s(&["reader", "reporting"])),
            "before statement one"
        );
        assert_eq!(
            expect(1),
            (s(&["auditors"]), s(&["reporting"])),
            "the rename is done: its new name is held, its old one is nobody's business"
        );
        assert_eq!(
            expect(2),
            (s(&[]), s(&["reporting"])),
            "the CREATE ROLE is done; the drop has not started"
        );
        assert_eq!(
            expect(3),
            (s(&[]), s(&["reporting"])),
            "one DROP MEMBER committed; the role still holds its name"
        );
        assert_eq!(expect(4), (s(&[]), s(&[])), "everything ran");
    }

    /// A created object enters the checkpoint's identities the moment its
    /// statement commits, under the plan's uid, columns and all; a name the
    /// plan does not know is left alone.
    #[test]
    fn a_created_object_is_adopted_into_the_live_identities_under_the_plans_uid() {
        use pbps_dialect::Created;
        let t_uid: pbps_model::Uid = "t_aaaaaa".parse().unwrap();
        let c_uid: pbps_model::Uid = "c_aaaaaa".parse().unwrap();
        let d_uid: pbps_model::Uid = "c_bbbbbb".parse().unwrap();
        let r_uid: pbps_model::Uid = "r_aaaaaa".parse().unwrap();
        let table: TableName = "app.audit".parse().unwrap();
        let mut plan_ids = IdsFile::default();
        plan_ids.tables.insert(t_uid.clone(), table.clone());
        plan_ids.columns.insert(
            c_uid.clone(),
            pbps_model::ColumnRef::new(table.clone(), "id"),
        );
        plan_ids.columns.insert(
            d_uid.clone(),
            pbps_model::ColumnRef::new("app.other".parse().unwrap(), "later"),
        );
        plan_ids.roles.insert(r_uid.clone(), "auditors".to_owned());

        let mut live = IdsFile::default();
        adopt_created(&mut live, &plan_ids, &[Created::Table(table.clone())]);
        assert_eq!(live.tables.get(&t_uid), Some(&table));
        assert!(live.columns.contains_key(&c_uid), "{live:?}");
        assert!(!live.columns.contains_key(&d_uid), "another table's column");
        assert!(live.roles.is_empty());

        adopt_created(
            &mut live,
            &plan_ids,
            &[
                Created::Column("app.other".parse().unwrap(), "later".into()),
                Created::Role("auditors".into()),
                Created::Role("nobody".into()),
                Created::Table("app.unknown".parse().unwrap()),
            ],
        );
        assert!(live.columns.contains_key(&d_uid));
        assert_eq!(live.roles.get(&r_uid).map(String::as_str), Some("auditors"));
        assert_eq!(live.roles.len(), 1);
        assert_eq!(live.tables.len(), 1);
    }

    /// Halfway through a rename that moves both schema and name, the table
    /// stands at neither end; the checkpoint's scope has to stand there too.
    #[test]
    fn a_checkpoint_reads_rows_under_the_name_the_catalog_has_now() {
        let uid: pbps_model::Uid = "t_aaaaaa".parse().unwrap();
        let mut final_ids = IdsFile::default();
        final_ids
            .tables
            .insert(uid.clone(), "app.customer".parse().unwrap());
        let mut live_ids = IdsFile::default();
        live_ids
            .tables
            .insert(uid, "app.old_customer".parse().unwrap());
        let scope = DataScope {
            mode: DataMode::Exact,
            keys: BTreeSet::new(),
        };
        let mut data = DataScopes::new();
        data.insert("app.customer".parse().unwrap(), scope.clone());
        // A table the plan creates later is not in the live mapping at all,
        // and keeps its final name: there is nothing to read yet either way.
        data.insert("app.later".parse().unwrap(), scope.clone());

        let now = scopes_under(&data, &final_ids, &live_ids);
        let names: Vec<String> = now.keys().map(ToString::to_string).collect();
        assert_eq!(names, ["app.later", "app.old_customer"]);
        assert_eq!(
            now[&"app.old_customer".parse::<TableName>().unwrap()],
            scope
        );
    }

    /// A schema target the database spells differently is refused; one it
    /// does not have at all is the probe's business, and one that agrees is
    /// nobody's.
    #[test]
    fn only_a_schema_the_database_spells_differently_is_reported() {
        let mut spelled = BTreeMap::new();
        spelled.insert("DBO".to_owned(), Some("dbo".to_owned()));
        spelled.insert("sales".to_owned(), Some("sales".to_owned()));
        spelled.insert("legacy".to_owned(), None);
        let declared_by = [("DBO".to_owned(), "granted to role `reporting`".to_owned())]
            .into_iter()
            .collect();

        let wrong = wrongly_spelt(&spelled, &declared_by);
        assert_eq!(wrong.len(), 1, "{wrong:?}");
        assert!(wrong[0].contains("`DBO`"), "{wrong:?}");
        assert!(
            wrong[0].contains("granted to role `reporting`"),
            "{wrong:?}"
        );
        assert!(wrong[0].contains("`dbo`"), "{wrong:?}");
    }

    fn pulled() -> Pulled {
        Pulled {
            schema: Schema::default(),
            warnings: Vec::new(),
            unexpressible: Vec::new(),
            limitations: vec![
                Limitation {
                    target: pbps_db::catalog::LimitationTarget::Relation(
                        "dbo.managed".parse().unwrap(),
                    ),
                    detail: "dbo.managed has a computed column".into(),
                },
                Limitation {
                    target: pbps_db::catalog::LimitationTarget::Relation(
                        "dbo.theirs".parse().unwrap(),
                    ),
                    detail: "dbo.theirs has a computed column".into(),
                },
            ],
            unmanaged_modules: vec![
                UnmanagedModule {
                    kind: "procedure",
                    name: "dbo.declared_secret".parse().unwrap(),
                    why: "its definition cannot be read back".into(),
                },
                UnmanagedModule {
                    kind: "procedure",
                    name: "dbo.stray_secret".parse().unwrap(),
                    why: "its definition cannot be read back".into(),
                },
            ],
        }
    }

    fn ids() -> IdsFile {
        let mut ids = IdsFile::default();
        ids.tables.insert(
            pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.managed", 0),
            "dbo.managed".parse().unwrap(),
        );
        ids
    }

    /// A module the managed set names and the catalog cannot read is a
    /// limitation of the projection, exactly as an unsupported feature on a
    /// managed table is. Reported as a warning instead, `baseline` recorded a
    /// schema without it, and the scope rebuilt from that schema then treated
    /// the same module as undeclared.
    #[test]
    fn a_module_in_the_managed_set_that_cannot_be_read_back_is_a_limitation() {
        let modules = std::iter::once("dbo.declared_secret".parse().unwrap()).collect();
        let found = managed_limitations(&pulled(), &ids(), &modules);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0], "dbo.managed has a computed column");
        assert!(
            found[1].contains("procedure dbo.declared_secret is in the managed set")
                && found[1].contains("cannot be read back"),
            "{found:?}"
        );
    }

    /// The other direction: an unreadable module outside the set is the
    /// unmanaged policy's business (see [`unmanaged_objects`]), not a
    /// limitation — and a limitation on somebody else's table is neither.
    #[test]
    fn objects_outside_the_managed_set_are_not_limitations() {
        let found = managed_limitations(&pulled(), &ids(), &Default::default());
        assert_eq!(found, ["dbo.managed has a computed column"]);

        let found = managed_limitations(&pulled(), &IdsFile::default(), &Default::default());
        assert!(found.is_empty(), "{found:?}");
    }

    /// The two inventories partition the unreadable modules: whichever set a
    /// module falls outside, one of them names it, so an encrypted module can
    /// never be silent in a command that consults both.
    #[test]
    fn every_unreadable_module_is_either_a_limitation_or_unmanaged() {
        let pulled = pulled();
        let modules = std::iter::once("dbo.declared_secret".parse().unwrap()).collect();
        let scoped = pbps_diff::scope(&pulled.schema, &IdsFile::default(), &modules);
        let unreadable = unreadable_modules(&pulled.unmanaged_modules);

        let limited = managed_limitations(&pulled, &IdsFile::default(), &modules);
        let unmanaged = unmanaged_objects(&scoped, &unreadable, &modules);
        assert!(limited.iter().any(|l| l.contains("dbo.declared_secret")));
        assert!(!limited.iter().any(|l| l.contains("dbo.stray_secret")));
        assert!(unmanaged.iter().any(|u| u.contains("dbo.stray_secret")));
        assert!(!unmanaged.iter().any(|u| u.contains("dbo.declared_secret")));
    }

    /// A column this plan adds or alters is held to what the plan gives it,
    /// field by field, once every statement has run. The shape comparison
    /// excludes exactly those fields, so nothing else says what became of them
    /// — and presence alone let a column another session retyped after the
    /// plan's own statement be recorded as the plan's result, while a created
    /// table's columns were already held to their declaration (DECISIONS 189).
    #[test]
    fn a_column_this_plan_writes_is_held_to_the_definition_it_gives() {
        let dbo_t: TableName = "dbo.t".parse().unwrap();
        let column = |ty: &str, nullable: bool, default: Option<&str>| {
            let mut c = pbps_model::Column::new(ty.parse().unwrap());
            c.nullable = nullable;
            c.default = default.map(str::to_owned);
            c
        };
        let schema = |columns: &[(&str, pbps_model::Column)]| {
            let mut t = pbps_model::Table::default();
            for (name, c) in columns {
                t.columns.insert((*name).to_owned(), c.clone());
            }
            let mut s = Schema::default();
            s.tables.insert(dbo_t.clone(), t);
            s
        };
        let plan = |change: pbps_model::Change| pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(change)],
        };
        let kept = column("int", false, None);
        let before = schema(&[("kept", kept.clone())]);

        // Adding: the read-back is the declaration, in the engine's spelling.
        let adding = plan(pbps_model::Change::AddColumn {
            uid: "c_aaaaaa".parse().unwrap(),
            table: dbo_t.clone(),
            name: "amount".to_owned(),
            column: Box::new(column("decimal", false, Some("0"))),
        });
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &adding,
            &before,
            &schema(&[
                ("kept", kept.clone()),
                ("amount", column("decimal(18,0)", false, Some("((0))"))),
            ]),
            "prod",
            Settled::Whole,
        )
        .expect("the engine's spelling of the declared column");
        for (what, got) in [
            ("type", column("decimal(18,2)", false, Some("((0))"))),
            ("nullability", column("decimal(18,0)", true, Some("((0))"))),
            ("default", column("decimal(18,0)", false, None)),
        ] {
            let e = refuse_unplanned_movement(
                &pbps_mssql::Mssql,
                &adding,
                &before,
                &schema(&[("kept", kept.clone()), ("amount", got)]),
                "prod",
                Settled::Whole,
            )
            .expect_err(what);
            assert!(format!("{e:#}").contains("`amount`"), "{what}: {e:#}");
        }

        // Altering: each change promises the field it moves, and only that.
        let retyping = plan(pbps_model::Change::AlterColumnType {
            uid: "c_aaaaaa".parse().unwrap(),
            column: dbo_t.column("kept"),
            from: "int".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_nullable: false,
            to_nullable: false,
        });
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &retyping,
            &before,
            &schema(&[("kept", column("bigint", false, None))]),
            "prod",
            Settled::Whole,
        )
        .expect("the type the plan gives it");
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &retyping,
            &before,
            &schema(&[("kept", column("int", false, None))]),
            "prod",
            Settled::Whole,
        )
        .expect_err("the ALTER was undone before the read");
        assert!(format!("{e:#}").contains("type"), "{e:#}");
        // `ALTER COLUMN` restates the nullability with the type, so a retype
        // promises it too.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &retyping,
            &before,
            &schema(&[("kept", column("bigint", true, None))]),
            "prod",
            Settled::Whole,
        )
        .expect_err("the nullability the ALTER restated");
        assert!(format!("{e:#}").contains("nullability"), "{e:#}");

        let loosening = plan(pbps_model::Change::AlterColumnNullability {
            uid: "c_aaaaaa".parse().unwrap(),
            column: dbo_t.column("kept"),
            ty: "int".parse().unwrap(),
            to_nullable: true,
        });
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &loosening,
            &before,
            &before,
            "prod",
            Settled::Whole,
        )
        .expect_err("still NOT NULL");
        assert!(format!("{e:#}").contains("nullability"), "{e:#}");

        let defaulting = plan(pbps_model::Change::AlterColumnDefault {
            uid: "c_aaaaaa".parse().unwrap(),
            column: dbo_t.column("kept"),
            from: None,
            to: Some("0".to_owned()),
        });
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &defaulting,
            &before,
            &schema(&[("kept", column("int", false, Some("((0))")))]),
            "prod",
            Settled::Whole,
        )
        .expect("the text is the engine's; that there is one is the promise");
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &defaulting,
            &before,
            &before,
            "prod",
            Settled::Whole,
        )
        .expect_err("no default arrived");
        assert!(format!("{e:#}").contains("default"), "{e:#}");

        // Not at a checkpoint: the statement may not have run yet.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &retyping,
            &before,
            &before,
            "prod",
            Settled::SoFar,
        )
        .expect("mid-run, the old type is not a failure");
    }

    /// A column is excused from the shape comparison only on the read where
    /// it is on one side alone. Across the read that spans its rename it is
    /// followed from the old name to the new; on every other read of a
    /// staged run it is two read-backs like any other column. Excusing the
    /// name outright left a renamed or added column exempt for the rest of
    /// the run (DECISIONS 189).
    #[test]
    fn a_column_is_followed_across_its_rename_and_compared_after_it() {
        let dbo_t: TableName = "dbo.t".parse().unwrap();
        let schema = |name: &str, ty: &str, nullable: bool| {
            let mut c = pbps_model::Column::new(ty.parse().unwrap());
            c.nullable = nullable;
            let mut t = pbps_model::Table::default();
            t.columns.insert(name.to_owned(), c);
            let mut s = Schema::default();
            s.tables.insert(dbo_t.clone(), t);
            s
        };
        let renaming = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::RenameColumn {
                    uid: "c_aaaaaa".parse().unwrap(),
                    table: dbo_t.clone(),
                    from: "a".to_owned(),
                    to: "b".to_owned(),
                },
            )],
        };
        // The read spanning the rename: one column, two names.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renaming,
            &schema("a", "int", false),
            &schema("b", "int", false),
            "prod",
            Settled::SoFar,
        )
        .expect("renamed, otherwise as it was");
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renaming,
            &schema("a", "int", false),
            &schema("b", "bigint", false),
            "prod",
            Settled::SoFar,
        )
        .expect_err("renamed and retyped, and the plan only renames");
        assert!(format!("{e:#}").contains("`b`"), "{e:#}");
        // A later read of the same staged run: both sides know it as `b`.
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renaming,
            &schema("b", "int", false),
            &schema("b", "int", true),
            "prod",
            Settled::SoFar,
        )
        .expect_err("loosened after the rename, by nobody in this plan");
        assert!(format!("{e:#}").contains("nullability"), "{e:#}");
        // And before it ran: both sides know it as `a`, and it has not moved.
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renaming,
            &schema("a", "int", false),
            &schema("a", "int", false),
            "prod",
            Settled::SoFar,
        )
        .expect("not renamed yet");

        // The same for a column the plan adds: on both sides of a later read,
        // it is compared like any other.
        let adding = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::AddColumn {
                    uid: "c_aaaaaa".parse().unwrap(),
                    table: dbo_t.clone(),
                    name: "b".to_owned(),
                    column: Box::new(pbps_model::Column::new("int".parse().unwrap())),
                },
            )],
        };
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &adding,
            &schema("b", "int", true),
            &schema("b", "bigint", true),
            "prod",
            Settled::SoFar,
        )
        .expect_err("added at an earlier checkpoint, retyped since");
        assert!(format!("{e:#}").contains("type"), "{e:#}");
        // A rename the plan pairs with a retype excuses the type across the
        // rename, and nothing else.
        let renaming_and_retyping = pbps_model::ChangeSet {
            changes: vec![
                pbps_model::PlannedChange::new(pbps_model::Change::RenameColumn {
                    uid: "c_aaaaaa".parse().unwrap(),
                    table: dbo_t.clone(),
                    from: "a".to_owned(),
                    to: "b".to_owned(),
                }),
                pbps_model::PlannedChange::new(pbps_model::Change::AlterColumnType {
                    uid: "c_aaaaaa".parse().unwrap(),
                    column: dbo_t.column("b"),
                    from: "int".parse().unwrap(),
                    to: "bigint".parse().unwrap(),
                    from_nullable: false,
                    to_nullable: false,
                }),
            ],
        };
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renaming_and_retyping,
            &schema("a", "int", false),
            &schema("b", "bigint", false),
            "prod",
            Settled::Whole,
        )
        .expect("renamed and retyped, as planned");
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &renaming_and_retyping,
            &schema("a", "int", false),
            &schema("b", "bigint", true),
            "prod",
            Settled::Whole,
        )
        .expect_err("the nullability is nobody's plan");
        assert!(format!("{e:#}").contains("nullability"), "{e:#}");
    }

    /// A part this plan adds is held to the definition it adds it with, not
    /// to being there: anyone who redefines a constraint drops and recreates
    /// it under the same name, so a read that finds the name says nothing
    /// about which definition is behind it (DECISIONS 189).
    #[test]
    fn a_part_this_plan_adds_is_held_to_its_definition() {
        let dbo_t: TableName = "dbo.t".parse().unwrap();
        let dbo_p: TableName = "dbo.p".parse().unwrap();
        let index = |on: &str, unique: bool| pbps_model::Index {
            columns: vec![pbps_model::IndexColumn {
                name: on.to_owned(),
                descending: false,
            }],
            include: Vec::new(),
            unique,
            filter: None,
        };
        let unique = |on: &str| pbps_model::UniqueConstraint {
            columns: vec![on.to_owned()],
        };
        let key = |on: &str| pbps_model::PrimaryKey {
            name: None,
            columns: vec![on.to_owned()],
        };
        let fk =
            |to: &TableName, on_delete: pbps_model::ReferentialAction| pbps_model::ForeignKey {
                columns: vec!["p_id".to_owned()],
                references_table: to.clone(),
                references_columns: vec!["id".to_owned()],
                on_delete,
                on_update: pbps_model::ReferentialAction::NoAction,
            };
        let with = |f: &dyn Fn(&mut pbps_model::Table)| {
            let mut t = pbps_model::Table::default();
            for c in ["id", "other", "p_id"] {
                t.columns.insert(
                    c.to_owned(),
                    pbps_model::Column::new("int".parse().unwrap()),
                );
            }
            f(&mut t);
            let mut s = Schema::default();
            s.tables.insert(dbo_t.clone(), t);
            s.tables.insert(dbo_p.clone(), pbps_model::Table::default());
            s
        };
        let bare = with(&|_| {});
        let plan = |change: pbps_model::Change| pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(change)],
        };
        let check = |plan: &pbps_model::ChangeSet, after: &Schema| {
            refuse_unplanned_movement(
                &pbps_mssql::Mssql,
                plan,
                &bare,
                after,
                "prod",
                Settled::Whole,
            )
        };
        let refused = |plan: &pbps_model::ChangeSet, after: &Schema, why: &str| {
            let e = check(plan, after).expect_err(why);
            let e = format!("{e:#}");
            assert!(e.contains("is not the one this plan adds"), "{why}: {e}");
        };

        let adding_index = plan(pbps_model::Change::AddIndex {
            table: dbo_t.clone(),
            name: "ix".to_owned(),
            index: Box::new(index("id", false)),
        });
        check(
            &adding_index,
            &with(&|t| {
                t.indexes.insert("ix".to_owned(), index("id", false));
            }),
        )
        .expect("the index as planned");
        refused(
            &adding_index,
            &with(&|t| {
                t.indexes.insert("ix".to_owned(), index("other", false));
            }),
            "an index of that name on another column",
        );
        refused(
            &adding_index,
            &with(&|t| {
                t.indexes.insert("ix".to_owned(), index("id", true));
            }),
            "an index of that name, made unique by somebody",
        );
        // Absent is a different finding from wrong, and keeps its own words.
        let e = check(&adding_index, &bare).expect_err("not there at all");
        assert!(format!("{e:#}").contains("is not there"), "{e:#}");

        let adding_unique = plan(pbps_model::Change::AddUnique {
            table: dbo_t.clone(),
            name: "uq".to_owned(),
            constraint: unique("id"),
        });
        refused(
            &adding_unique,
            &with(&|t| {
                t.unique.insert("uq".to_owned(), unique("other"));
            }),
            "a unique of that name over another column",
        );

        let keying = plan(pbps_model::Change::SetPrimaryKey {
            table: dbo_t.clone(),
            from: None,
            to: Some(key("id")),
        });
        check(
            &keying,
            &with(&|t| {
                t.primary_key = Some(pbps_model::PrimaryKey {
                    name: Some("PK__t__3213E83F".to_owned()),
                    columns: vec!["id".to_owned()],
                });
            }),
        )
        .expect("the engine names an unnamed key; that is not movement");
        refused(
            &keying,
            &with(&|t| {
                t.primary_key = Some(key("other"));
            }),
            "a key over another column",
        );

        let adding_fk = plan(pbps_model::Change::AddForeignKey {
            table: dbo_t.clone(),
            name: "fk".to_owned(),
            constraint: Box::new(fk(&dbo_p, pbps_model::ReferentialAction::Cascade)),
        });
        check(
            &adding_fk,
            &with(&|t| {
                t.foreign_keys.insert(
                    "fk".to_owned(),
                    fk(&dbo_p, pbps_model::ReferentialAction::Cascade),
                );
            }),
        )
        .expect("the foreign key as planned");
        refused(
            &adding_fk,
            &with(&|t| {
                t.foreign_keys.insert(
                    "fk".to_owned(),
                    fk(&dbo_p, pbps_model::ReferentialAction::NoAction),
                );
            }),
            "the same key without its cascade",
        );

        // A check is nothing but an expression, and the engine rewrites it:
        // its name is all a read-back can be held to (183).
        let adding_check = plan(pbps_model::Change::AddCheck {
            table: dbo_t.clone(),
            name: "ck".to_owned(),
            constraint: pbps_model::CheckConstraint {
                expression: "id > 0".to_owned(),
            },
        });
        check(
            &adding_check,
            &with(&|t| {
                t.checks.insert(
                    "ck".to_owned(),
                    pbps_model::CheckConstraint {
                        expression: "([id]>(0))".to_owned(),
                    },
                );
            }),
        )
        .expect("the engine's rendering of the check");
    }

    /// The remedy a staged refusal names depends on which read found the
    /// change. At a checkpoint the change is in the record and a resume goes
    /// on; at the closing read nothing records it and a resume refuses the
    /// database as moved — so telling the operator to resume there sent them
    /// down a path that cannot work (DECISIONS 190).
    #[test]
    fn a_closing_staged_refusal_does_not_promise_a_resume() {
        let schema = |rows: &[&str]| {
            let t = pbps_model::Table {
                data: Some(pbps_model::TableData {
                    mode: pbps_model::DataMode::Exact,
                    rows: rows
                        .iter()
                        .map(|k| (pbps_model::RowKey::from(*k), pbps_model::Row::default()))
                        .collect(),
                }),
                ..Default::default()
            };
            let mut s = Schema::default();
            s.tables.insert("dbo.other".parse().unwrap(), t);
            s
        };
        let untouched = pbps_model::ChangeSet {
            changes: Vec::new(),
        };
        let refusal_for = |environment: Option<&str>, read: StagedRead| {
            let e = staged_movement(
                &pbps_mssql::Mssql,
                &untouched,
                &schema(&["kept"]),
                &schema(&["kept", "rogue"]),
                "prod",
                environment,
                read,
            )
            .expect_err("a row nobody planned");
            format!("{e:#}")
        };
        let refusal = |read: StagedRead| refusal_for(Some("prod"), read);
        let at_checkpoint = refusal(StagedRead::Checkpoint {
            completed: 1,
            total: 2,
        });
        assert!(
            at_checkpoint.contains("resuming accepts it"),
            "{at_checkpoint}"
        );
        assert!(at_checkpoint.contains("1 of 2"), "{at_checkpoint}");
        // The checkpoint already recorded the movement, so `verify` compares
        // against a baseline that holds it and reads clean: the refusal must
        // not send the operator there to see it. Its own list is the record,
        // and `status` shows it again as the failed entry's reason.
        assert!(
            !at_checkpoint.contains("`pbps verify` shows"),
            "{at_checkpoint}"
        );
        assert!(at_checkpoint.contains("`pbps status`"), "{at_checkpoint}");
        let at_close = refusal(StagedRead::Closing { total: 2 });
        assert!(!at_close.contains("resuming accepts"), "{at_close}");
        assert!(at_close.contains("no checkpoint holds it"), "{at_close}");
        assert!(at_close.contains("`--resume` will refuse"), "{at_close}");
        // No checkpoint holds a change that landed after the last one, so at
        // the closing read `verify` does show it, and the pointer stays.
        assert!(at_close.contains("`pbps verify` shows"), "{at_close}");
        // Both say what moved and that nothing was undone — and neither
        // carries the transactional remedy, which used to sit one line above
        // "nothing was rolled back" in every staged refusal.
        for e in [&at_checkpoint, &at_close] {
            assert!(e.contains("dbo.other"), "{e}");
            assert!(e.contains("nothing was rolled back"), "{e}");
            assert!(!e.contains("transaction was rolled back"), "{e}");
            assert!(!e.contains("then apply again"), "{e}");
        }
    }

    /// A `--db` run is told where the record is, and sent to no command that
    /// cannot take one.
    ///
    /// `status` reports on the environments `pbps.yml` configures and accepts
    /// no target at all, so for a target given as a connection string the
    /// pointer added with DECISIONS 192's sibling led nowhere: to other
    /// databases, or to "no environments are configured". The sentence has to
    /// stand without it, because for half the callers there is nothing to name
    /// (DECISIONS 196).
    #[test]
    fn a_checkpoint_refusal_names_status_only_to_a_caller_who_can_run_it() {
        let schema = |rows: &[&str]| {
            let t = pbps_model::Table {
                data: Some(pbps_model::TableData {
                    mode: pbps_model::DataMode::Exact,
                    rows: rows
                        .iter()
                        .map(|k| (pbps_model::RowKey::from(*k), pbps_model::Row::default()))
                        .collect(),
                }),
                ..Default::default()
            };
            let mut s = Schema::default();
            s.tables.insert("dbo.other".parse().unwrap(), t);
            s
        };
        let untouched = pbps_model::ChangeSet {
            changes: Vec::new(),
        };
        let refusal = |environment: Option<&str>| {
            let e = staged_movement(
                &pbps_mssql::Mssql,
                &untouched,
                &schema(&["kept"]),
                &schema(&["kept", "rogue"]),
                "localhost/app",
                environment,
                StagedRead::Checkpoint {
                    completed: 1,
                    total: 2,
                },
            )
            .expect_err("a row nobody planned");
            format!("{e:#}")
        };

        let from_db = refusal(None);
        assert!(!from_db.contains("pbps status"), "{from_db}");
        // What is left has to be an answer on its own: where the record is, and
        // that the command which *can* be run against this target reads clean.
        assert!(from_db.contains("the record of what moved"), "{from_db}");
        assert!(
            from_db.contains("recorded as the failed entry's reason"),
            "{from_db}"
        );
        assert!(from_db.contains("`pbps verify` reads clean"), "{from_db}");
        assert!(from_db.contains("dbo.other"), "{from_db}");

        // `--env`: the environment is one `status` walks, so it is named.
        let from_env = refusal(Some("prod"));
        assert!(from_env.contains("`pbps status` shows"), "{from_env}");
    }

    /// A cell the plan leaves to its default is held there at the closing
    /// read. The read-back omits a cell the engine confirmed at its default,
    /// so a present cell on such a column is one somebody else wrote — and
    /// dropping the expectation because the value could not be named meant a
    /// rewrite between a staged `UPDATE` and its checkpoint was recorded as
    /// the plan's result. Only at the closing read, and only on a column
    /// whose default the engine confirms: a `NEWID()` cell is present on
    /// every read, and a checkpoint read spells cells after the checkpoint
    /// before it (DECISIONS 191).
    #[test]
    fn a_cell_left_to_its_default_is_held_there_at_the_close() {
        use pbps_model::{Cell, DataMode, RowKey, TableData, Value};
        let dbo_t: TableName = "dbo.t".parse().unwrap();
        let schema = |cells: &[(&str, &str)]| {
            let column = |ty: &str, default: Option<&str>| {
                let mut c = pbps_model::Column::new(ty.parse().unwrap());
                c.default = default.map(str::to_owned);
                c
            };
            let mut t = pbps_model::Table::default();
            t.columns
                .insert("code".to_owned(), column("varchar(20)", None));
            t.columns
                .insert("note".to_owned(), column("nvarchar(50)", Some("('n/a')")));
            t.columns.insert(
                "tag".to_owned(),
                column("uniqueidentifier", Some("(newid())")),
            );
            t.data = Some(TableData {
                mode: DataMode::Exact,
                rows: [(
                    RowKey::from("k"),
                    cells
                        .iter()
                        .map(|(c, v)| ((*c).to_owned(), Value::Text((*v).to_owned())))
                        .collect::<pbps_model::Row>(),
                )]
                .into_iter()
                .collect(),
            });
            let mut s = Schema::default();
            s.tables.insert(dbo_t.clone(), t);
            s
        };
        let plan = |change: pbps_model::Change| pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(change)],
        };
        // The declaration stops spelling `note`, so the plan sets it to
        // DEFAULT; `tag` was always left to its `NEWID()`.
        let defaulting = plan(pbps_model::Change::UpdateRow {
            table: dbo_t.clone(),
            key_column: "code".to_owned(),
            key: RowKey::from("k"),
            columns: [(
                "note".to_owned(),
                (
                    Cell::Value(Value::Text("custom".to_owned())),
                    Cell::Default("'n/a'".to_owned()),
                ),
            )]
            .into_iter()
            .collect(),
            unchanged: [("tag".to_owned(), Cell::Default("NEWID()".to_owned()))]
                .into_iter()
                .collect(),
            types: Default::default(),
            after_types: Default::default(),
        });
        let before = schema(&[("note", "custom")]);
        let check = |after: &Schema, settled: Settled| {
            refuse_unplanned_movement(
                &pbps_mssql::Mssql,
                &defaulting,
                &before,
                after,
                "prod",
                settled,
            )
        };
        // At the close, the cell is omitted: at its default, as planned. The
        // `NEWID()` cell is there with its value, and that proves nothing.
        check(
            &schema(&[("tag", "6F9619FF-8B86-D011-B42D-00C04FC964FF")]),
            Settled::Closing,
        )
        .expect("at its default, and an unconfirmable default beside it");
        // Present on a column the engine confirms: somebody wrote it.
        let e = check(&schema(&[("note", "rogue")]), Settled::Closing)
            .expect_err("a value where the plan left the default");
        assert!(format!("{e:#}").contains("`note`"), "{e:#}");
        assert!(
            format!("{e:#}").contains("leaves it at its default"),
            "{e:#}"
        );
        // The same read-back at the final checkpoint says nothing: that read
        // spells a cell the way the checkpoint before it did, so a cell at
        // its default may well be present there.
        check(&schema(&[("note", "rogue")]), Settled::Whole)
            .expect("a checkpoint read cannot tell at-default from a value");

        // An insert leaves columns to their defaults too.
        let inserting = plan(pbps_model::Change::InsertRow {
            table: dbo_t.clone(),
            key_column: "code".to_owned(),
            identity_key: false,
            key: RowKey::from("k"),
            row: Default::default(),
            defaults: [("note".to_owned(), "'n/a'".to_owned())]
                .into_iter()
                .collect(),
            types: Default::default(),
        });
        let empty = Schema::default();
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &inserting,
            &empty,
            &schema(&[("note", "rogue")]),
            "prod",
            Settled::Closing,
        )
        .expect_err("inserted at its default, read back with a value");
        assert!(format!("{e:#}").contains("`note`"), "{e:#}");
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &inserting,
            &empty,
            &schema(&[]),
            "prod",
            Settled::Closing,
        )
        .expect("inserted at its default, read back omitted");
    }
}
