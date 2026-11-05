//! PostgreSQL declaration structure and roles, checked before anything connects.
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

use pbps_model::ColumnName;
use pbps_model::storage::MethodPolicy;
use std::collections::BTreeSet;

use pbps_dialect::{DialectError, Expression};
use pbps_model::{
    GrantTarget, Index, ModuleId, ModuleKind, ObjectName, Permission, Role, Schema, Table,
};

use crate::quote;
use crate::types::DIALECT;

fn invalid(message: impl Into<String>) -> DialectError {
    DialectError::Invalid {
        dialect: DIALECT,
        message: message.into(),
    }
}

/// Structural declaration errors, measured on PostgreSQL 18.6. Unlike SQL
/// Server, repeated index keys, repeated INCLUDE columns, a key in INCLUDE,
/// and repeated local FK columns are legal here. Only PK/UNIQUE lists reject
/// duplicates. Index width depends on the server build and key-type eligibility
/// on installed operator classes. Both stay with the server, not guessed from
/// the stock catalogue by an offline validator (DECISIONS 452).
pub(crate) fn table_structure(table: &Table) -> Vec<DialectError> {
    let mut found = Vec::new();
    found.extend(table.constraint_name_conflicts().into_iter().map(invalid));
    // Refused whatever it names, so its structural problems are not asked
    // on top: the whole line is the mistake. `CLUSTER` is not the same
    // thing under another name — the engine's documentation calls it "a
    // one-time operation", after which new and updated rows go wherever
    // there is room — so there is no statement a plan could emit to honour
    // it (#1178).
    // The same for a column collation (#1175): the model holds SQL Server's,
    // and this reader still leaves a collated column out rather than
    // declaring it, so a declaration here has nothing it could round-trip to.
    for (name, column) in &table.columns {
        if let Some(collation) = &column.collation {
            found.push(invalid(format!(
                "column `{name}` names collation `{collation}`; column collations are modelled \
                 for SQL Server only, so remove the line",
                name = name.shown(),
                collation = pbps_model::escape::Escaped(collation.as_str()),
            )));
        }
    }
    if table.clustered.is_some() {
        found.push(invalid(
            "`clustered` is a SQL Server table layout: PostgreSQL has no clustered index \
             (`CLUSTER` reorders a table once and keeps no order), so remove the line",
        ));
    }
    found.extend(table.replica_identity_problems().into_iter().map(invalid));
    found.extend(table.storage_parameter_problems().into_iter().map(invalid));
    if let Some(pk) = &table.primary_key {
        found.extend(key_columns("primary key", &pk.columns, table, true));
    }
    for (name, unique) in &table.unique {
        let what = format!("unique constraint `{name}`", name = name.shown(),);
        found.extend(key_columns(&what, &unique.columns, table, true));
    }
    for (name, fk) in &table.foreign_keys {
        let what = format!("foreign key `{name}`", name = name.shown(),);
        found.extend(key_columns(&what, &fk.columns, table, false));
        if fk.columns.len() != fk.references_columns.len() {
            found.push(invalid(format!(
                "{what} has {} column(s) but references {}; the two sides must line up",
                fk.columns.len(),
                fk.references_columns.len()
            )));
        }
        if fk.references_columns.is_empty() {
            found.push(invalid(format!(
                "{what} names no columns on the referenced table"
            )));
        }
        // The referenced side, and only the referenced side. Measured on
        // 18.6, `FOREIGN KEY (a, b) REFERENCES p (x, x)` is
        // `42830: foreign key referenced-columns list must not contain
        // duplicates` — and it is refused even where a unique index really
        // does repeat that key, since `CREATE UNIQUE INDEX … (x, x)` is
        // itself legal here. So this is an analysis rule of the engine rather
        // than a missing-key error, unlike the 32-column width, which is the
        // build's `max_index_keys` and stays with the server (DECISIONS 452,
        // 506).
        //
        // The local list is left alone on purpose: 452 measured
        // `FOREIGN KEY (a, a) REFERENCES p (x, y)` **accepted** against a
        // composite unique key, and refusing it here would reject a valid
        // declaration. Two lists, two rules.
        //
        // Reported once per repeated name rather than once per repetition:
        // `(x, x, x)` is one mistake, and the remedy is the same for both
        // extra copies.
        let mut seen = BTreeSet::new();
        let mut said = BTreeSet::new();
        for column in &fk.references_columns {
            if !seen.insert(column) && said.insert(column) {
                found.push(invalid(format!(
                    "{what} names `{column}` twice on the referenced table;                      the referenced columns must be distinct",
                    column = column.shown(),
                )));
            }
        }
    }
    for (name, check) in &table.checks {
        // Asked of the engine's lexis rather than of Rust's whitespace class.
        // Non-ASCII whitespace can be an unquoted identifier here, so Unicode
        // `trim` would reject a legal boolean-column expression; `trim_ascii`
        // keeps those but misses the vertical tab, which this engine does
        // separate tokens with, and neither sees a comment (DECISIONS 504).
        if crate::LEXICON.expression_in(&check.expression) == Expression::Absent {
            found.push(invalid(format!(
                "check constraint `{name}` has an empty expression",
                name = name.shown(),
            )));
        }
    }
    // The prefix a staged attach's temporary CHECK takes (DEC-1638.1).
    // `baseline` removes one by its exact name, so a declared check that
    // could hold that name would be removed with it; and a table's
    // constraints of every kind share one namespace, so a key or a foreign
    // key holding it would fail the attach's `ADD`. None can.
    let constraints = table
        .primary_key
        .iter()
        .filter_map(|key| key.name.as_ref().map(|name| ("primary key", name)))
        .chain(table.unique.keys().map(|name| ("unique constraint", name)))
        .chain(table.foreign_keys.keys().map(|name| ("foreign key", name)))
        .chain(table.checks.keys().map(|name| ("check constraint", name)));
    for (kind, name) in constraints {
        if name.raw().starts_with(crate::emit::SCAN_CHECK_PREFIX) {
            found.push(invalid(format!(
                "{kind} `{name}` starts with `{}`, which pbps keeps for the temporary \
                 CHECK of a staged attach; rename it",
                crate::emit::SCAN_CHECK_PREFIX,
                name = name.shown(),
            )));
        }
    }
    for (name, index) in &table.indexes {
        let what = format!("index `{name}`", name = name.shown(),);
        // Column keys are held to the table; an expression key to being an
        // expression at all, as a filter is (DEC-1169.2). An index of
        // expression keys alone names no column and is not empty.
        let keys: Vec<_> = index
            .columns
            .iter()
            .filter_map(|c| c.key.column().cloned())
            .collect();
        if index.columns.is_empty() || !keys.is_empty() {
            found.extend(key_columns(&what, &keys, table, false));
        }
        for key in &index.columns {
            if let Some(expression) = key.key.expression()
                && crate::LEXICON.expression_in(expression) == Expression::Absent
            {
                found.push(invalid(format!("{what} has an empty key expression")));
            }
        }
        for column in &index.include {
            if !table.columns.contains_key(column) {
                found.push(invalid(format!(
                    "{what} includes `{column}`, which is not a column of this table",
                    column = column.shown(),
                )));
            }
        }
        // The same question, and the same answer: measured, a partial index's
        // `WHERE` refuses a comment-only or whitespace-only expression exactly
        // as a check constraint does.
        if index
            .filter
            .as_ref()
            .is_some_and(|f| crate::LEXICON.expression_in(f) == Expression::Absent)
        {
            found.push(invalid(format!("{what} has an empty filter expression")));
        }
        found.extend(index_method(&what, index, table));
    }
    found
}

