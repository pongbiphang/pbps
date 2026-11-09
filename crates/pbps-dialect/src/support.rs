//! What pbps does with each kind of object, on each engine (#2717,
//! DEC-2717.1).
//!
//! One record per engine and kind. The tables of `docs/STATUS.md` that list
//! kinds are rendered from here, and tests tie the records to the code they
//! describe: the PostgreSQL census query, and the loader's leading keys. A kind
//! the census starts naming, or a declaration the loader starts accepting,
//! fails a test until it has a record, so a kind cannot become held, named or
//! skipped without this table saying which.
//!
//! In code and not in a data file so that the tests and the engines read it
//! with no parser, as `pbps_pg::pull_coverage` does for catalog columns. That
//! module covers the *properties* of objects the pull holds; this one covers
//! *object kinds*. The long list of named properties in STATUS.md stays prose
//! for that reason.

use std::fmt::Write as _;

/// An engine a record is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Engine {
    Postgres,
    SqlServer,
}

/// What happens to an object of the kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Declared, pulled, diffed and planned.
    Managed,
    /// Held in part: the record's text says which part.
    Partly,
    /// Read, and named to the operator; never part of the schema.
    Named,
    /// Read past with nothing said, where something should be: a bug.
    Silent,
    /// The pull's queries do not read the catalog that lists it.
    NotRead,
}

impl Tier {
    /// The word the generated tables print.
    pub const fn word(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::Partly => "partly held",
            Self::Named => "named",
            Self::Silent => "silent",
            Self::NotRead => "not read",
        }
    }
}

/// A row of "Support planned": worded for that table, which orders kinds by
/// how often users meet them and says more than a tier.
#[derive(Clone, Copy, Debug)]
pub struct Planned {
    pub label: &'static str,
    pub today: &'static str,
    pub tracking: &'static str,
}

/// What pbps does with one kind of object on one engine.
#[derive(Clone, Copy, Debug)]
pub struct Record {
    pub engine: Engine,
    /// The kind, as its own table names it.
    pub label: &'static str,
    pub tier: Tier,
    /// The issues that track it, as a Markdown cell; empty for none.
    pub tracking: &'static str,
    /// The leading keys of the declaration files that declare it.
    pub declared_as: &'static [&'static str],
    /// The kinds the PostgreSQL census query names it by.
    pub census: &'static [&'static str],
    pub planned: Option<Planned>,
}

const fn issue(engine: Engine, label: &'static str, tier: Tier, tracking: &'static str) -> Record {
    Record {
        engine,
        label,
        tier,
        tracking,
        declared_as: &[],
        census: &[],
        planned: None,
    }
}

const fn declared(engine: Engine, key: &'static [&'static str], tier: Tier) -> Record {
    Record {
        declared_as: key,
        ..issue(engine, "", tier, "")
    }
}

const fn census(
    label: &'static str,
    kinds: &'static [&'static str],
    tracking: &'static str,
) -> Record {
    Record {
        census: kinds,
        ..issue(Engine::Postgres, label, Tier::Named, tracking)
    }
}

const CENSUS: &str = "[#2444](https://github.com/pongbiphang/pbps/issues/2444)";
const COVERAGE: &str = "[#168](https://github.com/pongbiphang/pbps/issues/168)";
const SEARCH: &str = "[#2728](https://github.com/pongbiphang/pbps/issues/2728)";
const MSSQL_CENSUS: &str = "[#2794](https://github.com/pongbiphang/pbps/issues/2794)";

use Engine::{Postgres, SqlServer};
use Tier::{Managed, Named, NotRead, Partly};

/// The leading keys of a declaration file, in the order the loader names them.
pub const DECLARATIONS: &[&str] = &[
    "table",
    "view",
    "procedure",
    "function",
    "trigger",
    "extension",
    "role",
    "enum",
    "domain",
    "text_search_config",
    "text_search_dictionary",
    "dependency",
];

