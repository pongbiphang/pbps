//! Reading declared rows back from a table — the connected half of ADR-0004,
//! on PostgreSQL.
//!
//! # The engine spells the values, and the *session* decides how
//!
//! The SQL Server counterpart pins the spelling per expression: a fixed
//! `CONVERT` style for each type, chosen so the text never moves. PostgreSQL
//! has no per-expression style. What it has is a handful of session settings
//! that decide how every value of a type renders at once — `bytea_output`,
//! `DateStyle`, `IntervalStyle`, `TimeZone`, `extra_float_digits` — so the
//! spelling is fixed by the *scope the read runs in*
//! ([`crate::catalog::read_rows`] opens it), and every cell here is rendered by
//! the one expression `CAST(… AS text)`.
//!
//! That is the same rule as SPEC §8.2's, arriving through a different door: the
//! database is the normalizer, whatever spelling it gives a `numeric(5,2)` or a
//! `timestamptz` is the spelling the state records, both sides of a drift check
//! see the same one, and a declaration that wants to match it writes it that
//! way (`pull --data` shows it). Parsing values into typed Rust and formatting
//! them back would put a second formatter between the engine and the comparison
//! and let the two disagree about `1.5` and `1.50`.
//!
//! Only two kinds survive the trip as anything but text: `boolean` becomes a
//! boolean and the three integer types become integers, because those are the
//! two shapes a declaration can write unquoted and they have to compare equal
//! to what was written.
//!
//! # A cell that holds the default is read back both ways
//!
//! A declared row says "the default" by leaving the column out
//! ([`pbps_model::Row`]). The catalog holds a value, not the fact that it came
//! from the default — so the read-back asks the engine, per cell, whether the
//! value *equals* the column's default, and reports that beside the value
//! ([`pbps_model::ObservedRow::at_default`]).
//!
//! **Only a literal default is compared**, and on this engine a literal arrives
//! from the catalog with a cast welded on: `'unnamed'::text`, `0`,
//! `'x'::character varying` (ADR-0013 §4). [`is_constant`] reads through the
//! cast, and everything else — `now()`, `nextval('s'::regclass)`,
//! `gen_random_uuid()` — is not asked about, because that comparison would
//! *run* the expression once per row. Such a cell cannot be told from its
//! default ([`pbps_model::ObservedRow::unknown`]), so it is taken at the
//! declaration's word, exactly as the other dialect takes it.
//!
//! # What cannot be read
//!
//! A table whose live primary key is not a single column has rows with no
//! identity, and the read **fails** rather than returning nothing: "absent",
//! "empty" and "unreadable" are three different answers, and only one of them
//! is good news.

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::DbError;
use pbps_dialect::DialectError;
use pbps_model::{ObservedRow, Row, RowKey, RowScope, Table, TableName, Value};

use crate::emit::{qualified, value_literal};
use crate::quote;

/// Why a table's rows could not be read back.
#[derive(Debug, thiserror::Error)]
pub enum RowsError {
    #[error("{table}: its rows cannot be read back — {why}")]
    Unreadable { table: TableName, why: String },

    #[error("{table}: reading its rows back failed: {source}")]
    Read {
        table: TableName,
        // Boxed so the error is not larger than every `Ok` it travels beside.
        #[source]
        source: Box<DbError>,
    },

    /// The engine sent a value the mapping cannot hold — an integer column
    /// whose text does not parse, say. A bug in this file, not bad data.
    #[error("{table}.{column}: the engine sent `{text}`, which is not a {kind}")]
    BadValue {
        table: TableName,
        column: String,
        text: String,
        kind: &'static str,
    },

    #[error(transparent)]
    Dialect(#[from] DialectError),
}

/// What a column's text becomes once it is back in the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Bool,
    Int,
    Text,
}

impl ValueKind {
    /// The base name is the catalogue's (`types::normalize`), so `int4` and
    /// `bool` never reach here — `integer` and `boolean` do.
    pub fn of(base: &str) -> ValueKind {
        match base {
            "boolean" => ValueKind::Bool,
            "smallint" | "integer" | "bigint" => ValueKind::Int,
            _ => ValueKind::Text,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            ValueKind::Bool => "boolean",
            ValueKind::Int => "integer",
            ValueKind::Text => "text",
        }
    }
}

/// One selected column and where its pieces land in the result row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub column: String,
    pub kind: ValueKind,
    /// Position of the value in the result row.
    pub value_at: usize,
    /// Position of the "equals the default" flag, for a column whose default
    /// is a literal the engine can compare without running anything.
    pub default_at: Option<usize>,
    /// Whether the column has a default at all — which decides what a NULL
    /// means (see the module docs).
    pub has_default: bool,
    /// The column has a default that is *not* asked about — an expression the
    /// engine would have to run, or a type without `=` — so the cell is taken
    /// as at its default (module docs).
    pub assume_default: bool,
}

/// The query that reads one table's rows, and how to read its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowQuery {
    pub sql: String,
    pub key: Slot,
    pub columns: Vec<Slot>,
    /// A second query, when the read was asked to spell keys: each requested
    /// spelling beside the engine's spelling of the row it names
    /// ([`pbps_model::ObservedTable::aliases`]). The engine decides that `01`
    /// names the `integer` row `1`, exactly as it will when the emitter's
    /// `WHERE "id" = E'01'` runs.
    pub aliases: Option<String>,
}

/// The single primary-key column, or why there is none.
fn key_column(name: &TableName, table: &Table) -> Result<String, RowsError> {
    match &table.primary_key {
        Some(pk) if pk.columns.len() == 1 => Ok(pk.columns[0].clone()),
        Some(pk) => Err(RowsError::Unreadable {
            table: name.clone(),
            why: format!(
                "its primary key has {} columns ({}), and rows are keyed by one",
                pk.columns.len(),
                pk.columns.join(", ")
            ),
        }),
        None => Err(RowsError::Unreadable {
            table: name.clone(),
            why: "it has no primary key, so its rows have no identity".to_owned(),
        }),
    }
}

/// Builds the read for one table, or `None` when the scope names no row at
/// all — there is nothing to ask, and `IN ()` is not SQL.
pub fn query(
    name: &TableName,
    table: &Table,
    scope: &RowScope,
) -> Result<Option<RowQuery>, RowsError> {
    let key = key_column(name, table)?;
    let Some(key_spec) = table.columns.get(&key) else {
        return Err(RowsError::Unreadable {
            table: name.clone(),
            why: format!("its primary key column `{key}` is not among the columns read"),
        });
    };
    if let RowScope::Keys(keys) = scope
        && keys.is_empty()
    {
        return Ok(None);
    }
    let requested: Vec<String> = scope
        .known()
        .iter()
        .map(|k| value_literal(k.as_str()))
        .collect();

    let mut select = vec![read_expr(&quote(&key)?)];
    let key_slot = Slot {
        column: key.clone(),
        kind: ValueKind::of(&key_spec.ty.base),
        value_at: 0,
        default_at: None,
        has_default: key_spec.default.is_some(),
        assume_default: false,
    };

    let mut columns = Vec::new();
    for (column, spec) in &table.columns {
        if *column == key {
            continue;
        }
        // An identity column that is not the key is the engine's to assign: a
        // declaration cannot set it (the model refuses the cell) and an UPDATE
        // cannot change it. Read back, its value would be compared with the
        // omission every declaration has to make, and every connected plan
        // would restate an UPDATE the engine refuses. So it is never read:
        // both sides omit it, and omission agrees with omission (DECISIONS 94).
        if spec.identity.is_some() {
            continue;
        }
        let quoted = quote(column)?;
        let value_at = select.len();
        select.push(read_expr(&quoted));
        let asked = confirms_default(spec);
        let default_at = match &spec.default {
            Some(default) if asked => {
                // **The same comparison the write path holds a defaulted cell
                // to** (`emit::defaulted_cell`), and for the same two reasons.
                //
                // The declared side goes through the column's type first,
                // because the stored side did on the way in and both are then
                // read as text. And the two texts are compared byte for byte,
                // because the native `=` is the *column's* comparison, and a
                // column may have a collation that calls two different
                // spellings equal. **Measured on 18.6**, a `text` column under
                // `und-u-ks-level2` holding `New` beside `DEFAULT 'new'`:
                //
                // ```text
                // label = ('new')                    ->  t
                // the same, as text under COLLATE "C" ->  f
                // ```
                //
                // The first answer marks the cell at its default, so
                // `ObservedRow::as_seen_by` omits it, the drift is not in the
                // read-back at all, and no connected plan or `verify` ever
                // proposes to put the value back. This is the read half of
                // DECISIONS 329: the write half was fixed there and this call
                // site was not swept (DECISIONS 330).
                let stored = crate::emit::binary(&read_expr(&quoted));
                let declared = crate::emit::binary(&read_expr(&format!(
                    "CAST(({default}) AS {})",
                    crate::types::normalize(&spec.ty).unwrap_or_else(|_| spec.ty.clone())
                )));
                select.push(format!(
                    // Both halves, because `=` is UNKNOWN for a NULL on either
                    // side and `DEFAULT NULL` is a real declaration.
                    "CASE WHEN {stored} = {declared} OR ({quoted} IS NULL AND ({default}) IS NULL) \
                     THEN true ELSE false END"
                ));
                Some(select.len() - 1)
            }
            _ => None,
        };
        columns.push(Slot {
            column: column.clone(),
            kind: ValueKind::of(&spec.ty.base),
            value_at,
            default_at,
            has_default: spec.default.is_some(),
            assume_default: spec.default.is_some() && !asked,
        });
    }

    let mut sql = format!(
        "SELECT {}\n  FROM {}",
        select
            .iter()
            .enumerate()
            .map(|(i, e)| format!("{e} AS c{i}"))
            .collect::<Vec<_>>()
            .join(",\n       "),
        qualified(name)?
    );
    if let RowScope::Keys(keys) = scope {
        // The same rendering the emitter's `WHERE "key" = E'...'` uses: an
        // untyped literal the engine converts to the key column's type.
        // Measured, an `E'…'` string is `unknown` exactly as a plain one is,
        // so `"id" IN (E'1')` on an `integer` column is the integer comparison
        // and not a text one.
        let list: Vec<String> = keys.iter().map(|k| value_literal(k.as_str())).collect();
        sql.push_str(&format!(
            "\n WHERE {} IN ({})",
            quote(&key)?,
            list.join(", ")
        ));
    }
    sql.push(';');

    let aliases = if requested.is_empty() {
        None
    } else {
        let table = qualified(name)?;
        let key = quote(&key)?;
        // One scalar subquery per requested spelling rather than a join
        // against a `VALUES` list: a `VALUES` column resolves to `text`, and
        // `"id" = v.requested` on an `integer` column is then an operator that
        // does not exist. Inside the subquery the literal stays `unknown` and
        // the engine converts it to the key column's type — which is the
        // conversion this query exists to observe, and the one the emitter's
        // predicate will make.
        Some(
            requested
                .iter()
                .map(|r| {
                    format!(
                        "SELECT {r} AS requested, \
                         (SELECT {} FROM {table} AS pbps_table \
                         WHERE pbps_table.{key} = {r}) AS canonical",
                        read_expr(&format!("pbps_table.{key}"))
                    )
                })
                .collect::<Vec<_>>()
                .join("\nUNION ALL\n")
                + ";",
        )
    };

    Ok(Some(RowQuery {
        sql,
        key: key_slot,
        columns,
        aliases,
    }))
}

