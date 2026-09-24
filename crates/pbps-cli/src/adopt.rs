//! What an adoption writes, and what it leaves out because `validate` would
//! refuse it (#902).
//!
//! `pull` and `init --from` turn a catalog read into declaration files. The
//! reader and the validator are two separate answers to "can a declaration say
//! this?", and wherever they disagree the adoption used to write a project its
//! very next command refused: a PostgreSQL identity whose increment outruns its
//! type (#504), a table or a grant in a schema named `$user` (#705). Teaching
//! the reader each rule the validator has would keep the two in step only
//! until the next rule; asking the validator itself keeps them in step by
//! construction. So an object the validator refuses is left out and named,
//! the way the reader already names what it cannot express, and what is
//! written is then loaded back and put through the same
//! [`crate::declaration_problems`] every other command uses (DECISIONS 141,
//! DEC-902.1).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use pbps_db::catalog::{LimitationTarget, Pulled, Unexpressible, UnmanagedModule};
use pbps_dialect::Dialect;
use pbps_model::{ModuleId, ModuleKind, Role, Schema};

use crate::declaration_file;

/// Takes out of `pulled.schema` every object the dialect's validation
/// refuses, and records each one where the adoption already reports what it
/// left behind: a table in `warnings`, a module in `unmanaged_modules`, a
/// grant in `unexpressible`, a role in `warnings`.
///
/// Per object, not per schema, because the alternative is what `init --from`
/// did: refuse the whole adoption over one table in a schema nobody uses. An
/// object left out is one the operator can see and settle; an adoption that
/// wrote nothing is one they cannot start from.
///
/// Only the questions one object can answer are asked here. The whole-schema
/// ones — two names that collide, a dependency on a missing module — have no
/// single object to blame, so they stay with [`check_staged`], which refuses
/// the write instead.
pub(crate) fn leave_out_what_validate_refuses(pulled: &mut Pulled, dialect: &dyn Dialect) {
    let schema = &mut pulled.schema;

    let refused: Vec<_> = schema
        .tables
        .iter()
        .filter_map(|(name, table)| {
            let why = rendered(dialect.validate_table(name, table));
            (!why.is_empty()).then(|| (name.clone(), why))
        })
        .collect();
    for (name, why) in refused {
        schema.tables.remove(&name);
        pulled.warnings.push(format!(
            "table `{name}` was left out: `pbps validate` refuses its declaration — {}",
            why.join("; ")
        ));
    }

    let refused: Vec<_> = schema
        .modules
        .iter()
        .filter_map(|(id, module)| {
            let why = rendered(dialect.validate_module(id, module));
            (!why.is_empty()).then(|| (id.clone(), module.kind, why.join("; ")))
        })
        .collect();
    for (id, kind, why) in refused {
        schema.modules.remove(&id);
        pulled.unmanaged_modules.push(UnmanagedModule {
            kind: kind.as_str(),
            target: LimitationTarget::module(id),
            why: format!("`pbps validate` refuses its declaration — {why}"),
        });
    }

    // A trigger whose table is not among the declarations is refused by
    // `module::check_names`, and the table leaving is the usual way one gets
    // there: its trigger goes with it, rather than stopping the whole write.
    let orphaned: Vec<_> = schema
        .modules
        .iter()
        .filter_map(|(id, module)| match id {
            ModuleId::Trigger { on, .. }
                if module.kind == ModuleKind::Trigger && !a_trigger_target(schema, on) =>
            {
                Some((id.clone(), on.clone()))
            }
            ModuleId::Trigger { .. } | ModuleId::Named(_) | ModuleId::Routine(_) => None,
        })
        .collect();
    for (id, on) in orphaned {
        schema.modules.remove(&id);
        pulled.unmanaged_modules.push(UnmanagedModule {
            kind: ModuleKind::Trigger.as_str(),
            target: LimitationTarget::module(id),
            why: format!("its table `{on}` is not among the declarations this adoption wrote"),
        });
    }

    // Roles last: which grants survive depends on which objects did.
    let mut probe = schema.clone();
    probe.roles.clear();
    let names: Vec<String> = schema.roles.keys().cloned().collect();
    for name in names {
        let role = schema.roles.get_mut(&name).expect("listed above");
        let (left_out, whole) = narrow_role(&name, role, &mut probe, dialect);
        pulled.unexpressible.extend(left_out);
        if let Some(why) = whole {
            schema.roles.remove(&name);
            pulled.warnings.push(format!(
                "role `{name}` was left out: `pbps validate` refuses its declaration — {why}"
            ));
        }
    }
}

