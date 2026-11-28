//! `pbps diff`: the declarations against a live database, with no ledger
//! (#3028, DEC-3028.1).
//!
//! `verify` asks whether a database moved since pbps last recorded it, and
//! `plan --db` what would make it match; both need the ledger. This asks the
//! one question that needs no record at all: does this database hold what the
//! declarations say. It reads the catalog, writes nothing, creates no ledger,
//! and has no SQL to show.
//!
//! Three things follow from having no record:
//!
//! - **Names are the only pairing.** A rename is a human's answer that `plan`
//!   asks for and records. With no record, a renamed object is a drop and a
//!   create, and the report says that this is all it could tell.
//! - **A stored text cannot be told from a respelling.** The engine respells
//!   every expression and module body it stores, and what was declared when an
//!   object was last written is in the ledger. So the texts are compared
//!   apart from the structure, and listed with both sides.
//! - **Roles, grants, row security and declared rows are not compared.** They
//!   are the parts whose reading depends on what pbps wrote earlier.

use super::*;
use crate::output::{self, Finding};
use pbps_model::escape::Escaped;
use pbps_model::{
    Change, ChangeSet, ColumnChange, Declared, GrantChange, ManagedType, TableChange,
};

const COMMAND: &str = "diff";

/// What `--format json` carries under `data`.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct LiveDiffData {
    environment: String,
    /// What would make the database hold the declarations, in a plan's
    /// direction. Structure only: a text is equal here wherever its object
    /// exists on both sides.
    #[schemars(with = "serde_json::Value")]
    changes: ChangeSet,
    /// The texts the database stores under another spelling than the
    /// declarations, or with another meaning; this command cannot tell which.
    text_differences: Vec<TextDifference>,
    /// Differences that are established and that no change can carry.
    unexpressible: Vec<String>,
    /// What the read left out of a managed object. Never a difference: the
    /// rest of the object was compared without it.
    not_compared: Vec<String>,
    /// What the database holds outside the declarations and the identity file.
    unmanaged: Vec<String>,
}

/// One text the declarations and the database spell differently.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct TextDifference {
    /// What the text belongs to, as the report prints it.
    object: String,
    declared: String,
    stored: String,
}

/// A failure with the finding it is reported under, chosen where it happened
/// and never inferred from the error afterwards (DEC-1804.1).
struct Failure {
    id: &'static str,
    error: anyhow::Error,
}

impl Failure {
    fn at(id: &'static str, error: impl Into<anyhow::Error>) -> Self {
        Self {
            id,
            error: error.into(),
        }
    }
}

/// The comparison of one read with the declarations. Pure.
struct Compared {
    changes: ChangeSet,
    text_differences: Vec<TextDifference>,
    unexpressible: Vec<String>,
}

