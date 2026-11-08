//! What the pull does with every column of every `pg_catalog` table (#168,
//! DEC-168.1).
//!
//! A column is **held** (read into the model, or kept whole in an engine
//! rendering such as `pg_get_functiondef`), **named** (a non-default value
//! prints a pull note, or the object is refused by name), or **irrelevant**
//! (with the reason). A live test per supported major lists the engine's
//! tables and columns and fails on any that has no disposition here, so a
//! property a new major adds must be decided before that major is supported.
//!
//! This is not the resolver capture's classification
//! (`resolver/capture/properties.rs`): that one says what a *sealed capture*
//! must witness (its `attstattarget` is "physical"), this one says what a
//! column means to the *schema*. The two share only the generic
//! [`pbps_db::resolver::capture::rules`] helpers.
//!
//! A table the pull does not read takes one row for the whole table. A table
//! it reads takes a row per column, so a new column cannot inherit a default.

use pbps_db::resolver::capture::rules::{Coverage, unclassified};

/// What the pull does with one column, or with a whole table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Read into the model or kept whole in an engine rendering. `Some(token)`
    /// names the text that must occur in the pull's sources when it is not the
    /// column's own name (a `pg_get_*` function, or a derived alias).
    Held(Option<&'static str>),
    /// A non-default value is named in a pull note or refused by name; the
    /// text says where.
    Named(&'static str),
    /// Proven not to affect the schema; the text says why.
    Irrelevant(&'static str),
}

use Disposition::{Held, Irrelevant, Named};

enum Rows {
    /// Every column, present and future, of a table the pull does not read.
    All(Disposition),
    /// A row per column, grouped by disposition.
    Each(&'static [(&'static [&'static str], Disposition)]),
}

const IDENTITY: Disposition = Irrelevant("the row's own address, not a property");
const OWNER: Disposition =
    Irrelevant("ownership is not modelled (SPEC section 5, ADR-0010 section 3)");
const DERIVED_TYPE: Disposition = Irrelevant("derived from the column's type");
const BUILD_STATE: Disposition = Irrelevant("build and vacuum bookkeeping");
const CLUSTER_WIDE: &str = "cluster-wide, not a schema (ADR-0010)";

/// The columns a major does not have, and the one it alone has: the table
/// below lists the union, and the test removes these for the other major.
const ONLY_ON_16: &[(&str, &str)] = &[("pg_attribute", "attcacheoff")];
const ONLY_ON_18: &[(&str, &str)] = &[
    ("pg_class", "relallfrozen"),
    ("pg_constraint", "conenforced"),
    ("pg_constraint", "conperiod"),
];

// One row per line is the point of the table: rustfmt would give each column
// its own line and bury the dispositions.
#[rustfmt::skip]
static TABLES: &[(&str, Rows)] = &[
    ("pg_aggregate", Rows::All(Named("user-defined aggregates are named as unheld modules"))),
    ("pg_am", Rows::Each(&[
        (&["amname"], Held(None)),
        (&["oid"], IDENTITY),
        (&["amhandler", "amtype"], Irrelevant("an access method is the engine's or an extension's; its name is what a table or index holds")),
    ])),
    ("pg_amop", Rows::All(Named("an operator class's members; the class is named by the census"))),
    ("pg_amproc", Rows::All(Named("an operator class's members; the class is named by the census"))),
    ("pg_attrdef", Rows::Each(&[
        (&["adrelid", "adnum", "adbin"], Held(None)),
        (&["oid"], IDENTITY),
    ])),
    ("pg_attribute", Rows::Each(&[
        (&["attrelid", "attname", "atttypid", "atttypmod", "attnum", "attnotnull", "attidentity", "attgenerated", "attisdropped", "attcollation"], Held(None)),
        (&["attacl"], Held(None)),
        (&["attstorage", "attcompression"], Named("a column's STORAGE and COMPRESSION: column_storage")),
        (&["attstattarget"], Named("a statistics target: unheld-property notice (S1, S5)")),
        (&["attoptions"], Named("column options such as n_distinct: unheld-property notice (S3)")),
        (&["attislocal", "attinhcount"], Named("table inheritance is named")),
        (&["attfdwoptions"], Named("foreign tables are named and left out")),
        (&["attlen", "attbyval", "attalign"], DERIVED_TYPE),
        (&["attndims"], Irrelevant("the engine ignores declared array dimensions (measured: `int[][3]` is `integer[]`)")),
        (&["atthasmissing", "attmissingval"], Irrelevant("the value rows added before a column's default were given: data, not schema")),
        (&["attcacheoff"], Irrelevant("a cached offset: runtime state")),
        (&["atthasdef"], Irrelevant("derived: the pg_attrdef row is the default")),
    ])),
    ("pg_auth_members", Rows::Each(&[
        (&["roleid", "member", "grantor", "admin_option", "inherit_option", "set_option"], Held(None)),
        (&["oid"], IDENTITY),
    ])),
    ("pg_authid", Rows::All(Irrelevant("the roles the pull reads, through the view pg_roles over this table; the password hash is never read"))),
    ("pg_cast", Rows::All(Named("casts are named by the census"))),
    ("pg_class", Rows::Each(&[
        (&["relname", "relnamespace", "relkind", "relpersistence", "relrowsecurity", "relforcerowsecurity", "relreplident", "relispartition", "relpartbound", "reloptions", "relam", "relacl", "reltoastrelid"], Held(None)),
        (&["reltablespace"], Named("a table or index in a non-default tablespace (#2445)")),
        (&["reloftype"], Named("typed tables are named")),
        (&["relhasrules", "relispopulated"], Named("rules and materialized views are named")),
        (&["relowner"], OWNER),
        (&["oid"], IDENTITY),
        (&["reltype"], Irrelevant("the relation's row type: its address")),
        (&["relfilenode", "relpages", "reltuples", "relallvisible", "relallfrozen", "relfrozenxid", "relminmxid", "relrewrite"], Irrelevant("storage location, statistics and vacuum horizons")),
        (&["relhasindex", "relnatts", "relchecks", "relhastriggers", "relhassubclass", "relisshared"], Irrelevant("a derived flag: the index, column, check, trigger and inheritance rows are the facts")),
    ])),
    ("pg_collation", Rows::All(Named("collation objects are named by the census; a column's own collation is named per column"))),
    ("pg_constraint", Rows::Each(&[
        (&["conname", "contype", "conrelid", "contypid", "conindid", "conparentid", "confrelid", "confupdtype", "confdeltype", "conkey", "confkey", "conbin"], Held(None)),
        // An exclusion constraint is held since C1a (DEC-2448.3): its operators and its timing are read
        // whole. `condeferrable` and `condeferred` of a key or a foreign key still draw their own note.
        (&["conexclop", "condeferrable", "condeferred"], Held(None)),
        (&["oid"], IDENTITY),
        (&["connamespace"], Irrelevant("a constraint lives in its table's schema")),
        (&["convalidated"], Named("NOT VALID checks and foreign keys are named")),
        (&["conenforced"], Named("NOT ENFORCED checks are named")),
        (&["connoinherit"], Named("NO INHERIT checks are named")),
        (&["confmatchtype"], Named("MATCH FULL is named")),
        (&["confdelsetcols"], Named("SET NULL / SET DEFAULT with a column list is named")),
        (&["conperiod"], Named("WITHOUT OVERLAPS keys are named")),
        (&["conislocal", "coninhcount"], Named("table inheritance is named")),
        (&["conpfeqop", "conppeqop", "conffeqop"], Irrelevant("the comparison operators, derived from the key columns' types")),
    ])),
    ("pg_conversion", Rows::All(Named("conversions: census notice (S8)"))),
    ("pg_database", Rows::All(Irrelevant("databases are cluster-wide (ADR-0010); the pull reads the current one's name and default collation only"))),
    ("pg_db_role_setting", Rows::All(Irrelevant("role and database settings are cluster configuration"))),
    ("pg_default_acl", Rows::All(Named("ALTER DEFAULT PRIVILEGES: a notice per entry"))),
    ("pg_depend", Rows::Each(&[
        (&["classid", "objid", "objsubid", "refclassid", "refobjid", "refobjsubid", "deptype"], Held(None)),
    ])),
    // Split by what the row is about, not by column (#2729): a comment on a table or one of its
    // columns is read whole into `description` (the `descriptions` batch part); a comment on any
    // other object is named by the unheld-property notice (S11), whose query now excludes the
    // held kinds. A column cannot carry both dispositions, so each is Held here and the notice
    // is pinned by its own live test (`catalog::properties`).
    ("pg_description", Rows::Each(&[
        (&["objoid", "classoid", "objsubid"], Held(None)),
        (&["description"], Held(None)),
    ])),
    ("pg_enum", Rows::Each(&[
        (&["enumtypid", "enumsortorder", "enumlabel"], Held(None)),
        (&["oid"], IDENTITY),
    ])),
    ("pg_event_trigger", Rows::All(Named("event triggers are named by the census"))),
    ("pg_extension", Rows::Each(&[
        (&["extname", "extnamespace", "extversion", "extrelocatable"], Held(None)),
        (&["oid"], IDENTITY),
        (&["extowner"], OWNER),
        (&["extconfig", "extcondition"], Irrelevant("the package's configuration tables hold data")),
    ])),
    ("pg_foreign_data_wrapper", Rows::All(Named("foreign data wrappers: census notice (S9)"))),
    ("pg_foreign_server", Rows::All(Named("foreign servers: census notice (S9)"))),
    ("pg_foreign_table", Rows::All(Named("foreign tables are named and left out"))),
    ("pg_index", Rows::Each(&[
        (&["indexrelid", "indrelid", "indnkeyatts", "indisexclusion", "indisunique", "indnullsnotdistinct", "indisprimary", "indisvalid", "indisreplident", "indkey", "indcollation", "indclass", "indoption", "indexprs", "indpred"], Held(None)),
        (&["indimmediate"], Named("DEFERRABLE keys are named")),
        (&["indisclustered"], Named("the index CLUSTER follows: unheld-property notice (S2)")),
        (&["indnatts"], Irrelevant("the key count plus the INCLUDE count: derived from indkey")),
        (&["indcheckxmin", "indisready", "indislive"], BUILD_STATE),
    ])),
    ("pg_inherits", Rows::Each(&[
        (&["inhrelid", "inhparent", "inhdetachpending"], Held(None)),
        (&["inhseqno"], Named("table inheritance is named")),
    ])),
    ("pg_init_privs", Rows::All(Irrelevant("what an extension installed the object with; the object's own ACL is what the pull reads"))),
    ("pg_language", Rows::All(Named("procedural languages: census notice (S10)"))),
    ("pg_largeobject", Rows::All(Irrelevant("large-object data: rows, not schema; the object is named by the census"))),
    ("pg_largeobject_metadata", Rows::All(Named("large objects are named by the census"))),
    ("pg_namespace", Rows::Each(&[
        (&["nspname", "nspacl"], Held(None)),
        (&["oid"], IDENTITY),
        (&["nspowner"], OWNER),
    ])),
    ("pg_opclass", Rows::All(Named("operator classes are named by the census"))),
    ("pg_operator", Rows::Each(&[
        // An exclusion constraint names its operators by schema and symbol (DEC-2448.3).
        (&["oprname", "oprnamespace"], Held(None)),
        (&["oid"], IDENTITY),
        (&["oprowner", "oprkind", "oprcanmerge", "oprcanhash", "oprleft", "oprright", "oprresult", "oprcom", "oprnegate", "oprcode", "oprrest", "oprjoin"], Named("a user-defined operator is named by the census")),
    ])),
    ("pg_opfamily", Rows::All(Named("operator families belong to an operator class, which the census names"))),
    ("pg_parameter_acl", Rows::Each(&[
        (&["parname", "paracl"], Held(None)),
        (&["oid"], IDENTITY),
    ])),
    ("pg_partitioned_table", Rows::Each(&[
        (&["partrelid", "partstrat", "partattrs"], Held(None)),
        (&["partclass", "partcollation"], Held(Some("pg_get_partkeydef"))),
        (&["partexprs"], Named("a partition key over an expression is named")),
        (&["partnatts"], Irrelevant("derived: the key's length")),
        (&["partdefid"], Irrelevant("the DEFAULT partition is held through its own bound")),
    ])),
    ("pg_policy", Rows::Each(&[
        (&["polname", "polrelid", "polcmd", "polpermissive", "polroles", "polqual", "polwithcheck"], Held(None)),
        (&["oid"], IDENTITY),
    ])),
    ("pg_proc", Rows::Each(&[
        (&["proname", "pronamespace", "prokind", "proacl"], Held(None)),
        (&["prolang", "procost", "prorows", "provariadic", "prosupport", "prosecdef", "proleakproof", "proisstrict", "proretset", "provolatile", "proparallel", "pronargs", "pronargdefaults", "prorettype", "proargtypes", "proallargtypes", "proargmodes", "proargnames", "proargdefaults", "protrftypes", "prosrc", "probin", "prosqlbody", "proconfig"], Held(Some("pg_get_functiondef"))),
        (&["oid"], IDENTITY),
        (&["proowner"], OWNER),
    ])),
    ("pg_publication", Rows::All(Named("publications are named by the census"))),
    ("pg_publication_namespace", Rows::All(Named("a publication's members: the publication is named by the census"))),
    ("pg_publication_rel", Rows::All(Named("a publication's members: the publication is named by the census"))),
    ("pg_range", Rows::All(Named("range types: unused ones are named by the census, a column of one is refused"))),
    ("pg_replication_origin", Rows::All(Irrelevant("replication progress tracking"))),
    ("pg_rewrite", Rows::Each(&[
        (&["rulename", "ev_class"], Held(None)),
        (&["oid"], IDENTITY),
        (&["ev_type", "ev_enabled", "is_instead", "ev_qual", "ev_action"], Named("rewrite rules other than a view's are named; a view's is held through pg_get_viewdef")),
    ])),
    ("pg_seclabel", Rows::All(Named("security labels: unheld-property notice (S12)"))),
    ("pg_sequence", Rows::Each(&[
        (&["seqrelid", "seqtypid", "seqstart", "seqincrement", "seqmax", "seqmin", "seqcache", "seqcycle"], Held(None)),
    ])),
    ("pg_shdepend", Rows::All(Irrelevant("shared dependencies of roles; ownership is not modelled and the ACLs are read from the objects"))),
    ("pg_shdescription", Rows::All(Irrelevant(CLUSTER_WIDE))),
    ("pg_shseclabel", Rows::All(Irrelevant(CLUSTER_WIDE))),
    ("pg_statistic", Rows::All(Irrelevant("planner statistics: measured data"))),
    ("pg_statistic_ext", Rows::All(Named("extended statistics are named by the census"))),
    ("pg_statistic_ext_data", Rows::All(Irrelevant("planner statistics: measured data"))),
    ("pg_subscription", Rows::All(Named("subscriptions are named by the census"))),
    ("pg_subscription_rel", Rows::All(Irrelevant("replication progress per table"))),
    ("pg_tablespace", Rows::All(Named("tablespaces beyond the two defaults are named by the census"))),
    ("pg_transform", Rows::All(Named("transforms are named by the census"))),
    ("pg_trigger", Rows::Each(&[
        (&["tgrelid", "tgparentid", "tgname", "tgisinternal", "tgconstraint"], Held(None)),
        (&["tgfoid", "tgtype", "tgconstrrelid", "tgconstrindid", "tgdeferrable", "tginitdeferred", "tgnargs", "tgattr", "tgargs", "tgqual", "tgoldtable", "tgnewtable"], Held(Some("pg_get_triggerdef"))),
        (&["tgenabled"], Named("triggers that are disabled or set to REPLICA / ALWAYS are named")),
        (&["oid"], IDENTITY),
    ])),
    ("pg_ts_config", Rows::Each(&[
        (&["cfgname", "cfgnamespace", "cfgparser"], Held(None)),
        (&["cfgowner"], OWNER),
        (&["oid"], IDENTITY),
    ])),
    ("pg_ts_config_map", Rows::Each(&[
        (&["mapcfg", "maptokentype", "mapseqno", "mapdict"], Held(None)),
    ])),
    ("pg_ts_dict", Rows::Each(&[
        (&["dictname", "dictnamespace", "dicttemplate", "dictinitoption"], Held(None)),
        (&["dictowner"], OWNER),
        (&["oid"], IDENTITY),
    ])),
    ("pg_ts_parser", Rows::Each(&[
        (&["prsname", "prsnamespace"], Held(None)),
        (&["prsstart", "prstoken", "prsend", "prsheadline", "prslextype"], Named("text search parsers: census notice (S7)")),
        (&["oid"], IDENTITY),
    ])),
    ("pg_ts_template", Rows::Each(&[
        (&["tmplname", "tmplnamespace"], Held(None)),
        (&["tmplinit", "tmpllexize"], Named("text search templates: census notice (S7)")),
        (&["oid"], IDENTITY),
    ])),
    ("pg_type", Rows::Each(&[
        (&["typname", "typnamespace", "typtype", "typrelid", "typelem", "typarray", "typbasetype", "typnotnull", "typtypmod", "typcollation", "typdefaultbin", "typacl", "typisdefined"], Held(None)),
        (&["oid"], IDENTITY),
        (&["typowner"], OWNER),
        (&["typlen", "typbyval", "typcategory", "typispreferred", "typdelim", "typalign", "typstorage", "typndims"], Irrelevant("a base type's representation, or a copy of what a column's own storage note already says")),
        (&["typdefault"], Irrelevant("the text form of typdefaultbin, which is read")),
        (&["typsubscript", "typinput", "typoutput", "typreceive", "typsend", "typmodin", "typmodout", "typanalyze"], Named("base types: unused ones are named by the census, a column of one is refused")),
    ])),
    ("pg_user_mapping", Rows::All(Named("user mappings: census notice (S9)"))),
];

/// The disposition of `table.column` on `major`, `None` when there is none.
#[must_use]
pub fn disposition(major: u32, table: &str, column: &str) -> Option<Disposition> {
    let (_, rows) = TABLES.iter().find(|(name, _)| *name == table)?;
    match rows {
        Rows::All(disposition) => Some(*disposition),
        Rows::Each(groups) => {
            if absent_on(major, table, column) {
                return None;
            }
            groups
                .iter()
                .find(|(columns, _)| columns.contains(&column))
                .map(|(_, disposition)| *disposition)
        }
    }
}

fn absent_on(major: u32, table: &str, column: &str) -> bool {
    let missing = match major {
        16 => ONLY_ON_18,
        _ => ONLY_ON_16,
    };
    missing.contains(&(table, column))
}

/// What is wrong between the engine's `(table, column)` pairs on `major` and
/// this table, one sentence each. Empty means every table and column is
/// accounted for and nothing listed is stale.
#[must_use]
pub fn problems(major: u32, engine: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    let tables: std::collections::BTreeSet<_> = engine.iter().map(|(t, _)| t.as_str()).collect();
    for table in &tables {
        if !TABLES.iter().any(|(name, _)| name == table) {
            out.push(format!("table `{table}` has no disposition"));
        }
    }
    for (table, rows) in TABLES {
        if !tables.contains(table) {
            out.push(format!(
                "`{table}` is listed but the engine has no such table"
            ));
        }
        if let Rows::Each(groups) = rows {
            for column in groups.iter().flat_map(|(columns, _)| columns.iter()) {
                let on_engine = engine.iter().any(|(t, c)| t == table && c == column);
                if !on_engine && !absent_on(major, table, column) && tables.contains(table) {
                    out.push(format!(
                        "`{table}.{column}` is listed but the engine has no such column"
                    ));
                }
            }
        }
    }
    let known = |t: &str| TABLES.iter().any(|(name, _)| *name == t);
    let unknown = unclassified(engine.iter().filter(|(t, _)| known(t)), |(t, c)| {
        disposition(major, t, c).map(|d| match d {
            Named(why) | Irrelevant(why) if why.trim().is_empty() => Coverage::WithReason(""),
            Named(why) | Irrelevant(why) => Coverage::WithReason(why),
            Held(_) => Coverage::Classified,
        })
    });
    for (table, column) in unknown {
        out.push(format!("column `{table}.{column}` has no disposition"));
    }
    out.sort();
    out.dedup();
    out
}

/// Every held column, with the token that must occur in the pull's sources.
#[must_use]
pub fn held_tokens() -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    for (table, rows) in TABLES {
        match rows {
            Rows::All(Held(via)) => out.push((*table, via.unwrap_or(""))),
            Rows::All(_) => {}
            Rows::Each(groups) => {
                for (columns, disposition) in *groups {
                    if let Held(via) = disposition {
                        out.extend(columns.iter().map(|c| (*table, via.unwrap_or(*c))));
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine's columns as the table itself lists them for `major`: the
    /// fabricated "engine" every negative below starts from.
    fn listed(major: u32) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (table, rows) in TABLES {
            match rows {
                Rows::All(_) => out.push(((*table).to_owned(), "any".to_owned())),
                Rows::Each(groups) => {
                    for column in groups.iter().flat_map(|(c, _)| c.iter()) {
                        if !absent_on(major, table, column) {
                            out.push(((*table).to_owned(), (*column).to_owned()));
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn a_complete_engine_listing_has_no_problem_on_either_major() {
        for major in [16, 18] {
            assert_eq!(
                problems(major, &listed(major)),
                Vec::<String>::new(),
                "PG{major}"
            );
        }
    }

    #[test]
    fn an_unclassified_column_fails() {
        let mut engine = listed(18);
        engine.push(("pg_class".into(), "relfuture".into()));
        let found = problems(18, &engine);
        assert_eq!(found, ["column `pg_class.relfuture` has no disposition"]);
    }

    #[test]
    fn an_unclassified_table_fails() {
        let mut engine = listed(18);
        engine.push(("pg_future".into(), "oid".into()));
        let found = problems(18, &engine);
        assert_eq!(found, ["table `pg_future` has no disposition"]);
    }

    #[test]
    fn a_column_a_major_does_not_have_is_stale_there_and_present_on_the_other() {
        // 18's `conenforced` is not a column on 16; if the 16 server reported
        // it the listing would still be complete, and if it did not, nothing
        // is stale.
        assert!(problems(16, &listed(16)).is_empty());
        let mut engine = listed(16);
        engine.retain(|(t, c)| !(t == "pg_class" && c == "relname"));
        assert_eq!(
            problems(16, &engine),
            ["`pg_class.relname` is listed but the engine has no such column"]
        );
        let mut engine = listed(18);
        engine.retain(|(t, _)| t != "pg_trigger");
        assert!(
            problems(18, &engine)
                .iter()
                .any(|p| p.contains("pg_trigger"))
        );
    }

    #[test]
    fn a_named_or_irrelevant_row_needs_its_reason() {
        for (table, rows) in TABLES {
            let check = |d: &Disposition| match d {
                Named(why) | Irrelevant(why) => assert!(!why.trim().is_empty(), "{table}"),
                Held(via) => assert!(via.is_none_or(|v| !v.is_empty()), "{table}"),
            };
            match rows {
                Rows::All(d) => check(d),
                Rows::Each(groups) => groups.iter().for_each(|(_, d)| check(d)),
            }
        }
        assert_eq!(disposition(18, "pg_class", "relfuture"), None);
        assert_eq!(disposition(18, "pg_future", "oid"), None);
    }

    #[test]
    fn a_column_is_in_exactly_one_group() {
        for (table, rows) in TABLES {
            if let Rows::Each(groups) = rows {
                let mut seen = std::collections::BTreeSet::new();
                for column in groups.iter().flat_map(|(c, _)| c.iter()) {
                    assert!(seen.insert(*column), "{table}.{column} is listed twice");
                }
            }
        }
    }

    /// A held column is one the pull's SQL or readers actually mention. The
    /// match is by name, so it can only be a lint (a name can occur in a
    /// comment), but a typo or a column nobody reads fails here.
    #[test]
    fn every_held_column_is_mentioned_in_the_pull_sources() {
        let sources = [
            include_str!("catalog.rs"),
            include_str!("catalog/census.rs"),
            include_str!("catalog/properties.rs"),
            include_str!("introspect.rs"),
            include_str!("modules.rs"),
            include_str!("user_types.rs"),
            include_str!("roles.rs"),
            include_str!("partman.rs"),
            include_str!("extension_provider.rs"),
            include_str!("search.rs"),
        ]
        .join("\n");
        let word = |token: &str| {
            sources.match_indices(token).any(|(at, _)| {
                let before = sources[..at].chars().next_back();
                let after = sources[at + token.len()..].chars().next();
                let part = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
                !part(before) && !part(after)
            })
        };
        let missing: Vec<_> = held_tokens()
            .into_iter()
            .filter(|(_, token)| !token.is_empty() && !word(token))
            .collect();
        assert!(missing.is_empty(), "held but never mentioned: {missing:?}");
    }

    #[tokio::test]
    #[ignore = "needs live PostgreSQL 16; set PBPS_TEST_PG_OLD_DB"]
    async fn every_pg_catalog_column_has_a_disposition_on_pg16() {
        live("PBPS_TEST_PG_OLD_DB", 16).await;
    }

    #[tokio::test]
    #[ignore = "needs live PostgreSQL 18; set PBPS_TEST_PG_DB"]
    async fn every_pg_catalog_column_has_a_disposition_on_pg18() {
        live("PBPS_TEST_PG_DB", 18).await;
    }

    async fn live(variable: &str, major: u32) {
        use pbps_db::{Conn, Driver};
        let server = std::env::var(variable).expect("the pinned PostgreSQL fixture");
        let mut conn = Conn::connect(Driver::Postgres, &server).await.unwrap();
        let rows = conn
            .query(
                "SELECT c.relname::text AS t, a.attname::text AS c
                   FROM pg_catalog.pg_class c
                   JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid
                  WHERE c.relnamespace = 'pg_catalog'::regnamespace AND c.relkind = 'r'
                    AND a.attnum > 0 AND NOT a.attisdropped
                  ORDER BY 1, a.attnum",
            )
            .await
            .unwrap();
        let engine: Vec<(String, String)> = rows
            .iter()
            .map(|r| {
                (
                    r.try_get::<&str>("t").unwrap().unwrap().to_owned(),
                    r.try_get::<&str>("c").unwrap().unwrap().to_owned(),
                )
            })
            .collect();
        let version = conn
            .query("SELECT (current_setting('server_version_num')::int / 10000)::text AS v")
            .await
            .unwrap();
        assert_eq!(
            version[0].try_get::<&str>("v").unwrap(),
            Some(major.to_string().as_str())
        );
        assert!(
            engine.len() > 400,
            "a query that found nothing agrees with anything"
        );
        let found = problems(major, &engine);
        assert!(found.is_empty(), "PG{major}:\n{}", found.join("\n"));
    }
}