/// Whether a trigger on `on` has somewhere to be: the rule
/// `module::check_names` applies, a table or a view.
fn a_trigger_target(schema: &Schema, on: &pbps_model::ObjectName) -> bool {
    schema.tables.contains_key(on)
        || schema
            .modules
            .iter()
            .any(|(id, m)| m.kind == ModuleKind::View && id.referenced_name().as_ref() == Some(on))
}

/// Takes out of `role` the grants that carry its problems, one at a time,
/// and returns them as unexpressible permissions — plus the reason the role
/// itself is refused, if it still is with every grant gone.
///
/// A grant is judged by what removing it fixes, not by validating it alone:
/// some rules are about the grants together — PostgreSQL's "a grant in a
/// schema needs `usage` on it" — and a grant validated on its own would fail
/// that rule for want of the `usage` its role does hold.
fn narrow_role(
    name: &str,
    role: &mut Role,
    probe: &mut Schema,
    dialect: &dyn Dialect,
) -> (Vec<Unexpressible>, Option<String>) {
    let mut left_out = Vec::new();
    let mut problems = role_problems(name, role, probe, dialect);
    // Repeated until a pass changes nothing: a grant whose removal fixes
    // nothing on the first pass can be the one left carrying a problem once
    // another has gone.
    loop {
        if problems.is_empty() {
            return (left_out, None);
        }
        let mut changed = false;
        let targets: Vec<_> = role.grants.keys().cloned().collect();
        for target in targets {
            let permissions = role.grants.remove(&target).expect("listed above");
            let after = role_problems(name, role, probe, dialect);
            if after.len() < problems.len() {
                let fixed: Vec<&str> = problems
                    .iter()
                    .filter(|p| !after.contains(p))
                    .map(String::as_str)
                    .collect();
                let words: Vec<&str> = permissions.iter().map(|p| p.as_str()).collect();
                left_out.push(Unexpressible {
                    role: name.to_owned(),
                    target: Some(target.clone()),
                    what: format!(
                        "role `{name}`: {} on `{target}` was left out: `pbps validate` refuses \
                         it — {}",
                        words.join(", "),
                        fixed.join("; ")
                    ),
                });
                problems = after;
                changed = true;
                if problems.is_empty() {
                    return (left_out, None);
                }
            } else {
                role.grants.insert(target, permissions);
            }
        }
        if !changed {
            break;
        }
    }
    // No single grant carries what is left. If the role is refused with none
    // at all, the role is the problem; otherwise it is a combination this
    // cannot attribute, and the staged check refuses the write over it.
    let bare = Role {
        description: role.description.clone(),
        grants: Default::default(),
    };
    let own = role_problems(name, &bare, probe, dialect);
    if own.is_empty() {
        return (left_out, None);
    }
    for (target, permissions) in std::mem::take(&mut role.grants) {
        let words: Vec<&str> = permissions.iter().map(|p| p.as_str()).collect();
        left_out.push(Unexpressible {
            role: name.to_owned(),
            what: format!(
                "role `{name}`: {} on `{target}` was left out with the role",
                words.join(", ")
            ),
            target: Some(target),
        });
    }
    (left_out, Some(own.join("; ")))
}

/// Every problem `declaration_problems` would report about this one role:
/// the dialect's rules and the model's grant-target rule, which needs the
/// schema's tables and modules but none of its other roles.
fn role_problems(
    name: &str,
    role: &Role,
    probe: &mut Schema,
    dialect: &dyn Dialect,
) -> Vec<String> {
    let mut out = rendered(dialect.validate_role(name, role, probe));
    probe.roles.insert(name.to_owned(), role.clone());
    out.extend(pbps_model::role::check(probe));
    probe.roles.clear();
    out
}

fn rendered(errors: Vec<pbps_dialect::DialectError>) -> Vec<String> {
    errors.iter().map(ToString::to_string).collect()
}

