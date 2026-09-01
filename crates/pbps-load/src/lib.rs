//! Loading, diagnosing and canonically rendering the declaration files.
//!
//! This is the only product crate that depends on a YAML library directly
//! (`pbps-config` also does, because it has to read `pbps.yml`). Every other
//! crate goes through here, which keeps replacing the YAML implementation a
//! bounded piece of work — see
//! [`docs/ADR-0001`](../../../docs/ADR-0001-yaml-crate.md).

pub mod convert;
pub mod dto;
pub mod error;
pub mod fmt;

use std::path::Path;

use pbps_model::{Hints, Schema, TableName};

pub use convert::{LoadedModule, LoadedTable};
pub use error::{LoadError, Semantic, SourceFile};
pub use fmt::{render, render_module};
pub use pbps_model::Intent;

/// The result of loading an entire `schema/` directory.
///
/// Three things, deliberately apart: the state, the one-shot intent, and the
/// persistent annotations that say *how* rather than *where* (`strategy:`,
/// `depends_on:`). Only the first may ever take part in a comparison of two
/// states — see inviolable constraint 1.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    pub schema: Schema,
    pub intents: Vec<Intent>,
    /// Only what was declared; absence means the default.
    pub hints: Hints,
}

/// What one declaration file turned out to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadedFile {
    Table(Box<LoadedTable>),
    Module(Box<LoadedModule>),
}

/// Loads one table from a string. `path` is used only in diagnostics.
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

/// Loads one module from a string. `path` is used only in diagnostics.
pub fn load_module_str(path: &Path, text: &str) -> Result<LoadedModule, Vec<LoadError>> {
    let src = SourceFile::new(path, text);
    let dto: dto::ModuleDto = serde_saphyr::from_str(text).map_err(|e| {
        vec![LoadError::Yaml {
            path: path.to_owned(),
            message: e.to_string(),
        }]
    })?;
    convert::convert_module(&src, dto)
}

/// Loads one file of either kind.
///
/// The leading key decides, and it is read in a pass of its own: trying the
/// table shape first and falling back on failure would answer a misspelled
/// `colunms:` with a complaint about a missing `definition:`.
pub fn load_file_str(path: &Path, text: &str) -> Result<LoadedFile, Vec<LoadError>> {
    let probe: dto::KindProbe = serde_saphyr::from_str(text).map_err(|e| {
        vec![LoadError::Yaml {
            path: path.to_owned(),
            message: e.to_string(),
        }]
    })?;
    if probe.table.is_some() {
        return load_table_str(path, text).map(|t| LoadedFile::Table(Box::new(t)));
    }
    if probe.view.is_some()
        || probe.procedure.is_some()
        || probe.function.is_some()
        || probe.trigger.is_some()
    {
        return load_module_str(path, text).map(|m| LoadedFile::Module(Box::new(m)));
    }
    Err(vec![LoadError::Yaml {
        path: path.to_owned(),
        message: "a declaration file starts with `table:`, `view:`, `procedure:`, `function:` \
                  or `trigger:`"
            .to_owned(),
    }])
}

pub fn load_file(path: &Path) -> Result<LoadedFile, Vec<LoadError>> {
    let text = read(path)?;
    load_file_str(path, &text)
}

pub fn load_table_file(path: &Path) -> Result<LoadedTable, Vec<LoadError>> {
    let text = read(path)?;
    load_table_str(path, &text)
}

fn read(path: &Path) -> Result<String, Vec<LoadError>> {
    std::fs::read_to_string(path).map_err(|source| {
        vec![LoadError::Io {
            path: path.to_owned(),
            source,
        }]
    })
}