pub fn cmd_live_diff(project: &Project, target: &Target, json: bool) -> anyhow::Result<()> {
    let catalog = std::cell::RefCell::new(Vec::new());
    let dialect = output::or_unanswerable(
        COMMAND,
        json,
        "project.unsupported-dialect",
        crate::dialect(project),
    )?;
    // Before any connection: declarations that do not load have no answer to
    // compare a database with, and a connection made first would report an
    // unreachable host in place of the file that is wrong.
    let loaded = declarations(project, dialect.as_ref(), json)?;
    let project_ids = output::or_unanswerable_at(
        COMMAND,
        json,
        "identity.unreadable",
        project.ids_dir(),
        crate::read_ids(project),
    )?;
    let scope = scope_ids(&loaded.schema, &project_ids);
    let modules = managed_modules(None, Some(&loaded.schema));

    let rt = output::or_unanswerable(COMMAND, json, "runtime.unavailable", db::runtime())?;
    let (managed, unmanaged, refusal) = match rt.block_on(async {
        let mut conn = db::connect(target)
            .await
            .map_err(|error| Failure::at("environment.unreachable", error))?;
        let mut managed = managed_state_full(
            &mut conn,
            &scope,
            &modules,
            &pbps_diff::external_parents(&loaded.schema),
            // Applied below, beside the finished comparison, so that neither
            // verdict can erase the other.
            target.catalog_policy(pbps_config::Unmanaged::Ignore),
            // No rows: declared rows are not compared.
            &BTreeMap::new(),
            crate::engine::Read::Snapshot,
            CatalogWarnings::for_output(json, &catalog),
            // No attach rehearsal asks this read about any relation.
            &BTreeSet::new(),
        )
        .await
        .map_err(|error| Failure::at("environment.unreadable", error))?;
        // Roles are outside this comparison, so a role is neither managed nor
        // unmanaged here: listing every role of the server as undeclared would
        // refuse, under `unmanaged: error`, a project that declares them all.
        managed.scoped.unmanaged_roles.clear();
        let unmanaged = unmanaged_objects(&managed.scoped, &managed.unreadable, &scope, &modules);
        let refusal = match apply_unmanaged_policy(&unmanaged, project.config.unmanaged) {
            Ok(()) => None,
            Err(error) => match error.downcast::<UnmanagedPolicy>() {
                Ok(policy) => Some(policy),
                Err(error) => return Err(Failure::at("diff.failed", error)),
            },
        };
        Ok((managed, unmanaged, refusal))
    }) {
        Ok(read) => read,
        Err(Failure { id, error }) => {
            // Unanswerable: the question was what this database holds, and
            // it could not be read (SPEC §9.8).
            if json {
                let mut findings = vec![Finding::error(
                    id,
                    format!("{}: {error:#}", Escaped(&target.label)),
                )];
                findings.extend(catalog.take());
                output::unanswerable(COMMAND, findings);
            }
            return Err(error);
        }
    };

    let declared = without_the_omitted(&loaded.schema, &managed.scoped.schema, &managed.omitted);
    let mut compared = compare(
        &declared,
        &loaded.hints,
        &scope,
        &managed.scoped.schema,
        managed.database_collation.as_deref(),
        dialect.as_ref(),
    );
    compared
        .unexpressible
        .extend(managed.scoped.unexpressible.iter().cloned());
    let differences = compared.changes.changes.len()
        + compared.text_differences.len()
        + compared.unexpressible.len();

    let mut findings = catalog.into_inner();
    if differences > 0 {
        findings.push(
            Finding::error(
                "diff.differs",
                format!(
                    "`{}` differs from the declarations ({differences} difference(s))",
                    Escaped(&target.label)
                ),
            )
            .remedy(
                "pbps plan --db for a reviewed plan that makes the database match, or pbps pull \
                 to take what the database holds",
            ),
        );
    }
    for what in &compared.unexpressible {
        findings.push(Finding::error(
            "change.unexpressible",
            format!("{}: {what}", Escaped(&target.label)),
        ));
    }
    if !compared.text_differences.is_empty() {
        findings.push(Finding::note("diff.text-differences", TEXT_NOTE));
    }
    if may_hold_a_rename(&compared.changes) {
        findings.push(Finding::note("diff.names-only", NAMES_NOTE));
    }
    // After the differences, and never one of them: the object was compared
    // without the part the read left out, and a database that matches in
    // everything else matches. Said every time, so that "no differences"
    // cannot stand for a part nobody looked at.
    for what in &managed.limitations {
        findings.push(Finding::warning(
            "diff.not-compared",
            format!("{}: {what}", Escaped(&target.label)),
        ));
    }
    if let Some(ref refusal) = refusal {
        findings.push(
            Finding::error(
                "diff.unmanaged-refused",
                format!("{}: {refusal}", Escaped(&target.label)),
            )
            .remedy("pbps pull, or relax `unmanaged:` in pbps.yml"),
        );
    }

    if json {
        // Machine output names each unmanaged object. A person has them from
        // the policy's own line, where `unmanaged:` asks for one.
        findings.extend(unmanaged.iter().map(|object| {
            Finding::note(
                "diff.unmanaged",
                format!(
                    "{object} is in the database and outside the managed set; it was not compared"
                ),
            )
        }));
        let data = LiveDiffData {
            environment: target.label.clone(),
            changes: compared.changes,
            text_differences: compared.text_differences,
            unexpressible: compared.unexpressible,
            not_compared: managed.limitations,
            unmanaged,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&output::Report::new(COMMAND, findings, Some(&data)))?
        );
    } else {
        println!("Environment: {}", Escaped(&target.label));
        if differences == 0 {
            println!("No differences.");
        }
        if !compared.changes.is_empty() {
            print!("{}", crate::report::changes(&compared.changes));
        }
        print!("{}", text_report(&compared.text_differences));
        eprint!("{}", output::human(&findings));
    }

    if differences > 0 || refusal.is_some() {
        // The command answered, and the answer is that something is there:
        // exit 2, not the exit 1 of a database it could not read (SPEC §9.8).
        return Err(crate::Found::reported().into());
    }
    Ok(())
}

