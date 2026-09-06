//! What SQL Server will refuse, checked before anything is generated.
//!
//! These are the errors worth catching in `pbps validate`, where the user sees
//! the file and the line, rather than at apply time as a message from the server
//! about a table it half-created. Every check here is a rule of the engine, not a
//! matter of taste — style opinions belong in `fmt`, not in an error.

use pbps_dialect::DialectError;
use pbps_model::{
    GrantTarget, Module, ModuleId, ModuleKind, ObjectName, Permission, Role, Schema, Table,
    TableName, Value,
};

use crate::ident;
use crate::rows::ValueKind;
use crate::types::{self, DIALECT};

/// SQL Server's limit on the number of key columns in one index.
const MAX_INDEX_KEY_COLUMNS: usize = 32;

fn invalid(message: impl Into<String>) -> DialectError {
    DialectError::Invalid {
        dialect: DIALECT,
        message: message.into(),
    }
}

/// The permissions SQL Server has, among the words the model spells
/// (ADR-0010 §6, DECISIONS 210). Measured on SQL Server 2025:
/// `sys.fn_builtin_permissions(DEFAULT)` names none of the other five —
/// `usage`, `create`, `truncate`, `trigger`, `maintain` — in any class, and
/// `GRANT USAGE ON dbo.t TO r` is not even a failed grant but a parse error
/// (Msg 102, "Incorrect syntax near 'USAGE'"), the same for each of the five
/// on an object and on a schema. Three places apply this one table:
/// `validate` refuses the word, `emit` will not render it, and the catalog
/// read-back reports it rather than fold it — so a word the model holds and
/// this engine lacks cannot reach a statement or a declaration by any path.
pub(crate) const PERMISSIONS: [Permission; 8] = [
    Permission::Select,
    Permission::Insert,
    Permission::Update,
    Permission::Delete,
    Permission::References,
    Permission::Execute,
    Permission::Alter,
    Permission::ViewDefinition,
];

/// Whether SQL Server has `p` at all, on any securable.
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

/// Every problem with a role (ADR-0005): the names it uses have to be ones
/// this dialect can write into `GRANT` and `CREATE ROLE`, each permission has
/// to be one this engine has at all (ADR-0010 §6), and then one the engine
/// defines on what the target *is*. `public` and the fixed database roles are
/// the engine's own and cannot be created, dropped or renamed; declaring one
/// would plan a statement the engine refuses.
pub fn role(name: &str, role: &Role, schema: &Schema) -> Vec<DialectError> {
    let mut errs = Vec::new();
    if let Err(e) = ident::quote(name) {
        errs.push(e);
    }
    if FIXED_ROLES.iter().any(|f| f.eq_ignore_ascii_case(name)) {
        errs.push(invalid(format!(
            "`{name}` is a built-in database role, which cannot be created, dropped or renamed; \
             declare a role of your own and grant to that"
        )));
    }
    for (target, permissions) in &role.grants {
        let parts: Vec<&str> = match target {
            GrantTarget::Object(o) => vec![&o.schema, &o.name],
            // Nothing overloads here, so a grant that names a signature names
            // a securable this engine cannot resolve (ADR-0009 §1). Refused
            // rather than narrowed to the bare name: the two spellings would
            // mean the same object on this dialect and different ones on
            // another, and quietly picking is how a grant lands on the wrong
            // overload the day a project moves.
            GrantTarget::Routine(r) => {
                errs.push(invalid(format!(
                    "role `{name}`: `{r}` names an argument list; SQL Server identifies a routine \
                     by name alone, so grant on `{}` instead",
                    r.name
                )));
                vec![&r.name.schema, &r.name.name]
            }
            GrantTarget::Schema(s) => vec![s],
        };
        for part in parts {
            if let Err(e) = ident::quote(part) {
                errs.push(e);
            }
        }
        // A word the model spells for the other engine (ADR-0010 §6). Refused
        // by name, on any target — the engine's parser stops at the word (Msg
        // 102) before it looks at the securable — and left out of the kind
        // check below, which would otherwise report the same grant twice.
        for p in permissions.iter().filter(|p| !has_permission(**p)) {
            errs.push(invalid(format!(
                "role `{name}`: `{}` on `{target}` is not a permission SQL Server has; it is                  PostgreSQL's (ADR-0010 §6), and this engine takes {}",
                p.as_str(),
                permission_words()
            )));
        }
        // `GRANT EXECUTE` on a table, `GRANT SELECT` on a procedure: the
        // engine refuses each (Msg 4606), and in a staged apply the grants
        // run after the other changes, so it would refuse it on a database
        // already changed. The permission is checked against the kind of the
        // target here, where nothing has run. An object the declarations do
        // not have is the model's finding, not this one's.
        let GrantTarget::Object(object) = target else {
            continue;
        };
        let Some(kind) = target_kind(object, schema) else {
            continue;
        };
        let Some(applicable) = kind.permissions() else {
            errs.push(invalid(format!(
                "grants on `{object}`, a trigger, which takes no permission at all: the engine \
                 has none on triggers — grant on the table it fires on instead"
            )));
            continue;
        };
        for p in permissions.iter().filter(|p| has_permission(**p)) {
            if !applicable.contains(p) {
                errs.push(invalid(format!(
                    "`{}` does not apply to `{object}`, {}: the engine refuses that GRANT; {} \
                     takes {}",
                    p.as_str(),
                    kind.article(),
                    kind.article(),
                    applicable
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
    }
    errs
}

/// The values an integer type holds, for the ones smaller than the model's
/// `i64`. A `bigint` holds every `i64`; a `tinyint` is unsigned and holds
/// none of the negatives, which is the surprise worth naming in the message
/// (DECISIONS 104).
fn int_range(base: &str) -> Option<(i128, i128)> {
    match base {
        "tinyint" => Some((0, 255)),
        "smallint" => Some((i128::from(i16::MIN), i128::from(i16::MAX))),
        "int" => Some((i128::from(i32::MIN), i128::from(i32::MAX))),
        _ => None,
    }
}

/// Why a key's text cannot possibly be read as `base`, or `None` when it
/// might be. Conservative on purpose: this refuses only shapes no spelling
/// of the type has — letters in a number, a GUID of the wrong length — and
/// leaves the rest to the engine, which is asked before anything is written
/// (DECISIONS 101, 103). A text type takes anything.
fn key_shape(base: &str, text: &str) -> Option<&'static str> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let fits = match base {
        "decimal" | "numeric" | "money" | "smallmoney" | "float" | "real" => {
            let n = text.strip_prefix(['-', '+']).unwrap_or(text);
            let (mantissa, exponent) = match n.split_once(['e', 'E']) {
                Some((m, e)) => (m, Some(e)),
                None => (n, None),
            };
            let mantissa_ok = match mantissa.split_once('.') {
                Some((i, f)) => {
                    (i.is_empty() || digits(i))
                        && (f.is_empty() || digits(f))
                        && !(i.is_empty() && f.is_empty())
                }
                None => digits(mantissa),
            };
            mantissa_ok && exponent.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e)))
        }
        "date" | "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" | "time" => {
            text.bytes().any(|b| b.is_ascii_digit())
                && text
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b" -:T./+Z".contains(&b))
        }
        "uniqueidentifier" => {
            let inner = text
                .strip_prefix('{')
                .and_then(|t| t.strip_suffix('}'))
                .unwrap_or(text);
            let hex = |s: &str| s.bytes().all(|b| b.is_ascii_hexdigit());
            let groups: Vec<&str> = inner.split('-').collect();
            (groups.len() == 5
                && groups
                    .iter()
                    .zip([8usize, 4, 4, 4, 12])
                    .all(|(g, n)| g.len() == n && hex(g)))
                || (groups.len() == 1 && inner.len() == 32 && hex(inner))
        }
        _ => true,
    };
    if fits {
        return None;
    }
    Some(match base {
        "decimal" | "numeric" | "money" | "smallmoney" | "float" | "real" => {
            "a numeric key is a number, like `1.50`"
        }
        "uniqueidentifier" => {
            "a `uniqueidentifier` key is a GUID, like `6F9619FF-8B86-D011-B42D-00C04FC964FF`"
        }
        _ => "a date or time key is written the way the engine reads it back, like `2026-09-03`",
    })
}

