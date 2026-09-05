//! Canonical rendering.
//!
//! The tool owns the format of the declaration files (SPEC §4.2): `pbps fmt`
//! rewrites each file in full, so ordinary YAML comments are lost and any
//! explanatory prose belongs in a `description` field.
//!
//! # The quoting rules are not about looks
//!
//! YAML parses a bare `no` / `yes` / `on` / `off` as a boolean, `null` and `~` as
//! nulls, and `0123` as a number. Without quoting on output, a file the tool
//! wrote would come back as a different type on the next read — that is, the tool
//! would produce files it cannot read back. So every scalar that could be
//! misread is quoted (see the experiments in ADR-0001).

use std::fmt::Write as _;

use pbps_model::{Intent, Module, ModuleId, ObjectName, PrimaryKey, Strategy, Table, TableName};

/// Renders one table as canonical YAML.
///
/// Renames in `intents` that concern this table are written back out as
/// `renamed_from` annotations: they are one-shot input and do not live in the
/// model, but rewriting the file must not lose them. The caller decides which
/// intents still belong in the file — `pbps fmt` passes only the ones not yet
/// absorbed into the ids file, which is how a redundant annotation gets
/// stripped (SPEC §6.2).
pub fn render(
    name: &TableName,
    table: &Table,
    intents: &[Intent],
    strategy: Option<&Strategy>,
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "table: {}", scalar(&name.to_string()));

    if let Some(d) = &table.description {
        let _ = writeln!(s, "description: {}", scalar(d));
    }

    if let Some(Intent::RenameTable { from, .. }) = intents
        .iter()
        .find(|i| matches!(i, Intent::RenameTable { to, .. } if to == name))
    {
        let _ = writeln!(s, "renamed_from: {}", scalar(&from.to_string()));
    }

    // Persistent, unlike `renamed_from`: rewriting the file must preserve it
    // (ADR-0003). The default renders as nothing, so a table that never asked
    // for a strategy keeps a file with no block.
    if let Some(st) = strategy.filter(|s| !s.is_default()) {
        s.push_str("\nstrategy:\n");
        if st.online {
            s.push_str("  online: true\n");
        }
    }

    s.push_str("\ncolumns:\n");
    for (col_name, c) in &table.columns {
        let _ = writeln!(s, "  {}:", scalar(col_name));
        let _ = writeln!(s, "    type: {}", scalar(&c.ty.to_string()));
        if !c.nullable {
            s.push_str("    nullable: false\n");
        }
        if let Some(d) = &c.default {
            let _ = writeln!(s, "    default: {}", scalar(d));
        }
        if let Some(id) = &c.identity {
            let _ = writeln!(s, "    identity: [{}, {}]", id.seed, id.increment);
        }
        if let Some(d) = &c.description {
            let _ = writeln!(s, "    description: {}", scalar(d));
        }
        if let Some(d) = &c.deprecated {
            let _ = writeln!(s, "    deprecated: {}", scalar(d));
        }
        if let Some(Intent::RenameColumn { from, .. }) = intents.iter().find(|i| {
            matches!(i, Intent::RenameColumn { table: t, to, .. } if t == name && to == col_name)
        }) {
            let _ = writeln!(s, "    renamed_from: {}", scalar(from));
        }
    }

    if let Some(pk) = &table.primary_key {
        s.push('\n');
        match pk {
            PrimaryKey {
                name: None,
                columns,
            } => {
                let _ = writeln!(s, "primary_key: {}", seq(columns));
            }
            PrimaryKey {
                name: Some(n),
                columns,
            } => {
                let _ = writeln!(s, "primary_key:");
                let _ = writeln!(s, "  name: {}", scalar(n));
                let _ = writeln!(s, "  columns: {}", seq(columns));
            }
        }
    }

    if !table.unique.is_empty() {
        s.push_str("\nunique:\n");
        for (n, u) in &table.unique {
            let _ = writeln!(s, "  {}: {}", scalar(n), seq(&u.columns));
        }
    }

    if !table.foreign_keys.is_empty() {
        s.push_str("\nforeign_keys:\n");
        for (n, fk) in &table.foreign_keys {
            let _ = writeln!(s, "  {}:", scalar(n));
            let _ = writeln!(s, "    columns: {}", seq(&fk.columns));
            let _ = writeln!(
                s,
                "    references: {}",
                scalar(&format!(
                    "{}({})",
                    fk.references_table,
                    fk.references_columns.join(", ")
                ))
            );
            if fk.on_delete != Default::default() {
                let _ = writeln!(s, "    on_delete: {}", action(fk.on_delete));
            }
            if fk.on_update != Default::default() {
                let _ = writeln!(s, "    on_update: {}", action(fk.on_update));
            }
        }
    }

    if !table.checks.is_empty() {
        s.push_str("\nchecks:\n");
        for (n, c) in &table.checks {
            let _ = writeln!(s, "  {}: {}", scalar(n), scalar(&c.expression));
        }
    }

    if !table.indexes.is_empty() {
        s.push_str("\nindexes:\n");
        for (n, ix) in &table.indexes {
            let _ = writeln!(s, "  {}:", scalar(n));
            let cols: Vec<String> = ix
                .columns
                .iter()
                .map(|c| {
                    if c.descending {
                        format!("{} desc", c.name)
                    } else {
                        c.name.clone()
                    }
                })
                .collect();
            let _ = writeln!(s, "    columns: {}", seq(&cols));
            if !ix.include.is_empty() {
                let _ = writeln!(s, "    include: {}", seq(&ix.include));
            }
            if ix.unique {
                s.push_str("    unique: true\n");
            }
            if let Some(f) = &ix.filter {
                let _ = writeln!(s, "    where: {}", scalar(f));
            }
        }
    }

    // Last, and after the constraints, because it is the only block that is
    // about the table's contents rather than its shape — and on a lookup table
    // it is much the longest.
    if let Some(d) = &table.data {
        s.push_str("\ndata:\n");
        let _ = writeln!(s, "  mode: {}", d.mode);
        s.push_str("  rows:\n");
        for (key, row) in &d.rows {
            let cells: Vec<String> = row
                .columns()
                .map(|(c, v)| format!("{}: {}", scalar(c), value(v)))
                .collect();
            // The key goes through `scalar`, so a code that looks like a number
            // or a boolean (`no`, `1`, `on` — all real status codes) comes back
            // as the text it is. The loader reads keys as strings, and an
            // unquoted `no:` would reach it as `false`.
            let _ = writeln!(s, "    {}: {{{}}}", scalar(&key.0), cells.join(", "));
        }
    }

    s
}

