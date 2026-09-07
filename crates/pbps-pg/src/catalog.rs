//! The catalog queries behind [`crate::introspect`].
//!
//! Only this file runs SQL against a live server; it converts `pg_catalog` rows
//! into the plain `Raw*` structs and hands them to the pure assembler. The
//! queries are static — nothing user-controlled is ever interpolated into them.
//!
//! # Every read runs under one search path
//!
//! ADR-0013 §3, and it is not housekeeping. **Measured on 18.6**, the same
//! database renders differently depending on the session's `search_path`:
//!
//! ```text
//! search_path = "$user", public   FOREIGN KEY (pid) REFERENCES p(id)
//!                                 money_amount
//! search_path = ''                FOREIGN KEY (pid) REFERENCES public.p(id)
//!                                 public.money_amount
//! ```
//!
//! `pg_get_constraintdef`, `pg_get_expr` and `format_type` all qualify a name
//! only when it is *not* visible in the path, so a snapshot taken by an
//! operator whose path includes the project's schema and one taken by an
//! operator whose path does not are different text for the same database.
//! Compared against each other that is drift nobody caused. The reads therefore
//! pin the empty path — the one value that does not move with the project's
//! shape — and put back whatever the session had.

use pbps_db::{Conn, DbError, Row};

use crate::introspect::{
    Pulled, RawCatalog, RawColumn, RawConstraint, RawIdentity, RawIndex, RawTable, assemble,
};

/// The schemas that are never a project's.
///
/// `pg_catalog` and `information_schema` are the engine's; `pg_toast` and the
/// `pg_temp_*`/`pg_toast_temp_*` schemas are its bookkeeping. The `__pbps_`
/// filter drops this tool's own state and lock tables, which arrive at Phase 5
/// step 8 — they must never enter the managed set, or the tool would plan
/// changes to itself.
const NOT_A_PROJECTS_SCHEMA: &str =
    "n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_%'";

/// `relkind = 'r'`, and the filter is the whole point: `pg_attribute` holds a
/// row for every index and sequence column too, so a reader without it reports
/// `child_id_seq` and `child_pk` as tables with columns.
///
/// A partitioned table (`p`) is deliberately not here. It is a table the model
/// cannot hold — the partition key has nowhere to go — and pulling it as an
/// ordinary one would produce a plan that recreates it without its partitions.
/// It is reported by [`PARTITIONED`] instead.
fn tables_query() -> String {
    format!(
        "SELECT c.oid::int8 AS oid, n.nspname AS schema_name, c.relname AS table_name
       FROM pg_catalog.pg_class c
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE c.relkind = 'r'
        AND {NOT_A_PROJECTS_SCHEMA}
        AND c.relname NOT LIKE '\\_\\_pbps\\_%'
      ORDER BY n.nspname, c.relname"
    )
}

/// The tables of a kind the model does not hold, so that they are named rather
/// than missing. A partitioned table read as absent is a plan that creates it.
fn partitioned_query() -> String {
    format!(
        "SELECT n.nspname AS schema_name, c.relname AS table_name, c.relkind::text AS kind
       FROM pg_catalog.pg_class c
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE c.relkind IN ('p', 'f')
        AND {NOT_A_PROJECTS_SCHEMA}
        AND c.relname NOT LIKE '\\_\\_pbps\\_%'
      ORDER BY n.nspname, c.relname"
    )
}

/// `NOT attisdropped` is ADR-0012 §6's first trap: the catalog keeps a dropped
/// column's slot, with a placeholder name and a `format_type` of `-`. Without
/// this filter every dropped column is a phantom column whose type will not
/// parse.
///
/// The identity's seed and increment come from the sequence behind the column,
/// found through `pg_depend` rather than `pg_get_serial_sequence` — that
/// function takes a *text* table name and would have to be handed one built by
/// interpolation.
fn columns_query() -> String {
    format!(
        "SELECT a.attrelid::int8 AS table_oid, a.attnum::int4 AS attnum, a.attname AS name,
            pg_catalog.format_type(a.atttypid, a.atttypmod) AS ty,
            NOT a.attnotnull AS nullable,
            pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS default_expr,
            a.attidentity::text AS identity_kind,
            a.attgenerated::text AS generated,
            s.seqstart::int8 AS seq_start,
            s.seqincrement::int8 AS seq_increment
       FROM pg_catalog.pg_attribute a
       JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
       LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
       LEFT JOIN pg_catalog.pg_depend dep
              ON dep.refobjid = a.attrelid AND dep.refobjsubid = a.attnum
             AND dep.classid = 'pg_class'::regclass AND dep.deptype = 'i'
       LEFT JOIN pg_catalog.pg_sequence s ON s.seqrelid = dep.objid
      WHERE c.relkind = 'r'
        AND {NOT_A_PROJECTS_SCHEMA}
        AND a.attnum > 0
        AND NOT a.attisdropped
      ORDER BY a.attrelid, a.attnum"
    )
}

