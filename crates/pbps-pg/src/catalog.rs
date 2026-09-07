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

use pbps_model::TableName;

use crate::introspect::{
    Limitation, Pulled, RawCatalog, RawColumn, RawConstraint, RawIdentity, RawIndex, RawTable,
    assemble,
};

/// The schemas that are never a project's.
///
/// `pg_catalog` and `information_schema` are the engine's; `pg_toast` and the
/// `pg_temp_*`/`pg_toast_temp_*` schemas are its bookkeeping.
///
/// `left(nspname, 3)` rather than `LIKE 'pg\_%'`, and **not** because it reads
/// better. A backslash in an ordinary string literal is only a backslash while
/// `standard_conforming_strings` is on; with it off the engine eats it —
/// measured, with a warning nothing here reads — the pattern becomes `pg_%`,
/// the `_` turns into a wildcard, and a project'"'"'s schema called `pga` vanishes
/// from the pull. The canonical scope pins that setting (DECISIONS 254), and
/// this predicate does not depend on it having worked: a filter with no escape
/// in it cannot be read two ways.
const NOT_A_PROJECTS_SCHEMA: &str = "n.nspname NOT IN ('pg_catalog', 'information_schema')
      AND pg_catalog.left(n.nspname, 3) <> 'pg_'";

/// This tool's own tables, which arrive at Phase 5 step 8. They must never
/// enter the managed set, or the tool would plan changes to itself.
///
/// **By name, not by prefix.** `NOT LIKE '\_\_pbps\_%'` also hides a
/// project's own `app.__pbps_customers`, and nothing refuses that declaration —
/// so the pull reported the table absent and the next plan tried to create one
/// that was already there. The two names SPEC §8.1 defines are the two tables
/// this tool owns; a step that adds a third adds it here, where a reader can
/// see what the list is for. The SQL Server pull lists the same two names.
const NOT_ONE_OF_OURS: &str = "c.relname NOT IN ('__pbps_state', '__pbps_lock')";

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
        AND {NOT_ONE_OF_OURS}
        AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i
                         WHERE i.inhrelid = c.oid OR i.inhparent = c.oid)
        AND NOT c.relrowsecurity
        AND NOT c.relforcerowsecurity
        AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_policy p WHERE p.polrelid = c.oid)
        AND c.relpersistence = 'p'
        AND c.relreplident = 'd'
        AND NOT c.relhasrules
        AND c.reloftype = 0
        AND c.relam = (SELECT am.oid FROM pg_catalog.pg_am am WHERE am.amname = 'heap')
      ORDER BY n.nspname, c.relname"
    )
}