/// One cell.
///
/// `Text` goes through `scalar`, which already quotes anything number-shaped —
/// which is exactly what keeps a quoted `'1.50'` quoted, and so keeps it out of
/// the float arm the loader refuses.
fn value(v: &pbps_model::Value) -> String {
    match v {
        pbps_model::Value::Null => "null".to_owned(),
        pbps_model::Value::Bool(b) => b.to_string(),
        pbps_model::Value::Int(i) => i.to_string(),
        pbps_model::Value::Text(t) => scalar(t),
    }
}

/// Renders one module as canonical YAML (ADR-0002).
///
/// The definition goes out as a literal block scalar (`|`), which is the only
/// YAML form that keeps SQL exactly as written: no escaping, no line joining,
/// and the round trip is byte-for-byte. A definition with trailing whitespace
/// or no final newline would come back subtly different, so the block is
/// written with `|-` and the text normalized to it — which is `fmt`'s job
/// anyway, and never changes what the SQL means.
///
/// `depends_on` is persistent, like `strategy:`: it is an answer about this
/// project that stays true, so rewriting the file must not lose it.
pub fn render_module(
    id: &ModuleId,
    module: &Module,
    depends_on: &std::collections::BTreeSet<ModuleId>,
) -> String {
    let mut s = String::new();
    // The file keeps its two lines for a trigger — `trigger: app.audit` and
    // `on: app.orders` — even though the model folds them into one identity
    // (ADR-0009 §1). The declaration is what a person reads, and splitting the
    // table back out is what makes `pull` of a database with a trigger produce
    // the file that database came from.
    let (declared, on) = match id {
        ModuleId::Trigger { on, name } => (
            ObjectName::new(on.schema.clone(), name.clone()).to_string(),
            Some(on.to_string()),
        ),
        other @ (ModuleId::Named(_) | ModuleId::Routine(_)) => (other.to_string(), None),
    };
    let _ = writeln!(s, "{}: {}", module.kind.as_str(), scalar(&declared));

    if let Some(d) = &module.description {
        let _ = writeln!(s, "description: {}", scalar(d));
    }
    if let Some(on) = on.as_deref() {
        let _ = writeln!(s, "on: {}", scalar(on));
    }
    if !depends_on.is_empty() {
        let names: Vec<String> = depends_on.iter().map(ToString::to_string).collect();
        let _ = writeln!(s, "depends_on: {}", seq(&names));
    }

    s.push_str("\ndefinition: |-\n");
    for line in module.definition.trim_end().lines() {
        if line.is_empty() {
            // A truly empty line needs no indent, and writing one would put
            // trailing whitespace in the file for nothing.
            s.push('\n');
        } else {
            // Everything else goes out verbatim, trailing spaces included. They
            // look like something to tidy up and are not: a line inside a
            // multiline T-SQL literal ends where its author put it, and
            // trimming here would have `pbps fmt` quietly change what the module
            // returns — the one thing a formatter must never do.
            let _ = writeln!(s, "  {line}");
        }
    }
    s
}

