//! 宣告檔的載入、診斷與正規化輸出。
//!
//! 這是唯一直接依賴 YAML 函式庫的產品 crate（`pbps-config` 因為要讀 `pbps.yml`
//! 也依賴它）。其餘 crate 一律透過這裡，讓日後替換 YAML 實作是有界的工作 ——
//! 見 [`docs/ADR-0001`](../../../docs/ADR-0001-yaml-crate.md)。

pub mod convert;
pub mod dto;
pub mod error;
pub mod fmt;

use std::path::Path;

use pbps_model::{Schema, TableName};

pub use convert::LoadedTable;
pub use error::{LoadError, Semantic, SourceFile};
pub use fmt::render;
pub use pbps_model::Intent;

/// 整個 `schema/` 目錄的載入結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    pub schema: Schema,
    pub intents: Vec<Intent>,
}

/// 從字串載入一張表。`path` 只用於診斷訊息。
pub fn load_table_str(path: &Path, text: &str) -> Result<LoadedTable, Vec<LoadError>> {
    let src = SourceFile::new(path, text);
    let dto: dto::TableDto = serde_saphyr::from_str(text).map_err(|e| {
        vec![LoadError::Yaml {
            path: path.to_owned(),
            message: e.to_string(),
        }]
    })?;
    convert::convert(&src, dto)
}

pub fn load_table_file(path: &Path) -> Result<LoadedTable, Vec<LoadError>> {
    let text = std::fs::read_to_string(path).map_err(|source| {
        vec![LoadError::Io {
            path: path.to_owned(),
            source,
        }]
    })?;
    load_table_str(path, &text)
}

/// 載入整個目錄。
///
/// 檔名不具語意 —— 表名由檔案內的 `table:` 決定。這讓使用者可以自由地按主題
/// 分子目錄，而不必讓檔名與表名綁死。
pub fn load_schema_dir(dir: &Path) -> Result<Loaded, Vec<LoadError>> {
    let mut files = Vec::new();
    collect_yaml_files(dir, &mut files).map_err(|source| {
        vec![LoadError::Io {
            path: dir.to_owned(),
            source,
        }]
    })?;
    // 目錄列舉順序依平台而異，排序後才能得到穩定的診斷順序。
    files.sort();

    let mut loaded = Loaded::default();
    let mut errs = Vec::new();
    let mut seen: std::collections::BTreeMap<TableName, std::path::PathBuf> =
        std::collections::BTreeMap::new();

    for path in files {
        match load_table_file(&path) {
            Ok(mut t) => {
                if let Some(first) = seen.get(&t.name) {
                    errs.push(LoadError::Yaml {
                        path: path.clone(),
                        message: format!("表 `{}` 已經在 `{}` 中宣告過了", t.name, first.display()),
                    });
                    continue;
                }
                seen.insert(t.name.clone(), path);
                loaded.intents.append(&mut t.intents);
                loaded.schema.tables.insert(t.name, t.table);
            }
            Err(mut e) => errs.append(&mut e),
        }
    }

    if errs.is_empty() {
        Ok(loaded)
    } else {
        Err(errs)
    }
}

