//! Reading declared rows back from a table — the connected half of ADR-0004.
//!
//! # The engine spells the values
//!
//! Every cell comes back as text that the **server** rendered (`CONVERT` with a
//! fixed style), never as a driver-typed value this crate then formats. That is
//! §8.2's rule — the database is the normalizer — applied to rows: whatever
//! spelling the engine gives a `decimal(5,2)` or a `datetime2` is the spelling
//! the state records, both sides of a drift check see the same one, and a
//! declaration that wants to match it writes it that way (`pull --data` shows
//! it). The alternative, parsing values into typed Rust and formatting them
//! back, would put a second formatter between the engine and the comparison
//! and let the two disagree about `1.5` and `1.50`.
//!
//! Only three kinds survive the trip as anything but text: `bit` becomes a
//! boolean and the four integer types become integers, because those are the
//! two shapes a declaration can write unquoted and they have to compare equal
//! to what was written.
//!
//! # A cell that holds the default is read back both ways
//!
//! A declared row says "the default" by leaving the column out ([`Row`]). The
//! catalog holds a value, not the fact that it came from the default — so the
//! read-back asks the engine, per cell, whether the value *equals* the column's
//! default, and reports that beside the value ([`ObservedRow::at_default`]).
//! Which reading a side uses is that side's choice, made where its own rows
//! are known ([`ObservedRow::as_seen_by`]): a declaration that omits `label`
//! compares equal to a table where every label is `'Unlabelled'`, and one that
//! writes `label: Unlabelled` compares equal to the same table — neither is
//! restated on every connected plan.
//!
//! **Only a literal default is compared.** `'Unlabelled'`, `0`, `NULL` are
//! asked about with a `CASE`; `SYSUTCDATETIME()`, `NEWID()` or `NEXT VALUE
//! FOR` are not, because that `CASE` would *run* the expression once per row
//! — a sequence advanced by a drift check, and `NEXT VALUE FOR` is not even
//! legal there, which made the table unreadable. Such a cell cannot be told
//! from its default ([`ObservedRow::unknown`]), so it is taken at the
//! declaration's word: at its default where the side omits it, the stored
//! value where the side spells it out — and, where there is no declaration
//! at all (`pull`), the stored value, because a `NEWID()` key or a
//! `GETDATE()` stamp is a value the block has to carry, not a default it
//! can be rebuilt from. A hand edit to an omitted cell of that kind is
//! therefore not seen; the remedy, for a column that matters, is to write
//! the value.
//!
//! A NULL in a column with no default is omitted outright, because the model
//! already treats the two spellings as one there (see
//! [`pbps_model::data::cell`]); writing it explicitly would make the same
//! table compare unequal to itself across two reads.
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
use pbps_model::{ColumnType, ObservedRow, Row, RowKey, RowScope, Table, TableName, Value};

