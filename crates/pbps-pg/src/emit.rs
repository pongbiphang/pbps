//! What one change looks like on PostgreSQL.
//!
//! The counterpart of `pbps-mssql/src/emit.rs`'s structural half, and the only
//! file in this crate that turns a [`Change`] into SQL a server will run.
//!
//! # The write scope
//!
//! Every statement here is wrapped in a `search_path` of its own — the object's
//! own schema first, then the project's configured extras
//! ([`Postgres::with_write_path_extras`]) — because PostgreSQL binds an
//! unqualified name in a *verbatim expression* when the object is created, and
//! this model holds three of those: a column's default, a check's expression
//! and an index's filter (ADR-0013 §3, and the same section's correction that
//! the rule is scoped to what the model can represent).
//!
//! It is per statement and not per session for the reason ADR-0013 measured: a
//! scope held over a statement is also a scope over everything that statement
//! fires. Nothing here fires a user's trigger — this is DDL — but the rule is
//! the write path's, not this file's, and one exception invites a second.
//!
//! Two things this scope deliberately does **not** carry, both because a
//! setting cannot take effect in the batch it appears in (measured; see
//! [`crate::Postgres::transaction_framing`]): `standard_conforming_strings` and
//! `check_function_bodies`. They are pinned by the transaction framing, which
//! is the earlier batch every connection runs.
//!
//! # What is not here
//!
//! Modules, roles and reference data, each of which is its own Phase 5 step and
//! refuses by name through [`crate::Unbuilt`] until it arrives.

use pbps_dialect::{Created, DialectError, Statement};
use pbps_model::{
    Change, Column, ColumnType, ForeignKey, Index, PrimaryKey, ReferentialAction, Strategy, Table,
    TableName, UniqueConstraint,
};

use crate::types::DIALECT;
use crate::{Postgres, Unbuilt, quote, types};

type Sql = Result<Vec<Statement>, DialectError>;

fn invalid(message: String) -> DialectError {
    DialectError::Invalid {
        dialect: DIALECT,
        message,
    }
}

/// `"schema"."name"`, quoted on both halves.
fn qualified(t: &TableName) -> Result<String, DialectError> {
    Ok(format!("{}.{}", quote(&t.schema)?, quote(&t.name)?))
}

fn column_list(columns: &[String]) -> Result<String, DialectError> {
    Ok(columns
        .iter()
        .map(|c| quote(c))
        .collect::<Result<Vec<_>, _>>()?
        .join(", "))
}

/// The types whose *text* is read through a session-sensitive input function
/// (ADR-0013 §3), spelled as [`types::normalize`] leaves them.
///
/// Derived from the rule and not recalled: a type is here when the same
/// characters mean different values under different settings. Measured on 18.6,
/// with the identical declaration created under two `DateStyle`s:
///
/// ```text
/// DEFAULT '01/02/2026' on a date        ->  '2026-01-02'::date  /  '2026-02-01'::date
/// DEFAULT '01/02/2026 03:04' on a timestamptz
///                                       ->  '2026-01-02 03:04:00+00'  /  '2026-02-01 03:04:00+00'
/// ```
///
/// No error, no warning: a different value in the table, decided by whoever ran
/// the DDL. `time` and `time with time zone` are on the list because ADR-0013
/// put them there — `'12:00 CST'::timetz` is `12:00:00-06` under one
/// abbreviation dictionary and `12:00:00+09:30` under another — and narrowing a
/// recorded list because today's probe did not reach one of its rows is how the
/// list stops being the rule it was derived from.
const SETTING_SENSITIVE: &[&str] = &[
    "date",
    "time without time zone",
    "time with time zone",
    "timestamp without time zone",
    "timestamp with time zone",
    "interval",
    "real",
    "double precision",
];

/// Whether `expression` is one string literal and nothing else.
///
/// Not a parser, and it does not have to be one: the question is only whether
/// the whole expression is a single literal, in any of the spellings this
/// engine has for one. Anything with a cast, a call or an operator in it
/// answers `false` and is emitted as written.
///
/// **All four openers, and a first version had only the first.** A rule about
/// `'…'` alone would have let `E'01/02/2026'` and `$$01/02/2026$$` through on a
/// `date` column — the same text, the same session-decided value, and a
/// spelling a person copying from somewhere else would write. `U&'…' UESCAPE
/// '!'` is the one form left over: it is two literals with a keyword between
/// them, it answers `false`, and it is named here so that the gap is a recorded
/// one rather than a spelling nobody thought of.
fn is_a_bare_literal(expression: &str) -> bool {
    let e = expression.trim();
    if e.starts_with('$') {
        return is_one_dollar_quoted_literal(e);
    }
    // `E'…'` is the only one of these in which a backslash escapes; `U&'…'`
    // gives the backslash a meaning of its own (a Unicode escape) that does not
    // change where the literal *ends*, which is the only thing asked here.
    let (escapes, rest) = if let Some(r) = e.strip_prefix("E'").or_else(|| e.strip_prefix("e'")) {
        (true, r)
    } else if let Some(r) = e.strip_prefix("U&'").or_else(|| e.strip_prefix("u&'")) {
        (false, r)
    } else if let Some(r) = e.strip_prefix('\'') {
        (false, r)
    } else {
        return false;
    };
    let Some(inner) = rest.strip_suffix('\'') else {
        return false;
    };
    closes_only_at_the_end(inner, escapes)
}