/// The tables of a kind the model does not hold, so that they are named rather
/// than missing. A table read as absent is a plan that creates it.
///
/// A partitioned table's `relkind` says so; an inheritance child's does not —
/// **measured**, a child of `INHERITS` is an ordinary `r` and only `pg_inherits`
/// tells it apart. Its inherited columns are `attislocal = false`, so a pull
/// that took it for an ordinary table would declare somebody else's columns as
/// its own and plan a table that has them locally instead of by inheritance.
///
/// **The parent is no more an ordinary table than the child is**, and it took a
/// review round to see it: measured, a `SELECT` from the parent returns the
/// children's rows as well as its own, and `ALTER TABLE parent ADD COLUMN`
/// gives the column to every child. A managed parent would therefore compare
/// clean while a plan against it silently changed tables nobody declared. Both
/// ends of `pg_inherits` are excluded, and each is named for its own reason.
fn partitioned_query() -> String {
    format!(
        "SELECT n.nspname AS schema_name, c.relname AS table_name, c.relkind::text AS kind,
            c.relrowsecurity AS row_security, c.relpersistence::text AS persistence,
            c.relforcerowsecurity AS force_row_security,
            (SELECT pg_catalog.count(*) FROM pg_catalog.pg_policy p
              WHERE p.polrelid = c.oid)::int8 AS policies,
            c.relreplident::text AS replica_identity,
            c.relhasrules AS has_rules, am.amname AS access_method,
            c.reloftype::regtype::text AS of_type,
            EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i WHERE i.inhparent = c.oid)
              AS inherited_from
       FROM pg_catalog.pg_class c
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
       LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam
      WHERE {NOT_A_PROJECTS_SCHEMA}
        AND {NOT_ONE_OF_OURS}
        AND (c.relkind IN ('p', 'f')
             OR (c.relkind = 'r'
                 AND (EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i
                               WHERE i.inhrelid = c.oid OR i.inhparent = c.oid)
                      OR c.relrowsecurity
                      OR c.relforcerowsecurity
                      OR EXISTS (SELECT 1 FROM pg_catalog.pg_policy p
                                  WHERE p.polrelid = c.oid)
                      OR c.relpersistence <> 'p'
                      OR c.relreplident <> 'd'
                      OR c.relhasrules
                      OR c.reloftype <> 0
                      OR c.relam <> (SELECT am.oid FROM pg_catalog.pg_am am
                                      WHERE am.amname = 'heap'))))
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
///
/// **Two joins, not one widened**, because they are two different things wearing
/// one shape. An identity's sequence is `deptype = 'i'`, internal: it is part of
/// the column. A `serial`'s is `deptype = 'a'`, auto: a separate object the
/// column merely defaults from, and one this model has nowhere to put. Reading
/// only `'i'` returned a `serial` column as an ordinary integer whose default
/// happens to say `nextval(...)`, with no word about the sequence that default
/// needs.
///
/// `IN ('i', 'a')` is what that first fix reached for, and it is wrong for a
/// reason measured here: `ALTER SEQUENCE s OWNED BY t.c` on a column that is
/// **already** an identity is legal, and then the column has both rows. One
/// join returned it twice, the identity's seed and increment were taken from
/// whichever row the engine handed back first, and the assembler's map kept the
/// last. Two joins give one row per column and take each fact from the
/// dependency that means it.
fn columns_query() -> String {
    format!(
        "SELECT a.attrelid::int8 AS table_oid, a.attnum::int4 AS attnum, a.attname AS name,
            pg_catalog.format_type(a.atttypid, a.atttypmod) AS ty,
            NOT a.attnotnull AS nullable,
            pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS default_expr,
            a.attidentity::text AS identity_kind,
            a.attgenerated::text AS generated,
            s.seqstart::int8 AS seq_start,
            s.seqincrement::int8 AS seq_increment,
            seq.relname AS sequence_name,
            s.seqmin::int8 AS seq_min, s.seqmax::int8 AS seq_max, s.seqcycle AS seq_cycle,
            s.seqcache::int8 AS seq_cache,
            (SELECT pg_catalog.string_agg(
                      DISTINCT pg_catalog.format('`%I.%I`', sn.nspname, sq.relname), ', ')
               FROM pg_catalog.pg_depend dd
               JOIN pg_catalog.pg_class sq ON sq.oid = dd.refobjid AND sq.relkind = 'S'
               JOIN pg_catalog.pg_namespace sn ON sn.oid = sq.relnamespace
              WHERE dd.classid = 'pg_attrdef'::regclass
                AND dd.objid = d.oid
                AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend od
                                 WHERE od.classid = 'pg_class'::regclass
                                   AND od.objid = sq.oid
                                   AND od.refobjid = a.attrelid
                                   AND od.refobjsubid = a.attnum
                                   AND od.deptype IN ('i', 'a'))) AS default_sequences,
            CASE WHEN a.attcollation <> ty.typcollation
                 THEN (SELECT co.collname FROM pg_catalog.pg_collation co
                        WHERE co.oid = a.attcollation)
            END AS collation
       FROM pg_catalog.pg_attribute a
       JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
       JOIN pg_catalog.pg_type ty ON ty.oid = a.atttypid
       LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
       LEFT JOIN (pg_catalog.pg_depend own
                  JOIN pg_catalog.pg_class seq
                    ON seq.oid = own.objid AND seq.relkind = 'S')
              ON own.refobjid = a.attrelid AND own.refobjsubid = a.attnum
             AND own.classid = 'pg_class'::regclass AND own.deptype = 'a'
       LEFT JOIN (pg_catalog.pg_depend idep
                  JOIN pg_catalog.pg_sequence s ON s.seqrelid = idep.objid
                  JOIN pg_catalog.pg_class iseq
                    ON iseq.oid = idep.objid AND iseq.relkind = 'S')
              ON idep.refobjid = a.attrelid AND idep.refobjsubid = a.attnum
             AND idep.classid = 'pg_class'::regclass AND idep.deptype = 'i'
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
            con.condeferrable AS deferrable, con.condeferred AS deferred,
            pg_catalog.pg_get_constraintdef(con.oid) AS definition,
            pg_catalog.pg_get_expr(con.conbin, con.conrelid) AS expression,
            con.confmatchtype::text AS match_type,
            pg_catalog.array_to_string(con.confdelsetcols, ',') AS delete_set_columns,
            con.conindid::int8 AS index_oid,
            con.conenforced AS enforced, con.conperiod AS period,
            con.connoinherit AS no_inherit,
            EXISTS (SELECT 1 FROM pg_catalog.pg_trigger tg
                     WHERE tg.tgconstraint = con.oid AND tg.tgenabled <> 'O')
              AS triggers_not_ordinary
       FROM pg_catalog.pg_constraint con
       JOIN pg_catalog.pg_class c ON c.oid = con.conrelid
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE c.relkind = 'r'
        AND {NOT_A_PROJECTS_SCHEMA}
      ORDER BY con.conrelid, con.conname"
    )
}