/// Renders one role as canonical YAML (ADR-0005).
///
/// A pending `RenameRole` intent for this role is written back as
/// `renamed_from:`, exactly as a table's is: the annotation survives `fmt`
/// until the identity file has absorbed it.
pub fn render_role(name: &str, role: &pbps_model::Role, pending: &[Intent]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "role: {}", scalar(name));
    if let Some(d) = &role.description {
        let _ = writeln!(s, "description: {}", scalar(d));
    }
    for i in pending {
        if let Intent::RenameRole { from, to } = i
            && to == name
        {
            let _ = writeln!(s, "renamed_from: {}", scalar(from));
        }
    }
    if !role.grants.is_empty() {
        s.push_str("\ngrants:\n");
        for (target, permissions) in &role.grants {
            let names: Vec<String> = permissions.iter().map(|p| p.as_str().to_owned()).collect();
            let _ = writeln!(s, "  {}: {}", scalar(&target.to_string()), seq(&names));
        }
    }
    s
}

fn action(a: pbps_model::ReferentialAction) -> &'static str {
    use pbps_model::ReferentialAction as R;
    match a {
        R::NoAction => "no_action",
        R::Cascade => "cascade",
        R::SetNull => "set_null",
        R::SetDefault => "set_default",
    }
}

fn seq(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| scalar(s))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Literals YAML would misread as booleans. YAML 1.1's set is larger than 1.2's;
/// this covers both.
const BOOLISH: &[&str] = &["y", "n", "yes", "no", "true", "false", "on", "off"];

/// Literals that would be misread as null.
const NULLISH: &[&str] = &["null", "~"];

/// Renders a scalar, quoting it when necessary.
fn scalar(s: &str) -> String {
    if needs_quotes(s) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_owned()
    }
}