/// 列出目錄下所有宣告檔，順序穩定。
///
/// `fmt` 需要逐檔處理，不能用 [`load_schema_dir`] 合併後的結果。
pub fn schema_files(dir: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    collect_yaml_files(dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_yaml_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_yaml_files(&path, out)?;
        } else if matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("yml") | Some("yaml")
        ) {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{ReferentialAction, TypeArg};

    fn p() -> &'static Path {
        Path::new("schema/dbo.customer.yml")
    }

    fn load(text: &str) -> LoadedTable {
        match load_table_str(p(), text) {
            Ok(t) => t,
            Err(e) => panic!("預期載入成功，卻得到：{}", render(&e)),
        }
    }

    fn errors(text: &str) -> Vec<LoadError> {
        load_table_str(p(), text).expect_err("預期載入失敗")
    }

    fn render(errs: &[LoadError]) -> String {
        errs.iter()
            .map(|e| format!("{e:?}: {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    const FULL: &str = r#"
table: dbo.customer
description: 客戶主檔
columns:
  customer_id:
    type: bigint
    nullable: false
    identity: [1, 1]
  full_name:
    type: NVARCHAR(100)
    nullable: false
    description: 客戶全名
  email:
    type: nvarchar(255)
  region_id:
    type: int
  balance:
    type: bigint
    nullable: false
    default: "0"
  legacy_code:
    type: varchar(20)
    deprecated: 改用 email 識別
primary_key: [customer_id]
unique:
  uq_customer_email: [email]
foreign_keys:
  fk_customer_region:
    columns: [region_id]
    references: dbo.region(region_id)
    on_delete: cascade
checks:
  ck_customer_balance: balance >= 0
indexes:
  ix_customer_name:
    columns: [full_name, customer_id desc]
    include: [email]
    unique: true
    where: legacy_code IS NULL
"#;

    #[test]
    fn full_document_loads() {
        let t = load(FULL);
        assert_eq!(t.name.to_string(), "dbo.customer");
        assert_eq!(t.table.description.as_deref(), Some("客戶主檔"));
        assert_eq!(t.table.columns.len(), 6);
    }

    /// 欄位順序決定 CREATE TABLE 的排列，必須照文件順序而非字典序。
    #[test]
    fn column_order_follows_the_document() {
        let t = load(FULL);
        assert_eq!(
            t.table.columns.keys().collect::<Vec<_>>(),
            [
                "customer_id",
                "full_name",
                "email",
                "region_id",
                "balance",
                "legacy_code"
            ]
        );
    }

    #[test]
    fn nullable_defaults_to_true() {
        let t = load(FULL);
        assert!(t.table.columns["email"].nullable);
        assert!(!t.table.columns["customer_id"].nullable);
    }

    /// 型別在載入時就正規化大小寫，否則 diff 會產生假的變更。
    #[test]
    fn types_are_normalised_on_load() {
        let t = load(FULL);
        let ty = &t.table.columns["full_name"].ty;
        assert_eq!(ty.base, "nvarchar");
        assert_eq!(ty.args, vec![TypeArg::Int(100)]);
        assert_eq!(ty.to_string(), "nvarchar(100)");
    }

    #[test]
    fn identity_and_deprecated_are_carried() {
        let t = load(FULL);
        let id = t.table.columns["customer_id"].identity.unwrap();
        assert_eq!((id.seed, id.increment), (1, 1));
        assert_eq!(
            t.table.columns["legacy_code"].deprecated.as_deref(),
            Some("改用 email 識別")
        );
    }

    #[test]
    fn constraints_and_indexes_are_parsed() {
        let t = load(FULL);
        assert_eq!(
            t.table.primary_key.as_ref().unwrap().columns,
            ["customer_id"]
        );
        assert_eq!(t.table.unique["uq_customer_email"].columns, ["email"]);

        let fk = &t.table.foreign_keys["fk_customer_region"];
        assert_eq!(fk.references_table.to_string(), "dbo.region");
        assert_eq!(fk.references_columns, ["region_id"]);
        assert_eq!(fk.on_delete, ReferentialAction::Cascade);

        let ix = &t.table.indexes["ix_customer_name"];
        assert!(ix.unique);
        assert_eq!(ix.include, ["email"]);
        assert_eq!(ix.filter.as_deref(), Some("legacy_code IS NULL"));
        assert_eq!(ix.columns[0].name, "full_name");
        assert!(!ix.columns[0].descending);
        assert!(ix.columns[1].descending, "`customer_id desc` 應為降冪");
    }

    /// 具名主鍵要支援，否則 pull 反向生成時會丟失既有約束名。
    #[test]
    fn primary_key_accepts_both_shapes() {
        let unnamed = load("table: dbo.t\ncolumns:\n  a: {type: int}\nprimary_key: [a]\n");
        assert_eq!(unnamed.table.primary_key.unwrap().name, None);

        let named = load(
            "table: dbo.t\ncolumns:\n  a: {type: int}\nprimary_key:\n  name: pk_t\n  columns: [a]\n",
        );
        assert_eq!(
            named.table.primary_key.unwrap().name.as_deref(),
            Some("pk_t")
        );
    }

    // ---- 意圖抽取 ----

    /// renamed_from 是一次性註記，不能進模型 —— 否則同一個狀態會因註記有無而不相等。
    #[test]
    fn rename_intent_is_extracted_not_stored() {
        let t = load(
            "table: dbo.customer\ncolumns:\n  full_name:\n    type: nvarchar(100)\n    renamed_from: customer_name\n",
        );
        assert_eq!(
            t.intents,
            vec![Intent::RenameColumn {
                table: "dbo.customer".parse().unwrap(),
                from: "customer_name".into(),
                to: "full_name".into(),
            }]
        );

        // 同一份宣告，有無註記都必須產生相同的 Table
        let without =
            load("table: dbo.customer\ncolumns:\n  full_name:\n    type: nvarchar(100)\n");
        assert_eq!(t.table, without.table);
        assert!(without.intents.is_empty());
    }

    #[test]
    fn table_rename_intent_is_extracted() {
        let t = load("table: dbo.client\nrenamed_from: dbo.customer\ncolumns:\n  a: {type: int}\n");
        assert_eq!(
            t.intents,
            vec![Intent::RenameTable {
                from: "dbo.customer".parse().unwrap(),
                to: "dbo.client".parse().unwrap(),
            }]
        );
    }

    // ---- 反向案例 ----

    #[test]
    fn unqualified_table_name_is_rejected() {
        let e = errors("table: customer\ncolumns:\n  a: {type: int}\n");
        assert!(render(&e).contains("表名無效"), "{}", render(&e));
    }

    #[test]
    fn invalid_type_is_rejected() {
        let e = errors("table: dbo.t\ncolumns:\n  a:\n    type: \"nvarchar(100\"\n");
        assert!(render(&e).contains("型別無效"), "{}", render(&e));
    }

    #[test]
    fn misspelled_field_is_rejected() {
        let e = errors("table: dbo.t\ncolumns:\n  a:\n    type: int\n    nulable: false\n");
        assert!(render(&e).contains("nulable"), "{}", render(&e));
    }

    /// 同名欄位被靜默吞掉會造成宣告與資料庫的無聲偏差（見 ADR-0001）。
    #[test]
    fn duplicate_column_is_rejected() {
        let e = errors("table: dbo.t\ncolumns:\n  a: {type: int}\n  a: {type: bigint}\n");
        assert!(render(&e).contains("duplicate"), "{}", render(&e));
    }

    #[test]
    fn malformed_foreign_key_target_is_rejected() {
        let e = errors(
            "table: dbo.t\ncolumns:\n  a: {type: int}\nforeign_keys:\n  fk:\n    columns: [a]\n    references: dbo.region\n",
        );
        assert!(render(&e).contains("外鍵目標無效"), "{}", render(&e));
    }

    #[test]
    fn malformed_index_direction_is_rejected() {
        let e = errors(
            "table: dbo.t\ncolumns:\n  a: {type: int}\nindexes:\n  ix:\n    columns: [a sideways]\n",
        );
        assert!(render(&e).contains("索引欄位無效"), "{}", render(&e));
    }

    /// 一次回報所有問題，不要修一個跑一次。
    #[test]
    fn multiple_errors_are_all_reported() {
        let e = errors(
            "table: dbo.t\ncolumns:\n  a:\n    type: \"int(\"\n  b:\n    type: \"varchar(\"\n",
        );
        assert_eq!(e.len(), 2, "應同時回報兩個型別錯誤：{}", render(&e));
    }

    // ---- 目錄載入 ----

    #[test]
    fn directory_load_merges_tables_and_rejects_duplicates() {
        let dir = std::env::temp_dir().join(format!("pbps-load-{}", std::process::id()));
        let nested = dir.join("raw");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            dir.join("a.yml"),
            "table: dbo.customer\ncolumns:\n  a: {type: int}\n",
        )
        .unwrap();
        std::fs::write(
            nested.join("b.yaml"),
            "table: dbo.region\ncolumns:\n  a: {type: int}\n",
        )
        .unwrap();

        let loaded = load_schema_dir(&dir).unwrap();
        assert_eq!(loaded.schema.tables.len(), 2, "應遞迴掃描子目錄");

        // 檔名不具語意，兩個檔案宣告同一張表要擋下
        std::fs::write(
            dir.join("c.yml"),
            "table: dbo.customer\ncolumns:\n  a: {type: int}\n",
        )
        .unwrap();
        let e = load_schema_dir(&dir).unwrap_err();
        assert!(render(&e).contains("已經在"), "{}", render(&e));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