use crate::emit::qualified;
use crate::ident::{literal, quote};

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
    pub fn of(base: &str) -> ValueKind {
        match base {
            "bit" => ValueKind::Bool,
            "tinyint" | "smallint" | "int" | "bigint" => ValueKind::Int,
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
    /// ([`pbps_model::ObservedTable::aliases`]). The engine decides that
    /// `N'01'` names the `int` row `1`, exactly as it will when the emitter's
    /// `WHERE [id] = N'01'` runs.
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
/// all — there is nothing to ask, and `IN ()` is not T-SQL.
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
    let requested: Vec<String> = scope.known().iter().map(|k| literal(k.as_str())).collect();

    let mut select = vec![read_expr(&quote(&key)?, &key_spec.ty.base)];
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
        // An IDENTITY column that is not the key is the engine's to assign: a
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
        select.push(read_expr(&quoted, &spec.ty.base));
        let asked = confirms_default(spec);
        let default_at = match &spec.default {
            Some(default) if asked => {
                select.push(format!(
                    // Both halves, because `=` is UNKNOWN for a NULL on either
                    // side and `DEFAULT NULL` is a real declaration.
                    "CASE WHEN {quoted} = ({default}) OR ({quoted} IS NULL AND ({default}) IS NULL) \
                     THEN 1 ELSE 0 END"
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
        // The same rendering the emitter's `WHERE key = N'...'` uses: always a
        // string literal, converted by the engine to the key column's type.
        // That conversion is wrong for a binary key — `N'0x01'` becomes the
        // characters, not the byte — which is why `validate` refuses a
        // binary column in a `data:` block (DECISIONS 70) rather than this
        // read and the emitter each guessing a type they do not carry.
        let list: Vec<String> = keys.iter().map(|k| literal(k.as_str())).collect();
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
        // Both sides aliased, so neither exposed name can collide with the
        // table's own: `FROM (VALUES ...) AS k JOIN [dbo].[k]` is refused by
        // the engine, and a table called `k` is not far-fetched.
        Some(format!(
            "SELECT pbps_requested.requested AS requested, {} AS canonical\n  \
             FROM (VALUES {}) AS pbps_requested(requested)\n  \
             JOIN {table} AS pbps_table ON pbps_table.{key} = pbps_requested.requested;",
            read_expr(&format!("pbps_table.{key}"), &key_spec.ty.base),
            requested
                .iter()
                .map(|r| format!("({r})"))
                .collect::<Vec<_>>()
                .join(", "),
        ))
    };

    Ok(Some(RowQuery {
        sql,
        key: key_slot,
        columns,
        aliases,
    }))
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
    /// `int`, `a` and `A` under a case-insensitive collation — would insert
    /// twice and fail on the second, and the alias query (71) cannot see
    /// them on a table that does not hold either yet (DECISIONS 106).
    pub collisions: Option<String>,
}

/// The engine's own answer to "is this text the spelling it reads back":
/// every declared text cell and every key, converted to its column's type
/// and rendered exactly as [`read_expr`] renders a stored value.
///
/// A declaration written `"1.5"` for a `decimal(5,2)` is stored as `1.50`
/// and read back as `1.50`, and every connected plan then restates an
/// update that changes nothing. Neither the model nor this crate can spell
/// a value the engine's way without becoming the engine (71 refused to
/// invent a normalizer for keys for the same reason), so the engine is
/// asked, before anything is written, and a declaration that disagrees is
/// refused with the spelling to write. `TRY_CONVERT` answers NULL for a
/// text the type cannot read at all — the other thing a table this plan
/// creates cannot be asked about through its alias query, since there is no
/// table yet. Keys are only checked for that: their spelling is aliased at
/// read time (71). Integer and bit cells are parsed by the loader and
/// spelled by the model; only text-kind columns carry a spelling to ask
/// What the catalog calls a table and its key column *now*, where the plan
/// about to be checked renames them.
///
/// The spelling checks run before a statement of the plan has run, so the
/// database still has the old names — and the one query here that names an
/// object rather than converting a literal, the key column's collation, found
/// nothing under the declared name and fell back to the database default
/// without saying so. Absent entries mean "as declared", which is right for
/// every table a plan does not rename and for one it has yet to create
/// (DECISIONS 148).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalogued {
    pub table: Option<TableName>,
    pub key_column: Option<String>,
}

/// Every declared table's catalog names, keyed by the declared table name.
pub type CatalogNames = std::collections::BTreeMap<TableName, Catalogued>;

/// about.
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
    // The catalog is asked under the names it has now, not the ones this plan
    // is about to give it: the collation read below happens before the rename
    // statement runs (DECISIONS 148).
    let qualified_name =
        crate::emit::qualified(at.table.as_ref().unwrap_or(name)).map_err(|e| {
            RowsError::Unreadable {
                table: name.clone(),
                why: e.to_string(),
            }
        })?;
    let key_name = at.key_column.clone().unwrap_or_else(|| key.clone());
    let query = |column: Option<String>,
                 base: &str,
                 ty: String,
                 literals: Vec<(RowKey, String)>|
     -> SpellingQuery {
        let rendered = read_expr(&format!("TRY_CONVERT({ty}, v.s)"), base);
        let values = literals
            .iter()
            .enumerate()
            .map(|(i, (_, text))| format!("({i}, {})", literal(text)))
            .collect::<Vec<_>>()
            .join(", ");
        let collisions = column.is_none().then(|| {
            let grouped = format!(
                "SELECT MIN(v.i) AS first, MAX(v.i) AS second, MIN({rendered}) AS canonical\n  \
                 FROM (VALUES {values}) AS v(i, s)\n \
                 WHERE TRY_CONVERT({ty}, v.s) IS NOT NULL\n \
                 GROUP BY TRY_CONVERT({ty}, v.s)"
            );
            let tail = "\nHAVING COUNT(*) > 1;";
            if ValueKind::of(base) != ValueKind::Text {
                // Nothing outside text has a collation, and `COLLATE` on a
                // number is an error rather than a no-op.
                return format!("{grouped}{tail}");
            }
            // Whether two spellings are one key is the *key column's*
            // question, and a `VALUES` literal carries the database's
            // default collation instead. On a case-sensitive database with a
            // case-insensitive key column, `a` and `A` are one row and this
            // reported no collision — two inserts that fail on the primary
            // key; the other way round, two distinct keys were refused as
            // one. The column's own collation is asked for here.
            //
            // A collation is a name, not a value, so it cannot be bound and
            // the statement has to be built around it. Only names of
            // letters, digits and `_` are concatenated — every real
            // collation name is one — and a table that does not exist yet
            // has none, which is right: the emitter writes no `COLLATE`, so
            // its column will be created with the database's default.
            format!(
                "DECLARE @coll sysname = (SELECT c.collation_name FROM sys.columns c\n  \
                   WHERE c.object_id = OBJECT_ID({}) AND c.name = {}\n    \
                     AND c.collation_name NOT LIKE N'%[^A-Za-z0-9_]%');\n\
                 DECLARE @sql nvarchar(max) = {}\n  \
                   + COALESCE(N' COLLATE ' + @coll, N'') + {};\n\
                 EXEC sp_executesql @sql;",
                literal(&qualified_name),
                literal(&key_name),
                literal(&grouped),
                literal(tail),
            )
        });
        SpellingQuery {
            column,
            ty,
            literals,
            sql: format!("SELECT v.i AS i, {rendered} AS c\n  FROM (VALUES {values}) AS v(i, s);"),
            collisions,
        }
    };

    let mut out = Vec::new();
    let (base, ty) = ty_of(&key)?;
    let keys: Vec<(RowKey, String)> = data
        .rows
        .keys()
        .map(|k| (k.clone(), k.as_str().to_owned()))
        .collect();
    if !keys.is_empty() {
        out.push(query(None, &base, ty, keys));
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
            out.push(query(Some(column.clone()), &base, ty, literals));
        }
    }
    Ok(out)
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

/// Reads one row of the alias query: the requested spelling and the engine's.
pub fn decode_alias(name: &TableName, row: &pbps_db::Row) -> Result<(RowKey, RowKey), RowsError> {
    let read = |source: DbError| RowsError::Read {
        table: name.clone(),
        source: Box::new(source),
    };
    let requested: Option<&str> = row.try_get_at(0).map_err(read)?;
    let canonical: Option<&str> = row.try_get_at(1).map_err(read)?;
    match (requested, canonical) {
        (Some(r), Some(c)) => Ok((RowKey::from(r), RowKey::from(c))),
        // The join cannot produce a NULL on either side; one here means the
        // query and this reader disagree.
        _ => Err(read(DbError::BadRow(
            "the alias query returned a NULL key".to_owned(),
        ))),
    }
}

/// The expression that renders one column as the text the state will hold.
///
/// Styles are fixed so the spelling never depends on a session setting:
/// `126` is ISO 8601 for every date and time type, `1` keeps the `0x` prefix
/// on binary, `3` is the round-trippable 17-digit form for floats, and `2`
/// is the four-decimal form for money. A
/// fixed-width `char` is trimmed because the engine itself ignores the padding
/// when it compares — `'ab' = 'ab   '` — and a declaration should not have to
/// count spaces to agree with it.
pub(crate) fn read_expr(quoted: &str, base: &str) -> String {
    match base {
        "date" | "time" | "datetime" | "datetime2" | "datetimeoffset" | "smalldatetime" => {
            format!("CONVERT(nvarchar(max), {quoted}, 126)")
        }
        "float" | "real" => format!("CONVERT(nvarchar(max), {quoted}, 3)"),
        // `money` and `smallmoney` hold four decimal places and the default
        // style renders two: `1.0001` came back `1.00`, so a pull wrote a
        // declaration for a value the table does not hold and the next
        // `verify` compared the two truncations and called them equal.
        // Measured on a live server; style 2 renders all four and round-trips
        // (DECISIONS 115).
        "money" | "smallmoney" => format!("CONVERT(nvarchar(max), {quoted}, 2)"),
        "binary" | "varbinary" | "timestamp" => format!("CONVERT(nvarchar(max), {quoted}, 1)"),
        "image" => format!("CONVERT(nvarchar(max), CONVERT(varbinary(max), {quoted}), 1)"),
        // CLR types have no conversion to a string type; they render themselves.
        "geometry" | "geography" | "hierarchyid" => format!("{quoted}.ToString()"),
        "char" | "nchar" => format!("RTRIM({quoted})"),
        _ => format!("CONVERT(nvarchar(max), {quoted})"),
    }
}

/// The expression that turns text [`read_expr`] produced back into a value of
/// the type it was read from — `read_expr`'s inverse, and only ever used as
/// its inverse.
///
/// It exists so a cell can be held to what the plan recorded across a column
/// this same plan retypes. The recorded text is the *old* type's spelling and
/// the column now holds the value the `ALTER` converted, so neither spelling
/// compares with the other: what does compare is the recorded text put back
/// through the old type and then converted the way the engine converted the
/// column (DECISIONS 149). The tool never computes that conversion — it asks
/// the engine for it.
///
/// Each style is the one `read_expr` wrote with, because parsing has to undo
/// exactly what rendering did: `126` for the date and time types, `1` for the
/// `0x` form of binary. The rest need none — `CONVERT` reads the 17-digit
/// float form and the four-decimal money form without being told, and a
/// `char` re-pads to its own width, which is what the column holds anyway.
///
/// `TRY_CONVERT`, not `CONVERT`: a recorded text the target type cannot hold
/// means the cell is not what the plan recorded, and the caller's own
/// "the row is not as the plan recorded it" is a better answer than the
/// engine's conversion error (Msg 245).
pub(crate) fn from_text(literal: &str, ty: &ColumnType) -> String {
    match ty.base.as_str() {
        "date" | "time" | "datetime" | "datetime2" | "datetimeoffset" | "smalldatetime" => {
            format!("TRY_CONVERT({ty}, {literal}, 126)")
        }
        "binary" | "varbinary" | "timestamp" => format!("TRY_CONVERT({ty}, {literal}, 1)"),
        _ => format!("TRY_CONVERT({ty}, {literal})"),
    }
}

/// Whether the engine is asked to confirm a cell of this column at its
/// default: the default is a literal it can compare without running anything,
/// and the type has `=`. One function because two callers ask it — the row
/// reader, to decide what to put in the query, and the apply guard, to know
/// whether an omitted cell in the read-back *means* at-default (DECISIONS
/// 191) — and two spellings of it would drift.
pub fn confirms_default(spec: &pbps_model::Column) -> bool {
    spec.default
        .as_deref()
        .is_some_and(|d| comparable(&spec.ty.base) && is_constant(d))
}

/// Whether a default expression is a literal — a number, a string, `NULL`, a
/// hex constant, under any number of parentheses (the catalog wraps them) —
/// which the engine can compare a stored value against without running
/// anything. Everything else is a function call or an expression, and the
/// module docs say why those are never put in the query. Conservative on
/// purpose: a literal read as "not a literal" only costs the comparison.
pub fn is_constant(default: &str) -> bool {
    let Some(clean) = constant_trivia(default) else {
        return false;
    };
    constant_operand(&clean, 0)
}

// Preserve token boundaries: removing a comment must not turn `1/*c*/2`
// into a number, or `-/*c*/-1` into a line comment.
fn constant_trivia(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' || c == '[' {
            let close = if c == '[' { ']' } else { '\'' };
            out.push(c);
            loop {
                let next = chars.next()?;
                out.push(next);
                if next == close {
                    if chars.peek() != Some(&close) {
                        break;
                    }
                    out.push(chars.next()?);
                }
            }
        } else if c == '-' && chars.peek() == Some(&'-') {
            chars.next();
            for next in chars.by_ref() {
                if matches!(next, '\r' | '\n') {
                    break;
                }
            }
            out.push(' ');
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut depth = 1;
            while depth != 0 {
                let next = chars.next()?;
                if next == '/' && chars.peek() == Some(&'*') {
                    chars.next();
                    depth += 1;
                } else if next == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    depth -= 1;
                }
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn numeric_cast(s: &str) -> Option<(&str, ColumnType)> {
    let lower = s.to_ascii_lowercase();
    let cast = lower.strip_prefix("cast").and_then(|rest| {
        let start = s.len() - rest.len();
        let body = s[start..].trim().strip_prefix('(')?.strip_suffix(')')?;
        // AS is a token: tabs/newlines separate it just as spaces do.
        // Comments have already become whitespace, and the target's type
        // parser still refuses a suffix that is not a numeric type.
        let (at, _) = body
            .to_ascii_lowercase()
            .rmatch_indices("as")
            .find(|(at, _)| {
                body[..*at].ends_with(char::is_whitespace)
                    && body[*at + 2..].starts_with(char::is_whitespace)
            })?;
        Some((&body[..at], body[at + 2..].trim()))
    });
    let convert = lower.strip_prefix("convert").and_then(|rest| {
        let start = s.len() - rest.len();
        let body = s[start..].trim().strip_prefix('(')?.strip_suffix(')')?;
        let mut parens = 0usize;
        let separator = body.char_indices().find_map(|(at, c)| {
            match c {
                '(' => parens += 1,
                ')' => parens = parens.saturating_sub(1),
                ',' if parens == 0 => return Some(at),
                _ => {}
            }
            None
        })?;
        Some((&body[separator + 1..], body[..separator].trim()))
    });
    let (operand, ty) = cast.or(convert)?;
    let ty = ty.replace(['[', ']'], "").parse::<ColumnType>().ok()?;
    Some((operand, crate::types::normalize(&ty).ok()?))
}

// This is a bounded safety proof, not a replacement SQL evaluator: its value
// never enters a query or the model. The original SQL still supplies the value.
// Exact decimal syntax has at most 38 digits; numeric strings use that syntax
// (scientific notation also reaches float/real). Approximate-to-exact casts,
// currency-formatted strings, arbitrary expressions and user types stay unknown.
enum Numeric {
    Exact(ExactNumeric),
    Approximate(f64),
    Text(String),
    Null,
}

struct ExactNumeric {
    coefficient: i128,
    scale: u32,
    bounds: (i128, i128),
    tiny: bool,
    money: bool,
}

fn decimal_number(text: &str) -> Option<ExactNumeric> {
    let text = text.trim_matches(' ');
    let negative = text.starts_with('-');
    let text = text
        .strip_prefix(['-', '+'])
        .unwrap_or(text)
        .trim_start_matches(' ');
    let (integer, fraction) = text.split_once('.').unwrap_or((text, ""));
    if integer.is_empty() && fraction.is_empty()
        || !integer
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
        || fraction.len() > 38
    {
        return None;
    }
    let digits = format!("{integer}{fraction}");
    let digits = digits.trim_start_matches('0');
    if digits.len() > 38 {
        return None;
    }
    let coefficient = if digits.is_empty() {
        0
    } else {
        digits.parse::<i128>().ok()?
    };
    let limit = 10i128.pow(38) - 1;
    Some(ExactNumeric {
        coefficient: if negative { -coefficient } else { coefficient },
        scale: fraction.len() as u32,
        bounds: (-limit, limit),
        tiny: false,
        money: false,
    })
}

fn rescale(coefficient: i128, from: u32, to: u32, round: bool) -> Option<i128> {
    if to >= from {
        coefficient.checked_mul(10i128.checked_pow(to - from)?)
    } else {
        let divisor = 10i128.checked_pow(from - to)?;
        let truncated = coefficient / divisor;
        if round && (coefficient % divisor).abs() >= divisor / 2 {
            truncated.checked_add(coefficient.signum())
        } else {
            Some(truncated)
        }
    }
}

fn numeric_conversion(value: Numeric, ty: &ColumnType) -> Option<Numeric> {
    let base = ty.base.as_str();
    if !matches!(
        base,
        "tinyint"
            | "smallint"
            | "int"
            | "bigint"
            | "decimal"
            | "money"
            | "smallmoney"
            | "real"
            | "float"
    ) {
        return None;
    }
    if matches!(value, Numeric::Null) {
        return Some(Numeric::Null);
    }
    if matches!(base, "real" | "float") {
        let value = match value {
            Numeric::Exact(n) => n.coefficient as f64 / 10f64.powi(n.scale as i32),
            Numeric::Approximate(n) => n,
            // Approximate conversions reject the sign gap that exact
            // numeric string conversions accept; retain it for this parser.
            Numeric::Text(s) => s.trim_matches(' ').parse::<f64>().ok()?,
            Numeric::Null => unreachable!(),
        };
        let value = if base == "real" {
            f64::from(value as f32)
        } else {
            value
        };
        return value.is_finite().then_some(Numeric::Approximate(value));
    }
    let integer_target = matches!(base, "tinyint" | "smallint" | "int" | "bigint");
    let value = match value {
        Numeric::Exact(n) => n,
        Numeric::Text(s) => {
            // Unlike a numeric operand, the string '1.25' does not convert
            // to int at all. Fractional money operands instead round to int.
            if integer_target && s.contains('.') {
                return None;
            }
            decimal_number(&s)?
        }
        Numeric::Approximate(_) | Numeric::Null => return None,
    };
    let (scale, bounds) = match base {
        "tinyint" => (0, (0, 255)),
        "smallint" => (0, (i128::from(i16::MIN), i128::from(i16::MAX))),
        "int" => (0, (i128::from(i32::MIN), i128::from(i32::MAX))),
        "bigint" => (0, (i128::from(i64::MIN), i128::from(i64::MAX))),
        "money" => (4, (i128::from(i64::MIN), i128::from(i64::MAX))),
        "smallmoney" => (4, (i128::from(i32::MIN), i128::from(i32::MAX))),
        "decimal" => {
            let [
                pbps_model::TypeArg::Int(precision),
                pbps_model::TypeArg::Int(scale),
            ] = ty.args.as_slice()
            else {
                return None;
            };
            let limit = 10i128.checked_pow((*precision).try_into().ok()?)? - 1;
            ((*scale).try_into().ok()?, (-limit, limit))
        }
        _ => return None,
    };
    let coefficient = rescale(
        value.coefficient,
        value.scale,
        scale,
        !integer_target || value.money,
    )?;
    if !(bounds.0..=bounds.1).contains(&coefficient) {
        return None;
    }
    Some(Numeric::Exact(ExactNumeric {
        coefficient,
        scale,
        bounds,
        tiny: base == "tinyint",
        money: matches!(base, "money" | "smallmoney"),
    }))
}

fn numeric_value(text: &str, depth: usize) -> Option<Numeric> {
    if depth > 64 {
        return None;
    }
    let mut s = text.trim();
    while s.len() >= 2 && s.starts_with('(') && s.ends_with(')') {
        s = s[1..s.len() - 1].trim();
    }
    if let Some(operand) = s.strip_prefix(['-', '+']) {
        let mut value = numeric_value(operand, depth + 1)?;
        if matches!(value, Numeric::Text(_)) {
            return None;
        }
        if s.starts_with('-') {
            match &mut value {
                Numeric::Exact(n) => {
                    // Measured: unary minus promotes tinyint to smallint,
                    // but the other bounded types retain their own bounds.
                    if n.tiny {
                        n.bounds = (i128::from(i16::MIN), i128::from(i16::MAX));
                        n.tiny = false;
                    }
                    n.coefficient = n.coefficient.checked_neg()?;
                    if !(n.bounds.0..=n.bounds.1).contains(&n.coefficient) {
                        return None;
                    }
                }
                Numeric::Approximate(n) => *n = -*n,
                Numeric::Text(_) | Numeric::Null => {}
            }
        }
        return Some(value);
    }
    if let Some((operand, ty)) = numeric_cast(s) {
        return numeric_conversion(numeric_value(operand, depth + 1)?, &ty);
    }
    if s.eq_ignore_ascii_case("null") {
        return Some(Numeric::Null);
    }
    let quoted = s
        .strip_prefix(['N', 'n'])
        .filter(|rest| rest.starts_with('\''))
        .unwrap_or(s);
    if let Some(value) = quoted
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return (!value.contains('\'')).then(|| Numeric::Text(value.to_owned()));
    }
    if s.contains(['e', 'E']) {
        let value = s.parse::<f64>().ok()?;
        return value.is_finite().then_some(Numeric::Approximate(value));
    }
    decimal_number(s).map(Numeric::Exact)
}

fn constant_operand(default: &str, depth: usize) -> bool {
    // Declarations are untrusted input; nested signs/casts must not exhaust
    // the reader's stack. An unusually deep constant can remain unconfirmed.
    if depth > 64 {
        return false;
    }
    let mut s = default.trim();
    while s.len() >= 2 && s.starts_with('(') && s.ends_with(')') {
        s = s[1..s.len() - 1].trim();
    }
    if s.is_empty() {
        return false;
    }
    // Unary plus is erased from a CAST's catalog deparse, so the proven
    // numeric CONVERT spelling must be recognized without a remaining sign.
    if s.starts_with(['-', '+']) || numeric_cast(s).is_some() {
        return numeric_value(s, depth).is_some();
    }
    if s.eq_ignore_ascii_case("null") {
        return true;
    }
    let quoted = s
        .strip_prefix(['N', 'n'])
        .filter(|rest| rest.starts_with('\''))
        .unwrap_or(s);
    if let Some(inner) = quoted
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        // `'It''s'` is one literal; `'a' + 'b'` is not.
        return !inner.replace("''", "").contains('\'');
    }
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return hex.bytes().all(|b| b.is_ascii_hexdigit());
    }
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let number = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mantissa, exponent) = match number.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (number, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some((int, frac)) => {
            (int.is_empty() || digits(int))
                && (frac.is_empty() || digits(frac))
                && !(int.is_empty() && frac.is_empty())
        }
        None => digits(mantissa),
    };
    mantissa_ok && exponent.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e)))
}