const TEXT_NOTE: &str = "a text difference is a text the database stores in another form than the \
     declarations write it. The engine respells what it stores, and without a recorded state this \
     command cannot tell a respelling from a change. `pbps pull` takes the database's spelling; \
     `pbps plan --db` compares against what was declared when the object was last written";

const NAMES_NOTE: &str = "objects are paired by name alone: a renamed table or column reads as a \
     drop and a create. `pbps plan` asks about renames and records the answer";

/// The declarations, checked as far as they can be without a database.
///
/// One path for both formats: the loader's diagnostics, the names the dialect
/// spells alike, then every problem `validate` would report that a statement
/// would have met. The same order `plan` takes them in.
fn declarations(
    project: &Project,
    dialect: &dyn Dialect,
    json: bool,
) -> anyhow::Result<pbps_load::Loaded> {
    let mut loaded = match crate::load_quiet(project) {
        Ok(loaded) => loaded,
        Err(errors) => {
            let count = errors.len();
            if json {
                output::unanswerable(COMMAND, errors.iter().map(crate::load_finding).collect());
            } else {
                for error in errors {
                    crate::print_load_error(error);
                }
            }
            bail!("the declarations have {count} problem(s)");
        }
    };
    let mut collisions = crate::routine_ids_as_the_dialect_spells_them(
        &mut loaded.schema,
        &mut loaded.hints,
        dialect,
    );
    collisions.extend(pbps_dialect::check_fixed_sequence_names(
        &loaded.schema,
        dialect,
    ));
    if !collisions.is_empty() {
        if json {
            output::unanswerable(
                COMMAND,
                collisions
                    .iter()
                    .map(|collision| Finding::error("schema.name-collision", collision.clone()))
                    .collect(),
            );
        } else {
            for collision in &collisions {
                eprintln!("error: {collision}");
            }
        }
        bail!("the declarations have {} problem(s)", collisions.len());
    }
    let problems = crate::declaration_problems(&loaded, dialect);
    if !problems.is_empty() {
        let count = problems.len();
        let listed = problems
            .iter()
            .map(|(_, problem)| problem.as_str())
            .collect::<Vec<_>>()
            .join("\n  ");
        if json {
            output::unanswerable(
                COMMAND,
                problems
                    .into_iter()
                    .map(|(id, problem)| Finding::error(id, problem))
                    .collect(),
            );
        }
        bail!(
            "the declarations have {count} problem(s) that would reach the database:\n  {listed}\n\
             `pbps validate` reports these with their source lines."
        );
    }
    Ok(loaded)
}

/// Every name this command answers for: what the declarations name, and what
/// the identity file still names.
///
/// The second half is what makes a table that left the declarations a drop,
/// as it is for `plan`, where the name alone would make it somebody else's
/// table. No role: roles are not compared.
fn scope_ids(declared: &Schema, project: &IdsFile) -> IdsFile {
    let mut scope = pbps_diff::observed_ids(declared, project);
    let mut taken = scope.held_uids().cloned().collect();
    scope.fill_names_from(project, &mut taken);
    scope.roles.clear();
    scope
}