/// Every constraint of every kind, including the ones this reader does not
/// know: the assembler decides what to do with each, and a kind it has never
/// seen is reported rather than dropped in a `WHERE` nobody re-reads.
fn constraints_query() -> String {
    format!(
        "SELECT con.conrelid::int8 AS table_oid, con.conname AS name, con.contype::text AS kind,
            pg_catalog.array_to_string(con.conkey, ',') AS conkey,
            pg_catalog.array_to_string(con.confkey, ',') AS confkey,
            con.confrelid::int8 AS ref_table,
            con.confdeltype::text AS on_delete, con.confupdtype::text AS on_update,
            con.convalidated AS validated,
            pg_catalog.pg_get_constraintdef(con.oid) AS definition,
            con.conindid::int8 AS index_oid
       FROM pg_catalog.pg_constraint con
       JOIN pg_catalog.pg_class c ON c.oid = con.conrelid
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE c.relkind = 'r'
        AND {NOT_A_PROJECTS_SCHEMA}
      ORDER BY con.conrelid, con.conname"
    )
}

/// `indkey` and `indoption` are `int2vector`s, which no driver here reads.
/// Rendered as text they are space-separated, so they arrive as a list this
/// file parses — and `indoption` covers the **key** columns only, never the
/// `INCLUDE` payload, which is why it is shorter than `indkey` on an index that
/// has one.
fn indexes_query() -> String {
    format!(
        "SELECT i.indexrelid::int8 AS oid, i.indrelid::int8 AS table_oid, ic.relname AS name,
            i.indisunique AS is_unique, i.indisprimary AS is_primary,
            i.indisexclusion AS is_exclusion, i.indnkeyatts::int4 AS key_count,
            replace(i.indkey::text, ' ', ',') AS keys,
            replace(i.indoption::text, ' ', ',') AS options,
            pg_catalog.pg_get_expr(i.indpred, i.indrelid) AS filter,
            i.indexprs IS NOT NULL AS has_expressions,
            am.amname AS method
       FROM pg_catalog.pg_index i
       JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid
       JOIN pg_catalog.pg_class c ON c.oid = i.indrelid
       JOIN pg_catalog.pg_am am ON am.oid = ic.relam
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE c.relkind = 'r'
        AND {NOT_A_PROJECTS_SCHEMA}
      ORDER BY i.indrelid, ic.relname"
    )
}

/// Reads the whole managed set back, under the canonical search path.
///
/// The path is put back whatever happens: a caller's connection is not this
/// function's to leave changed, and a read that failed halfway is exactly when
/// a stale setting would be hardest to notice.
pub async fn introspect(conn: &mut Conn) -> Result<Pulled, DbError> {
    let before = current_search_path(conn).await?;
    let read = read_all(conn).await;
    // The restore runs before the `?`, so a failed read still hands the
    // connection back as it was found.
    let restored = set_search_path(conn, &before).await;
    let raw = read?;
    restored?;

    let mut pulled = assemble(&raw.0);
    // Prepended: a table the model cannot hold at all is the thing a reader
    // most needs to see, and it is not about any table in the schema.
    let mut warnings = raw.1;
    warnings.append(&mut pulled.warnings);
    pulled.warnings = warnings;
    Ok(pulled)
}