/// The access methods and operator classes this model holds (DEC-1169.1): the
/// B-tree under each type's default class, and GIN over `jsonb` under
/// `jsonb_ops`, its default, or `jsonb_path_ops`. Anything else would be
/// created as declared but read back as a limitation, so it is refused here
/// rather than applied and then refused. What each method may do is its row in
/// the model's policy table, and which classes are the engine's own is
/// [`crate::index_policy`]'s, shared with the pull (DEC-2448.1).
///
/// Measured on 18.6: `gin` refuses `UNIQUE` ("access method \"gin\" does not
/// support unique indexes"), `INCLUDE` and `ASC`/`DESC`, and an `int` column
/// has no default GIN class at all.
fn index_method(what: &str, index: &Index, table: &Table) -> Vec<DialectError> {
    use crate::index_policy::{ClassProblem, class_problem, holds_key_type, key_scope, offered};
    let policy = index.method.policy();
    let method = index.method.as_str().to_ascii_uppercase();
    let mut found = Vec::new();
    if index.unique && !policy.unique {
        found.push(invalid(format!(
            "{what} is a {method} index, which cannot be unique"
        )));
    }
    if !index.include.is_empty() && !policy.include {
        found.push(invalid(format!(
            "{what} is a {method} index, which cannot include payload columns"
        )));
    }
    for key in &index.columns {
        let shown = pbps_model::escape::Escaped(key.key.text());
        if key.descending && !policy.ordered {
            found.push(invalid(format!(
                "{what} orders `{shown}` descending, and a {method} index has no order"
            )));
        }
        let column = match key.key.column() {
            Some(column) => Some(column),
            None if !policy.expression_keys => {
                found.push(invalid(format!(
                    "{what} is a {method} index over the expression `{shown}`, and this model \
                     holds {method} only over {}",
                    key_scope(policy)
                )));
                continue;
            }
            None => None,
        };
        if let Some(column) = column {
            // A missing column is already named by `key_columns`.
            if table
                .columns
                .get(column)
                .is_some_and(|c| !holds_key_type(policy, c))
            {
                found.push(invalid(format!(
                    "{what} is a {method} index over `{column}`, and this model holds {method} \
                     only over {}",
                    key_scope(policy),
                    column = column.shown(),
                )));
            }
        }
        found.extend(key_collation(what, policy, &method, key, table));
        if let (Some(column), false) = (column, policy.typed_columns.is_empty()) {
            found.extend(typed_column(what, policy, &method, key, column, table));
        }
        let Some(class) = &key.opclass else { continue };
        let at = format!("`{shown}`");
        let named = class.shown();
        found.push(invalid(match class_problem(policy, class) {
            None => continue,
            // The B-tree's own wording: its direction words are the likelier slip.
            Some(ClassProblem::DefaultOnly) => format!(
                "{what} indexes {at} with operator class `{named}`; a B-tree index here \
                 takes only its type's default class, so leave the class out \
                 (or, if `{named}` was meant as a direction, it is `asc` or `desc`)"
            ),
            Some(ClassProblem::DefaultSpelledOut) => format!(
                "{what} names `{named}` for {at}, which is {method}'s default for \
                 `{}`; leave the class out",
                policy.key_type.unwrap_or("its key type")
            ),
            Some(ClassProblem::NotHeld) => format!(
                "{what} indexes {at} with operator class `{named}`; a {method} index \
                 over `{}` here takes `{}` or, by leaving it out, the \
                 default `{}`",
                policy.key_type.unwrap_or("its key type"),
                policy.classes.join("` or `"),
                policy.default_class.unwrap_or("its default"),
            ),
            Some(ClassProblem::Outside) => format!(
                "{what} indexes {at} with operator class `{named}`, which a schema owns; this \
                 model holds only the engine's own classes ({}) until a pinned provider vouches \
                 for one (#2447 PR F)",
                if policy.classes.is_empty() && policy.default_class.is_none() {
                    "none, for a B-tree".to_owned()
                } else {
                    offered(policy)
                }
            ),
        }));
    }
    found
}