/// The default operator class is resolved the way the engine resolves it, not
/// by matching `opcintype` to the column's type. **Measured**: an index on a
/// `character varying` column has no exact match at all — there is no default
/// btree opclass whose `opcintype` is `varchar` — so the exact-match subquery
/// returned NULL, `indclass <> NULL` was unknown, and `varchar_pattern_ops`
/// read back as an ordinary index. `GetDefaultOpClass` falls back to a
/// *preferred* type in the same category that the column's type is binary
/// coercible to, which is how `varchar` reaches `text_ops`; the `count(*) = 1`
/// guard is its "ambiguous operator class" case.
///
/// A domain is unwrapped to its base type first, for the same reason the engine
/// does it. And the comparison is `IS DISTINCT FROM`, so a default this query
/// cannot resolve reports the index rather than passing it as ordinary — not
/// being able to tell is not the same as there being nothing to tell.
///
/// `indkey` and `indoption` are `int2vector`s, which no driver here reads.
/// Rendered as text they are space-separated, so they arrive as a list this
/// file parses — and `indoption` covers the **key** columns only, never the
/// `INCLUDE` payload, which is why it is shorter than `indkey` on an index that
/// has one.
fn indexes_query() -> String {
    format!(
        "SELECT i.indexrelid::int8 AS oid, i.indrelid::int8 AS table_oid, ic.relname AS name,
            i.indisunique AS is_unique, i.indisprimary AS is_primary,
            i.indisexclusion AS is_exclusion, i.indisvalid AS is_valid,
            i.indnullsnotdistinct AS nulls_not_distinct,
            i.indnkeyatts::int4 AS key_count,
            replace(i.indkey::text, ' ', ',') AS keys,
            replace(i.indoption::text, ' ', ',') AS options,
            pg_catalog.pg_get_expr(i.indpred, i.indrelid) AS filter,
            i.indexprs IS NOT NULL AS has_expressions,
            am.amname AS method,
            EXISTS (
              SELECT 1
                FROM pg_catalog.generate_series(1, i.indnkeyatts) AS k(n)
                CROSS JOIN LATERAL (
                  SELECT COALESCE(
                    (SELECT t.typbasetype FROM pg_catalog.pg_type t
                      WHERE t.oid = a.atttypid AND t.typtype = 'd' AND t.typbasetype <> 0),
                    a.atttypid) AS coltype,
                    a.attcollation AS colcollation
                    FROM pg_catalog.pg_attribute a
                   WHERE a.attrelid = i.indrelid AND a.attnum = i.indkey[k.n - 1]) AS col
               WHERE i.indclass[k.n - 1] IS DISTINCT FROM COALESCE(
                       (SELECT oc.oid FROM pg_catalog.pg_opclass oc
                         WHERE oc.opcmethod = ic.relam AND oc.opcdefault
                           AND oc.opcintype = col.coltype),
                       (SELECT CASE WHEN count(*) = 1 THEN min(oc.oid) END
                          FROM pg_catalog.pg_opclass oc
                          JOIN pg_catalog.pg_type tt ON tt.oid = oc.opcintype
                          JOIN pg_catalog.pg_type st ON st.oid = col.coltype
                         WHERE oc.opcmethod = ic.relam AND oc.opcdefault
                           AND tt.typispreferred AND tt.typcategory = st.typcategory
                           AND EXISTS (SELECT 1 FROM pg_catalog.pg_cast ct
                                        WHERE ct.castsource = col.coltype
                                          AND ct.casttarget = oc.opcintype
                                          AND ct.castmethod = 'b'
                                          AND ct.castcontext = 'i')))
                  OR (i.indcollation[k.n - 1] <> 0
                      AND i.indcollation[k.n - 1] <> col.colcollation)
            ) AS nondefault_column_options
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

const BEGIN: &str = "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY";

/// The two halves of the probe that says whether a transaction is already open.
///
/// A `SET LOCAL` lives exactly as long as the transaction it runs in. Outside a
/// transaction block that is the statement itself, so a second statement reads
/// the setting back empty; inside the caller's transaction the value is still
/// there. **Measured through this driver**, which is the point: every other way
/// of asking reads the same in both states here, because the extended query
/// protocol starts the implicit transaction before the statement's own clock —
/// `xact_start = query_start` and `transaction_timestamp() =
/// statement_timestamp()` are both false even outside a transaction.
///
/// A custom GUC under this tool's own prefix, so that a caller whose transaction
/// this refuses is left holding nothing it did not already have.
const PROBE_SET: &str = "SELECT pg_catalog.set_config('pbps.in_a_transaction', 'yes', true)";
const PROBE_READ: &str =
    "SELECT COALESCE(current_setting('pbps.in_a_transaction', true), '') AS probe";

/// `true` is `is_local`: the setting belongs to this transaction and goes back
/// when it ends, whichever way it ends.
///
/// `search_path` is the one that decides how a *name* is rendered. The rest
/// decide how a **value** is, and they matter for the same reason: the
/// expressions this pull carries are carried verbatim (ADR-0013 §4), so a
/// setting that changes what the deparser prints manufactures drift between two
/// pulls of the same unchanged database, and a rebuild of every constraint and
/// index it touches.
///
/// **Measured**, each of these changes a rendered expression on 18.6:
/// `quote_all_identifiers` turns `id > 0` into `"id" > 0`; `DateStyle` turns
/// `'2020-01-02'::date` into `'02.01.2020'::date`; `TimeZone` moves a
/// `timestamptz` default to another wall clock; `IntervalStyle` turns
/// `'1 day 02:00:00'` into `'1 2:00:00'`; `bytea_output` turns `'\x0102'` into
/// `'\\001\\002'`. `extra_float_digits` is here for the same class of reason
/// without a case that showed it — it governs how many digits a float prints.
///
/// `lc_monetary` is deliberately absent: it belongs to the same class, and
/// `SET` fails outright on a locale the server does not have, which would turn
/// a readable database into an unreadable one.
const CANONICAL_PATH: &str = "SELECT pg_catalog.set_config('search_path', '', true),
       pg_catalog.set_config('quote_all_identifiers', 'off', true),
       pg_catalog.set_config('datestyle', 'ISO, MDY', true),
       pg_catalog.set_config('intervalstyle', 'postgres', true),
       pg_catalog.set_config('timezone', 'UTC', true),
       pg_catalog.set_config('bytea_output', 'hex', true),
       pg_catalog.set_config('extra_float_digits', '1', true),
       pg_catalog.set_config('standard_conforming_strings', 'on', true)";

/// Reads the whole managed set back: one snapshot, one search path, no writes.
///
/// **All of it inside one transaction**, and each word of `BEGIN ISOLATION
/// LEVEL REPEATABLE READ READ ONLY` is load-bearing.
///
/// *Repeatable read*, because five autocommit statements are five snapshots. A
/// table dropped after the tables query and before the columns query comes back
/// as a live table with no columns at all — which assembles cleanly, compares
/// as a table whose every column was deleted, and plans accordingly. Under one
/// snapshot the five reads cannot disagree about what exists.
///
/// *Read only*, because introspection has no business writing and the engine
/// can say so rather than this code promising it. Measured, `CREATE TABLE` in
/// such a transaction is `cannot execute CREATE TABLE in a read-only
/// transaction`.
///
/// And the canonical search path is set **`is_local`**, so the transaction
/// carries it and ending the transaction puts back whatever the session had.
/// Measured, that holds on `COMMIT` and on `ROLLBACK` alike — which is what
/// makes "a read that failed halfway leaves the session changed" not a case to
/// handle but a case that cannot arise.
pub async fn introspect(conn: &mut Conn) -> Result<Pulled, DbError> {
    refuse_a_caller_owned_transaction(conn).await?;
    conn.execute(BEGIN).await?;
    let raw = match read_all(conn).await {
        Ok(raw) => {
            conn.execute("COMMIT").await?;
            raw
        }
        // `ROLLBACK` rather than `COMMIT`, and its own error is dropped: a
        // transaction the server has already killed must not replace the
        // failure that killed it.
        Err(e) => {
            let _ = conn.execute("ROLLBACK").await;
            return Err(schema_changed_underneath(e));
        }
    };

    let mut pulled = assemble(&raw.0);
    // Prepended: a table the model cannot hold at all is the thing a reader
    // most needs to see, and it is not about any table in the schema.
    //
    // Recorded as a `Limitation` as well as a warning, for the same reason
    // every other one is: a caller that asks "what could this pull not say
    // about this table?" must not be told "nothing" about the tables it could
    // not say anything about at all. The two lists stay one-to-one.
    let mut warnings: Vec<String> = raw.1.iter().map(|l| l.detail.clone()).collect();
    warnings.append(&mut pulled.warnings);
    pulled.warnings = warnings;
    let mut limitations = raw.1;
    limitations.append(&mut pulled.limitations);
    pulled.limitations = limitations;
    Ok(pulled)
}

async fn read_all(conn: &mut Conn) -> Result<(RawCatalog, Vec<Limitation>), DbError> {
    conn.query(CANONICAL_PATH).await?;

    let mut warnings = Vec::new();
    for row in conn.query(&partitioned_query()).await? {
        let schema = text(&row, "schema_name")?;
        let name = text(&row, "table_name")?;
        // An `r` reaches here for one of three reasons, and the message has to
        // say which: none of them is visible in `relkind`.
        let kind = match text(&row, "kind")?.as_str() {
            "p" => "a partitioned table",
            "f" => "a foreign table",
            "r" if flag(&row, "row_security")? => {
                "a table with row-level security enabled, whose policies this model does not hold"
            }
            // `FORCE` makes the policies apply to the table's owner too, and
            // it is a separate flag: measured, a table can carry it with row
            // level security not enabled at all.
            "r" if flag(&row, "force_row_security")? => {
                "a table with `FORCE ROW LEVEL SECURITY`, which decides whether its own owner is \
                 subject to the policies"
            }
            // Policies with the switch off. They do nothing today, and that is
            // the trap: a rebuild drops them, and whoever turns row-level
            // security on afterwards gets a table with no policies — open,
            // where this one was about to be closed.
            "r" if number(&row, "policies")? > 0 => {
                "a table with row-level security policies that are not in force, which this model \
                 does not hold. They do nothing while the switch is off, so a rebuild would drop \
                 them silently and enabling row-level security afterwards would leave the table \
                 open"
            }
            "r" if text(&row, "persistence")? == "u" => "an UNLOGGED table",
            "r" if text(&row, "persistence")? == "t" => "a temporary table",
            // Which old-row data logical replication publishes, and whether a
            // replicated `UPDATE` or `DELETE` is allowed at all. `d` is the
            // default — the primary key — and is the only one the model can
            // read back, because it is the only one that is not a choice.
            "r" if text(&row, "replica_identity")? == "f" => {
                "a table with `REPLICA IDENTITY FULL`, which publishes every old column"
            }
            "r" if text(&row, "replica_identity")? == "n" => {
                "a table with `REPLICA IDENTITY NOTHING`, which no replicated `UPDATE` or \
                 `DELETE` may touch"
            }
            "r" if text(&row, "replica_identity")? == "i" => {
                "a table whose `REPLICA IDENTITY` is a named index rather than its primary key"
            }
            // `CREATE TABLE ... OF t`: the row shape is the composite type's,
            // and `ALTER TYPE` changes the table. Read back as an ordinary
            // table it is an independent one that no longer follows anything.
            "r" if optional_text(&row, "of_type")?.as_deref() != Some("-") => &format!(
                "a table of the composite type `{}`, whose shape follows it",
                optional_text(&row, "of_type")?.unwrap_or_default()
            ),
            // A `DO INSTEAD` rule decides what an `INSERT` on this table
            // actually does — including nothing at all.
            "r" if flag(&row, "has_rules")? => {
                "a table with rewrite rules, which decide what a write to it does"
            }
            // Not heap: how the rows are stored, and what the table can do
            // with them. The model has one kind of table.
            "r" if optional_text(&row, "access_method")?.as_deref() != Some("heap") => {
                "a table on a table access method other than `heap`"
            }
            // Both ends of an inheritance, because both are unusable and for
            // different reasons. **Measured**: a `SELECT` from the parent
            // returns the children's rows too, and `ALTER TABLE parent ADD
            // COLUMN` gives the column to every child — so a plan that changes
            // a managed parent changes tables nobody declared.
            "r" if flag(&row, "inherited_from")? => {
                "a table other tables inherit from, whose reads return their rows and whose \
                 changes recurse into them"
            }
            "r" => "a table that inherits from another",
            other => &format!("a relation of kind `{other}`"),
        };
        warnings.push(Limitation {
            table: TableName::new(&schema, &name),
            detail: format!(
                "`{schema}.{name}` is {kind}, which this model does not hold. It is left out of \
                 the pull entirely — not read back as an ordinary table, which would make a plan \
                 that recreates it without what makes it one."
            ),
        });
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
                min: number(&row, "seq_min")?,
                max: number(&row, "seq_max")?,
                cycles: flag(&row, "seq_cycle")?,
                cache: number(&row, "seq_cache")?,
            }),
            _ => None,
        };
        // The `serial` case: a sequence the column defaults from and does not
        // contain. The identity's own sequence is the other join and never
        // reaches here.
        let owned_sequence = optional_text(&row, "sequence_name")?;
        raw.columns.push(RawColumn {
            table_oid: number(&row, "table_oid")?,
            attnum: small(&row, "attnum")?,
            name: text(&row, "name")?,
            ty: text(&row, "ty")?,
            nullable: flag(&row, "nullable")?,
            default: optional_text(&row, "default_expr")?,
            identity,
            generated: first_char(&text(&row, "generated")?).is_some(),
            owned_sequence,
            default_sequences: optional_text(&row, "default_sequences")?,
            collation: optional_text(&row, "collation")?,
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
            deferrable: flag(&row, "deferrable")?,
            deferred: flag(&row, "deferred")?,
            definition: text(&row, "definition")?,
            expression: optional_text(&row, "expression")?,
            match_type: first_char(&text(&row, "match_type")?).unwrap_or(' '),
            delete_set_columns: numbers(
                &optional_text(&row, "delete_set_columns")?.unwrap_or_default(),
            ),
            index_oid: {
                let oid = number(&row, "index_oid")?;
                (oid != 0).then_some(oid)
            },
            enforced: flag(&row, "enforced")?,
            period: flag(&row, "period")?,
            no_inherit: flag(&row, "no_inherit")?,
            triggers_not_ordinary: flag(&row, "triggers_not_ordinary")?,
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
            valid: flag(&row, "is_valid")?,
            nulls_not_distinct: flag(&row, "nulls_not_distinct")?,
            key_count: small(&row, "key_count")?.max(0) as usize,
            columns: numbers(&text(&row, "keys")?),
            options: numbers(&text(&row, "options")?),
            filter: optional_text(&row, "filter")?,
            has_expressions: flag(&row, "has_expressions")?,
            method: text(&row, "method")?,
            nondefault_column_options: flag(&row, "nondefault_column_options")?,
        });
    }
    Ok((raw, warnings))
}