/// Writes `schema` into `dir` as declaration files, one per object, and
/// returns the paths written. `dir` must exist.
///
/// One writer for `pull`, for its staged check, and for `init`'s staging:
/// the files a check loaded have to be the files the command writes.
pub(crate) fn write_declarations(
    dir: &Path,
    schema: &Schema,
    public_execute: &pbps_model::PublicExecute,
) -> anyhow::Result<BTreeSet<PathBuf>> {
    // Before the first file: two declarations whose names differ only in case
    // encode to filenames that differ only in case, and a filesystem that
    // ignores case would keep one of each — the second silently written over
    // the first, with the identity file naming both (DECISIONS 135).
    declaration_file::refuse_folded_paths(&declaration_file::paths_of(dir, schema)?)?;
    let mut written = BTreeSet::new();
    for (name, table) in &schema.tables {
        let path = declaration_file::path(dir, name, None)?;
        std::fs::write(&path, pbps_load::render(name, table, &[], None))
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        written.insert(path);
    }
    // Modules go into files of their own, named for the kind as well as the
    // object: a view and a table cannot collide in the database, so they must
    // not collide on disk either (ADR-0002).
    for (id, module) in &schema.modules {
        let path = declaration_file::module_path(dir, id, module.kind)?;
        std::fs::write(
            &path,
            pbps_load::render_module(id, module, &Default::default(), public_execute.contains(id)),
        )
        .with_context(|| format!("cannot write `{}`", path.display()))?;
        written.insert(path);
    }
    // Roles (ADR-0005), one file each, with the grants the catalog holds on
    // objects pbps can express.
    for (name, role) in &schema.roles {
        let path = declaration_file::role_path(dir, name)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create `{}`", parent.display()))?;
        }
        std::fs::write(&path, pbps_load::render_role(name, role, &[]))
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        written.insert(path);
    }
    Ok(written)
}

/// Loads the declarations staged in `dir` and refuses them if `validate`
/// would: from the serialized files, not the values that produced them, so a
/// renderer that drifted from the loader is caught before anything appears
/// in the project.
///
/// What reaches this has been through [`leave_out_what_validate_refuses`], so
/// a problem here is one no single object carries — or a validator rule that
/// function does not ask. Either way the project the command would leave
/// behind is one its next command refuses, and writing nothing is the honest
/// outcome.
pub(crate) fn check_staged(dir: &Path, dialect: &dyn Dialect) -> anyhow::Result<pbps_load::Loaded> {
    let loaded = pbps_load::load_schema_dir(dir).map_err(|errors| {
        anyhow::anyhow!(
            "the staged declarations have {} problem(s):\n  {}",
            errors.len(),
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n  ")
        )
    })?;
    let problems: Vec<String> = crate::declaration_problems(&loaded, dialect)
        .into_iter()
        .map(|(_, problem)| problem)
        .collect();
    if !problems.is_empty() {
        anyhow::bail!(
            "the staged declarations are not valid for {}:\n  {}",
            dialect.name(),
            problems.join("\n  ")
        );
    }
    Ok(loaded)
}