/// Every record. The order is the order of the generated tables.
pub const RECORDS: &[Record] = &[
    // What a project declares and no table below lists.
    declared(Postgres, &["table"], Managed),
    declared(SqlServer, &["table"], Managed),
    declared(Postgres, &["view"], Managed),
    declared(SqlServer, &["view"], Managed),
    declared(Postgres, &["procedure"], Managed),
    declared(SqlServer, &["procedure"], Managed),
    declared(Postgres, &["function"], Managed),
    declared(SqlServer, &["function"], Managed),
    declared(Postgres, &["trigger"], Managed),
    declared(SqlServer, &["trigger"], Managed),
    // PostgreSQL grants to a role and never creates one; SQL Server's roles
    // are the database's, and a plan creates them.
    declared(Postgres, &["role"], Partly),
    declared(SqlServer, &["role"], Managed),
    declared(Postgres, &["enum"], Managed),
    declared(Postgres, &["domain"], Managed),
    declared(SqlServer, &["dependency"], Managed),
    // PostgreSQL: the kinds with support planned, in that table's order, among
    // the census kinds in the order of the census query.
    Record {
        declared_as: &["extension"],
        census: &["extension"],
        planned: Some(Planned {
            label: "Extensions, as declared and versioned objects",
            today: "named by census",
            tracking: "#2447; census [#2444](https://github.com/pongbiphang/pbps/issues/2444)",
        }),
        ..issue(Postgres, "Extensions", Partly, CENSUS)
    },
    Record {
        planned: Some(Planned {
            label: "`gist` indexes (held, DEC-2448.2) and `EXCLUDE` constraints (read, declare \
                    and add held, DEC-2448.3; drop, replace, rebuild and partitions next)",
            today: "partly held",
            tracking: "#2448",
        }),
        ..issue(
            Postgres,
            "`gist` indexes and `EXCLUDE` constraints",
            Partly,
            "#2448",
        )
    },
    Record {
        planned: Some(Planned {
            label: "`brin` indexes",
            today: "named",
            tracking: "#2449",
        }),
        ..issue(Postgres, "`brin` indexes", Named, "#2449")
    },
    census("Comments on extensions", &["comment on extension"], CENSUS),
    census("Publications", &["publication"], CENSUS),
    census("Subscriptions", &["subscription"], CENSUS),
    census("Standalone sequences", &["standalone sequence"], CENSUS),
    census("User-defined operators", &["operator"], CENSUS),
    census("Operator classes", &["operator class"], CENSUS),
    census("Extended statistics", &["extended statistics"], CENSUS),
    census("Event triggers", &["event trigger"], CENSUS),
    census("Casts", &["cast"], CENSUS),
    census("Collation objects", &["collation"], CENSUS),
    census("Unused range types", &["range type"], CENSUS),
    census("Unused multirange types", &["multirange type"], CENSUS),
    census("Unused base types", &["base type"], CENSUS),
    census("Transforms", &["transform"], CENSUS),
    census("Large objects", &["large object"], CENSUS),
    census(
        "Tablespaces beyond the two defaults",
        &["tablespace"],
        CENSUS,
    ),
    Record {
        declared_as: &["text_search_config", "text_search_dictionary"],
        census: &["text search configuration", "text search dictionary"],
        planned: Some(Planned {
            label: "Text search configurations and dictionaries (a user's, over the built-in \
                    parser and templates, are held and pulled, DEC-2728.1; create, alter, rename \
                    and drop next)",
            today: "partly held",
            tracking: "#2728",
        }),
        ..issue(
            Postgres,
            "Text search configurations and dictionaries over a custom parser or template, or \
             provided by a package",
            Partly,
            SEARCH,
        )
    },
    census(
        "Text search parsers and templates",
        &["text search parser", "text search template"],
        COVERAGE,
    ),
    census("Conversions", &["conversion"], COVERAGE),
    census(
        "Foreign data wrappers, foreign servers and user mappings",
        &["foreign data wrapper", "foreign server", "user mapping"],
        COVERAGE,
    ),
    census(
        "User-created procedural languages",
        &["procedural language"],
        COVERAGE,
    ),
    // SQL Server: what the pull names, then what its queries never read.
    issue(
        SqlServer,
        "CLR procedures, functions and triggers",
        Named,
        "",
    ),
    issue(
        SqlServer,
        "Modules whose definition cannot be read back (`WITH ENCRYPTION`, or no `VIEW DEFINITION`)",
        Named,
        "",
    ),
    issue(SqlServer, "Columns of a user-defined alias type", Named, ""),
    issue(
        SqlServer,
        "Sequences",
        NotRead,
        "#2721; [#2794](https://github.com/pongbiphang/pbps/issues/2794)",
    ),
    issue(SqlServer, "Synonyms", NotRead, MSSQL_CENSUS),
    issue(SqlServer, "User-defined table types", NotRead, MSSQL_CENSUS),
    issue(SqlServer, "Security policies", NotRead, MSSQL_CENSUS),
    issue(
        SqlServer,
        "Database-level DDL triggers",
        NotRead,
        MSSQL_CENSUS,
    ),
];