/// Whether `=` is defined on the type. Where it is not, the default is not
/// asked about and the cell is taken as at its default (module docs).
pub(crate) fn comparable(base: &str) -> bool {
    !matches!(
        base,
        "xml" | "geometry" | "geography" | "text" | "ntext" | "image"
    )
}

/// One cell's text as a model value.
pub fn value_of(kind: ValueKind, text: &str) -> Option<Value> {
    Some(match kind {
        ValueKind::Bool => match text {
            "1" => Value::Bool(true),
            "0" => Value::Bool(false),
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
        // would have had to run). The third is not the first: a `NEWID()`
        // key or a `GETDATE()` stamp holds a value nobody can tell from its
        // default, and `pull` has to write that value, not drop it.
        let confirmed = match slot.default_at {
            Some(at) => {
                row.try_get_at::<i32>(at)
                    .map_err(|e| read(name, e))?
                    .unwrap_or(0)
                    == 1
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, ColumnType, PrimaryKey};
    use std::str::FromStr;

    fn table(pk: Option<Vec<&str>>, columns: &[(&str, &str, Option<&str>)]) -> Table {
        let mut t = Table::default();
        for (name, ty, default) in columns {
            let mut c = Column::new(ColumnType::from_str(ty).unwrap());
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
        TableName::new("dbo", "status")
    }

    #[test]
    fn the_query_reads_the_key_first_then_every_other_column() {
        let t = table(
            Some(vec!["code"]),
            &[
                ("code", "varchar(20)", None),
                ("label", "nvarchar(50)", Some("'Unlabelled'")),
                ("rank", "int", None),
            ],
        );
        let q = query(
            &name(),
            &t,
            &RowScope::Every {
                known: Default::default(),
            },
        )
        .unwrap()
        .unwrap();
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
        assert!(q.sql.contains("FROM [dbo].[status]"), "{}", q.sql);
        assert!(!q.sql.contains("WHERE"), "{}", q.sql);
        assert!(
            q.sql.contains(
                "CASE WHEN [label] = ('Unlabelled') OR ([label] IS NULL AND ('Unlabelled') IS NULL)"
            ),
            "{}",
            q.sql
        );
        assert!(q.sql.trim_end().ends_with(';'), "{}", q.sql);
    }

    /// The engine's column, not the declaration's: never selected, so it never
    /// meets the omission every declaration has to make.
    #[test]
    fn a_non_key_identity_column_is_never_read() {
        let mut t = table(
            Some(vec!["code"]),
            &[
                ("code", "varchar(10)", None),
                ("label", "nvarchar(50)", None),
            ],
        );
        let mut seq = Column::new(ColumnType::from_str("int").unwrap()).not_null();
        seq.identity = Some(pbps_model::Identity {
            seed: 1,
            increment: 1,
        });
        t.columns.insert("seq".to_owned(), seq);
        let q = query(
            &name(),
            &t,
            &RowScope::Every {
                known: Default::default(),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(q.columns.len(), 1, "{:?}", q.columns);
        assert_eq!(q.columns[0].column, "label");
        assert!(!q.sql.contains("[seq]"), "{}", q.sql);
    }

    /// One query per column that has a spelling to ask about — the key for
    /// convertibility, each text-kind column for its literals — rendered
    /// the way the read-back renders a stored value; integer and bit
    /// columns, and columns nobody spells, get none.
    #[test]
    fn spelling_queries_ask_the_engine_about_every_declared_text_and_every_key() {
        use pbps_model::{DataMode, Row, TableData};
        let mut t = table(
            Some(vec!["code"]),
            &[
                ("code", "varchar(10)", None),
                ("pct", "decimal(5,2)", None),
                ("rank", "int", None),
                ("since", "date", None),
            ],
        );
        let mut std = Row::default();
        std.0.insert("pct".into(), Value::Text("1.5".into()));
        std.0.insert("rank".into(), Value::Int(1));
        let mut old = Row::default();
        old.0.insert("since".into(), Value::Text("2026-9-3".into()));
        t.data = Some(TableData {
            mode: DataMode::Exact,
            rows: [(RowKey::from("std"), std), (RowKey::from("old"), old)]
                .into_iter()
                .collect(),
        });
        let qs = spelling_queries(&name(), &t, &Catalogued::default()).unwrap();
        let columns: Vec<Option<&str>> = qs.iter().map(|q| q.column.as_deref()).collect();
        assert_eq!(columns, [None, Some("pct"), Some("since")], "{qs:#?}");
        assert_eq!(qs[0].ty, "varchar(10)");
        assert_eq!(qs[0].literals.len(), 2);
        assert_eq!(qs[1].ty, "decimal(5, 2)");
        assert_eq!(qs[1].literals, [(RowKey::from("std"), "1.5".to_owned())]);
        assert!(
            qs[1]
                .sql
                .contains("CONVERT(nvarchar(max), TRY_CONVERT(decimal(5, 2), v.s)) AS c"),
            "{}",
            qs[1].sql
        );
        assert!(
            qs[1].sql.contains("(VALUES (0, N'1.5')) AS v(i, s)"),
            "{}",
            qs[1].sql
        );
        // Only the key asks about collisions, grouped by what the engine reads.
        let dupes = qs[0].collisions.as_deref().unwrap();
        assert!(
            dupes.contains("GROUP BY TRY_CONVERT(varchar(10), v.s)")
                && dupes.contains("HAVING COUNT(*) > 1"),
            "{dupes}"
        );
        assert!(qs[1].collisions.is_none());
        // A date is rendered in the fixed style the read-back uses.
        assert!(
            qs[2].sql.contains("TRY_CONVERT(date, v.s), 126)"),
            "{}",
            qs[2].sql
        );
        // No block, nothing to ask.
        t.data = None;
        assert!(
            spelling_queries(&name(), &t, &Catalogued::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_ensure_scope_asks_only_for_its_keys_as_string_literals() {
        let t = table(Some(vec!["id"]), &[("id", "int", None), ("n", "int", None)]);
        let keys = ["7", "o'k"].into_iter().map(RowKey::from).collect();
        let q = query(&name(), &t, &RowScope::Keys(keys)).unwrap().unwrap();
        assert!(q.sql.contains("WHERE [id] IN (N'7', N'o''k')"), "{}", q.sql);
        // And asks the engine which row each spelling names, in the engine's
        // own spelling of the key — the same comparison the DML will make.
        let aliases = q.aliases.unwrap();
        assert!(
            aliases.contains("FROM (VALUES (N'7'), (N'o''k')) AS pbps_requested(requested)"),
            "{aliases}"
        );
        assert!(
            aliases.contains(
                "JOIN [dbo].[status] AS pbps_table ON pbps_table.[id] = pbps_requested.requested"
            ),
            "{aliases}"
        );
        assert!(
            aliases.contains("CONVERT(nvarchar(max), pbps_table.[id]) AS canonical"),
            "{aliases}"
        );
        // A read that spells nothing (`pull`) asks nothing.
        let q = query(
            &name(),
            &t,
            &RowScope::Every {
                known: Default::default(),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(q.aliases, None);
    }

    /// `IN ()` is not T-SQL, and nothing is being asked for anyway.
    #[test]
    fn an_empty_key_scope_reads_nothing() {
        let t = table(Some(vec!["id"]), &[("id", "int", None)]);
        assert!(
            query(&name(), &t, &RowScope::Keys(Default::default()))
                .unwrap()
                .is_none()
        );
    }

    /// Unreadable is an error, never an empty answer.
    #[test]
    fn a_table_whose_rows_have_no_identity_is_refused_by_name() {
        let none = table(None, &[("id", "int", None)]);
        let e = query(
            &name(),
            &none,
            &RowScope::Every {
                known: Default::default(),
            },
        )
        .unwrap_err();
        assert!(e.to_string().contains("no primary key"), "{e}");
        let two = table(
            Some(vec!["a", "b"]),
            &[("a", "int", None), ("b", "int", None)],
        );
        let e = query(
            &name(),
            &two,
            &RowScope::Every {
                known: Default::default(),
            },
        )
        .unwrap_err();
        assert!(e.to_string().contains("2 columns"), "{e}");
    }

    #[test]
    fn types_without_equality_get_no_default_flag() {
        let t = table(
            Some(vec!["id"]),
            &[
                ("id", "int", None),
                ("doc", "xml", Some("''")),
                ("blob", "image", Some("0x")),
            ],
        );
        let q = query(
            &name(),
            &t,
            &RowScope::Every {
                known: Default::default(),
            },
        )
        .unwrap()
        .unwrap();
        assert!(q.columns.iter().all(|s| s.default_at.is_none()), "{q:?}");
        assert!(q.columns.iter().all(|s| s.has_default), "{q:?}");
        // Not asked about is taken at the declaration's word.
        assert!(q.columns.iter().all(|s| s.assume_default), "{q:?}");
    }

    /// A default the engine would have to *run* is never put in the query: a
    /// `CASE` over `NEXT VALUE FOR` is refused by the engine, and one over
    /// `NEWID()` runs it once per row. Only a literal is compared.
    #[test]
    fn signed_numeric_casts_are_confirmed_only_when_evaluation_cannot_throw() {
        for (default, safe) in [
            ("-CAST('abc' AS int)", false),
            ("-CAST('256' AS tinyint)", false),
            ("-CAST('255' AS tinyint)", true),
            ("-CAST('-32768' AS smallint)", false),
            ("+CAST('-32768' AS smallint)", true),
            ("-CAST('-9223372036854775808' AS bigint)", false),
            ("+CAST('-9223372036854775808' AS bigint)", true),
            ("-CAST(1.25 AS int)", true),
            ("-CAST('1.25' AS int)", false),
            ("-CAST(CAST(1.5 AS money) AS int)", true),
            ("-CAST('9.995' AS decimal(3,2))", false),
            ("-CAST('9.994' AS decimal(3,2))", true),
            ("-CAST('922337203685477.5807' AS money)", true),
            ("-CAST('922337203685477.5808' AS money)", false),
            ("-CAST('-922337203685477.5808' AS money)", false),
            ("+CAST('-922337203685477.5808' AS money)", true),
            ("-CAST('214748.3647' AS smallmoney)", true),
            ("-CAST('214748.3648' AS smallmoney)", false),
            ("-CAST('214748.36475' AS smallmoney)", false),
            ("-CAST('-214748.3648' AS smallmoney)", false),
            ("+CAST('-214748.3648' AS smallmoney)", true),
            ("-CAST('-0.00005' AS money)", true),
            ("-CAST('+ 1' AS float)", false),
            ("-CAST('+-1' AS float)", false),
            ("-CAST('--1' AS real)", false),
            ("-CAST('1e38' AS real)", true),
            ("-CAST('1e39' AS real)", false),
            ("-CAST('1e308' AS float)", true),
            ("-CAST('1e309' AS float)", false),
            ("-CAST('1e-400' AS float)", true),
            ("-CAST('1e-100' AS real)", true),
            ("-CAST('$1' AS money)", false),
            // Approximate-to-exact conversion and strings beyond the exact
            // engine's 38-digit grammar remain unknown, never SQL NULL.
            ("-CAST(CAST('1.25' AS float) AS int)", false),
            (
                "-CAST('123456789012345678901234567890123456789' AS decimal(38,0))",
                false,
            ),
        ] {
            assert_eq!(is_constant(default), safe, "{default}");
        }
    }

    #[test]
    fn a_default_the_engine_would_have_to_run_is_never_evaluated() {
        for literal in [
            "0",
            "((0))",
            "(-1)",
            "- 1",
            "+ 1",
            "-(-1)",
            "- /* c */ 1",
            "- /* a /* b */ c */ ( + 1)",
            "- -- c\n1",
            "-CAST('1' AS int)",
            "-CAST('1'\tAS\tint)",
            "-CAST('1'\nAS\nint)",
            "( -CONVERT([int],'1'))",
            "-CAST('1.25' AS decimal(10,2))",
            "( -CONVERT([numeric](10,2),'1.25'))",
            "1.5",
            ".5",
            "1e3",
            "-2.5E-3",
            "'x'",
            "N'x'",
            "'It''s'",
            "NULL",
            "(null)",
            "0x",
            "0xDEADbeef",
            "N''",
        ] {
            assert!(is_constant(literal), "{literal}");
        }
        for expression in [
            "getdate()",
            "(SYSUTCDATETIME())",
            "NEXT VALUE FOR dbo.seq",
            "(newid())",
            "'a' + 'b'",
            "(1)+(2)",
            "abs(-1)",
            "",
            "()",
            "0x1G",
            "1.2.3",
            "--1",
            "- /* unfinished",
            "- 1 + 2",
            "- abs(1)",
            "- CAST(NEWID() AS int)",
            "- CAST(NEWID()\tAS\tint)",
            "- CAST(1\nASint)",
            "- CONVERT(int, NEXT VALUE FOR dbo.seq)",
            "- CAST('1' AS dbo.custom)",
            "- CAST('01/02/2026' AS datetime)",
            "- CONVERT(decimal(10,2), RAND())",
            "- CONVERT(decimal(10,2), '1', 0)",
            "1/* c */2",
            "'unterminated",
        ] {
            assert!(!is_constant(expression), "{expression}");
        }

        let t = table(
            Some(vec!["id"]),
            &[
                ("id", "int", None),
                ("label", "nvarchar(50)", Some("'Unlabelled'")),
                ("seq", "int", Some("NEXT VALUE FOR dbo.seq")),
                ("stamp", "datetime2", Some("sysutcdatetime()")),
            ],
        );
        let q = query(
            &name(),
            &t,
            &RowScope::Every {
                known: Default::default(),
            },
        )
        .unwrap()
        .unwrap();
        assert!(!q.sql.contains("NEXT VALUE"), "{}", q.sql);
        assert!(
            !q.sql.to_ascii_lowercase().contains("sysutcdatetime"),
            "{}",
            q.sql
        );
        let by_name = |c: &str| q.columns.iter().find(|s| s.column == c).unwrap().clone();
        assert_eq!(by_name("label").default_at, Some(2));
        assert!(!by_name("label").assume_default);
        assert_eq!(by_name("seq").default_at, None);
        assert!(by_name("seq").assume_default);
        assert!(by_name("stamp").assume_default);
    }

    #[test]
    fn the_engine_spells_dates_binary_and_floats_with_fixed_styles() {
        assert_eq!(
            read_expr("[d]", "datetime2"),
            "CONVERT(nvarchar(max), [d], 126)"
        );
        assert_eq!(
            read_expr("[b]", "varbinary"),
            "CONVERT(nvarchar(max), [b], 1)"
        );
        assert_eq!(read_expr("[f]", "float"), "CONVERT(nvarchar(max), [f], 3)");
        assert_eq!(read_expr("[c]", "char"), "RTRIM([c])");
        assert_eq!(read_expr("[g]", "geography"), "[g].ToString()");
        assert_eq!(read_expr("[n]", "decimal"), "CONVERT(nvarchar(max), [n])");
        // The default style truncates money to two decimals; the type holds
        // four.
        assert_eq!(read_expr("[m]", "money"), "CONVERT(nvarchar(max), [m], 2)");
        assert_eq!(
            read_expr("[m]", "smallmoney"),
            "CONVERT(nvarchar(max), [m], 2)"
        );
    }

    #[test]
    fn bits_and_integers_come_back_typed_and_everything_else_as_text() {
        assert_eq!(value_of(ValueKind::Bool, "1"), Some(Value::Bool(true)));
        assert_eq!(value_of(ValueKind::Bool, "0"), Some(Value::Bool(false)));
        assert_eq!(value_of(ValueKind::Bool, "true"), None);
        assert_eq!(value_of(ValueKind::Int, "-7"), Some(Value::Int(-7)));
        assert_eq!(value_of(ValueKind::Int, "1.5"), None);
        assert_eq!(
            value_of(ValueKind::Text, "1.50"),
            Some(Value::Text("1.50".to_owned()))
        );
        assert_eq!(ValueKind::of("bigint"), ValueKind::Int);
        assert_eq!(ValueKind::of("bit"), ValueKind::Bool);
        assert_eq!(ValueKind::of("decimal"), ValueKind::Text);
    }

    /// The canonical form is what lets two reads of one table be `==`: a NULL
    /// is omitted only where the model already reads omission as NULL, and
    /// everything else is explicit — whether or not it equals the default,
    /// which is reported beside it rather than folded in.
    #[test]
    fn a_null_is_omitted_only_where_the_model_reads_omission_as_null() {
        let with_default = Slot {
            column: "label".into(),
            kind: ValueKind::Text,
            value_at: 1,
            default_at: Some(2),
            has_default: true,
            assume_default: false,
        };
        assert_eq!(
            canonical(&with_default, Some("Unlabelled")),
            Ok(Some(Value::Text("Unlabelled".into())))
        );
        assert_eq!(
            canonical(&with_default, Some("New")),
            Ok(Some(Value::Text("New".into())))
        );
        // A NULL where a default exists is *not* the default: it was sent, and
        // an omitted cell would say the opposite.
        assert_eq!(canonical(&with_default, None), Ok(Some(Value::Null)));

        let without = Slot {
            has_default: false,
            default_at: None,
            ..with_default
        };
        assert_eq!(canonical(&without, None), Ok(None));
        // And a value the mapping cannot hold names the kind it expected.
        let int = Slot {
            kind: ValueKind::Int,
            ..without
        };
        assert_eq!(canonical(&int, Some("x")), Err("integer"));
    }
}
