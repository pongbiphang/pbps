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

use std::collections::BTreeMap;

use pbps_db::{Conn, DbError, Param, Row};

use pbps_model::{Schema, TableName};

// What the spelling checks found is `pbps-db`'s shape (DECISIONS 417).
pub use pbps_db::catalog::Spellings;

use crate::introspect::{
    GrantedKind, Limitation, LimitationTarget, Pulled, RawCatalog, RawColumn, RawConstraint,
    RawDefaultAcl, RawEmptyRoutineAcl, RawGrant, RawIdentity, RawIndex, RawModule, RawModuleArg,
    RawOtherGrant, RawOwner, RawRole, RawSharedDependency, RawTable, assemble,
};

/// The emitter quotes both parts of every table name. PostgreSQL compares
/// those identifiers exactly, independently of text-column collations.
pub fn matching_table_names(wanted: &[TableName], observed: &[TableName]) -> Vec<TableName> {
    wanted
        .iter()
        .filter(|name| observed.contains(name))
        .cloned()
        .collect()
}

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

/// [`NOT_A_PROJECTS_SCHEMA`], asked of one name in Rust.
///
/// The two have to agree, and the live test
/// `the_schemas_the_reader_skips_are_the_ones_a_declaration_may_not_name` puts
/// the question to the engine rather than to this file: it reads every schema
/// the cluster has, asks the SQL predicate which of them the pull keeps, and
/// requires this function to answer the same about each.
///
/// It exists because a declaration may name a schema directly — `schema::x` is
/// a grant target — and a grant in a schema the pull does not read comes back
/// as absent. Recorded that way, the apply's own read-back refuses it and
/// every plan after it proposes the same `GRANT` again
/// ([`crate::validate::role`] refuses the declaration instead).
///
/// The table and module checks that had this predicate written out ask it here
/// now: three copies of one filter are three things to remember when a
/// fifteenth schema of the engine's own arrives.
pub(crate) fn a_projects_schema(name: &str) -> bool {
    !matches!(name, "pg_catalog" | "information_schema") && !name.starts_with("pg_")
}

/// The ledger names and schema come from the ledger implementation, so the
/// catalog and validation reserve exactly the same identities (DECISIONS 284).
/// A project's same-named tables in another schema remain ordinary tables.
pub(crate) const OURS: [&str; 2] = [
    pbps_db::ledger::STATE_TABLE_NAME,
    pbps_db::ledger::LOCK_TABLE_NAME,
];

pub(crate) fn is_ours(name: &TableName) -> bool {
    name.schema == crate::state::LEDGER_SCHEMA && OURS.contains(&name.name.as_str())
}

/// Catalog predicates use the relation `c` and its namespace `n`. This also
/// scopes the unsupported-table and trigger readers: visibility must agree
/// across every projection of the same table.
fn not_one_of_ours() -> String {
    format!(
        "NOT (n.nspname = {} AND c.relname IN ({}))",
        crate::emit::value_literal(crate::state::LEDGER_SCHEMA),
        OURS.map(crate::emit::value_literal).join(", ")
    )
}

/// The grants reader also sees views and sequences. A view in the ledger's
/// schema is still a module, so its ACL must remain visible (DECISIONS 386).
fn not_one_of_our_tables() -> String {
    format!("(c.relkind <> 'r' OR {})", not_one_of_ours())
}

/// `relkind = 'r'`, and the filter is the whole point: `pg_attribute` holds a
/// row for every index and sequence column too, so a reader without it reports
/// `child_id_seq` and `child_pk` as tables with columns.
///
/// A partitioned table (`p`) is deliberately not here. It is a table the model
/// cannot hold — the partition key has nowhere to go — and pulling it as an
/// ordinary one would produce a plan that recreates it without its partitions.
/// It is reported by [`PARTITIONED`] instead.
fn tables_query() -> String {
    let not_one_of_ours = not_one_of_ours();
    let not_an_extensions = not_an_extensions("c.oid", "pg_class");
    let held_tree = partition_tree_table();
    format!(
        "SELECT c.oid::int8 AS oid, n.nspname AS schema_name, c.relname AS table_name,
            CASE WHEN c.relkind = 'p'
                 THEN (SELECT pg_catalog.to_jsonb(pg_catalog.array_agg(a.attname::text ORDER BY k.n))
                         FROM pg_catalog.pg_partitioned_table pt
                        CROSS JOIN LATERAL pg_catalog.unnest(pt.partattrs::int2[])
                                   WITH ORDINALITY AS k(attnum, n)
                         JOIN pg_catalog.pg_attribute a
                           ON a.attrelid = c.oid AND a.attnum = k.attnum
                        WHERE pt.partrelid = c.oid)
            END AS partition_key,
            CASE WHEN c.relispartition
                 THEN (SELECT h.inhparent::int8 FROM pg_catalog.pg_inherits h
                        WHERE h.inhrelid = c.oid)
            END AS partition_parent,
            CASE WHEN c.relispartition
                 THEN pg_catalog.pg_get_expr(c.relpartbound, c.oid)
            END AS partition_bound,
            c.relreplident::text AS replica_identity,
            COALESCE((SELECT x.indexrelid::int8 FROM pg_catalog.pg_index x
                       WHERE x.indrelid = c.oid AND x.indisreplident), 0) AS identity_index,
            c.relpersistence::text AS persistence,
            COALESCE(c.reloptions, '{{}}'::text[]) AS reloptions,
            COALESCE((SELECT tc.reloptions FROM pg_catalog.pg_class tc
                       WHERE tc.oid = c.reltoastrelid), '{{}}'::text[]) AS toast_reloptions
       FROM pg_catalog.pg_class c
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE ({ORDINARY_TABLE} OR {held_tree})
        AND {NOT_A_PROJECTS_SCHEMA}
        AND {not_one_of_ours}
        AND {not_an_extensions}
      ORDER BY n.nspname, c.relname"
    )
}

/// Whether the partitioned table whose oid is `root` heads a tree this model
/// holds (#1170, DEC-1170.1): RANGE over plain columns, and partitions that
/// are their parent's and nothing else, so that rebuilding the tree from the
/// parent and each partition's bound gives back what is there.
///
/// **The parent**: not itself a partition, RANGE, and a key whose deparsed
/// text is its columns' quoted names and nothing else, so that an expression,
/// an operator class or a collation, each of which `pg_get_partkeydef` prints,
/// leaves it out. Permanent, no storage parameters, the default replica
/// identity, no access method of its own, no row security, rules or triggers:
/// none of them is in the partitioned table's declaration yet.
///
/// **Each partition**: attached (no detach pending), an ordinary heap
/// table, the same in all of that but its persistence and storage
/// parameters, which are its own (#1580), with no grant on a column, which no
/// table declares (a grant on the partition itself is its own, read as any
/// table's, #1579), whose columns are inherited and its parent's in
/// the parent's order with the parent's identities and generations. Its
/// defaults and NOT NULLs are the parent's or its own (#1578): a default of
/// its own where the parent's column has one or none, a NOT NULL of its own
/// where the parent's is nullable, but never a default dropped where the
/// parent's has one. Measured on 16 and 18: a table `ATTACH`ed as a partition
/// keeps its own column order and has none of the parent's defaults, which
/// `PARTITION OF` would give it, so `attislocal` alone does not say so. A
/// column the table dropped before it was attached stays local, measured on
/// 16 and 18, and is no column (#1545). Every constraint a clone
/// (`conparentid`, the keys and foreign keys), inherited and not local (a
/// CHECK, and on 18 a NOT NULL row), a CHECK of its own, local and
/// inherited from nowhere (#1577), or on 18 a validated NOT NULL row of its
/// own (#1578). An index is a clone of the parent's or the
/// partition's own (#1577); one backing a constraint of its own is left out
/// with that constraint. Anything else in any partition leaves the whole tree
/// out, named.
fn partition_tree(root: &str) -> String {
    // NOT NULL and a plain default are compared below, where a partition may
    // hold its own (#1578); a generation expression is in `pg_attrdef` too,
    // and is compared here, where it must be the parent's.
    let columns = |rel: &str| {
        format!(
            "(SELECT pg_catalog.array_agg(ROW(a.attname, a.atttypid, a.atttypmod, a.attcollation,
                                               a.attidentity, a.attgenerated,
                                               CASE WHEN a.attgenerated <> ''
                                                    THEN pg_catalog.pg_get_expr(d.adbin, d.adrelid)
                                               END)::text
                                           ORDER BY a.attnum)
                FROM pg_catalog.pg_attribute a
                LEFT JOIN pg_catalog.pg_attrdef d
                  ON d.adrelid = a.attrelid AND d.adnum = a.attnum
               WHERE a.attrelid = {rel} AND a.attnum > 0 AND NOT a.attisdropped)"
        )
    };
    let plain = |rel: &str| {
        format!(
            "NOT {rel}.relrowsecurity AND NOT {rel}.relforcerowsecurity
             AND NOT {rel}.relhasrules AND {rel}.reloftype = 0
             AND {rel}.relreplident = 'd'
             AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_policy pol
                              WHERE pol.polrelid = {rel}.oid)
             AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_trigger tg
                              WHERE tg.tgrelid = {rel}.oid AND NOT tg.tgisinternal)"
        )
    };
    // A partition's persistence and storage parameters are its own (#1580),
    // measured on 16 and 18: neither is taken from the parent, which has no
    // storage and refuses parameters, and 16's UNLOGGED parent gives its
    // partitions nothing. Its TOAST table's are not held, as a table's are
    // not, and leave the tree out below.
    let parent_plain = format!(
        "{} AND pc.relpersistence = 'p' AND pc.reloptions IS NULL",
        plain("pc")
    );
    let child_plain = format!("{} AND ch.relpersistence IN ('p', 'u')", plain("ch"));
    let parent_columns = columns("pc.oid");
    let child_columns = columns("ch.oid");
    format!(
        "EXISTS (SELECT 1 FROM pg_catalog.pg_class pc
                   JOIN pg_catalog.pg_partitioned_table pt ON pt.partrelid = pc.oid
                  WHERE pc.oid = {root} AND pc.relkind = 'p' AND NOT pc.relispartition
                    AND pt.partstrat = 'r' AND pc.relam = 0
                    AND {parent_plain}
                    AND pg_catalog.pg_get_partkeydef(pc.oid) = 'RANGE (' ||
                        (SELECT pg_catalog.string_agg(pg_catalog.quote_ident(a.attname), ', '
                                                      ORDER BY k.n)
                           FROM pg_catalog.unnest(pt.partattrs::int2[])
                                WITH ORDINALITY AS k(attnum, n)
                           JOIN pg_catalog.pg_attribute a
                             ON a.attrelid = pc.oid AND a.attnum = k.attnum) || ')'
                    AND NOT EXISTS (
                      SELECT 1 FROM pg_catalog.pg_inherits h
                        JOIN pg_catalog.pg_class ch ON ch.oid = h.inhrelid
                       WHERE h.inhparent = pc.oid
                         AND NOT (ch.relkind = 'r' AND ch.relispartition
                                  AND NOT h.inhdetachpending
                                  AND {child_plain}
                                  AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute ca
                                                   WHERE ca.attrelid = ch.oid AND ca.attnum > 0
                                                     AND NOT ca.attisdropped
                                                     AND (ca.attislocal OR ca.attacl IS NOT NULL))
                                  AND ch.relam = (SELECT am.oid FROM pg_catalog.pg_am am
                                                   WHERE am.amname = 'heap')
                                  AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_class tc
                                                   WHERE tc.oid = ch.reltoastrelid
                                                     AND tc.reloptions IS NOT NULL)
                                  AND {child_columns} IS NOT DISTINCT FROM {parent_columns}
                                  AND NOT EXISTS (
                                    SELECT 1 FROM pg_catalog.pg_attribute ca
                                      JOIN pg_catalog.pg_attribute pa
                                        ON pa.attrelid = pc.oid AND pa.attname = ca.attname
                                      LEFT JOIN pg_catalog.pg_attrdef cd
                                        ON cd.adrelid = ca.attrelid AND cd.adnum = ca.attnum
                                      LEFT JOIN pg_catalog.pg_attrdef pd
                                        ON pd.adrelid = pa.attrelid AND pd.adnum = pa.attnum
                                     WHERE ca.attrelid = ch.oid AND ca.attnum > 0
                                       AND NOT ca.attisdropped
                                       AND ((pa.attnotnull AND NOT ca.attnotnull)
                                            OR (pd.oid IS NOT NULL AND cd.oid IS NULL)))
                                  AND NOT EXISTS (
                                    SELECT 1 FROM pg_catalog.pg_constraint k
                                     WHERE k.conrelid = ch.oid
                                       AND k.conparentid = 0
                                       AND NOT (k.contype IN ('c', 'n') AND NOT k.conislocal)
                                       AND NOT (k.contype = 'c' AND k.conislocal
                                                AND k.coninhcount = 0)
                                       AND NOT (k.contype = 'n' AND k.convalidated)))))"
    )
}

/// Whether `c` is a table of a partition tree [`partition_tree`] holds: its
/// parent, or one of its partitions.
fn partition_tree_table() -> String {
    format!(
        "((c.relkind = 'p' AND {}) OR (c.relkind = 'r' AND c.relispartition AND {}))",
        partition_tree("c.oid"),
        partition_tree(
            "(SELECT h.inhparent FROM pg_catalog.pg_inherits h WHERE h.inhrelid = c.oid)"
        )
    )
}

/// A `USING INDEX` replica identity whose index is gone: PostgreSQL lets that
/// index be dropped and keeps `relreplident = 'i'` with no index marked
/// `indisreplident`, which identifies no row (measured on 16 and 18, #1444).
/// Not `nothing`, which is a choice someone made; a state no declaration
/// spells, so the table is left out and named.
macro_rules! identity_names_no_index {
    () => {
        "(c.relreplident = 'i'
             AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_index x
                              WHERE x.indrelid = c.oid AND x.indisreplident))"
    };
}
const IDENTITY_NAMES_NO_INDEX: &str = identity_names_no_index!();

/// What makes a `pg_class` row `c` a table this model holds: the predicate of
/// [`tables_query`], as one string, so that the one other reader that has to
/// agree with it — the trigger arm of [`modules_query`], and its complement in
/// [`unheld_modules_query`] — cannot drift from it. Each flag is the negation
/// of a case [`partitioned_query`] names.
const ORDINARY_TABLE: &str = concat!(
    "c.relkind = 'r'
        AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i
                         WHERE i.inhrelid = c.oid OR i.inhparent = c.oid)
        AND NOT c.relrowsecurity
        AND NOT c.relforcerowsecurity
        AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_policy p WHERE p.polrelid = c.oid)
        AND c.relpersistence IN ('p', 'u')
        AND NOT ",
    identity_names_no_index!(),
    "
        AND NOT c.relhasrules
        AND c.reloftype = 0
        AND c.relam = (SELECT am.oid FROM pg_catalog.pg_am am WHERE am.amname = 'heap')"
);

/// `pg_get_viewdef` omits these options. Only the two boolean options set to
/// false have the behavior a plain declaration recreates; check options and
/// unknown options must be named instead. The engine keeps boolean aliases
/// such as `off` and `0` in `reloptions`, so compare their values, not spelling.
/// Shared by the view reader and the trigger-parent predicate (DECISIONS 304).
const DEFAULT_VIEW_OPTIONS: &str = "NOT EXISTS (
            SELECT 1 FROM pg_catalog.pg_options_to_table(c.reloptions) AS opt
             WHERE (CASE WHEN opt.option_name IN ('security_invoker', 'security_barrier')
                         THEN opt.option_value::boolean
                         ELSE true END) IS DISTINCT FROM false)";

/// `pg_get_triggerdef` cannot encode DISABLE, REPLICA or ALWAYS. Only ordinary
/// mode survives CREATE; share the rule with the omission inventory (473).
const DEFAULT_TRIGGER_MODE: &str = "tg.tgenabled = 'O'";

/// Whether the relation `c` a trigger is on is one the pull reads back — a
/// view, or a table [`tables_query`] holds.
///
/// A trigger is read with its relation or not at all. **Measured**, the engine
/// allows a trigger on a partitioned table and on an `UNLOGGED` one, and a
/// trigger read back without the relation it is on is a module whose `on:`
/// names a table the schema does not have — `check_names` refuses that, so
/// the whole pull was one nothing could load. The relation is already named
/// as a limitation by its own reader; the trigger is named beside it
/// by [`unheld_modules_query`] rather than silently gone.
fn on_a_relation_the_pull_holds() -> String {
    let not_one_of_ours = not_one_of_ours();
    // The relation readers' own filter too, over both arms: measured, a user's
    // `INSTEAD OF` trigger on a view an extension owns is not extension-owned
    // itself, and read back it named a view the pull had left out. A table an
    // extension owns leaves the pull for the same reason (DECISIONS 305), so a
    // user's trigger on one would name a table the schema does not have.
    let not_an_extensions = not_an_extensions("c.oid", "pg_class");
    format!(
        "({not_an_extensions}
          AND ((c.relkind = 'v' AND {DEFAULT_VIEW_OPTIONS})
               OR ({ORDINARY_TABLE} AND {not_one_of_ours})))"
    )
}

/// Objects owned by an extension, which are nobody's declarations.
///
/// `CREATE EXTENSION` installs functions, views, types — and **tables**, which
/// `ALTER EXTENSION … ADD TABLE` also hands over — that belong to the extension
/// and are dropped with it. `CREATE EXTENSION … SCHEMA app` puts them in a
/// project's schema, where a reader without this filter reports every one as an
/// undeclared object and the next plan offers to drop them — objects whose
/// declaration lives in a `.sql` file the extension owns and this project does
/// not have. There is no way to declare one back: under `unmanaged: error` the
/// next command refuses a database nothing is wrong with.
///
/// Left out rather than reported: they are not a limitation of the model, they
/// are somebody else's objects. `DROP EXTENSION` is how one goes away
/// (DECISIONS 305).
const NOT_AN_EXTENSIONS: &str = "NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d
                    WHERE d.objid = %OID% AND d.classid = %CLASS%::regclass
                      AND d.deptype = 'e')";

fn not_an_extensions(oid: &str, class: &str) -> String {
    NOT_AN_EXTENSIONS
        .replace("%OID%", oid)
        .replace("%CLASS%", &format!("'pg_catalog.{class}'"))
}

/// Which `pg_proc` rows are modules this model holds.
///
/// One spelling, used by both the module query and the argument query, and the
/// reason is not tidiness: a routine whose row the first query returns and the
/// second does not is keyed as `f()` — a different object from `f(integer)`,
/// under a name that looks right. One rule, one home (PITFALLS).
const ROUTINE_IS_A_MODULE: &str = "p.prokind IN ('f', 'p')";

/// Views, functions, procedures and triggers, each with the text this engine
/// deparses for it (ADR-0009 §2).
///
/// One query and not four, because the assembler wants one list and the four
/// catalogs answer the same four questions — the kind, where it lives, what it
/// is called, and what it says. `pg_get_functiondef` is asked only of `f` and
/// `p`: **measured**, it refuses an aggregate by name (`"agg" is an aggregate
/// function`), so a `prokind` filter is not tidiness but the difference between
/// a pull and an error.
///
/// The identity's argument types are a second query — one row each — rather
/// than a joined string, because a type name may contain the character that
/// would separate them: `format_type` quotes one that needs it, and a reader
/// splitting on commas would cut `"a,b"` in half.
fn modules_query() -> String {
    let view_not_extension = not_an_extensions("c.oid", "pg_class");
    let proc_not_extension = not_an_extensions("p.oid", "pg_proc");
    let trigger_not_extension = not_an_extensions("tg.oid", "pg_trigger");
    let held = on_a_relation_the_pull_holds();
    format!(
        "SELECT c.oid::int8 AS oid, 'v' AS kind, n.nspname AS schema_name,
                c.relname AS name, '' AS on_table,
                pg_catalog.pg_get_viewdef(c.oid, true) AS definition
           FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE c.relkind = 'v'
            AND {DEFAULT_VIEW_OPTIONS}
            AND {NOT_A_PROJECTS_SCHEMA}
            AND {view_not_extension}
          UNION ALL
         SELECT p.oid::int8, p.prokind::text, n.nspname, p.proname, '',
                pg_catalog.pg_get_functiondef(p.oid)
           FROM pg_catalog.pg_proc p
           JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
          WHERE {ROUTINE_IS_A_MODULE}
            AND {NOT_A_PROJECTS_SCHEMA}
            AND {proc_not_extension}
          UNION ALL
         SELECT tg.oid::int8, 't', n.nspname, tg.tgname, c.relname,
                pg_catalog.pg_get_triggerdef(tg.oid)
           FROM pg_catalog.pg_trigger tg
           JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE NOT tg.tgisinternal
            AND {DEFAULT_TRIGGER_MODE}
            AND {NOT_A_PROJECTS_SCHEMA}
            AND {trigger_not_extension}
            AND {held}
          ORDER BY 3, 4, 1"
    )
}

