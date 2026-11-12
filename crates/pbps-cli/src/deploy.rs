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

mod apply;
mod apply_staged;
mod bootstrap;
mod checkout;
mod execution;
mod extension_pins;
mod guard;
mod ledger;
mod limitations;
mod managed;
mod names;
mod pinned_movement;
mod plan_db;
mod scopes;
mod services;
mod spelling;
pub(crate) mod test_run;
mod walk;

pub(crate) use apply::*;
use apply_staged::*;
use bootstrap::*;
use execution::*;
pub(crate) use extension_pins::*;
pub(crate) use ledger::*;
pub(crate) use limitations::*;
pub(crate) use managed::*;
pub(crate) use names::*;
use pinned_movement::*;
pub(crate) use plan_db::*;
use scopes::*;
pub(crate) use spelling::*;
pub(crate) use walk::*;

use services::{Attempt, apply_identified};
pub(crate) use services::{cmd_bootstrap, cmd_plan_db};

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
pub(crate) fn refuse_occupied_names(
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

#[cfg(test)]
mod checkpoint_edges_tests;