/// What a grant target is, as far as the engine's permission rules care.
///
/// A function is three kinds to the engine: a scalar one is executed, an
/// inline table-valued one is queried like a view, and a multi-statement one
/// is queried but never written to. Which one it is stands in its own
/// `RETURNS` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Table,
    View,
    Procedure,
    ScalarFunction,
    InlineFunction,
    MultiStatementFunction,
    Trigger,
}

impl TargetKind {
    /// The permissions the engine defines on this kind, among the ones a
    /// declaration can name. Measured on a live SQL Server 2025 for every
    /// pair (DECISIONS 89); `None` is a trigger, on which `GRANT` names no
    /// object at all (Msg 15151).
    const fn permissions(self) -> Option<&'static [Permission]> {
        use Permission::*;
        Some(match self {
            TargetKind::Table | TargetKind::View | TargetKind::InlineFunction => &[
                Select,
                Insert,
                Update,
                Delete,
                References,
                Alter,
                ViewDefinition,
            ],
            TargetKind::Procedure | TargetKind::ScalarFunction => {
                &[Execute, References, Alter, ViewDefinition]
            }
            TargetKind::MultiStatementFunction => &[Select, References, Alter, ViewDefinition],
            TargetKind::Trigger => return None,
        })
    }

    const fn article(self) -> &'static str {
        match self {
            TargetKind::Table => "a table",
            TargetKind::View => "a view",
            TargetKind::Procedure => "a procedure",
            TargetKind::ScalarFunction => "a scalar function",
            TargetKind::InlineFunction => "an inline table-valued function",
            TargetKind::MultiStatementFunction => "a multi-statement table-valued function",
            TargetKind::Trigger => "a trigger",
        }
    }
}

fn target_kind(object: &ObjectName, schema: &Schema) -> Option<TargetKind> {
    if schema.tables.contains_key(object) {
        return Some(TargetKind::Table);
    }
    // By the name the grant namespace knows, which is the identity's own name
    // (ADR-0009 §1): a grant is written against `app.f`, and on this engine
    // exactly one module answers to it.
    let (_, module) = schema
        .modules
        .iter()
        .find(|(id, _)| id.referenced_name().as_ref() == Some(object))?;
    Some(match module.kind {
        ModuleKind::View => TargetKind::View,
        ModuleKind::Procedure => TargetKind::Procedure,
        ModuleKind::Trigger => TargetKind::Trigger,
        ModuleKind::Function => match returns(&module.definition) {
            Returns::Table => TargetKind::InlineFunction,
            Returns::TableVariable => TargetKind::MultiStatementFunction,
            Returns::Scalar => TargetKind::ScalarFunction,
        },
    })
}

/// What a function's `RETURNS` clause says it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Returns {
    /// `RETURNS int`, or no `RETURNS` at all — which the engine refuses on
    /// its own, and which is a scalar function's shape as far as a grant is
    /// concerned.
    Scalar,
    /// `RETURNS TABLE`: an inline table-valued function.
    Table,
    /// `RETURNS @t TABLE (...)`: a multi-statement table-valued function.
    TableVariable,
}

/// Read off the code, not the raw text: the word may sit in a comment or a
/// literal, and only the first `RETURNS` outside them is the clause.
fn returns(definition: &str) -> Returns {
    let code = pbps_model::module::code_only(definition);
    let mut words = code
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',')
        .filter(|w| !w.is_empty());
    while let Some(w) = words.next() {
        if !w.eq_ignore_ascii_case("returns") {
            continue;
        }
        return match words.next() {
            Some(t) if t.eq_ignore_ascii_case("table") => Returns::Table,
            Some(v) if v.starts_with('@') => match words.next() {
                Some(t) if t.eq_ignore_ascii_case("table") => Returns::TableVariable,
                _ => Returns::Scalar,
            },
            _ => Returns::Scalar,
        };
    }
    Returns::Scalar
}