/// The parts no side is compared in (see the module's note).
fn without_the_parts_not_compared(schema: &mut Schema) {
    schema.roles.clear();
    for table in schema.tables.values_mut() {
        table.row_security = Default::default();
        table.row_policies.clear();
        table.data = None;
    }
}

/// The declarations without what the read left out.
///
/// An object the read omitted (an encrypted procedure, a PostgreSQL table with
/// a rewrite rule) stands in the database but is absent from the schema read
/// back, so compared as read it would be a create. The read already says it
/// was left out (`diff.not-compared`); here it is no difference either way, as
/// `plan --db` refuses to plan around it (ADR-0002). Only an object absent
/// from the read is set aside: a target can name a part of an object the read
/// still holds, and the rest of that object is compared.
fn without_the_omitted(
    declared: &Schema,
    live: &Schema,
    omitted: &[pbps_db::catalog::LimitationTarget],
) -> Schema {
    use pbps_db::catalog::LimitationTarget;
    let mut declared = declared.clone();
    declared.modules.retain(|id, _| {
        live.modules.contains_key(id) || !omitted.iter().any(|target| target.matches_module(id))
    });
    declared.tables.retain(|name, _| {
        live.tables.contains_key(name)
            || !omitted
                .iter()
                .any(|target| matches!(target, LimitationTarget::Relation(n) if n == name))
    });
    declared.types.retain(|name, _| {
        live.types.contains_key(name)
            || !omitted.iter().any(|target| {
                matches!(target, LimitationTarget::Type(n)
                    if n.schema == name.schema() && n.name == name.name())
            })
    });
    declared.search.retain(|name, _| {
        live.search.contains_key(name)
            || !omitted
                .iter()
                .any(|target| matches!(target, LimitationTarget::Search(n) if n == name))
    });
    declared
}

/// Puts back a derived domain's DEFAULT that the overlay took for a copy.
///
/// The overlay reads an omitted DEFAULT on a derived domain as the copy the
/// engine made of its base's at creation (DEC-2537.1). That holds for a domain
/// pbps created and recorded. Here nothing is recorded, so only a default
/// equal to the base domain's own is taken for that copy; any other one is the
/// database's, and is compared.
fn keep_the_defaults_no_base_explains(overlaid: &mut Schema, live: &Schema) {
    let base_default =
        |base: &pbps_model::ColumnType| match base.type_ref().and_then(|name| live.types.get(name))
        {
            Some(pbps_model::ManagedType::Domain { default, .. }) => default.as_ref(),
            _ => None,
        };
    for (name, definition) in &mut overlaid.types {
        if let (
            pbps_model::ManagedType::Domain {
                base,
                default: default @ None,
                ..
            },
            Some(pbps_model::ManagedType::Domain {
                default: Some(stored),
                ..
            }),
        ) = (definition, live.types.get(name))
            && base_default(base) != Some(stored)
        {
            *default = Some(stored.clone());
        }
    }
}