/// The expression that renders one column as the text the state will hold.
///
/// One expression for every type, where the SQL Server dialect needs a style
/// per family. The spelling is not fixed here at all — it is fixed by the
/// canonical settings the read runs under ([`crate::catalog::read_rows`]), and
/// a second mechanism on top of them would be a second thing to keep true.
///
/// **Measured on 18.6** under those settings, one row of every type this
/// dialect's catalogue admits:
///
/// ```text
/// numeric(5,2)  1.50           real 0.12345678   double precision 0.12345678901234568
/// bytea         \x0102         date 2026-01-02   interval P1DT2H
/// timestamptz   2026-01-15 12:00:00+00           timetz 12:00:00-06
/// character(5) holding 'ab'    ab
/// ```
///
/// The last is the one worth naming: a cast from `character(n)` to `text`
/// drops the padding (DECISIONS 322), which is what the engine itself does when it compares —
/// `'ab'::char(5) = 'ab'` — so a declaration does not have to count spaces to
/// agree with its own database.
///
/// **It is also what lets every comparison in this dialect reach every type.**
/// `json` is the one type in the catalogue with no `=` at all — measured,
/// `'{"a":1}'::json = '{"a":1}'::json` is `operator does not exist`, while
/// `jsonb` has one and `xml` is not in the catalogue — and a comparison
/// written through this expression never asks for that operator. The types
/// that keep a native `=` in this crate are the key and foreign-key columns,
/// and `json` cannot be one: measured, it has no default `btree` operator
/// class, so it cannot carry a primary key or a unique constraint and nothing
/// can reference it (DECISIONS 331).
pub(crate) fn read_expr(quoted: &str) -> String {
    format!("CAST({quoted} AS text)")
}

/// Whether the engine is asked to confirm a cell of this column at its
/// default: the default is a literal it can compare without running anything,
/// and the type has `=`. One function because two callers ask it — the row
/// reader, to decide what to put in the query, and the apply guard, to know
/// whether an omitted cell in the read-back *means* at-default (DECISIONS
/// 191) — and two spellings of it would drift.
pub fn confirms_default(spec: &pbps_model::Column) -> bool {
    spec.default.as_deref().is_some_and(is_constant)
}

/// Whether a default expression is a literal — which the engine can compare a
/// stored value against without running anything.
///
/// **The cast is the difference from the other engine.** PostgreSQL hands a
/// default back deparsed and typed: `'unnamed'` comes back `'unnamed'::text`,
/// `'x'` on a `varchar(9)` comes back `'x'::character varying`, and only a
/// bare numeric comes back as it went in (ADR-0013 §4). A reader that did not
/// look through the cast would call every string default an expression, ask
/// the engine about none of them, and read every such cell back as "cannot be
/// told from its default".
///
/// Conservative on purpose in the other direction: a literal read as "not a
/// literal" only costs the comparison, and `nextval('s'::regclass)` — which
/// ends in a cast and is emphatically not a literal — must never be read as
/// one, because asking about it would consume a sequence value (DECISIONS 323).
pub fn is_constant(default: &str) -> bool {
    let s = unwrapped(default);
    if s.is_empty() {
        return false;
    }
    if s.eq_ignore_ascii_case("null") {
        return true;
    }
    // **A bare boolean, which this engine deparses without a cast.**
    // Measured, `boolean DEFAULT true` comes back from `pg_get_expr` as the
    // one word `true` — so a predicate that admitted only NULL, quoted
    // strings and numbers called it an expression, never asked the engine
    // whether the cell equalled it, and read every such cell back as one
    // nobody can tell from its default. A hand edit that flipped the stored
    // value was then projected as an omission and no plan proposed to put it
    // back (DECISIONS 329).
    if s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false") {
        return true;
    }
    // A string in any spelling this engine reads — `'…'`, `E'…'`, `U&'…'`,
    // `N'…'`, `$tag$…$tag$`, and a literal continued across a newline — by
    // the reader the emitter already trusts to say where one ends; `'a' ||
    // 'b'` is not one. The catalog deparses every one of them to `'…'::text`,
    // so the other spellings reach here only from a declaration, verbatim
    // (DECISIONS 355).
    // By byte, because the first character need not be one byte — `é()` is
    // a valid expression and a slice at 1 would panic on it (DECISIONS 357).
    let b = s.as_bytes();
    let opener = |letter: u8, then: &[u8]| {
        b.first().is_some_and(|c| c.eq_ignore_ascii_case(&letter)) && b[1..].starts_with(then)
    };
    if s.starts_with(['\'', '$']) || opener(b'e', b"'") || opener(b'n', b"'") || opener(b'u', b"&'")
    {
        return crate::emit::is_a_bare_literal(s);
    }
    // A typed literal, `DATE '2026-02-01'`: a type name and a string, which
    // this engine reads as the one constant the catalog deparses to
    // `'2026-02-01'::date`, so the spelling reaches here only from a
    // declaration, as the other spellings of a string do (DECISIONS 367).
    if is_a_typed_literal(s) {
        return true;
    }
    // A sign, and whatever trivia, parentheses and casts stand between it
    // and its operand, any number of times over. **Measured** on 18.6:
    // `- 1`, `- /* c */ 1`, `- - 1` and `+-1` are each accepted and folded,
    // and the catalog itself deparses a declared `+1` as `(+ 1)` and `+-1`
    // as `(+ '-1'::integer)`, so a rule that took the sign off and read the
    // space after it as part of the number called every read-back positive
    // default an expression. A sign before a bare string, `NULL` or a
    // boolean the engine refuses by name (`operator is not unique: -
    // unknown`), so what follows the signs is a number or a cast the
    // engine already resolved, `(- NULL::integer)` (DECISIONS 365).
    let mut rest = s;
    while let Some(after) = rest.strip_prefix(['-', '+']) {
        rest = unwrapped(after);
    }
    if rest.len() == s.len() {
        return is_a_number(rest);
    }
    !rest.is_empty() && is_constant(rest)
}

/// Whether `s` is a SQL-standard typed literal, a type name followed by one
/// string: `DATE '2026-02-01'`, `INTERVAL '1' DAY`, `NUMERIC(5,2) '1.5'`,
/// `pg_catalog.date E'…'`, `date'…'` with no gap at all.
///
/// **Measured** on 18.6: each of those is accepted and stored as the cast
/// literal, `'2026-02-01'::date`, `'00:01:00'::interval`, `1.5::numeric(5,2)`;
/// the string may take any of its spellings, `E'…'`, `U&'…'`, `$$…$$`; a
/// comment may stand between the type and the string; an interval's field
/// words may follow the string. `DATE ('…')` and `TEXT[] '{a}'` are syntax
/// errors, and a type name the catalog lacks — `foo 'x'`, `lower 'x'` — is
/// refused by name, so a word that is no type admits nothing the engine
/// stores. What follows the string may be words alone: `TEXT 'a' || 'b'` is
/// an expression (DECISIONS 367).
fn is_a_typed_literal(s: &str) -> bool {
    let bytes = s.as_bytes();
    // The first quote that opens a string, read past any comment before it.
    let mut i = 0;
    let start = loop {
        if i >= bytes.len() {
            return false;
        }
        match bytes[i] {
            b'\'' | b'$' => break i,
            b'-' | b'/' => match comment_end(s, i) {
                Some(end) => i = end,
                None if s[i..].starts_with("/*") => return false,
                None => i += 1,
            },
            _ => i += 1,
        }
    };
    // The letter or `U&` that opens `E'…'`, `N'…'` or `U&'…'` belongs to the
    // string, unless it ends the type's own word, as the `e` of `date'…'`
    // does — the rule `string_end` reads an escape prefix by.
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut head_end = start;
    if bytes[start] == b'\'' {
        if start >= 1
            && (bytes[start - 1].eq_ignore_ascii_case(&b'e')
                || bytes[start - 1].eq_ignore_ascii_case(&b'n'))
            && !(start >= 2 && is_word(bytes[start - 2]))
        {
            head_end = start - 1;
        } else if start >= 2
            && bytes[start - 1] == b'&'
            && bytes[start - 2].eq_ignore_ascii_case(&b'u')
            && !(start >= 3 && is_word(bytes[start - 3]))
        {
            head_end = start - 2;
        }
    }
    let Some(ty) = type_text(&s[..head_end]) else {
        return false;
    };
    if ty.is_empty() || ty.contains('[') || !looks_like_a_type(&ty) {
        return false;
    }
    let tail = &s[head_end..];
    if crate::emit::is_a_bare_literal(tail) {
        return true;
    }
    // The string, then an interval's field qualifier and nothing else: not
    // any words — `BOOLEAN 'false' OR flip()` is an expression, and one a
    // probe must not evaluate a second time (DECISIONS 368).
    let Some(end) = string_end(tail, start - head_end) else {
        return false;
    };
    crate::emit::is_a_bare_literal(&tail[..end]) && is_an_interval_qualifier(&tail[end..])
}