/// The roles every SQL Server database has, which no declaration may claim.
pub const FIXED_ROLES: [&str; 10] = [
    "public",
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

/// Every problem with a module, not just the first (ADR-0002).
///
/// The checks are few on purpose. Whether the body compiles is the engine's
/// question, and asking it here would mean parsing T-SQL — which this tool does
/// not do. What is checked is what the *emitter* needs to be true in order to
/// produce a statement at all, plus the two shapes that would otherwise become
/// a puzzling engine error on a database that is already half-changed.
pub fn module(id: &ModuleId, module: &Module) -> Vec<DialectError> {
    let mut errs = Vec::new();
    let name = id.object_name();

    for part in [&name.schema, &name.name] {
        if let Err(e) = ident::quote(part) {
            errs.push(e);
        }
    }

    // Nothing overloads on SQL Server, so a signature names an object this
    // engine cannot have (ADR-0009 §1). `Dialect::overloads` says the same
    // thing for the whole-schema check; this one catches a module that
    // reached the emitter another way — a saved plan, a state snapshot.
    if let Some(args) = id.args() {
        let spelled: Vec<String> = args.iter().map(ToString::to_string).collect();
        errs.push(invalid(format!(
            "{} `{name}` is declared with the argument list `({})`; SQL Server identifies a {} by \
             name alone",
            module.kind,
            spelled.join(","),
            module.kind
        )));
    }

    let body = module.definition.trim();
    if body.is_empty() {
        errs.push(invalid(format!(
            "{} `{name}` has an empty definition",
            module.kind
        )));
    }

    // A `GO` is not T-SQL: it is a batch separator the client interprets. One
    // inside a definition would be sent to the server verbatim and rejected —
    // and a user who wrote it meant to split the object into pieces that
    // `CREATE OR ALTER` cannot express.
    //
    // Read off the code, not the raw text: T-SQL allows the word inside a
    // literal or a comment, and a procedure that returns or documents a script
    // is a perfectly ordinary thing to want to manage.
    if pbps_model::module::code_only(body)
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case("go"))
    {
        errs.push(invalid(format!(
            "{} `{name}` contains a `GO` batch separator; a module is one batch, and `GO` is a \
             client instruction rather than something the server understands",
            module.kind
        )));
    }

    // An encrypted module has no readable definition, so pbps could never
    // compare it and would re-state it on every plan. Saying so at validate
    // time is better than a drift report that never goes quiet.
    // Matched on the collapsed text, not the literal string: `WITH\nENCRYPTION`
    // and `WITH  ENCRYPTION` are the same option, and a declaration that slipped
    // past this check would be applied and then come back with a NULL
    // definition — the module would drop out of every snapshot and every later
    // plan would try to create it again.
    if pbps_dialect::Dialect::normalize_definition(&crate::Mssql, body)
        .to_ascii_uppercase()
        .contains("WITH ENCRYPTION")
    {
        errs.push(invalid(format!(
            "{} `{name}` is declared WITH ENCRYPTION, whose definition cannot be read back; \
             pbps cannot manage it (ADR-0002)",
            module.kind
        )));
    }

    match (module.kind, id) {
        (ModuleKind::Trigger, ModuleId::Trigger { on, .. }) => {
            for part in [&on.schema, &on.name] {
                if let Err(e) = ident::quote(part) {
                    errs.push(e);
                }
            }
        }
        (ModuleKind::Trigger, _) => errs.push(invalid(format!(
            "trigger `{name}` does not say which table it is on (`on:`)"
        ))),
        (kind, ModuleId::Trigger { on, .. }) => errs.push(invalid(format!(
            "`{name}` is a {kind} and cannot be `on: {on}`; only a trigger names a table"
        ))),
        (_, ModuleId::Named(_) | ModuleId::Routine(_)) => {}
    }

    errs
}

