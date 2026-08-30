//! Phase 0 spike - evaluating candidate YAML crates.
//!
//! Acceptance criteria (per docs/SPEC.md §13.1):
//!   A. syntax error      -> is a line number reported?
//!   B. type error        -> is a line number reported (the value's position, not
//!                           the start of the document)?
//!   C. unknown field     -> is a line number reported?
//!   D. duplicate key     -> is it detected at all (two fields with one name)?
//!   E. **span of a semantic error** -> the document is valid but some value is
//!      invalid for the domain (an unknown type, a duplicate uid, …). Can the line
//!      number of that value be obtained? This is the linter's core requirement.

use std::collections::BTreeMap;

// ------------------------------------------------------------------- fixtures

const GOOD: &str = r#"table: dbo.customer
columns:
  customer_id:
    type: bigint
    nullable: false
  full_name:
    type: nvarchar(100)
    nullable: false
  email:
    type: nvarchar(255)
"#;

/// A. Syntax error: broken indentation.
const SYNTAX_ERR: &str = r#"table: dbo.customer
columns:
  customer_id:
    type: bigint
   nullable: false
"#;

/// B. Type error: a bool was expected, a string was given.
const TYPE_ERR: &str = r#"table: dbo.customer
columns:
  customer_id:
    type: bigint
    nullable: false
  email:
    type: nvarchar(255)
    nullable: maybe
"#;

/// C. Unknown field: `nullabel` is a typo.
const UNKNOWN_FIELD: &str = r#"table: dbo.customer
columns:
  customer_id:
    type: bigint
    nullabel: false
"#;

/// D. Duplicate key: one table with two `email` columns.
const DUP_KEY: &str = r#"table: dbo.customer
columns:
  email:
    type: nvarchar(255)
  email:
    type: varchar(50)
"#;

/// E. Semantic error: the YAML is perfectly valid, but `bigInt(9)` is not a
///    valid MSSQL type. The linter has to be able to point at line 7.
const SEMANTIC_ERR: &str = r#"table: dbo.customer
columns:
  customer_id:
    type: bigint
    nullable: false
  balance:
    type: bigInt(9)
    nullable: false
"#;

// ---------------------------------------------------------------- data model

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Table {
    table: String,
    columns: BTreeMap<String, Column>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Column {
    #[serde(rename = "type")]
    ty: String,
    #[serde(default = "yes")]
    nullable: bool,
}

fn yes() -> bool {
    true
}

fn banner(s: &str) {
    println!("\n{}\n{}", s, "=".repeat(s.chars().count()));
}

fn case(name: &str, r: Result<impl std::fmt::Debug, impl std::fmt::Display>) {
    println!("\n--- {name} ---");
    match r {
        Ok(v) => println!("OK: {v:?}"),
        Err(e) => println!("ERR:\n{e}"),
    }
}

// ---------------------------------------------------------------- saphyr

mod saphyr_probe {
    use super::*;
    use serde_saphyr::Spanned;

    /// The model for E: the type field is wrapped in `Spanned` to recover that
    /// value's position in the source.
    #[derive(Debug, serde::Deserialize)]
    struct SpannedTable {
        #[allow(dead_code)]
        table: String,
        columns: BTreeMap<String, SpannedColumn>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct SpannedColumn {
        #[serde(rename = "type")]
        ty: Spanned<String>,
    }

    pub fn run() {
        banner("serde-saphyr");

        case("good", serde_saphyr::from_str::<Table>(GOOD));
        case("A. syntax error", serde_saphyr::from_str::<Table>(SYNTAX_ERR));
        case("B. type error", serde_saphyr::from_str::<Table>(TYPE_ERR));
        case("C. unknown field", serde_saphyr::from_str::<Table>(UNKNOWN_FIELD));
        case("D. duplicate key", serde_saphyr::from_str::<Table>(DUP_KEY));

        println!("\n--- E. span of a semantic error ---");
        match serde_saphyr::from_str::<SpannedTable>(SEMANTIC_ERR) {
            Err(e) => println!("ERR: {e}"),
            Ok(t) => {
                for (name, col) in &t.columns {
                    println!(
                        "  {name:<12} type={:<14} defined={:?} referenced={:?}",
                        col.ty.value, col.ty.defined, col.ty.referenced
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------- marked-yaml

mod marked_probe {
    use super::*;
    use marked_yaml::Spanned;

    #[derive(Debug, serde::Deserialize)]
    struct SpannedTable {
        #[allow(dead_code)]
        table: String,
        columns: BTreeMap<String, SpannedColumn>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct SpannedColumn {
        #[serde(rename = "type")]
        ty: Spanned<String>,
    }

    pub fn run() {
        banner("marked-yaml");

        case("good", marked_yaml::from_yaml::<Table>(0, GOOD));
        case("A. syntax error", marked_yaml::from_yaml::<Table>(0, SYNTAX_ERR));
        case("B. type error", marked_yaml::from_yaml::<Table>(0, TYPE_ERR));
        case("C. unknown field", marked_yaml::from_yaml::<Table>(0, UNKNOWN_FIELD));
        case("D. duplicate key", marked_yaml::from_yaml::<Table>(0, DUP_KEY));

        println!("\n--- E. span of a semantic error ---");
        match marked_yaml::from_yaml::<SpannedTable>(0, SEMANTIC_ERR) {
            Err(e) => println!("ERR: {e}"),
            Ok(t) => {
                for (name, col) in &t.columns {
                    let span = col.ty.span();
                    println!("  {name:<12} type={:<14} span={span:?}", &**col.ty);
                }
            }
        }
    }
}

/// F. The Norway problem: YAML 1.1 reads no/yes/on/off as booleans.
///    Are a column named `no` and a value of `no_action` safe?
const NORWAY: &str = r#"table: dbo.region
columns:
  no:
    type: int
  code:
    type: varchar(2)
    nullable: no
"#;

fn norway() {
    banner("Norway probe (serde-saphyr)");
    case("F. key `no` / value `no`", serde_saphyr::from_str::<Table>(NORWAY));
}

fn main() {
    saphyr_probe::run();
    marked_probe::run();
    norway();
}