/// Whether the quote that ends `inner` is the first one that could have.
///
/// A doubled quote is inside the literal; a single one would have closed it
/// early, which means the expression is more than this literal.
fn closes_only_at_the_end(inner: &str, escapes: bool) -> bool {
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if escapes => i += 2,
            b'\'' if bytes.get(i + 1) == Some(&b'\'') => i += 2,
            b'\'' => return false,
            // Every other byte, which includes the continuation bytes of a
            // multi-byte character: none of them can be one of the two above.
            _ => i += 1,
        }
    }
    // A step that ran past the end consumed the closing quote as an escaped
    // one, so the literal did not end there — `E'a\'` is not a closed literal.
    i == bytes.len()
}

/// Whether `e` is one `$tag$…$tag$` literal and nothing else.
fn is_one_dollar_quoted_literal(e: &str) -> bool {
    let Some(rest) = e.strip_prefix('$') else {
        return false;
    };
    let Some(at) = rest.find('$') else {
        return false;
    };
    let tag = &rest[..at];
    // The tag's own rule: empty, or a name that does not start with a digit.
    if !tag.is_empty()
        && !(tag.starts_with(|c: char| c.is_alphabetic() || c == '_')
            && tag.chars().all(|c| c.is_alphanumeric() || c == '_'))
    {
        return false;
    }
    let delim = format!("${tag}$");
    let Some(body) = e[delim.len()..].strip_suffix(&delim) else {
        return false;
    };
    // Nothing inside can close it, which is the whole point of the form — but
    // a *second* pair of the same tag would mean two literals side by side.
    !body.contains(&delim)
}

/// Refuses a default whose value would be decided by whoever applies it.
///
/// ADR-0013 §3: a plain-literal default on a setting-sensitive column reaches
/// the server as the resolved typed spelling, canonicalized by the engine at
/// plan time. This function is the offline half of that decision — the emitter
/// has no connection and cannot ask — and it refuses rather than guesses,
/// because the two spellings differ in the value stored and in nothing a later
/// read could tell apart.
///
/// A default that already carries a cast is what the engine itself reads back
/// (`pg_get_expr` welds one on, ADR-0013 §4), so a declaration pulled from a
/// live database is never refused here.
/// Its caller in `validate` is not a second guard, it is the *earlier* one, and
/// it is where the whole rule is actually enforced: [`Change::AlterColumnDefault`]
/// carries a `ColumnRef` and two expressions and **no type**, so the emitter
/// cannot ask this question on the one path that changes a default on a column
/// that already exists. `validate_table` sees the declaration, types and all,
/// and every command that hands statements to a database runs it
/// (DECISIONS 141).
pub(crate) fn refuse_an_unresolved_default(
    column: &str,
    ty: &ColumnType,
    default: &str,
) -> Option<DialectError> {
    if !SETTING_SENSITIVE.contains(&ty.base.as_str()) || !is_a_bare_literal(default) {
        return None;
    }
    Some(invalid(format!(
        "column `{column}` is `{ty}` and its default is the bare literal {default}. Two things \
         are wrong with that. What the text means is decided by the session that runs the \
         `CREATE` — measured, `'01/02/2026'` on a `date` stores 2026-01-02 under `DateStyle` MDY \
         and 2026-02-01 under DMY, with no error either way — and a bare literal is not the \
         spelling this engine reads a default back in, so the declaration and the database would \
         disagree on every plan after the first. Write it as the engine renders it, with the cast \
         it welds on: `'2026-01-02'::date` (ADR-0013 §3, §4)."
    )))
}

fn null_clause(nullable: bool) -> &'static str {
    if nullable { "NULL" } else { "NOT NULL" }
}

/// One line of a `CREATE TABLE` column list, or the body of an `ALTER TABLE ADD`.
fn column_definition(name: &str, column: &Column) -> Result<String, DialectError> {
    let mut s = format!("{} {}", quote(name)?, types::normalize(&column.ty)?);
    if let Some(id) = column.identity {
        // `GENERATED ALWAYS`, never `BY DEFAULT`: the model holds a seed and an
        // increment and nothing that could tell the two apart, and
        // introspection says so in as many words — read back, a `BY DEFAULT`
        // identity is indistinguishable from the one a plan would emit. The
        // sequence's bounds and its cache are left to the engine for the same
        // reason: those are the values `types::identity_seed_range` and the
        // pull's default-cache check expect to see.
        s.push_str(&format!(
            " GENERATED ALWAYS AS IDENTITY (START WITH {} INCREMENT BY {})",
            id.seed, id.increment
        ));
    }
    s.push(' ');
    s.push_str(null_clause(column.nullable));
    if let Some(expr) = &column.default {
        if let Some(e) = refuse_an_unresolved_default(name, &types::normalize(&column.ty)?, expr) {
            return Err(e);
        }
        s.push_str(&format!(" DEFAULT {expr}"));
    }
    Ok(s)
}

fn primary_key_clause(pk: &PrimaryKey) -> Result<String, DialectError> {
    let cols = column_list(&pk.columns)?;
    Ok(match &pk.name {
        Some(n) => format!("CONSTRAINT {} PRIMARY KEY ({cols})", quote(n)?),
        // Unnamed leaves the server to invent one, which is a choice a user can
        // make and is emitted faithfully rather than named on their behalf.
        None => format!("PRIMARY KEY ({cols})"),
    })
}