/// Every problem with the table, not just the first.
///
/// Stopping at the first would turn fixing a table into as many round trips as
/// it has mistakes.
pub fn table(name: &TableName, table: &Table) -> Vec<DialectError> {
    let mut errs = Vec::new();

    for part in [&name.schema, &name.name] {
        if let Err(e) = ident::quote(part) {
            errs.push(e);
        }
    }

    let mut identity_columns = Vec::new();
    for (col_name, col) in &table.columns {
        if let Err(e) = ident::quote(col_name) {
            errs.push(e);
        }
        match types::normalize(&col.ty) {
            Ok(_) => {}
            Err(e) => {
                errs.push(e);
                // Everything below asks questions about the type, and asking
                // them of a type that does not exist only produces noise on top
                // of the real error.
                continue;
            }
        }
        if let Some(identity) = col.identity {
            identity_columns.push(col_name.clone());
            if !types::can_be_identity(&col.ty) {
                errs.push(invalid(format!(
                    "column `{col_name}` is IDENTITY, which needs an integer type or a decimal with scale 0, not `{}`",
                    col.ty
                )));
            }
            if col.nullable {
                errs.push(invalid(format!(
                    "column `{col_name}` is IDENTITY, so it cannot be nullable"
                )));
            }
            if identity.increment == 0 {
                errs.push(invalid(format!(
                    "column `{col_name}` has an IDENTITY increment of 0, which never advances"
                )));
            }
            if col.default.is_some() {
                errs.push(invalid(format!(
                    "column `{col_name}` is IDENTITY, so it cannot also have a default"
                )));
            }
        }
    }
    if identity_columns.len() > 1 {
        errs.push(invalid(format!(
            "a table may have only one IDENTITY column, but `{}` are all marked",
            identity_columns.join("`, `")
        )));
    }

    // A declared row value travels as a string literal and the engine
    // converts it — right for numbers, dates and text, wrong for bytes:
    // `N'0x01'` into a varbinary stores the characters, not the byte, and a
    // key read back as `0x01` then fails to select its own row. A
    // `sql_variant` fails the other way round: the text goes in, but the
    // variant's own base type does not come back out, so a pulled `int`
    // variant would be written back as an `nvarchar` one. The emitter
    // carries no types (a plan applies with no checkout), so both are
    // refused here, by name, rather than guessed at twice (DECISIONS 70, 87);
    // a spatial value loses its SRID the same way (90).
    // A scalar of the wrong *kind* is refused for the same reason: a bare
    // `1` for a `varchar` column is stored and read back as text, and a
    // declaration that disagrees with its own database on every plan is
    // worse than none (87).
    if let Some(data) = &table.data {
        let base_of = |column: &str| {
            table
                .columns
                .get(column)
                .and_then(|c| types::normalize(&c.ty).ok())
                .map(|t| t.base)
        };
        let unholdable = |base: &str| match base {
            "binary" | "varbinary" | "image" | "timestamp" => Some((
                "a binary column",
                "row values are written as text, and text does not convert to the bytes it \
                 names",
            )),
            "sql_variant" => Some((
                "a `sql_variant` column",
                "row values are written as text, and the variant's own base type does not \
                 survive the trip",
            )),
            "geometry" | "geography" => Some((
                "a spatial column",
                "row values are written as text, and the SRID a spatial value carries is not in \
                 its text — the engine would give it the default one",
            )),
            _ => None,
        };
        if let Some(pk) = &table.primary_key
            && let [key] = pk.columns.as_slice()
            && let Some((what, why)) = base_of(key).as_deref().and_then(unholdable)
        {
            errs.push(invalid(format!(
                "a `data:` block cannot key its rows by `{key}`, {what}: {why}"
            )));
        }
        // The key travels as a string literal too, and the engine converts
        // it on the way in. A key an `int` column cannot hold is not caught
        // by the alias query on a table this plan creates — there is no
        // table to ask yet — so the accepted plan failed at its first
        // insert. Checked here, by the kind the column reads back as: an
        // integer key is digits with an optional sign, a `bit` key one of
        // its four spellings, and any text is a text key (DECISIONS 99).
        if let Some(pk) = &table.primary_key
            && let [key_column] = pk.columns.as_slice()
            && let Some(base) = base_of(key_column)
        {
            let kind = ValueKind::of(&base);
            for key in data.rows.keys() {
                let text = key.as_str().trim();
                let fits = match kind {
                    ValueKind::Int => {
                        let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
                        let shaped =
                            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
                        // And within the type: `300` is an integer no `tinyint`
                        // holds, and the engine refuses the insert the same way.
                        shaped
                            && text.parse::<i128>().is_ok_and(|n| {
                                int_range(&base).is_none_or(|(lo, hi)| (lo..=hi).contains(&n))
                                    && (i128::from(i64::MIN)..=i128::from(i64::MAX)).contains(&n)
                            })
                    }
                    ValueKind::Bool => {
                        matches!(text, "0" | "1")
                            || text.eq_ignore_ascii_case("true")
                            || text.eq_ignore_ascii_case("false")
                    }
                    ValueKind::Text => key_shape(&base, text).is_none(),
                };
                if !fits {
                    errs.push(invalid(format!(
                        "row key `{key}` cannot be a `{base}`, the type of the key column \
                         `{key_column}`: the engine would refuse the insert — {}",
                        match kind {
                            ValueKind::Int =>
                                "an integer key is digits, with an optional sign, \
                                               within what the type holds",
                            ValueKind::Bool => "a `bit` key is `0`, `1`, `true` or `false`",
                            ValueKind::Text => key_shape(&base, text).unwrap_or_default(),
                        }
                    )));
                }
            }
        }
        for (key, row) in &data.rows {
            for (column, value) in &row.0 {
                let Some(base) = base_of(column) else {
                    continue;
                };
                if let Some((what, why)) = unholdable(&base) {
                    errs.push(invalid(format!(
                        "row `{key}` sets `{column}`, {what}, which a `data:` block cannot \
                         hold: {why} — leave the column out of the row"
                    )));
                    continue;
                }
                let kind = ValueKind::of(&base);
                let agrees = matches!(
                    (kind, value),
                    (_, Value::Null)
                        | (ValueKind::Bool, Value::Bool(_))
                        | (ValueKind::Int, Value::Int(_))
                        | (ValueKind::Text, Value::Text(_))
                );
                if !agrees {
                    let spelling = match kind {
                        ValueKind::Bool => "`true` or `false`",
                        ValueKind::Int => "a bare integer",
                        ValueKind::Text => "a quoted string",
                    };
                    errs.push(invalid(format!(
                        "row `{key}` sets `{column}` to {value}, {}, but `{base}` reads back as \
                         {}: the declaration would disagree with its own database on every \
                         plan — write it as {spelling}",
                        value.kind(),
                        kind.name()
                    )));
                }
                // The right kind, but not a value the type holds: the kind
                // check cannot see it, the spelling probe asks the engine only
                // about text, and the insert would fail at apply (DECISIONS 104).
                if let Value::Int(n) = value
                    && let Some((lo, hi)) = int_range(&base)
                    && !(lo..=hi).contains(&i128::from(*n))
                {
                    errs.push(invalid(format!(
                        "row `{key}` sets `{column}` to {n}, but `{base}` holds {lo} to {hi}: the \
                         engine would refuse the insert"
                    )));
                }
            }
        }
    }

    if let Some(pk) = &table.primary_key {
        if let Some(n) = &pk.name
            && let Err(e) = ident::quote(n)
        {
            errs.push(e);
        }
        errs.extend(key_columns("primary key", &pk.columns, table));
        for c in &pk.columns {
            if table.columns.get(c).is_some_and(|c| c.nullable) {
                errs.push(invalid(format!(
                    "primary key column `{c}` is nullable; a primary key column must be NOT NULL"
                )));
            }
        }
    }

    for (n, u) in &table.unique {
        if let Err(e) = ident::quote(n) {
            errs.push(e);
        }
        errs.extend(key_columns(
            &format!("unique constraint `{n}`"),
            &u.columns,
            table,
        ));
    }

    for (n, fk) in &table.foreign_keys {
        if let Err(e) = ident::quote(n) {
            errs.push(e);
        }
        errs.extend(key_columns(
            &format!("foreign key `{n}`"),
            &fk.columns,
            table,
        ));
        if fk.columns.len() != fk.references_columns.len() {
            errs.push(invalid(format!(
                "foreign key `{n}` has {} column(s) but references {}; the two sides must line up",
                fk.columns.len(),
                fk.references_columns.len()
            )));
        }
        if fk.references_columns.is_empty() {
            errs.push(invalid(format!(
                "foreign key `{n}` names no columns on the referenced table"
            )));
        }
    }

    for (n, c) in &table.checks {
        if let Err(e) = ident::quote(n) {
            errs.push(e);
        }
        if c.expression.trim().is_empty() {
            errs.push(invalid(format!(
                "check constraint `{n}` has an empty expression"
            )));
        }
    }

    for (n, idx) in &table.indexes {
        if let Err(e) = ident::quote(n) {
            errs.push(e);
        }
        let keys: Vec<String> = idx.columns.iter().map(|c| c.name.clone()).collect();
        errs.extend(key_columns(&format!("index `{n}`"), &keys, table));
        if keys.len() > MAX_INDEX_KEY_COLUMNS {
            errs.push(invalid(format!(
                "index `{n}` has {} key columns; SQL Server allows at most {MAX_INDEX_KEY_COLUMNS}",
                keys.len()
            )));
        }
        for inc in &idx.include {
            if !table.columns.contains_key(inc) {
                errs.push(invalid(format!(
                    "index `{n}` includes `{inc}`, which is not a column of this table"
                )));
            }
            if keys.contains(inc) {
                errs.push(invalid(format!(
                    "index `{n}` has `{inc}` both as a key column and as an included column"
                )));
            }
        }
        if idx.filter.as_ref().is_some_and(|f| f.trim().is_empty()) {
            errs.push(invalid(format!(
                "index `{n}` has an empty filter expression"
            )));
        }
    }

    errs
}