/// One row per routine argument, in position order.
///
/// `format_type` under the canonical empty `search_path`, which is what makes
/// this the identity the engine keys on: a built-in bare, a user type
/// schema-qualified, every modifier already discarded (ADR-0009 §1).
fn module_args_query() -> String {
    let proc_not_extension = not_an_extensions("p.oid", "pg_proc");
    format!(
        "SELECT p.oid::int8 AS oid, u.pos::int8 AS pos,
                pg_catalog.format_type(u.ty, NULL) AS ty
           FROM pg_catalog.pg_proc p
           JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
      CROSS JOIN LATERAL pg_catalog.unnest(p.proargtypes)
                    WITH ORDINALITY AS u(ty, pos)
          WHERE {ROUTINE_IS_A_MODULE}
            AND {NOT_A_PROJECTS_SCHEMA}
            AND {proc_not_extension}
          ORDER BY 1, 2"
    )
}

/// The module-shaped objects the model does not hold, so that they are named
/// rather than missing — the same rule as [`unheld_query`] for tables.
///
/// A materialized view is a view with rows; a view's non-default options do not
/// survive `pg_get_viewdef`; an aggregate and a window function are `pg_proc`
/// entries `pg_get_functiondef` refuses outright. Reading any of them back as
/// the ordinary kind would make a plan that recreates it as something else.
///
/// [`NOT_AN_EXTENSIONS`] here too, and for the same reason it is on the module
/// queries: an extension installed into a project's schema owns aggregates and
/// materialized views of its own, and DECISIONS 305 says those are left out
/// **silently** rather than reported. Reported, they are worse than noise —
/// `managed_limitations` refuses every command for a limitation whose name is
/// in the managed set, so an extension object colliding with a declared name
/// would refuse a plan that is correct. A filter the ordinary reader applies
/// and the limitation reader does not is a rule with a hole in it.
fn unheld_modules_query() -> String {
    let view_not_extension = not_an_extensions("c.oid", "pg_class");
    let proc_not_extension = not_an_extensions("p.oid", "pg_proc");
    let trigger_not_extension = not_an_extensions("tg.oid", "pg_trigger");
    let held = on_a_relation_the_pull_holds();
    format!(
        "SELECT n.nspname AS schema_name, c.relname AS name,
                'a materialized view, which holds rows a plan would have to refresh' AS detail,
                'm' AS kind, c.oid::int8 AS oid, '' AS on_table
           FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE c.relkind = 'm'
            AND {NOT_A_PROJECTS_SCHEMA}
            AND {view_not_extension}
          UNION ALL
         SELECT n.nspname, c.relname,
                'a view with options ' || pg_catalog.array_to_string(c.reloptions, ', '),
                'v', c.oid::int8, ''
           FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE c.relkind = 'v'
            AND NOT ({DEFAULT_VIEW_OPTIONS})
            AND {NOT_A_PROJECTS_SCHEMA}
            AND {view_not_extension}
          UNION ALL
         SELECT n.nspname, p.proname,
                CASE p.prokind WHEN 'a' THEN 'an aggregate function'
                               ELSE 'a window function' END, p.prokind::text, p.oid::int8, ''
           FROM pg_catalog.pg_proc p
           JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
          WHERE p.prokind IN ('a', 'w')
            AND {NOT_A_PROJECTS_SCHEMA}
            AND {proc_not_extension}
          UNION ALL
         SELECT n.nspname, tg.tgname,
                'a trigger on `' || n.nspname || '.' || c.relname
                  || '` (not a table or a view this pull holds)', 't', tg.oid::int8, c.relname
           FROM pg_catalog.pg_trigger tg
           JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE NOT tg.tgisinternal
            AND {NOT_A_PROJECTS_SCHEMA}
            AND {trigger_not_extension}
            AND NOT {held}
          UNION ALL
         SELECT n.nspname, tg.tgname,
                CASE tg.tgenabled WHEN 'D' THEN 'a trigger that is disabled'
                                  WHEN 'R' THEN 'a trigger that fires only on a replica'
                                  WHEN 'A' THEN 'a trigger that fires always, replica or not'
                                  ELSE 'a trigger with an unknown enable mode' END
                  || ' (`pg_trigger.tgenabled` = ' || tg.tgenabled::text || ')',
                't', tg.oid::int8, c.relname
           FROM pg_catalog.pg_trigger tg
           JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE NOT tg.tgisinternal
            AND NOT ({DEFAULT_TRIGGER_MODE})
            AND {NOT_A_PROJECTS_SCHEMA}
            AND {trigger_not_extension}
            AND {held}
          ORDER BY 1, 2"
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
///
/// An extension's table is not here either, for the reason
/// [`unheld_modules_query`] leaves an extension's materialized view out: it is
/// nobody's declaration, and `managed_limitations` refuses every command for a
/// limitation whose name is in the managed set (DECISIONS 305).
fn partitioned_query() -> String {
    let not_one_of_ours = not_one_of_ours();
    let not_an_extensions = not_an_extensions("c.oid", "pg_class");
    let held_tree = partition_tree_table();
    format!(
        "SELECT n.nspname AS schema_name, c.relname AS table_name, c.relkind::text AS kind,
            c.relrowsecurity AS row_security, c.relpersistence::text AS persistence,
            c.relforcerowsecurity AS force_row_security,
            (SELECT pg_catalog.count(*) FROM pg_catalog.pg_policy p
              WHERE p.polrelid = c.oid)::int8 AS policies,
            {IDENTITY_NAMES_NO_INDEX} AS identity_names_no_index,
            c.relhasrules AS has_rules, am.amname AS access_method,
            c.reloftype::regtype::text AS of_type,
            EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i WHERE i.inhparent = c.oid)
              AS inherited_from,
            c.relispartition AS partition
       FROM pg_catalog.pg_class c
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
       LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam
      WHERE {NOT_A_PROJECTS_SCHEMA}
        AND {not_one_of_ours}
        AND {not_an_extensions}
        AND NOT {held_tree}
        AND (c.relkind IN ('p', 'f')
             OR (c.relkind = 'r'
                 AND (EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i
                               WHERE i.inhrelid = c.oid OR i.inhparent = c.oid)
                      OR c.relrowsecurity
                      OR c.relforcerowsecurity
                      OR EXISTS (SELECT 1 FROM pg_catalog.pg_policy p
                                  WHERE p.polrelid = c.oid)
                      OR c.relpersistence NOT IN ('p', 'u')
                      OR {IDENTITY_NAMES_NO_INDEX}
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
      WHERE c.relkind IN ('r', 'p')
        AND {NOT_A_PROJECTS_SCHEMA}
        AND a.attnum > 0
        AND NOT a.attisdropped
      ORDER BY a.attrelid, a.attnum"
    )
}

/// Every constraint of every kind, including the ones this reader does not
/// know: the assembler decides what to do with each, and a kind it has never
/// seen is reported rather than dropped in a `WHERE` nobody re-reads.
///
/// `schema_name` and `table_name` ride along for [`deparsed_away`]'s sake, not
/// the assembler's: `RawConstraint` already carries `table_oid`, but the error
/// path that fires when `pg_get_constraintdef` comes back `NULL` needs a name
/// an operator can read, and the two joins that produce it are already here.
fn constraints_query() -> String {
    // A contype='t' row has an internal pg_depend edge (deptype='i') to its
    // user constraint trigger: DROP TRIGGER removes both. The module holds
    // its definition; listing the companion again invents a limitation (473).
    // These flags arrived in PostgreSQL 18. JSON field lookup can represent
    // their absence on older catalogs without making the SQL fail to parse;
    // pre-18 constraints are enforced and have no temporal period (424).
    format!(
        "SELECT con.conrelid::int8 AS table_oid, con.conname AS name, con.contype::text AS kind,
            n.nspname AS schema_name, c.relname AS table_name,
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
            COALESCE((to_jsonb(con)->>'conenforced')::boolean, true) AS enforced,
            COALESCE((to_jsonb(con)->>'conperiod')::boolean, false) AS period,
            con.connoinherit AS no_inherit,
            NOT con.conislocal OR con.coninhcount > 0 AS inherited,
            EXISTS (SELECT 1 FROM pg_catalog.pg_trigger tg
                     WHERE tg.tgconstraint = con.oid AND tg.tgenabled <> 'O')
              AS triggers_not_ordinary
       FROM pg_catalog.pg_constraint con
       JOIN pg_catalog.pg_class c ON c.oid = con.conrelid
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE c.relkind IN ('r', 'p')
        AND con.contype <> 't'
        AND con.conparentid = 0
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
/// does it. And a key's class is reported unless it `IS NOT DISTINCT FROM` the
/// resolved default, so a default this query cannot resolve reports the class
/// rather than passing it as the default — not being able to tell is not the
/// same as there being nothing to tell.
///
/// An expression key has no column to take a type from; its type is the index
/// relation's own attribute at that position, which is the expression's result
/// type, so its default class is resolved the same way (DEC-1169.2).
/// `key_texts` holds each expression key's text as the engine renders it, and
/// `''` for a column key. An expression's own collation is not in the
/// catalog, so an expression key under anything but the database default
/// (`100`) is reported rather than guessed at.
///
/// `key_classes` holds one entry per key, `''` for the default and
/// `schema.name` otherwise, so the reader can accept exactly the classes the
/// model holds and name every other one (DEC-1169.1). A collation that is not the
/// column's own is its own flag: no class makes up for it.
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
            EXISTS (SELECT 1 FROM pg_catalog.pg_inherits h WHERE h.inhrelid = i.indexrelid)
              AS attached,
            am.amname AS method,
            COALESCE(ic.reloptions, '{{}}'::text[]) AS reloptions,
            (SELECT COALESCE(pg_catalog.json_agg(
                      CASE WHEN i.indclass[k.n - 1] IS NOT DISTINCT FROM COALESCE(
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
                           THEN ''
                           ELSE ocn.nspname || '.' || occ.opcname END
                      ORDER BY k.n), '[]'::pg_catalog.json)
               FROM pg_catalog.generate_series(1, i.indnkeyatts) AS k(n)
               JOIN pg_catalog.pg_opclass occ ON occ.oid = i.indclass[k.n - 1]
               JOIN pg_catalog.pg_namespace ocn ON ocn.oid = occ.opcnamespace
               LEFT JOIN LATERAL (
                 SELECT COALESCE(
                   (SELECT t.typbasetype FROM pg_catalog.pg_type t
                     WHERE t.oid = a.atttypid AND t.typtype = 'd' AND t.typbasetype <> 0),
                   a.atttypid) AS coltype
                   FROM pg_catalog.pg_attribute a
                  WHERE (i.indkey[k.n - 1] <> 0
                         AND a.attrelid = i.indrelid AND a.attnum = i.indkey[k.n - 1])
                     OR (i.indkey[k.n - 1] = 0
                         AND a.attrelid = i.indexrelid AND a.attnum = k.n)) AS col ON true
            ) AS key_classes,
            (SELECT COALESCE(pg_catalog.json_agg(
                      CASE WHEN i.indkey[k.n - 1] = 0
                           THEN pg_catalog.pg_get_indexdef(i.indexrelid, k.n::int4, true)
                           ELSE '' END
                      ORDER BY k.n), '[]'::pg_catalog.json)
               FROM pg_catalog.generate_series(1, i.indnkeyatts) AS k(n)
            ) AS key_texts,
            EXISTS (
              SELECT 1
                FROM pg_catalog.generate_series(1, i.indnkeyatts) AS k(n)
                JOIN pg_catalog.pg_attribute a
                  ON a.attrelid = i.indrelid AND a.attnum = i.indkey[k.n - 1]
               WHERE i.indcollation[k.n - 1] <> 0
                 AND i.indcollation[k.n - 1] <> a.attcollation
            ) OR EXISTS (
              SELECT 1
                FROM pg_catalog.generate_series(1, i.indnkeyatts) AS k(n)
               WHERE i.indkey[k.n - 1] = 0
                 AND i.indcollation[k.n - 1] NOT IN (0, 100)
            ) AS nondefault_collation
       FROM pg_catalog.pg_index i
       JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid
       JOIN pg_catalog.pg_class c ON c.oid = i.indrelid
       JOIN pg_catalog.pg_am am ON am.oid = ic.relam
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE c.relkind IN ('r', 'p')
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
pub(crate) const PROBE_READ: &str =
    "SELECT COALESCE(current_setting('pbps.in_a_transaction', true), '') AS probe";

/// A value this call invents, because a constant one can already be sitting in
/// the session.
///
/// The probe is `set_config(…, is_local => true)` in one statement and
/// `current_setting` in the next: inside a transaction the setting survives to
/// be read, and outside one the implicit transaction ends and it does not.
/// Compared against a constant, that read has a third outcome nobody asked
/// for — a session that already carries
/// `SET pbps.in_a_transaction = 'yes'` answers `'yes'` on an autocommit
/// connection, and every caller of the probe then believes something the
/// engine never said. On the rebuild's side that means `LOCK TABLE` released
/// at the end of its own statement, and a carried-state read that is not
/// serialized with the `DROP` at all: a guard still in the code and no longer
/// guarding. On the pull's side it means the opposite and just as bad — a
/// connection with no transaction refused as though it had one.
///
/// A value invented per call cannot be sitting in the session. The read is
/// then only equal if *this* call's `set_config` survived, which is exactly
/// the question. Hex and `-` only, because it is interpolated into a
/// statement.
pub(crate) fn probe_token() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64);
    format!("{:x}-{:x}-{:x}", std::process::id(), nanos, n)
}

pub(crate) fn probe_set(token: &str) -> String {
    format!("SELECT pg_catalog.set_config('pbps.in_a_transaction', '{token}', true)")
}

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
///
/// `jit` is the one setting here that changes no rendering, only the cost of
/// reading (DEC-1445.1). The catalog batch's planner estimate is over the default
/// `jit_above_cost`, almost all of it the per-key operator-class subqueries in
/// [`indexes_query`], so a server with JIT compiled the batch on every read.
/// **Measured** on 18.6 against a near-empty database: ~280 ms of compilation
/// (1,048 functions) for a statement that executes in ~15 ms. Off here rather
/// than by trimming the estimate, because the estimate grows with the catalog
/// and the threshold is the server's configuration: a larger database, or
/// `jit_above_cost = 0`, would cross it again. Local like the rest, so the
/// statements an apply runs after the read are under the session's own `jit`.
pub(crate) const CANONICAL_PATH: &str = "SELECT pg_catalog.set_config('search_path', '', true),
       pg_catalog.set_config('quote_all_identifiers', 'off', true),
       pg_catalog.set_config('datestyle', 'ISO, MDY', true),
       pg_catalog.set_config('intervalstyle', 'postgres', true),
       pg_catalog.set_config('timezone', 'UTC', true),
       pg_catalog.set_config('bytea_output', 'hex', true),
       pg_catalog.set_config('extra_float_digits', '1', true),
       pg_catalog.set_config('standard_conforming_strings', 'on', true),
       pg_catalog.set_config('jit', 'off', true)";

/// Reads the whole managed set back: one snapshot, one search path, no writes.
///
/// The catalog is one statement snapshot (DECISIONS 423). The owned read
/// retains `REPEATABLE READ READ ONLY`, shared with the row reader whose
/// table and alias queries must also agree on one snapshot.
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
    introspect_in(conn, Scope::Own).await
}

/// Reads the managed set back **inside the transaction the caller holds**,
/// seeing what it has written and not yet committed.
///
/// For one caller: the apply's read-back, which records what a plan built in
/// the same transaction as the build, so that the ledger entry is as atomic as
/// the change it describes and a read that failed undoes the statements too
/// (`pbps-cli`, DECISIONS 147). [`introspect`] refuses that transaction (253)
/// because it cannot tell whose it is; this function is the caller saying
/// "mine, and read what I have done".
///
/// Under a savepoint, so that the canonical scope goes back when the read
/// ends and the caller's transaction is left exactly as deep, as writable and
/// under the same settings as it was found (see [`SAVEPOINT`]). The caller
/// remains READ COMMITTED so its closing view is not frozen before the DDL.
/// All catalog queries now share one statement snapshot; the CLI revalidates
/// the managed projection before recording it (DECISIONS 423).
pub async fn introspect_within_transaction(conn: &mut Conn) -> Result<Pulled, DbError> {
    introspect_in(conn, Scope::CallersTransaction).await
}

async fn introspect_in(conn: &mut Conn, scope: Scope) -> Result<Pulled, DbError> {
    open(conn, scope).await?;
    let outcome = read_all(conn).await;
    let raw = close(conn, scope, outcome)
        .await
        .map_err(schema_changed_underneath)?;

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
    pulled.unmanaged_modules.extend(raw.2);
    pulled.unmanaged_modules.sort();
    pulled.unmanaged_modules.dedup();
    Ok(pulled)
}

type CatalogRead = (
    RawCatalog,
    Vec<Limitation>,
    Vec<pbps_db::catalog::UnmanagedModule>,
);

type CatalogBatch = BTreeMap<String, Vec<serde_json::Value>>;

/// All catalog relations are sampled by one statement, even when the caller
/// has already written under READ COMMITTED. JSON is only the internal row
/// transport; the checked decoder below still constructs the same RawCatalog.
/// The role every statement of this connection runs as, and so the only
/// grantor whose entries a `REVOKE` this tool emits can remove.
/// `current_user` rather than `session_user`: a `SET ROLE` moves both the
/// grantor a `GRANT` records and the one a `REVOKE` can match.
const SESSION: &str = "SELECT current_user::text AS role";

fn batch_query() -> String {
    // Aggregation must preserve semantic position order: sorting the JSON
    // itself would reorder columns and routine arguments (pinned live).
    [
        ("partitioned", partitioned_query(), "schema_name, table_name"),
        ("tables", tables_query(), "schema_name, table_name"),
        ("columns", columns_query(), "table_oid, attnum"),
        ("constraints", constraints_query(), "table_oid, name"),
        ("indexes", indexes_query(), "table_oid, name"),
        ("modules", modules_query(), "schema_name, name, oid"),
        ("module_args", module_args_query(), "oid, pos"),
        ("roles", ROLES.to_owned(), "name"),
        ("grants", grants_query(), "schema_name, object_name, column_name, grantee, privilege_type"),
        ("empty_routine_acls", empty_routine_acls_query(), "schema_name, name, oid"),
        ("routine_args", grant_routine_args_query(), "oid, pos"),
        ("other_acls", OTHER_ACLS.to_owned(), "class, name, grantee, privilege_type"),
        ("held_elsewhere", HELD_ELSEWHERE.to_owned(), "role_name, in_database, deptype"),
        ("default_acls", DEFAULT_ACLS.to_owned(), "grantor, in_schema, objtype"),
        ("unheld_modules", unheld_modules_query(), "schema_name, name"),
        ("session", SESSION.to_owned(), "role"),
        ("owners", owners_query(), "schema_name, object_name, routine_oid, kind"),
    ]
    .into_iter()
    .map(|(part, query, order)| format!(
        "SELECT '{part}'::text AS part, COALESCE(jsonb_agg(to_jsonb(r) ORDER BY {order}), '[]'::jsonb)::text AS payload FROM ({query}) r"
    ))
    .collect::<Vec<_>>()
    .join(" UNION ALL ")
}

async fn read_batch(conn: &mut Conn) -> Result<CatalogBatch, DbError> {
    let mut batch = CatalogBatch::new();
    for row in conn.query(&batch_query()).await? {
        let part = text(&row, "part")?;
        let payload = text(&row, "payload")?;
        let rows = serde_json::from_str(&payload)
            .map_err(|e| DbError::BadRow(format!("invalid catalog batch {part}: {e}")))?;
        if batch.insert(part.clone(), rows).is_some() {
            return Err(DbError::BadRow(format!("duplicate catalog batch {part}")));
        }
    }
    Ok(batch)
}

async fn read_all(conn: &mut Conn) -> Result<CatalogRead, DbError> {
    conn.query(CANONICAL_PATH).await?;
    decode_batch(&read_batch(conn).await?)
}

fn decode_batch(batch: &CatalogBatch) -> Result<CatalogRead, DbError> {
    let mut unmanaged_modules = Vec::new();
    let mut warnings = Vec::new();
    for row in batch
        .get("partitioned")
        .ok_or_else(|| missing("partitioned"))?
    {
        let schema = text(row, "schema_name")?;
        let name = text(row, "table_name")?;
        // An `r` reaches here for one of three reasons, and the message has to
        // say which: none of them is visible in `relkind`.
        let kind = match text(row, "kind")?.as_str() {
            // A tree is held whole or not at all (#1170, DEC-1170.1).
            "p" => {
                "a partitioned table other than one pbps holds: RANGE over plain columns, with \
                 no row security, rules, triggers, storage parameters or replica identity, and \
                 partitions that are its own and nothing else"
            }
            "r" if flag(row, "partition")? => {
                "a partition of a partitioned table pbps does not hold whole, or one that has \
                 columns, constraints, indexes, grants or settings of its own"
            }
            "f" => "a foreign table",
            "r" if flag(row, "row_security")? => {
                "a table with row-level security enabled, whose policies this model does not hold"
            }
            // `FORCE` makes the policies apply to the table's owner too, and
            // it is a separate flag: measured, a table can carry it with row
            // level security not enabled at all.
            "r" if flag(row, "force_row_security")? => {
                "a table with `FORCE ROW LEVEL SECURITY`, which decides whether its own owner is \
                 subject to the policies"
            }
            // Policies with the switch off. They do nothing today, and that is
            // the trap: a rebuild drops them, and whoever turns row-level
            // security on afterwards gets a table with no policies — open,
            // where this one was about to be closed.
            "r" if number(row, "policies")? > 0 => {
                "a table with row-level security policies that are not in force, which this model \
                 does not hold. They do nothing while the switch is off, so a rebuild would drop \
                 them silently and enabling row-level security afterwards would leave the table \
                 open"
            }
            "r" if text(row, "persistence")? == "t" => "a temporary table",
            // Every replica identity is declared (#1444) but one that names
            // an index which is gone: see `IDENTITY_NAMES_NO_INDEX`.
            "r" if flag(row, "identity_names_no_index")? => {
                "a table whose `REPLICA IDENTITY USING INDEX` names an index that was dropped, \
                 so it identifies no row; set it again, or to `DEFAULT`, and pull again"
            }
            // `CREATE TABLE ... OF t`: the row shape is the composite type's,
            // and `ALTER TYPE` changes the table. Read back as an ordinary
            // table it is an independent one that no longer follows anything.
            "r" if optional_text(row, "of_type")?.as_deref() != Some("-") => &format!(
                "a table of the composite type `{}`, whose shape follows it",
                optional_text(row, "of_type")?.unwrap_or_default()
            ),
            // A `DO INSTEAD` rule decides what an `INSERT` on this table
            // actually does — including nothing at all.
            "r" if flag(row, "has_rules")? => {
                "a table with rewrite rules, which decide what a write to it does"
            }
            // Not heap: how the rows are stored, and what the table can do
            // with them. The model has one kind of table.
            "r" if optional_text(row, "access_method")?.as_deref() != Some("heap") => {
                "a table on a table access method other than `heap`"
            }
            // Both ends of an inheritance, because both are unusable and for
            // different reasons. **Measured**: a `SELECT` from the parent
            // returns the children's rows too, and `ALTER TABLE parent ADD
            // COLUMN` gives the column to every child — so a plan that changes
            // a managed parent changes tables nobody declared.
            "r" if flag(row, "inherited_from")? => {
                "a table other tables inherit from, whose reads return their rows and whose \
                 changes recurse into them"
            }
            "r" => "a table that inherits from another",
            other => &format!("a relation of kind `{other}`"),
        };
        warnings.push(Limitation {
            target: LimitationTarget::Relation(TableName::new(&schema, &name)),
            detail: format!(
                "`{schema}.{name}` is {kind}, which this model does not hold. It is left out of \
                 the pull entirely -- not read back as an ordinary table, which would make a plan \
                 that recreates it without what makes it one."
            ),
        });
    }

    let mut raw = RawCatalog::default();
    for row in batch.get("tables").ok_or_else(|| missing("tables"))? {
        raw.tables.push(RawTable {
            oid: number(row, "oid")?,
            schema: text(row, "schema_name")?,
            name: text(row, "table_name")?,
            replica_identity: first_char(&text(row, "replica_identity")?).unwrap_or(' '),
            identity_index: {
                let oid = number(row, "identity_index")?;
                (oid != 0).then_some(oid)
            },
            reloptions: strings(row, "reloptions")?,
            toast_reloptions: strings(row, "toast_reloptions")?,
            unlogged: text(row, "persistence")? == "u",
            // Present on every row, `null` on all but a partitioned parent:
            // a row without the field is a malformed read, not an ordinary
            // table.
            partition_key: match row.get("partition_key") {
                None => return Err(missing("partition_key")),
                Some(serde_json::Value::Null) => None,
                Some(_) => Some(strings(row, "partition_key")?),
            },
            partition_of: match optional_text(row, "partition_bound")? {
                Some(bound) => Some((number(row, "partition_parent")?, bound)),
                None => None,
            },
        });
    }
    for row in batch.get("columns").ok_or_else(|| missing("columns"))? {
        let identity = match first_char(&text(row, "identity_kind")?) {
            Some(kind @ ('a' | 'd')) => Some(RawIdentity {
                always: kind == 'a',
                // A column the catalog calls an identity always has a sequence
                // behind it. Defaulting rather than failing here would invent
                // `GENERATED ... (START WITH 0)`, which is not a thing this
                // engine will accept back (see `types::identity_seed_range`).
                seed: number(row, "seq_start")?,
                increment: number(row, "seq_increment")?,
                min: number(row, "seq_min")?,
                max: number(row, "seq_max")?,
                cycles: flag(row, "seq_cycle")?,
                cache: number(row, "seq_cache")?,
            }),
            _ => None,
        };
        // The `serial` case: a sequence the column defaults from and does not
        // contain. The identity's own sequence is the other join and never
        // reaches here.
        let owned_sequence = optional_text(row, "sequence_name")?;
        raw.columns.push(RawColumn {
            table_oid: number(row, "table_oid")?,
            attnum: small(row, "attnum")?,
            name: text(row, "name")?,
            ty: text(row, "ty")?,
            nullable: flag(row, "nullable")?,
            default: optional_text(row, "default_expr")?,
            identity,
            generated: first_char(&text(row, "generated")?),
            owned_sequence,
            default_sequences: optional_text(row, "default_sequences")?,
            collation: optional_text(row, "collation")?,
        });
    }
    for row in batch
        .get("constraints")
        .ok_or_else(|| missing("constraints"))?
    {
        let ref_table = number(row, "ref_table")?;
        let kind = first_char(&text(row, "kind")?).unwrap_or('?');
        let schema = text(row, "schema_name")?;
        let table = text(row, "table_name")?;
        let name = text(row, "name")?;
        // Not `text`: a `NULL` here is not a column this reader has lost track
        // of, and saying so sends the reader to the one place that is right —
        // the same reasoning as the module branch above, for the same reason
        // (`pg_get_constraintdef` resolves through the syscache, not the
        // snapshot). The constraint's own name has no schema of its own, so
        // the vanished object is named `schema.table.constraint`.
        let Some(definition) = optional_text(row, "definition")? else {
            return Err(deparsed_away(kind, &schema, &format!("{table}.{name}")));
        };
        raw.constraints.push(RawConstraint {
            table_oid: number(row, "table_oid")?,
            name,
            kind,
            columns: numbers(&optional_text(row, "conkey")?.unwrap_or_default()),
            ref_columns: numbers(&optional_text(row, "confkey")?.unwrap_or_default()),
            // 0 is `pg_constraint`'s "no referenced table", not an oid.
            ref_table: (ref_table != 0).then_some(ref_table),
            on_delete: first_char(&text(row, "on_delete")?).unwrap_or(' '),
            on_update: first_char(&text(row, "on_update")?).unwrap_or(' '),
            validated: flag(row, "validated")?,
            deferrable: flag(row, "deferrable")?,
            deferred: flag(row, "deferred")?,
            definition,
            expression: optional_text(row, "expression")?,
            match_type: first_char(&text(row, "match_type")?).unwrap_or(' '),
            delete_set_columns: numbers(
                &optional_text(row, "delete_set_columns")?.unwrap_or_default(),
            ),
            index_oid: {
                let oid = number(row, "index_oid")?;
                (oid != 0).then_some(oid)
            },
            enforced: flag(row, "enforced")?,
            period: flag(row, "period")?,
            no_inherit: flag(row, "no_inherit")?,
            triggers_not_ordinary: flag(row, "triggers_not_ordinary")?,
            inherited: flag(row, "inherited")?,
        });
    }
    for row in batch.get("indexes").ok_or_else(|| missing("indexes"))? {
        raw.indexes.push(RawIndex {
            oid: number(row, "oid")?,
            table_oid: number(row, "table_oid")?,
            name: text(row, "name")?,
            unique: flag(row, "is_unique")?,
            primary: flag(row, "is_primary")?,
            exclusion: flag(row, "is_exclusion")?,
            valid: flag(row, "is_valid")?,
            nulls_not_distinct: flag(row, "nulls_not_distinct")?,
            key_count: small(row, "key_count")?.max(0) as usize,
            columns: numbers(&text(row, "keys")?),
            options: numbers(&text(row, "options")?),
            filter: optional_text(row, "filter")?,
            has_expressions: flag(row, "has_expressions")?,
            method: text(row, "method")?,
            key_classes: strings(row, "key_classes")?,
            key_texts: strings(row, "key_texts")?,
            nondefault_collation: flag(row, "nondefault_collation")?,
            attached: flag(row, "attached")?,
            reloptions: strings(row, "reloptions")?,
        });
    }
    for row in batch.get("modules").ok_or_else(|| missing("modules"))? {
        let kind = first_char(&text(row, "kind")?).unwrap_or('?');
        let schema = text(row, "schema_name")?;
        let name = text(row, "name")?;
        // Not `text`: a `NULL` here is not a column this reader has lost track
        // of, and saying so sends the reader to the one place that is right.
        let Some(definition) = optional_text(row, "definition")? else {
            return Err(deparsed_away(kind, &schema, &name));
        };
        raw.modules.push(RawModule {
            oid: number(row, "oid")?,
            kind,
            schema,
            name,
            on_table: text(row, "on_table")?,
            definition,
        });
    }
    for row in batch
        .get("module_args")
        .ok_or_else(|| missing("module_args"))?
    {
        raw.module_args.push(RawModuleArg {
            routine_oid: number(row, "oid")?,
            position: number(row, "pos")?,
            ty: text(row, "ty")?,
        });
    }
    for row in batch.get("roles").ok_or_else(|| missing("roles"))? {
        raw.roles.push(RawRole {
            name: text(row, "name")?,
            superuser: flag(row, "superuser")?,
        });
    }
    // Exactly one row, always: `current_user` is never null and never plural.
    // Absent means the batch itself did not run, which is a different finding
    // from "this connection has no role" and must not read as one.
    raw.session_role = text(
        batch
            .get("session")
            .ok_or_else(|| missing("session"))?
            .first()
            .ok_or_else(|| DbError::BadRow("catalog batch part session is empty".to_owned()))?,
        "role",
    )?;
    for row in batch.get("owners").ok_or_else(|| missing("owners"))? {
        raw.owners.push(RawOwner {
            schema: text(row, "schema_name")?,
            object: optional_text(row, "object_name")?,
            kind: granted_kind(&text(row, "source")?, &text(row, "kind")?),
            routine_oid: row.integer("routine_oid")?,
            owner: text(row, "owner")?,
        });
    }
    for row in batch.get("grants").ok_or_else(|| missing("grants"))? {
        raw.grants.push(RawGrant {
            // `None` is PUBLIC, which the query spells as a NULL rather than
            // letting `pg_get_userbyid(0)` render its own `unknown (OID=0)`:
            // that string is a name a role could in principle have, and this
            // one distinction must not be a string comparison.
            grantee: optional_text(row, "grantee")?,
            schema: text(row, "schema_name")?,
            object: optional_text(row, "object_name")?,
            kind: granted_kind(&text(row, "source")?, &text(row, "kind")?),
            routine_oid: row.integer("routine_oid")?,
            permission: text(row, "privilege_type")?,
            grantable: flag(row, "is_grantable")?,
            column: optional_text(row, "column_name")?,
            defaulted: flag(row, "defaulted")?,
            owner: text(row, "owner")?,
            grantor: text(row, "grantor")?,
            revocable: flag(row, "revocable")?,
        });
    }
    for row in batch
        .get("empty_routine_acls")
        .ok_or_else(|| missing("empty_routine_acls"))?
    {
        raw.empty_routine_acls.push(RawEmptyRoutineAcl {
            schema: text(row, "schema_name")?,
            name: text(row, "name")?,
            routine_oid: number(row, "oid")?,
        });
    }
    for row in batch
        .get("routine_args")
        .ok_or_else(|| missing("routine_args"))?
    {
        raw.routine_args.push(RawModuleArg {
            routine_oid: number(row, "oid")?,
            position: number(row, "pos")?,
            ty: text(row, "ty")?,
        });
    }
    for row in batch
        .get("other_acls")
        .ok_or_else(|| missing("other_acls"))?
    {
        raw.other_grants.push(RawOtherGrant {
            grantee: optional_text(row, "grantee")?,
            class: text(row, "class")?,
            name: text(row, "name")?,
            permission: text(row, "privilege_type")?,
            grantable: flag(row, "is_grantable")?,
            owner: optional_text(row, "owner")?,
            defaulted: flag(row, "defaulted")?,
        });
    }
    for row in batch
        .get("held_elsewhere")
        .ok_or_else(|| missing("held_elsewhere"))?
    {
        raw.held_elsewhere.push(RawSharedDependency {
            role: text(row, "role_name")?,
            database: optional_text(row, "in_database")?,
            deptype: first_char(&text(row, "deptype")?).unwrap_or('?'),
            objects: number(row, "objects")?,
        });
    }
    for row in batch
        .get("default_acls")
        .ok_or_else(|| missing("default_acls"))?
    {
        raw.default_acls.push(RawDefaultAcl {
            grantor: text(row, "grantor")?,
            in_schema: optional_text(row, "in_schema")?,
            objtype: first_char(&text(row, "objtype")?).unwrap_or('?'),
            acl: text(row, "acl")?,
        });
    }
    for row in batch
        .get("unheld_modules")
        .ok_or_else(|| missing("unheld_modules"))?
    {
        let schema = text(row, "schema_name")?;
        let name = text(row, "name")?;
        let here = TableName::new(&schema, &name);
        let target = match text(row, "kind")?.as_str() {
            "v" | "m" => LimitationTarget::Relation(here),
            "a" | "w" => {
                let oid = number(row, "oid")?;
                let args = raw
                    .routine_args
                    .iter()
                    .filter(|arg| arg.routine_oid == oid)
                    .map(|arg| arg.ty.parse::<pbps_model::RoutineArg>())
                    .collect::<Result<Vec<_>, _>>();
                match args {
                    Ok(args) => LimitationTarget::Module(pbps_model::ModuleId::Routine(
                        pbps_model::RoutineId::new(here, args),
                    )),
                    Err(_) => LimitationTarget::UnnameableModule(here),
                }
            }
            "t" => LimitationTarget::Module(pbps_model::ModuleId::Trigger {
                on: TableName::new(&schema, text(row, "on_table")?),
                name,
            }),
            other => {
                return Err(DbError::BadRow(format!(
                    "unknown unsupported module kind {other}"
                )));
            }
        };
        let kind = match text(row, "kind")?.as_str() {
            "v" => "view",
            "m" => "materialized view",
            "a" => "aggregate",
            "w" => "window function",
            "t" => "trigger",
            _ => unreachable!("unknown kinds were refused above"),
        };
        let detail = format!(
            "`{target}` is {}, which this model does not hold. It is left out of the \
             pull entirely -- not read back as an ordinary module, which would make a plan \
             that recreates it as something else.",
            text(row, "detail")?
        );
        unmanaged_modules.push(pbps_db::catalog::UnmanagedModule {
            kind,
            target: target.clone(),
            why: detail.clone(),
        });
        warnings.push(Limitation { target, detail });
    }
    Ok((raw, warnings, unmanaged_modules))
}

/// The principals that could hold a grant in this database (ADR-0005).
///
/// The cluster's reserved namespace is left out, and by the same
/// `left(rolname, 3)` predicate [`NOT_A_PROJECTS_SCHEMA`] uses rather than a
/// `LIKE 'pg\_%'` whose escape depends on a setting: measured, PostgreSQL 18.6
/// has sixteen `pg_*` roles, `CREATE ROLE pg_thing` is `role name "pg_thing"
/// is reserved`, and `validate_role` refuses a declaration that names one. What
/// they are granted is the cluster's business rather than this database's.
///
/// `pg_roles` rather than `pg_authid`: the second holds the password hashes and
/// is superuser-only, and the pull runs as the least-privileged account it can.
const ROLES: &str = "\
SELECT r.rolname AS name, r.rolsuper AS superuser
  FROM pg_catalog.pg_roles r
 WHERE pg_catalog.left(r.rolname, 3) <> 'pg_'
 ORDER BY r.rolname";

/// Every grant in this database, one row per `(grantee, permission)` pair.
///
/// # `coalesce(acl, acldefault(...))`, and why the engine expands it
///
/// A NULL ACL is not an empty one: it means the built-in default for the
/// object's kind, and the engine will hand it out. Measured on 18.6, a fresh
/// function has `proacl IS NULL` and `acldefault('f', owner)` is
/// `{=X/owner,owner=X/owner}` — PUBLIC executes it. Read as empty, `pull`
/// would write a role holding nothing where it holds the owner's whole set,
/// and would describe an open function as closed.
///
/// `acldefault` is asked rather than answered here because the answer moves
/// with the release: `MAINTAIN` joined the relation default in PostgreSQL 17,
/// measured `{owner=arwdDxt/owner}` on 16.15 against `{owner=arwdDxtm/owner}`
/// on 18.6. A table of defaults written into this crate would have been wrong
/// on one of those two servers.
///
/// `aclexplode` is asked for the same reason: an `aclitem`'s letters are the
/// engine's own encoding, positional and extended by releases, and a letter
/// this code did not know would read as no permission at all.
///
/// # The routine arm carries an oid, not a signature
///
/// The signature is assembled from [`grant_routine_args_query`]'s rows, and
/// that is the whole point: a rendered signature has to be split to be used
/// again, and a comma is not a separator. **Measured**, a type named
/// `amount,type` renders as `cm."amount,type"`, so the comma that separates
/// arguments and the comma inside one are the same character. Split, every
/// fragment failed to parse and a valid grant on a managed routine became
/// unexpressible — which refuses the connected plan.
///
/// The arguments themselves are `unnest(proargtypes)` with `format_type`, and
/// **not** `pg_get_function_identity_arguments`: measured, that renders a
/// procedure's argument as `IN integer`, mode and all, while
/// [`module_args_query`] renders the same routine's identity as `integer`. A
/// grant target has to be the identity the module pull produced or nothing
/// matches it — the managed-set filter compares `GrantTarget::Routine` against
/// `ModuleId::Routine`.
///
/// The four arms are the four catalogs that carry an ACL a declaration could
/// name. Column grants come too (`pg_attribute.attacl`), because the object's
/// own ACL does not show them — measured, after `GRANT SELECT (a) ON m9.v`,
/// `relacl` is NULL — and a reader that only asked the object would report a
/// role as holding nothing on a table it can read a column of.
fn grants_query() -> String {
    let not_one_of_our_tables = not_one_of_our_tables();
    let relation_revocable = revocable_by_current_role(
        "c.relowner",
        "COALESCE(c.relacl, pg_catalog.acldefault(\
         (CASE c.relkind WHEN 'S' THEN 's' ELSE 'r' END)::\"char\", c.relowner))",
    );
    let routine_revocable = revocable_by_current_role(
        "p.proowner",
        "COALESCE(p.proacl, pg_catalog.acldefault('f'::\"char\", p.proowner))",
    );
    let schema_revocable = revocable_by_current_role(
        "n.nspowner",
        "COALESCE(n.nspacl, pg_catalog.acldefault('n'::\"char\", n.nspowner))",
    );
    format!(
        "WITH RECURSIVE {MEMBERSHIP_DEPTHS}
         SELECT n.nspname AS schema_name, c.relname AS object_name,
                'rel' AS source, c.relkind::text AS kind,
                NULL::int8 AS routine_oid, NULL::text AS column_name,
                CASE WHEN a.grantee = 0 THEN NULL
                     ELSE pg_catalog.pg_get_userbyid(a.grantee) END AS grantee,
                a.privilege_type, a.is_grantable,
                c.relacl IS NULL AS defaulted,
                pg_catalog.pg_get_userbyid(c.relowner) AS owner,
                pg_catalog.pg_get_userbyid(a.grantor) AS grantor,
                {relation_revocable} AS revocable
           FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
           CROSS JOIN LATERAL pg_catalog.aclexplode(
                COALESCE(c.relacl, pg_catalog.acldefault(
                    (CASE c.relkind WHEN 'S' THEN 's' ELSE 'r' END)::\"char\", c.relowner))) AS a
           CROSS JOIN pg_catalog.pg_roles me
          WHERE me.rolname = current_user
            AND {NOT_AN_INDEX_OR_TOAST}
            AND {NOT_A_PROJECTS_SCHEMA}
            AND {not_one_of_our_tables}
         UNION ALL
         SELECT n.nspname, c.relname, 'rel', c.relkind::text,
                NULL::int8, at.attname,
                CASE WHEN a.grantee = 0 THEN NULL
                     ELSE pg_catalog.pg_get_userbyid(a.grantee) END,
                a.privilege_type, a.is_grantable,
                false, pg_catalog.pg_get_userbyid(c.relowner),
                pg_catalog.pg_get_userbyid(a.grantor),
                -- A column-level grant is outside the model whatever its
                -- grantor, and is reported before this is read (DECISIONS 97).
                false
           FROM pg_catalog.pg_attribute at
           JOIN pg_catalog.pg_class c ON c.oid = at.attrelid
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
           CROSS JOIN LATERAL pg_catalog.aclexplode(at.attacl) AS a
          WHERE at.attacl IS NOT NULL
            AND {NOT_A_PROJECTS_SCHEMA}
            AND {not_one_of_our_tables}
         UNION ALL
         SELECT n.nspname, p.proname, 'pro', p.prokind::text,
                p.oid::int8, NULL::text,
                CASE WHEN a.grantee = 0 THEN NULL
                     ELSE pg_catalog.pg_get_userbyid(a.grantee) END,
                a.privilege_type, a.is_grantable,
                p.proacl IS NULL, pg_catalog.pg_get_userbyid(p.proowner),
                pg_catalog.pg_get_userbyid(a.grantor),
                {routine_revocable}
           FROM pg_catalog.pg_proc p
           JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
           CROSS JOIN LATERAL pg_catalog.aclexplode(
                COALESCE(p.proacl, pg_catalog.acldefault('f'::\"char\", p.proowner))) AS a
           CROSS JOIN pg_catalog.pg_roles me
          WHERE me.rolname = current_user AND {NOT_A_PROJECTS_SCHEMA}
         UNION ALL
         SELECT n.nspname, NULL::text, 'nsp', '',
                NULL::int8, NULL::text,
                CASE WHEN a.grantee = 0 THEN NULL
                     ELSE pg_catalog.pg_get_userbyid(a.grantee) END,
                a.privilege_type, a.is_grantable,
                n.nspacl IS NULL, pg_catalog.pg_get_userbyid(n.nspowner),
                pg_catalog.pg_get_userbyid(a.grantor),
                {schema_revocable}
           FROM pg_catalog.pg_namespace n
           CROSS JOIN LATERAL pg_catalog.aclexplode(
                COALESCE(n.nspacl, pg_catalog.acldefault('n'::\"char\", n.nspowner))) AS a
           CROSS JOIN pg_catalog.pg_roles me
          WHERE me.rolname = current_user AND {NOT_A_PROJECTS_SCHEMA}
         ORDER BY 1, 2, 6, 7, 8"
    )
}

/// Empty ACLs have no rows for `aclexplode`, so retain their routine identity
/// separately in the same statement snapshot (#250). `cardinality`, not array
/// equality: measured, REVOKE creates an empty ACL with dimensions `[1:0]`,
/// which compares unequal to `'{}'::aclitem[]` despite having no entries.
fn empty_routine_acls_query() -> String {
    format!(
        "SELECT n.nspname AS schema_name, p.proname AS name, p.oid::int8 AS oid
           FROM pg_catalog.pg_proc p
           JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
          WHERE p.prokind IN ('f', 'p')
            AND pg_catalog.cardinality(p.proacl) = 0
            AND {NOT_A_PROJECTS_SCHEMA}
          ORDER BY 1, 2, 3"
    )
}

/// The `pg_class` rows that can carry a grant at all.
///
/// An index and a TOAST table cannot: they have no `GRANT` of their own, and
/// the permission that reaches them is the one on the table they belong to.
/// Everything else is here — including the three this model does not declare —
/// because a grant on one of them is a real grant a role really holds, and
/// **measured**, `GRANT SELECT ON mk.mv` (a materialized view) and
/// `GRANT SELECT ON mk.parent` (a partitioned table) both land in `relacl`. A
/// filter that named only the declarable kinds reported the role as holding
/// nothing there, which is *absent* reading as *empty*.
/// Who owns each securable a grant can name — read from the object catalogs,
/// not from the ACLs.
///
/// An ACL row is the wrong source for this. `aclexplode` returns nothing for
/// an ACL that is explicitly empty, and an ACL that holds only other
/// principals' entries has no owner row in it either. **Measured** on 18.6:
/// `REVOKE ALL ON s.t FROM a2_owner` leaves `relacl = '{}'`, and the same
/// revoke beside one grant leaves `{a2_reader=r/a2_owner}`. Either way the
/// object is owned, and a reader that learned owners from ACL rows alone
/// would report those two as owned by nobody — absent reading as empty, and
/// `owned_targets` accepting a declaration that cannot converge (#261).
///
/// Three arms rather than four: a column-level grant is on its table, and
/// takes that table's owner.
fn owners_query() -> String {
    let not_one_of_our_tables = not_one_of_our_tables();
    format!(
        "SELECT n.nspname AS schema_name, c.relname AS object_name,
                'rel' AS source, c.relkind::text AS kind, NULL::int8 AS routine_oid,
                pg_catalog.pg_get_userbyid(c.relowner) AS owner
           FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE {NOT_AN_INDEX_OR_TOAST}
            AND {NOT_A_PROJECTS_SCHEMA}
            AND {not_one_of_our_tables}
         UNION ALL
         SELECT n.nspname, p.proname, 'pro', p.prokind::text, p.oid::int8,
                pg_catalog.pg_get_userbyid(p.proowner)
           FROM pg_catalog.pg_proc p
           JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
          WHERE {NOT_A_PROJECTS_SCHEMA}
         UNION ALL
         SELECT n.nspname, NULL::text, 'nsp', '', NULL::int8,
                pg_catalog.pg_get_userbyid(n.nspowner)
           FROM pg_catalog.pg_namespace n
          WHERE {NOT_A_PROJECTS_SCHEMA}
         ORDER BY 1, 2, 5"
    )
}

/// Whether a `REVOKE` from this connection would remove an ACL entry.
///
/// An effective grant option answers `GRANT`, not `REVOKE`: PostgreSQL only
/// changes the entries attributed to the grantor it selects. Owner and
/// superuser act as the owner; otherwise the current role wins when it holds a
/// **direct** option, and a **nearest unique inherited** option is
/// sufficient too — measured on 18.6, `ih_deploy` inheriting `ih_mid` and
/// holding no direct option of its own revoked `ih_reader=r/ih_mid`, while the
/// same revoke with a competing direct option in place removed nothing and
/// reported success.
///
/// Among inherited candidates the engine takes the **nearest**: it walks the
/// inheritable memberships breadth first from the current role (with the
/// database owner's implicit `pg_database_owner`) and selects the first role
/// holding the option (`select_best_grantor`). So a unique candidate at the
/// least depth wins. Measured on 18 and 16, a deployer inheriting the owner
/// directly and a grant-option holder through one more role revoked the
/// owner's entry (#565). Candidates sharing the least depth are still
/// ambiguous: the walk's order within one depth is the catalog's, and
/// PostgreSQL does not promise it (DECISIONS 483). A candidate whose depth the
/// walk does not find declines the selection rather than being left out,
/// since leaving out a nearer competitor would let a farther role look
/// nearest (DEC-565.1).
///
/// The depths come from [`MEMBERSHIP_DEPTHS`], which the caller's statement
/// declares once, rather than from a walk inside this expression (#1422).
///
/// One original grantor per grantee, target **and privilege** as well: a
/// `REVOKE` removes only the entries its selected grantor put there, so
/// measured on 18.6 `r1=ar/own1` beside `r1=r/dep1` keeps its `SELECT` when
/// `dep1` revokes. The test is per privilege and not per target, because two
/// *different* privileges of one grantee may come from two grantors and each
/// still be revocable alone — `REVOKE SELECT` by `dep1` took `r1=r/dep1` away
/// and left `r1=a/own1` standing (measured). What one statement cannot do is
/// combine two grantors' authority, and [`crate::emit`] keeps that
/// unrepresentable by revoking one privilege per statement (DECISIONS 518,
/// #251).
///
/// `owner` and `acl` are the catalog columns of the securable in the caller's
/// query; the caller supplies `me` as a `pg_roles` row for `current_user`,
/// and declares [`MEMBERSHIP_DEPTHS`] in its `WITH RECURSIVE`.
pub(crate) fn revocable_by_current_role(owner: &str, acl: &str) -> String {
    // The inherited candidates, each with its least depth from the statement's
    // `member_depth` (NULL where the walk found none).
    let candidates = format!(
        "SELECT k.oid, d.depth, min(d.depth) OVER () AS nearest FROM (
             SELECT {owner} AS oid WHERE pg_catalog.pg_has_role(me.oid, {owner}, 'USAGE')
             UNION
             SELECT opt.grantee FROM pg_catalog.aclexplode({acl}) opt
              WHERE opt.is_grantable AND opt.privilege_type = a.privilege_type
                AND opt.grantee <> 0
                AND pg_catalog.pg_has_role(me.oid, opt.grantee, 'USAGE')
           ) k LEFT JOIN member_depth d ON d.oid = k.oid"
    );
    format!(
        "COALESCE(a.grantor = CASE
             WHEN me.rolsuper OR me.oid = {owner} THEN {owner}
             WHEN EXISTS (SELECT FROM pg_catalog.aclexplode({acl}) own
                 WHERE own.grantee = me.oid AND own.is_grantable
                   AND own.privilege_type = a.privilege_type) THEN me.oid
             ELSE (SELECT CASE
                     WHEN count(*) = 1 THEN min(n.oid::bigint)::oid
                     WHEN NOT bool_or(n.depth IS NULL)
                      AND count(*) FILTER (WHERE n.depth = n.nearest) = 1
                       THEN (min(n.oid::bigint) FILTER (WHERE n.depth = n.nearest))::oid
                   END
                     FROM ({candidates}) n)
         END, false) AND (SELECT count(DISTINCT grantor)
             FROM pg_catalog.aclexplode({acl})
            WHERE grantee = a.grantee AND privilege_type = a.privilege_type) = 1"
    )
}

/// The `WITH RECURSIVE` members [`revocable_by_current_role`] reads: every
/// role the current role reaches through inheritable memberships (with the
/// database owner's implicit `pg_database_owner`), at its least depth.
///
/// Computed once per statement and joined per ACL row. Written inside each
/// row's expression instead, the planner multiplied the recursive walk's cost
/// by every ACL row, the estimate crossed `jit_above_cost`, and every catalog
/// read paid about 450 ms of JIT compilation on 18 (CI on #1422). One row
/// per role and depth keeps a graph of repeated diamonds to roles × depth
/// rows, and the bound is the number of roles, which no acyclic membership
/// path can exceed.
pub(crate) const MEMBERSHIP_DEPTHS: &str = "\
member_reach(oid, depth) AS (
    SELECT r.oid, 0 FROM pg_catalog.pg_roles r WHERE r.rolname = current_user
    UNION
    SELECT e.roleid, r.depth + 1 FROM member_reach r
      JOIN (SELECT m.member, m.roleid FROM pg_catalog.pg_auth_members m
             WHERE m.inherit_option
            UNION ALL
            SELECT d.datdba, 'pg_database_owner'::pg_catalog.regrole::oid
              FROM pg_catalog.pg_database d
             WHERE d.datname = pg_catalog.current_database()) e
        ON e.member = r.oid
     WHERE r.depth < (SELECT count(*) FROM pg_catalog.pg_roles)),
member_depth AS MATERIALIZED (
    SELECT oid, min(depth) AS depth FROM member_reach GROUP BY oid)";

const NOT_AN_INDEX_OR_TOAST: &str = "c.relkind NOT IN ('i', 'I', 't')";

/// The argument types of every routine a grant can name, one row per
/// argument, in order.
///
/// [`module_args_query`]'s wider twin. That one is filtered to the routines
/// this model declares; a grant may be on any routine at all — an aggregate,
/// an extension's function — and the reader has to be able to say what it is
/// on before it can say the model cannot hold it.
///
/// `proargtypes` rather than `proallargtypes`, matching the module pull: it is
/// the `IN` and `INOUT` types, which is what the engine's own identity for a
/// routine is made of.
fn grant_routine_args_query() -> String {
    format!(
        "SELECT p.oid::int8 AS oid, u.pos::int8 AS pos,
                pg_catalog.format_type(u.ty, NULL) AS ty
           FROM pg_catalog.pg_proc p
           JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
      CROSS JOIN LATERAL pg_catalog.unnest(p.proargtypes)
                    WITH ORDINALITY AS u(ty, pos)
          WHERE {NOT_A_PROJECTS_SCHEMA}
          ORDER BY 1, 2"
    )
}

/// Every grant in a catalog whose target the model cannot name (see
/// [`RawOtherGrant`], which lists the fourteen `aclitem[]` columns and why
/// each is here or is not).
///
/// Reported, never compared. A role that gained `USAGE ON LANGUAGE c` out of
/// band has changed, and a reader that only looked at the catalogs it *can*
/// name would compare the rest and call it clean — the failure DECISIONS 105
/// records on the other engine.
///
/// `pg_default_acl` is read by [`DEFAULT_ACLS`] instead, because it is not a
/// grant on anything that exists. `pg_init_privs` is not read at all: it holds
/// what an extension's objects had at *install*, which is a record of the past
/// and not a live permission. `pg_tablespace` is not read either — a
/// tablespace is a cluster object, and what holds a role across the cluster is
/// [`HELD_ELSEWHERE`]'s question.
///
/// `pg_database` is filtered to **this** database: `CONNECT`, `TEMP` or
/// `CREATE` on the one being deployed to is a fact about this deployment,
/// while the same on another is that database's business.
///
/// Each arm carries its object's **owner**, for the same reason the ordinary
/// ACL read does (DECISIONS 371): these columns are NULL until somebody
/// touches them, and the moment one is touched the engine writes the owner's
/// own inherent entry beside the change — measured on 18.6, `REVOKE USAGE ON
/// TYPE ot.money_kind FROM PUBLIC` turns a NULL `typacl` into
/// `{ot_owner=U/ot_owner}`. Read as a grant, that entry says a managed role
/// holds something unnameable and refuses every plan connected to it, where
/// nothing was granted at all.
///
/// Each privilege also carries whether the engine's `acldefault` contains it.
/// An explicit PUBLIC entry can be either materialized default access or a
/// new exposure; ACL nullness alone cannot distinguish the two (#258).
const OTHER_ACLS: &str = "\
SELECT 'a type' AS class, pg_catalog.format_type(t.oid, NULL) AS name,
       CASE WHEN a.grantee = 0 THEN NULL
            ELSE pg_catalog.pg_get_userbyid(a.grantee) END AS grantee,
       a.privilege_type, a.is_grantable,
       pg_catalog.pg_get_userbyid(t.typowner) AS owner,
       EXISTS (SELECT 1 FROM pg_catalog.aclexplode(
                   pg_catalog.acldefault('T'::\"char\", t.typowner)) AS dflt
                WHERE dflt.grantee = a.grantee
                  AND dflt.privilege_type = a.privilege_type
                  AND dflt.is_grantable = a.is_grantable) AS defaulted
  FROM pg_catalog.pg_type t
  CROSS JOIN LATERAL pg_catalog.aclexplode(t.typacl) AS a
 UNION ALL
SELECT 'a procedural language', l.lanname,
       CASE WHEN a.grantee = 0 THEN NULL
            ELSE pg_catalog.pg_get_userbyid(a.grantee) END,
       a.privilege_type, a.is_grantable,
       pg_catalog.pg_get_userbyid(l.lanowner),
       EXISTS (SELECT 1 FROM pg_catalog.aclexplode(
                   pg_catalog.acldefault('l'::\"char\", l.lanowner)) AS dflt
                WHERE dflt.grantee = a.grantee
                  AND dflt.privilege_type = a.privilege_type
                  AND dflt.is_grantable = a.is_grantable)
  FROM pg_catalog.pg_language l
  CROSS JOIN LATERAL pg_catalog.aclexplode(l.lanacl) AS a
 UNION ALL
SELECT 'a foreign data wrapper', w.fdwname,
       CASE WHEN a.grantee = 0 THEN NULL
            ELSE pg_catalog.pg_get_userbyid(a.grantee) END,
       a.privilege_type, a.is_grantable,
       pg_catalog.pg_get_userbyid(w.fdwowner),
       EXISTS (SELECT 1 FROM pg_catalog.aclexplode(
                   pg_catalog.acldefault('F'::\"char\", w.fdwowner)) AS dflt
                WHERE dflt.grantee = a.grantee
                  AND dflt.privilege_type = a.privilege_type
                  AND dflt.is_grantable = a.is_grantable)
  FROM pg_catalog.pg_foreign_data_wrapper w
  CROSS JOIN LATERAL pg_catalog.aclexplode(w.fdwacl) AS a
 UNION ALL
SELECT 'a foreign server', s.srvname,
       CASE WHEN a.grantee = 0 THEN NULL
            ELSE pg_catalog.pg_get_userbyid(a.grantee) END,
       a.privilege_type, a.is_grantable,
       pg_catalog.pg_get_userbyid(s.srvowner),
       EXISTS (SELECT 1 FROM pg_catalog.aclexplode(
                   pg_catalog.acldefault('S'::\"char\", s.srvowner)) AS dflt
                WHERE dflt.grantee = a.grantee
                  AND dflt.privilege_type = a.privilege_type
                  AND dflt.is_grantable = a.is_grantable)
  FROM pg_catalog.pg_foreign_server s
  CROSS JOIN LATERAL pg_catalog.aclexplode(s.srvacl) AS a
 UNION ALL
SELECT 'a configuration parameter', p.parname,
       CASE WHEN a.grantee = 0 THEN NULL
            ELSE pg_catalog.pg_get_userbyid(a.grantee) END,
       a.privilege_type, a.is_grantable,
       -- `pg_parameter_acl` has no owner column: a configuration parameter
       -- belongs to nobody, so no entry in it can be the zero point.
       NULL::name, false
  FROM pg_catalog.pg_parameter_acl p
  CROSS JOIN LATERAL pg_catalog.aclexplode(p.paracl) AS a
 UNION ALL
SELECT 'a large object', m.oid::text,
       CASE WHEN a.grantee = 0 THEN NULL
            ELSE pg_catalog.pg_get_userbyid(a.grantee) END,
       a.privilege_type, a.is_grantable,
       pg_catalog.pg_get_userbyid(m.lomowner),
       EXISTS (SELECT 1 FROM pg_catalog.aclexplode(
                   pg_catalog.acldefault('L'::\"char\", m.lomowner)) AS dflt
                WHERE dflt.grantee = a.grantee
                  AND dflt.privilege_type = a.privilege_type
                  AND dflt.is_grantable = a.is_grantable)
  FROM pg_catalog.pg_largeobject_metadata m
  CROSS JOIN LATERAL pg_catalog.aclexplode(m.lomacl) AS a
 UNION ALL
SELECT 'this database', d.datname,
       CASE WHEN a.grantee = 0 THEN NULL
            ELSE pg_catalog.pg_get_userbyid(a.grantee) END,
       a.privilege_type, a.is_grantable,
       pg_catalog.pg_get_userbyid(d.datdba),
       EXISTS (SELECT 1 FROM pg_catalog.aclexplode(
                   pg_catalog.acldefault('d'::\"char\", d.datdba)) AS dflt
                WHERE dflt.grantee = a.grantee
                  AND dflt.privilege_type = a.privilege_type
                  AND dflt.is_grantable = a.is_grantable)
  FROM pg_catalog.pg_database d
  CROSS JOIN LATERAL pg_catalog.aclexplode(d.datacl) AS a
 WHERE d.datname = pg_catalog.current_database()
 ORDER BY 1, 2, 3, 4";

/// What holds a role **outside this database** (ADR-0010 §4).
///
/// Its own query, and the only read here that is not about this database at
/// all. `pg_shdepend` is a shared catalog: it has a row per dependency on a
/// role, keyed by the database the dependent lives in — and measured, a role
/// with every grant *here* revoked is still refused a `DROP ROLE` with
/// `DETAIL: 1 object in database otherdb`. So a report built from this
/// database's own catalog would say "nothing is stopping it" and be wrong,
/// which is the member of *absent, empty and unreadable* that reads as good
/// news.
///
/// Rows for **this** database are excluded on purpose: what holds a role here
/// is in the grants the rest of this file already reads, and repeating it
/// would bury the one fact only this query has. The objects cannot be named
/// either way — their oids belong to another database's catalog — so what
/// comes back is the database and a count.
///
/// `pg_shdepend` is world-readable, so a least-privileged deployment account
/// gets the same answer a superuser does.
const HELD_ELSEWHERE: &str = "\
SELECT pg_catalog.pg_get_userbyid(sd.refobjid) AS role_name,
       d.datname AS in_database,
       sd.deptype::text AS deptype,
       count(*)::int8 AS objects
  FROM pg_catalog.pg_shdepend sd
  LEFT JOIN pg_catalog.pg_database d ON d.oid = sd.dbid
 WHERE sd.refclassid = 'pg_catalog.pg_authid'::regclass
   AND (d.datname IS NULL OR d.datname <> pg_catalog.current_database())
   AND pg_catalog.left(pg_catalog.pg_get_userbyid(sd.refobjid), 3) <> 'pg_'
 GROUP BY 1, 2, 3
 ORDER BY 1, 2, 3";

/// Every `ALTER DEFAULT PRIVILEGES` entry (ADR-0010 §2).
///
/// **Not filtered to the deploying account**, unlike the one
/// [`crate::modules`] asks before a rebuild: that one answers "what would the
/// replacement I am about to create arrive with", and this one answers "what
/// standing instructions does this database carry". An entry belonging to
/// another role is exactly the fact §2 is about — two declarations that read
/// identically mean different things, and the difference is who runs the plan.
const DEFAULT_ACLS: &str = "\
SELECT pg_catalog.pg_get_userbyid(da.defaclrole) AS grantor,
       n.nspname AS in_schema,
       da.defaclobjtype::text AS objtype,
       da.defaclacl::text AS acl
  FROM pg_catalog.pg_default_acl da
  LEFT JOIN pg_catalog.pg_namespace n ON n.oid = da.defaclnamespace
 ORDER BY 1, 2, 3";

/// Whether this connection is inside a transaction block, asked the one way
/// that reads differently in the two states through this driver (DECISIONS
/// 253): a `SET LOCAL` on this tool's own GUC, read back in a second
/// statement. The setting is left behind inside a caller's transaction, under
/// this tool's prefix, and ends with it.
pub async fn in_transaction(conn: &mut Conn) -> Result<bool, DbError> {
    let token = probe_token();
    conn.query(&probe_set(&token)).await?;
    let rows = conn.query(PROBE_READ).await?;
    let row = rows.first().ok_or_else(|| missing("probe"))?;
    Ok(text(row, "probe")? == token)
}

/// Where a canonical-scope read runs (DECISIONS 253, 418).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    /// Its own `REPEATABLE READ READ ONLY` transaction (250). Refused inside
    /// a caller's transaction, which it would otherwise commit (253).
    Own,
    /// Inside the transaction the caller already holds, under a savepoint,
    /// reading what that transaction has written and not yet committed. For
    /// one caller: the apply's read-back, which records what a plan built in
    /// the same transaction as the build (`pbps-cli`, DECISIONS 147). Refused
    /// outside a transaction: "nothing uncommitted to see" is [`Scope::Own`]'s
    /// answer, not this one's (418).
    CallersTransaction,
}

/// The savepoint an in-transaction read runs under, and what it sets there.
///
/// `SET LOCAL transaction_read_only = on` keeps the promise `READ ONLY` makes
/// for the other scope, for exactly as long as the savepoint: measured, a
/// `CREATE TABLE` under it is `cannot execute CREATE TABLE in a read-only
/// transaction`, and after `ROLLBACK TO SAVEPOINT` the caller's transaction is
/// writable again. The canonical settings are `set_config(…, is_local)` and
/// come back the same way — measured, the caller's `search_path` is what it
/// was — which is why the savepoint is rolled back on success too: there is
/// nothing a read has to keep, and everything it set has to go.
const SAVEPOINT: &str = "SAVEPOINT pbps_read; SET LOCAL transaction_read_only = on";
const UNWIND: &str = "ROLLBACK TO SAVEPOINT pbps_read; RELEASE SAVEPOINT pbps_read";

async fn open(conn: &mut Conn, scope: Scope) -> Result<(), DbError> {
    match scope {
        Scope::Own => {
            refuse_a_caller_owned_transaction(conn).await?;
            conn.execute(BEGIN).await
        }
        Scope::CallersTransaction => {
            if !in_transaction(conn).await? {
                return Err(DbError::Refused(
                    "this connection has no open transaction, and a read-back of one \
                     cannot run outside it.\nThe read exists to see what the \
                     caller's transaction has written and not yet committed; outside \
                     a transaction there is nothing of the kind, and the plain read \
                     takes its own snapshot instead."
                        .to_owned(),
                ));
            }
            conn.execute(SAVEPOINT).await
        }
    }
}

/// Ends the scope the way its outcome requires. The scope's own failure to end
/// is dropped on the error path: a transaction the server has already killed
/// must not replace the failure that killed it.
async fn close<T>(
    conn: &mut Conn,
    scope: Scope,
    outcome: Result<T, DbError>,
) -> Result<T, DbError> {
    match (scope, outcome) {
        (Scope::Own, Ok(value)) => {
            conn.execute("COMMIT").await?;
            Ok(value)
        }
        (Scope::Own, Err(e)) => {
            let _ = conn.execute("ROLLBACK").await;
            Err(e)
        }
        (Scope::CallersTransaction, Ok(value)) => {
            conn.execute(UNWIND).await?;
            Ok(value)
        }
        (Scope::CallersTransaction, Err(e)) => {
            let _ = conn.execute(UNWIND).await;
            Err(e)
        }
    }
}

/// A permission query that compares catalog-rendered routine types must use
/// the same spelling as introspection. Reuse its transaction/savepoint scope
/// so both successful and failed reads restore the caller's settings (#330).
pub(crate) async fn canonical_query(
    conn: &mut Conn,
    sql: &str,
    params: &[Param<'_>],
) -> Result<Vec<Row>, DbError> {
    let scope = if in_transaction(conn).await? {
        Scope::CallersTransaction
    } else {
        Scope::Own
    };
    open(conn, scope).await?;
    let outcome = async {
        conn.query(CANONICAL_PATH).await?;
        conn.query_with(sql, params).await
    }
    .await;
    close(conn, scope, outcome).await
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
/// it a schema; with it, the read fails. What is left is to say so — and
/// legibly: this seam's `From<tokio_postgres::Error>` used to render `XX000`
/// as the literal `db error`, an unreadable failure being the third thing
/// CLAUDE.md's rule names, until issue #167 read the server's own sentence
/// off `as_db_error()` instead of `Display`. [`schema_changed_underneath`]
/// below still folds that sentence into a sentence of its own: the server's
/// text says a cache lookup failed, and what a reader needs is why — that
/// this snapshot cannot see a concurrent `DROP` and is refusing rather than
/// guessing.
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
    if in_transaction(conn).await? {
        return Err(DbError::Refused(
            "this connection already has an open transaction, and a pull cannot run \
             inside one.\nThe read takes its own `REPEATABLE READ READ ONLY` \
             transaction to own its snapshot and restore its local settings. \
             PostgreSQL does not nest transactions, so running here would neither get \
             that snapshot nor be able to end without committing yours. Commit or roll \
             back first."
                .to_owned(),
        ));
    }
    Ok(())
}

fn schema_changed_underneath(e: DbError) -> DbError {
    match e.server_error_code().as_deref() {
        Some("XX000") => e.context(
            "the catalog changed while it was being read.\n\
                 Something applied DDL to this database during the pull. The read is taken in \
                 one snapshot so that it cannot report half of a change as a whole schema, and \
                 this is that guard firing. Run it again when the other change has finished.",
        ),
        // A deadlock, which — before issue #167 — reached an operator as
        // `db error` and nothing else, and now carries the server's own
        // "deadlock detected" sentence besides. It is the third way the
        // catalog moves under this read, and the only one where the engine
        // has already decided the outcome.
        //
        // The pull deparses every view in the database, and
        // `pg_get_viewdef` opens each one — so the read holds `ACCESS SHARE`
        // on relations a rebuild wants exclusively (ADR-0009 §3), and the two
        // orders can cross. Measured from the server log:
        //
        // ```text
        // deadlock detected
        // Process A: LOCK TABLE "app"."granted" IN ACCESS EXCLUSIVE MODE
        // Process B: SELECT … pg_get_viewdef(c.oid, true) …
        // ```
        //
        // Nothing is half-read and nothing is half-written: the engine chose a
        // victim and rolled it back whole. What the message has to say is that
        // it was a tie, not a fault, and that running again is the answer.
        Some("40P01") => e.context(
            "the catalog changed while it was being read (deadlock).\n\
                 Another session was changing this database while the pull was reading it, and \
                 the two needed the same objects in opposite orders. The engine broke the tie \
                 and rolled this read back whole. Run it again when the other change has \
                 finished.",
        ),
        _ => e,
    }
}

/// A deparse that came back `NULL`, which is the object having gone.
///
/// **Measured**, the deparsers answer `NULL` for an oid that is not there
/// rather than raising:
///
/// ```text
/// pg_get_viewdef(999999, true)  ->  NULL
/// pg_get_functiondef(999999)    ->  NULL
/// ```
///
/// The pull reads the catalog in one `REPEATABLE READ` snapshot so that it
/// cannot report half of a change as a whole schema — but a deparser resolves
/// its oid through the syscache against a *fresh* snapshot, so an object
/// dropped between the scan and the deparse comes back as a row with a name
/// and no definition. That is the catalog moving under the read, which is the
/// case [`schema_changed_underneath`] already exists for; it arrives here as a
/// `NULL` instead of as `XX000` because these two functions do not raise.
///
/// Reported as [`missing`] it read as "the query and this code have gone out
/// of step", which sends a reader to look for a renamed column — the one thing
/// that is not wrong here. **Absent, empty and unreadable are three different
/// things**, and a vanished object is the third.
fn deparsed_away(kind: char, schema: &str, name: &str) -> DbError {
    DbError::Refused(format!(
        "the catalog changed while it was being read: `{schema}.{name}` (kind `{kind}`) was \
         there when the catalog was scanned and gone when its definition was deparsed.\n\
         Something applied DDL to this database during the pull. The read is taken in one \
         snapshot so that it cannot report half of a change as a whole schema, and this is \
         that guard firing. Run it again when the other change has finished."
    ))
}

fn missing(column: &str) -> DbError {
    DbError::BadRow(format!(
        "the catalog query returned no `{column}`, which means the query and this code have gone \
         out of step"
    ))
}

trait CatalogFields {
    fn string(&self, column: &str) -> Result<Option<String>, DbError>;
    fn integer(&self, column: &str) -> Result<Option<i64>, DbError>;
    fn boolean(&self, column: &str) -> Result<Option<bool>, DbError>;
}

impl CatalogFields for Row {
    fn string(&self, column: &str) -> Result<Option<String>, DbError> {
        Ok(self.try_get::<&str>(column)?.map(str::to_owned))
    }
    fn integer(&self, column: &str) -> Result<Option<i64>, DbError> {
        self.try_get::<i64>(column)
    }
    fn boolean(&self, column: &str) -> Result<Option<bool>, DbError> {
        self.try_get::<bool>(column)
    }
}

fn json_field<T>(
    row: &serde_json::Value,
    column: &str,
    read: impl FnOnce(&serde_json::Value) -> Option<T>,
) -> Result<Option<T>, DbError> {
    match row.get(column) {
        None => Err(missing(column)),
        Some(serde_json::Value::Null) => Ok(None),
        Some(value) => read(value).map(Some).ok_or_else(|| {
            DbError::BadRow(format!("catalog column `{column}` has the wrong JSON type"))
        }),
    }
}

impl CatalogFields for serde_json::Value {
    fn string(&self, column: &str) -> Result<Option<String>, DbError> {
        json_field(self, column, |v| v.as_str().map(str::to_owned))
    }
    fn integer(&self, column: &str) -> Result<Option<i64>, DbError> {
        json_field(self, column, serde_json::Value::as_i64)
    }
    fn boolean(&self, column: &str) -> Result<Option<bool>, DbError> {
        json_field(self, column, serde_json::Value::as_bool)
    }
}

fn text(row: &impl CatalogFields, column: &str) -> Result<String, DbError> {
    optional_text(row, column)?.ok_or_else(|| missing(column))
}

fn optional_text(row: &impl CatalogFields, column: &str) -> Result<Option<String>, DbError> {
    row.string(column)
}

fn number(row: &impl CatalogFields, column: &str) -> Result<i64, DbError> {
    row.integer(column)?.ok_or_else(|| missing(column))
}

fn small(row: &impl CatalogFields, column: &str) -> Result<i32, DbError> {
    i32::try_from(number(row, column)?)
        .map_err(|_| DbError::BadRow(format!("catalog column `{column}` does not fit an i32")))
}

fn flag(row: &impl CatalogFields, column: &str) -> Result<bool, DbError> {
    row.boolean(column)?.ok_or_else(|| missing(column))
}

/// A JSON array of strings. The query coalesces an empty aggregate to `[]`, so
/// a `NULL` here is a column this reader has lost track of, not an empty list,
/// and is an error like any other shape it does not expect.
fn strings(row: &serde_json::Value, column: &str) -> Result<Vec<String>, DbError> {
    let bad = || DbError::BadRow(format!("catalog column `{column}` is not a list of text"));
    row.get(column)
        .ok_or_else(|| missing(column))?
        .as_array()
        .ok_or_else(bad)?
        .iter()
        .map(|v| v.as_str().map(str::to_owned).ok_or_else(bad))
        .collect()
}

/// The catalog a grant row came from, and that catalog's kind letter.
///
/// The two alphabets overlap — `relkind` `f` is a foreign table and `prokind`
/// `f` is a function — so the query says which it is, and this keeps the two
/// apart by construction rather than by a comment nobody re-reads.
fn granted_kind(source: &str, kind: &str) -> GrantedKind {
    match source {
        "rel" => GrantedKind::Relation(first_char(kind).unwrap_or('?')),
        "pro" => GrantedKind::Routine(first_char(kind).unwrap_or('?')),
        _ => GrantedKind::Schema,
    }
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

/// Reads the rows of every scoped table the schema has (ADR-0004).
///
/// The scope decides *which* rows — every row of an `exact` table, the declared
/// keys of an `ensure` one — and it is supplied by the caller, because a
/// database holds rows, not a notion of which of them are declared. A scoped
/// table the schema does not have gets no entry: it is missing, which the
/// managed-set check reports, and "missing" must not come back as "empty".
///
/// A table whose rows cannot be read (no single-column key, or a value the
/// model cannot hold) fails the whole read rather than being skipped: a state
/// recorded without it would say the table declares no rows, and the next drift
/// check would be blind to the rows it exists to watch.
///
/// **It takes its own transaction**, for the reason [`introspect`] does and one
/// more. `read_rows` is where this dialect's value spellings are fixed: the
/// canonical settings are `set_config(…, is_local)` and therefore belong to a
/// transaction, so a read outside one would render every date, interval,
/// `bytea` and float under whatever the caller's session has (ADR-0013 §3, and
/// [`crate::rows::read_expr`] for what that costs).
pub async fn read_rows(
    conn: &mut Conn,
    schema: &Schema,
    scopes: &std::collections::BTreeMap<TableName, pbps_model::RowScope>,
) -> Result<pbps_model::ObservedRows, crate::rows::RowsError> {
    read_rows_in(conn, schema, scopes, Scope::Own).await
}

/// [`read_rows`] inside the transaction the caller holds, for the reason and
/// under the terms of [`introspect_within_transaction`]: the rows a plan's
/// statements just wrote, read back before the commit that makes them so
/// (DECISIONS 147, 418).
pub async fn read_rows_within_transaction(
    conn: &mut Conn,
    schema: &Schema,
    scopes: &std::collections::BTreeMap<TableName, pbps_model::RowScope>,
) -> Result<pbps_model::ObservedRows, crate::rows::RowsError> {
    read_rows_in(conn, schema, scopes, Scope::CallersTransaction).await
}

async fn read_rows_in(
    conn: &mut Conn,
    schema: &Schema,
    scopes: &std::collections::BTreeMap<TableName, pbps_model::RowScope>,
    scope: Scope,
) -> Result<pbps_model::ObservedRows, crate::rows::RowsError> {
    let read = |table: &TableName, source: DbError| crate::rows::RowsError::Read {
        table: table.clone(),
        source: Box::new(source),
    };
    let any = TableName::new("", "");
    open(conn, scope).await.map_err(|e| read(&any, e))?;
    // The scope's own error is a read error of no table in particular; the
    // read's is already one.
    let outcome = read_every_scoped_table(conn, schema, scopes).await;
    match outcome {
        Ok(out) => close(conn, scope, Ok(out)).await.map_err(|e| read(&any, e)),
        Err(e) => {
            // The scope is unwound for the read's error, which is the one
            // returned; the unwinding's own outcome is not it.
            let _: Result<(), DbError> =
                close(conn, scope, Err(DbError::BadRow(String::new()))).await;
            Err(e)
        }
    }
}

async fn read_every_scoped_table(
    conn: &mut Conn,
    schema: &Schema,
    scopes: &std::collections::BTreeMap<TableName, pbps_model::RowScope>,
) -> Result<pbps_model::ObservedRows, crate::rows::RowsError> {
    let mut out = pbps_model::ObservedRows::new();
    let any = TableName::new("", "");
    conn.query(CANONICAL_PATH)
        .await
        .map_err(|e| crate::rows::RowsError::Read {
            table: any,
            source: Box::new(e),
        })?;
    for (name, scope) in scopes {
        let Some(table) = schema.tables.get(name) else {
            continue;
        };
        let mut observed = pbps_model::ObservedTable::default();
        if let Some(query) = crate::rows::query(name, table, scope)? {
            let read = |source| crate::rows::RowsError::Read {
                table: name.clone(),
                source: Box::new(source),
            };
            for row in &conn.query(&query.sql).await.map_err(read)? {
                let (key, cells) = crate::rows::decode(name, &query, row)?;
                observed.rows.insert(key, cells);
            }
            if let Some(sql) = &query.aliases {
                for row in &conn.query(sql).await.map_err(read)? {
                    // A requested key the table does not hold has no alias to
                    // record — the row is simply not there, which is what an
                    // `InsertRow` is for.
                    if let (requested, Some(canonical)) = crate::rows::decode_alias(name, row)? {
                        observed.aliases.insert(requested, canonical);
                    }
                }
            }
        }
        out.insert(name.clone(), observed);
    }
    Ok(out)
}

/// Every declared value the engine would not read back as written, and every
/// pair of declared keys it reads as one row (DECISIONS 101, 106; ADR-0013 §5).
///
/// Asked before anything is written, and asked of the engine: neither the model
/// nor this crate can spell a value the engine's way without becoming the
/// engine.
///
/// Inside the canonical scope, because the answer *is* a spelling and the
/// spelling is what the settings decide. The comparison it feeds is against a
/// state read under those same settings, and a check run under the operator's
/// would report a date as misspelt on one machine and clean on the next.
///
/// The key columns' collations are read here, by [`key_collations`], under
/// the names `at` carries: they decide which two keys are one row, and a
/// caller cannot know them without asking this catalog.
pub async fn misspelt(
    conn: &mut Conn,
    schema: &Schema,
    at: &crate::rows::CatalogNames,
) -> Result<Spellings, crate::rows::RowsError> {
    let any = TableName::new("", "");
    let read = |table: &TableName, source: DbError| crate::rows::RowsError::Read {
        table: table.clone(),
        source: Box::new(source),
    };
    refuse_a_caller_owned_transaction(conn)
        .await
        .map_err(|e| read(&any, e))?;
    // The one input the checks read from the catalog rather than take from
    // the declaration, read here so that the question is answered whole: a
    // caller that hands in the names and forgets the collations gets the
    // database default silently, which is the fallback DECISIONS 148 exists
    // to refuse. `at` is the caller's under the names it knows; the copy
    // carries what the catalog adds.
    let mut at = at.clone();
    key_collations(conn, &mut at, schema)
        .await
        .map_err(|e| read(&any, e))?;
    conn.execute(BEGIN).await.map_err(|e| read(&any, e))?;
    let out = ask_about_every_spelling(conn, schema, &at).await;
    let mut out = match out {
        Ok(out) => {
            conn.execute("COMMIT").await.map_err(|e| read(&any, e))?;
            out
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK").await;
            return Err(e);
        }
    };
    ask_about_partition_defaults(conn, schema, &mut out).await;
    Ok(out)
}

/// One partition's own default to ask about: the column, both declared texts,
/// and the schemas whose paths the emitter writes each under.
struct OwnDefault<'a> {
    partition: &'a TableName,
    parent: &'a TableName,
    column: &'a str,
    own: &'a str,
    parents: &'a str,
    ty: String,
}

/// Which partitions' declared own defaults the engine stores in the same text
/// as their parents', so a plan that sets one is refused by name before any
/// statement rather than by the apply's closing check after it (#1609).
///
/// DEC-1609.1: the engine's text for a declared expression exists only once it
/// is stored, so each pair is stored as two column defaults of a temporary
/// table in a transaction that is always rolled back, and read back with
/// `pg_get_expr` under the empty path the reader uses. Each text is written
/// under the path the emitter writes it under: the partition's own under its
/// schema, its parent's under the parent's (#1607 review). Measured on 16 and
/// 18, `(1)` and `1` are stored as `1`, and `'other'` as `'other'::text`.
///
/// Its own transaction, since the spelling read's is `READ ONLY` and the
/// engine refuses `CREATE TEMP TABLE` in one. Asked only for a partition that
/// overrides a column whose parent declares a default, so a plan with none
/// creates nothing. What cannot be asked — no `TEMP` privilege, an enabled
/// DDL event trigger (one fires on the `CREATE`, measured, so none is run
/// under one; #1669), a text that names an object the plan has yet to
/// create, one that may name a temporary schema (#1706) — is listed in
/// `defaults_unasked`, never read as an answer.
async fn ask_about_partition_defaults(conn: &mut Conn, schema: &Schema, out: &mut Spellings) {
    let mut asked: Vec<OwnDefault<'_>> = Vec::new();
    for (partition, table) in &schema.tables {
        let Some(of) = &table.partition_of else {
            continue;
        };
        let Some(parent) = schema.tables.get(&of.parent) else {
            continue;
        };
        for (column, own) in &of.columns {
            let (Some(own), Some(theirs)) = (own.default.as_deref(), parent.columns.get(column))
            else {
                continue;
            };
            let Some(parents) = theirs.default.as_deref() else {
                continue;
            };
            // The same text is validation's to refuse (DEC-1578.1).
            if own == parents {
                continue;
            }
            // Another session's temporary schema is open to any role with
            // `TEMP`, which `PUBLIC` holds by default, so what is in it can
            // change after the read below. Only a superuser reaches one,
            // and only by naming it: a non-superuser is refused `USAGE`
            // and no path searches one, measured on 16 and 18 (#1706).
            let declared_ty = theirs.ty.to_string();
            if [own, parents, declared_ty.as_str()]
                .into_iter()
                .any(may_name_a_temporary_schema)
            {
                out.defaults_unasked.push(format!(
                    "partition {partition} column `{column}`: its declared text may name a \
                     temporary schema, whose objects another session can change"
                ));
                continue;
            }
            match crate::types::normalize(&theirs.ty) {
                Ok(ty) => asked.push(OwnDefault {
                    partition,
                    parent: &of.parent,
                    column,
                    own,
                    parents,
                    ty: ty.to_string(),
                }),
                Err(_) => out.defaults_unasked.push(format!(
                    "partition {partition} column `{column}`: its type `{}` is not one this \
                     check can create a column of",
                    theirs.ty
                )),
            }
        }
    }
    if asked.is_empty() {
        return;
    }
    // The probe's DDL fires the target's DDL event triggers, and a rollback
    // takes back only what they wrote: a `nextval` they call, a session lock
    // they take, a message they send stays. A connected plan is read-only
    // (SPEC §9.8), so with one enabled nothing is stored and each pair stays
    // to the apply's closing check (#1669). A failed read is no answer either.
    let listed = |names: Vec<String>| {
        names
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut refused = match enabled_ddl_event_triggers(conn).await {
        Ok(names) if names.is_empty() => None,
        Ok(names) => Some(format!(
            "the target has the enabled DDL event trigger(s) {}, which this check's \
             temporary table would fire",
            listed(names)
        )),
        Err(e) => Some(format!("cannot read the target's event triggers: {e}")),
    };
    // A domain's CHECK runs when the parse reads a literal as a composite,
    // array or range over that domain, and it runs as this role: storing
    // `'(5)'::app.c` runs it, measured on 16 and 18. A check another role
    // can rewrite would run that role's code with the deployer's privileges
    // during a plan that is read-only, and an error it raises can carry what
    // it read (#1663). Which domains a declared text reaches is the parse's
    // to find, and nothing holds the catalog still between a read and the
    // store, so any role but a trusted one that can change it at all leaves
    // every pair unasked (#1707).
    if refused.is_none() {
        refused = match untrusted_catalog_writers(conn).await {
            Ok(names) if names.is_empty() => None,
            Ok(names) => Some(format!(
                "the role(s) {}, neither a superuser nor able to become this one, can \
                 change this database's catalog, and storing a default can run a check \
                 written there as this role",
                names.join(", ")
            )),
            Err(e) => Some(format!(
                "cannot read who can change the target's catalog: {}",
                redacted(e)
            )),
        };
    }
    if let Some(why) = refused {
        out.defaults_unasked.extend(
            asked
                .iter()
                .map(|d| format!("partition {} column `{}`: {why}", d.partition, d.column)),
        );
        return;
    }
    let result = store_and_read(conn, &asked).await;
    // Rolled back whatever happened: nothing this asks may outlive it.
    let _ = conn.execute("ROLLBACK").await;
    match result {
        Ok(answers) => {
            for (d, answer) in asked.iter().zip(answers) {
                match answer {
                    Ok((own, parents)) if own == parents => {
                        out.defaults_as_parents
                            .push(pbps_db::catalog::DefaultAsParents {
                                partition: d.partition.clone(),
                                column: d.column.to_owned(),
                                declared: d.own.to_owned(),
                                stored: own,
                            });
                    }
                    Ok(_) => {}
                    Err(why) => out.defaults_unasked.push(format!(
                        "partition {} column `{}`: {why}",
                        d.partition, d.column
                    )),
                }
            }
        }
        Err(why) => out.defaults_unasked.extend(
            asked
                .iter()
                .map(|d| format!("partition {} column `{}`: {why}", d.partition, d.column)),
        ),
    }
}

/// Whether a declared text could name a temporary schema (`pg_temp_N`,
/// `pg_toast_temp_N` or the `pg_temp` alias). Read on the text, not the
/// parse, since the parse is what runs the check. An identifier cannot be
/// split by a comment or quoting, and folding case covers both spellings; a
/// `U&` escape can spell the name without its letters, so any is counted.
/// A text that only mentions one in a literal is left unasked too: that
/// costs a warning, never a wrong answer.
fn may_name_a_temporary_schema(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("pg_temp") || text.contains("pg_toast_temp") || text.contains("u&")
}

/// Every event trigger that DDL could fire: any not disabled, since one
/// enabled for replicas only still fires under a session that runs as one.
async fn enabled_ddl_event_triggers(conn: &mut Conn) -> Result<Vec<String>, DbError> {
    let rows = conn
        .query(
            "SELECT evtname::text AS name FROM pg_catalog.pg_event_trigger
              WHERE evtenabled OPERATOR(pg_catalog.<>) 'D'
                AND evtevent::text IN ('ddl_command_start', 'ddl_command_end', 'sql_drop')
              ORDER BY 1",
        )
        .await?;
    // `evtname` is NOT NULL; a NULL read still counts as a trigger, never
    // as none.
    rows.iter()
        .map(|row| {
            Ok(row
                .try_get::<&str>("name")?
                .unwrap_or("<unnamed>")
                .to_owned())
        })
        .collect()
}

/// The roles other than this one or a superuser that can change this
/// database's catalog, and so what a default's parse reaches, between the
/// probe's read and its store (#1707). Listing the dangerous kinds of object
/// failed five ways in review: a check added `NOT VALID` to a domain, a
/// column of a new domain added to a composite or table, a type created in
/// a schema or a new schema, a membership granted without `SET`, and one
/// granted with `ADMIN` alone (#1706). Catalog lookups see DDL other sessions
/// commit mid-transaction, so the only state that holds until the store is
/// one no such role can change at all.
///
/// A role is trusted when it is a superuser or can `SET ROLE` to this role
/// or to a superuser. Inheriting this role's privileges is not enough: the
/// check would run with `current_user` this role, which a row security
/// policy tells apart from a member that only inherits. From each other
/// role the read walks `pg_auth_members` along memberships granted with
/// `SET`, `INHERIT` or `ADMIN` (with `ADMIN` it can grant the role with `SET`
/// to one of its own), once rather than asking `pg_has_role` of every pair,
/// which took seconds over a few hundred roles. The catalog is open to it
/// when anything it reaches is this role or a superuser, owns any object in
/// this database (whatever its kind: a `pg_shdepend` owner row), or holds
/// `CREATE` on the database or a schema. Predefined roles act only through
/// their members, so `pg_database_owner` counts only under another owner.
async fn untrusted_catalog_writers(conn: &mut Conn) -> Result<Vec<String>, DbError> {
    let rows = conn
        .query(
            "WITH RECURSIVE me AS (
                 SELECT r.oid FROM pg_catalog.pg_roles r
                  WHERE r.rolname OPERATOR(pg_catalog.=) CURRENT_USER),
             here AS (
                 SELECT db.oid, db.datdba FROM pg_catalog.pg_database db
                  WHERE db.datname OPERATOR(pg_catalog.=) pg_catalog.current_database()),
             others AS (
                 SELECT r.oid, r.rolname FROM pg_catalog.pg_roles r
                  WHERE r.oid OPERATOR(pg_catalog.>=) 16384::pg_catalog.oid
                    AND NOT r.rolsuper
                    AND NOT pg_catalog.pg_has_role(r.oid, (SELECT oid FROM me), 'SET')
                    AND NOT EXISTS (
                        SELECT FROM pg_catalog.pg_roles su
                         WHERE su.rolsuper
                           AND pg_catalog.pg_has_role(r.oid, su.oid, 'SET'))),
             walk AS (
                 SELECT o.oid AS origin, o.oid FROM others o
                 UNION
                 SELECT w.origin, m.roleid FROM pg_catalog.pg_auth_members m
                   JOIN walk w ON w.oid OPERATOR(pg_catalog.=) m.member
                  WHERE m.set_option OR m.inherit_option OR m.admin_option),
             reach AS (
                 SELECT origin, oid FROM walk
                 UNION
                 SELECT w.origin, 'pg_database_owner'::pg_catalog.regrole::pg_catalog.oid
                   FROM walk w JOIN here ON here.datdba OPERATOR(pg_catalog.=) w.oid)
             SELECT DISTINCT pg_catalog.format('`%I`', o.rolname) AS name
               FROM others o
               JOIN reach r ON r.origin OPERATOR(pg_catalog.=) o.oid
              WHERE r.oid OPERATOR(pg_catalog.=) (SELECT oid FROM me)
                 OR EXISTS (SELECT FROM pg_catalog.pg_roles su
                             WHERE su.oid OPERATOR(pg_catalog.=) r.oid AND su.rolsuper)
                 OR EXISTS (
                        SELECT FROM pg_catalog.pg_shdepend d
                         WHERE d.refclassid OPERATOR(pg_catalog.=)
                                   'pg_catalog.pg_authid'::pg_catalog.regclass
                           AND d.refobjid OPERATOR(pg_catalog.=) r.oid
                           AND d.dbid OPERATOR(pg_catalog.=) (SELECT oid FROM here)
                           AND d.deptype OPERATOR(pg_catalog.=) 'o')
                 OR pg_catalog.has_database_privilege(r.oid, (SELECT oid FROM here), 'CREATE')
                 OR EXISTS (
                        SELECT FROM pg_catalog.pg_namespace n
                         WHERE NOT pg_catalog.pg_is_other_temp_schema(n.oid)
                           AND pg_catalog.has_schema_privilege(r.oid, n.oid, 'CREATE'))
              ORDER BY 1",
        )
        .await?;
    // `typname` is NOT NULL; a NULL read still counts as a domain, never as
    // none.
    rows.iter()
        .map(|row| {
            Ok(row
                .try_get::<&str>("name")?
                .unwrap_or("<unnamed>")
                .to_owned())
        })
        .collect()
}

/// What a failure inside the probe tells the user: the engine's SQLSTATE,
/// never its text. The text is whatever the code the parse reached chose to
/// raise, and that code ran with this role's privileges (#1663). A failure
/// with no server code is the driver's own and is told as it is.
fn redacted(e: DbError) -> String {
    match e.server_error_code() {
        Some(code) => format!("the engine refused it (SQLSTATE {code})"),
        None => e.to_string(),
    }
}

/// Stores one declared text as `column`'s default, under `under`'s path, in a
/// savepoint of its own.
///
/// The path is the emitter's own, `pg_temp` last included: left out, the
/// engine searches the temporary schema first, and an unqualified name would
/// reach this probe's table where the apply reaches the schema's (#1659). A
/// connected plan's dialect carries no write-path extras.
async fn store_one(conn: &mut Conn, column: &str, under: &str, text: &str) -> Result<(), String> {
    let path =
        crate::emit::write_path(&crate::Postgres::new(), under).map_err(|e| e.to_string())?;
    let fail = redacted;
    conn.execute("SAVEPOINT pbps_1609").await.map_err(fail)?;
    let result = async {
        conn.query(&format!(
            "SELECT pg_catalog.set_config('search_path', '{}', true)",
            path.replace('\'', "''")
        ))
        .await?;
        // One statement through the extended protocol, never a batch: the
        // engine refuses a second command in it, so a declared text cannot
        // end this transaction and commit SQL of its own during a plan that
        // is read-only (SPEC §9.8; #1609 security review).
        conn.query(&format!(
            "ALTER TABLE pg_temp.pbps_1609 ALTER COLUMN {column} SET DEFAULT {}",
            crate::emit::verbatim(text)
        ))
        .await
        .map(drop)
    }
    .await;
    match result {
        Ok(_) => conn
            .execute("RELEASE SAVEPOINT pbps_1609")
            .await
            .map(drop)
            .map_err(fail),
        Err(e) => {
            conn.execute("ROLLBACK TO SAVEPOINT pbps_1609")
                .await
                .map_err(fail)?;
            Err(redacted(e))
        }
    }
}

/// Stores each pair in one temporary table and reads both back, one savepoint
/// per text so one that cannot be stored leaves the rest asked. The caller
/// rolls the transaction back.
async fn store_and_read(
    conn: &mut Conn,
    asked: &[OwnDefault<'_>],
) -> Result<Vec<Result<(String, String), String>>, String> {
    let fail = redacted;
    conn.execute("BEGIN").await.map_err(fail)?;
    // Every parser setting the apply pins, not only the deparse's: a role's
    // `transform_null_equals = on` would store `FALSE = NULL` as `FALSE IS
    // NULL` here and not at the apply (#1609 review). A `SET` in a
    // transaction that rolls back is undone with it.
    conn.execute(crate::SESSION_PINS).await.map_err(fail)?;
    conn.query(CANONICAL_PATH).await.map_err(fail)?;
    let columns = asked
        .iter()
        .enumerate()
        .map(|(i, d)| format!("p{i} {ty}, o{i} {ty}", ty = d.ty))
        .collect::<Vec<_>>()
        .join(", ");
    // One statement, as each default below is.
    conn.query(&format!("CREATE TEMP TABLE pbps_1609 ({columns})"))
        .await
        .map_err(fail)?;
    let mut stored = Vec::with_capacity(asked.len());
    for (i, d) in asked.iter().enumerate() {
        let both = match store_one(conn, &format!("p{i}"), &d.parent.schema, d.parents).await {
            Ok(()) => store_one(conn, &format!("o{i}"), &d.partition.schema, d.own).await,
            Err(e) => Err(format!("its parent's default cannot be stored here: {e}")),
        };
        stored.push(both);
    }
    conn.query(CANONICAL_PATH).await.map_err(fail)?;
    let rows = conn
        .query(
            "SELECT a.attname::text AS name, pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS text
               FROM pg_catalog.pg_attrdef d
               JOIN pg_catalog.pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum
              WHERE d.adrelid = 'pg_temp.pbps_1609'::pg_catalog.regclass",
        )
        .await
        .map_err(fail)?;
    let mut texts = BTreeMap::new();
    for row in &rows {
        let name: Option<&str> = row.try_get("name").map_err(fail)?;
        let text: Option<&str> = row.try_get("text").map_err(fail)?;
        // A NULL is no text, which the lookup below reports as such.
        if let (Some(name), Some(text)) = (name, text) {
            texts.insert(name.to_owned(), text.to_owned());
        }
    }
    Ok(stored
        .into_iter()
        .enumerate()
        .map(|(i, set)| {
            set?;
            match (texts.get(&format!("o{i}")), texts.get(&format!("p{i}"))) {
                (Some(own), Some(parents)) => Ok((own.clone(), parents.clone())),
                _ => Err("the engine stored no text for it".to_owned()),
            }
        })
        .collect())
}

async fn ask_about_every_spelling(
    conn: &mut Conn,
    schema: &Schema,
    at: &crate::rows::CatalogNames,
) -> Result<Spellings, crate::rows::RowsError> {
    let any = TableName::new("", "");
    conn.query(CANONICAL_PATH)
        .await
        .map_err(|e| crate::rows::RowsError::Read {
            table: any,
            source: Box::new(e),
        })?;
    let mut out = Spellings::default();
    for (partition, q) in crate::rows::bound_spelling_queries(schema)? {
        let read = |source| crate::rows::RowsError::Read {
            table: partition.clone(),
            source: Box::new(source),
        };
        for row in &conn.query(&q.sql).await.map_err(read)? {
            let (i, canonical) = crate::rows::decode_spelling(&partition, row)?;
            let Some(declared) = q.values.get(i) else {
                continue;
            };
            if canonical.as_deref() == Some(declared.as_str()) {
                continue;
            }
            out.bounds.push(pbps_db::catalog::MisspeltBound {
                partition: partition.clone(),
                column: q.column.clone(),
                declared: declared.clone(),
                ty: q.ty.clone(),
                canonical,
            });
        }
    }
    let as_declared = crate::rows::Catalogued::default();
    for (name, table) in &schema.tables {
        let at = at.get(name).unwrap_or(&as_declared);
        for q in crate::rows::spelling_queries(name, table, at)? {
            let read = |source| crate::rows::RowsError::Read {
                table: name.clone(),
                source: Box::new(source),
            };
            if let Some(sql) = &q.collisions {
                for row in &conn.query(sql).await.map_err(read)? {
                    let (a, b, canonical) = crate::rows::decode_collision(name, row)?;
                    let (Some((first, _)), Some((second, _))) =
                        (q.literals.get(a), q.literals.get(b))
                    else {
                        continue;
                    };
                    // The key column's own type and collation decide it; the
                    // second insert would fail on the primary key.
                    out.conflicts.push(pbps_model::RowConflict {
                        table: name.clone(),
                        first: first.clone(),
                        second: second.clone(),
                        canonical: pbps_model::RowKey::from(canonical.as_str()),
                    });
                }
            }
            for row in &conn.query(&q.sql).await.map_err(read)? {
                let (i, canonical) = crate::rows::decode_spelling(name, row)?;
                let Some((key, declared)) = q.literals.get(i) else {
                    continue;
                };
                if canonical.as_deref() == Some(declared.as_str()) {
                    continue;
                }
                out.misspelt.push(crate::rows::Misspelt {
                    table: name.clone(),
                    key: key.clone(),
                    column: q.column.clone(),
                    declared: declared.clone(),
                    ty: q.ty.clone(),
                    canonical,
                });
            }
        }
    }
    Ok(out)
}

/// How the database spells each of the schema names a declaration grants on:
/// `None` where it has no schema of that name at all.
///
/// The SQL Server question (DECISIONS 142), asked of this engine for the same
/// reason and answered the only way it can be here. An identifier the emitter
/// writes is always quoted, and a quoted name is compared byte for byte, so
/// the one spelling this database can have for a declared schema is the
/// declared one: `App` and `app` are two schemas, not two spellings. The query
/// therefore answers presence — `nspname` equal to the text, exactly — and can
/// never answer a different spelling. Asked rather than assumed, so that "the
/// schema is there under this name" and "there is no such schema" stay two
/// answers the caller receives from the engine (DECISIONS 417).
pub async fn schema_spellings(
    conn: &mut Conn,
    names: &std::collections::BTreeSet<String>,
) -> Result<std::collections::BTreeMap<String, Option<String>>, DbError> {
    let mut out = std::collections::BTreeMap::new();
    if names.is_empty() {
        return Ok(out);
    }
    let wanted: Vec<&String> = names.iter().collect();
    let params: Vec<Param<'_>> = wanted.iter().map(|n| Param::Str(n.as_str())).collect();
    let values: Vec<String> = (1..=wanted.len())
        .map(|i| format!("(${i}::text)"))
        .collect();
    let sql = format!(
        "WITH wanted(schema_name) AS (VALUES {})\n\
         SELECT w.schema_name, n.nspname AS spelled\n  \
           FROM wanted w\n  \
           LEFT JOIN pg_catalog.pg_namespace n ON n.nspname = w.schema_name",
        values.join(", ")
    );
    for row in &conn.query_with(&sql, &params).await? {
        let name: &str = row.try_get("schema_name")?.ok_or_else(|| {
            DbError::BadRow("the schema spelling query returned a NULL name".to_owned())
        })?;
        let spelled: Option<&str> = row.try_get("spelled")?;
        out.insert(name.to_owned(), spelled.map(ToOwned::to_owned));
    }
    Ok(out)
}

/// A relation-namespace entry the catalog inventory does not report, at a
/// name a plan creates (#951): a sequence, an index, a partitioned index or a
/// standalone composite type. Each shares this engine's one namespace per
/// schema with tables and views, so `CREATE TABLE` or `CREATE VIEW` at its
/// name is refused. The inventory reads tables, views and the relations it
/// cannot represent, never these, so they are asked about here, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameOccupant {
    pub name: TableName,
    /// `sequence`, `index`, `partitioned index` or `composite type`.
    pub kind: &'static str,
    /// The table an index is on, or the table whose column owns a sequence.
    pub owner: Option<TableName>,
    /// The column that owns a sequence, which dropping frees its name.
    pub owner_column: Option<String>,
}

/// The [`NameOccupant`]s at `names`, and those `owners` own (their indexes
/// and owned sequences), read in the caller's transaction. The owners' are
/// what a cross-schema rename carries into its destination (#1084).
pub async fn relation_name_occupants(
    conn: &mut Conn,
    names: &[TableName],
    owners: &[TableName],
) -> Result<Vec<NameOccupant>, DbError> {
    if names.is_empty() && owners.is_empty() {
        return Ok(Vec::new());
    }
    let params: Vec<Param<'_>> = names
        .iter()
        .chain(owners)
        .flat_map(|n| [Param::Str(n.schema.as_str()), Param::Str(n.name.as_str())])
        .collect();
    // `VALUES` cannot be empty: an empty list is a query of no rows.
    let values = |from: usize, count: usize| {
        if count == 0 {
            return "SELECT NULL::text, NULL::text WHERE false".to_owned();
        }
        let rows: Vec<String> = (from..from + count)
            .map(|i| format!("(${}::text, ${}::text)", 2 * i + 1, 2 * i + 2))
            .collect();
        format!("VALUES {}", rows.join(", "))
    };
    let sql = format!(
        "WITH wanted(schema_name, relation_name) AS ({}),\n     \
              owners(schema_name, table_name) AS ({})\n\
         SELECT n.nspname AS schema_name, c.relname AS relation_name,\n       \
                c.relkind::text AS relkind,\n       \
                ownns.nspname AS owner_schema, own.relname AS owner_name,\n       \
                att.attname AS owner_column\n  \
           FROM pg_catalog.pg_class c\n  \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace\n  \
           LEFT JOIN pg_catalog.pg_index i ON i.indexrelid = c.oid\n  \
           LEFT JOIN pg_catalog.pg_depend d\n    \
             ON c.relkind = 'S'\n   \
            AND d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass\n   \
            AND d.objid = c.oid\n   \
            AND d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass\n   \
            AND d.deptype IN ('a', 'i')\n  \
           LEFT JOIN pg_catalog.pg_class own\n    \
             ON own.oid = COALESCE(i.indrelid, d.refobjid)\n  \
           LEFT JOIN pg_catalog.pg_namespace ownns ON ownns.oid = own.relnamespace\n  \
           LEFT JOIN pg_catalog.pg_attribute att\n    \
             ON att.attrelid = d.refobjid AND att.attnum = d.refobjsubid AND d.refobjsubid > 0\n \
          WHERE c.relkind IN ('S', 'i', 'I', 'c')\n   \
            AND (EXISTS (SELECT 1 FROM wanted w\n                  \
                          WHERE w.schema_name = n.nspname AND w.relation_name = c.relname)\n        \
                 OR EXISTS (SELECT 1 FROM owners o\n                     \
                             WHERE o.schema_name = ownns.nspname AND o.table_name = own.relname))",
        values(0, names.len()),
        values(names.len(), owners.len())
    );
    let mut out = Vec::new();
    for row in &conn.query_with(&sql, &params).await? {
        let text = |column: &str| -> Result<String, DbError> {
            row.try_get::<&str>(column)?
                .map(str::to_owned)
                .ok_or_else(|| {
                    DbError::BadRow(format!("the name-occupant query returned a NULL {column}"))
                })
        };
        let kind = match text("relkind")?.as_str() {
            "S" => "sequence",
            "i" => "index",
            "I" => "partitioned index",
            "c" => "composite type",
            other => {
                return Err(DbError::BadRow(format!(
                    "the name-occupant query returned relkind `{other}`"
                )));
            }
        };
        let owner = match (
            row.try_get::<&str>("owner_schema")?,
            row.try_get::<&str>("owner_name")?,
        ) {
            (Some(schema), Some(name)) => Some(TableName::new(schema, name)),
            _ => None,
        };
        out.push(NameOccupant {
            name: TableName::new(text("schema_name")?, text("relation_name")?),
            kind,
            owner,
            owner_column: row.try_get::<&str>("owner_column")?.map(str::to_owned),
        });
    }
    Ok(out)
}

/// The tables and views at `names`, managed or not, each with its kind, read
/// in the caller's transaction (#1765): what a cross-schema rename's carried
/// indexes and owned sequences meet beside the [`NameOccupant`]s. `SET
/// SCHEMA` refuses to carry one onto a name any of them holds (measured on
/// 18). The declarations cannot answer this: what is carried keeps its
/// catalog name, an unnamed key's index and an owned sequence included.
pub async fn relations_at(
    conn: &mut Conn,
    names: &[TableName],
) -> Result<Vec<(TableName, &'static str)>, DbError> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let params: Vec<Param<'_>> = names
        .iter()
        .flat_map(|n| [Param::Str(n.schema.as_str()), Param::Str(n.name.as_str())])
        .collect();
    let rows: Vec<String> = (0..names.len())
        .map(|i| format!("(${}::text, ${}::text)", 2 * i + 1, 2 * i + 2))
        .collect();
    let sql = format!(
        "WITH wanted(schema_name, relation_name) AS (VALUES {})\n\
         SELECT n.nspname AS schema_name, c.relname AS relation_name, c.relkind::text AS relkind\n  \
           FROM pg_catalog.pg_class c\n  \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace\n  \
           JOIN wanted w ON w.schema_name = n.nspname AND w.relation_name = c.relname\n \
          WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f')",
        rows.join(", ")
    );
    let mut out = Vec::new();
    for row in &conn.query_with(&sql, &params).await? {
        let text = |column: &str| -> Result<String, DbError> {
            row.try_get::<&str>(column)?
                .map(str::to_owned)
                .ok_or_else(|| {
                    DbError::BadRow(format!("the relation query returned a NULL {column}"))
                })
        };
        let kind = match text("relkind")?.as_str() {
            "r" => "table",
            "p" => "partitioned table",
            "v" => "view",
            "m" => "materialized view",
            "f" => "foreign table",
            other => {
                return Err(DbError::BadRow(format!(
                    "the relation query returned relkind `{other}`"
                )));
            }
        };
        out.push((
            TableName::new(text("schema_name")?, text("relation_name")?),
            kind,
        ));
    }
    Ok(out)
}

/// A name in a schema that an unnamed primary key's index cannot take
/// (#1645): a relation's of any kind, a composite type's included, or a
/// constraint's in that schema, on any of its tables. Measured on 16 and 18:
/// either makes the engine number the key's index (`_pkey1`), and a
/// constraint of that name in another schema, an enum or a domain does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyNameHolder {
    pub name: TableName,
    /// The table an index or a constraint belongs to, or whose column owns a
    /// sequence, which moves and goes with it (measured on 16 and 18, #1729
    /// review); `None` for any other relation, a table among them.
    pub owner: Option<TableName>,
    /// Whether it is a primary key's index or constraint.
    pub primary_key: bool,
    /// Whether it is a constraint rather than a relation: a check and an
    /// index of one name on one table are two holders, and dropping one
    /// leaves the other.
    pub constraint: bool,
    /// The trigger whose `pg_constraint` row it is (`contype = 't'`), by the
    /// trigger's own name, which `DROP TRIGGER` frees it with (measured on 16
    /// and 18, #1738). The row is named after the trigger when it is created,
    /// but `ALTER TRIGGER ... RENAME` renames only the trigger, and an
    /// ordinary trigger may then take the row's name (measured, #1752
    /// review). An ordinary trigger has no row and holds no name.
    pub trigger: Option<String>,
}

/// The [`KeyNameHolder`]s whose names start with one of `prefixes`, each a
/// `(schema, prefix)`, read in the caller's transaction. A prefix is the part
/// every name the engine may try for a key's index starts with
/// (`implicit_primary_key_fallback`), so the rows are a superset the caller
/// matches whole names against.
pub async fn key_name_holders(
    conn: &mut Conn,
    prefixes: &[(String, String)],
) -> Result<Vec<KeyNameHolder>, DbError> {
    if prefixes.is_empty() {
        return Ok(Vec::new());
    }
    let params: Vec<Param<'_>> = prefixes
        .iter()
        .flat_map(|(schema, prefix)| [Param::Str(schema.as_str()), Param::Str(prefix.as_str())])
        .collect();
    let rows: Vec<String> = (0..prefixes.len())
        .map(|i| format!("(${}::text, ${}::text)", 2 * i + 1, 2 * i + 2))
        .collect();
    let sql = format!(
        "WITH wanted(schema_name, prefix) AS (VALUES {})\n\
         SELECT n.nspname AS schema_name, c.relname::text AS holder_name,\n       \
                ownns.nspname AS owner_schema, own.relname AS owner_name,\n       \
                COALESCE(i.indisprimary, false) AS primary_key,\n       \
                false AS is_constraint, NULL::text AS trigger_name\n  \
           FROM pg_catalog.pg_class c\n  \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace\n  \
           LEFT JOIN pg_catalog.pg_index i ON i.indexrelid = c.oid\n  \
           LEFT JOIN pg_catalog.pg_depend d\n    \
             ON c.relkind = 'S'\n   \
            AND d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass\n   \
            AND d.objid = c.oid\n   \
            AND d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass\n   \
            AND d.deptype IN ('a', 'i')\n  \
           LEFT JOIN pg_catalog.pg_class own ON own.oid = COALESCE(i.indrelid, d.refobjid)\n  \
           LEFT JOIN pg_catalog.pg_namespace ownns ON ownns.oid = own.relnamespace\n \
          WHERE EXISTS (SELECT 1 FROM wanted w\n                  \
                         WHERE w.schema_name = n.nspname\n                    \
                           AND starts_with(c.relname::text, w.prefix))\n\
         UNION ALL\n\
         SELECT n.nspname, con.conname::text, tn.nspname, t.relname, con.contype = 'p', true,\n       \
                (SELECT tg.tgname::text FROM pg_catalog.pg_trigger tg\n                  \
                  WHERE con.contype = 't' AND tg.tgconstraint = con.oid)\n  \
           FROM pg_catalog.pg_constraint con\n  \
           JOIN pg_catalog.pg_namespace n ON n.oid = con.connamespace\n  \
           LEFT JOIN pg_catalog.pg_class t ON t.oid = con.conrelid\n  \
           LEFT JOIN pg_catalog.pg_namespace tn ON tn.oid = t.relnamespace\n \
          WHERE EXISTS (SELECT 1 FROM wanted w\n                  \
                         WHERE w.schema_name = n.nspname\n                    \
                           AND starts_with(con.conname::text, w.prefix))",
        rows.join(", ")
    );
    let mut out = Vec::new();
    for row in &conn.query_with(&sql, &params).await? {
        let text = |column: &str| -> Result<String, DbError> {
            row.try_get::<&str>(column)?
                .map(str::to_owned)
                .ok_or_else(|| {
                    DbError::BadRow(format!(
                        "the key-name holder query returned a NULL {column}"
                    ))
                })
        };
        let owner = match (
            row.try_get::<&str>("owner_schema")?,
            row.try_get::<&str>("owner_name")?,
        ) {
            (Some(schema), Some(name)) => Some(TableName::new(schema, name)),
            _ => None,
        };
        out.push(KeyNameHolder {
            name: TableName::new(text("schema_name")?, text("holder_name")?),
            owner,
            primary_key: row.try_get::<bool>("primary_key")?.ok_or_else(|| {
                DbError::BadRow("the key-name holder query returned a NULL primary_key".into())
            })?,
            constraint: row.try_get::<bool>("is_constraint")?.ok_or_else(|| {
                DbError::BadRow("the key-name holder query returned a NULL is_constraint".into())
            })?,
            trigger: row.try_get::<&str>("trigger_name")?.map(str::to_owned),
        });
    }
    Ok(out)
}

/// A permanent table whose foreign key references a partitioned table
/// (#1595). The engine refuses a permanent table's key to an unlogged table,
/// but not one through a partitioned table that has, or later gets, an
/// unlogged partition (measured on 16 and 18): a crash empties the partition
/// and leaves the referencing rows pointing at nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermanentReferencer {
    /// The partitioned table the key references.
    pub parent: TableName,
    /// The table holding the key: an ordinary table, or a permanent leaf
    /// partition of a partitioned one, which carries a copy of its parent's
    /// key, under the same name or, attached with a key of its own, under
    /// that one's.
    pub table: TableName,
    pub key: String,
    /// The key as its table declares it: for a leaf's copy, the partitioned
    /// table's key it descends from (`conparentid`, followed to the top),
    /// which is the one a plan drops; otherwise `table` and `key` again.
    pub root_table: TableName,
    pub root_key: String,
}

/// The [`PermanentReferencer`]s of `parents`, read in the caller's
/// transaction. Only tables holding rows are asked about: a partitioned
/// referencing table holds none, and each of its leaf partitions carries the
/// key itself, so an unlogged leaf, which a crash empties too, is left out.
/// Each leaf's key is also named as its partitioned table declares it.
pub async fn permanent_referencers(
    conn: &mut Conn,
    parents: &[TableName],
) -> Result<Vec<PermanentReferencer>, DbError> {
    if parents.is_empty() {
        return Ok(Vec::new());
    }
    let params: Vec<Param<'_>> = parents
        .iter()
        .flat_map(|n| [Param::Str(n.schema.as_str()), Param::Str(n.name.as_str())])
        .collect();
    let rows: Vec<String> = (0..parents.len())
        .map(|i| format!("(${}::text, ${}::text)", 2 * i + 1, 2 * i + 2))
        .collect();
    // Each leaf's key walked up `conparentid` to the key its partitioned
    // table declares (measured on 16 and 18: a leaf attached with a key of
    // its own keeps that key's name under the parent's).
    let sql = format!(
        "WITH RECURSIVE wanted(schema_name, table_name) AS (VALUES {}),\n\
         leaves AS (\n  \
           SELECT k.oid AS leaf, k.oid AS at, k.conparentid AS next\n    \
             FROM pg_catalog.pg_constraint k\n    \
             JOIN pg_catalog.pg_class c ON c.oid = k.conrelid\n    \
             JOIN pg_catalog.pg_class pc ON pc.oid = k.confrelid\n    \
             JOIN pg_catalog.pg_namespace pn ON pn.oid = pc.relnamespace\n    \
             JOIN wanted w ON w.schema_name = pn.nspname AND w.table_name = pc.relname\n   \
            WHERE k.contype = 'f' AND c.relkind = 'r' AND c.relpersistence = 'p'\n  \
           UNION ALL\n  \
           SELECT l.leaf, q.oid, q.conparentid\n    \
             FROM leaves l JOIN pg_catalog.pg_constraint q ON q.oid = l.next\n\
         )\n\
         SELECT pn.nspname AS parent_schema, pc.relname AS parent_name,\n       \
                n.nspname AS table_schema, c.relname AS table_name,\n       \
                k.conname AS key_name,\n       \
                rn.nspname AS root_schema, rc.relname AS root_name,\n       \
                r.conname AS root_key\n  \
           FROM leaves l\n  \
           JOIN pg_catalog.pg_constraint k ON k.oid = l.leaf\n  \
           JOIN pg_catalog.pg_class c ON c.oid = k.conrelid\n  \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace\n  \
           JOIN pg_catalog.pg_class pc ON pc.oid = k.confrelid\n  \
           JOIN pg_catalog.pg_namespace pn ON pn.oid = pc.relnamespace\n  \
           JOIN pg_catalog.pg_constraint r ON r.oid = l.at\n  \
           JOIN pg_catalog.pg_class rc ON rc.oid = r.conrelid\n  \
           JOIN pg_catalog.pg_namespace rn ON rn.oid = rc.relnamespace\n \
          WHERE l.next = 0\n \
          ORDER BY 3, 4, 5, 1, 2",
        rows.join(", ")
    );
    let mut out = Vec::new();
    for row in &conn.query_with(&sql, &params).await? {
        let text = |column: &str| -> Result<String, DbError> {
            row.try_get::<&str>(column)?
                .map(str::to_owned)
                .ok_or_else(|| {
                    DbError::BadRow(format!(
                        "the referencing-key query returned a NULL {column}"
                    ))
                })
        };
        out.push(PermanentReferencer {
            parent: TableName::new(text("parent_schema")?, text("parent_name")?),
            table: TableName::new(text("table_schema")?, text("table_name")?),
            key: text("key_name")?,
            root_table: TableName::new(text("root_schema")?, text("root_name")?),
            root_key: text("root_key")?,
        });
    }
    Ok(out)
}

/// What the catalog calls each declared table's key column collation *now*.
///
/// The one input to the spelling checks that is read from the database rather
/// than declared, and the one that decides whether two keys are one row. A
/// table this plan creates has none — the emitter writes no `COLLATE`, so its
/// column will take the database default — and that is what `None` means.
///
/// Read under the names the catalog has now, which is the caller's to supply:
/// the checks run before the plan's first statement, so a table this revision
/// renames is still under its old name (DECISIONS 148).
pub async fn key_collations(
    conn: &mut Conn,
    at: &mut crate::rows::CatalogNames,
    schema: &Schema,
) -> Result<(), DbError> {
    let as_declared = crate::rows::Catalogued::default();
    let mut wanted: Vec<(TableName, TableName, String)> = Vec::new();
    for (name, table) in &schema.tables {
        if table.data.is_none() {
            continue;
        }
        let entry = at.get(name).unwrap_or(&as_declared);
        let Some(key) = table
            .primary_key
            .as_ref()
            .filter(|pk| pk.columns.len() == 1)
            .map(|pk| pk.columns[0].clone())
        else {
            continue;
        };
        wanted.push((
            name.clone(),
            entry.table.clone().unwrap_or_else(|| name.clone()),
            entry.key_column.clone().unwrap_or(key),
        ));
    }
    for (declared, stored, column) in wanted {
        // `to_regclass` and not a cast: a table this plan creates does not
        // exist yet, and `'s.t'::regclass` is an error where this is a NULL.
        let sql = format!(
            "SELECT co.collname AS collation_name, ns.nspname AS collation_schema\n  \
               FROM pg_catalog.pg_attribute a\n  \
               JOIN pg_catalog.pg_collation co ON co.oid = a.attcollation\n  \
               JOIN pg_catalog.pg_namespace ns ON ns.oid = co.collnamespace\n \
              WHERE a.attrelid = pg_catalog.to_regclass({})\n                \
                AND a.attname = {}",
            crate::emit::value_literal(&format!(
                "{}.{}",
                quote_for_regclass(&stored.schema),
                quote_for_regclass(&stored.name)
            )),
            crate::emit::value_literal(&column),
        );
        let rows = conn.query(&sql).await?;
        let Some(row) = rows.first() else { continue };
        let entry = at.entry(declared).or_default();
        entry.key_collation = Some((text(row, "collation_schema")?, text(row, "collation_name")?));
    }
    Ok(())
}

/// One half of a name for `to_regclass`, which parses its argument as SQL
/// rather than taking it literally: a name with a `"` or a `.` in it has to
/// arrive quoted or the function reads it as two.
fn quote_for_regclass(part: &str) -> String {
    format!("\"{}\"", part.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
    async fn a_failed_canonical_permission_query_restores_both_transaction_scopes() {
        let connection = std::env::var("PBPS_TEST_PG_DB").unwrap();
        let mut conn = Conn::connect(pbps_db::Driver::Postgres, &connection)
            .await
            .unwrap();
        conn.execute("SET search_path = public, pg_catalog")
            .await
            .unwrap();
        for caller_owned in [false, true] {
            if caller_owned {
                conn.execute("BEGIN; SET LOCAL search_path = pg_catalog, public")
                    .await
                    .unwrap();
            }
            let before = conn.query("SHOW search_path").await.unwrap();
            let error = canonical_query(&mut conn, "SELECT 1 / 0", &[])
                .await
                .err()
                .expect("division by zero must fail");
            let after = conn.query("SHOW search_path").await.unwrap();
            let still_in_transaction = in_transaction(&mut conn).await.unwrap();
            if caller_owned {
                conn.execute("ROLLBACK").await.unwrap();
            }
            assert_eq!(error.server_error_code().as_deref(), Some("22012"));
            assert_eq!(
                before[0].try_get::<&str>("search_path").unwrap(),
                after[0].try_get::<&str>("search_path").unwrap()
            );
            assert_eq!(still_in_transaction, caller_owned);
        }
    }

    /// The read itself runs without JIT, in both scopes, and the caller's own
    /// `jit` is what the statements after it run under (DEC-1445.1). The session
    /// asks for JIT on everything, so the setting the read sees is the scope's
    /// and not the server default.
    #[tokio::test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
    async fn the_canonical_scope_reads_without_jit_and_hands_back_the_sessions_own() {
        let connection = std::env::var("PBPS_TEST_PG_DB").unwrap();
        let mut conn = Conn::connect(pbps_db::Driver::Postgres, &connection)
            .await
            .unwrap();
        conn.execute("SET jit = on; SET jit_above_cost = 0")
            .await
            .unwrap();
        let jit = |rows: Vec<Row>| text(&rows[0], "jit").unwrap();
        for caller_owned in [false, true] {
            if caller_owned {
                conn.execute("BEGIN").await.unwrap();
            }
            let during =
                jit(
                    canonical_query(&mut conn, "SELECT current_setting('jit') AS jit", &[])
                        .await
                        .unwrap(),
                );
            let after = jit(conn
                .query("SELECT current_setting('jit') AS jit")
                .await
                .unwrap());
            if caller_owned {
                conn.execute("ROLLBACK").await.unwrap();
            }
            assert_eq!(during, "off", "caller_owned = {caller_owned}");
            assert_eq!(after, "on", "caller_owned = {caller_owned}");
        }
    }

    use super::*;

    #[test]
    fn a_text_that_could_name_a_temporary_schema_is_told_by_any_spelling() {
        for text in [
            "('(1)')::pg_temp_3.c",
            "('(1)')::PG_TEMP_3.c",
            "('(1)')::\"pg_temp_3\".c",
            "('(1)')::pg_temp.c",
            "('(1)')::pg_toast_temp_3.c",
            "('(1)')::U&\"\\0070g_temp_3\".c",
            "('(1)')::u&\"pg\\005ftemp_3\".c",
        ] {
            assert!(may_name_a_temporary_schema(text), "{text}");
        }
    }

    #[test]
    fn a_text_naming_no_temporary_schema_is_still_asked() {
        for text in [
            "(1)",
            "'other'::text",
            "('(1)')::app.c",
            "app.temperature()",
            "('(1)')::pg_catalog.int4",
            "pg_catalog.now()",
        ] {
            assert!(!may_name_a_temporary_schema(text), "{text}");
        }
    }

    #[test]
    fn catalog_guidance_preserves_a_separate_source_only_for_recognized_codes() {
        for code in [Some("XX000"), Some("40P01"), Some("57014"), None] {
            let error = schema_changed_underneath(DbError::Driver {
                message: "original server sentence".into(),
                code: code.map(str::to_owned),
            });
            assert_eq!(error.server_error_code().as_deref(), code);
            if matches!(code, Some("XX000" | "40P01")) {
                assert!(error.to_string().contains("Run it again"));
                assert!(!error.to_string().contains("original server sentence"));
                assert_eq!(
                    std::error::Error::source(&error).unwrap().to_string(),
                    "original server sentence"
                );
            } else {
                assert!(matches!(error, DbError::Driver { .. }));
                assert_eq!(error.to_string(), "original server sentence");
            }
        }
    }

    #[test]
    fn catalog_batch_fields_distinguish_null_missing_and_wrong_types() {
        let row = serde_json::json!({"text": "held", "none": null, "large": 9223372036854775807_i64, "flag": false});
        assert_eq!(text(&row, "text").unwrap(), "held");
        assert_eq!(optional_text(&row, "none").unwrap(), None);
        assert!(optional_text(&row, "absent").is_err());
        assert!(optional_text(&row, "flag").is_err());
        assert_eq!(number(&row, "large").unwrap(), i64::MAX);
        assert!(small(&row, "large").is_err());
        assert!(!flag(&row, "flag").unwrap());
        assert!(flag(&row, "none").is_err());
        assert!(decode_batch(&CatalogBatch::new()).is_err());
    }

    /// Every batch key [`decode_batch`] requires, each holding no rows —
    /// a starting point for a test that cares about exactly one part.
    fn empty_catalog_batch() -> CatalogBatch {
        let mut batch = CatalogBatch::new();
        for part in [
            "partitioned",
            "tables",
            "columns",
            "constraints",
            "indexes",
            "modules",
            "module_args",
            "roles",
            "grants",
            "routine_args",
            "empty_routine_acls",
            "other_acls",
            "held_elsewhere",
            "default_acls",
            "unheld_modules",
            "owners",
        ] {
            batch.insert(part.to_owned(), Vec::new());
        }
        // The one part that is never empty: `current_user` always answers.
        batch.insert(
            "session".to_owned(),
            vec![serde_json::json!({"role": "deploy"})],
        );
        batch
    }

    /// Absent, empty and unreadable are three different things, and none of
    /// them is "this connection has no role". A grant's grantor is compared
    /// against this name to decide whether any `REVOKE` could remove it
    /// (#251), so a silently empty one would make every entry look like the
    /// deployer's own.
    #[test]
    fn the_connections_own_role_cannot_be_read_as_absent() {
        let mut batch = empty_catalog_batch();
        assert_eq!(decode_batch(&batch).unwrap().0.session_role, "deploy");
        batch.insert("session".to_owned(), Vec::new());
        assert!(decode_batch(&batch).is_err(), "an empty part");
        batch.insert("session".to_owned(), vec![serde_json::json!({})]);
        assert!(decode_batch(&batch).is_err(), "a row without the column");
        batch.remove("session");
        assert!(decode_batch(&batch).is_err(), "a missing part");
    }

    #[test]
    fn empty_acl_inventory_cannot_read_missing_or_malformed_as_empty() {
        let mut batch = empty_catalog_batch();
        assert!(
            decode_batch(&batch)
                .unwrap()
                .0
                .empty_routine_acls
                .is_empty()
        );
        batch.remove("empty_routine_acls");
        assert!(decode_batch(&batch).is_err());
        let row = serde_json::json!({"schema_name": "app", "name": "closed", "oid": 42});
        batch.insert("empty_routine_acls".to_owned(), vec![row.clone()]);
        assert_eq!(
            decode_batch(&batch).unwrap().0.empty_routine_acls,
            vec![RawEmptyRoutineAcl {
                schema: "app".to_owned(),
                name: "closed".to_owned(),
                routine_oid: 42,
            }]
        );
        for key in ["schema_name", "name", "oid"] {
            let mut broken = row.clone();
            broken.as_object_mut().unwrap().remove(key);
            batch.insert("empty_routine_acls".to_owned(), vec![broken]);
            assert!(decode_batch(&batch).is_err(), "missing {key}");
        }
    }

    #[test]
    fn an_other_acl_default_requires_a_boolean_catalog_fact() {
        let mut batch = empty_catalog_batch();
        let row = serde_json::json!({"grantee": null, "class": "a type", "name": "app.t",
            "privilege_type": "USAGE", "is_grantable": false, "owner": "deploy", "defaulted": true});
        batch.insert("other_acls".to_owned(), vec![row.clone()]);
        assert!(decode_batch(&batch).unwrap().0.other_grants[0].defaulted);
        for value in [serde_json::Value::Null, serde_json::json!("false")] {
            let mut broken = row.clone();
            broken["defaulted"] = value;
            batch.insert("other_acls".to_owned(), vec![broken]);
            assert!(decode_batch(&batch).is_err());
        }
        let mut missing = row;
        missing.as_object_mut().unwrap().remove("defaulted");
        batch.insert("other_acls".to_owned(), vec![missing]);
        assert!(decode_batch(&batch).is_err());
    }

    /// A whole, otherwise-valid constraint row — every column
    /// [`RawConstraint`] needs — so that a test varying `definition` alone
    /// exercises exactly that column, not some other one this fixture left
    /// out. (An earlier, sparser version of this fixture caught a
    /// deliberate revert on `conkey` instead, which was true but not the
    /// point.)
    ///
    /// `definition`'s three states are three different facts: `None` is the
    /// column truly absent from the row (a real reader/query mismatch);
    /// `Some(None)` is the column present and JSON `null` (the deparser
    /// answering `NULL` for an oid that is gone); `Some(Some(s))` is an
    /// ordinary value, for a test that does not care about either.
    fn bare_constraint_row(definition: Option<Option<&str>>) -> serde_json::Value {
        let mut row = serde_json::json!({
            "table_oid": 1, "name": "c1", "kind": "c",
            "schema_name": "s", "table_name": "t",
            "conkey": "1", "confkey": null, "ref_table": 0,
            "on_delete": " ", "on_update": " ",
            "validated": true, "deferrable": false, "deferred": false,
            "expression": null, "match_type": " ",
            "delete_set_columns": null, "index_oid": 0,
            "enforced": true, "period": false, "no_inherit": false,
            "triggers_not_ordinary": false, "inherited": false,
        });
        if let Some(def) = definition {
            row["definition"] = match def {
                Some(s) => serde_json::json!(s),
                None => serde_json::Value::Null,
            };
        }
        row
    }

    /// The constraint branch this issue is about: a `NULL` `pg_get_constraintdef`
    /// is the constraint having vanished between the scan and the deparse, not
    /// a row this reader has lost track of. Mirrors the module branch's own
    /// rule and pins the message an operator sees: which table, which
    /// constraint, and never "gone out of step" (issue #357).
    #[test]
    fn a_constraint_whose_definition_deparsed_away_says_the_catalog_moved() {
        let mut batch = empty_catalog_batch();
        batch.insert(
            "constraints".to_owned(),
            vec![bare_constraint_row(Some(None))],
        );
        let err = decode_batch(&batch).unwrap_err().to_string();
        assert!(
            err.contains("the catalog changed while it was being read"),
            "{err}"
        );
        assert!(
            err.contains("s.t.c1"),
            "the message must name which constraint on which table vanished: {err}"
        );
        assert!(!err.contains("gone out of step"), "{err}");
    }

    /// The negative case that matters: a column the query genuinely never
    /// returned — as opposed to one the deparser answered `NULL` — is still a
    /// real reader/query mismatch, and must still say so. Classifying every
    /// `NULL` as a vanished object would trade one wrong diagnosis for
    /// another.
    #[test]
    fn a_constraint_row_missing_its_definition_column_entirely_still_reports_missing() {
        let mut batch = empty_catalog_batch();
        batch.insert("constraints".to_owned(), vec![bare_constraint_row(None)]);
        let err = decode_batch(&batch).unwrap_err().to_string();
        assert!(err.contains("gone out of step"), "{err}");
        assert!(
            !err.contains("the catalog changed while it was being read"),
            "{err}"
        );
    }

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

    /// The shared framing also serves multi-statement row reads, which need
    /// `REPEATABLE READ`. Without `READ ONLY` nothing but this comment stops
    /// a future owned read from writing, and with `false` for
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
        // Not a rendering setting: the cost of the read (DEC-1445.1). Local like the
        // rest; pinned live by `the_canonical_scope_reads_without_jit_and_hands_back_the_sessions_own`.
        assert!(CANONICAL_PATH.contains("'jit', 'off', true"));
        // Asked of this backend, and of the statement rather than the
        // transaction: `xact_start = query_start` is what "no transaction was
        // open before this one" looks like. Pinned live by
        // `a_pull_inside_the_callers_own_transaction_is_refused`.
        // Set, then read in a *separate* statement: the whole probe is that a
        // `SET LOCAL` outlives its own statement only inside a transaction.
        // Pinned live by `a_pull_inside_the_callers_own_transaction_is_refused`.
        assert!(probe_set("abc").contains("'pbps.in_a_transaction', 'abc', true"));
        assert!(PROBE_READ.contains("current_setting('pbps.in_a_transaction', true)"));
        // And the value is this call's, not a constant a session can already
        // hold: two probes never agree, so a retained setting cannot answer
        // for one of them.
        assert_ne!(probe_token(), probe_token());
        assert!(
            probe_token()
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '-'),
            "the token is interpolated into a statement"
        );
    }

    /// The filters ADR-0012 §6 and this file's own documentation turn on. A
    /// query that lost one of these fails silently — with phantom columns, or
    /// with indexes read back as tables — so they are asserted rather than
    /// trusted to review.
    #[test]
    fn the_filters_that_keep_a_phantom_out_are_in_the_queries_that_need_them() {
        let not_one_of_ours = not_one_of_ours();
        assert!(
            columns_query().contains("NOT a.attisdropped"),
            "ADR-0012 §6"
        );
        assert!(tables_query().contains("relkind = 'r'"), "TABLES");
        // A partitioned parent's columns, keys and indexes are read as an
        // ordinary table's (#1170); an index or a sequence is still neither.
        for (name, sql) in [
            ("COLUMNS", columns_query()),
            ("CONSTRAINTS", constraints_query()),
            ("INDEXES", indexes_query()),
        ] {
            assert!(sql.contains("c.relkind IN ('r', 'p')"), "{name}");
        }
        // A clone is its parent constraint's, on a partition or on the table
        // a foreign key to a partitioned table is declared on (#1170).
        assert!(constraints_query().contains("con.conparentid = 0"));
        for (name, sql) in [
            ("TABLES", tables_query()),
            ("PARTITIONED", partitioned_query()),
        ] {
            assert!(sql.contains(&not_one_of_ours), "{name}: {sql}");
            // By name: a prefix would also hide a project's own table
            // (`app.__pbps_customers`), which nothing refuses at declaration
            // time, and the pull would report it absent.
            assert!(!sql.contains("pbps\\_%"), "{name}: {sql}");
        }
    }

    /// An extension's objects are nobody's declarations (DECISIONS 305), and
    /// that holds for a table as much as for a module: `CREATE EXTENSION …
    /// SCHEMA app` installs one into a project's schema and `ALTER EXTENSION …
    /// ADD TABLE` hands an existing one over. A reader with the filter beside
    /// one without it is the shape this catches — the modules left out
    /// silently, the table beside them pulled as undeclared, and no way to
    /// declare it back. Pinned live by
    /// `what_the_model_cannot_hold_is_named_and_never_silently_dropped`.
    #[test]
    fn an_extensions_table_is_neither_held_nor_named_by_any_reader_that_reads_tables() {
        let not_an_extensions = not_an_extensions("c.oid", "pg_class");
        for (name, sql) in [
            ("TABLES", tables_query()),
            ("PARTITIONED", partitioned_query()),
            ("HELD", on_a_relation_the_pull_holds()),
            ("UNHELD_MODULES", unheld_modules_query()),
        ] {
            assert!(sql.contains(&not_an_extensions), "{name}: {sql}");
        }
        // Over *both* arms of the held predicate, not the view's alone: the
        // filter that keeps an extension's table out of `tables_query` leaves
        // a user's trigger on one with no relation to be read with, and a
        // trigger read without its relation is a pull `check_names` refuses.
        // One filter, ahead of both arms.
        let held = on_a_relation_the_pull_holds();
        let filter = held.find(&not_an_extensions).expect("the filter");
        let view_arm = held.find("c.relkind = 'v'").expect("the view arm");
        let table_arm = held.find(ORDINARY_TABLE).expect("the table arm");
        assert!(filter < view_arm && filter < table_arm, "{held}");
        // And deliberately not the grants reader: a grant on an extension's
        // object is a grant a role really holds, so hiding it would be absent
        // reading as empty. Its own filters are `not_one_of_our_tables` and
        // `NOT_AN_INDEX_OR_TOAST`.
        assert!(
            !grants_query().contains(&not_an_extensions),
            "{}",
            grants_query()
        );
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

    /// The filter and the validation are the same rule, and this is the thread
    /// between them: `validate_table` refuses a declaration naming one of these
    /// tables because the reader hides it, so a name added to one and not the
    /// other is a table that is created and then never seen again.
    /// The module query and the argument query have to agree about which
    /// `pg_proc` rows are modules, and the failure if they do not is silent:
    /// a routine the first returns and the second does not is keyed `f()`,
    /// which is a different object from `f(integer)` under a name that looks
    /// right.
    /// A trigger is read with its relation or not at all: the arm that reads
    /// it and the arm that names it as left out are the table reader's own
    /// predicate and its negation, and a copy of that predicate would be a
    /// rule with two spellings.
    #[test]
    fn a_trigger_is_selected_by_the_predicate_that_selects_its_table() {
        assert!(tables_query().contains(ORDINARY_TABLE));
        let held = on_a_relation_the_pull_holds();
        assert!(held.contains(ORDINARY_TABLE));
        assert!(held.contains("c.relkind = 'v'"));
        assert!(held.contains(&not_an_extensions("c.oid", "pg_class")));
        assert!(
            modules_query().contains(&format!("AND {held}")),
            "{}",
            modules_query()
        );
        assert!(
            unheld_modules_query().contains(&format!("AND NOT {held}")),
            "{}",
            unheld_modules_query()
        );
    }

    #[test]
    fn a_routine_and_its_arguments_are_selected_by_one_predicate() {
        for query in [modules_query(), module_args_query()] {
            assert!(query.contains(ROUTINE_IS_A_MODULE), "{query}");
            // And no second one: a query that filtered `prokind` twice could
            // satisfy the line above and still disagree with the other query.
            // The `prokind` the module query *selects* is not a filter.
            assert_eq!(query.matches("prokind IN").count(), 1, "{query}");
        }
        // The extension filter is the other half of "the same rows", and it is
        // built by one function for both.
        for query in [modules_query(), module_args_query()] {
            assert!(query.contains("d.deptype = 'e'"), "{query}");
        }
    }

    #[test]
    fn the_filter_hides_exactly_the_qualified_names_the_validation_refuses() {
        let not_one_of_ours = not_one_of_ours();
        assert!(not_one_of_ours.contains(&format!(
            "n.nspname = {}",
            crate::emit::value_literal(crate::state::LEDGER_SCHEMA)
        )));
        for qualified in [crate::state::STATE_TABLE, crate::state::LOCK_TABLE] {
            assert!(is_ours(&qualified.parse().unwrap()), "{qualified}");
        }
        for qualified in [
            "app.__pbps_state",
            "app.__pbps_lock",
            "Public.__pbps_state",
            "public.__pbps_customers",
        ] {
            assert!(!is_ours(&qualified.parse().unwrap()), "{qualified}");
        }
        for name in OURS {
            assert!(
                not_one_of_ours.contains(&format!("'{name}'")),
                "`{name}` is refused by the validation and not hidden by the filter"
            );
        }
        // And nothing else: a third name in the filter that the validation does
        // not know is the same drift from the other side.
        assert_eq!(
            not_one_of_ours.matches('\'').count(),
            OURS.len() * 2 + 2,
            "the filter names only the ledger schema and table names: {not_one_of_ours}"
        );
    }

    /// The grants query hides the ledger by name **and** by kind, because the
    /// ledger is two tables and `modules_query` keeps a view whatever it is
    /// called. Hiding a view's ACL row instead pulls the view and drops the
    /// grant on it, which no plan can then reach.
    #[test]
    fn the_grants_query_hides_the_ledger_by_kind_schema_and_name() {
        let not_one_of_ours = not_one_of_ours();
        let not_one_of_our_tables = not_one_of_our_tables();
        let sql = grants_query();
        assert!(sql.contains(&not_one_of_our_tables), "{sql}");
        // And not the kindless filter, which is what over-applied it.
        assert!(
            !sql.contains(&format!("AND {not_one_of_ours}")),
            "the grants query still filters every relkind by name: {sql}"
        );
        for name in OURS {
            assert!(
                not_one_of_our_tables.contains(&format!("'{name}'")),
                "`{name}` is hidden from the inventory and not from the grants"
            );
        }
        assert_eq!(
            not_one_of_our_tables.matches('\'').count(),
            OURS.len() * 2 + 4,
            "the schema, two names, and the `r` that says which kind: {not_one_of_our_tables}"
        );
    }
}