fn unique_clause(name: &str, u: &UniqueConstraint) -> Result<String, DialectError> {
    Ok(format!(
        "CONSTRAINT {} UNIQUE ({})",
        quote(name)?,
        column_list(&u.columns)?
    ))
}

const fn referential_action(a: ReferentialAction) -> &'static str {
    match a {
        ReferentialAction::NoAction => "NO ACTION",
        ReferentialAction::Cascade => "CASCADE",
        ReferentialAction::SetNull => "SET NULL",
        ReferentialAction::SetDefault => "SET DEFAULT",
    }
}

fn foreign_key_clause(name: &str, fk: &ForeignKey) -> Result<String, DialectError> {
    let mut s = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        quote(name)?,
        column_list(&fk.columns)?,
        qualified(&fk.references_table)?,
        column_list(&fk.references_columns)?
    );
    // NO ACTION is the default, and spelling out a default adds noise to a plan
    // a human has to read at a deployment gate.
    if fk.on_delete != ReferentialAction::NoAction {
        s.push_str(&format!(" ON DELETE {}", referential_action(fk.on_delete)));
    }
    if fk.on_update != ReferentialAction::NoAction {
        s.push_str(&format!(" ON UPDATE {}", referential_action(fk.on_update)));
    }
    Ok(s)
}

/// Whether this index would be built with `CONCURRENTLY`.
///
/// Two conditions, and the second is not a convenience. **Measured on 18.6**, a
/// concurrent build cannot share a batch with anything:
///
/// ```text
/// SET LOCAL search_path = m2s; CREATE INDEX CONCURRENTLY ix1 ON m2s.t (id);
///   -> ERROR: CREATE INDEX CONCURRENTLY cannot run inside a transaction block
/// ```
///
/// so the statement cannot carry the write `search_path` its filter would be
/// bound under, and a path set by a *preceding* statement is not there after a
/// staged apply resumes on a new connection. An index with a filter is
/// therefore built the ordinary way and the `online` hint is dropped — which
/// the trait allows and requires the reason for: a hint says how to get there,
/// so a dialect that cannot honour one emits the statement without it rather
/// than refusing a plan whose destination is the same either way. An index
/// without a filter has no expression to bind and needs no path at all, so
/// nothing is lost by leaving the scope off it.
const fn built_concurrently(index: &Index, strategy: Strategy) -> bool {
    strategy.online && index.filter.is_none()
}

fn create_index(
    table: &TableName,
    name: &str,
    index: &Index,
    strategy: Strategy,
) -> Result<String, DialectError> {
    let keys = index
        .columns
        .iter()
        .map(|c| {
            Ok(format!(
                "{} {}",
                quote(&c.name)?,
                if c.descending { "DESC" } else { "ASC" }
            ))
        })
        .collect::<Result<Vec<_>, DialectError>>()?
        .join(", ");

    let mut s = format!(
        "CREATE {}INDEX {}{} ON {} ({keys})",
        if index.unique { "UNIQUE " } else { "" },
        if built_concurrently(index, strategy) {
            "CONCURRENTLY "
        } else {
            ""
        },
        quote(name)?,
        qualified(table)?
    );
    if !index.include.is_empty() {
        s.push_str(&format!(" INCLUDE ({})", column_list(&index.include)?));
    }
    if let Some(filter) = &index.filter {
        s.push_str(&format!(" WHERE ({filter})"));
    }
    s.push(';');
    Ok(s)
}

/// Drops whatever primary key the table currently has.
///
/// A `DO` block when the declaration did not name it, for the reason SQL
/// Server's counterpart uses dynamic SQL: only a key pbps created carries a
/// predictable name, and adopting a database pbps did not create is the whole
/// point of `pull`. `quote_ident` and `quote_literal` do the quoting inside the
/// block, so a name the server invented cannot break out of it.
fn drop_primary_key(table: &TableName, pk: &PrimaryKey) -> Result<String, DialectError> {
    let q = qualified(table)?;
    Ok(match &pk.name {
        Some(n) => format!("ALTER TABLE {q} DROP CONSTRAINT {};", quote(n)?),
        // Both interpolations are in **literal** position and neither is in
        // code position, which is the trap: the name is inside a `DO` body and
        // then again inside the format string that body executes, so quoting it
        // as an identifier there would close the literal at the first
        // apostrophe. It is escaped for `format` first — a `%` in a name is a
        // placeholder to that function — and quoted as a literal after.
        None => {
            let body = format!(
                "DECLARE pk name := (SELECT conname FROM pg_catalog.pg_constraint\n\
                 \x20                    WHERE conrelid = {}::pg_catalog.regclass AND contype = 'p');\n\
                 BEGIN\n\
                 \x20   IF pk IS NOT NULL THEN\n\
                 \x20       EXECUTE pg_catalog.format({}, pk);\n\
                 \x20   END IF;\n\
                 END",
                literal(&q),
                literal(&format!(
                    "ALTER TABLE {} DROP CONSTRAINT %I",
                    q.replace('%', "%%")
                ))
            );
            let tag = dollar_tag(&body);
            format!("DO {tag}\n{body}\n{tag};")
        }
    })
}