/// The one failure the snapshot cannot prevent, named so that it does not
/// arrive as a mystery.
///
/// `REPEATABLE READ` fixes the snapshot of the catalog *tables*, and
/// `pg_get_constraintdef`, `pg_get_expr` and `format_type` do not read them:
/// they go through the syscache, which follows the latest committed catalog
/// state. **Measured** — a `DROP TABLE` committed in another session while this
/// transaction is open, and the next rendering call fails:
///
/// ```text
/// ERROR:  cache lookup failed for attribute 1 of relation 115849
/// SQLSTATE: XX000
/// ```
///
/// That is the snapshot doing its job in the only direction it can. Without the
/// transaction the pull would have returned a table with no columns and called
/// it a schema; with it, the read fails. What is left is to say so, because the
/// driver renders `XX000` as `db error` and an unreadable failure is the third
/// thing CLAUDE.md's rule names (issue #167).
/// The one question that has to be asked **before** `BEGIN`.
///
/// PostgreSQL does not nest: inside an open transaction a plain `BEGIN` is a
/// warning and nothing more, so the `COMMIT` at the end of a successful read
/// would commit the caller's transaction — and whatever it had written — while
/// the snapshot and the read-only guarantee this whole framing exists for were
/// never established at all. There is no `COMMIT` that means "only mine".
///
/// Measured, this framing's own `BEGIN ISOLATION LEVEL ...` does fail inside a
/// transaction that has already run a statement, because `SET TRANSACTION` may
/// not follow a query. But that is an accident of when the caller last spoke,
/// not a guarantee, and it fails by poisoning their transaction rather than by
/// leaving it alone. The probe is asked first so that the answer is the same
/// either way.
///
/// It is a refusal rather than an accommodation. Reading inside somebody's
/// transaction would answer from their uncommitted writes, which is not what
/// "what the database looks like" means.
async fn refuse_a_caller_owned_transaction(conn: &mut Conn) -> Result<(), DbError> {
    conn.query(PROBE_SET).await?;
    let rows = conn.query(PROBE_READ).await?;
    let row = rows.first().ok_or_else(|| missing("probe"))?;
    if text(row, "probe")? == "yes" {
        return Err(DbError::Driver {
            code: None,
            message: "this connection already has an open transaction, and a pull cannot run \
                      inside one.\nThe read takes its own `REPEATABLE READ READ ONLY` \
                      transaction so that its five queries cannot disagree about what exists. \
                      PostgreSQL does not nest transactions, so running here would neither get \
                      that snapshot nor be able to end without committing yours. Commit or roll \
                      back first."
                .to_owned(),
        });
    }
    Ok(())
}