/// Loads a whole directory.
///
/// File names carry no meaning — the object's name comes from the leading key
/// inside the file. That lets users split things into subdirectories by topic
/// without tying file names to object names.
///
/// Tables and modules share one `seen` map because SQL Server keeps them in one
/// namespace per schema: a view named after a table is a collision the engine
/// would only report at apply time, on a database that is already half-changed.
pub fn load_schema_dir(dir: &Path) -> Result<Loaded, Vec<LoadError>> {
    let mut files = Vec::new();
    collect_yaml_files(dir, &mut files).map_err(|source| {
        vec![LoadError::Io {
            path: dir.to_owned(),
            source,
        }]
    })?;
    // Directory enumeration order is platform-dependent; sorting is what makes
    // the order of diagnostics stable.
    files.sort();

    let mut loaded = Loaded::default();
    let mut errs = Vec::new();
    let mut seen: std::collections::BTreeMap<TableName, std::path::PathBuf> =
        std::collections::BTreeMap::new();

    for path in files {
        let file = match load_file(&path) {
            Ok(f) => f,
            Err(mut e) => {
                errs.append(&mut e);
                continue;
            }
        };

        let name = match &file {
            LoadedFile::Table(t) => t.name.clone(),
            LoadedFile::Module(m) => m.name.clone(),
        };
        if let Some(first) = seen.get(&name) {
            errs.push(LoadError::Yaml {
                path: path.clone(),
                message: format!("`{name}` was already declared in `{}`", first.display()),
            });
            continue;
        }
        seen.insert(name.clone(), path);

        match file {
            LoadedFile::Table(t) => {
                let mut t = *t;
                loaded.intents.append(&mut t.intents);
                if let Some(s) = t.strategy {
                    loaded.hints.strategies.insert(name.clone(), s);
                }
                loaded.schema.tables.insert(name, t.table);
            }
            LoadedFile::Module(m) => {
                let m = *m;
                if !m.depends_on.is_empty() {
                    loaded.hints.module_deps.insert(name.clone(), m.depends_on);
                }
                loaded.schema.modules.insert(name, m.module);
            }
        }
    }

    if errs.is_empty() {
        Ok(loaded)
    } else {
        Err(errs)
    }
}