/// A `$…$` tag that the body cannot close.
///
/// **The scan is the point.** PostgreSQL's lexer looks for a dollar-quote's
/// closing tag *literally*, without regard for quotes inside it, so a body
/// containing the tag ends the block there — and a table named `x$pbps$y` is a
/// legal identifier that would do exactly that, with the rest of the block
/// arriving as top-level SQL. Choosing a tag the body does not contain makes
/// that unrepresentable rather than checked for.
fn dollar_tag(body: &str) -> String {
    (0..)
        .map(|n| {
            if n == 0 {
                "$pbps$".to_owned()
            } else {
                format!("$pbps{n}$")
            }
        })
        .find(|tag| !body.contains(tag.as_str()))
        .expect("a body is finite and the tags are not")
}

/// A string literal, quoted the way the engine's own `quote_literal` does.
///
/// Doubling is the whole rule only while `standard_conforming_strings` is `on`,
/// which the transaction framing pins and which this crate's scanner already
/// assumes (ADR-0011 Amendment 2).
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The write `search_path` for an object in `schema`.
fn write_path(pg: &Postgres, schema: &str) -> Result<String, DialectError> {
    let mut parts = vec![quote(schema)?];
    for extra in pg.write_path_extras() {
        parts.push(quote(extra)?);
    }
    Ok(parts.join(", "))
}

/// One statement, under the write path of `schema`.
///
/// `SET` and not `SET LOCAL`: a rendered script is run statement by statement
/// outside any transaction, where `SET LOCAL` is a warning and a no-op, and a
/// scope that quietly does nothing is worse than none. The `RESET` afterwards
/// is in the same batch, so inside a transaction a failure rolls it back with
/// everything else, and outside one it returns the connection to the settings
/// the operator's environment gives it.
fn scoped(pg: &Postgres, schema: &str, body: &str) -> Result<Statement, DialectError> {
    Ok(Statement::new(format!(
        "SET search_path = {};\n{body}\nRESET search_path;",
        write_path(pg, schema)?
    )))
}

/// The same, for a table's own schema.
fn on(pg: &Postgres, table: &TableName, body: &str) -> Result<Statement, DialectError> {
    scoped(pg, &table.schema, body)
}

fn one(pg: &Postgres, table: &TableName, body: String) -> Sql {
    Ok(vec![on(pg, table, &body)?])
}