/// The records of one engine, in table order.
pub fn records(engine: Engine) -> impl Iterator<Item = &'static Record> {
    RECORDS.iter().filter(move |r| r.engine == engine)
}

/// The tier an engine holds a declaration's leading key at, or `None` when the
/// engine refuses the declaration.
pub fn declared_tier(engine: Engine, key: &str) -> Option<Tier> {
    records(engine)
        .find(|r| r.declared_as.contains(&key))
        .map(|r| r.tier)
}

/// The generated tables of `docs/STATUS.md`, each under the name of the region
/// it fills.
pub fn status_tables() -> Vec<(&'static str, String)> {
    vec![
        ("declared", declared_table()),
        ("postgres-planned", planned_table()),
        ("postgres-silent", silent_table()),
        ("postgres-census", census_table()),
        ("sqlserver", sqlserver_table()),
    ]
}

fn declared_table() -> String {
    let cell = |engine, key| declared_tier(engine, key).map_or("refused", Tier::word);
    let mut out = String::from("| Declaration | PostgreSQL | SQL Server |\n|---|---|---|\n");
    for key in DECLARATIONS {
        let _ = writeln!(
            out,
            "| `{key}:` | {} | {} |",
            cell(Postgres, key),
            cell(SqlServer, key)
        );
    }
    out
}

fn planned_table() -> String {
    let mut out = String::from("| Kind | Today | Tracking |\n|---|---|---|\n");
    for planned in records(Postgres).filter_map(|r| r.planned) {
        let _ = writeln!(
            out,
            "| {} | {} | {} |",
            planned.label, planned.today, planned.tracking
        );
    }
    out
}

fn silent_table() -> String {
    let mut out = String::from("| Kind | Tracking |\n|---|---|\n");
    for record in records(Postgres).filter(|r| r.tier == Tier::Silent) {
        let _ = writeln!(out, "| {} | {} |", record.label, record.tracking);
    }
    out
}

fn census_table() -> String {
    let mut out = String::from("| Kind | Tracking |\n|---|---|\n");
    for record in records(Postgres).filter(|r| !r.census.is_empty()) {
        let _ = writeln!(out, "| {} | {} |", record.label, record.tracking);
    }
    out
}

fn sqlserver_table() -> String {
    let mut out = String::from("| Kind | Today | Tracking |\n|---|---|---|\n");
    for record in records(SqlServer).filter(|r| r.declared_as.is_empty()) {
        let _ = writeln!(
            out,
            "| {} | {} | {} |",
            record.label,
            record.tier.word(),
            record.tracking
        );
    }
    out
}
