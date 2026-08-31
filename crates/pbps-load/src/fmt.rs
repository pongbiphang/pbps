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

use pbps_model::{Intent, PrimaryKey, Strategy, Table, TableName};

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

    fn render_errors(errs: &[crate::LoadError]) -> String {
        errs.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }
}