pub(crate) fn emit(pg: &Postgres, change: &Change, strategy: Strategy) -> Sql {
    match change {
        // The first statement is the one that brings the table into being; it
        // says so, and a staged checkpoint adopts the table from there.
        Change::CreateTable { name, table, .. } => {
            let mut out = create_table(pg, name, table)?;
            if let Some(first) = out.first_mut() {
                first.creates.push(Created::Table(name.clone()));
            }
            Ok(out)
        }

        Change::DropTable { name, .. } => {
            one(pg, name, format!("DROP TABLE {};", qualified(name)?))
        }

        // Two statements when both halves move, and neither engine has one that
        // does both: `RENAME TO` cannot cross a schema and `SET SCHEMA` cannot
        // rename. The transfer goes first, so the name in between is the old
        // one in the new schema — and each statement says what it does to the
        // name (`Statement::renaming`), because between them the table is
        // findable under neither the baseline's name nor the plan's.
        Change::RenameTable { from, to, .. } => rename_table(pg, from, to),

        // No `own_batch`, and that is the difference from T-SQL rather than an
        // omission: measured, `ALTER TABLE t ADD COLUMN c int; ALTER TABLE t
        // ADD CONSTRAINT ck CHECK (c > 0); CREATE INDEX ix ON t (c);` is
        // accepted as one batch here, because PostgreSQL analyses each
        // statement of a simple query when it reaches it. What it *does* read
        // up front is the whole batch's *lexis*, which is why
        // `standard_conforming_strings` is pinned by the framing and not here.
        Change::AddColumn {
            table,
            name,
            column,
            ..
        } => Ok(vec![
            on(
                pg,
                table,
                &format!(
                    "ALTER TABLE {} ADD COLUMN {};",
                    qualified(table)?,
                    column_definition(name, column)?
                ),
            )?
            .creating(Created::Column(table.clone(), name.clone())),
        ]),

        Change::DropColumn { column, .. } => one(
            pg,
            &column.table,
            format!(
                "ALTER TABLE {} DROP COLUMN {};",
                qualified(&column.table)?,
                quote(&column.name)?
            ),
        ),

        Change::RenameColumn {
            table, from, to, ..
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {};",
                qualified(table)?,
                quote(from)?,
                quote(to)?
            ),
        ),

        Change::AlterColumnType {
            column,
            from,
            to,
            from_nullable,
            to_nullable,
            ..
        } => {
            let normalized = types::normalize(to)?;
            let was = types::normalize(from)?;
            // No `USING`, ever (ADR-0012 §5). Where the engine would need one,
            // the change is refused here with the clause named, rather than
            // carried to the server to fail there — or, worse, performed under
            // a cast pbps chose, which is a data transformation nobody
            // declared, nobody reviewed and nobody can find in git.
            //
            // Both ends normalized first, and that is not a formality: the
            // catalogue's families are keyed on the spelling the engine gives
            // back, so `varchar(10)` unnormalized is a type it does not know
            // and every change from one reads as `Incompatible` — a widening
            // refused for needing a clause it does not need.
            if types::change_risk(&was, &normalized) == pbps_dialect::TypeChangeRisk::Incompatible {
                return Err(invalid(format!(
                    "column `{}` cannot be changed from `{}` to `{normalized}`: this engine \
                     refuses the conversion outright — `column \"{}\" cannot be cast \
                     automatically` — and its remedy is a `USING` clause, which pbps does not \
                     emit. A `USING` expression says what the data becomes, and that is a \
                     transformation to declare and review, not one for a tool to choose \
                     (ADR-0012 §5). Add the new column, fill it in a declared step, and drop the \
                     old one.",
                    column.name,
                    types::normalize(from)?,
                    column.name
                )));
            }
            // One `ALTER TABLE` takes both subcommands (ADR-0011, Amendment 1),
            // and the nullability is restated only when it moves: unlike SQL
            // Server, a `TYPE` subcommand here leaves `NOT NULL` where it was,
            // so restating it always would put a line in plan.sql that changes
            // nothing.
            let mut parts = vec![format!(
                "ALTER COLUMN {} TYPE {normalized}",
                quote(&column.name)?
            )];
            if from_nullable != to_nullable {
                parts.push(format!(
                    "ALTER COLUMN {} {}",
                    quote(&column.name)?,
                    if *to_nullable {
                        "DROP NOT NULL"
                    } else {
                        "SET NOT NULL"
                    }
                ));
            }
            one(
                pg,
                &column.table,
                format!(
                    "ALTER TABLE {} {};",
                    qualified(&column.table)?,
                    parts.join(", ")
                ),
            )
        }

        // `ty` is carried for SQL Server, which restates the whole column
        // definition and reads an omitted `NULL` as nullable. This engine has a
        // subcommand for exactly this and needs no type, so the field is unused
        // here — deliberately, and not because it was missed.
        Change::AlterColumnNullability {
            column,
            to_nullable,
            ..
        } => one(
            pg,
            &column.table,
            format!(
                "ALTER TABLE {} ALTER COLUMN {} {};",
                qualified(&column.table)?,
                quote(&column.name)?,
                if *to_nullable {
                    "DROP NOT NULL"
                } else {
                    "SET NOT NULL"
                }
            ),
        ),

        // A default is not a named object here — it is a property of the column
        // — so there is nothing to drop by name and no generated constraint
        // name to guess. `SET DEFAULT` replaces whatever was there.
        Change::AlterColumnDefault { column, to, .. } => one(
            pg,
            &column.table,
            match to {
                Some(expr) => format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {expr};",
                    qualified(&column.table)?,
                    quote(&column.name)?
                ),
                None => format!(
                    "ALTER TABLE {} ALTER COLUMN {} DROP DEFAULT;",
                    qualified(&column.table)?,
                    quote(&column.name)?
                ),
            },
        ),

        // Deprecation is a fact about the declarations, not about the database.
        // `COMMENT ON` would make it one, and that is a decision with its own
        // round trip to design; until then the honest output is nothing at all
        // rather than a statement that pretends to do something.
        Change::SetColumnDeprecated { .. } => Ok(Vec::new()),

        Change::SetPrimaryKey { table, from, to } => {
            let mut out = Vec::new();
            if let Some(pk) = from {
                out.push(on(pg, table, &drop_primary_key(table, pk)?)?);
            }
            if let Some(pk) = to {
                out.push(on(
                    pg,
                    table,
                    &format!(
                        "ALTER TABLE {} ADD {};",
                        qualified(table)?,
                        primary_key_clause(pk)?
                    ),
                )?);
            }
            Ok(out)
        }

        // No concurrent path for a unique constraint, and that is measured
        // rather than assumed: this engine builds the backing index under an
        // `ACCESS EXCLUSIVE` lock, and the online spelling is a two-step —
        // `CREATE UNIQUE INDEX CONCURRENTLY` then `ADD CONSTRAINT … USING
        // INDEX` — whose halves commit separately. A plan that half-applied
        // would be a change the gate never approved, so the hint is dropped
        // (see `built_concurrently`) rather than honoured by splitting one
        // declared constraint into two committed steps.
        Change::AddUnique {
            table,
            name,
            constraint,
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} ADD {};",
                qualified(table)?,
                unique_clause(name, constraint)?
            ),
        ),

        Change::AddForeignKey {
            table,
            name,
            constraint,
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} ADD {};",
                qualified(table)?,
                foreign_key_clause(name, constraint)?
            ),
        ),

        Change::AddCheck {
            table,
            name,
            constraint,
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({});",
                qualified(table)?,
                quote(name)?,
                constraint.expression
            ),
        ),

        Change::DropUnique { table, name }
        | Change::DropForeignKey { table, name }
        | Change::DropCheck { table, name } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} DROP CONSTRAINT {};",
                qualified(table)?,
                quote(name)?
            ),
        ),

        Change::AddIndex { table, name, index } => {
            let sql = create_index(table, name, index, strategy)?;
            if built_concurrently(index, strategy) {
                // Alone in its batch and outside the transaction, because the
                // engine says so in as many words: `CREATE INDEX CONCURRENTLY
                // cannot run inside a transaction block`. Saying it here rather
                // than in the runner is what lets a plan carrying one be
                // refused at plan time, with the whole plan intact, instead of
                // halfway through an apply.
                Ok(vec![Statement::new(sql).own_batch().non_transactional()])
            } else {
                Ok(vec![on(pg, table, &sql)?])
            }
        }

        // No `CONCURRENTLY` on the drop, deliberately. It would make the
        // statement non-transactional — and so the whole plan — to spare an
        // `ACCESS EXCLUSIVE` lock held for a catalog update, which is the one
        // part of an index's life that is not proportional to the table.
        Change::DropIndex { table, name } => one(
            pg,
            table,
            format!("DROP INDEX {}.{};", quote(&table.schema)?, quote(name)?),
        ),

        // The mode is a property of the declaration, not of the database: it
        // decides what future plans do about undeclared rows. The row changes
        // it implies are separate entries in this same plan.
        Change::SetDataMode { .. } => Ok(Vec::new()),

        Change::CreateModule { .. } | Change::AlterModule { .. } | Change::DropModule { .. } => {
            Err(Unbuilt::Modules.refuse())
        }
        Change::CreateRole { .. }
        | Change::DropRole { .. }
        | Change::RenameRole { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. } => Err(Unbuilt::Roles.refuse()),
        Change::InsertRow { .. } | Change::UpdateRow { .. } | Change::DeleteRow { .. } => {
            Err(Unbuilt::ReferenceData.refuse())
        }
    }
}