fn compare(
    declared: &Schema,
    hints: &pbps_model::Hints,
    scope: &IdsFile,
    live: &Schema,
    database_collation: Option<&str>,
    dialect: &dyn Dialect,
) -> Compared {
    let mut declared = declarations_as_compared(declared, database_collation);
    let mut live = live.clone();
    without_the_parts_not_compared(&mut declared);
    without_the_parts_not_compared(&mut live);
    // The cut keeps every text search object of the database, since a plan
    // looks each one up by the recorded identity. Here an identity is made for
    // whatever is on the live side, so one nobody declared would be a drop.
    live.search
        .retain(|name, _| scope.search_uid(name).is_some());

    // Both sides take each name's identity from the one scope, so a name on
    // both sides is one object, and a name on one side is an object the other
    // lacks. Nothing else pairs two objects here.
    let declared_ids = pbps_diff::observed_ids(&declared, scope);
    let live_ids = pbps_diff::observed_ids(&live, scope);

    // Structure first, with every declared text put over the engine's
    // spelling of it wherever the object stands on both sides; compared as
    // read, an unchanged check is dropped and re-added on SQL Server.
    let mut overlaid = Declared::from_schema(&declared).overlay(&live);
    keep_the_defaults_no_base_explains(&mut overlaid, &live);
    let text_differences = text_differences(
        &Declared::from_schema(&live),
        &Declared::from_schema(&overlaid),
        dialect,
    );
    let mut base = overlaid;
    // A read knows the sequence names it leaves out (#1631).
    pbps_dialect::name_read_sequences(&mut base, dialect);
    let mut diffed = pbps_diff::diff_partial(
        pbps_diff::Side {
            schema: &base,
            ids: &live_ids,
        },
        pbps_diff::Side {
            schema: &declared,
            ids: &declared_ids,
        },
        dialect,
        // The declarations' own: the direction is a plan's.
        hints,
    );
    // What a routine's creation lets `PUBLIC` run is a grant by another name.
    diffed.changes.changes.retain(|planned| {
        !crate::rehearsal::is_authorization(&planned.change)
            && !matches!(
                planned.change,
                Change::Permission(GrantChange::PublicExecution { .. })
            )
    });
    Compared {
        changes: diffed.changes,
        text_differences,
        unexpressible: diffed.errors.iter().map(ToString::to_string).collect(),
    }
}

/// Each pair of one table's records that both sides hold.
fn held_by_both<'a, K: Ord, V>(
    stored: &'a BTreeMap<TableName, BTreeMap<K, V>>,
    declared: &'a BTreeMap<TableName, BTreeMap<K, V>>,
) -> impl Iterator<Item = (&'a TableName, &'a K, &'a V, &'a V)> {
    stored.iter().flat_map(move |(table, records)| {
        records.iter().filter_map(move |(key, stored)| {
            let declared = declared.get(table)?.get(key)?;
            Some((table, key, declared, stored))
        })
    })
}

fn differs(out: &mut Vec<TextDifference>, object: String, declared: &str, stored: &str) {
    if declared != stored {
        out.push(TextDifference {
            object,
            declared: declared.to_owned(),
            stored: stored.to_owned(),
        });
    }
}

