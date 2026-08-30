//! Phase 0 spike — YAML crate 選型驗證。
//!
//! 驗收條件（依 docs/SPEC.md §13.1）：
//!   A. 語法錯誤        → 是否給出行號
//!   B. 型別錯誤        → 是否給出行號（值的位置，不是文件開頭）
//!   C. 未知欄位        → 是否給出行號
//!   D. 重複 key        → 是否偵測得到（兩個同名欄位）
//!   E. **語意錯誤的 span** → 文件合法，但某個值在業務上無效
//!      （未知型別、uid 重複…）能否取得該值的行號 —— 這是 Linter 的核心需求

use std::collections::BTreeMap;

// ---------------------------------------------------------------- 測試素材

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

/// A. 語法錯誤：縮排壞掉
const SYNTAX_ERR: &str = r#"table: dbo.customer
columns:
  customer_id:
    type: bigint
   nullable: false
"#;

/// B. 型別錯誤：null 期望 bool，給了字串
const TYPE_ERR: &str = r#"table: dbo.customer
columns:
  customer_id:
    type: bigint
    nullable: false
  email:
    type: nvarchar(255)
    nullable: maybe
"#;

/// C. 未知欄位：nullabel 拼錯
const UNKNOWN_FIELD: &str = r#"table: dbo.customer
columns:
  customer_id:
    type: bigint
    nullabel: false
"#;

/// D. 重複 key：同一張表出現兩個 email
const DUP_KEY: &str = r#"table: dbo.customer
columns:
  email:
    type: nvarchar(255)
  email:
    type: varchar(50)
"#;

/// E. 語意錯誤：YAML 完全合法，但 `bigInt(9)` 不是有效的 MSSQL 型別。
///    Linter 必須能指到第 7 行。
const SEMANTIC_ERR: &str = r#"table: dbo.customer
columns:
  customer_id:
    type: bigint
    nullable: false
  balance:
    type: bigInt(9)
    nullable: false
"#;

// ---------------------------------------------------------------- 資料模型

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

    /// E 的模型：型別欄位包在 Spanned 裡，取得該值在原始檔中的位置
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
        case("A. 語法錯誤", serde_saphyr::from_str::<Table>(SYNTAX_ERR));
        case("B. 型別錯誤", serde_saphyr::from_str::<Table>(TYPE_ERR));
        case("C. 未知欄位", serde_saphyr::from_str::<Table>(UNKNOWN_FIELD));
        case("D. 重複 key", serde_saphyr::from_str::<Table>(DUP_KEY));

        println!("\n--- E. 語意錯誤的 span ---");
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
        case("A. 語法錯誤", marked_yaml::from_yaml::<Table>(0, SYNTAX_ERR));
        case("B. 型別錯誤", marked_yaml::from_yaml::<Table>(0, TYPE_ERR));
        case("C. 未知欄位", marked_yaml::from_yaml::<Table>(0, UNKNOWN_FIELD));
        case("D. 重複 key", marked_yaml::from_yaml::<Table>(0, DUP_KEY));

        println!("\n--- E. 語意錯誤的 span ---");
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

/// F. Norway problem：YAML 1.1 把 no/yes/on/off 當布林。
///    欄位名 `no`、值 `no_action` 是否安全？
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