/// Whether `s` is one of the field qualifiers the grammar lets follow an
/// `INTERVAL` literal — `DAY`, `HOUR TO MINUTE`, `SECOND(3)`, `DAY TO
/// SECOND (2)` — read past comments and case. **Measured** on 18.6: each of
/// those is accepted; `DAYS` and `TO DAY` are syntax errors (DECISIONS 368).
fn is_an_interval_qualifier(s: &str) -> bool {
    const FIELDS: [&str; 6] = ["year", "month", "day", "hour", "minute", "second"];
    let Some(text) = type_text(s) else {
        return false;
    };
    let lower = text.to_ascii_lowercase();
    // One precision, `(n)`, at the very end and only after `second`.
    let (words, precision) = match lower.split_once('(') {
        Some((head, tail)) => match tail.strip_suffix(')') {
            Some(n) if !n.trim().is_empty() && n.trim().bytes().all(|b| b.is_ascii_digit()) => {
                (head, true)
            }
            _ => return false,
        },
        None => (lower.as_str(), false),
    };
    let words: Vec<&str> = words.split_whitespace().collect();
    let last_is_second = words.last() == Some(&"second");
    if precision && !last_is_second {
        return false;
    }
    match words.as_slice() {
        [field] => FIELDS.contains(field),
        [from, "to", to] => FIELDS.contains(from) && FIELDS.contains(to),
        _ => false,
    }
}

/// Whether `s` is one numeric literal in a spelling this engine reads.
///
/// **Measured** on 18.6: an underscore may stand between two digits of the
/// integer part, the fraction or the exponent (`1_000.000_1`, `1e1_0`), and
/// right after a base prefix (`0x_F`), never at either end or doubled; a
/// base-prefixed integer — `0xFF`, `0o17`, `0b101` — has no fraction and no
/// exponent (`0xFF.5` is a syntax error); and `1.`, `.5` and `1e5` are the
/// decimal shapes they always were (DECISIONS 360).
fn is_a_number(s: &str) -> bool {
    // Digits of `radix` with single underscores between them; `leading`
    // admits one underscore before the first digit, for the base prefix.
    let digits = |part: &str, radix: u32, leading: bool| {
        let part = if leading {
            part.strip_prefix('_').unwrap_or(part)
        } else {
            part
        };
        !part.is_empty()
            && !part.starts_with('_')
            && !part.ends_with('_')
            && !part.contains("__")
            && part.chars().all(|c| c == '_' || c.is_digit(radix))
    };
    let lower = s.to_ascii_lowercase();
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            return digits(rest, radix, true);
        }
    }
    let (mantissa, exponent) = match lower.split_once('e') {
        Some((m, e)) => (m, Some(e)),
        None => (lower.as_str(), None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some((int, frac)) => {
            (int.is_empty() || digits(int, 10, false))
                && (frac.is_empty() || digits(frac, 10, false))
                && !(int.is_empty() && frac.is_empty())
        }
        None => digits(mantissa, 10, false),
    };
    mantissa_ok
        && exponent.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e), 10, false))
}

/// A default with its parentheses, its casts and its comments taken off,
/// which is the shape the tests below and the pre-delete probe both ask
/// questions of.
///
/// A comment is whitespace to this engine, at either end of the text and at
/// either end of what a cast or a grouping wraps: **measured**, `NULL /* note
/// */::text`, `CAST(NULL /* note */ AS text)`, `/* lead */ NULL` and a
/// parenthesised `NULL -- line` all leave the column with no default at all,
/// as a bare `NULL` does (DECISIONS 351, 358). Read by the emitter's own
/// readers of trivia, which know that an unterminated comment is not trivia.
pub(crate) fn unwrapped(default: &str) -> &str {
    unwrapped_with_type(default).0
}

/// [`unwrapped`], and the type of the outermost cast taken off on the way —
/// the type the whole expression has, which is what the engine coerces to
/// the column's (DECISIONS 361). `None` where nothing was cast.
pub(crate) fn unwrapped_with_type(default: &str) -> (&str, Option<String>) {
    let mut s = default.trim();
    let mut outermost = None;
    loop {
        let before = s;
        s = crate::emit::without_trailing_trivia(crate::emit::after_the_gap(s).0);
        while s.len() >= 2 && s.starts_with('(') && s.ends_with(')') {
            s = s[1..s.len() - 1].trim();
        }
        if let Some((rest, ty)) = without_a_cast_typed(s) {
            outermost.get_or_insert(ty);
            s = rest;
        }
        if let Some((rest, ty)) = without_a_cast_call_typed(s) {
            outermost.get_or_insert(ty);
            s = rest;
        }
        if s == before {
            return (s, outermost);
        }
    }
}

/// The byte index just past the comment that opens at `at` in `expr` — a `--`
/// to its newline, or a `/*` to the `*/` that closes it, nested ones counted —
/// or `None` where nothing closes a block comment, which is text the engine
/// refuses by name and not a gap (DECISIONS 359). `None` too where nothing
/// opens at `at`.
fn comment_end(expr: &str, at: usize) -> Option<usize> {
    let rest = &expr[at..];
    if let Some(after) = rest.strip_prefix("--") {
        // At a newline in either spelling, as the emitter's readers end one
        // (`emit::NEWLINE`): a rule written for `\n` alone reads the rest of
        // the expression as commented (DECISIONS 363).
        return Some(
            after
                .find(crate::emit::NEWLINE)
                .map_or(expr.len(), |n| at + 2 + n + 1),
        );
    }
    let after = rest.strip_prefix("/*")?;
    crate::emit::end_of_block_comment(after).map(|tail| expr.len() - tail.len())
}

/// The byte index just past the string literal that opens at `at` in `expr` —
/// a `'` or the `$` of a `$tag$` — or `None` where nothing closes it, or `$`
/// opens nothing. A `'` opened by `E` (and not by an identifier ending in
/// `e`) takes backslash escapes, so `E'it\'s'` is one literal and not a
/// string ending at its apostrophe; a doubled `''` is one quote in any
/// string; a dollar-quoted string ends at its own tag and nothing inside it
/// — a `'` or a `::` — is structure (DECISIONS 357).
fn string_end(expr: &str, at: usize) -> Option<usize> {
    let bytes = expr.as_bytes();
    if bytes[at] == b'$' {
        let tag = crate::emit::dollar_delimiter(&expr[at..])?;
        let body = at + tag.len();
        return expr[body..].find(tag).map(|end| body + end + tag.len());
    }
    let escapes = at >= 1
        && bytes[at - 1].eq_ignore_ascii_case(&b'e')
        && !(at >= 2 && (bytes[at - 2].is_ascii_alphanumeric() || bytes[at - 2] == b'_'));
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if escapes => i += 2,
            b'\'' if bytes.get(i + 1) == Some(&b'\'') => i += 2,
            b'\'' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// The byte index just past the quoted identifier that opens at `at` in
/// `expr` — a `""` inside it is one quote, not its end — or `None` where
/// nothing closes it (DECISIONS 364).
fn identifier_end(expr: &str, at: usize) -> Option<usize> {
    let bytes = expr.as_bytes();
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' if bytes.get(i + 1) == Some(&b'"') => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// `expr` with an enclosing SQL-standard `CAST(… AS type)` removed, and that
/// type, or `None` where the text is not one.
///
/// The engine deparses every cast as `…::type`, so this spelling never comes
/// out of the catalog — it comes out of a declaration, which keeps the user's
/// text, and a `DEFAULT CAST(NULL AS text)` the engine does not even store
/// (measured: no `pg_attrdef` row at all) is still the NULL the plan writes
/// (DECISIONS 350). The `AS` has to be the last one at the top level outside
/// a string literal, and what follows it a type name as [`without_a_cast`]
/// admits one.
fn without_a_cast_call_typed(expr: &str) -> Option<(&str, String)> {
    let rest = expr.trim();
    let head = rest.get(..4)?;
    if !head.eq_ignore_ascii_case("cast") {
        return None;
    }
    // `CAST /* c */ (…)`: a comment between the keyword and its parenthesis
    // is whitespace to the engine (DECISIONS 359).
    let inner = crate::emit::after_the_gap(&rest[4..])
        .0
        .strip_prefix('(')?
        .strip_suffix(')')?;
    // The closing parenthesis has to be the opening one's: `CAST(a AS
    // text) || CAST(b AS text)` ends in `)` too.
    let bytes = inner.as_bytes();
    let mut depth = 0i32;
    let mut last_as = None;
    // Whether the keyword `AS` may start at the byte at `i`: what stands
    // before it is a gap — whitespace or a comment, `NULL/**/AS/**/text`
    // is the engine's `NULL AS text` (DECISIONS 363) — or the end of a
    // token the lexer closes by itself, a string literal, a quoted
    // identifier or a `)`: **measured**, `CAST('keep'AS text)`,
    // `CAST($$x$$AS text)` and `CAST((NULL)AS text)` are each accepted,
    // while `1AS` is "trailing junk after numeric literal" and `NULLAS` one
    // word (DECISIONS 364).
    let mut after_gap = true;
    // Whether the keyword `AS` may end at the byte at `i`: a gap, or the
    // `"` of a quoted type name — `CAST(NULL AS"text")` is accepted, and
    // `AS(text)` is not (DECISIONS 364).
    let opens_a_gap = |i: usize| {
        bytes
            .get(i)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'"')
            || inner[i.min(inner.len())..].starts_with("/*")
            || inner[i.min(inner.len())..].starts_with("--")
    };
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'$' => {
                i = string_end(inner, i)?;
                after_gap = true;
                continue;
            }
            b'"' => {
                // A quoted identifier, a doubled `""` one quote inside it
                // (DECISIONS 364).
                i = identifier_end(inner, i)?;
                after_gap = true;
                continue;
            }
            b'-' | b'/' if comment_end(inner, i).is_some() => {
                i = comment_end(inner, i)?;
                after_gap = true;
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => return None,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            b'a' | b'A'
                if depth == 0
                    && after_gap
                    && bytes
                        .get(i + 1)
                        .is_some_and(|b| b.eq_ignore_ascii_case(&b's'))
                    && opens_a_gap(i + 2) =>
            {
                last_as = Some(i);
            }
            _ => {}
        }
        after_gap = bytes[i].is_ascii_whitespace() || bytes[i] == b')';
        i += 1;
    }
    if depth != 0 {
        return None;
    }
    let at = last_as?;
    let ty = type_text(&inner[at + 2..])?;
    looks_like_a_type(&ty).then(|| (inner[..at].trim(), ty))
}