/// The texts `declared` holds in another form than `stored`.
///
/// `stored` is the read as it came and `declared` the same read under the
/// overlay, so the two hold the same objects, and every pair that differs is a
/// text the overlay replaced. An object on one side only has no pair: it is a
/// structural change, and the changes carry it.
fn text_differences(
    stored: &Declared,
    declared: &Declared,
    dialect: &dyn Dialect,
) -> Vec<TextDifference> {
    let mut out = Vec::new();
    let (was, now) = (&stored.expressions, &declared.expressions);
    for (table, column, declared, stored) in held_by_both(&was.defaults, &now.defaults) {
        let object = format!("default of {}.{}", table.shown(), column.shown());
        differs(&mut out, object, declared, stored);
    }
    for (table, column, declared, stored) in held_by_both(&was.generated, &now.generated) {
        let object = format!(
            "generation expression of {}.{}",
            table.shown(),
            column.shown()
        );
        differs(&mut out, object, declared, stored);
    }
    for (table, column, declared, stored) in held_by_both(&was.computed, &now.computed) {
        let object = format!("computed column {}.{}", table.shown(), column.shown());
        differs(&mut out, object, declared, stored);
    }
    for (table, name, declared, stored) in held_by_both(&was.checks, &now.checks) {
        let object = format!("check {} on {}", name.shown(), table.shown());
        differs(&mut out, object, declared, stored);
    }
    for (table, name, declared, stored) in held_by_both(&was.filters, &now.filters) {
        let object = format!("filter of index {} on {}", name.shown(), table.shown());
        differs(&mut out, object, declared, stored);
    }
    for (table, name, declared, stored) in held_by_both(&was.keys, &now.keys) {
        for (n, pair) in declared.iter().zip(stored).enumerate() {
            if let (Some(declared), Some(stored)) = pair {
                let object = format!(
                    "key {} of index {} on {}",
                    n + 1,
                    name.shown(),
                    table.shown()
                );
                differs(&mut out, object, declared, stored);
            }
        }
    }
    for (table, name, declared, stored) in
        held_by_both(&was.exclusion_filters, &now.exclusion_filters)
    {
        let object = format!(
            "filter of exclusion constraint {} on {}",
            name.shown(),
            table.shown()
        );
        differs(&mut out, object, declared, stored);
    }
    for (table, name, declared, stored) in held_by_both(&was.exclusion_keys, &now.exclusion_keys) {
        for (n, pair) in declared.iter().zip(stored).enumerate() {
            if let (Some(declared), Some(stored)) = pair {
                let object = format!(
                    "element {} of exclusion constraint {} on {}",
                    n + 1,
                    name.shown(),
                    table.shown()
                );
                differs(&mut out, object, declared, stored);
            }
        }
    }
    for (id, stored) in &stored.modules {
        let Some(declared) = declared.modules.get(id) else {
            continue;
        };
        // Layout is not a difference in a module's body: the differ compares
        // two definitions under the same normalization.
        if dialect.normalize_definition(declared) != dialect.normalize_definition(stored) {
            differs(
                &mut out,
                format!("definition of {}", id.shown()),
                declared,
                stored,
            );
        }
    }
    for (name, stored) in &stored.types {
        let Some(declared) = declared.types.get(name) else {
            continue;
        };
        match (declared, stored) {
            (
                ManagedType::Domain {
                    default: declared_default,
                    checks: declared_checks,
                    ..
                },
                ManagedType::Domain {
                    default: stored_default,
                    checks: stored_checks,
                    ..
                },
            ) => {
                if let (Some(declared), Some(stored)) = (declared_default, stored_default) {
                    let object = format!("default of domain {}", name.shown());
                    differs(&mut out, object, declared, stored);
                }
                for (check, stored) in stored_checks {
                    if let Some(declared) = declared_checks.get(check) {
                        let object = format!("check {} on domain {}", check.shown(), name.shown());
                        differs(&mut out, object, declared, stored);
                    }
                }
            }
            (ManagedType::Enum { .. }, _)
            | (ManagedType::Domain { .. }, ManagedType::Enum { .. }) => {}
        }
    }
    out
}

/// The text differences as a person reads them: the object, then both texts
/// one under the other, each escaped (DEC-2032.1).
fn text_report(differences: &[TextDifference]) -> String {
    if differences.is_empty() {
        return String::new();
    }
    let mut out = format!("\nText differences ({}):\n", differences.len());
    for difference in differences {
        out.push_str(&format!(
            "\n  {}\n    declared: {}\n    stored:   {}\n",
            Escaped(&difference.object),
            Escaped(&difference.declared),
            Escaped(&difference.stored)
        ));
    }
    out
}

/// Whether the changes hold a drop and a create that a rename would also
/// explain: a table dropped beside a table created, or a column dropped from
/// a table that gains one.
///
/// A hint for the note and nothing more; it pairs no two objects.
fn may_hold_a_rename(changes: &ChangeSet) -> bool {
    let has = |wanted: fn(&Change) -> bool| changes.changes.iter().any(|p| wanted(&p.change));
    let tables = has(|change| matches!(change, Change::Table(TableChange::DropTable { .. })))
        && has(|change| matches!(change, Change::Table(TableChange::CreateTable { .. })));
    let dropped_from: BTreeSet<&TableName> = changes
        .changes
        .iter()
        .filter_map(|planned| {
            if let Change::Column(ColumnChange::DropColumn { column, .. }) = &planned.change {
                Some(&column.table)
            } else {
                None
            }
        })
        .collect();
    let columns = changes.changes.iter().any(|planned| {
        matches!(
            &planned.change,
            Change::Column(ColumnChange::AddColumn { table, .. }) if dropped_from.contains(table)
        )
    });
    tables || columns
}

#[cfg(test)]
mod tests;