/// The project's declaration policies over what an adoption is about to
/// write: every rule `validate` evaluates, with its type spelling, context and
/// suppressions (ADR-0008). A finding at `error` refuses the write, since the
/// next `validate` would refuse the project; the rest are returned for the
/// caller to print.
///
/// Refused rather than left out, unlike [`leave_out_what_validate_refuses`]:
/// a rule is the project's own choice about names and sizes, not something
/// the engine cannot do, and its remedy — a suppression with a reason, a
/// different severity — is a line in `pbps.yml` the operator writes, not an
/// object to settle by hand. Leaving the table out would silently narrow the
/// adoption to what the project's rules happen to like (DECISIONS 114).
///
/// A `policies:` block that is itself invalid is not evaluated, as in
/// `validate`, which reports it: the block's problem is not the files'.
pub(crate) fn refuse_policy_errors(
    project: &pbps_config::Project,
    loaded: &pbps_load::Loaded,
    dialect: &dyn Dialect,
) -> anyhow::Result<Vec<pbps_model::Finding>> {
    let policies = project.config.policies();
    if !policies.check().is_empty() {
        return Ok(Vec::new());
    }
    let spelled = crate::types_as_the_dialect_spells_them(&loaded.schema, dialect);
    let (errors, rest): (Vec<_>, Vec<_>) =
        pbps_policy::declarations(&spelled, &policies, &crate::policy_context(project, false))
            .into_iter()
            .partition(|f| f.severity == pbps_model::Severity::Error);
    if !errors.is_empty() {
        anyhow::bail!(
            "these declarations break a rule `pbps.yml` sets to `error`:\n  {}\n\
             Nothing was written. Suppress the rule for the object with a reason, lower its \
             severity, or change its parameters; a table over `data.max-rows` can also be \
             pulled without `--data`.",
            errors
                .iter()
                .map(|f| format!("{}: {}", f.id, f.message))
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
    Ok(rest)
}

/// A scratch directory for staged declarations, removed when dropped.
///
/// Under the system's temporary directory, not the project: a project whose
/// `schema_dir` is its root would otherwise list the stage as declarations
/// of its own if the process died before the guard ran.
pub(crate) struct Stage(PathBuf);

impl Stage {
    pub(crate) fn new(what: &str) -> anyhow::Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("pbps-{what}-stage-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&path)
            .with_context(|| format!("cannot create staging directory `{}`", path.display()))?;
        Ok(Self(path))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, Identity, Module, Permission, PrimaryKey, Table, TableName};

    fn table(ty: &str, identity: Option<Identity>) -> Table {
        let mut t = Table::default();
        let mut id = Column::new(ty.parse().unwrap()).not_null();
        id.identity = identity;
        t.columns.insert("id".into(), id);
        t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
        });
        t
    }

    fn role(grants: &[(&str, Permission)]) -> Role {
        let mut r = Role::default();
        for (target, p) in grants {
            r.grants
                .entry(target.parse().unwrap())
                .or_default()
                .insert(*p);
        }
        r
    }

    fn pg() -> Box<dyn Dialect> {
        Box::new(pbps_pg::Postgres::new())
    }

    /// The two shapes #504 and #705 found: the reader expresses them, the
    /// validator refuses them. Each is left out and named; its neighbours —
    /// including the exact-span identity the validator accepts — stay.
    #[test]
    fn a_table_validate_refuses_is_left_out_and_named_and_its_neighbours_stay() {
        let mut pulled = Pulled::default();
        let s = &mut pulled.schema;
        s.tables
            .insert(TableName::new("app", "t"), table("integer", None));
        s.tables
            .insert(TableName::new("$user", "mine"), table("integer", None));
        let over = Identity {
            seed: 1,
            increment: 40000,
        };
        s.tables
            .insert(TableName::new("app", "big"), table("smallint", Some(over)));
        // The directional span is accepted exactly: 1..=32767 upward, and
        // -1 down to -32768 for a descending sequence.
        for (name, seed, increment) in [("edge", 1, 32766), ("down", -1, -32767)] {
            let at = Identity { seed, increment };
            s.tables
                .insert(TableName::new("app", name), table("smallint", Some(at)));
        }

        leave_out_what_validate_refuses(&mut pulled, pg().as_ref());

        let kept: Vec<String> = pulled.schema.tables.keys().map(|n| n.to_string()).collect();
        assert_eq!(kept, ["app.down", "app.edge", "app.t"]);
        let warned = pulled.warnings.join("\n");
        assert!(
            warned.contains("table `$user.mine` was left out"),
            "{warned}"
        );
        // The actual identity is in the reason, not a rounded one.
        assert!(
            warned.contains("table `app.big` was left out") && warned.contains("40000"),
            "{warned}"
        );
        assert_eq!(pulled.warnings.len(), 2, "{warned}");
    }

    /// A grant is judged by what removing it fixes. Validated on its own,
    /// `select` on `app.t` would fail PostgreSQL's "a grant in a schema needs
    /// `usage` on it" rule even where its role holds that `usage`.
    #[test]
    fn a_grant_is_left_out_only_when_it_carries_a_problem() {
        let mut pulled = Pulled::default();
        let s = &mut pulled.schema;
        s.tables
            .insert(TableName::new("app", "t"), table("integer", None));
        s.tables
            .insert(TableName::new("$user", "mine"), table("integer", None));
        s.tables
            .insert(TableName::new("other", "u"), table("integer", None));
        s.roles.insert(
            "reader".into(),
            role(&[
                ("schema::app", Permission::Usage),
                ("app.t", Permission::Select),
                ("schema::$user", Permission::Usage),
                ("$user.mine", Permission::Select),
                // No `usage` on `other`: the one this role cannot use.
                ("other.u", Permission::Select),
            ]),
        );

        leave_out_what_validate_refuses(&mut pulled, pg().as_ref());

        let kept: Vec<String> = pulled.schema.roles["reader"]
            .grants
            .keys()
            .map(ToString::to_string)
            .collect();
        assert_eq!(kept, ["app.t", "schema::app"]);
        let mut left: Vec<String> = pulled
            .unexpressible
            .iter()
            .map(|u| {
                assert_eq!(u.role, "reader");
                u.target.as_ref().unwrap().to_string()
            })
            .collect();
        left.sort();
        assert_eq!(left, ["$user.mine", "other.u", "schema::$user"]);
        // The reason is the validator's own sentence.
        let other = pulled
            .unexpressible
            .iter()
            .find(|u| u.what.contains("`other.u`"))
            .unwrap();
        assert!(other.what.contains("no `usage`"), "{}", other.what);
        assert!(
            pulled.warnings.len() == 1,
            "only the table: {:?}",
            pulled.warnings
        );
        assert!(pbps_model::role::check(&pulled.schema).is_empty());
    }

    /// A role nothing but its name makes invalid is left out whole, and every
    /// grant it held is named with it.
    #[test]
    fn a_role_refused_by_name_is_left_out_with_its_grants() {
        let mut pulled = Pulled::default();
        let s = &mut pulled.schema;
        s.tables
            .insert(TableName::new("dbo", "t"), table("int", None));
        s.roles
            .insert("db_owner".into(), role(&[("dbo.t", Permission::Select)]));
        s.roles
            .insert("app_reader".into(), role(&[("dbo.t", Permission::Select)]));

        leave_out_what_validate_refuses(&mut pulled, &pbps_mssql::Mssql);

        assert!(pulled.schema.roles.contains_key("app_reader"));
        assert!(!pulled.schema.roles.contains_key("db_owner"));
        assert!(
            pulled
                .warnings
                .iter()
                .any(|w| w.contains("role `db_owner` was left out")),
            "{:?}",
            pulled.warnings
        );
        assert_eq!(pulled.unexpressible.len(), 1, "{:?}", pulled.unexpressible);
        assert!(
            pulled.unexpressible[0]
                .what
                .contains("left out with the role")
        );
    }

    /// A module the validator refuses goes to the inventory of what is left
    /// alone, and so does a trigger whose table did not make it.
    #[test]
    fn a_refused_module_and_a_trigger_whose_table_left_are_inventoried() {
        let mut pulled = Pulled::default();
        let s = &mut pulled.schema;
        s.tables
            .insert(TableName::new("app", "t"), table("integer", None));
        let view = |sql: &str| Module {
            kind: ModuleKind::View,
            description: None,
            definition: sql.into(),
        };
        s.modules
            .insert("$user.v".parse().unwrap(), view("SELECT 1 AS x"));
        s.modules
            .insert("app.v".parse().unwrap(), view("SELECT 1 AS x"));
        let trigger = Module {
            kind: ModuleKind::Trigger,
            description: None,
            definition: "AFTER INSERT ON {} FOR EACH ROW EXECUTE FUNCTION app.f()".into(),
        };
        let on = |t: &str| Module {
            definition: trigger.definition.replace("{}", t),
            ..trigger.clone()
        };
        // The ledger's table is never pulled; the trigger on it was (#891).
        s.modules
            .insert("app.gone.audit".parse().unwrap(), on("app.gone"));
        s.modules
            .insert("app.t.audit".parse().unwrap(), on("app.t"));

        leave_out_what_validate_refuses(&mut pulled, pg().as_ref());

        let mut kept: Vec<String> = pulled
            .schema
            .modules
            .keys()
            .map(|id| id.to_string())
            .collect();
        kept.sort();
        assert_eq!(kept, ["app.t.audit", "app.v"]);
        let mut left: Vec<String> = pulled
            .unmanaged_modules
            .iter()
            .map(|m| format!("{} {}", m.kind, m.target))
            .collect();
        left.sort();
        assert_eq!(left, ["trigger app.gone.audit", "view $user.v"]);
        assert!(pbps_model::module::check_names(&pulled.schema).is_empty());
    }

    /// Nothing refused, nothing moved: the common case is untouched.
    #[test]
    fn a_pull_validate_accepts_is_left_exactly_as_read() {
        let mut pulled = Pulled::default();
        pulled
            .schema
            .tables
            .insert(TableName::new("app", "t"), table("integer", None));
        pulled.schema.roles.insert(
            "reader".into(),
            role(&[
                ("schema::app", Permission::Usage),
                ("app.t", Permission::Select),
            ]),
        );
        let before = pulled.clone();
        leave_out_what_validate_refuses(&mut pulled, pg().as_ref());
        assert_eq!(pulled, before);
    }

    /// What no single object carries is refused whole, before any file
    /// reaches the project, and the message names the dialect it asked.
    #[test]
    fn a_staged_project_validate_refuses_is_refused_and_names_its_dialect() {
        let mut schema = Schema::default();
        let mut t = table("integer", None);
        t.primary_key.as_mut().unwrap().name = Some("u".into());
        schema.tables.insert(TableName::new("app", "t"), t);
        // PostgreSQL puts an index in its schema's relation namespace, so a
        // primary key named after another table collides with it.
        schema
            .tables
            .insert(TableName::new("app", "u"), table("integer", None));

        let stage = Stage::new("adopt-test").unwrap();
        write_declarations(stage.path(), &schema, &Default::default()).unwrap();
        let e = check_staged(stage.path(), pg().as_ref())
            .unwrap_err()
            .to_string();
        assert!(e.contains("not valid for postgres"), "{e}");

        // The control: the same files without the collision pass.
        let stage = Stage::new("adopt-test").unwrap();
        schema.tables.remove(&TableName::new("app", "u"));
        write_declarations(stage.path(), &schema, &Default::default()).unwrap();
        let loaded = check_staged(stage.path(), pg().as_ref()).unwrap();
        assert_eq!(loaded.schema, schema);
    }

    /// The project's own rules are evaluated as `validate` evaluates them:
    /// at `error` nothing is written, below it the finding is handed back,
    /// and a suppression with a reason excuses the table by name (#919).
    #[test]
    fn a_rule_the_project_sets_to_error_refuses_the_adoption_and_a_suppression_excuses_it() {
        let mut schema = Schema::default();
        schema
            .tables
            .insert(TableName::new("app", "Bad"), table("integer", None));
        schema
            .tables
            .insert(TableName::new("app", "good"), table("integer", None));
        let stage = Stage::new("adopt-policy-test").unwrap();
        write_declarations(stage.path(), &schema, &Default::default()).unwrap();
        let loaded = check_staged(stage.path(), pg().as_ref()).unwrap();

        let project_with = |policies: &str| {
            let dir = Stage::new("adopt-policy-project").unwrap();
            let config = dir.path().join("pbps.yml");
            std::fs::write(&config, format!("dialect: postgres\n{policies}")).unwrap();
            let project = pbps_config::Project::load(&config).unwrap();
            (dir, project)
        };
        let rule = "policies:\n  rules:\n    naming.table: {severity: SEVERITY, pattern: \"^[a-z][a-z0-9_]*$\"}\n";

        let (_d, strict) = project_with(&rule.replace("SEVERITY", "error"));
        let e = refuse_policy_errors(&strict, &loaded, pg().as_ref())
            .unwrap_err()
            .to_string();
        assert!(e.contains("naming.table") && e.contains("app.Bad"), "{e}");
        assert!(e.contains("Nothing was written"), "{e}");
        assert!(!e.contains("app.good"), "{e}");

        // Below `error` it is reported, not refused.
        let (_d, lenient) = project_with(&rule.replace("SEVERITY", "warning"));
        let found = refuse_policy_errors(&lenient, &loaded, pg().as_ref()).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, pbps_model::Severity::Warning);

        // Excused by name, with a reason: the same pull goes through quietly.
        let excused = format!(
            "{}  suppress:\n    - rule: naming.table\n      on: app.Bad\n      reason: inherited\n",
            rule.replace("SEVERITY", "error")
        );
        let (_d, excused) = project_with(&excused);
        let found = refuse_policy_errors(&excused, &loaded, pg().as_ref()).unwrap();
        assert!(found.is_empty(), "{found:?}");

        // A project with no rules of its own refuses nothing.
        let (_d, plain) = project_with("");
        assert!(
            refuse_policy_errors(&plain, &loaded, pg().as_ref())
                .unwrap()
                .is_empty()
        );
    }
}
