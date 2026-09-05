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

pub use convert::{LoadedModule, LoadedRole, LoadedTable};
pub use error::{LoadError, Semantic, SourceFile};
pub use fmt::{render, render_module, render_role};
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
    Role(Box<LoadedRole>),
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

/// Loads one role from a string. `path` is used only in diagnostics.
pub fn load_role_str(path: &Path, text: &str) -> Result<LoadedRole, Vec<LoadError>> {
    let src = SourceFile::new(path, text);
    let dto: dto::RoleDto = serde_saphyr::from_str(text).map_err(|e| {
        vec![LoadError::Yaml {
            path: path.to_owned(),
            message: e.to_string(),
        }]
    })?;
    convert::convert_role(&src, dto)
}

/// Loads one file of any kind.
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
    if probe.role.is_some() {
        return load_role_str(path, text).map(|r| LoadedFile::Role(Box::new(r)));
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
        message: "a declaration file starts with `table:`, `view:`, `procedure:`, `function:`, \
                  `trigger:` or `role:`"
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
/// "You declared this twice, and here is the other one."
///
/// One spelling for tables, modules and roles alike: which namespace a name
/// was already taken in is decided by the caller, and the message reads the
/// same whichever it was.
fn already(path: &std::path::Path, what: &str, first: &std::path::Path) -> LoadError {
    LoadError::Yaml {
        path: path.to_owned(),
        message: format!("{what} was already declared in `{}`", first.display()),
    }
}

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
    let mut seen_tables: std::collections::BTreeMap<TableName, std::path::PathBuf> =
        std::collections::BTreeMap::new();
    // Modules are counted apart from tables, and by their whole identity:
    // whether a table and a module may share a name is the engine's answer,
    // not the loader's — on PostgreSQL a table `app.f` and a function
    // `app.f(int)` are two objects (ADR-0009 §1) — and two overloads of one
    // function are two declarations, not a file declared twice.
    // `pbps_dialect::check_module_names` asks the engine the other question.
    let mut seen_modules: std::collections::BTreeMap<pbps_model::ModuleId, std::path::PathBuf> =
        std::collections::BTreeMap::new();
    // Roles live in their own namespace — a role and a table may share a
    // word — so they are checked for duplicates among themselves.
    let mut seen_roles: std::collections::BTreeMap<String, std::path::PathBuf> =
        std::collections::BTreeMap::new();

    for path in files {
        let file = match load_file(&path) {
            Ok(f) => f,
            Err(mut e) => {
                errs.append(&mut e);
                continue;
            }
        };

        let file = match file {
            LoadedFile::Table(t) => {
                if let Some(first) = seen_tables.get(&t.name) {
                    errs.push(already(&path, &format!("`{}`", t.name), first));
                    continue;
                }
                seen_tables.insert(t.name.clone(), path);
                LoadedFile::Table(t)
            }
            LoadedFile::Module(m) => {
                if let Some(first) = seen_modules.get(&m.id) {
                    errs.push(already(&path, &format!("`{}`", m.id), first));
                    continue;
                }
                seen_modules.insert(m.id.clone(), path);
                LoadedFile::Module(m)
            }
            LoadedFile::Role(r) => {
                let mut r = *r;
                if let Some(first) = seen_roles.get(&r.name) {
                    errs.push(already(&path, &format!("role `{}`", r.name), first));
                    continue;
                }
                seen_roles.insert(r.name.clone(), path);
                loaded.intents.append(&mut r.intents);
                loaded.schema.roles.insert(r.name, r.role);
                continue;
            }
        };

        match file {
            LoadedFile::Table(t) => {
                let mut t = *t;
                loaded.intents.append(&mut t.intents);
                if let Some(s) = t.strategy {
                    loaded.hints.strategies.insert(t.name.clone(), s);
                }
                loaded.schema.tables.insert(t.name, t.table);
            }
            LoadedFile::Module(m) => {
                let m = *m;
                if !m.depends_on.is_empty() {
                    loaded.hints.module_deps.insert(m.id.clone(), m.depends_on);
                }
                loaded.schema.modules.insert(m.id, m.module);
            }
            // Merged above, in its own namespace.
            LoadedFile::Role(_) => {}
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
        assert_eq!(m.id.to_string(), "dbo.active_customer");
        assert_eq!(m.module.kind, pbps_model::ModuleKind::View);
        assert_eq!(
            m.module.definition.trim(),
            "SELECT customer_id FROM dbo.customer"
        );
        assert!(m.id.attached_to().is_none());

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

    /// The file keeps the two lines it always had — `trigger: app.audit` and
    /// `on: app.orders` — and they fold into one identity (ADR-0009 §1). No
    /// declaration written for the old model has to change.
    #[test]
    fn a_trigger_file_loads_to_an_identity_holding_its_table() {
        let m = load_module(
            "trigger: app.audit\non: app.orders\ndefinition: AFTER INSERT AS SELECT 1\n",
        );
        assert_eq!(
            m.id,
            pbps_model::ModuleId::Trigger {
                on: "app.orders".parse().unwrap(),
                name: "audit".to_owned()
            }
        );
        assert_eq!(m.id.to_string(), "app.orders.audit");
        assert_eq!(m.id.attached_to().unwrap().to_string(), "app.orders");
    }

    /// A trigger's schema is its table's — SQL Server puts it there and
    /// PostgreSQL gives it none — so two spellings that disagree are a
    /// declaration whose halves mean different things, not one to resolve
    /// silently.
    #[test]
    fn a_trigger_named_in_another_schema_than_its_table_is_refused() {
        let e = load_module_str(
            Path::new("m.yml"),
            "trigger: other.audit\non: app.orders\ndefinition: AFTER INSERT AS SELECT 1\n",
        )
        .expect_err("the two schemas disagree");
        assert!(render(&e).contains("lives in the schema"), "{}", render(&e));
        assert!(render(&e).contains("app.audit"), "{}", render(&e));
    }

    /// A signature in the name is a routine identity, and the argument types
    /// are lifted out of it — never parsed from `definition`, which keeps its
    /// parameter names, modes and defaults (ADR-0009 §1).
    #[test]
    fn a_function_declared_with_a_signature_loads_to_a_routine() {
        let m = load_module(
            "function: app.f(int, text)\ndefinition: (a integer, b text) RETURNS int AS $$ SELECT 1 $$\n",
        );
        let pbps_model::ModuleId::Routine(r) = &m.id else {
            panic!("not a routine: {:?}", m.id);
        };
        assert_eq!(r.name.to_string(), "app.f");
        assert_eq!(
            r.args.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["int", "text"]
        );
        assert_eq!(m.id.to_string(), "app.f(int,text)");

        // And one that takes nothing keeps its parentheses: `app.g()` is a
        // routine, `app.g` is a name in the table namespace.
        let none = load_module("function: app.g()\ndefinition: () RETURNS int AS $$ SELECT 1 $$\n");
        assert!(matches!(none.id, pbps_model::ModuleId::Routine(_)));
        assert_eq!(none.id.to_string(), "app.g()");
    }

    /// `on:` on anything but a trigger would read as if it did something.
    #[test]
    fn only_a_trigger_may_name_a_table_in_a_file() {
        let e = load_module_str(
            Path::new("m.yml"),
            "view: dbo.v\non: dbo.customer\ndefinition: SELECT 1\n",
        )
        .expect_err("a view has no table");
        assert!(render(&e).contains("only a trigger"), "{}", render(&e));
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

        // A view named after a table loads here and is refused by the
        // dialect, not by the loader: whether the two share a namespace is
        // the engine's answer (ADR-0009 §1), and on PostgreSQL a table
        // `dbo.customer` and a function `dbo.customer(int)` are two objects.
        // `pbps_dialect::check_module_names` is where the refusal lives now,
        // and `a_module_named_after_a_table_is_refused` in the CLI's flow
        // tests holds `validate` to still making it.
        std::fs::write(
            dir.join("clash.yml"),
            "view: dbo.customer\ndefinition: SELECT 1\n",
        )
        .unwrap();
        let with_clash = load_schema_dir(&dir).expect("the loader has no namespace rule");
        assert_eq!(with_clash.schema.tables.len(), 2);
        assert_eq!(with_clash.schema.modules.len(), 2);
        std::fs::remove_file(dir.join("clash.yml")).unwrap();

        // Two files declaring one module, though, is the loader's own
        // question, and the answer does not depend on any engine.
        std::fs::write(
            dir.join("v2.yml"),
            "view: dbo.active_customer\ndefinition: SELECT 2\n",
        )
        .unwrap();
        let e = load_schema_dir(&dir).unwrap_err();
        assert!(
            render(&e).contains("was already declared"),
            "{}",
            render(&e)
        );
        std::fs::remove_file(dir.join("v2.yml")).unwrap();

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

    // ---- roles (ADR-0005) ----

    const A_ROLE: &str = "role: app_reader\ndescription: Read-only access\n\ngrants:\n  dbo.customer: [select, view-definition]\n  \"schema::app\": [execute]\n";

    #[test]
    fn a_role_file_loads_and_renders_canonically() {
        let f = load_file_str(Path::new("schema/app_reader.role.yml"), A_ROLE).unwrap();
        let LoadedFile::Role(r) = f else {
            panic!("not a role: {f:?}");
        };
        assert_eq!(r.name, "app_reader");
        assert_eq!(r.role.description.as_deref(), Some("Read-only access"));
        let customer = &r.role.grants[&"dbo.customer".parse().unwrap()];
        assert!(customer.contains(&pbps_model::Permission::ViewDefinition));
        assert!(r.intents.is_empty());
        // Sorted on the way out, so declaration order never shows as a diff.
        assert_eq!(render_role(&r.name, &r.role, &[]), A_ROLE);
    }

    #[test]
    fn a_role_rename_is_intent_not_state_and_survives_fmt_while_pending() {
        let text = "role: app_reader\nrenamed_from: reader\n";
        let f = load_file_str(Path::new("r.yml"), text).unwrap();
        let LoadedFile::Role(r) = f else {
            panic!("not a role: {f:?}");
        };
        assert_eq!(
            r.intents,
            vec![Intent::RenameRole {
                from: "reader".into(),
                to: "app_reader".into()
            }]
        );
        assert_eq!(r.role, pbps_model::Role::default());
        assert_eq!(render_role(&r.name, &r.role, &r.intents), text);
        assert_eq!(render_role(&r.name, &r.role, &[]), "role: app_reader\n");
    }

    #[test]
    fn a_role_with_a_dotted_name_loads_and_an_unknown_permission_is_refused() {
        // `[app.reader]` is a legal principal name; a role is not in a
        // schema, but the dot is the name's, and `pull` writes it back as is.
        let f = load_file_str(Path::new("r.yml"), "role: app.reader\n").unwrap();
        match f {
            LoadedFile::Role(r) => assert_eq!(r.name, "app.reader"),
            other @ (LoadedFile::Table(_) | LoadedFile::Module(_)) => {
                panic!("not a role: {other:?}")
            }
        }
        let errs = load_file_str(Path::new("r.yml"), "role: r\ngrants:\n  dbo.t: [control]\n")
            .unwrap_err();
        assert!(errs[0].to_string().contains("control"), "{errs:?}");
        let errs =
            load_file_str(Path::new("r.yml"), "role: r\ngrants:\n  dbo.t: []\n").unwrap_err();
        assert!(errs[0].to_string().contains("no permission"), "{errs:?}");
        let errs =
            load_file_str(Path::new("r.yml"), "role: r\ngrants:\n  t: [select]\n").unwrap_err();
        assert!(errs[0].to_string().contains("grant target"), "{errs:?}");
    }

    /// Two spellings of one target are one map key once parsed, and the
    /// map kept whichever came last: `select` on `SCHEMA::app` vanished
    /// behind `execute` on `schema::app`, and the next connected plan
    /// revoked it (DECISIONS 126).
    ///
    /// Only the prefix has two spellings. 126 counted surrounding whitespace
    /// as a third, and the engine disagrees: measured, `[ app]` and `[app]`
    /// are two schemas, so those are two targets and the case below asserts
    /// they both survive (DECISIONS 178).
    #[test]
    fn two_spellings_of_one_grant_target_are_refused_not_merged() {
        // The prefix is the only part with two spellings.
        let (a, b) = ("SCHEMA::app", "schema::app");
        let text = format!("role: r\ngrants:\n  \"{a}\": [select]\n  \"{b}\": [execute]\n");
        let errs = load_file_str(Path::new("r.yml"), &text).unwrap_err();
        let rendered = render(&errs);
        assert!(
            rendered.contains("name the same grant target"),
            "{a} / {b}: {rendered}"
        );
        // And a target whose name differs only by padding is a *different*
        // securable, so both are kept with their own permissions.
        let loaded = load_file_str(
            Path::new("r.yml"),
            "role: r\ngrants:\n  \"schema:: app\": [select]\n  \"schema::app\": [execute]\n",
        )
        .expect("two schemas, not two spellings of one");
        let LoadedFile::Role(role) = loaded else {
            panic!("a role file");
        };
        assert_eq!(role.role.grants.len(), 2, "{:?}", role.role.grants);

        // One spelling, twice, is the YAML duplicate the parser refuses.
        let errs = load_file_str(
            Path::new("r.yml"),
            "role: r\ngrants:\n  dbo.t: [select]\n  dbo.t: [execute]\n",
        )
        .unwrap_err();
        assert!(render(&errs).contains("duplicate"), "{}", render(&errs));
    }

    #[test]
    fn two_files_declaring_one_role_are_refused_but_a_role_may_share_a_tables_word() {
        let dir = std::env::temp_dir().join(format!("pbps-load-roles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.yml"), "role: customer\n").unwrap();
        std::fs::write(
            dir.join("b.yml"),
            "table: dbo.customer\ncolumns:\n  id: {type: int}\n",
        )
        .unwrap();
        let loaded = load_schema_dir(&dir).unwrap();
        assert!(loaded.schema.roles.contains_key("customer"));
        assert!(loaded.schema.tables.len() == 1);

        std::fs::write(dir.join("c.yml"), "role: customer\n").unwrap();
        let errs = load_schema_dir(&dir).unwrap_err();
        assert!(errs[0].to_string().contains("already declared"), "{errs:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