fn needs_quotes(s: &str) -> bool {
    if s.is_empty() || s != s.trim() {
        return true;
    }
    let lower = s.to_ascii_lowercase();
    if BOOLISH.contains(&lower.as_str()) || NULLISH.contains(&lower.as_str()) {
        return true;
    }
    // Strings that look like numbers.
    if s.parse::<f64>().is_ok() || s.parse::<i64>().is_ok() {
        return true;
    }
    // Characters that would affect YAML structure.
    if s.contains([
        ':', '#', '\n', '\r', '\t', '"', '\'', ',', '[', ']', '{', '}',
    ]) {
        return true;
    }
    matches!(
        s.chars().next(),
        Some('-' | '?' | '&' | '*' | '!' | '|' | '>' | '%' | '@' | '`')
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A formatter that changes what the code does is worse than no formatter.
    /// Trailing spaces on a line inside a multiline T-SQL literal are part of
    /// the string, so `fmt` has to leave them where their author put them —
    /// they look exactly like whitespace to tidy up, which is the trap.
    #[test]
    fn trailing_spaces_inside_a_literal_survive_formatting() {
        let module = pbps_model::Module {
            kind: pbps_model::ModuleKind::View,
            description: None,
            definition: "SELECT 'first  \nsecond' AS note".into(),
        };
        let name: pbps_model::ObjectName = "dbo.v".parse().unwrap();
        let out = render_module(&ModuleId::Named(name), &module, &Default::default());
        assert!(out.contains("SELECT 'first  "), "{out}");

        let back = crate::load_module_str(Path::new("dbo.v.yml"), &out)
            .unwrap_or_else(|e| panic!("the rendered file failed to load: {e:?}"));
        assert_eq!(back.module.definition, module.definition);
    }

    fn round_trip(yaml: &str) {
        let a = crate::load_table_str(Path::new("t.yml"), yaml)
            .unwrap_or_else(|e| panic!("the original file failed to load: {e:?}"));
        let out = render(&a.name, &a.table, &a.intents, a.strategy.as_ref());
        let b = crate::load_table_str(Path::new("t.yml"), &out).unwrap_or_else(|e| {
            panic!("the rewritten file does not read back: {e:?}\noutput:\n{out}")
        });
        assert_eq!(a.name, b.name, "output:\n{out}");
        assert_eq!(a.table, b.table, "output:\n{out}");
        assert_eq!(a.intents, b.intents, "output:\n{out}");

        // Idempotence: formatting an already-formatted file must change nothing.
        let out2 = render(&b.name, &b.table, &b.intents, b.strategy.as_ref());
        assert_eq!(out, out2, "fmt is not idempotent");
    }

    fn module_round_trip(yaml: &str) -> String {
        let a = crate::load_module_str(Path::new("m.yml"), yaml)
            .unwrap_or_else(|e| panic!("the original file failed to load: {e:?}"));
        let out = render_module(&a.id, &a.module, &a.depends_on);
        let b = crate::load_module_str(Path::new("m.yml"), &out).unwrap_or_else(|e| {
            panic!("the rewritten file does not read back: {e:?}\noutput:\n{out}")
        });
        assert_eq!(a.id, b.id, "output:\n{out}");
        assert_eq!(a.module, b.module, "output:\n{out}");
        assert_eq!(a.depends_on, b.depends_on, "output:\n{out}");
        assert_eq!(
            render_module(&b.id, &b.module, &b.depends_on),
            out,
            "fmt is not idempotent"
        );
        out
    }

    /// Measured on SQL Server 2025: `CREATE ROLE [ app_pad ]` stores the name
    /// with its padding, and `pull` writes it back quoted, because
    /// `needs_quotes` refuses any scalar that is not its own `trim()`. The
    /// loader then trimmed it, so a freshly pulled project named a role the
    /// database does not have while the ids file named the one it does — an
    /// ambiguous replacement rather than a clean plan (DECISIONS 177).
    #[test]
    fn a_role_name_keeps_the_whitespace_the_database_gave_it() {
        for name in [" app_pad ", "trail ", " lead"] {
            let out = render_role(name, &pbps_model::Role::default(), &[]);
            let back = crate::load_role_str(Path::new("r.yml"), &out)
                .unwrap_or_else(|e| panic!("the rendered file failed to load: {e:?}\n{out}"));
            assert_eq!(back.name, name, "output:\n{out}");
        }

        // The rename intent names the *old* spelling, and it is the same kind
        // of name: trimmed, it would ask the database to rename a role that is
        // not there.
        let out = render_role(
            "kept",
            &pbps_model::Role::default(),
            &[pbps_model::Intent::RenameRole {
                from: " was ".into(),
                to: "kept".into(),
            }],
        );
        let back = crate::load_role_str(Path::new("r.yml"), &out)
            .unwrap_or_else(|e| panic!("the rendered file failed to load: {e:?}\n{out}"));
        assert_eq!(
            back.intents,
            [pbps_model::Intent::RenameRole {
                from: " was ".into(),
                to: "kept".into(),
            }],
            "output:\n{out}"
        );

        // A name that is nothing but whitespace is still no name.
        let out = render_role("   ", &pbps_model::Role::default(), &[]);
        assert!(
            crate::load_role_str(Path::new("r.yml"), &out).is_err(),
            "output:\n{out}"
        );
    }

    /// 177 one crate over, and the half my sweep missed: `GrantTarget` trimmed
    /// too. Measured on SQL Server 2025: `CREATE SCHEMA [ app]` keeps its
    /// padding, and `pull` renders the target as a quoted `"schema:: app"` —
    /// so a trimmed reload targets a schema that is not there (DECISIONS 178).
    #[test]
    fn a_grant_target_keeps_the_whitespace_the_database_gave_it() {
        let mut role = pbps_model::Role::default();
        let one =
            |p| -> std::collections::BTreeSet<pbps_model::Permission> { [p].into_iter().collect() };
        role.grants.insert(
            pbps_model::GrantTarget::Schema(" app".to_owned()),
            one(pbps_model::Permission::Select),
        );
        role.grants.insert(
            pbps_model::GrantTarget::Object(pbps_model::TableName::new(" app", " t ")),
            one(pbps_model::Permission::Insert),
        );

        let out = render_role("r", &role, &[]);
        let back = crate::load_role_str(Path::new("r.yml"), &out)
            .unwrap_or_else(|e| panic!("the rendered file failed to load: {e:?}\n{out}"));
        assert_eq!(back.role.grants, role.grants, "output:\n{out}");

        // A target that is nothing but the prefix is still no target.
        assert!(
            "schema::   ".parse::<pbps_model::GrantTarget>().is_err(),
            "an empty schema name"
        );
    }

    /// The definition is SQL, and SQL is exactly the kind of text YAML quoting
    /// mangles: a `#`, a `:` or a leading `-` in the wrong place would come
    /// back as something else. A literal block keeps it verbatim.
    #[test]
    fn a_view_round_trips_with_its_sql_intact() {
        let out = module_round_trip(
            "view: dbo.active_customer\ndescription: Customers that are not legacy records\ndefinition: |-\n  SELECT customer_id, full_name  -- the columns callers use\n  FROM dbo.customer\n  WHERE legacy_code IS NULL\n",
        );
        assert!(out.contains("definition: |-"), "{out}");
        assert!(out.contains("  WHERE legacy_code IS NULL"), "{out}");
    }

    /// `depends_on` is persistent, like `strategy:` — an answer about the
    /// project that stays true, so rewriting the file must not lose it.
    #[test]
    fn a_modules_persistent_annotations_survive_formatting() {
        let out = module_round_trip(
            "view: dbo.top\ndepends_on: [dbo.middle, dbo.base]\ndefinition: |-\n  SELECT 1\n",
        );
        assert!(out.contains("depends_on: [dbo.base, dbo.middle]"), "{out}");
    }

    #[test]
    fn a_trigger_keeps_the_table_it_is_on() {
        let out = module_round_trip(
            "trigger: dbo.trg_customer_audit\non: dbo.customer\ndefinition: |-\n  AFTER INSERT\n  AS INSERT INTO dbo.audit (n) SELECT COUNT(*) FROM inserted;\n",
        );
        assert!(
            out.starts_with("trigger: dbo.trg_customer_audit\non: dbo.customer\n"),
            "{out}"
        );
    }

    /// A routine's signature is part of its name, so `fmt` writes it back on
    /// the leading key — and a file already in canonical form is a fixed
    /// point. `int, text` is canonicalized to `int,text` the way every other
    /// spelling `fmt` owns is; what the *engine* calls those types is the
    /// dialect's answer, applied one layer up (ADR-0009 §1).
    #[test]
    fn a_routine_keeps_its_signature_through_formatting() {
        let out = module_round_trip(
            "function: app.f(int, text)\ndefinition: |-\n  (a integer, b text) RETURNS int AS $$ SELECT 1 $$\n",
        );
        // Quoted, because the comma in the signature is one of the
        // characters that would otherwise change YAML's reading of the line —
        // the same rule that already quotes a name containing a `:`.
        assert!(out.starts_with("function: \"app.f(int,text)\"\n"), "{out}");
        assert!(out.contains("(a integer, b text)"), "{out}");

        // And two overloads keep their own bodies rather than one file
        // rewriting the other.
        let other = module_round_trip(
            "function: app.f(bigint)\ndefinition: |-\n  (a bigint) RETURNS int AS $$ SELECT 2 $$\n",
        );
        assert!(other.starts_with("function: app.f(bigint)\n"), "{other}");

        // And a `depends_on:` naming an overload survives the flow sequence
        // it is written in, which is where an unquoted comma would split one
        // name into two.
        let dependent = module_round_trip(
            "view: app.v\ndepends_on: [\"app.f(int,text)\"]\ndefinition: |-\n  SELECT 1\n",
        );
        assert!(
            dependent.contains("depends_on: [\"app.f(int,text)\"]"),
            "{dependent}"
        );
    }

    #[test]
    fn full_document_round_trips() {
        round_trip(
            r#"
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
    renamed_from: customer_name
  region_id:
    type: int
  balance:
    type: bigint
    nullable: false
    default: "0"
  legacy:
    type: varchar(20)
    deprecated: superseded by email as the identifier
primary_key: [customer_id]
unique:
  uq_a: [region_id]
foreign_keys:
  fk_region:
    columns: [region_id]
    references: dbo.region(region_id)
    on_delete: cascade
checks:
  ck_balance: balance >= 0
indexes:
  ix_name:
    columns: [full_name, customer_id desc]
    include: [region_id]
    unique: true
    where: legacy IS NULL
"#,
        );
    }

    #[test]
    fn named_primary_key_round_trips() {
        round_trip(
            "table: dbo.t\ncolumns:\n  a: {type: int}\nprimary_key:\n  name: pk_t\n  columns: [a]\n",
        );
    }

    #[test]
    fn table_rename_annotation_round_trips() {
        round_trip("table: dbo.b\nrenamed_from: dbo.a\ncolumns:\n  a: {type: int}\n");
    }

    /// This is why the quoting rules exist: a column named `no` with a default of
    /// `yes` would come back as booleans on the next read if left unquoted.
    #[test]
    fn boolish_scalars_survive_a_round_trip() {
        round_trip(
            "table: dbo.t\ncolumns:\n  \"no\":\n    type: int\n    default: \"yes\"\n    description: \"off\"\n",
        );
    }

    #[test]
    fn nullish_and_numeric_scalars_survive_a_round_trip() {
        round_trip(
            "table: dbo.t\ncolumns:\n  a:\n    type: int\n    default: \"null\"\n    description: \"0123\"\n  b:\n    type: int\n    default: \"~\"\n",
        );
    }

    #[test]
    fn expressions_with_structural_characters_survive() {
        round_trip(
            "table: dbo.t\ncolumns:\n  a: {type: int}\nchecks:\n  ck: \"a > 0 AND a < 100\"\n  ck2: \"a IN (1, 2, 3)\"\n",
        );
    }

    #[test]
    fn quoting_decisions_are_as_expected() {
        assert_eq!(scalar("customer_id"), "customer_id");
        assert_eq!(scalar("nvarchar(100)"), "nvarchar(100)");
        assert_eq!(scalar("Café clientèle"), "Café clientèle");

        assert_eq!(scalar("no"), "\"no\"");
        assert_eq!(scalar("YES"), "\"YES\"");
        assert_eq!(scalar("null"), "\"null\"");
        assert_eq!(scalar("~"), "\"~\"");
        assert_eq!(scalar("0123"), "\"0123\"");
        assert_eq!(scalar("1.5"), "\"1.5\"");
        assert_eq!(scalar(""), "\"\"");
        assert_eq!(scalar("a: b"), "\"a: b\"");
        assert_eq!(scalar("- x"), "\"- x\"");
        assert_eq!(scalar(" x"), "\" x\"");
    }

    /// Output must be stable, or fmt would manufacture a phantom git diff on
    /// every run.
    #[test]
    fn output_is_stable() {
        let yaml = "table: dbo.t\ncolumns:\n  b: {type: int}\n  a: {type: int}\n";
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        let first = render(&t.name, &t.table, &t.intents, None);
        for _ in 0..10 {
            assert_eq!(render(&t.name, &t.table, &t.intents, None), first);
        }
        // Column order follows the declaration; nothing is reordered.
        assert!(first.find("  b:").unwrap() < first.find("  a:").unwrap());
    }
}

#[cfg(test)]
mod strategy_tests {
    use super::*;
    use std::path::Path;

    fn load(text: &str) -> crate::LoadedTable {
        crate::load_table_str(Path::new("t.yml"), text).expect("should load")
    }

    const WITH_STRATEGY: &str =
        "table: dbo.order_line\nstrategy:\n  online: true\ncolumns:\n  id: {type: bigint}\n";

    #[test]
    fn a_strategy_block_is_read_and_kept_out_of_the_model() {
        let t = load(WITH_STRATEGY);
        assert_eq!(t.strategy, Some(Strategy { online: true }));
        // The whole point: the table itself is indistinguishable from one
        // declared without a strategy, so Schema equality is unaffected.
        let plain = load("table: dbo.order_line\ncolumns:\n  id: {type: bigint}\n");
        assert_eq!(t.table, plain.table);
        assert_eq!(plain.strategy, None);
    }

    /// Unlike `renamed_from`, a strategy is persistent: rewriting the file must
    /// not silently turn an online alter into a blocking one.
    #[test]
    fn fmt_preserves_a_strategy() {
        let t = load(WITH_STRATEGY);
        let out = render(&t.name, &t.table, &t.intents, t.strategy.as_ref());
        assert!(out.contains("strategy:\n  online: true\n"), "{out}");

        let again = load(&out);
        assert_eq!(again.strategy, t.strategy);
        assert_eq!(
            render(
                &again.name,
                &again.table,
                &again.intents,
                again.strategy.as_ref()
            ),
            out,
            "rendering must be a fixpoint"
        );
    }

    /// The default renders as nothing, or every pulled file would grow a block
    /// saying "do the ordinary thing".
    #[test]
    fn a_default_strategy_renders_no_block() {
        let t = load("table: dbo.t\nstrategy:\n  online: false\ncolumns:\n  id: {type: int}\n");
        let out = render(&t.name, &t.table, &t.intents, t.strategy.as_ref());
        assert!(!out.contains("strategy"), "{out}");
    }

    /// ADR-0003: a typo must not silently become a no-op, leaving the user
    /// believing a large table is being altered online when it is not.
    #[test]
    fn an_unknown_strategy_key_is_rejected() {
        let e = crate::load_table_str(
            Path::new("t.yml"),
            "table: dbo.t\nstrategy:\n  onlnie: true\ncolumns:\n  id: {type: int}\n",
        )
        .expect_err("a misspelled key must not be accepted");
        assert!(
            render_errors(&e).contains("onlnie"),
            "the error must name the offending key: {}",
            render_errors(&e)
        );
    }

    #[test]
    fn a_data_block_round_trips() {
        let yaml = "table: dbo.order_status\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  label: {type: nvarchar(50), nullable: false}\n\nprimary_key: [code]\n\ndata:\n  mode: exact\n  rows:\n    cancelled: {label: Cancelled}\n    new: {label: New}\n";
        let a = load(yaml);
        let out = render(&a.name, &a.table, &a.intents, a.strategy.as_ref());
        let b = load(&out);
        assert_eq!(a.table, b.table, "output:\n{out}");
        // Idempotent, like every other block.
        assert_eq!(
            out,
            render(&b.name, &b.table, &b.intents, b.strategy.as_ref())
        );
    }

    /// The trap this format has that no other block here does: reference data
    /// is exactly where boolean-ish and number-shaped *codes* live. `no` is a
    /// real status code, `1` is a real key, and YAML reads both as something
    /// else unless `fmt` quotes them.
    #[test]
    fn boolish_and_numeric_row_keys_survive_a_round_trip() {
        let t = load(
            "table: dbo.answer\ncolumns:\n  code: {type: varchar(3), nullable: false}\n  n: {type: int}\nprimary_key: [code]\ndata:\n  mode: ensure\n  rows:\n    \"no\": {n: 0}\n    \"1\": {n: 1}\n",
        );
        let out = render(&t.name, &t.table, &t.intents, t.strategy.as_ref());
        assert!(
            out.contains("\"no\":"),
            "an unquoted `no` key is a boolean:\n{out}"
        );
        assert!(
            out.contains("\"1\":"),
            "an unquoted `1` key is a number:\n{out}"
        );
        let again = load(&out);
        assert_eq!(t.table, again.table);
    }

    /// A quoted decimal must stay quoted, or the next load hits the refusal in
    /// `convert_data` — `fmt` would have broken a file that was valid when it
    /// arrived.
    #[test]
    fn a_quoted_decimal_stays_quoted() {
        let t = load(
            "table: dbo.rate\ncolumns:\n  code: {type: varchar(3), nullable: false}\n  pct: {type: \"decimal(5,2)\"}\nprimary_key: [code]\ndata:\n  mode: exact\n  rows:\n    std: {pct: \"1.50\"}\n",
        );
        let out = render(&t.name, &t.table, &t.intents, t.strategy.as_ref());
        assert!(out.contains("\"1.50\""), "{out}");
        assert_eq!(t.table, load(&out).table);
    }

    /// The negative case for the whole block: an unquoted decimal is refused,
    /// and the message says what to do rather than quietly rounding it.
    #[test]
    fn a_bare_decimal_is_refused_with_the_remedy() {
        let errs = crate::load_table_str(
            Path::new("t.yml"),
            "table: dbo.rate\ncolumns:\n  code: {type: varchar(3), nullable: false}\n  pct: {type: \"decimal(5,2)\"}\nprimary_key: [code]\ndata:\n  mode: exact\n  rows:\n    std: {pct: 1.50}\n",
        )
        .unwrap_err();
        let text = render_errors(&errs);
        assert!(text.contains("an unquoted decimal"), "{text}");
        assert!(text.contains("pct"), "the column must be named: {text}");
        // And it must not echo the number back — by then it has already been
        // through `f64`, so `1.50` would print as `1.5`.
        assert!(
            !text.contains("1.5"),
            "the diagnostic rounded the value: {text}"
        );
    }

    #[test]
    fn an_unknown_data_mode_is_refused() {
        let errs = crate::load_table_str(
            Path::new("t.yml"),
            "table: dbo.t\ncolumns:\n  a: {type: int, nullable: false}\nprimary_key: [a]\ndata:\n  mode: exactly\n  rows: {}\n",
        )
        .unwrap_err();
        let text = render_errors(&errs);
        assert!(text.contains("unknown data mode"), "{text}");
    }

    #[test]
    fn a_table_with_no_data_block_renders_none() {
        let t = load("table: dbo.t\ncolumns:\n  a: {type: int}\n");
        assert!(t.table.data.is_none());
        let out = render(&t.name, &t.table, &t.intents, t.strategy.as_ref());
        assert!(!out.contains("data:"), "{out}");
    }

    fn render_errors(errs: &[crate::LoadError]) -> String {
        errs.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }
}