/// What a key's collation may be (DEC-2448.2): held for a column key of a
/// method that collates, over a column the engine can collate, and never the
/// collation a key already has. Measured on 16.15 and 18.6: `COLLATE` on an
/// `integer`, `inet` or `int4range` key is "collations are not supported by
/// type ...", and `COLLATE "default"` on a default-collation column is stored
/// as no collation at all.
fn key_collation(
    what: &str,
    policy: &MethodPolicy,
    method: &str,
    key: &pbps_model::IndexColumn,
    table: &Table,
) -> Option<DialectError> {
    let collation = key.collation.as_deref()?;
    let shown = pbps_model::escape::Escaped(key.key.text());
    let named = pbps_model::escape::Escaped(collation);
    if !policy.collation {
        return Some(invalid(format!(
            "{what} collates `{shown}` by `{named}`, and a {method} index has no collation"
        )));
    }
    let Some(column) = key.key.column() else {
        return Some(invalid(format!(
            "{what} collates the expression `{shown}` by `{named}`; this model holds a collation \
             for a column key only, because the catalog does not tell an expression's own \
             collation from one written beside it"
        )));
    };
    if collation == "default" {
        return Some(invalid(format!(
            "{what} collates `{shown}` by `default`, which is what a key already has; leave the \
             collation out"
        )));
    }
    if collation.is_empty() || collation.contains(['.', '"', '\0']) {
        return Some(invalid(format!(
            "{what} collates `{shown}` by `{named}`, which is not the name of a `pg_catalog` \
             collation; a collation of another schema is not supported"
        )));
    }
    let ty = table
        .columns
        .get(column)
        .and_then(|c| crate::types::normalize(&c.ty).ok())?;
    // A type of the user's (a domain over `text`, an enum) is for the engine to
    // answer; a built-in one is collatable only if it is a string. An array is
    // collatable exactly when its element is: `text[]`, `varchar[]` and
    // `char[]` take a key collation, `int[]` is `42804` (measured on 16.15 and
    // 18.6).
    let element = ty.element();
    let collatable = element.type_ref().is_some()
        || matches!(
            element.base.as_str(),
            "text" | "character varying" | "character"
        );
    (!collatable).then(|| {
        invalid(format!(
            "{what} collates `{shown}` by `{named}`, and column `{column}` is of type `{}`, which \
             has no collation",
            pbps_model::escape::Escaped(&ty.base),
            column = column.shown(),
        ))
    })
}

/// A method held over a few column types only (GiST, DEC-2448.2): a column key
/// must be of one of them, and name the class the table says (`inet_ops` for
/// `inet`), or none where the type has a default. Every other type is indexed
/// through an expression.
fn typed_column(
    what: &str,
    policy: &MethodPolicy,
    method: &str,
    key: &pbps_model::IndexColumn,
    column: &ColumnName,
    table: &Table,
) -> Option<DialectError> {
    let ty = crate::types::normalize(&table.columns.get(column)?.ty).ok()?;
    let shown = pbps_model::escape::Escaped(key.key.text());
    let admitted = (!ty.is_array() && ty.type_ref().is_none())
        .then(|| {
            policy
                .typed_columns
                .iter()
                .find(|(base, _)| *base == ty.base)
        })
        .flatten();
    let Some((_, required)) = admitted else {
        return Some(invalid(format!(
            "{what} is a {method} index over `{shown}` of type `{}`, which has no {method} \
             operator class here; index an expression over it, such as `int4range(a, b)` \
             (this model holds {method} over a column of the types {} only)",
            pbps_model::escape::Escaped(&ty.base),
            policy
                .typed_columns
                .iter()
                .map(|(base, _)| format!("`{base}`"))
                .collect::<Vec<_>>()
                .join(", "),
        )));
    };
    let named = key.opclass.as_ref().and_then(|c| c.as_bare());
    match (required, named) {
        (Some(class), Some(n)) if *class == n => None,
        (Some(class), _) => Some(invalid(format!(
            "{what} indexes `{shown}` of type `{}` under {method}, which needs the class \
             `{class}`: name it with `opclass: {class}`",
            pbps_model::escape::Escaped(&ty.base),
        ))),
        (None, Some(_)) => Some(invalid(format!(
            "{what} names a class for `{shown}` of type `{}`, which takes its default under \
             {method}; leave the class out",
            pbps_model::escape::Escaped(&ty.base),
        ))),
        (None, None) => None,
    }
}

