//! What PostgreSQL will refuse about a role, checked before anything connects.
//!
//! The counterpart of `pbps-mssql/src/validate.rs`'s role half, and the place
//! [ADR-0010](../../../docs/ADR-0010-postgres-privileges.md) §1, §2 and §6
//! land. Every rule here is a rule of the engine, measured on 18.6; a matter
//! of taste is not a rule and does not belong in an error.
//!
//! # Why the schema grant is a refusal and not a warning
//!
//! PostgreSQL checks the *schema* before it looks at the object. A role
//! granted `SELECT` on a table in a schema it has no `USAGE` on holds a
//! permission the engine will never consult: measured, `has_table_privilege`
//! answers `t` while the very same role's `SELECT` is
//! `permission denied for schema app` (ADR-0010 §1). That is a declaration
//! whose plan applies cleanly and leaves the role unable to reach what it was
//! granted — the failure this project exists to make loud — so it is refused
//! where the user has the file open, with the line to add.

use std::collections::BTreeSet;

use pbps_dialect::DialectError;
use pbps_model::{GrantTarget, ModuleId, ModuleKind, ObjectName, Permission, Role, Schema};

use crate::quote;
use crate::types::DIALECT;

fn invalid(message: impl Into<String>) -> DialectError {
    DialectError::Invalid {
        dialect: DIALECT,
        message: message.into(),
    }
}

/// The permissions PostgreSQL has, among the words the model spells
/// (ADR-0010 §6). The union minus SQL Server's two: measured on 18.6,
/// `GRANT ALTER ON app.customer TO r` is
/// `ERROR: unrecognized privilege type "alter"` and `VIEW DEFINITION` is a
/// syntax error at `DEFINITION` — on an object and on a schema alike, so
/// neither is a permission this engine has anywhere.
///
/// `maintain` **is** in this list. Whether the server has it is a question
/// about the server, not about the word: it arrived in PostgreSQL 17, the
/// model holds no server version, and the check therefore belongs to the
/// connected path ([`crate::roles::unsupported_permissions`]) rather than here
/// (ADR-0010 amendment).
pub(crate) const PERMISSIONS: [Permission; 11] = [
    Permission::Select,
    Permission::Insert,
    Permission::Update,
    Permission::Delete,
    Permission::References,
    Permission::Execute,
    Permission::Usage,
    Permission::Create,
    Permission::Truncate,
    Permission::Trigger,
    Permission::Maintain,
];

/// Whether PostgreSQL has `p` at all, on any target.
pub(crate) fn has_permission(p: Permission) -> bool {
    PERMISSIONS.contains(&p)
}

/// The words this engine has, for a message.
pub(crate) fn permission_words() -> String {
    PERMISSIONS
        .iter()
        .map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The permissions the engine defines on a schema. Measured:
/// `GRANT SELECT ON SCHEMA app TO r` is
/// `ERROR: invalid privilege type SELECT for schema`, and
/// `GRANT USAGE, CREATE ON SCHEMA app` is accepted and reads back as `UC`.
const SCHEMA_PERMISSIONS: [Permission; 2] = [Permission::Usage, Permission::Create];

/// PostgreSQL's own name for "everyone", which is not a role a declaration may
/// name.
///
/// Measured, and the quoting is the trap: `GRANT INSERT ON gr.t TO "public"` —
/// quoted, the way every identifier this emitter writes is quoted — grants to
/// PUBLIC (`=a/postgres` in the ACL), while `CREATE ROLE "public"` is
/// `role name "public" is reserved`. So a role declared under that name is not
/// a role at all: pbps would compare a set of grants against every principal
/// in the cluster, and a `GRANT` it wrote would open the object to all of
/// them. `"Public"` is an ordinary role and is left alone — measured, it lands
/// in the ACL as `Public=d/postgres`.
const PUBLIC: &str = "public";

/// What a grant target is, among the kinds this model can declare.
///
/// A trigger is missing on purpose: measured, `GRANT SELECT ON gr.gr_trg` is
/// `relation "gr.gr_trg" does not exist` — a trigger is not a securable on
/// this engine, and it is not an [`ObjectName`] either
/// ([`ModuleId::referenced_name`] answers `None` for one), so the model refuses
/// the target before this file sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Table,
    View,
    Function,
    Procedure,
}