async fn read_all(conn: &mut Conn) -> Result<(RawCatalog, Vec<String>), DbError> {
    set_search_path(conn, "").await?;

    let mut warnings = Vec::new();
    for row in conn.query(&partitioned_query()).await? {
        let schema = text(&row, "schema_name")?;
        let name = text(&row, "table_name")?;
        let kind = match text(&row, "kind")?.as_str() {
            "p" => "a partitioned table",
            "f" => "a foreign table",
            other => &format!("a relation of kind `{other}`"),
        };
        warnings.push(format!(
            "`{schema}.{name}` is {kind}, which this model does not hold. It is left out of the \
             pull entirely — not read back as an ordinary table, which would make a plan that \
             recreates it without what makes it one."
        ));
    }

    let mut raw = RawCatalog::default();
    for row in conn.query(&tables_query()).await? {
        raw.tables.push(RawTable {
            oid: number(&row, "oid")?,
            schema: text(&row, "schema_name")?,
            name: text(&row, "table_name")?,
        });
    }
    for row in conn.query(&columns_query()).await? {
        let identity = match first_char(&text(&row, "identity_kind")?) {
            Some(kind @ ('a' | 'd')) => Some(RawIdentity {
                always: kind == 'a',
                // A column the catalog calls an identity always has a sequence
                // behind it. Defaulting rather than failing here would invent
                // `GENERATED ... (START WITH 0)`, which is not a thing this
                // engine will accept back (see `types::identity_seed_range`).
                seed: number(&row, "seq_start")?,
                increment: number(&row, "seq_increment")?,
            }),
            _ => None,
        };
        raw.columns.push(RawColumn {
            table_oid: number(&row, "table_oid")?,
            attnum: small(&row, "attnum")?,
            name: text(&row, "name")?,
            ty: text(&row, "ty")?,
            nullable: flag(&row, "nullable")?,
            default: optional_text(&row, "default_expr")?,
            identity,
            generated: first_char(&text(&row, "generated")?).is_some(),
        });
    }
    for row in conn.query(&constraints_query()).await? {
        let ref_table = number(&row, "ref_table")?;
        raw.constraints.push(RawConstraint {
            table_oid: number(&row, "table_oid")?,
            name: text(&row, "name")?,
            kind: first_char(&text(&row, "kind")?).unwrap_or('?'),
            columns: numbers(&optional_text(&row, "conkey")?.unwrap_or_default()),
            ref_columns: numbers(&optional_text(&row, "confkey")?.unwrap_or_default()),
            // 0 is `pg_constraint`'s "no referenced table", not an oid.
            ref_table: (ref_table != 0).then_some(ref_table),
            on_delete: first_char(&text(&row, "on_delete")?).unwrap_or(' '),
            on_update: first_char(&text(&row, "on_update")?).unwrap_or(' '),
            validated: flag(&row, "validated")?,
            definition: text(&row, "definition")?,
            index_oid: {
                let oid = number(&row, "index_oid")?;
                (oid != 0).then_some(oid)
            },
        });
    }
    for row in conn.query(&indexes_query()).await? {
        raw.indexes.push(RawIndex {
            oid: number(&row, "oid")?,
            table_oid: number(&row, "table_oid")?,
            name: text(&row, "name")?,
            unique: flag(&row, "is_unique")?,
            primary: flag(&row, "is_primary")?,
            exclusion: flag(&row, "is_exclusion")?,
            key_count: small(&row, "key_count")?.max(0) as usize,
            columns: numbers(&text(&row, "keys")?),
            options: numbers(&text(&row, "options")?),
            filter: optional_text(&row, "filter")?,
            has_expressions: flag(&row, "has_expressions")?,
            method: text(&row, "method")?,
        });
    }
    Ok((raw, warnings))
}

async fn current_search_path(conn: &mut Conn) -> Result<String, DbError> {
    let rows = conn
        .query("SELECT pg_catalog.current_setting('search_path') AS path")
        .await?;
    match rows.first() {
        Some(row) => text(row, "path"),
        None => Err(missing("search_path")),
    }
}

/// The path is set through `set_config` rather than `SET`, because `SET` takes
/// a bare identifier list and putting the old value back would mean building a
/// statement out of it.
async fn set_search_path(conn: &mut Conn, path: &str) -> Result<(), DbError> {
    let rows = conn
        .query(&format!(
            "SELECT pg_catalog.set_config('search_path', {}, false) AS path",
            quote_literal(path)
        ))
        .await?;
    rows.first()
        .map(|_| ())
        .ok_or_else(|| missing("search_path"))
}

/// A string literal for this engine, doubling the quotes inside it.
///
/// The only value that reaches it is a `search_path` this same function read
/// back a moment ago, but "the caller only ever passes something safe" is not a
/// property a reader can check, and it is one refactoring removes.
fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn missing(column: &str) -> DbError {
    DbError::BadRow(format!(
        "the catalog query returned no `{column}`, which means the query and this code have gone \
         out of step"
    ))
}