/// The checks shared by every construct that builds a key out of columns.
fn key_columns(what: &str, columns: &[String], table: &Table) -> Vec<DialectError> {
    let mut errs = Vec::new();
    if columns.is_empty() {
        errs.push(invalid(format!("{what} names no columns")));
    }
    let mut seen = Vec::new();
    for c in columns {
        match table.columns.get(c) {
            None => errs.push(invalid(format!(
                "{what} references `{c}`, which is not a column of this table"
            ))),
            Some(col) if !types::is_indexable(&col.ty) => errs.push(invalid(format!(
                "{what} uses `{c}`, whose type `{}` cannot be part of a key",
                col.ty
            ))),
            Some(_) => {}
        }
        if seen.contains(&c) {
            errs.push(invalid(format!("{what} names `{c}` twice")));
        }
        seen.push(c);
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{
        CheckConstraint, Column, ColumnType, ForeignKey, Identity, Index, IndexColumn, PrimaryKey,
        UniqueConstraint,
    };

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    fn base_table() -> (TableName, Table) {
        let mut t = Table::default();
        t.columns
            .insert("id".into(), Column::new(ty("bigint")).not_null());
        t.columns
            .insert("email".into(), Column::new(ty("nvarchar(255)")));
        t.columns
            .insert("body".into(), Column::new(ty("nvarchar(max)")));
        (TableName::new("dbo", "customer"), t)
    }

    fn messages(errs: &[DialectError]) -> String {
        errs.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_well_formed_table_produces_no_errors() {
        let (name, mut t) = base_table();
        t.primary_key = Some(PrimaryKey {
            name: Some("pk_customer".into()),
            columns: vec!["id".into()],
        });
        t.unique.insert(
            "uq_email".into(),
            UniqueConstraint {
                columns: vec!["email".into()],
            },
        );
        assert_eq!(messages(&table(&name, &t)), "");
    }

    #[test]
    fn a_key_over_a_missing_column_is_reported() {
        let (name, mut t) = base_table();
        t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["nope".into()],
        });
        let errs = table(&name, &t);
        assert!(
            messages(&errs).contains("not a column of this table"),
            "{}",
            messages(&errs)
        );
    }

    /// The engine refuses a nullable PK column at CREATE time; catching it at
    /// validate time points at the file instead of at a failed apply.
    #[test]
    fn a_nullable_primary_key_column_is_reported() {
        let (name, mut t) = base_table();
        t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["email".into()],
        });
        assert!(messages(&table(&name, &t)).contains("must be NOT NULL"));
    }

    #[test]
    fn a_key_over_an_unindexable_type_is_reported() {
        let (name, mut t) = base_table();
        t.unique.insert(
            "uq_body".into(),
            UniqueConstraint {
                columns: vec!["body".into()],
            },
        );
        assert!(messages(&table(&name, &t)).contains("cannot be part of a key"));
    }

    #[test]
    fn identity_rules_are_enforced() {
        let (name, mut t) = base_table();
        let mut c = Column::new(ty("nvarchar(10)"));
        c.identity = Some(Identity {
            seed: 1,
            increment: 0,
        });
        t.columns.insert("seq".into(), c);
        let msg = messages(&table(&name, &t));
        assert!(msg.contains("needs an integer type"), "{msg}");
        assert!(msg.contains("cannot be nullable"), "{msg}");
        assert!(msg.contains("never advances"), "{msg}");
    }

    /// Refused, not converted: the emitter has no types to convert with.
    #[test]
    fn a_binary_cell_or_key_in_a_data_block_is_refused_by_name() {
        use pbps_model::{DataMode, Row, RowKey, TableData, Value};
        let (name, mut t) = base_table();
        t.columns
            .insert("blob".into(), Column::new(ty("varbinary(8)")));
        let mut row = Row::default();
        row.0.insert("blob".into(), Value::Text("0x01".into()));
        t.data = Some(TableData {
            mode: DataMode::Exact,
            rows: [(RowKey::from("1"), row)].into_iter().collect(),
        });
        let msg = messages(&table(&name, &t));
        assert!(msg.contains("`blob`, a binary column"), "{msg}");

        // The same row with the cell left out is fine: no value flows.
        t.data
            .as_mut()
            .unwrap()
            .rows
            .insert(RowKey::from("1"), Row::default());
        assert!(!messages(&table(&name, &t)).contains("binary"));

        // A binary key is refused whatever the rows set.
        t.columns
            .insert("id".into(), Column::new(ty("binary(4)")).not_null());
        t.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["id".into()],
        });
        let msg = messages(&table(&name, &t));
        assert!(msg.contains("key its rows by `id`"), "{msg}");
    }

    /// Refused, not normalized: the differ has no column types to normalize
    /// with, and a cell read back as another kind would drift on every plan.
    #[test]
    fn a_cell_of_the_wrong_kind_for_its_column_is_refused_with_the_spelling_that_fits() {
        use pbps_model::{DataMode, Row, RowKey, TableData, Value};
        let (name, mut t) = base_table();
        t.columns
            .insert("code".into(), Column::new(ty("varchar(10)")));
        t.columns.insert("rank".into(), Column::new(ty("int")));
        t.columns.insert("flag".into(), Column::new(ty("bit")));
        let with = |t: &mut Table, cells: Vec<(&str, Value)>| {
            let mut row = Row::default();
            for (c, v) in cells {
                row.0.insert(c.to_owned(), v);
            }
            t.data = Some(TableData {
                mode: DataMode::Exact,
                rows: [(RowKey::from("1"), row)].into_iter().collect(),
            });
        };
        for (column, value, spelling) in [
            ("code", Value::Int(1), "a quoted string"),
            ("code", Value::Bool(true), "a quoted string"),
            ("rank", Value::Text("1".into()), "a bare integer"),
            ("rank", Value::Bool(true), "a bare integer"),
            ("flag", Value::Int(1), "`true` or `false`"),
            ("flag", Value::Text("true".into()), "`true` or `false`"),
        ] {
            with(&mut t, vec![(column, value)]);
            let msg = messages(&table(&name, &t));
            assert!(
                msg.contains(&format!("sets `{column}` to")) && msg.contains(spelling),
                "{column}: {msg}"
            );
        }
        // The kind that reads back, and NULL in any column, pass.
        with(
            &mut t,
            vec![
                ("code", Value::Text("1".into())),
                ("rank", Value::Int(1)),
                ("flag", Value::Bool(true)),
            ],
        );
        let msg = messages(&table(&name, &t));
        assert!(!msg.contains("reads back"), "{msg}");
        with(
            &mut t,
            vec![
                ("code", Value::Null),
                ("rank", Value::Null),
                ("flag", Value::Null),
            ],
        );
        let msg = messages(&table(&name, &t));
        assert!(!msg.contains("reads back"), "{msg}");
    }

    /// The right kind is not enough: `256` is an integer no `tinyint` holds,
    /// and a negative one is the surprise. Keys the same.
    #[test]
    fn an_integer_outside_what_its_column_holds_is_refused_with_the_bounds() {
        use pbps_model::{DataMode, Row, RowKey, TableData, Value};
        let (name, mut t) = base_table();
        t.columns.insert("tiny".into(), Column::new(ty("tinyint")));
        t.columns
            .insert("small".into(), Column::new(ty("smallint")));
        t.columns.insert("wide".into(), Column::new(ty("int")));
        t.columns.insert("big".into(), Column::new(ty("bigint")));
        let with = |t: &mut Table, cells: Vec<(&str, i64)>| {
            let mut row = Row::default();
            for (c, v) in cells {
                row.0.insert(c.to_owned(), Value::Int(v));
            }
            t.data = Some(TableData {
                mode: DataMode::Exact,
                rows: [(RowKey::from("1"), row)].into_iter().collect(),
            });
        };
        with(
            &mut t,
            vec![
                ("tiny", 255),
                ("small", -32768),
                ("wide", 2_147_483_647),
                ("big", i64::MIN),
            ],
        );
        assert!(!messages(&table(&name, &t)).contains("holds"));
        with(
            &mut t,
            vec![("tiny", 256), ("small", 32768), ("wide", -2_147_483_649)],
        );
        let msg = messages(&table(&name, &t));
        assert!(
            msg.contains("sets `tiny` to 256, but `tinyint` holds 0 to 255"),
            "{msg}"
        );
        assert!(msg.contains("sets `small` to 32768"), "{msg}");
        assert!(msg.contains("sets `wide` to -2147483649"), "{msg}");
        with(&mut t, vec![("tiny", -1)]);
        assert!(messages(&table(&name, &t)).contains("`tinyint` holds 0 to 255"));

        // A key too: shaped like an integer and still not one the type holds.
        t.columns
            .insert("k".into(), Column::new(ty("tinyint")).not_null());
        t.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["k".into()],
        });
        t.data = Some(TableData {
            mode: DataMode::Exact,
            rows: [
                (RowKey::from("255"), Row::default()),
                (RowKey::from("300"), Row::default()),
                (RowKey::from("-1"), Row::default()),
            ]
            .into_iter()
            .collect(),
        });
        let msg = messages(&table(&name, &t));
        assert!(msg.contains("row key `300` cannot be a `tinyint`"), "{msg}");
        assert!(msg.contains("row key `-1` cannot be a `tinyint`"), "{msg}");
        assert!(!msg.contains("row key `255`"), "{msg}");
    }

    /// The key is a literal like any cell, and a table this plan creates has
    /// no engine to ask about it; the kind of the key column decides.
    #[test]
    fn a_row_key_the_key_column_cannot_hold_is_refused_by_name() {
        use pbps_model::{DataMode, Row, RowKey, TableData};
        let (name, mut t) = base_table();
        let with = |t: &mut Table, type_name: &str, keys: &[&str]| {
            t.columns
                .insert("k".into(), Column::new(ty(type_name)).not_null());
            t.primary_key = Some(pbps_model::PrimaryKey {
                name: None,
                columns: vec!["k".into()],
            });
            t.data = Some(TableData {
                mode: DataMode::Exact,
                rows: keys
                    .iter()
                    .map(|k| (RowKey::from(*k), Row::default()))
                    .collect(),
            });
        };
        with(&mut t, "int", &["1", "-7", "+3", "007"]);
        assert!(!messages(&table(&name, &t)).contains("row key"));
        with(&mut t, "int", &["not-an-int", "1.5", ""]);
        let msg = messages(&table(&name, &t));
        assert!(
            msg.contains("row key `not-an-int` cannot be a `int`"),
            "{msg}"
        );
        assert!(msg.contains("row key `1.5`"), "{msg}");
        assert!(msg.contains("row key ``"), "{msg}");
        with(&mut t, "bit", &["0", "1", "TRUE", "false"]);
        assert!(!messages(&table(&name, &t)).contains("row key"));
        with(&mut t, "bit", &["yes"]);
        assert!(messages(&table(&name, &t)).contains("row key `yes` cannot be a `bit`"));
        // Text takes anything.
        with(&mut t, "varchar(10)", &["not-an-int", ""]);
        assert!(!messages(&table(&name, &t)).contains("row key"));
        // A type that reads back as text still has shapes it cannot read;
        // those are refused here, and the rest is the engine's to judge
        // before anything is written (DECISIONS 101, 103).
        with(&mut t, "decimal(5,2)", &["1.50", "-.5", "1e3", "007"]);
        assert!(!messages(&table(&name, &t)).contains("row key"));
        with(&mut t, "decimal(5,2)", &["1,5", "abc", "."]);
        let msg = messages(&table(&name, &t));
        assert!(msg.contains("row key `1,5` cannot be a `decimal`"), "{msg}");
        assert!(msg.contains("row key `abc`"), "{msg}");
        assert!(msg.contains("row key `.`"), "{msg}");
        with(
            &mut t,
            "date",
            &["2026-09-03", "20260903", "2026-09-03T10:00:00Z"],
        );
        assert!(!messages(&table(&name, &t)).contains("row key"));
        with(&mut t, "date", &["not-a-date", "tomorrow"]);
        let msg = messages(&table(&name, &t));
        assert!(
            msg.contains("row key `not-a-date` cannot be a `date`"),
            "{msg}"
        );
        with(
            &mut t,
            "uniqueidentifier",
            &[
                "6F9619FF-8B86-D011-B42D-00C04FC964FF",
                "{6f9619ff-8b86-d011-b42d-00c04fc964ff}",
                "6F9619FF8B86D011B42D00C04FC964FF",
            ],
        );
        assert!(!messages(&table(&name, &t)).contains("row key"));
        with(
            &mut t,
            "uniqueidentifier",
            &["not-a-guid", "6F9619FF-8B86-D011-B42D"],
        );
        let msg = messages(&table(&name, &t));
        assert!(
            msg.contains("row key `not-a-guid` cannot be a `uniqueidentifier`"),
            "{msg}"
        );
        assert!(msg.contains("row key `6F9619FF-8B86-D011-B42D`"), "{msg}");
    }

    /// Refused for what it is, not for its kind: the text would go in, but
    /// the variant's base type would not come back out.
    #[test]
    fn a_sql_variant_cell_or_key_in_a_data_block_is_refused_by_name() {
        use pbps_model::{DataMode, Row, RowKey, TableData, Value};
        let (name, mut t) = base_table();
        t.columns.insert("v".into(), Column::new(ty("sql_variant")));
        let mut row = Row::default();
        row.0.insert("v".into(), Value::Int(1));
        t.data = Some(TableData {
            mode: DataMode::Exact,
            rows: [(RowKey::from("1"), row)].into_iter().collect(),
        });
        let msg = messages(&table(&name, &t));
        assert!(msg.contains("`v`, a `sql_variant` column"), "{msg}");
        assert!(!msg.contains("reads back"), "{msg}");

        // Left out of the row, nothing flows and nothing is refused.
        t.data
            .as_mut()
            .unwrap()
            .rows
            .insert(RowKey::from("1"), Row::default());
        assert!(!messages(&table(&name, &t)).contains("sql_variant"));

        // A `sql_variant` key is refused whatever the rows set.
        t.columns
            .insert("id".into(), Column::new(ty("sql_variant")).not_null());
        t.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["id".into()],
        });
        let msg = messages(&table(&name, &t));
        assert!(
            msg.contains("key its rows by `id`, a `sql_variant` column"),
            "{msg}"
        );
    }

    /// Every pair measured on the engine (DECISIONS 89): the wrong permission
    /// is refused by the kind of its target, the right one passes, and an
    /// object the declarations lack is left to the model's own rule.
    #[test]
    fn a_permission_the_engine_does_not_define_on_the_target_is_refused_by_kind() {
        use pbps_model::{Module, Permission};
        let mut schema = Schema::default();
        let (t_name, t) = base_table();
        schema.tables.insert(t_name, t);
        let module = |kind: ModuleKind, body: &str| Module {
            kind,
            description: None,
            definition: body.to_owned(),
        };
        schema.modules.insert(
            ModuleId::Named(TableName::new("dbo", "v")),
            module(ModuleKind::View, "SELECT 1 AS one"),
        );
        schema.modules.insert(
            ModuleId::Named(TableName::new("dbo", "p")),
            module(ModuleKind::Procedure, "AS SELECT 1"),
        );
        schema.modules.insert(
            ModuleId::Named(TableName::new("dbo", "fs")),
            module(ModuleKind::Function, "() RETURNS int AS BEGIN RETURN 1 END"),
        );
        schema.modules.insert(
            ModuleId::Named(TableName::new("dbo", "fi")),
            module(
                ModuleKind::Function,
                "() RETURNS TABLE AS RETURN SELECT 1 AS one",
            ),
        );
        schema.modules.insert(
            ModuleId::Named(TableName::new("dbo", "fm")),
            module(
                ModuleKind::Function,
                "() RETURNS @r TABLE (one int) AS BEGIN INSERT @r VALUES (1); RETURN END",
            ),
        );
        schema.modules.insert(
            ModuleId::Named(TableName::new("dbo", "tr")),
            module(ModuleKind::Trigger, "AFTER INSERT AS SELECT 1"),
        );
        let grant = |target: &str, p: Permission| {
            let mut role = Role::default();
            role.grants
                .insert(target.parse().unwrap(), [p].into_iter().collect());
            super::role("r", &role, &schema)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        };
        for (target, p, what) in [
            ("dbo.customer", Permission::Execute, "a table"),
            ("dbo.v", Permission::Execute, "a view"),
            ("dbo.p", Permission::Select, "a procedure"),
            ("dbo.fs", Permission::Select, "a scalar function"),
            (
                "dbo.fi",
                Permission::Execute,
                "an inline table-valued function",
            ),
            (
                "dbo.fm",
                Permission::Insert,
                "a multi-statement table-valued function",
            ),
            (
                "dbo.fm",
                Permission::Execute,
                "a multi-statement table-valued function",
            ),
        ] {
            let msg = grant(target, p);
            assert!(
                msg.contains(&format!(
                    "`{}` does not apply to `{target}`, {what}",
                    p.as_str()
                )),
                "{target} {p:?}: {msg}"
            );
        }
        for (target, p) in [
            ("dbo.customer", Permission::Insert),
            ("dbo.v", Permission::Delete),
            ("dbo.p", Permission::Execute),
            ("dbo.fs", Permission::Execute),
            ("dbo.fs", Permission::References),
            ("dbo.fi", Permission::Update),
            ("dbo.fm", Permission::Select),
            ("schema::dbo", Permission::Execute),
            ("schema::dbo", Permission::Select),
        ] {
            let msg = grant(target, p);
            assert!(msg.is_empty(), "{target} {p:?}: {msg}");
        }
        // A trigger takes nothing, whatever is asked.
        let msg = grant("dbo.tr", Permission::Select);
        assert!(
            msg.contains("a trigger, which takes no permission"),
            "{msg}"
        );
        // An object nobody declares is the model's finding; this check has no
        // kind to measure against and says nothing.
        assert!(grant("dbo.ghost", Permission::Execute).is_empty());
    }

    /// The clause is read off the code, past comments and literals, and a
    /// function with no clause is a scalar one to a grant.
    #[test]
    fn a_function_is_scalar_inline_or_multi_statement_by_its_returns_clause() {
        assert_eq!(
            returns("(@a int) RETURNS int AS BEGIN RETURN @a END"),
            Returns::Scalar
        );
        assert_eq!(
            returns("() returns table as return select 1 as one"),
            Returns::Table
        );
        assert_eq!(
            returns("()\nRETURNS @out TABLE (id int)\nAS BEGIN RETURN END"),
            Returns::TableVariable
        );
        assert_eq!(
            returns("() -- RETURNS TABLE, says the comment\nRETURNS int AS BEGIN RETURN 1 END"),
            Returns::Scalar
        );
        assert_eq!(
            returns("() RETURNS nvarchar(10) AS BEGIN RETURN 'RETURNS TABLE' END"),
            Returns::Scalar
        );
        assert_eq!(returns("() AS BEGIN RETURN 1 END"), Returns::Scalar);
    }

    /// Refused for what it is: the text of a spatial value has no SRID.
    #[test]
    fn a_spatial_cell_or_key_in_a_data_block_is_refused_by_name() {
        use pbps_model::{DataMode, Row, RowKey, TableData, Value};
        let (name, mut t) = base_table();
        t.columns
            .insert("shape".into(), Column::new(ty("geography")));
        let mut row = Row::default();
        row.0
            .insert("shape".into(), Value::Text("POINT (1 2)".into()));
        t.data = Some(TableData {
            mode: DataMode::Exact,
            rows: [(RowKey::from("1"), row)].into_iter().collect(),
        });
        let msg = messages(&table(&name, &t));
        assert!(msg.contains("`shape`, a spatial column"), "{msg}");
        t.data
            .as_mut()
            .unwrap()
            .rows
            .insert(RowKey::from("1"), Row::default());
        assert!(!messages(&table(&name, &t)).contains("spatial"));
        t.columns
            .insert("id".into(), Column::new(ty("geometry")).not_null());
        t.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["id".into()],
        });
        assert!(messages(&table(&name, &t)).contains("key its rows by `id`, a spatial column"));
    }

    #[test]
    fn two_identity_columns_are_reported() {
        let (name, mut t) = base_table();
        for col in ["a", "b"] {
            let mut c = Column::new(ty("int")).not_null();
            c.identity = Some(Identity {
                seed: 1,
                increment: 1,
            });
            t.columns.insert(col.into(), c);
        }
        assert!(messages(&table(&name, &t)).contains("only one IDENTITY column"));
    }

    #[test]
    fn a_foreign_key_with_mismatched_sides_is_reported() {
        let (name, mut t) = base_table();
        t.foreign_keys.insert(
            "fk_x".into(),
            ForeignKey {
                columns: vec!["id".into(), "email".into()],
                references_table: TableName::new("dbo", "other"),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        assert!(messages(&table(&name, &t)).contains("must line up"));
    }

    #[test]
    fn an_index_including_one_of_its_own_keys_is_reported() {
        let (name, mut t) = base_table();
        t.indexes.insert(
            "ix_email".into(),
            Index {
                columns: vec![IndexColumn {
                    name: "email".into(),
                    descending: false,
                }],
                include: vec!["email".into()],
                unique: false,
                filter: None,
            },
        );
        let msg = messages(&table(&name, &t));
        assert!(
            msg.contains("both as a key column and as an included column"),
            "{msg}"
        );
    }

    #[test]
    fn empty_expressions_are_reported() {
        let (name, mut t) = base_table();
        t.checks.insert(
            "ck".into(),
            CheckConstraint {
                expression: "  ".into(),
            },
        );
        assert!(messages(&table(&name, &t)).contains("empty expression"));
    }

    /// One pass must surface every problem: the loop must not stop at the first.
    #[test]
    fn all_problems_are_reported_in_one_pass() {
        let (name, mut t) = base_table();
        t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["nope".into()],
        });
        t.checks.insert(
            "ck".into(),
            CheckConstraint {
                expression: "".into(),
            },
        );
        assert!(table(&name, &t).len() >= 2);
    }

    // ---- modules ----

    fn a_module(kind: ModuleKind, definition: &str) -> Module {
        Module {
            kind,
            description: None,
            definition: definition.to_owned(),
        }
    }

    fn module_errors(name: &str, m: &Module) -> String {
        module(&name.parse().unwrap(), m)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_well_formed_view_produces_no_errors() {
        assert!(
            module(
                &"dbo.active_customer".parse().unwrap(),
                &a_module(ModuleKind::View, "SELECT customer_id FROM dbo.customer")
            )
            .is_empty()
        );
    }

    /// `GO` is a client instruction, not T-SQL. Sent to the server it is a
    /// syntax error, and the user who wrote it meant something the emitter
    /// cannot express as one CREATE OR ALTER.
    #[test]
    fn a_batch_separator_inside_a_definition_is_refused() {
        let e = module_errors(
            "dbo.v",
            &a_module(ModuleKind::View, "SELECT 1\nGO\nSELECT 2"),
        );
        assert!(e.contains("GO"), "{e}");
    }

    /// An encrypted module cannot be read back, so every plan would re-state
    /// it and every drift check would fire.
    #[test]
    fn an_encrypted_module_is_refused() {
        let e = module_errors(
            "dbo.v",
            &a_module(ModuleKind::View, "WITH ENCRYPTION AS SELECT 1"),
        );
        assert!(e.contains("cannot be read back"), "{e}");
    }

    /// `GO` is a client instruction, but the two letters are ordinary text
    /// inside a literal or a comment — and a procedure that returns or
    /// documents a deployment script is a perfectly ordinary thing to manage.
    #[test]
    fn go_inside_a_literal_or_a_comment_is_not_a_batch_separator() {
        for body in [
            "AS SELECT 'first line\nGO\nsecond line' AS script",
            "AS /* the caller runs\nGO\nafterwards */ SELECT 1",
            "AS SELECT 1 -- GO",
        ] {
            let e = module_errors("dbo.p", &a_module(ModuleKind::Procedure, body));
            assert!(!e.contains("batch separator"), "{body}: {e}");
        }
        // A real one is still refused.
        let e = module_errors(
            "dbo.p",
            &a_module(ModuleKind::Procedure, "AS SELECT 1\nGO\nSELECT 2"),
        );
        assert!(e.contains("batch separator"), "{e}");
    }

    /// The option is tokens, not one exact string. A spelling that slipped
    /// through would be applied and then read back as NULL, so the module would
    /// drop out of every snapshot and every later plan would create it again.
    #[test]
    fn encryption_is_recognised_however_it_is_spaced() {
        for body in [
            "WITH  ENCRYPTION AS SELECT 1",
            "WITH\nENCRYPTION AS SELECT 1",
            "WITH\t ENCRYPTION\n AS SELECT 1",
        ] {
            let e = module_errors("dbo.v", &a_module(ModuleKind::View, body));
            assert!(e.contains("cannot be read back"), "{body}: {e}");
        }
        // And a body that merely mentions the words apart is not the option.
        let e = module_errors(
            "dbo.v",
            &a_module(ModuleKind::View, "AS SELECT 'encryption' AS with_note"),
        );
        assert!(!e.contains("cannot be read back"), "{e}");
    }

    #[test]
    fn a_trigger_needs_a_table_and_nothing_else_may_have_one() {
        let e = module_errors(
            "dbo.trg",
            &a_module(ModuleKind::Trigger, "AFTER INSERT AS SELECT 1"),
        );
        assert!(e.contains("which table"), "{e}");

        // A view under a trigger's identity: the loader refuses the `on:`
        // that would build one, and this is the same check for a plan or a
        // state that arrived as JSON.
        let e = module_errors("dbo.customer.v", &a_module(ModuleKind::View, "SELECT 1"));
        assert!(e.contains("only a trigger"), "{e}");
    }

    /// Nothing overloads on SQL Server, so a declared signature names an
    /// object this engine cannot have (ADR-0009 §1).
    #[test]
    fn a_signature_is_refused() {
        let e = module_errors(
            "dbo.f(int,nvarchar(10))",
            &a_module(ModuleKind::Function, "() RETURNS int AS BEGIN RETURN 1 END"),
        );
        assert!(e.contains("argument list `(int,nvarchar(10))`"), "{e}");
        assert!(e.contains("by name alone"), "{e}");
    }

    #[test]
    fn an_empty_definition_is_refused() {
        let e = module_errors("dbo.v", &a_module(ModuleKind::View, "  \n "));
        assert!(e.contains("empty definition"), "{e}");
    }

    // ---- roles (ADR-0005) ----

    /// The model spells PostgreSQL's five too (ADR-0010 §6); this engine has
    /// none of them, on any securable — measured, `GRANT USAGE` is a parse
    /// error before the target is looked at — so each is refused by name,
    /// once, on an object and on a schema alike, and the kind check does not
    /// report the same grant a second time.
    #[test]
    fn a_permission_this_engine_lacks_is_refused_by_name_on_any_target() {
        use pbps_model::Permission;
        let mut schema = Schema::default();
        let (t_name, t) = base_table();
        schema.tables.insert(t_name, t);
        let errors = |target: &str, ps: &[Permission]| {
            let mut role = Role::default();
            role.grants
                .insert(target.parse().unwrap(), ps.iter().copied().collect());
            super::role("r", &role, &schema)
        };
        for p in [
            Permission::Usage,
            Permission::Create,
            Permission::Truncate,
            Permission::Trigger,
            Permission::Maintain,
        ] {
            for target in ["dbo.customer", "schema::dbo"] {
                let errs = errors(target, &[p]);
                assert_eq!(errs.len(), 1, "{target} {p:?}: {errs:?}");
                let msg = errs[0].to_string();
                assert!(
                    msg.contains(&format!("`{}` on `{target}`", p.as_str()))
                        && msg.contains("PostgreSQL's")
                        && msg.contains("view-definition"),
                    "{msg}"
                );
            }
        }
        // Beside a word the engine has: one finding, for the one word.
        let errs = errors("dbo.customer", &[Permission::Select, Permission::Usage]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].to_string().contains("`usage`"), "{errs:?}");
        // And the engine's own eight are still its own.
        for p in super::PERMISSIONS {
            assert!(super::has_permission(p), "{p:?}");
        }
        assert!(errors("schema::dbo", &super::PERMISSIONS).is_empty());
    }

    #[test]
    fn a_built_in_role_cannot_be_declared_and_an_ordinary_one_can() {
        let mut role = Role::default();
        role.grants.insert(
            "dbo.customer".parse().unwrap(),
            [pbps_model::Permission::Select].into_iter().collect(),
        );
        assert!(super::role("app_reader", &role, &Schema::default()).is_empty());
        let errs = super::role("db_datareader", &role, &Schema::default());
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].to_string().contains("built-in"), "{errs:?}");
        // Case is the engine's, not the file's.
        assert!(!super::role("PUBLIC", &role, &Schema::default()).is_empty());
        // And a target that cannot be quoted is refused where the name is.
        let mut bad = Role::default();
        bad.grants.insert(
            GrantTarget::Schema("a\0b".into()),
            [pbps_model::Permission::Select].into_iter().collect(),
        );
        assert!(!super::role("ok", &bad, &Schema::default()).is_empty());
    }
}