/// Lists every declaration file under a directory, in a stable order.
///
/// `fmt` has to work file by file and cannot use [`load_schema_dir`]'s merged
/// result.
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
            Err(e) => panic!("expected the load to succeed, got: {}", render(&e)),
        }
    }

    fn errors(text: &str) -> Vec<LoadError> {
        load_table_str(p(), text).expect_err("expected the load to fail")
    }

    fn render(errs: &[LoadError]) -> String {
        errs.iter()
            .map(|e| format!("{e:?}: {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    const FULL: &str = r#"
table: dbo.customer
description: Customer master
columns:
  customer_id:
    type: bigint
    nullable: false
    identity: [1, 1]
  full_name:
    type: NVARCHAR(100)
    nullable: false
    description: The customer's full name
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
    deprecated: superseded by email as the identifier
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
        assert_eq!(t.table.description.as_deref(), Some("Customer master"));
        assert_eq!(t.table.columns.len(), 6);
    }

    /// Column order decides the CREATE TABLE layout, so it must follow the
    /// document rather than alphabetical order.
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

    /// Type case is normalized at load time, or diff would invent changes that
    /// are not there.
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
            Some("superseded by email as the identifier")
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
        assert!(
            ix.columns[1].descending,
            "`customer_id desc` should be descending"
        );
    }

    /// Named primary keys have to be supported, or reverse generation via pull
    /// would lose the existing constraint name.
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

    // ---- intent extraction ----

    /// renamed_from is a one-shot annotation and must not enter the model, or the
    /// same state would compare unequal depending on whether it is present.
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

        // The same declaration must produce the same Table with or without the
        // annotation.
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

    // ---- negative cases ----

    #[test]
    fn unqualified_table_name_is_rejected() {
        let e = errors("table: customer\ncolumns:\n  a: {type: int}\n");
        assert!(render(&e).contains("invalid table name"), "{}", render(&e));
    }

    #[test]
    fn invalid_type_is_rejected() {
        let e = errors("table: dbo.t\ncolumns:\n  a:\n    type: \"nvarchar(100\"\n");
        assert!(render(&e).contains("invalid type"), "{}", render(&e));
    }

    #[test]
    fn misspelled_field_is_rejected() {
        let e = errors("table: dbo.t\ncolumns:\n  a:\n    type: int\n    nulable: false\n");
        assert!(render(&e).contains("nulable"), "{}", render(&e));
    }

    /// Silently swallowing a duplicate column would let the declaration and the
    /// database diverge without a sound (see ADR-0001).
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
        assert!(
            render(&e).contains("invalid foreign key target"),
            "{}",
            render(&e)
        );
    }

    #[test]
    fn malformed_index_direction_is_rejected() {
        let e = errors(
            "table: dbo.t\ncolumns:\n  a: {type: int}\nindexes:\n  ix:\n    columns: [a sideways]\n",
        );
        assert!(
            render(&e).contains("invalid index column"),
            "{}",
            render(&e)
        );
    }

    /// Report every problem at once, rather than fix-one-run-again.
    #[test]
    fn multiple_errors_are_all_reported() {
        let e = errors(
            "table: dbo.t\ncolumns:\n  a:\n    type: \"int(\"\n  b:\n    type: \"varchar(\"\n",
        );
        assert_eq!(
            e.len(),
            2,
            "both type errors should be reported together: {}",
            render(&e)
        );
    }

    // ---- modules (ADR-0002) ----

    const A_VIEW: &str =
        "view: dbo.active_customer\ndefinition: |-\n  SELECT customer_id FROM dbo.customer\n";

    fn load_module(text: &str) -> LoadedModule {
        match load_module_str(Path::new("schema/dbo.v.yml"), text) {
            Ok(m) => m,
            Err(e) => panic!("expected the load to succeed, got: {}", render(&e)),
        }
    }

    #[test]
    fn the_leading_key_decides_the_kind_and_the_name() {
        let m = load_module(A_VIEW);
        assert_eq!(m.name.to_string(), "dbo.active_customer");
        assert_eq!(m.module.kind, pbps_model::ModuleKind::View);
        assert_eq!(
            m.module.definition.trim(),
            "SELECT customer_id FROM dbo.customer"
        );
        assert!(m.module.on.is_none());

        for (text, kind) in [
            (
                "procedure: dbo.p\ndefinition: AS SELECT 1\n",
                pbps_model::ModuleKind::Procedure,
            ),
            (
                "function: dbo.f\ndefinition: () RETURNS int AS BEGIN RETURN 1 END\n",
                pbps_model::ModuleKind::Function,
            ),
            (
                "trigger: dbo.t\non: dbo.customer\ndefinition: AFTER INSERT AS SELECT 1\n",
                pbps_model::ModuleKind::Trigger,
            ),
        ] {
            assert_eq!(load_module(text).module.kind, kind);
        }
    }

    /// A file has to be one object. Two leading keys is not a shape the tool
    /// can guess its way through, and guessing is what it exists not to do.
    #[test]
    fn a_file_declaring_two_objects_is_rejected() {
        let e = load_module_str(
            Path::new("m.yml"),
            "view: dbo.v\nprocedure: dbo.p\ndefinition: AS SELECT 1\n",
        )
        .expect_err("two leading keys must not load");
        assert!(render(&e).contains("2 objects at once"), "{}", render(&e));
    }

    #[test]
    fn a_file_with_no_leading_key_says_which_keys_exist() {
        let e = load_file_str(Path::new("m.yml"), "definition: AS SELECT 1\n")
            .expect_err("a file with no leading key must not load");
        assert!(render(&e).contains("`view:`"), "{}", render(&e));
    }

    #[test]
    fn a_misspelled_module_field_is_rejected() {
        let e = load_module_str(Path::new("m.yml"), "view: dbo.v\ndefinitoin: SELECT 1\n")
            .expect_err("a typo must not load");
        assert!(render(&e).contains("definitoin"), "{}", render(&e));
    }

    /// The dispatch reads the leading key in a pass of its own, so a typo
    /// inside a table file is reported as a table problem — not as a module
    /// missing its `definition:`.
    #[test]
    fn dispatch_reports_the_error_of_the_kind_that_was_declared() {
        let e = load_file_str(
            Path::new("t.yml"),
            "table: dbo.t\ncolunms:\n  a: {type: int}\n",
        )
        .expect_err("a typo must not load");
        assert!(render(&e).contains("colunms"), "{}", render(&e));
        assert!(!render(&e).contains("definition"), "{}", render(&e));
    }

    // ---- directory loading ----

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

        std::fs::write(nested.join("v.yml"), A_VIEW).unwrap();

        let loaded = load_schema_dir(&dir).unwrap();
        assert_eq!(
            loaded.schema.tables.len(),
            2,
            "subdirectories should be scanned recursively"
        );
        assert_eq!(
            loaded.schema.modules.len(),
            1,
            "modules load from the same directory as tables"
        );

        // Tables and modules share one namespace in the database, so they share
        // one here: a view named after a table would fail at apply time, on a
        // database that is already half-changed.
        std::fs::write(
            dir.join("clash.yml"),
            "view: dbo.customer\ndefinition: SELECT 1\n",
        )
        .unwrap();
        let e = load_schema_dir(&dir).unwrap_err();
        assert!(
            render(&e).contains("was already declared"),
            "{}",
            render(&e)
        );
        std::fs::remove_file(dir.join("clash.yml")).unwrap();

        // File names carry no meaning, so two files declaring one table must be
        // rejected.
        std::fs::write(
            dir.join("c.yml"),
            "table: dbo.customer\ncolumns:\n  a: {type: int}\n",
        )
        .unwrap();
        let e = load_schema_dir(&dir).unwrap_err();
        assert!(
            render(&e).contains("was already declared"),
            "{}",
            render(&e)
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