impl TargetKind {
    /// The permissions the engine defines on this kind, among the ones a
    /// declaration can name. Measured on 18.6, every word against every kind:
    /// a table and a view take the same eight, and a function and a procedure
    /// take `EXECUTE` and nothing else.
    const fn permissions(self) -> &'static [Permission] {
        use Permission::*;
        match self {
            TargetKind::Table | TargetKind::View => &[
                Select, Insert, Update, Delete, References, Truncate, Trigger, Maintain,
            ],
            TargetKind::Function | TargetKind::Procedure => &[Execute],
        }
    }

    const fn article(self) -> &'static str {
        match self {
            TargetKind::Table => "a table",
            TargetKind::View => "a view",
            TargetKind::Function => "a function",
            TargetKind::Procedure => "a procedure",
        }
    }
}

/// What the declarations say `object` is, or why the name does not identify
/// one thing.
///
/// `Ok(None)` is "the declarations do not have it" — the model's own finding
/// ([`pbps_model::role::check`]), not this one's, and reporting it twice would
/// have the user fix one message and see the other.
///
/// **Which namespace a bare name means is read off the permissions**, exactly
/// as [`crate::emit`]'s `securable` reads it. PostgreSQL keeps relations and
/// routines in two namespaces and a name may be in both — measured on 18.6, a
/// table `co.f` and a function `co.f(integer)` coexist, `GRANT SELECT ON TABLE
/// co.f` reaches the table and `GRANT EXECUTE ON ROUTINE co.f` reaches the
/// routine. Answering "a table" for both would refuse the second, which is a
/// grant this engine runs (DECISIONS 379).
fn target_kind(
    object: &ObjectName,
    permissions: &BTreeSet<Permission>,
    schema: &Schema,
) -> Result<Option<TargetKind>, DialectError> {
    // A relation of that name, if the declarations have one. Not consulted
    // first when `EXECUTE` is asked for: the emitter would write `ON ROUTINE`
    // there, and the routine is what the grant reaches.
    let relation = || {
        if schema.tables.contains_key(object) {
            return Some(TargetKind::Table);
        }
        schema
            .modules
            .iter()
            .find(|(id, m)| {
                m.kind == ModuleKind::View && id.referenced_name().as_ref() == Some(object)
            })
            .map(|_| TargetKind::View)
    };
    if !permissions.contains(&Permission::Execute)
        && let Some(kind) = relation()
    {
        return Ok(Some(kind));
    }
    let answering: Vec<(&ModuleId, ModuleKind)> = schema
        .modules
        .iter()
        .filter(|(id, _)| matches!(id, ModuleId::Routine(_)))
        .filter(|(id, _)| id.referenced_name().as_ref() == Some(object))
        .map(|(id, m)| (id, m.kind))
        .collect();
    // Overloading is why `GrantTarget::Routine` exists (ADR-0009 §1), and this
    // is the case that needs it. Measured: `GRANT EXECUTE ON ROUTINE gr.f` on
    // an overloaded name is `routine name "gr.f" is not unique`, with the
    // engine's own hint to name the argument list — a statement that fails
    // after everything ordered before it has run. Refused here, where the
    // remedy is a line in a file.
    if answering.len() > 1 {
        let mut signatures: Vec<String> = answering
            .iter()
            .map(|(id, _)| id.to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        signatures.sort();
        return Err(invalid(format!(
            "`{object}` names {} overloads, so PostgreSQL cannot tell which one this grant is \
             on (`routine name \"{object}\" is not unique`); write the signature instead — one \
             of {}",
            answering.len(),
            signatures.join(", ")
        )));
    }
    if let Some((_, kind)) = answering.first() {
        return Ok(Some(match kind {
            ModuleKind::Procedure => TargetKind::Procedure,
            // A trigger has no `referenced_name`, and a view is not a
            // `ModuleId::Routine`, so neither answers here.
            ModuleKind::Function | ModuleKind::View | ModuleKind::Trigger => TargetKind::Function,
        }));
    }
    // `EXECUTE` on a name the declarations hold only as a relation: the kind
    // is the relation's, so the message below names what it actually is.
    Ok(relation())
}

/// What the declarations say the routine at this exact signature is.
///
/// `None` is "the declarations do not have it", which is the model's finding
/// (`pbps_model::role::check`) and not this one's.
fn routine_kind(routine: &pbps_model::RoutineId, schema: &Schema) -> Option<TargetKind> {
    let (_, module) = schema
        .modules
        .iter()
        .find(|(id, _)| matches!(id, ModuleId::Routine(other) if other == routine))?;
    Some(match module.kind {
        ModuleKind::Procedure => TargetKind::Procedure,
        ModuleKind::View | ModuleKind::Function | ModuleKind::Trigger => TargetKind::Function,
    })
}

/// Every problem with a role (ADR-0005, ADR-0010).
///
/// The names have to be ones this dialect can write, each permission has to be
/// one this engine has at all (§6) and then one the engine defines on what the
/// target *is*, and a grant on an object has to come with the `USAGE` that
/// makes it reach anything (§1).
///
/// Returns one error per problem, all of them: a role with three unspellable
/// grants should need one pass.
pub fn role(name: &str, role: &Role, schema: &Schema) -> Vec<DialectError> {
    let mut errs = Vec::new();
    if let Err(e) = quote(name) {
        errs.push(e);
    }
    if name == PUBLIC {
        errs.push(invalid(format!(
            "`{PUBLIC}` is PostgreSQL's name for every principal in the cluster, not a role: \
             `CREATE ROLE \"{PUBLIC}\"` is refused as reserved, and a `GRANT ... TO \"{PUBLIC}\"` \
             — quoted, as this emitter writes every name — opens the object to all of them. \
             Declare a role of your own and grant to that; what PUBLIC holds is reported by \
             `pull` as context and never managed (ADR-0010 §5)"
        )));
    }
    // `pg_` is the engine's own prefix: measured, `CREATE ROLE pg_thing` is
    // `role name "pg_thing" is reserved`. Nothing here creates a role
    // (`manages_roles` is false), so the refusal is not about the `CREATE` —
    // it is that the sixteen `pg_*` roles are the cluster's predefined ones,
    // whose membership and grants are the DBA's and not this project's.
    if name.starts_with("pg_") {
        errs.push(invalid(format!(
            "`{name}` is in PostgreSQL's reserved `pg_` namespace, which holds the cluster's \
             predefined roles; the engine refuses `CREATE ROLE` on such a name, and what those \
             roles are granted is the cluster's business rather than this database's \
             (ADR-0010 §3)"
        )));
    }

    // The schemas this role holds `USAGE` on, from its own declaration. A
    // membership could supply it too — and membership is deliberately never
    // declared, compared or touched (ADR-0005), so it is not a fact this file
    // could read even if it wanted to.
    let usable: BTreeSet<&str> = role
        .grants
        .iter()
        .filter(|(_, permissions)| permissions.contains(&Permission::Usage))
        .filter_map(|(target, _)| match target {
            GrantTarget::Schema(s) => Some(s.as_str()),
            GrantTarget::Object(_) | GrantTarget::Routine(_) => None,
        })
        .collect();

    for (target, permissions) in &role.grants {
        let parts: Vec<&str> = match target {
            GrantTarget::Object(o) => vec![&o.schema, &o.name],
            GrantTarget::Routine(r) => vec![&r.name.schema, &r.name.name],
            GrantTarget::Schema(s) => vec![s],
        };
        for part in parts {
            if let Err(e) = quote(part) {
                errs.push(e);
            }
        }
        // A word the model spells for the other engine (§6). Refused by name,
        // on any target — the engine's parser stops at the word before it
        // looks at the target — and left out of the kind check below, which
        // would otherwise report the same grant twice.
        for p in permissions.iter().filter(|p| !has_permission(**p)) {
            errs.push(invalid(format!(
                "role `{name}`: `{}` on `{target}` is not a permission PostgreSQL has; it is SQL \
                 Server's (ADR-0010 §6), and this engine takes {}",
                p.as_str(),
                permission_words()
            )));
        }
        let engine_words = || permissions.iter().copied().filter(|p| has_permission(*p));

        match target {
            // §2. A `schema::` grant means "present and future" on SQL Server,
            // and PostgreSQL has no such thing to mean: `GRANT SELECT ON ALL
            // TABLES IN SCHEMA` is one-shot — measured, a table created after
            // it is not covered — and `ALTER DEFAULT PRIVILEGES` covers only
            // what *one* role goes on to create, which makes who runs the plan
            // part of what the declaration means. Refused rather than
            // approximated by either: the two spellings would read identically
            // in the file and differ in the database.
            GrantTarget::Schema(s) => {
                for p in engine_words().filter(|p| !SCHEMA_PERMISSIONS.contains(p)) {
                    errs.push(invalid(format!(
                        "role `{name}`: `{}` on `{target}` is not a permission PostgreSQL defines \
                         on a schema (`invalid privilege type {} for schema`), which takes only \
                         {}. On this engine a schema grant does not carry to the objects in it: \
                         `GRANT ... ON ALL TABLES IN SCHEMA` applies once to what is there now, \
                         and `ALTER DEFAULT PRIVILEGES` covers only what one role creates \
                         afterwards (ADR-0010 §2). Grant `{}` on each object instead, and keep \
                         `schema::{s}: [usage]` so the role can reach them",
                        p.as_str(),
                        p.as_str().to_ascii_uppercase(),
                        SCHEMA_PERMISSIONS
                            .iter()
                            .map(|p| p.as_str())
                            .collect::<Vec<_>>()
                            .join(" and "),
                        p.as_str(),
                    )));
                }
            }
            GrantTarget::Object(_) | GrantTarget::Routine(_) => {
                let schema_of = target.schema();
                // §1, and it is checked once per target rather than once per
                // permission: a grant with three permissions on a schema the
                // role cannot enter is one mistake.
                if !usable.contains(schema_of) {
                    errs.push(invalid(format!(
                        "role `{name}`: `{target}` is granted in schema `{schema_of}`, which this \
                         role has no `usage` on — PostgreSQL checks the schema before the object, \
                         so every permission here reaches nothing and the role's own query is \
                         `permission denied for schema {schema_of}` (ADR-0010 §1, measured). Add \
                         `schema::{schema_of}: [usage]` to this role"
                    )));
                }
                let kind = match target {
                    GrantTarget::Object(o) => match target_kind(o, permissions, schema) {
                        Ok(kind) => kind,
                        Err(e) => {
                            errs.push(e);
                            continue;
                        }
                    },
                    // A signature names one overload, so the schema answers
                    // directly. `EXECUTE` is the only permission either kind
                    // takes, but which kind it is decides what the message
                    // calls it — and a message that told an operator their
                    // procedure was a function would be one more thing to
                    // disbelieve.
                    GrantTarget::Routine(r) => routine_kind(r, schema),
                    GrantTarget::Schema(_) => unreachable!("matched above"),
                };
                let Some(kind) = kind else { continue };
                for p in engine_words().filter(|p| !kind.permissions().contains(p)) {
                    errs.push(invalid(format!(
                        "role `{name}`: `{}` does not apply to `{target}`, {}: the engine refuses \
                         that GRANT, and it would refuse it on a database the changes before it \
                         had already altered. {} takes {}",
                        p.as_str(),
                        kind.article(),
                        kind.article(),
                        kind.permissions()
                            .iter()
                            .map(|p| p.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            }
        }
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Module, ModuleKind, RoutineArg, RoutineId, Table};

    /// A schema with one table, one view, one procedure and two overloads of
    /// one function — the shapes a grant can name, and the one that is not an
    /// identity.
    fn declarations() -> Schema {
        let mut schema = Schema::default();
        schema.tables.insert(
            "app.customer".parse().expect("a table name parses"),
            Table::default(),
        );
        let mut module = |id: &str, kind: ModuleKind| {
            schema.modules.insert(
                id.parse().expect("a module id parses"),
                Module {
                    kind,
                    description: None,
                    definition: "SELECT 1".to_owned(),
                },
            );
        };
        module("app.recent", ModuleKind::View);
        module("app.archive(integer)", ModuleKind::Procedure);
        module("app.f(integer)", ModuleKind::Function);
        module("app.f(text)", ModuleKind::Function);
        module("app.solo(integer)", ModuleKind::Function);
        // A name in both namespaces. **Measured on 18.6**: a table `co.f` and
        // a function `co.f(integer)` coexist, and each takes its own GRANT.
        schema.tables.insert(
            "app.both".parse().expect("a table name parses"),
            Table::default(),
        );
        module("app.both(integer)", ModuleKind::Function);
        schema
    }

    fn granting(pairs: &[(&str, &[Permission])]) -> Role {
        let mut role = Role::default();
        for (target, permissions) in pairs {
            role.grants.insert(
                target.parse().expect("a grant target parses"),
                permissions.iter().copied().collect(),
            );
        }
        role
    }

    fn messages(name: &str, role: &Role) -> Vec<String> {
        role_errors(name, role)
            .into_iter()
            .map(|e| e.to_string())
            .collect()
    }

    fn role_errors(name: &str, role: &Role) -> Vec<DialectError> {
        super::role(name, role, &declarations())
    }

    /// The declaration this file exists to accept. Every rule below refuses
    /// something; a file with only refusing tests cannot tell "correct" from
    /// "refuses everything".
    #[test]
    fn a_role_that_can_reach_what_it_is_granted_has_nothing_wrong_with_it() {
        let role = granting(&[
            ("schema::app", &[Permission::Usage]),
            (
                "app.customer",
                &[Permission::Select, Permission::Insert, Permission::Maintain],
            ),
            ("app.recent", &[Permission::Select]),
            ("app.f(integer)", &[Permission::Execute]),
            ("app.archive(integer)", &[Permission::Execute]),
            ("app.solo", &[Permission::Execute]),
        ]);
        let problems = messages("app_reader", &role);
        assert!(problems.is_empty(), "{problems:?}");
    }

    /// Relations and routines are two namespaces on this engine and a name may
    /// be in both. **Measured on 18.6**: with a table `co.f` and a function
    /// `co.f(integer)` in place, `GRANT SELECT ON TABLE co.f` reaches the
    /// table and `GRANT EXECUTE ON ROUTINE co.f` reaches the routine. Which
    /// one a bare target means is read off the permissions — the same answer
    /// [`crate::emit`] writes into the statement — so calling it a table
    /// whenever a table of that name exists refused a grant this engine runs.
    #[test]
    fn a_bare_name_in_both_namespaces_is_read_off_the_permissions() {
        let both = |permissions: &[Permission]| {
            granting(&[
                ("schema::app", &[Permission::Usage]),
                ("app.both", permissions),
            ])
        };
        for permissions in [&[Permission::Execute][..], &[Permission::Select][..]] {
            let problems = messages("app_reader", &both(permissions));
            assert!(problems.is_empty(), "{permissions:?}: {problems:?}");
        }
        // The kind still decides what the word may be: a set with `execute` in
        // it is the routine's, and `truncate` is not a routine's word. The
        // emitter would write one `ON ROUTINE` statement carrying both.
        let problems = messages(
            "app_reader",
            &both(&[Permission::Execute, Permission::Truncate]),
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("a function"), "{}", problems[0]);
    }

    /// §1, and it is a refusal because the plan applies cleanly and the role
    /// still cannot read the table: measured, `has_table_privilege` says `t`
    /// while the role's own `SELECT` is `permission denied for schema app`.
    #[test]
    fn a_grant_in_a_schema_the_role_cannot_enter_is_refused_naming_the_line_to_add() {
        let role = granting(&[("app.customer", &[Permission::Select])]);
        let problems = messages("app_reader", &role);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("`usage`"), "{}", problems[0]);
        assert!(
            problems[0].contains("schema::app: [usage]"),
            "{}",
            problems[0]
        );
        assert!(
            problems[0].contains("permission denied for schema app"),
            "{}",
            problems[0]
        );
    }

    /// One mistake, one message: a grant with three permissions on a schema
    /// the role cannot enter is one missing `usage`, not three.
    #[test]
    fn the_missing_usage_is_reported_once_per_target_and_not_once_per_permission() {
        let role = granting(&[(
            "app.customer",
            &[Permission::Select, Permission::Insert, Permission::Delete],
        )]);
        assert_eq!(messages("app_reader", &role).len(), 1);
    }

    /// `create` on the schema is not `usage` on it. The two are separate
    /// letters in the ACL (`UC`) and only one of them opens the door.
    #[test]
    fn create_on_the_schema_is_not_the_usage_that_makes_a_grant_reach_anything() {
        let role = granting(&[
            ("schema::app", &[Permission::Create]),
            ("app.customer", &[Permission::Select]),
        ]);
        let problems = messages("app_reader", &role);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("no `usage` on"), "{}", problems[0]);
    }

    /// §2. A `schema::` grant means "present and future" on the other engine
    /// and PostgreSQL has nothing that means it, so the word is refused and
    /// the message says what to write instead.
    #[test]
    fn a_table_permission_on_a_schema_target_is_refused_naming_the_object_grants() {
        let role = granting(&[("schema::app", &[Permission::Usage, Permission::Select])]);
        let problems = messages("app_reader", &role);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("invalid privilege type SELECT for schema"),
            "{}",
            problems[0]
        );
        assert!(
            problems[0].contains("ALL TABLES IN SCHEMA"),
            "{}",
            problems[0]
        );
        assert!(
            problems[0].contains("ALTER DEFAULT PRIVILEGES"),
            "{}",
            problems[0]
        );
        assert!(
            problems[0].contains("schema::app: [usage]"),
            "{}",
            problems[0]
        );
    }

    /// §6. The two words the model holds for the other engine, refused by
    /// name — measured, `GRANT ALTER` is `unrecognized privilege type
    /// "alter"`, before the engine looks at the securable at all.
    #[test]
    fn the_two_words_this_engine_does_not_have_are_refused_by_name() {
        for permission in [Permission::Alter, Permission::ViewDefinition] {
            let role = granting(&[
                ("schema::app", &[Permission::Usage]),
                ("app.customer", &[permission]),
            ]);
            let problems = messages("app_reader", &role);
            assert_eq!(problems.len(), 1, "{permission}: {problems:?}");
            assert!(
                problems[0].contains("is not a permission PostgreSQL has"),
                "{}",
                problems[0]
            );
            assert!(problems[0].contains("SQL Server's"), "{}", problems[0]);
        }
    }

    /// A word this engine lacks is reported once, by name, and not a second
    /// time as a word the kind does not take: two messages about one line
    /// send the user to fix the same thing twice.
    #[test]
    fn a_word_this_engine_lacks_is_not_also_reported_against_the_kind() {
        let role = granting(&[
            ("schema::app", &[Permission::Usage]),
            ("app.f(integer)", &[Permission::Alter]),
        ]);
        assert_eq!(messages("app_reader", &role).len(), 1);
    }

    /// The measured matrix: a table takes no `EXECUTE` and a routine takes
    /// nothing but. The engine refuses each, and it would refuse it on a
    /// database the changes before it had already altered.
    #[test]
    fn a_permission_the_kind_does_not_take_is_refused_before_a_plan_exists() {
        let cases = [
            ("app.customer", Permission::Execute, "a table"),
            ("app.recent", Permission::Execute, "a view"),
            ("app.f(integer)", Permission::Select, "a function"),
            ("app.archive(integer)", Permission::Truncate, "a procedure"),
            ("app.solo", Permission::Usage, "a function"),
        ];
        for (target, permission, article) in cases {
            let role = granting(&[
                ("schema::app", &[Permission::Usage]),
                (target, &[permission]),
            ]);
            let problems = messages("app_reader", &role);
            assert_eq!(problems.len(), 1, "{target}: {problems:?}");
            assert!(problems[0].contains(article), "{}", problems[0]);
            assert!(problems[0].contains("does not apply to"), "{}", problems[0]);
        }
    }

    /// A routine target is checked against `EXECUTE` even where the routine is
    /// named by signature, which is the path that does not go through
    /// `target_kind`.
    #[test]
    fn a_signature_target_takes_execute_and_nothing_else() {
        let role = granting(&[
            ("schema::app", &[Permission::Usage]),
            ("app.f(text)", &[Permission::Select]),
        ]);
        let problems = messages("app_reader", &role);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("a function"), "{}", problems[0]);
    }

    /// ADR-0009 §1. A bare name is not an identity where the kind overloads,
    /// and the engine says so at apply time — `routine name "app.f" is not
    /// unique`. Refused here, with the signatures to write instead.
    #[test]
    fn a_bare_name_for_an_overloaded_routine_is_refused_with_the_signatures() {
        let role = granting(&[
            ("schema::app", &[Permission::Usage]),
            ("app.f", &[Permission::Execute]),
        ]);
        let problems = messages("app_reader", &role);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("is not unique"), "{}", problems[0]);
        assert!(problems[0].contains("app.f(integer)"), "{}", problems[0]);
        assert!(problems[0].contains("app.f(text)"), "{}", problems[0]);
    }

    /// An object the declarations do not have is the model's finding
    /// (`pbps_model::role::check`), not this one's: reported twice, the user
    /// fixes one message and sees the other.
    #[test]
    fn an_object_the_declarations_do_not_have_is_left_to_the_model_to_report() {
        let role = granting(&[
            ("schema::app", &[Permission::Usage]),
            ("app.nowhere", &[Permission::Select]),
        ]);
        let problems = messages("app_reader", &role);
        assert!(problems.is_empty(), "{problems:?}");
    }

    /// Measured, and the quoting is the trap: `GRANT INSERT ON gr.t TO
    /// "public"` — quoted, the way this emitter writes every name — grants to
    /// PUBLIC, while `CREATE ROLE "public"` is refused as reserved.
    #[test]
    fn public_is_not_a_role_a_declaration_may_name() {
        let problems = messages("public", &Role::default());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("every principal in the cluster"),
            "{}",
            problems[0]
        );
        // A different name, and an ordinary role: measured, `"Public"` lands
        // in the ACL as `Public=d/postgres`.
        assert!(messages("Public", &Role::default()).is_empty());
    }

    /// The engine's own namespace: measured, `CREATE ROLE pg_thing` is
    /// `role name "pg_thing" is reserved`.
    #[test]
    fn a_role_in_the_engines_reserved_namespace_is_refused() {
        let problems = messages("pg_read_all_data", &Role::default());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("reserved"), "{}", problems[0]);
    }

    /// A name the engine cannot hold is refused wherever it appears — the
    /// role's own, and each half of a target's.
    #[test]
    fn a_name_the_engine_cannot_hold_is_refused_on_the_role_and_on_its_targets() {
        let long = "n".repeat(crate::MAX_IDENT_BYTES + 1);
        assert!(!messages(&long, &Role::default()).is_empty());
        let mut role = Role::default();
        role.grants.insert(
            GrantTarget::Schema(long.clone()),
            [Permission::Usage].into_iter().collect(),
        );
        assert!(!messages("app_reader", &role).is_empty());
        let mut role = Role::default();
        role.grants.insert(
            GrantTarget::Routine(RoutineId::new(
                pbps_model::ObjectName::new(long, "f".to_owned()),
                vec!["integer".parse::<RoutineArg>().expect("a type name")],
            )),
            [Permission::Execute].into_iter().collect(),
        );
        assert!(!messages("app_reader", &role).is_empty());
    }
}