fn text(row: &Row, column: &str) -> Result<String, DbError> {
    optional_text(row, column)?.ok_or_else(|| missing(column))
}

fn optional_text(row: &Row, column: &str) -> Result<Option<String>, DbError> {
    Ok(row.try_get::<&str>(column)?.map(str::to_owned))
}

fn number(row: &Row, column: &str) -> Result<i64, DbError> {
    row.try_get::<i64>(column)?.ok_or_else(|| missing(column))
}

fn small(row: &Row, column: &str) -> Result<i32, DbError> {
    row.try_get::<i32>(column)?.ok_or_else(|| missing(column))
}

fn flag(row: &Row, column: &str) -> Result<bool, DbError> {
    row.try_get::<bool>(column)?.ok_or_else(|| missing(column))
}

fn first_char(s: &str) -> Option<char> {
    s.chars().next().filter(|c| !c.is_whitespace())
}

/// A comma-separated list of numbers, as the queries render `smallint[]` and
/// `int2vector`.
///
/// Anything that is not a number is dropped rather than defaulted to zero: a
/// zero in `indkey` **means** an expression column, so inventing one here would
/// turn a parse failure into a claim about the index.
fn numbers(s: &str) -> Vec<i32> {
    s.split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two catalog spellings this file has to read, and the shapes that
    /// are not numbers at all.
    #[test]
    fn a_catalog_vector_reads_as_the_numbers_in_it_and_nothing_else() {
        assert_eq!(numbers("1,2,3"), [1, 2, 3]);
        // `pg_index.indkey` rendered as text, after the query's `replace`.
        assert_eq!(numbers("8,4,7"), [8, 4, 7]);
        // A `0` in `indkey` is an expression column and must survive as one.
        assert_eq!(numbers("0,4"), [0, 4]);
        assert_eq!(numbers(""), [] as [i32; 0]);
        // Not a number: dropped, never defaulted to a `0` that would read as
        // an expression column.
        assert_eq!(numbers("x"), [] as [i32; 0]);
        assert_eq!(numbers("1,x,3"), [1, 3]);
    }

    /// `contype` and friends arrive as one-character text, and a foreign key's
    /// action columns are a **space** on a constraint that has none.
    #[test]
    fn a_single_character_column_is_read_only_when_it_holds_one() {
        assert_eq!(first_char("c"), Some('c'));
        assert_eq!(first_char("a"), Some('a'));
        assert_eq!(first_char(""), None);
        assert_eq!(
            first_char(" "),
            None,
            "the empty `attidentity` and the non-key action"
        );
    }

    /// The path put back is the one that was found, whatever is in it.
    #[test]
    fn a_search_path_goes_back_as_the_literal_it_was() {
        assert_eq!(quote_literal(""), "''");
        assert_eq!(quote_literal("\"$user\", public"), "'\"$user\", public'");
        assert_eq!(quote_literal("it's"), "'it''s'");
    }

    /// Every query names its schema filter from one place, so a schema that is
    /// the engine's cannot be included by one read and excluded by another.
    #[test]
    fn every_query_excludes_the_engines_own_schemas() {
        for (name, sql) in [
            ("TABLES", tables_query()),
            ("PARTITIONED", partitioned_query()),
            ("COLUMNS", columns_query()),
            ("CONSTRAINTS", constraints_query()),
            ("INDEXES", indexes_query()),
        ] {
            assert!(sql.contains(NOT_A_PROJECTS_SCHEMA), "{name}");
        }
    }

    /// The filters ADR-0012 §6 and this file's own documentation turn on. A
    /// query that lost one of these fails silently — with phantom columns, or
    /// with indexes read back as tables — so they are asserted rather than
    /// trusted to review.
    #[test]
    fn the_filters_that_keep_a_phantom_out_are_in_the_queries_that_need_them() {
        assert!(
            columns_query().contains("NOT a.attisdropped"),
            "ADR-0012 §6"
        );
        for (name, sql) in [
            ("TABLES", tables_query()),
            ("COLUMNS", columns_query()),
            ("CONSTRAINTS", constraints_query()),
            ("INDEXES", indexes_query()),
        ] {
            assert!(sql.contains("relkind = 'r'"), "{name}");
        }
        for (name, sql) in [
            ("TABLES", tables_query()),
            ("PARTITIONED", partitioned_query()),
        ] {
            assert!(sql.contains("\\_\\_pbps\\_%"), "{name}: {sql}");
        }
    }
}