fn key_columns(
    what: &str,
    columns: &[ColumnName],
    table: &Table,
    distinct: bool,
) -> Vec<DialectError> {
    let mut found = Vec::new();
    if columns.is_empty() {
        found.push(invalid(format!("{what} names no columns")));
    }
    let mut seen = BTreeSet::new();
    for name in columns {
        if !table.columns.contains_key(name) {
            found.push(invalid(format!(
                "{what} references `{name}`, which is not a column of this table",
                name = name.shown(),
            )));
        }
        if distinct && !seen.insert(name) {
            found.push(invalid(format!(
                "{what} names `{name}` twice",
                name = name.shown(),
            )));
        }
    }
    found
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

/// The schema every principal can enter without holding a grant on it.
///
/// It is the same word, and for a related reason: `initdb` grants `USAGE` on
/// the schema `public` to PUBLIC in every database it makes. **Measured on
/// 18.6**, `pg_namespace.nspacl` for it is
/// `{pg_database_owner=UC/pg_database_owner,=U/pg_database_owner}` — the second
/// entry is PUBLIC's `USAGE`, and it is not `acldefault`'s doing
/// (`acldefault('n', ...)` is `{owner=UC/owner}` alone) but the state every
/// database starts in. A role holding only `SELECT` on `public.pubt` reads it
/// with no schema grant at all, measured.
///
/// So §1's rule — a grant in a schema the role cannot enter reaches nothing —
/// does not hold here, and applying it refused every project whose tables live
/// where PostgreSQL puts them by default, including the one `pull` writes from
/// such a database.
///
/// A DBA who revokes that `USAGE` makes this check say nothing where it would
/// have had something to say. That is the limit this check already has: `USAGE`
/// can also arrive through a membership, which is never declared, compared or
/// touched (ADR-0005), so what it catches is the ordinary mistake — a project's
/// own schema with no `usage` line — and not every unreachable grant there is.
/// What PUBLIC holds on a schema is reported by `pull` as context (ADR-0010
/// §5, DECISIONS 383).
const REACHABLE_WITHOUT_A_GRANT: &str = "public";

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
    Sequence,
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
            TargetKind::Sequence => &[Usage, Select, Update],
        }
    }

    const fn article(self) -> &'static str {
        match self {
            TargetKind::Table => "a table",
            TargetKind::View => "a view",
            TargetKind::Function => "a function",
            TargetKind::Procedure => "a procedure",
            TargetKind::Sequence => "a serial sequence",
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
    // Overloading is why `GrantTarget::Routine` exists (ADR-0009 §1), and a
    // bare name is not that identity — so on this engine a routine is granted
    // on by signature and by nothing else, however few overloads there are
    // today.
    //
    // **Not only the ambiguous case.** With one overload the engine takes the
    // bare `GRANT EXECUTE ON ROUTINE gr.f` happily, and the plan still never
    // converges: `pull` reads every routine grant back out of `pg_proc` as
    // `app.f(integer)` — the signature is all the catalog has — so a
    // declaration spelling it `app.f` differs from the state on every
    // comparison, and each plan revokes the signature and grants the bare name
    // again for ever. The mirror of this is refused on the other engine, where
    // nothing overloads and a signature is the spelling its catalog cannot
    // produce (`pbps_mssql::validate::role`).
    //
    // (DECISIONS 381.) The two overloads case says more, because there the
    // statement fails as well: measured, `GRANT EXECUTE ON ROUTINE gr.f` on an overloaded name is
    // `routine name "gr.f" is not unique`, with the engine's own hint to name
    // the argument list — after everything ordered before it has run.
    if !answering.is_empty() {
        let mut signatures: Vec<String> = answering
            .iter()
            .map(|(id, _)| id.raw().to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        signatures.sort();
        let signatures: Vec<_> = signatures
            .iter()
            .map(|signature| pbps_model::escape::Escaped(signature).to_string())
            .collect();
        return Err(invalid(if signatures.len() == 1 {
            format!(
                "`{object}` is a routine, which this engine identifies by its signature and not \
                 by its name (ADR-0009 section 1): `pull` reads the grant back as `{}`, so a declaration \
                 spelling it `{object}` would differ from the database on every plan and each \
                 plan would revoke and re-grant it. Write `{}` instead",
                signatures[0],
                signatures[0],
                object = object.shown()
            )
        } else {
            format!(
                "`{object}` names {} overloads, so PostgreSQL cannot tell which one this grant is \
                 on (`routine name \"{object}\" is not unique`); write the signature instead -- \
                 one of {}",
                answering.len(),
                signatures.join(", "),
                object = object.shown()
            )
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
pub fn role(name: &pbps_model::RoleName, role: &Role, schema: &Schema) -> Vec<DialectError> {
    let mut errs = Vec::new();
    if let Err(e) = quote(name.raw()) {
        errs.push(e);
    }
    if name.raw() == PUBLIC {
        errs.push(invalid(format!(
            "`{PUBLIC}` is PostgreSQL's name for every principal in the cluster, not a role: \
             `CREATE ROLE \"{PUBLIC}\"` is refused as reserved, and a `GRANT ... TO \"{PUBLIC}\"` \
             -- quoted, as this emitter writes every name -- opens the object to all of them. \
             Declare a role of your own and grant to that; what PUBLIC holds is reported by \
             `pull` as context and never managed (ADR-0010 section 5)"
        )));
    }
    // `pg_` is the engine's own prefix: measured, `CREATE ROLE pg_thing` is
    // `role name "pg_thing" is reserved`. Nothing here creates a role
    // (`manages_roles` is false), so the refusal is not about the `CREATE` —
    // it is that the sixteen `pg_*` roles are the cluster's predefined ones,
    // whose membership and grants are the DBA's and not this project's.
    if name.raw().starts_with("pg_") {
        errs.push(invalid(format!(
            "`{name}` is in PostgreSQL's reserved `pg_` namespace, which holds the cluster's \
             predefined roles; the engine refuses `CREATE ROLE` on such a name, and what those \
             roles are granted is the cluster's business rather than this database's \
             (ADR-0010 section 3)",
            name = name.shown(),
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
            GrantTarget::Object(_) | GrantTarget::Routine(_) | GrantTarget::Sequence(_) => None,
        })
        .collect();

    for (target, permissions) in &role.grants {
        let parts: Vec<&str> = match target {
            GrantTarget::Object(o) => vec![&o.schema, &o.name],
            GrantTarget::Routine(r) => vec![&r.name.schema, &r.name.name],
            GrantTarget::Sequence(c) => vec![&c.table.schema, &c.table.name, c.name.raw()],
            GrantTarget::Schema(s) => vec![s],
        };
        for part in parts {
            if let Err(e) = quote(part) {
                errs.push(e);
            }
        }
        // A schema the pull does not read. The engine takes the grant —
        // measured, `GRANT USAGE ON SCHEMA information_schema TO r` runs — and
        // the reader skips those schemas because they are the engine's own and
        // not a project's (`crate::catalog::a_projects_schema`). The grant
        // would then come back as absent: the apply's own read-back would
        // refuse it for not having achieved what it asked, and every plan after
        // it would propose the same `GRANT` again. Refused here, where the
        // remedy is a line in a file.
        let schema_of = target.schema();
        // The name that fails somewhere else entirely, and the reason a grant
        // needs the check a table already has: `$user` is what this engine
        // substitutes for the current role's own schema wherever a
        // `search_path` names it, quoted or not
        // (`emit::NOT_A_SCHEMA_A_PATH_CAN_NAME`). Every `Grant` and `Revoke`
        // is emitted through `scoped`, whose `write_path` refuses that name —
        // so the declaration passes this command and stops `plan` instead,
        // one command later than the remedy is (#272).
        if schema_of == crate::emit::NOT_A_SCHEMA_A_PATH_CAN_NAME {
            errs.push(invalid(format!(
                "role `{name}`: `{target}` is in a schema named `{schema_of}`, which this \
                 engine reads as the current role's own schema wherever a `search_path` names \
                 it -- quoting does not make it literal. Every grant this dialect emits sets \
                 the path first, so no plan can carry this one. Grant in a schema the path \
                 can name",
                target = target.shown(),
                name = name.shown(),
            )));
            continue;
        }
        if !crate::catalog::a_projects_schema(schema_of) {
            errs.push(invalid(format!(
                "role `{name}`: `{target}` is in schema `{schema_of}`, which is the engine's \
                 own and not a project's: `pg_catalog`, `information_schema` and every `pg_` \
                 schema are left out of the pull, so a grant there reads back as absent and \
                 every plan would propose it again (DECISIONS 385). Grant in a schema this \
                 project declares",
                target = target.shown(),
                name = name.shown(),
                schema_of = pbps_model::escape::Escaped(schema_of),
            )));
            continue;
        }
        // A word the model spells for the other engine (§6). Refused by name,
        // on any target — the engine's parser stops at the word before it
        // looks at the target — and left out of the kind check below, which
        // would otherwise report the same grant twice.
        for p in permissions
            .iter()
            .filter(|p| **p != Permission::SerialSequences && !has_permission(**p))
        {
            errs.push(invalid(format!(
                "role `{name}`: `{}` on `{target}` is not a permission PostgreSQL has; it is SQL \
                 Server's (ADR-0010 section 6), and this engine takes {}",
                p.as_str(),
                permission_words(),
                target = target.shown(),
                name = name.shown(),
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
                         afterwards (ADR-0010 section 2). Grant `{}` on each object instead, and keep \
                         `schema::{s}: [usage]` so the role can reach them",
                        p.as_str(),
                        p.as_str().to_ascii_uppercase(),
                        SCHEMA_PERMISSIONS
                            .iter()
                            .map(|p| p.as_str())
                            .collect::<Vec<_>>()
                            .join(" and "),
                        p.as_str(),

                        target = target.shown(),
                        name = name.shown(),
                        s = pbps_model::escape::Escaped(s),
                    )));
                }
            }
            GrantTarget::Object(_) | GrantTarget::Routine(_) | GrantTarget::Sequence(_) => {
                let schema_of = target.schema();
                // §1, and it is checked once per target rather than once per
                // permission: a grant with three permissions on a schema the
                // role cannot enter is one mistake.
                if !usable.contains(schema_of) && schema_of != REACHABLE_WITHOUT_A_GRANT {
                    errs.push(invalid(format!(
                        "role `{name}`: `{target}` is granted in schema `{schema_of}`, which this \
                         role has no `usage` on -- PostgreSQL checks the schema before the object, \
                         so every permission here reaches nothing and the role's own query is \
                         `permission denied for schema {schema_of}` (ADR-0010 section 1, measured). Add \
                         `schema::{schema_of}: [usage]` to this role",
                        target = target.shown(),
                        name = name.shown(),
                        schema_of = pbps_model::escape::Escaped(schema_of),
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
                    GrantTarget::Sequence(_) => Some(TargetKind::Sequence),
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
                            .join(", "),
                        target = target.shown(),
                        name = name.shown(),
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
    use pbps_model::{IndexMethod, Module, ModuleKind, RoutineArg, RoutineId, Table};

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
                    options: Default::default(),
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
        super::role(&pbps_model::RoleName::from(name), role, &declarations())
    }

    /// `baseline` removes a staged attach's temporary CHECK by its exact
    /// name, so no declared constraint may take the prefix that name has
    /// (DEC-1638.1): a check would be removed with it, and any kind would
    /// fail the attach's `ADD`, in the namespace they share.
    #[test]
    fn a_declared_constraint_cannot_take_the_staged_attachs_name() {
        let refused = |kind: &str, name: &str| {
            let mut table = Table::default();
            for column in ["n", "m"] {
                table.columns.insert(
                    column.into(),
                    pbps_model::Column::new("integer".parse().unwrap()),
                );
            }
            if kind == "primary key" {
                table.primary_key = Some(pbps_model::PrimaryKey {
                    name: Some(name.into()),
                    columns: vec!["n".into()],
                    storage_parameters: Default::default(),
                });
            } else if kind == "unique constraint" {
                table.unique.insert(
                    name.into(),
                    pbps_model::UniqueConstraint {
                        columns: vec!["m".into()],
                        storage_parameters: Default::default(),
                    },
                );
            } else if kind == "foreign key" {
                table.foreign_keys.insert(
                    name.into(),
                    pbps_model::ForeignKey {
                        columns: vec!["m".into()],
                        references_table: "app.other".parse().unwrap(),
                        references_columns: vec!["id".into()],
                        on_delete: Default::default(),
                        on_update: Default::default(),
                    },
                );
            } else {
                table.checks.insert(
                    name.into(),
                    pbps_model::CheckConstraint {
                        expression: "n > 0".into(),
                    },
                );
            }
            table_structure(&table).iter().any(|e| {
                let said = e.to_string();
                said.contains(&format!("{kind} `{name}`"))
                    && said.contains("keeps for the temporary CHECK")
            })
        };
        for kind in [
            "primary key",
            "unique constraint",
            "foreign key",
            "check constraint",
        ] {
            assert!(refused(kind, "pbps_attach_t_01"), "{kind}");
            assert!(refused(kind, "pbps_attach_"), "{kind}");
            // Negative: the prefix elsewhere in the name, or spelled
            // otherwise, is another name to the engine, and never one pbps
            // writes.
            for name in [
                "ck_pbps_attach_t",
                "PBPS_ATTACH_t",
                "pbps_attach",
                "pbps_ck",
            ] {
                assert!(!refused(kind, name), "{kind} {name}");
            }
        }
    }

    /// A table with a `jsonb` and an `integer` column, and one index over the
    /// keys given, each `(column, opclass, descending)`.
    fn indexed(method: IndexMethod, keys: &[(&str, Option<&str>, bool)]) -> Table {
        let mut table = Table::default();
        table.columns.insert(
            "body".into(),
            pbps_model::Column::new("jsonb".parse().unwrap()),
        );
        table.columns.insert(
            "n".into(),
            pbps_model::Column::new("integer".parse().unwrap()),
        );
        table.indexes.insert(
            "ix".into(),
            Index {
                columns: keys
                    .iter()
                    .map(|(name, class, descending)| pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Column((*name).into()),
                        descending: *descending,
                        opclass: class.map(|c| c.parse().unwrap()),
                        collation: None,
                    })
                    .collect(),
                include: Vec::new(),
                unique: false,
                filter: None,
                method,
                storage_parameters: Default::default(),
            },
        );
        table
    }

    fn structure(table: &Table) -> Vec<String> {
        table_structure(table)
            .into_iter()
            .map(|e| e.to_string())
            .collect()
    }

    /// A table with one column of each type the GiST and collation rules look
    /// at, and one index of `method` over `keys`, each `(column or expression,
    /// opclass, collation)`.
    fn gist_table(method: IndexMethod, keys: &[(&str, Option<&str>, Option<&str>)]) -> Table {
        let mut table = Table::default();
        for (name, ty) in [
            ("ip", "inet"),
            ("net", "cidr"),
            ("doc", "tsvector"),
            ("q", "tsquery"),
            ("n", "integer"),
            ("body", "text"),
            ("tags", "text[]"),
            ("vtags", "varchar(5)[]"),
            ("ctags", "char(3)[]"),
            ("nums", "integer[]"),
        ] {
            table
                .columns
                .insert(name.into(), pbps_model::Column::new(ty.parse().unwrap()));
        }
        table.indexes.insert(
            "ix".into(),
            Index {
                columns: keys
                    .iter()
                    .map(|(key, class, collation)| pbps_model::IndexColumn {
                        key: if table.columns.contains_key(*key) {
                            pbps_model::IndexKey::Column((*key).into())
                        } else {
                            pbps_model::IndexKey::Expression((*key).to_owned())
                        },
                        descending: false,
                        opclass: class.map(|c| c.parse().unwrap()),
                        collation: collation.map(str::to_owned),
                    })
                    .collect(),
                include: Vec::new(),
                unique: false,
                filter: None,
                method,
                storage_parameters: Default::default(),
            },
        );
        table
    }

    /// GiST over the column types it is held over, each under the class the
    /// engine needs, and over any expression (DEC-2448.2; measured: `inet` and
    /// `cidr` have no default class, `tsvector` and `tsquery` do).
    #[test]
    fn a_gist_key_is_a_held_column_type_under_its_class_or_an_expression() {
        use IndexMethod::Gist;
        for keys in [
            vec![("doc", None, None)],
            vec![("q", None, None)],
            vec![("ip", Some("inet_ops"), None)],
            vec![("net", Some("pg_catalog.inet_ops"), None)],
            vec![("int4range(n, n + 1)", None, None)],
            vec![("int4range(n, n + 1)", None, None), ("doc", None, None)],
        ] {
            assert!(structure(&gist_table(Gist, &keys)).is_empty(), "{keys:?}");
        }
    }

    /// Negative: a column of any other type, `inet` without its class, a class
    /// on a type with a default, the default spelled out, and a class of
    /// another schema are each refused by name, the first with the advice to
    /// index an expression.
    #[test]
    fn a_gist_key_the_engine_has_no_class_for_is_refused_by_name() {
        use IndexMethod::Gist;
        let said = |keys: &[(&str, Option<&str>, Option<&str>)]| {
            structure(&gist_table(Gist, keys)).join("\n")
        };
        let int = said(&[("n", None, None)]);
        assert!(
            int.contains("`integer`") && int.contains("index an expression"),
            "{int}"
        );
        assert!(said(&[("tags", None, None)]).contains("index an expression"));
        let bare = said(&[("ip", None, None)]);
        assert!(bare.contains("needs the class `inet_ops`"), "{bare}");
        let on_default = said(&[("doc", Some("inet_ops"), None)]);
        assert!(on_default.contains("leave the class out"), "{on_default}");
        let spelled = said(&[("int4range(n, n)", Some("range_ops"), None)]);
        assert!(spelled.contains("`range_ops`"), "{spelled}");
        let outside = said(&[("ip", Some("ext.inet_ops"), None)]);
        assert!(outside.contains("a schema owns"), "{outside}");
        // GiST is never unique, ordered, or INCLUDEs an expression's payload.
        let mut unique = gist_table(Gist, &[("doc", None, None)]);
        unique.indexes.get_mut("ix").unwrap().unique = true;
        assert!(structure(&unique).join("\n").contains("cannot be unique"));
        let mut desc = gist_table(Gist, &[("doc", None, None)]);
        desc.indexes.get_mut("ix").unwrap().columns[0].descending = true;
        assert!(structure(&desc).join("\n").contains("has no order"));
    }

    /// A column key may be collated by a `pg_catalog` collation on a column the
    /// engine can collate; every other case is refused by name (DEC-2448.2;
    /// measured on 16.15 and 18.6: `COLLATE` on `integer`, `inet`, `int4range`
    /// is "collations are not supported by type ...").
    #[test]
    fn a_key_collation_is_held_for_a_collatable_column_key_only() {
        use IndexMethod::{Btree, Gin, Gist};
        let said = |m, keys: &[(&str, Option<&str>, Option<&str>)]| {
            structure(&gist_table(m, keys)).join("\n")
        };
        assert!(said(Btree, &[("body", None, Some("C"))]).is_empty());
        let int = said(Btree, &[("n", None, Some("C"))]);
        assert!(int.contains("has no collation"), "{int}");
        let ip = said(Gist, &[("ip", Some("inet_ops"), Some("C"))]);
        assert!(
            ip.contains("`inet`") && ip.contains("has no collation"),
            "{ip}"
        );
        // An array is collatable by its element (measured: `text[]`,
        // `varchar[]` and `char[]` build under `COLLATE "C"`, `int[]` is 42804).
        for column in ["tags", "vtags", "ctags"] {
            assert!(
                said(Btree, &[(column, None, Some("C"))]).is_empty(),
                "{column}"
            );
        }
        // Still refused for GiST: `text[]` has no GiST class, whatever its
        // collation, and the advice is the existing one.
        let gist = said(Gist, &[("tags", None, Some("C"))]);
        assert!(gist.contains("index an expression"), "{gist}");
        let array = said(Btree, &[("nums", None, Some("C"))]);
        assert!(
            array.contains("has no collation") && array.contains("`integer`"),
            "{array}"
        );
        let expression = said(Btree, &[("lower(body)", None, Some("C"))]);
        assert!(expression.contains("column key only"), "{expression}");
        let default = said(Btree, &[("body", None, Some("default"))]);
        assert!(default.contains("leave the collation out"), "{default}");
        let schema = said(Btree, &[("body", None, Some("other.mine"))]);
        assert!(
            schema.contains("not the name of a `pg_catalog`"),
            "{schema}"
        );
        let gin = said(Gin, &[("body", None, Some("C"))]);
        assert!(gin.contains("has no collation"), "{gin}");
    }

    /// GIN over `jsonb` is accepted with its default class or
    /// `jsonb_path_ops`, filtered or not; everything the engine refuses for
    /// GIN, or this model would read back as a limitation, is refused before
    /// it is applied (DEC-1169.1).
    #[test]
    fn a_gin_index_is_held_to_jsonb_and_to_what_the_engine_accepts() {
        use IndexMethod::Gin;
        assert!(structure(&indexed(Gin, &[("body", None, false)])).is_empty());
        assert!(structure(&indexed(Gin, &[("body", Some("jsonb_path_ops"), false)])).is_empty());
        let mut filtered = indexed(Gin, &[("body", None, false)]);
        filtered.indexes.get_mut("ix").unwrap().filter = Some("n > 0".into());
        assert!(structure(&filtered).is_empty());

        let refused = |table: Table, expected: &str| {
            let found = structure(&table);
            assert!(
                found.iter().any(|m| m.contains(expected)),
                "expected `{expected}` in {found:?}"
            );
        };
        refused(indexed(Gin, &[("n", None, false)]), "only over `jsonb`");
        refused(indexed(Gin, &[("body", None, true)]), "has no order");
        refused(
            indexed(Gin, &[("body", Some("jsonb_ops"), false)]),
            "leave the class out",
        );
        refused(
            indexed(Gin, &[("body", Some("gin_trgm_ops"), false)]),
            "takes `jsonb_path_ops`",
        );
        let mut unique = indexed(Gin, &[("body", None, false)]);
        unique.indexes.get_mut("ix").unwrap().unique = true;
        refused(unique, "cannot be unique");
        let mut include = indexed(Gin, &[("body", None, false)]);
        include.indexes.get_mut("ix").unwrap().include = vec!["n".into()];
        refused(include, "cannot include");
    }

    /// An expression key is held on a B-tree, and refused on GIN, empty, or
    /// with a class; an index of expression keys alone names no column and
    /// is not "empty" (DEC-1169.2).
    #[test]
    fn an_expression_key_is_held_to_a_btree_and_to_being_an_expression() {
        let with_key = |method: IndexMethod, text: &str, class: Option<&str>| {
            let mut table = indexed(method, &[("body", None, false)]);
            table.indexes.get_mut("ix").unwrap().columns = vec![pbps_model::IndexColumn {
                key: pbps_model::IndexKey::Expression(text.into()),
                descending: false,
                opclass: class.map(|c| c.parse().unwrap()),
                collation: None,
            }];
            structure(&table)
        };
        assert!(with_key(IndexMethod::Btree, "lower(body::text)", None).is_empty());
        let refused = |found: Vec<String>, expected: &str| {
            assert!(
                found.iter().any(|m| m.contains(expected)),
                "expected `{expected}` in {found:?}"
            );
        };
        refused(
            with_key(IndexMethod::Gin, "body", None),
            "over the expression",
        );
        refused(
            with_key(IndexMethod::Btree, " -- nothing", None),
            "empty key expression",
        );
        // A non-breaking space is an identifier to this engine, not blank
        // (DECISIONS 504): not refused as empty.
        let nbsp = with_key(IndexMethod::Btree, "\u{a0}", None);
        assert!(
            !nbsp.iter().any(|m| m.contains("empty key expression")),
            "{nbsp:?}"
        );
        refused(
            with_key(
                IndexMethod::Btree,
                "lower(body::text)",
                Some("text_pattern_ops"),
            ),
            "operator class",
        );
    }

    /// A B-tree takes no operator class here, and a class that looks like a
    /// misspelt direction says so (DEC-1169.1); without one it is unchanged.
    #[test]
    fn a_btree_index_takes_no_operator_class() {
        assert!(structure(&indexed(IndexMethod::Btree, &[("n", None, true)])).is_empty());
        let found = structure(&indexed(IndexMethod::Btree, &[("n", Some("dsc"), false)]));
        assert!(
            found
                .iter()
                .any(|m| m.contains("operator class `dsc`") && m.contains("`asc` or `desc`")),
            "{found:?}"
        );
    }

    /// A class a schema owns is parsed by the loader and refused here, by
    /// name, on every method and key shape, until a pinned provider vouches
    /// for it (#2447 PR F); the engine's own spelling `pg_catalog.x` is the
    /// bare class and gets the bare class's verdict (DEC-2448.1).
    #[test]
    fn a_class_a_schema_owns_is_refused_by_name_and_pg_catalog_is_not_one() {
        let refused = |table: Table, expected: &[&str]| {
            let found = structure(&table);
            assert!(
                found.iter().any(|m| expected.iter().all(|e| m.contains(e))),
                "expected {expected:?} in {found:?}"
            );
        };
        refused(
            indexed(IndexMethod::Btree, &[("n", Some("ext.int4_ops"), false)]),
            &["`ext.int4_ops`", "a schema owns", "#2447 PR F"],
        );
        refused(
            indexed(
                IndexMethod::Gin,
                &[("body", Some("ext.jsonb_path_ops"), false)],
            ),
            &["`ext.jsonb_path_ops`", "#2447 PR F", "`jsonb_ops`"],
        );
        // The same name in `pg_catalog` is the bare class, and held.
        assert!(
            structure(&indexed(
                IndexMethod::Gin,
                &[("body", Some("pg_catalog.jsonb_path_ops"), false)]
            ))
            .is_empty()
        );
        // Negative: the bare btree class stays refused, with its own wording.
        refused(
            indexed(
                IndexMethod::Btree,
                &[("n", Some("pg_catalog.int4_ops"), false)],
            ),
            &["takes only its type's default class"],
        );
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
            ("app.solo(integer)", &[Permission::Execute]),
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
        // A table word on the bare name is the table's, and there is nothing
        // wrong with it.
        let problems = messages("app_reader", &both(&[Permission::Select]));
        assert!(problems.is_empty(), "{problems:?}");
        // `execute` chooses the other namespace, and there the bare name is
        // not an identity at all: refused with the signature `pull` will
        // write. The message is the routine's, not `execute does not apply to
        // a table`.
        let problems = messages("app_reader", &both(&[Permission::Execute]));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("app.both(integer)"), "{}", problems[0]);
        // And a mixed set is the routine's too — `execute` is what chooses
        // the namespace — so it is refused as one bare routine target rather
        // than reported as `truncate does not apply to a function`: the
        // spelling is what is wrong with it, and the kind check never runs.
        let problems = messages(
            "app_reader",
            &both(&[Permission::Execute, Permission::Truncate]),
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("app.both(integer)"), "{}", problems[0]);
    }

    /// A schema the pull does not read is a schema a declaration may not name.
    ///
    /// The engine takes the grant — measured, `GRANT USAGE ON SCHEMA
    /// information_schema TO r` runs — and the reader leaves those schemas out
    /// because they are the engine's own. The grant would come back as absent,
    /// the apply's own read-back would refuse it, and every plan after it would
    /// propose the same `GRANT` again.
    #[test]
    fn a_schema_the_pull_does_not_read_is_refused_as_a_target() {
        for target in [
            "schema::information_schema",
            "schema::pg_catalog",
            "schema::pg_toast",
            "information_schema.tables",
        ] {
            let role = granting(&[
                ("schema::app", &[Permission::Usage]),
                (target, &[Permission::Usage]),
            ]);
            let problems = messages("app_reader", &role);
            assert_eq!(problems.len(), 1, "{target}: {problems:?}");
            assert!(
                problems[0].contains("the engine's own"),
                "{target}: {}",
                problems[0]
            );
        }
        // A schema whose name merely starts with `p` is a project's.
        let role = granting(&[("schema::pga", &[Permission::Usage])]);
        assert!(messages("app_reader", &role).is_empty());
    }

    /// The one schema §1 does not apply to. **Measured on 18.6**: a role
    /// holding `SELECT` on `public.pubt` and nothing else reads it, because
    /// `initdb` grants `USAGE` on `public` to PUBLIC in every database
    /// (`nspacl` is `{pg_database_owner=UC/pg_database_owner,=U/pg_database_owner}`).
    /// Requiring the usage line there refused every project whose tables live
    /// where PostgreSQL puts them, and refused the project `pull` writes from
    /// such a database — which no `schema::public: [usage]` grant would appear
    /// in, because PUBLIC is not a role that can be declared.
    #[test]
    fn the_public_schema_needs_no_usage_line_to_be_reachable() {
        let mut schema = declarations();
        schema.tables.insert(
            "public.pubt".parse().expect("a table name parses"),
            Table::default(),
        );
        let mut role = Role::default();
        role.grants.insert(
            "public.pubt".parse().expect("a grant target parses"),
            [Permission::Select].into_iter().collect(),
        );
        let problems: Vec<String> =
            super::role(&pbps_model::RoleName::from("app_reader"), &role, &schema)
                .into_iter()
                .map(|e| e.to_string())
                .collect();
        assert!(problems.is_empty(), "{problems:?}");
        // And a schema that is not `public` still needs it.
        let mut role = Role::default();
        role.grants.insert(
            "app.customer".parse().expect("a grant target parses"),
            [Permission::Select].into_iter().collect(),
        );
        let problems: Vec<String> =
            super::role(&pbps_model::RoleName::from("app_reader"), &role, &schema)
                .into_iter()
                .map(|e| e.to_string())
                .collect();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("`usage`"), "{}", problems[0]);
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

    /// Issue #272. `$user` is not a schema name here: this engine substitutes
    /// the current role's own schema for it wherever a `search_path` names
    /// it, and every grant this dialect emits sets the path first. The
    /// offline command took the declaration and `plan` stopped on it — the
    /// right answer, one command later than the remedy is.
    ///
    /// # Which spellings are the hazard, measured
    ///
    /// A grant target keeps the quoting it was written with: `$user.t` and
    /// `schema::$user` name the schema `$user`, while `"$user".t` names a
    /// schema whose name *contains* the quote characters. Only the first two
    /// reach the substitution — `quote` renders `$user` as `"$user"`, which
    /// is the token a `search_path` replaces, and renders the quoted name as
    /// `"""$user"""`, which is not. So the bare spellings are refused and the
    /// odd-but-distinct name is left alone, along with every schema that
    /// merely has the word in it.
    #[test]
    fn a_grant_target_in_a_schema_a_path_cannot_name_is_refused() {
        for target in ["schema::$user", "$user.t", "$user.f(integer)"] {
            let role = granting(&[(target, &[Permission::Usage, Permission::Select])]);
            let problems = messages("app_reader", &role);
            assert_eq!(problems.len(), 1, "{target}: {problems:?}");
            assert!(problems[0].contains("$user"), "{target}: {}", problems[0]);
            assert!(
                problems[0].contains("the current role's own schema"),
                "{target}: {}",
                problems[0]
            );
            // One message, not that one plus the kind and permission checks
            // below it: the target is unusable, so there is nothing further
            // to say about it.
            assert!(
                !problems[0].contains("DECISIONS 385"),
                "{target}: {}",
                problems[0]
            );
        }
        // Neither a schema that merely has the word in its name, nor the one
        // whose name holds the quote characters, is the engine's token.
        for target in ["schema::user", "schema::\"$user\""] {
            let role = granting(&[(target, &[Permission::Usage])]);
            let problems = messages("app_reader", &role);
            assert!(
                !problems.iter().any(|m| m.contains("a `search_path` names")),
                "{target}: {problems:?}"
            );
        }
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
            ("app.solo(integer)", Permission::Usage, "a function"),
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

    /// ADR-0009 §1 again, and the case the engine itself accepts: with one
    /// overload `GRANT EXECUTE ON ROUTINE app.solo` runs. The plan still never
    /// converges — `pull` has only `pg_proc` to read a routine grant out of,
    /// so it comes back as `app.solo(integer)` every time, differs from a
    /// declaration spelling it `app.solo`, and every plan revokes the
    /// signature and grants the bare name again.
    ///
    /// The mirror image is refused on the other engine, where nothing
    /// overloads and the signature is the spelling its catalog cannot produce
    /// (`pbps_mssql::validate::role`). One spelling per engine, and it is the
    /// one that survives a round trip.
    #[test]
    fn a_bare_routine_name_is_refused_with_the_signature_the_pull_will_write() {
        let role = granting(&[
            ("schema::app", &[Permission::Usage]),
            ("app.solo", &[Permission::Execute]),
        ]);
        let problems = messages("app_reader", &role);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("app.solo(integer)"), "{}", problems[0]);
        // And the signature it names is accepted.
        let role = granting(&[
            ("schema::app", &[Permission::Usage]),
            ("app.solo(integer)", &[Permission::Execute]),
        ]);
        assert!(messages("app_reader", &role).is_empty());
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
    fn serial_sequence_targets_admit_only_sequence_words_and_need_schema_usage() {
        for permission in Permission::ALL {
            let role = granting(&[
                ("schema::app", &[Permission::Usage]),
                ("sequence::app.t.id", &[permission]),
            ]);
            let errors = messages("reader", &role);
            assert_eq!(
                errors.is_empty(),
                [Permission::Usage, Permission::Select, Permission::Update].contains(&permission),
                "{permission}: {errors:?}"
            );
            if !errors.is_empty() {
                assert!(
                    errors.iter().any(
                        |e| e.contains(permission.as_str()) && e.contains("sequence::app.t.id")
                    )
                );
            }
        }
        let errors = messages(
            "reader",
            &granting(&[("sequence::app.t.id", &[Permission::Usage])]),
        );
        assert!(
            errors.iter().any(|e| e.contains("schema::app")),
            "{errors:?}"
        );
    }

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