fn rename_table(pg: &Postgres, from: &TableName, to: &TableName) -> Sql {
    let mut out = Vec::new();
    let mut at = from.clone();
    if from.schema != to.schema {
        let moved = TableName::new(&to.schema, &from.name);
        out.push(
            on(
                pg,
                &at,
                &format!(
                    "ALTER TABLE {} SET SCHEMA {};",
                    qualified(&at)?,
                    quote(&to.schema)?
                ),
            )?
            .renaming(at.clone(), moved.clone()),
        );
        at = moved;
    }
    if at.name != to.name {
        out.push(
            on(
                pg,
                &at,
                &format!(
                    "ALTER TABLE {} RENAME TO {};",
                    qualified(&at)?,
                    quote(&to.name)?
                ),
            )?
            .renaming(at.clone(), to.clone()),
        );
    }
    Ok(out)
}

fn create_table(pg: &Postgres, name: &TableName, table: &Table) -> Sql {
    if table.columns.is_empty() {
        return Err(invalid(format!("table `{name}` has no columns")));
    }
    let q = qualified(name)?;

    let mut body: Vec<String> = Vec::new();
    for (col_name, column) in &table.columns {
        body.push(column_definition(col_name, column)?);
    }
    // The primary key goes inline; every other constraint is added afterwards,
    // so that creating a table and altering one take the same code path and
    // cannot drift apart.
    if let Some(pk) = &table.primary_key {
        body.push(primary_key_clause(pk)?);
    }

    let mut out = vec![on(
        pg,
        name,
        &format!("CREATE TABLE {q} (\n    {}\n);", body.join(",\n    ")),
    )?];

    for (n, u) in &table.unique {
        out.push(on(
            pg,
            name,
            &format!("ALTER TABLE {q} ADD {};", unique_clause(n, u)?),
        )?);
    }
    for (n, c) in &table.checks {
        out.push(on(
            pg,
            name,
            &format!(
                "ALTER TABLE {q} ADD CONSTRAINT {} CHECK ({});",
                quote(n)?,
                c.expression
            ),
        )?);
    }
    for (n, fk) in &table.foreign_keys {
        out.push(on(
            pg,
            name,
            &format!("ALTER TABLE {q} ADD {};", foreign_key_clause(n, fk)?),
        )?);
    }
    for (n, idx) in &table.indexes {
        // No online build here: the table was created by the statement above it
        // and holds no rows, so there is nothing for a concurrent build to
        // spare — and a concurrent one could not share this transaction.
        out.push(on(
            pg,
            name,
            &create_index(name, n, idx, Strategy::default())?,
        )?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_dialect::Dialect;
    use pbps_model::{CheckConstraint, ColumnType, Identity, IndexColumn, Uid, UidKind};

    fn ty(s: &str) -> ColumnType {
        s.parse().expect("a type parses")
    }

    fn name(schema: &str, table: &str) -> TableName {
        TableName::new(schema, table)
    }

    fn sql_of(pg: &Postgres, change: &Change) -> Vec<String> {
        pg.emit(change, Strategy::default())
            .expect("emit")
            .into_iter()
            .map(|s| s.sql)
            .collect()
    }

    /// The scope is the object's own schema first and the extras after it, in
    /// the order they were configured — because the order is what a name
    /// resolves through, and ADR-0013 measured a view keeping the binding its
    /// creation order gave it.
    #[test]
    fn every_statement_sets_the_write_path_and_gives_it_back() {
        let pg = Postgres::with_write_path_extras(vec!["shared".into(), "public".into()]);
        let sql = sql_of(
            &pg,
            &Change::DropTable {
                uid: Uid::generate(UidKind::Table),
                name: name("app", "t"),
            },
        );
        assert_eq!(
            sql,
            vec![
                "SET search_path = \"app\", \"shared\", \"public\";\n\
                 DROP TABLE \"app\".\"t\";\n\
                 RESET search_path;"
            ]
        );
    }

    /// With nothing configured the path is the object's own schema alone, which
    /// is the value every project has until there is a key to set.
    #[test]
    fn the_path_of_an_unconfigured_project_is_the_objects_own_schema() {
        let sql = sql_of(
            &Postgres::new(),
            &Change::DropIndex {
                table: name("app", "t"),
                name: "ix".into(),
            },
        );
        assert_eq!(
            sql,
            vec!["SET search_path = \"app\";\nDROP INDEX \"app\".\"ix\";\nRESET search_path;"]
        );
    }

    /// An extra that cannot be an identifier is refused where it is used, not
    /// silently dropped from the path: a path one schema short binds a name
    /// somewhere else and says nothing.
    #[test]
    fn an_extra_that_cannot_be_an_identifier_refuses_the_statement() {
        let pg = Postgres::with_write_path_extras(vec![String::new()]);
        let refusal = pg
            .emit(
                &Change::DropTable {
                    uid: Uid::generate(UidKind::Table),
                    name: name("app", "t"),
                },
                Strategy::default(),
            )
            .expect_err("an empty schema name is not an identifier");
        assert!(
            matches!(refusal, DialectError::UnquotableIdent(_)),
            "{refusal}"
        );
    }

    /// The bare-literal test is about the *whole* expression being one literal.
    /// Everything else is emitted as written, because everything else is an
    /// expression the engine resolves rather than a text a setting reads.
    #[test]
    fn only_a_whole_bare_literal_is_one() {
        for yes in [
            "'2026-01-02'",
            "  '2026-01-02'  ",
            "''",
            "'it''s'",
            // The three other spellings of one literal, each of which a rule
            // about `'…'` alone would have let through on a `date`.
            r"E'2026-01-02'",
            r"e'it\'s'",
            "$$2026-01-02$$",
            "$d$2026-01-02$d$",
            "U&'2026-01-02'",
        ] {
            assert!(is_a_bare_literal(yes), "{yes}");
        }
        for no in [
            "'2026-01-02'::date",
            "DATE '2026-01-02'",
            "'a' || 'b'",
            "now()",
            "'a', 'b'",
            "",
            "'",
            "current_date",
            // Two literals, not one, in each spelling.
            r"E'a' || E'b'",
            "$$a$$ || $$b$$",
            // The closing quote is escaped, so nothing closed the literal.
            r"E'a\'",
            // A tag that is not a tag, and a body that is not closed.
            "$1$a$1$",
            "$d$a$e$",
            // The recorded gap: two literals with a keyword between them.
            "U&'a' UESCAPE '!'",
        ] {
            assert!(!is_a_bare_literal(no), "{no}");
        }
    }

    /// `GENERATED ALWAYS`, and no bounds or cache: those are the values
    /// introspection expects to see, and a plan that wrote others would read
    /// back as an identity this model cannot tell from the one it declared.
    #[test]
    fn an_identity_is_generated_always_and_leaves_the_sequence_at_its_defaults() {
        let mut table = Table::default();
        let mut id = Column::new(ty("bigint")).not_null();
        id.identity = Some(Identity {
            seed: 7,
            increment: 3,
        });
        table.columns.insert("id".into(), id);
        let sql = sql_of(
            &Postgres::new(),
            &Change::CreateTable {
                uid: Uid::generate(UidKind::Table),
                name: name("app", "t"),
                table: Box::new(table),
            },
        );
        assert!(
            sql[0].contains("GENERATED ALWAYS AS IDENTITY (START WITH 7 INCREMENT BY 3)"),
            "{}",
            sql[0]
        );
        assert!(
            !sql[0].contains("MINVALUE") && !sql[0].contains("CACHE"),
            "{}",
            sql[0]
        );
    }

    /// A change that is a fact about the declarations and not about the
    /// database emits nothing — and that is not the same as an unbuilt part,
    /// which is an error.
    #[test]
    fn a_change_the_database_cannot_hold_emits_nothing_rather_than_pretending() {
        let deprecated = Change::SetColumnDeprecated {
            uid: Uid::generate(UidKind::Column),
            column: name("app", "t").column("c"),
            reason: Some("gone in March".into()),
        };
        let mode = Change::SetDataMode {
            table: name("app", "t"),
            from: Some(pbps_model::DataMode::Ensure),
            to: Some(pbps_model::DataMode::Exact),
        };
        for change in [deprecated, mode] {
            assert!(
                Postgres::new()
                    .emit(&change, Strategy::default())
                    .expect("emit")
                    .is_empty()
            );
        }
    }

    /// `NO ACTION` is this engine's default too, and spelling out a default
    /// adds a line to a plan a human reads at a deployment gate.
    #[test]
    fn a_referential_action_is_spelled_only_when_it_is_not_the_default() {
        let fk = |on_delete, on_update| ForeignKey {
            columns: vec!["a".into()],
            references_table: name("app", "parent"),
            references_columns: vec!["b".into()],
            on_delete,
            on_update,
        };
        let emit = |fk| {
            sql_of(
                &Postgres::new(),
                &Change::AddForeignKey {
                    table: name("app", "t"),
                    name: "fk".into(),
                    constraint: Box::new(fk),
                },
            )
            .remove(0)
        };
        let quiet = emit(fk(ReferentialAction::NoAction, ReferentialAction::NoAction));
        assert!(
            !quiet.contains("ON DELETE") && !quiet.contains("ON UPDATE"),
            "{quiet}"
        );
        let loud = emit(fk(
            ReferentialAction::SetDefault,
            ReferentialAction::Cascade,
        ));
        assert!(loud.contains("ON DELETE SET DEFAULT"), "{loud}");
        assert!(loud.contains("ON UPDATE CASCADE"), "{loud}");
    }

    /// A name is quoted wherever it goes, including inside the `DO` block that
    /// drops an unnamed key: there the table is in *literal* position, and the
    /// identifier quoting that is right in code position would be wrong.
    #[test]
    fn a_name_that_needs_quoting_is_quoted_in_both_positions() {
        let sql = sql_of(
            &Postgres::new(),
            &Change::SetPrimaryKey {
                table: name("odd schema", "it's"),
                from: Some(PrimaryKey {
                    name: None,
                    columns: vec!["id".into()],
                }),
                to: None,
            },
        );
        let block = &sql[0];
        assert!(
            block.contains("SET search_path = \"odd schema\";"),
            "{block}"
        );
        // Code position: doubled quotes. Literal position: doubled apostrophe —
        // and the statement `format` builds is a literal *inside* a literal, so
        // its own apostrophes are doubled a second time. Quoting the name as an
        // identifier there would have closed the format string at the `'`.
        assert!(
            block.contains("'\"odd schema\".\"it''s\"'::pg_catalog.regclass"),
            "{block}"
        );
        assert!(
            block.contains(
                "pg_catalog.format('ALTER TABLE \"odd schema\".\"it''s\" DROP CONSTRAINT %I', pk)"
            ),
            "{block}"
        );
        // The `DO` body is dollar-quoted, so that literal needs one level of
        // doubling and not two — and the tag is chosen so the body cannot end
        // it early.
        assert!(
            block.starts_with("SET search_path = \"odd schema\";\nDO $pbps$\n"),
            "{block}"
        );
    }

    /// A name carrying the tag would close the block and put the rest of it on
    /// the server as top-level SQL. The tag is chosen against the body, so
    /// there is no name that can do it.
    #[test]
    fn a_name_that_spells_the_dollar_tag_does_not_end_the_block() {
        let sql = sql_of(
            &Postgres::new(),
            &Change::SetPrimaryKey {
                table: name("app", "x$pbps$y"),
                from: Some(PrimaryKey {
                    name: None,
                    columns: vec!["id".into()],
                }),
                to: None,
            },
        );
        let block = &sql[0];
        assert!(block.contains("DO $pbps1$"), "{block}");
        assert_eq!(block.matches("$pbps1$").count(), 2, "{block}");
    }

    /// An index without a filter and without the hint is the ordinary case, and
    /// it is scoped like everything else; the concurrent one is the only
    /// statement here that is not, because it cannot be.
    #[test]
    fn only_the_concurrent_index_leaves_the_scope_off() {
        let index = Index {
            columns: vec![IndexColumn {
                name: "n".into(),
                descending: true,
            }],
            include: vec!["m".into()],
            unique: true,
            filter: None,
        };
        let change = Change::AddIndex {
            table: name("app", "t"),
            name: "ix".into(),
            index: Box::new(index),
        };
        let offline = sql_of(&Postgres::new(), &change).remove(0);
        assert!(
            offline.starts_with("SET search_path = \"app\";"),
            "{offline}"
        );
        assert!(
            offline.contains(
                "CREATE UNIQUE INDEX \"ix\" ON \"app\".\"t\" (\"n\" DESC) INCLUDE (\"m\");"
            ),
            "{offline}"
        );
        let online = Postgres::new()
            .emit(&change, Strategy { online: true })
            .expect("emit")
            .remove(0);
        assert_eq!(
            online.sql,
            "CREATE UNIQUE INDEX CONCURRENTLY \"ix\" ON \"app\".\"t\" (\"n\" DESC) INCLUDE (\"m\");"
        );
    }

    /// A check's expression reaches the server as written. The parentheses
    /// around it are the emitter's, so a declaration that is already
    /// parenthesised does not need to guess whether to add its own.
    #[test]
    fn a_declared_expression_is_emitted_verbatim() {
        let sql = sql_of(
            &Postgres::new(),
            &Change::AddCheck {
                table: name("app", "t"),
                name: "ck".into(),
                constraint: CheckConstraint {
                    expression: "(n > 0)".into(),
                },
            },
        )
        .remove(0);
        assert!(
            sql.contains("ADD CONSTRAINT \"ck\" CHECK ((n > 0));"),
            "{sql}"
        );
    }

    /// A table with no columns is not a table this engine will make, and the
    /// refusal names it rather than letting the server answer with a syntax
    /// error about a bracket.
    #[test]
    fn a_table_with_no_columns_is_refused_by_name() {
        let refusal = Postgres::new()
            .emit(
                &Change::CreateTable {
                    uid: Uid::generate(UidKind::Table),
                    name: name("app", "empty"),
                    table: Box::new(Table::default()),
                },
                Strategy::default(),
            )
            .expect_err("no columns");
        assert!(refusal.to_string().contains("app.empty"), "{refusal}");
    }
}