/// Whether `name` is a type name and nothing else: words of letters, digits,
/// `_`, `.` and quotes, one `(5,2)` modifier among them — before a trailing
/// `with time zone`, after `interval day to second` — and any number of
/// array markers at the end, `[]`, `[2]`, `ARRAY[2]`, spaced or not.
/// **Measured**, every one of those is a type this engine casts to
/// (DECISIONS 361).
fn looks_like_a_type(name: &str) -> bool {
    let mut rest = name.trim();
    while let Some(open) = rest.rfind('[') {
        let Some(inside) = rest[open + 1..].strip_suffix(']') else {
            break;
        };
        if !inside.trim().bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        rest = rest[..open].trim_end();
    }
    let words = match rest.split_once('(') {
        Some((head, tail)) => {
            let Some((args, after)) = tail.split_once(')') else {
                return false;
            };
            if !args
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b',' || b == b' ')
            {
                return false;
            }
            format!("{head} {after}")
        }
        None => rest.to_owned(),
    };
    let words = words.trim();
    !words.is_empty()
        && words
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b' ' | b'"'))
}

/// `expr` with one trailing `::type` removed, and that type, or `None` where
/// the text does not end in a cast.
///
/// The `::` has to be the *last* one outside a string literal, and what
/// follows it has to look like a type name and nothing else: letters, digits,
/// `_`, `.`, spaces and quotes, with an optional `(5,2)` modifier or `[]`.
/// That is what separates `'unnamed'::text` from `'a'::text || 'b'`, whose
/// trailing `::text` is in the middle of an expression.
fn without_a_cast_typed(expr: &str) -> Option<(&str, String)> {
    let bytes = expr.as_bytes();
    let mut last = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'$' => {
                i = string_end(expr, i)?;
                continue;
            }
            b'-' | b'/' if comment_end(expr, i).is_some() => {
                i = comment_end(expr, i)?;
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => return None,
            b':' if bytes.get(i + 1) == Some(&b':') => {
                last = Some(i);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    let at = last?;
    let ty = type_text(&expr[at + 2..])?;
    looks_like_a_type(&ty).then(|| (expr[..at].trim_end(), ty))
}

/// The text of a type with every comment inside it read as the whitespace it
/// is to the engine, runs of whitespace as one space, and nothing at either
/// end — or `None` where a block comment never closes, which is text the
/// engine refuses by name. **Measured**: `double /* note */ precision`,
/// `timestamp /* c */ with time zone` and `text /* c */ []` are each the type
/// without the comment (DECISIONS 362).
fn type_text(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match comment_end(raw, i) {
            Some(end) => {
                out.push(' ');
                i = end;
            }
            None if raw[i..].starts_with("/*") => return None,
            None => {
                let c = raw[i..].chars().next()?;
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    Some(out.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// One cell's text as a model value.
pub fn value_of(kind: ValueKind, text: &str) -> Option<Value> {
    Some(match kind {
        // The engine's own spelling of a boolean cast to text, and only it: a
        // `t`/`f` would mean the read went through some other rendering.
        ValueKind::Bool => match text {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return None,
        },
        ValueKind::Int => Value::Int(text.parse().ok()?),
        ValueKind::Text => Value::Text(text.to_owned()),
    })
}

/// The canonical cell: `None` means the column is omitted from the row.
///
/// The rules are in the module docs; they are what make two reads of the same
/// table produce the same [`Row`], which `StateSnapshot::matches` relies on.
pub fn canonical(slot: &Slot, text: Option<&str>) -> Result<Option<Value>, &'static str> {
    match text {
        None if !slot.has_default => Ok(None),
        None => Ok(Some(Value::Null)),
        Some(t) => value_of(slot.kind, t).map(Some).ok_or(slot.kind.name()),
    }
}

/// Reads one result row into the model.
pub fn decode(
    name: &TableName,
    query: &RowQuery,
    row: &pbps_db::Row,
) -> Result<(RowKey, ObservedRow), RowsError> {
    let read = |table: &TableName, source: DbError| RowsError::Read {
        table: table.clone(),
        source: Box::new(source),
    };
    let key_text: Option<&str> = row
        .try_get_at(query.key.value_at)
        .map_err(|e| read(name, e))?;
    let Some(key_text) = key_text else {
        // A NULL primary key cannot exist; a NULL here means the query and the
        // slots disagree.
        return Err(read(
            name,
            DbError::BadRow("the key column came back NULL".to_owned()),
        ));
    };

    let mut cells = BTreeMap::new();
    let mut at_default = BTreeSet::new();
    let mut unknown = BTreeSet::new();
    for slot in &query.columns {
        let text: Option<&str> = row.try_get_at(slot.value_at).map_err(|e| read(name, e))?;
        // Three answers, not two: the engine said it is the default, the
        // engine said it is not, or the engine was never asked (a default it
        // would have had to run). The third is not the first: a
        // `gen_random_uuid()` key or a `now()` stamp holds a value nobody can
        // tell from its default, and `pull` has to write that value, not drop
        // it.
        let confirmed = match slot.default_at {
            Some(at) => row
                .try_get_at::<bool>(at)
                .map_err(|e| read(name, e))?
                .unwrap_or(false),
            None => false,
        };
        match canonical(slot, text) {
            Ok(Some(v)) => {
                cells.insert(slot.column.clone(), v);
                if confirmed {
                    at_default.insert(slot.column.clone());
                } else if slot.assume_default {
                    unknown.insert(slot.column.clone());
                }
            }
            Ok(None) => {}
            Err(kind) => {
                return Err(RowsError::BadValue {
                    table: name.clone(),
                    column: slot.column.clone(),
                    text: text.unwrap_or_default().to_owned(),
                    kind,
                });
            }
        }
    }
    Ok((
        RowKey::from(key_text),
        ObservedRow {
            cells: Row(cells),
            at_default,
            unknown,
        },
    ))
}

/// Reads one row of the alias query: the requested spelling and the engine's.
pub fn decode_alias(
    name: &TableName,
    row: &pbps_db::Row,
) -> Result<(RowKey, Option<RowKey>), RowsError> {
    let read = |source: DbError| RowsError::Read {
        table: name.clone(),
        source: Box::new(source),
    };
    let requested: Option<&str> = row.try_get_at(0).map_err(read)?;
    let canonical: Option<&str> = row.try_get_at(1).map_err(read)?;
    let Some(requested) = requested else {
        // The requested spelling is a literal this query put there; a NULL
        // means the query and this reader disagree.
        return Err(read(DbError::BadRow(
            "the alias query returned a NULL requested key".to_owned(),
        )));
    };
    // A NULL canonical is the table not holding that row, which is ordinary:
    // the scalar subquery answers NULL where the join it replaced produced no
    // row at all.
    Ok((RowKey::from(requested), canonical.map(RowKey::from)))
}

/// One declared spelling the engine reads back differently, or cannot read
/// at all (DECISIONS 101).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Misspelt {
    pub table: TableName,
    pub key: RowKey,
    /// `None` for the key itself.
    pub column: Option<String>,
    pub declared: String,
    /// The column's type, as the engine was asked to read the text.
    pub ty: String,
    /// What the engine reads back; `None` when it cannot convert the text.
    pub canonical: Option<String>,
}

/// Every declared literal of one column, sent to the engine to be read the
/// way the read-back reads it, beside its index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpellingQuery {
    /// `None` for the key column.
    pub column: Option<String>,
    pub ty: String,
    pub literals: Vec<(RowKey, String)>,
    pub sql: String,
    /// For the key column only: the query that groups the keys by what the
    /// engine reads them as and returns each group of more than one, as the
    /// indexes of two of its members and the spelling the engine gives the
    /// group. Two keys the engine reads as one row — `1` and `01` for an
    /// `integer`, `New` and `new` under a case-insensitive collation — would
    /// insert twice and fail on the second, and the alias query cannot see
    /// them on a table that does not hold either yet (DECISIONS 106).
    pub collisions: Option<String>,
}

/// What the catalog calls a table, its key column and that column's collation
/// *now*, where the plan about to be checked renames them.
///
/// The spelling checks run before a statement of the plan has run, so the
/// database still has the old names — and the collation, which is the one
/// input here that is read from the catalog rather than declared, is read
/// under those names. Absent entries mean "as declared", which is right for
/// every table a plan does not rename and for one it has yet to create
/// (DECISIONS 148).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalogued {
    pub table: Option<TableName>,
    pub key_column: Option<String>,
    /// The key column's collation, schema and name, where it has one that is
    /// not the database default. `None` is not "the default collation" as a
    /// value — it is "nothing to write", which leaves the comparison to
    /// whatever the expression's own collation is, and that is the right
    /// answer for a table this plan is about to create: the emitter writes no
    /// `COLLATE`, so its column will be created with the database's default.
    pub key_collation: Option<(String, String)>,
}

/// Every declared table's catalog names, keyed by the declared table name.
pub type CatalogNames = std::collections::BTreeMap<TableName, Catalogued>;

/// The engine's own answer to "is this text the spelling it reads back":
/// every declared text cell and every key, converted to its column's type and
/// rendered exactly as [`read_expr`] renders a stored value.
///
/// A declaration written `"1.5"` for a `numeric(5,2)` is stored as `1.50` and
/// read back as `1.50`, and every connected plan then restates an update that
/// changes nothing. Neither the model nor this crate can spell a value the
/// engine's way without becoming the engine, so the engine is asked, before
/// anything is written, and a declaration that disagrees is refused with the
/// spelling to write.
///
/// **`pg_input_is_valid` is this engine's `TRY_CONVERT`, and it is stricter in
/// the direction that matters.** Measured: `pg_input_is_valid('abcdefghijk',
/// 'character varying(10)')` is false, while `CAST('abcdefghijk' AS character
/// varying(10))` silently returns the first ten characters. Asking the
/// question the strict way is what turns "a value the table quietly truncates"
/// into "a value the engine cannot read as this type", which is what the
/// declaration's author needs to be told.
///
/// Integer and boolean cells are parsed by the loader and spelled by the
/// model; only text-kind columns carry a spelling to ask about.
pub fn spelling_queries(
    name: &TableName,
    table: &Table,
    at: &Catalogued,
) -> Result<Vec<SpellingQuery>, RowsError> {
    let Some(data) = &table.data else {
        return Ok(Vec::new());
    };
    let key = key_column(name, table)?;
    let ty_of = |column: &str| -> Result<(String, String), RowsError> {
        let spec = table
            .columns
            .get(column)
            .ok_or_else(|| RowsError::Unreadable {
                table: name.clone(),
                why: format!("`{column}` is not among its columns"),
            })?;
        let ty = crate::types::normalize(&spec.ty).map_err(|e| RowsError::Unreadable {
            table: name.clone(),
            why: e.to_string(),
        })?;
        Ok((ty.base.clone(), ty.to_string()))
    };
    let query = |column: Option<String>,
                 base: &str,
                 ty: String,
                 literals: Vec<(RowKey, String)>|
     -> Result<SpellingQuery, RowsError> {
        let readable = format!("pg_catalog.pg_input_is_valid(v.s, {})", value_literal(&ty));
        let rendered = read_expr(&format!("CAST(v.s AS {ty})"));
        let values = literals
            .iter()
            .enumerate()
            .map(|(i, (_, text))| format!("({i}, {})", value_literal(text)))
            .collect::<Vec<_>>()
            .join(", ");
        // `OFFSET 0` is an optimization fence, and it is the whole reason
        // these queries do not raise on the very text they exist to report.
        // **Measured**: with two rows in the `VALUES`, the `CASE` above
        // protects the cast and an unreadable value comes back NULL; with
        // **one** row the planner folds the list into a `Result` node and
        // evaluates the cast while planning —
        //
        // ```text
        // one row,  no fence:  ERROR: invalid input syntax for type integer: "oops"
        // one row,  fenced:    NULL, which is the answer the caller reports
        // ```
        //
        // — so a guard that held for every declaration with two bad keys let
        // the single-key case through. The fence goes on both queries, not
        // on the one that was measured failing (DECISIONS 324).
        let source = format!(
            "(SELECT pbps_v.i, pbps_v.s FROM (VALUES {values}) AS pbps_v(i, s) OFFSET 0) AS v(i, s)"
        );
        let collisions = match column {
            Some(_) => None,
            None => {
                // Whether two spellings are one key is the *key column's*
                // question, and a literal in a `VALUES` list carries the
                // database's default collation instead. Under a
                // case-insensitive column `New` and `new` are one row, and
                // grouping without the column's collation reported no
                // collision — two inserts that fail on the primary key.
                //
                // Only a character type can carry one: `COLLATE` on a
                // `numeric` or a `date` is an error rather than a no-op, so
                // the clause is written where the type accepts it and
                // nowhere else.
                let collated = match (&at.key_collation, collatable(base)) {
                    (Some((schema, name)), true) => format!(
                        " COLLATE {}.{}",
                        quote(schema).map_err(RowsError::Dialect)?,
                        quote(name).map_err(RowsError::Dialect)?
                    ),
                    _ => String::new(),
                };
                Some(format!(
                    "SELECT min(v.i) AS first, max(v.i) AS second, min({rendered}) AS canonical\n  \
                     FROM {source}\n \
                     WHERE {readable}\n \
                     GROUP BY CAST(v.s AS {ty}){collated}\n\
                     HAVING count(*) > 1;"
                ))
            }
        };
        Ok(SpellingQuery {
            column,
            ty,
            literals,
            sql: format!(
                "SELECT v.i AS i, CASE WHEN {readable} THEN {rendered} END AS c\n  \
                 FROM {source};"
            ),
            collisions,
        })
    };

    let mut out = Vec::new();
    let (base, ty) = ty_of(&key)?;
    let keys: Vec<(RowKey, String)> = data
        .rows
        .keys()
        .map(|k| (k.clone(), k.as_str().to_owned()))
        .collect();
    if !keys.is_empty() {
        out.push(query(None, &base, ty, keys)?);
    }
    for (column, spec) in &table.columns {
        if *column == key || spec.identity.is_some() {
            continue;
        }
        let (base, ty) = ty_of(column)?;
        if ValueKind::of(&base) != ValueKind::Text {
            continue;
        }
        let literals: Vec<(RowKey, String)> = data
            .rows
            .iter()
            .filter_map(|(k, row)| match row.get(column) {
                Some(Value::Text(t)) => Some((k.clone(), t.clone())),
                _ => None,
            })
            .collect();
        if !literals.is_empty() {
            out.push(query(Some(column.clone()), &base, ty, literals)?);
        }
    }
    Ok(out)
}

/// Whether a `COLLATE` clause is legal on this type. The catalogue's three
/// character types and nothing else; measured, `COLLATE` on a `numeric` is
/// `collations are not supported by type numeric`.
fn collatable(base: &str) -> bool {
    matches!(base, "character" | "character varying" | "text")
}

/// Reads one row of a spelling query: the literal's index and what the
/// engine made of it.
pub fn decode_spelling(
    name: &TableName,
    row: &pbps_db::Row,
) -> Result<(usize, Option<String>), RowsError> {
    let read = |source: DbError| RowsError::Read {
        table: name.clone(),
        source: Box::new(source),
    };
    let i: Option<i32> = row.try_get_at(0).map_err(read)?;
    let c: Option<&str> = row.try_get_at(1).map_err(read)?;
    let Some(i) = i.and_then(|i| usize::try_from(i).ok()) else {
        return Err(read(DbError::BadRow(
            "the spelling query returned a NULL index".to_owned(),
        )));
    };
    Ok((i, c.map(str::to_owned)))
}

/// Reads one row of a collision query: two indexes the engine reads as one
/// key, and the spelling it gives that key.
pub fn decode_collision(
    name: &TableName,
    row: &pbps_db::Row,
) -> Result<(usize, usize, String), RowsError> {
    let read = |source: DbError| RowsError::Read {
        table: name.clone(),
        source: Box::new(source),
    };
    let first: Option<i32> = row.try_get_at(0).map_err(read)?;
    let second: Option<i32> = row.try_get_at(1).map_err(read)?;
    let canonical: Option<&str> = row.try_get_at(2).map_err(read)?;
    match (
        first.and_then(|i| usize::try_from(i).ok()),
        second.and_then(|i| usize::try_from(i).ok()),
    ) {
        (Some(a), Some(b)) => Ok((a, b, canonical.unwrap_or_default().to_owned())),
        _ => Err(read(DbError::BadRow(
            "the collision query returned a NULL index".to_owned(),
        ))),
    }
}

/// What this engine refuses about a `data:` block, each measured on 18.6.
///
/// Model rules — a row's key is its identity in every dialect — live in
/// `pbps_model::data::check`. These are the ones whose answer is this
/// engine's.
pub(crate) fn data_problems(name: &TableName, table: &Table) -> Vec<DialectError> {
    let Some(data) = &table.data else {
        return Vec::new();
    };
    let mut errs = Vec::new();
    let invalid = |message: String| DialectError::Invalid {
        dialect: crate::types::DIALECT,
        message,
    };
    let base_of = |column: &str| {
        table
            .columns
            .get(column)
            .and_then(|c| crate::types::normalize(&c.ty).ok())
            .map(|t| t.base)
    };

    // A pinned identity key leaves the sequence behind (ADR-0013 §2).
    //
    // SQL Server's `SET IDENTITY_INSERT` keeps the seed at least as high as
    // the value written, so ADR-0004's construct is sound there. **Measured**,
    // this engine's `OVERRIDING SYSTEM VALUE` does not:
    //
    // ```text
    // INSERT INTO r.pinned (id, v) OVERRIDING SYSTEM VALUE VALUES (1, …), (2, …);
    // INSERT INTO r.pinned (v) VALUES (…);
    //   ERROR:  duplicate key value violates unique constraint "pinned_pkey"
    //   DETAIL:  Key (id)=(1) already exists.
    // ```
    //
    // The apply succeeds, the plan verifies clean, and the *application's*
    // next insert fails — after the deployment, in someone else's code, with
    // nothing in the plan that mentions it. ADR-0013 §2 takes six measured
    // obstacles to close that by advancing the sequence and closes none of
    // them completely: a `nextval` walks past any lock, a sequence cannot be
    // locked at all, an allocation already handed out cannot be recalled, the
    // advance survives a rollback, and a `CYCLE` sequence makes it
    // non-terminating. So the construct is refused, which removes all six at
    // once (DECISIONS 321).
    if let Some(pk) = &table.primary_key
        && let [key] = pk.columns.as_slice()
        && table.columns.get(key).is_some_and(|c| c.identity.is_some())
    {
        errs.push(invalid(format!(
            "a `data:` block cannot key its rows by `{key}`, an identity column. This engine \
             writes a pinned key with `OVERRIDING SYSTEM VALUE` and leaves the sequence behind — \
             measured, two pinned rows and the next ordinary insert fails on the primary key — \
             so the deployment succeeds and the application breaks afterwards. The sequence is \
             `{}.{}_{key}_seq`, and there is nothing pbps can safely do to it: a `nextval` walks \
             past any lock, a sequence cannot be locked, and an advance survives the rollback of \
             a failed apply (ADR-0013 §2). Declare the rows by a natural key, or place them \
             outside pbps and adopt them with `pbps baseline`.",
            name.schema, name.name
        )));
    }

    for (key, row) in &data.rows {
        for (column, value) in &row.0 {
            let Some(base) = base_of(column) else {
                continue;
            };
            let kind = ValueKind::of(&base);
            // A scalar of the wrong *kind* would disagree with its own
            // database on every plan: a bare `1` in a `text` column is stored
            // and read back as text, and a quoted `"5"` in an `integer` one is
            // read back as the bare integer. The read-back is the arbiter, so
            // the declaration has to be written in the shape the read-back
            // produces (DECISIONS 87, and mirrored here because the shape is
            // the same on both engines even though the type names are not).
            let agrees = matches!(
                (kind, value),
                (_, Value::Null)
                    | (ValueKind::Bool, Value::Bool(_))
                    | (ValueKind::Int, Value::Int(_))
                    | (ValueKind::Text, Value::Text(_))
            );
            if !agrees {
                errs.push(invalid(format!(
                    "row `{key}` sets `{column}` to {value}, {}, but `{base}` reads back as {}: \
                     the declaration would disagree with its own database on every plan — write \
                     it as {}",
                    value.kind(),
                    kind.name(),
                    match kind {
                        ValueKind::Bool => "`true` or `false`",
                        ValueKind::Int => "a bare integer",
                        ValueKind::Text => "a quoted string",
                    }
                )));
                continue;
            }
            // The right kind, but not a value the type holds. The spelling
            // check asks the engine only about text, so an out-of-range
            // integer would otherwise reach the insert (DECISIONS 104).
            if let Value::Int(n) = value
                && let Some(range) = crate::types::identity_range(&table.columns[column].ty)
                && !range.contains(n)
            {
                errs.push(invalid(format!(
                    "row `{key}` sets `{column}` to {n}, but `{base}` holds {} to {}: the engine \
                     would refuse the insert",
                    range.start(),
                    range.end()
                )));
            }
            if let (Value::Text(t), "bytea") = (value, base.as_str())
                && let Some(why) = not_a_bytea(t)
            {
                errs.push(invalid(format!(
                    "row `{key}` sets `{column}`, a `bytea` column, to {t:?}, {why}. This engine \
                     reads a `bytea` back as `\\x` followed by hex digits and pbps writes it back \
                     as `decode('…','hex')` (ADR-0013 §3); a declaration in any other spelling \
                     would be restated by every plan."
                )));
            }
        }
    }
    // The key travels as a literal the engine converts on the way in, and a
    // key a `bytea` column cannot read is not caught by the alias query on a
    // table this plan creates — there is no table to ask yet.
    if let Some(pk) = &table.primary_key
        && let [key_column] = pk.columns.as_slice()
        && base_of(key_column).as_deref() == Some("bytea")
    {
        for key in data.rows.keys() {
            if let Some(why) = not_a_bytea(key.as_str()) {
                errs.push(invalid(format!(
                    "row key `{key}` cannot be a `bytea`, the type of the key column \
                     `{key_column}`: {why}"
                )));
            }
        }
    }
    errs
}

/// Why a declared text is not the spelling this engine reads a `bytea` back
/// in, or `None` where it is.
fn not_a_bytea(text: &str) -> Option<&'static str> {
    let Some(hex) = text.strip_prefix("\\x") else {
        return Some("which does not begin with `\\x`");
    };
    if hex.len() % 2 != 0 {
        return Some("whose hex has an odd number of digits");
    }
    (!hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some("whose tail is not hex digits")
}

/// What `validate` could not answer about a schema's declared rows, offline.
///
/// **Whether two declared keys are one row is the engine's question**
/// (ADR-0013 §5), and it has two halves this command cannot ask. The
/// conversion half: `1` and `01` are one `integer` row, and `New` and `new`
/// are two rows or one depending on the key column's collation — which lives
/// on the live column and which ADR-0013 keeps out of `pbps-model`. **Measured
/// on 18.6**, and the flag that looks like the answer is not one:
///
/// ```text
/// CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
/// INSERT INTO m5.keys VALUES ('New'); INSERT INTO m5.keys VALUES ('new');
///   ERROR:  duplicate key value violates unique constraint "keys_pkey"
/// ```
///
/// while another nondeterministic collation keeps the two apart — so reading
/// `collisdeterministic` as proof of collision would refuse a perfectly good
/// declaration. `plan --db` asks the engine about the actual keys, under the
/// column's own collation; `validate` asks nobody.
///
/// A note rather than silence, because a command that reports clean about a
/// question it never asked is the shape this project's own rule is about:
/// absent, empty and unreadable are three different answers (DECISIONS 327).
pub fn not_checked_offline(schema: &pbps_model::Schema) -> Vec<String> {
    let mut out = Vec::new();
    for (name, table) in &schema.tables {
        let Some(data) = &table.data else { continue };
        // One key cannot collide with itself.
        if data.rows.len() < 2 {
            continue;
        }
        let Some(key) = table
            .primary_key
            .as_ref()
            .filter(|pk| pk.columns.len() == 1)
            .map(|pk| pk.columns[0].clone())
        else {
            continue;
        };
        out.push(format!(
            "`{name}`: whether two of its {} declared row keys are one row is decided by the \
             live `{key}` column — by the type's conversion, and, for a character type, by that \
             column's collation, which is not in the declarations. A case-insensitive or \
             nondeterministic collation makes `New` and `new` one key and the second insert \
             fails on the primary key. `pbps plan --db` asks the engine; this run did not \
             (ADR-0013 §5).",
            data.rows.len()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, ColumnType, DataMode, PrimaryKey, TableData};
    use std::str::FromStr;

    fn table(pk: Option<Vec<&str>>, columns: &[(&str, &str, Option<&str>)]) -> Table {
        let mut t = Table::default();
        for (name, ty, default) in columns {
            let mut c = Column::new(ColumnType::from_str(ty).expect("a type parses"));
            c.default = default.map(str::to_owned);
            t.columns.insert((*name).to_owned(), c);
        }
        t.primary_key = pk.map(|c| PrimaryKey {
            name: None,
            columns: c.into_iter().map(str::to_owned).collect(),
        });
        t
    }

    fn name() -> TableName {
        TableName::new("app", "status")
    }

    fn every() -> RowScope {
        RowScope::Every {
            known: Default::default(),
        }
    }

    fn with_rows(t: &mut Table, rows: &[(&str, &[(&str, Value)])]) {
        let mut data = TableData {
            mode: DataMode::Ensure,
            rows: Default::default(),
        };
        for (key, cells) in rows {
            let mut row = Row::default();
            for (column, value) in *cells {
                row.0.insert((*column).to_owned(), value.clone());
            }
            data.rows.insert(RowKey::from(*key), row);
        }
        t.data = Some(data);
    }

    #[test]
    fn the_query_reads_the_key_first_then_every_other_column() {
        let t = table(
            Some(vec!["code"]),
            &[
                ("code", "varchar(20)", None),
                ("label", "text", Some("'Unlabelled'::text")),
                ("rank", "integer", None),
            ],
        );
        let q = query(&name(), &t, &every())
            .expect("the table is readable")
            .expect("the scope names rows");
        assert_eq!(q.key.value_at, 0);
        assert_eq!(q.key.column, "code");
        assert_eq!(q.columns.len(), 2);
        assert_eq!(q.columns[0].column, "label");
        assert_eq!(q.columns[0].value_at, 1);
        assert_eq!(q.columns[0].default_at, Some(2));
        assert_eq!(q.columns[1].column, "rank");
        assert_eq!(q.columns[1].value_at, 3);
        assert_eq!(q.columns[1].default_at, None);
        assert_eq!(q.columns[1].kind, ValueKind::Int);
        assert!(q.sql.contains("FROM \"app\".\"status\""), "{}", q.sql);
        assert!(!q.sql.contains("WHERE"), "{}", q.sql);
        // Byte for byte, and through the column's type — the same comparison
        // the write path makes (DECISIONS 330), not the column's own `=`.
        assert!(
            q.sql.contains(
                "CASE WHEN (CAST(\"label\" AS text)) COLLATE \"C\" = \
                 (CAST(CAST(('Unlabelled'::text) AS text) AS text)) COLLATE \"C\""
            ),
            "{}",
            q.sql
        );
        assert!(q.sql.trim_end().ends_with(';'), "{}", q.sql);
    }

    /// The engine's column, not the declaration's: never selected, so it never
    /// meets the omission every declaration has to make (DECISIONS 94).
    #[test]
    fn a_non_key_identity_column_is_never_read() {
        let mut t = table(
            Some(vec!["code"]),
            &[("code", "varchar(10)", None), ("label", "text", None)],
        );
        let mut seq = Column::new(ColumnType::from_str("integer").expect("a type")).not_null();
        seq.identity = Some(pbps_model::Identity {
            seed: 1,
            increment: 1,
        });
        t.columns.insert("seq".to_owned(), seq);
        let q = query(&name(), &t, &every())
            .expect("the table is readable")
            .expect("the scope names rows");
        assert_eq!(q.columns.len(), 1, "{:?}", q.columns);
        assert_eq!(q.columns[0].column, "label");
        assert!(!q.sql.contains("\"seq\""), "{}", q.sql);
    }

    /// A table with no single-column key fails the read rather than answering
    /// "no rows": absent, empty and unreadable are three different answers.
    #[test]
    fn a_table_whose_rows_have_no_identity_is_unreadable_and_not_empty() {
        let t = table(
            Some(vec!["a", "b"]),
            &[("a", "integer", None), ("b", "integer", None)],
        );
        let e = query(&name(), &t, &every()).expect_err("a composite key has no row identity");
        assert!(e.to_string().contains("rows are keyed by one"), "{e}");

        let none = table(None, &[("a", "integer", None)]);
        let e = query(&name(), &none, &every()).expect_err("no key at all");
        assert!(e.to_string().contains("no primary key"), "{e}");
    }

    /// The whole difference from the other engine's reader: this one has to
    /// look through the cast the catalog welds on, or every string default is
    /// read as an expression the engine must not be asked about (ADR-0013 §4).
    #[test]
    fn a_literal_is_recognised_through_the_cast_the_catalog_welds_on() {
        for constant in [
            "'unnamed'::text",
            "'x'::character varying",
            "'x'::character varying(9)",
            "0",
            "(0)",
            "-1.5e3",
            "NULL::text",
            "'2026-01-02'::date",
            "('a''b'::text)",
            // The SQL-standard spelling, which only a declaration carries
            // (DECISIONS 350).
            "CAST(NULL AS text)",
            "cast ( NULL as integer )",
            "(CAST('x' AS text))",
            "CAST(1 AS numeric(5,2))",
            "CAST(NULL AS text[])",
            "CAST('it''s' AS text)::text",
            // A comment is whitespace, wherever it stands (DECISIONS 358).
            "NULL /* note */::text",
            "CAST(NULL /* note */ AS text)",
            "/* lead */ NULL",
            "NULL -- note",
            "'x' /* c */::text",
            "('x' -- c\n)::text",
            // Inside a cast, wherever the engine reads a comment as a gap
            // (DECISIONS 359).
            "CAST(NULL AS text /* note */)",
            "CAST(NULL AS /* c */ text)",
            "CAST /* c */ (NULL AS text)",
            "NULL:: /* c */ text",
            "CAST(NULL AS text /* ) */)",
            "CAST( /* ( */ NULL /* AS */ AS text)",
            "CAST(/* nested /* inner */ still */ NULL AS text)",
            "CAST(NULL AS text -- line\n)",
            "NULL::text -- ) AS\n",
            // A type with words after its modifier, and arrays in every
            // spelling (DECISIONS 361).
            "CAST(NULL AS timestamp(3) with time zone)",
            "NULL::timestamp(3) with time zone",
            "CAST(NULL AS TIMESTAMP (3) WITH TIME ZONE)",
            "CAST(NULL AS character varying(10)[])",
            "NULL::numeric(5,2)[]",
            "CAST(NULL AS interval day to second(3))",
            "NULL::text []",
            "CAST(NULL AS text[][])",
            "NULL::text ARRAY[2]",
            "'2026-01-02'::timestamp(3) with time zone",
            // A comment between the words of a type, before its modifier or
            // its array marker (DECISIONS 362).
            "CAST(NULL AS double /* note */ precision)",
            "NULL::double -- c\nprecision",
            "CAST(NULL AS timestamp /* c */ with time zone)",
            "CAST(NULL AS numeric(5, /* c */ 2))",
            "NULL::text /* c */ []",
            "CAST(NULL AS timestamp /* c */ (3) with time zone)",
            // A line comment ends at a carriage return too, and a comment is
            // the gap around `AS` (DECISIONS 363).
            "CAST(NULL -- note\r AS text)",
            "CAST(NULL -- c\rAS text)",
            "NULL::text -- c\r",
            "CAST(NULL/**/AS/**/text)",
            "CAST(NULL/**/AS text)",
            "CAST(NULL AS/**/text)",
            "CAST(NULL AS/**/text/**/)",
            "CAST(NULL--c\nAS--d\ntext)",
            // A string, a `)` or a quoted identifier ends a token by itself,
            // and a quoted type name starts one (DECISIONS 364).
            "CAST('keep'AS text)",
            "CAST(E'x'AS text)",
            "CAST($$x$$AS text)",
            "CAST((NULL)AS text)",
            "CAST(NULL AS\"text\")",
            "CAST(NULL AS pg_catalog.text)",
            "CAST(NULL AS \"pg_catalog\" . \"text\")",
            "CAST(NULL AS pg_catalog/**/./**/text)",
            "NULL::\"text\"",
            "NULL::PG_CATALOG.INT4",
            // A sign with a gap, a comment, a grouping or another sign
            // between it and its operand, and the catalog's own deparsing
            // of a signed default (DECISIONS 365).
            "- 1",
            "+ 1",
            "- /* c */ 1",
            "-\n1",
            "+-1",
            "- - 1",
            "(+ 1)",
            "(+ '-1'::integer)",
            "(- (+ 1))",
            "-(1)",
            "- 0x1F",
            "-'1'::integer",
            "(- NULL::integer)",
            "- /* c */ - /* d */ 1.5e1",
            // A typed literal, in every spelling of its string, with a
            // comment or no gap before it, qualified, quoted, modified, and
            // an interval's field words after it (DECISIONS 367).
            "DATE '2026-02-01'",
            "date'2026-02-01'",
            "DATE /* c */ '2026-02-01'",
            "DATE E'2026-02-01'",
            "DATE U&'2026-02-01'",
            "DATE $$2026-02-01$$",
            "pg_catalog.date '2026-02-01'",
            "\"text\" 'x'",
            "TIMESTAMP WITH TIME ZONE '2026-02-01 00:00+00'",
            "TIMESTAMP(0) '2026-02-01 00:00:00'",
            "NUMERIC(5,2) '1.5'",
            "INTERVAL '1' DAY",
            "INTERVAL '1' HOUR TO MINUTE",
            "INTERVAL '1' SECOND(3)",
            "INTERVAL '1 2:03:04.5678' DAY TO SECOND (2)",
            "interval '1' day",
            "INTERVAL '1' /* c */ DAY /* d */",
            "- INTERVAL '1 day'",
            "(DATE '2026-02-01')",
            "DATE '2026-02-01'::date",
            "text U&'k!0065ep' UESCAPE '!'",
            // Every spelling of a number this engine reads (DECISIONS 360).
            "2_55",
            "0xFF",
            "0o17",
            "0b101",
            "1_000.5",
            "1_000.000_1",
            "1e1_0",
            "0xF_F",
            "-0x1F",
            "1.5e-1_0",
            "0x_F",
            "0XfF",
            // `e` is a hex digit, not an exponent, after a base prefix.
            "0xFFe1",
            // The other spellings of a string, which only a declaration
            // carries (DECISIONS 355).
            "E'old'",
            "e'it\\'s'",
            "U&'old'",
            "U&'old' UESCAPE '!'",
            "N'old'",
            "$$old$$",
            "$q$it's$q$",
            "(E'old')::text",
            "CAST($$old$$ AS text)",
            // An escaped quote and a cast around it; a dollar-quoted string
            // whose body would read as structure (DECISIONS 357).
            "E'it\\'s'::text",
            "CAST(E'it\\'s' AS text)",
            "$$it's$$::text",
            "$$a::b$$::text",
            "CAST($q$a AS b$q$ AS text)",
            "'é'::text",
            "E'a\\\\'::text",
        ] {
            assert!(is_constant(constant), "{constant}");
        }
        // And nothing that would run. `nextval` ends in a cast and is
        // emphatically not a literal: asking about it consumes a sequence
        // value, which is the failure this predicate exists to prevent.
        for expression in [
            "nextval('app.s'::regclass)",
            "now()",
            "gen_random_uuid()",
            "'a'::text || 'b'",
            "CURRENT_TIMESTAMP",
            "",
            "::text",
            "CAST(now() AS text)",
            "CAST(lower('ZZ') AS text)",
            "CAST('a' AS text) || CAST('b' AS text)",
            "CAST('a' AS text || 'b')",
            "CAST(NULL AS text) IS NULL",
            "E'a' || 'b'",
            "$$a$$ || $$b$$",
            "E'unterminated",
            "U&'old' UESCAPE '!!'",
            // A multibyte first character is an expression like any other,
            // not a panic (DECISIONS 357).
            "é()",
            "é'x'",
            "ê",
            // Nothing closes the comment, and the engine refuses that by name.
            "NULL /* unterminated",
            "/* unterminated NULL",
            "CAST(NULL AS text /* unterminated )",
            "CAST(NULL /* AS text) */ AS",
            // Not numbers, each refused by the engine (DECISIONS 360).
            "1_",
            "_1",
            "1__0",
            "1._5",
            "1_.5",
            "0xFF.5",
            "0x",
            "1e_5",
            "0b102",
            // Not typed literals: a grouped string, an array type, an
            // expression after the string, two strings, a type alone.
            "DATE ('2026-02-01')",
            "TEXT[] '{a}'",
            "TEXT 'a' || 'b'",
            "TEXT 'a' 'b'",
            "DATE",
            "DATE /* unterminated '2026-02-01'",
            "DATE 'unterminated",
            "TIMESTAMP '2026-02-01' AT TIME ZONE 'UTC'",
            // Words after the string that are no interval qualifier: an
            // expression, a collation, a misspelt field (DECISIONS 368).
            "BOOLEAN 'false' OR flip()",
            "BOOLEAN 'false' OR random() > 0.5",
            "INTEGER '1' + 1",
            "TEXT 'a' COLLATE ci",
            "INTERVAL '1' DAYS",
            "INTERVAL '1' TO DAY",
            "INTERVAL '1' DAY(3)",
            "INTERVAL '1' DAY TO",
            "INTERVAL '1' SECOND()",
            // A sign with nothing, or an expression, behind it.
            "-",
            "- -",
            "+ ",
            "-x",
            "- 1 + 1",
            "1 -",
            "- (1 + 1)",
            "-'1' || '2'",
            "- /* unterminated 1",
            // Not types: two modifiers, an unclosed one, a non-numeric array
            // bound, and an operator after the cast.
            "NULL::numeric(5)(2)",
            "NULL::numeric(5",
            "NULL::text[a]",
            "NULL::text || 'b'",
            "NULL::double /* unterminated precision",
            "CAST(NULL/**/ASX text)",
            "CAST(NULLAS text)",
            "CAST(1AS text)",
            "CAST(NULL AS(text))",
            "CAST(NULL AS \"unclosed)",
            "CAST(NULL AS text -- ) unterminated",
            "E'a\\'::text",
            "$$open::text",
            "$q$open$$",
        ] {
            assert!(!is_constant(expression), "{expression}");
        }
    }

    /// Which decides what the read-back asks the engine, and what it takes on
    /// the declaration's word. The type does not come into it: every
    /// comparison is made as text, so a `json` default is asked about like any
    /// other literal (DECISIONS 331).
    #[test]
    fn only_a_literal_default_is_asked_about_whatever_its_type() {
        let literal = table(
            Some(vec!["code"]),
            &[
                ("code", "text", None),
                ("label", "text", Some("'x'::text")),
                ("stamp", "timestamptz", Some("now()")),
                ("doc", "json", Some("'{}'::json")),
            ],
        );
        let q = query(&name(), &literal, &every())
            .expect("readable")
            .expect("rows");
        let slot = |column: &str| {
            q.columns
                .iter()
                .find(|s| s.column == column)
                .expect("the column is read")
                .clone()
        };
        assert!(slot("label").default_at.is_some());
        // Asked about nothing the engine would have to run…
        assert!(slot("stamp").default_at.is_none());
        assert!(slot("stamp").assume_default);
        // …and asked about a `json` literal, which has no native `=` at all
        // (measured: `json = json` is `operator does not exist`) and needs
        // none, because both sides are read as text. Taking it on the
        // declaration's word instead put a hand-edited document among the
        // cells nobody can tell from their default, where the read-back drops
        // it and no plan settles the drift.
        assert!(slot("doc").default_at.is_some());
        assert!(!slot("doc").assume_default);
    }

    /// The engine's own spelling of a boolean, and only it. A `t` would mean
    /// the read went through some other rendering, and taking it would let two
    /// readers disagree about what the table holds.
    #[test]
    fn a_boolean_reads_back_as_the_engine_spells_it_and_not_as_a_digit() {
        assert_eq!(value_of(ValueKind::Bool, "true"), Some(Value::Bool(true)));
        assert_eq!(value_of(ValueKind::Bool, "t"), None);
        assert_eq!(value_of(ValueKind::Bool, "1"), None);
        assert_eq!(value_of(ValueKind::Int, "7"), Some(Value::Int(7)));
        assert_eq!(value_of(ValueKind::Int, "7.0"), None);
    }

    /// The requested key goes into the comparison untyped, so the *engine*
    /// decides that `01` names the `integer` row `1` — the same conversion the
    /// emitter's predicate will make.
    #[test]
    fn the_alias_query_asks_the_engine_to_convert_each_requested_key() {
        let t = table(Some(vec!["id"]), &[("id", "integer", None)]);
        let q = query(
            &name(),
            &t,
            &RowScope::Every {
                known: [RowKey::from("01")].into_iter().collect(),
            },
        )
        .expect("readable")
        .expect("rows");
        let aliases = q.aliases.expect("a key was spelled");
        assert!(aliases.contains("E'01' AS requested"), "{aliases}");
        assert!(
            aliases.contains("WHERE pbps_table.\"id\" = E'01'"),
            "{aliases}"
        );
        // Never `CAST(pbps_table."id" AS text) = E'01'`: that would be a text
        // comparison and would call `1` and `01` two different rows.
        assert!(
            !aliases.contains("AS text) = E'01'"),
            "the join must not compare as text: {aliases}"
        );
    }

    /// One query per column that has a spelling to ask about — the key, and
    /// each text-kind column that carries a literal — and none for the kinds
    /// the loader parses and the model spells.
    #[test]
    fn spelling_queries_ask_the_engine_about_every_declared_text_and_every_key() {
        let mut t = table(
            Some(vec!["code"]),
            &[
                ("code", "varchar(10)", None),
                ("pct", "numeric(5,2)", None),
                ("rank", "integer", None),
                ("since", "date", None),
            ],
        );
        with_rows(
            &mut t,
            &[(
                "std",
                &[
                    ("pct", Value::Text("1.5".into())),
                    ("rank", Value::Int(3)),
                    ("since", Value::Text("2026-01-02".into())),
                ],
            )],
        );
        let queries = spelling_queries(&name(), &t, &Catalogued::default()).expect("built");
        let asked: Vec<Option<String>> = queries.iter().map(|q| q.column.clone()).collect();
        assert_eq!(
            asked,
            vec![None, Some("pct".to_owned()), Some("since".to_owned())],
            "{asked:?}"
        );
        // The key's query is the only one that also groups for collisions.
        assert!(queries[0].collisions.is_some());
        assert!(queries[1].collisions.is_none());
        // Every one of them is fenced, and the fence is not decoration: with a
        // single row the planner folds the list and evaluates the cast while
        // planning, which raises on the very text the query exists to report.
        for q in &queries {
            assert!(q.sql.contains("OFFSET 0"), "{}", q.sql);
            assert!(q.sql.contains("pg_input_is_valid"), "{}", q.sql);
        }
    }

    /// Whether two spellings are one key is the *key column's* question, and a
    /// literal carries the database's default collation instead. The clause is
    /// written where the type takes one and nowhere else — `COLLATE` on a
    /// `numeric` is an error, not a no-op.
    #[test]
    fn the_collision_query_carries_the_key_columns_collation_only_where_the_type_takes_one() {
        let collated = |ty: &str| {
            let mut t = table(Some(vec!["code"]), &[("code", ty, None)]);
            with_rows(&mut t, &[("New", &[]), ("new", &[])]);
            let at = Catalogued {
                key_collation: Some(("app".to_owned(), "ci".to_owned())),
                ..Catalogued::default()
            };
            spelling_queries(&name(), &t, &at).expect("built")[0]
                .collisions
                .clone()
                .expect("the key groups for collisions")
        };
        assert!(
            collated("varchar(10)").contains("COLLATE \"app\".\"ci\""),
            "{}",
            collated("varchar(10)")
        );
        assert!(
            !collated("integer").contains("COLLATE"),
            "{}",
            collated("integer")
        );
    }

    /// ADR-0013 §2, and the message has to name both ways forward: the
    /// declaration is not fixable by writing it differently.
    #[test]
    fn an_identity_keyed_data_block_is_refused_and_names_its_sequence() {
        let mut t = table(Some(vec!["id"]), &[("id", "integer", None)]);
        t.columns.get_mut("id").expect("the column").identity = Some(pbps_model::Identity {
            seed: 1,
            increment: 1,
        });
        with_rows(&mut t, &[("1", &[])]);
        let problems = data_problems(&name(), &t);
        assert_eq!(problems.len(), 1, "{problems:?}");
        let message = problems[0].to_string();
        assert!(message.contains("app.status_id_seq"), "{message}");
        assert!(message.contains("OVERRIDING SYSTEM VALUE"), "{message}");
        assert!(message.contains("pbps baseline"), "{message}");

        // And an identity column that is *not* the key is nobody's problem
        // here: the reader never selects it.
        let mut beside = table(
            Some(vec!["code"]),
            &[("code", "text", None), ("seq", "integer", None)],
        );
        beside.columns.get_mut("seq").expect("the column").identity = Some(pbps_model::Identity {
            seed: 1,
            increment: 1,
        });
        with_rows(&mut beside, &[("a", &[])]);
        assert!(data_problems(&name(), &beside).is_empty());
    }

    /// A cell whose *kind* the read-back would not produce, and one the type
    /// cannot hold. Both would be restated by every plan, or refused by the
    /// engine at the insert.
    #[test]
    fn a_cell_the_read_back_would_not_produce_is_refused_by_name() {
        let mut t = table(
            Some(vec!["code"]),
            &[
                ("code", "text", None),
                ("label", "text", None),
                ("rank", "smallint", None),
                ("blob", "bytea", None),
            ],
        );
        with_rows(
            &mut t,
            &[
                ("a", &[("label", Value::Int(1))]),
                ("b", &[("rank", Value::Text("3".into()))]),
                ("c", &[("rank", Value::Int(40_000))]),
                ("d", &[("blob", Value::Text("0102".into()))]),
                ("e", &[("blob", Value::Text("\\x010".into()))]),
                // And the shapes that are right, which must produce nothing.
                (
                    "f",
                    &[
                        ("label", Value::Text("ok".into())),
                        ("rank", Value::Int(3)),
                        ("blob", Value::Text("\\x0102".into())),
                    ],
                ),
                ("g", &[("label", Value::Null)]),
            ],
        );
        let messages: Vec<String> = data_problems(&name(), &t)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(messages.len(), 5, "{messages:#?}");
        assert!(messages[0].contains("row `a` sets `label`"), "{messages:?}");
        assert!(messages[1].contains("row `b` sets `rank`"), "{messages:?}");
        assert!(
            messages[2].contains("holds -32768 to 32767"),
            "{messages:?}"
        );
        assert!(messages[3].contains("does not begin with"), "{messages:?}");
        assert!(messages[4].contains("odd number of digits"), "{messages:?}");
    }

    /// The offline half of ADR-0013 §5: `validate` cannot judge the keys, and
    /// says so rather than reporting clean.
    #[test]
    fn offline_validation_says_it_did_not_judge_the_keys() {
        let mut schema = pbps_model::Schema::default();
        let mut two = table(Some(vec!["code"]), &[("code", "varchar(10)", None)]);
        with_rows(&mut two, &[("New", &[]), ("new", &[])]);
        schema.tables.insert(name(), two);
        // One key cannot collide with itself, and a table with no `data:`
        // block declares no keys at all: neither earns a note, because a note
        // about nothing is noise that teaches a reader to skip them.
        let mut one = table(Some(vec!["code"]), &[("code", "varchar(10)", None)]);
        with_rows(&mut one, &[("only", &[])]);
        schema.tables.insert(TableName::new("app", "one"), one);
        schema.tables.insert(
            TableName::new("app", "plain"),
            table(Some(vec!["code"]), &[("code", "varchar(10)", None)]),
        );

        let notes = not_checked_offline(&schema);
        assert_eq!(notes.len(), 1, "{notes:#?}");
        assert!(notes[0].contains("app.status"), "{}", notes[0]);
        assert!(notes[0].contains("collation"), "{}", notes[0]);
        assert!(notes[0].contains("plan --db"), "{}", notes[0]);
    }
}