fn schema_changed_underneath(e: DbError) -> DbError {
    match &e {
        DbError::Driver { code, .. } if code.as_deref() == Some("XX000") => DbError::Driver {
            code: code.clone(),
            message: format!(
                "the catalog changed while it was being read: {e}.\n\
                 Something applied DDL to this database during the pull. The read is taken in \
                 one snapshot so that it cannot report half of a change as a whole schema, and \
                 this is that guard firing. Run it again when the other change has finished."
            ),
        },
        DbError::Driver { .. }
        | DbError::BadConnectionString(_)
        | DbError::Connect { .. }
        | DbError::ConnectTimeout { .. }
        | DbError::WrongSession { .. }
        | DbError::BadRow(_) => e,
    }
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

    /// Each word of the framing is load-bearing and none of them is visible in
    /// a passing test, so they are asserted here: without `REPEATABLE READ` the
    /// five reads are five snapshots, without `READ ONLY` nothing but this
    /// comment stops a future query from writing, and with `false` for
    /// `is_local` the search path outlives the transaction that set it.
    #[test]
    fn the_read_is_framed_as_one_snapshot_that_cannot_write_or_outlive_itself() {
        assert!(BEGIN.contains("REPEATABLE READ"));
        assert!(BEGIN.contains("READ ONLY"));
        assert!(CANONICAL_PATH.contains("'search_path', '', true"));
        // How a value prints, not only how a name does (DECISIONS 254). Each
        // was measured to change a rendered expression; pinned live by
        // `the_pull_does_not_move_when_the_sessions_search_path_does`.
        for setting in [
            "quote_all_identifiers",
            "datestyle",
            "intervalstyle",
            "timezone",
            "bytea_output",
            "extra_float_digits",
        ] {
            assert!(CANONICAL_PATH.contains(setting), "{setting}");
        }
        // Asked of this backend, and of the statement rather than the
        // transaction: `xact_start = query_start` is what "no transaction was
        // open before this one" looks like. Pinned live by
        // `a_pull_inside_the_callers_own_transaction_is_refused`.
        // Set, then read in a *separate* statement: the whole probe is that a
        // `SET LOCAL` outlives its own statement only inside a transaction.
        // Pinned live by `a_pull_inside_the_callers_own_transaction_is_refused`.
        assert!(PROBE_SET.contains("'pbps.in_a_transaction', 'yes', true"));
        assert!(PROBE_READ.contains("current_setting('pbps.in_a_transaction', true)"));
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
            assert!(sql.contains(NOT_ONE_OF_OURS), "{name}: {sql}");
            // By name: a prefix would also hide a project's own table
            // (`app.__pbps_customers`), which nothing refuses at declaration
            // time, and the pull would report it absent.
            assert!(!sql.contains("pbps\\_%"), "{name}: {sql}");
        }
    }

    /// A backslash in an ordinary string literal is only a backslash while
    /// `standard_conforming_strings` is on. The canonical scope pins it, and no
    /// query relies on that having worked: measured, with it off the engine
    /// eats the backslash, `'pg\_%'` becomes the pattern `pg_%`, and a
    /// project's schema called `pga` disappears from the pull.
    #[test]
    fn no_query_reads_differently_under_the_other_string_literal_mode() {
        for (name, sql) in [
            ("TABLES", tables_query()),
            ("PARTITIONED", partitioned_query()),
            ("COLUMNS", columns_query()),
            ("CONSTRAINTS", constraints_query()),
            ("INDEXES", indexes_query()),
        ] {
            assert!(!sql.contains('\\'), "{name} carries a backslash: {sql}");
        }
        assert!(!NOT_A_PROJECTS_SCHEMA.contains('\\'));
        assert!(CANONICAL_PATH.contains("standard_conforming_strings"));
    }
}
