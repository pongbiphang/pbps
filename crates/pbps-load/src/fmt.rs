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

use pbps_model::{
    Clustered, Intent, Module, ModuleId, ObjectName, PrimaryKey, ReplicaIdentity, Strategy, Table,
    TableName,
};

/// Renders one table as canonical YAML.
///
/// Renames in `intents` that concern this table are written back out as
/// `renamed_from` annotations: they are one-shot input and do not live in the
/// model, but rewriting the file must not lose them. The caller decides which
/// intents still belong in the file — `pbps fmt` passes only the ones not yet
/// absorbed into the ids file, which is how a redundant annotation gets
/// stripped (SPEC §6.2).
/// Storage parameters as a flow mapping, `{fillfactor: 70, …}`, each value
/// in its canonical spelling and bare, as `storage_parameters:` writes them
/// (#1441, #1442).
fn flow(parameters: &std::collections::BTreeMap<String, String>) -> String {
    let pairs: Vec<String> = parameters
        .iter()
        .map(|(name, value)| format!("{}: {value}", scalar(name)))
        .collect();
    format!("{{{}}}", pairs.join(", "))
}

pub fn render(
    name: &TableName,
    table: &Table,
    intents: &[Intent],
    strategy: Option<&Strategy>,
) -> String {
    render_partitioned(name, table, &[], intents, strategy)
}

/// [`render`], for a partitioned parent with its partitions, which are
/// written in its file (#1170). Each is named bare in the parent's schema,
/// qualified in another, in name order.
pub fn render_partitioned(
    name: &TableName,
    table: &Table,
    partitions: &[(&TableName, &Table)],
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
        if let Some(g) = &c.generated {
            let _ = writeln!(
                s,
                "    generated: {{expression: {}, stored: {}}}",
                scalar(&g.expression),
                g.stored
            );
        }
        if let Some(collation) = &c.collation {
            let _ = writeln!(s, "    collation: {}", scalar(collation.as_str()));
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

    if !table.computed.is_empty() {
        s.push_str("\ncomputed:\n");
        for (col_name, c) in &table.computed {
            let _ = writeln!(
                s,
                "  {}: {{expression: {}{}{}}}",
                scalar(col_name),
                scalar(&c.expression),
                if c.persisted { ", persisted: true" } else { "" },
                if c.not_null { ", not_null: true" } else { "" }
            );
        }
    }

    if let Some(pk) = &table.primary_key {
        s.push('\n');
        match pk {
            PrimaryKey {
                name: None,
                columns,
                storage_parameters,
            } if storage_parameters.is_empty() => {
                let _ = writeln!(s, "primary_key: {}", seq(columns));
            }
            PrimaryKey {
                name,
                columns,
                storage_parameters,
            } => {
                let _ = writeln!(s, "primary_key:");
                if let Some(n) = name {
                    let _ = writeln!(s, "  name: {}", scalar(n));
                }
                let _ = writeln!(s, "  columns: {}", seq(columns));
                if !storage_parameters.is_empty() {
                    let _ = writeln!(s, "  storage_parameters: {}", flow(storage_parameters));
                }
            }
        }
    }

    if !table.unique.is_empty() {
        s.push_str("\nunique:\n");
        for (n, u) in &table.unique {
            if u.storage_parameters.is_empty() {
                let _ = writeln!(s, "  {}: {}", scalar(n), seq(&u.columns));
            } else {
                let _ = writeln!(
                    s,
                    "  {}: {{columns: {}, storage_parameters: {}}}",
                    scalar(n),
                    seq(&u.columns),
                    flow(&u.storage_parameters)
                );
            }
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

    render_checks_and_indexes(&mut s, table, "", "\n");

    // After the indexes it may name, and on one line: a layout is a single
    // choice, not a block.
    match &table.clustered {
        None => {}
        Some(Clustered::Heap) => s.push_str("\nclustered: heap\n"),
        Some(Clustered::Unique(n)) => {
            let _ = writeln!(s, "\nclustered: {{unique: {}}}", scalar(n));
        }
        Some(Clustered::Index(n)) => {
            let _ = writeln!(s, "\nclustered: {{index: {}}}", scalar(n));
        }
    }
    // The same, for the same reason.
    match &table.replica_identity {
        None => {}
        Some(ReplicaIdentity::Full) => s.push_str("\nreplica_identity: full\n"),
        Some(ReplicaIdentity::Nothing) => s.push_str("\nreplica_identity: nothing\n"),
        Some(ReplicaIdentity::PrimaryKey) => s.push_str("\nreplica_identity: primary_key\n"),
        Some(ReplicaIdentity::Unique(n)) => {
            let _ = writeln!(s, "\nreplica_identity: {{unique: {}}}", scalar(n));
        }
        Some(ReplicaIdentity::Index(n)) => {
            let _ = writeln!(s, "\nreplica_identity: {{index: {}}}", scalar(n));
        }
    }
    // Only where it is not the default (#1443).
    if table.unlogged {
        s.push_str("\nunlogged: true\n");
    }
    if let Some(by) = &table.partition_by {
        let _ = writeln!(s, "\npartition_by: {}", seq(&by.columns));
        if !partitions.is_empty() {
            s.push_str("\npartitions:\n");
            let mut sorted: Vec<&(&TableName, &Table)> = partitions.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            for (child, t) in sorted {
                let label = if child.schema == name.schema {
                    child.name.clone()
                } else {
                    child.to_string()
                };
                let list = |d: &[pbps_model::BoundDatum]| {
                    d.iter()
                        .map(|v| match v {
                            pbps_model::BoundDatum::Value(text) => bound_value(text),
                            end @ (pbps_model::BoundDatum::MinValue
                            | pbps_model::BoundDatum::MaxValue) => end.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let bound = t.partition_of.as_ref().map(|p| &p.bound);
                // On one line while the partition is its bound alone; a block
                // once it has checks or indexes of its own (#1577).
                if t.checks.is_empty() && t.indexes.is_empty() {
                    let bound = match bound {
                        Some(pbps_model::PartitionBound::Range { from, to }) => {
                            format!("{{from: [{}], to: [{}]}}", list(from), list(to))
                        }
                        Some(pbps_model::PartitionBound::Default) | None => "default".to_owned(),
                    };
                    let _ = writeln!(s, "  {}: {bound}", scalar(&label));
                } else {
                    let _ = writeln!(s, "  {}:", scalar(&label));
                    match bound {
                        Some(pbps_model::PartitionBound::Range { from, to }) => {
                            let _ =
                                writeln!(s, "    from: [{}]\n    to: [{}]", list(from), list(to));
                        }
                        Some(pbps_model::PartitionBound::Default) | None => {
                            s.push_str("    default: true\n");
                        }
                    }
                    render_checks_and_indexes(&mut s, t, "    ", "");
                }
            }
        }
    }
    // After the columns it names (#1176).
    if let Some(st) = &table.system_time {
        let _ = writeln!(
            s,
            "\nsystem_time:\n  period: {}",
            seq(&[st.start.clone(), st.end.clone()])
        );
        if st.hidden {
            s.push_str("  hidden: true\n");
        }
        if let Some(v) = &st.versioning {
            let _ = writeln!(
                s,
                "  versioning:\n    history: {}",
                scalar(&v.history.to_string())
            );
            if let Some(r) = &v.retention {
                let _ = writeln!(s, "    retention: {}", scalar(&r.to_string()));
            }
        }
    }
    // One parameter a line, by name, each in its canonical spelling, bare:
    // every canonical value is a plain token (`true`, `70`, `0.05`, `auto`),
    // and quoted it would read as text, which is the same value but not
    // what anyone writes (#1441).
    if !table.storage_parameters.is_empty() {
        s.push_str("\nstorage_parameters:\n");
        for (name, value) in &table.storage_parameters {
            let _ = writeln!(s, "  {}: {value}", scalar(name));
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
/// project that stays true, so rewriting the file must not lose it. So is
/// `public_execute`, and losing that one would not merely reorder a plan: the
/// next one would take the engine's default `EXECUTE` away from a routine the
/// declaration had asked to leave open (ADR-0010 §5).
pub fn render_module(
    id: &ModuleId,
    module: &Module,
    depends_on: &std::collections::BTreeSet<ModuleId>,
    public_execute: bool,
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
    // Written only when it is `true`. `false` is what the absent key means,
    // and a formatter that spelled the default out would put the line into
    // every view and trigger file in the project, where the loader refuses it.
    if public_execute {
        let _ = writeln!(s, "public_execute: true");
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

/// A bound value, always double-quoted: it is the engine's text for a value
/// of any key type, and bare it would be read by YAML's rules, `0x1F` as 31
/// and `MINVALUE` as the unbounded end (#1170).
fn bound_value(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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

/// A table's `checks:` and `indexes:`, each line under `pad`: the top level
/// of a table's file, or a partition's entry under its parent's `partitions:`
/// (#1577). `lead` goes before each section's first line, the blank line
/// between top-level sections.
fn render_checks_and_indexes(s: &mut String, table: &Table, pad: &str, lead: &str) {
    if !table.checks.is_empty() {
        let _ = writeln!(s, "{lead}{pad}checks:");
        for (n, c) in &table.checks {
            let _ = writeln!(s, "{pad}  {}: {}", scalar(n), scalar(&c.expression));
        }
    }

    if !table.indexes.is_empty() {
        let _ = writeln!(s, "{lead}{pad}indexes:");
        for (n, ix) in &table.indexes {
            let _ = writeln!(s, "{pad}  {}:", scalar(n));
            if !ix.method.is_btree() {
                let _ = writeln!(s, "{pad}    method: {}", ix.method.as_str());
            }
            if !ix.storage_parameters.is_empty() {
                let _ = writeln!(
                    s,
                    "{pad}    storage_parameters: {}",
                    flow(&ix.storage_parameters)
                );
            }
            // `columns:` while every key is a column, as every index was
            // written before expressions; `keys:`, one mapping each, once any
            // is an expression (DEC-1169.2).
            if ix.columns.iter().all(|c| c.key.column().is_some()) {
                let cols: Vec<String> = ix
                    .columns
                    .iter()
                    .map(|c| {
                        let mut spelled = c.key.text().to_owned();
                        if let Some(class) = &c.opclass {
                            spelled = format!("{spelled} {class}");
                        }
                        if c.descending {
                            spelled.push_str(" desc");
                        }
                        spelled
                    })
                    .collect();
                let _ = writeln!(s, "{pad}    columns: {}", seq(&cols));
            } else {
                let _ = writeln!(s, "{pad}    keys:");
                for c in &ix.columns {
                    let (field, text) = match &c.key {
                        pbps_model::IndexKey::Column(name) => ("column", name),
                        pbps_model::IndexKey::Expression(text) => ("expression", text),
                    };
                    let _ = write!(s, "{pad}      - {field}: {}", scalar(text));
                    if let Some(class) = &c.opclass {
                        let _ = write!(s, "\n{pad}        opclass: {}", scalar(class));
                    }
                    if c.descending {
                        let _ = write!(s, "\n{pad}        order: desc");
                    }
                    s.push('\n');
                }
            }
            if !ix.include.is_empty() {
                let _ = writeln!(s, "{pad}    include: {}", seq(&ix.include));
            }
            if ix.unique {
                let _ = writeln!(s, "{pad}    unique: true");
            }
            if let Some(f) = &ix.filter {
                let _ = writeln!(s, "{pad}    where: {}", scalar(f));
            }
        }
    }
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
        let out = render_module(&ModuleId::Named(name), &module, &Default::default(), false);
        assert!(out.contains("SELECT 'first  "), "{out}");

        let back = crate::load_module_str(Path::new("dbo.v.yml"), &out)
            .unwrap_or_else(|e| panic!("the rendered file failed to load: {e:?}"));
        assert_eq!(back.module.definition, module.definition);
    }

    fn round_trip(yaml: &str) {
        let a = crate::load_table_str(Path::new("t.yml"), yaml)
            .unwrap_or_else(|e| panic!("the original file failed to load: {e:?}"));
        let render_all = |t: &crate::LoadedTable| {
            let partitions: Vec<_> = t.partitions.iter().map(|(n, p)| (n, p)).collect();
            render_partitioned(
                &t.name,
                &t.table,
                &partitions,
                &t.intents,
                t.strategy.as_ref(),
            )
        };
        let out = render_all(&a);
        let b = crate::load_table_str(Path::new("t.yml"), &out).unwrap_or_else(|e| {
            panic!("the rewritten file does not read back: {e:?}\noutput:\n{out}")
        });
        assert_eq!(a.name, b.name, "output:\n{out}");
        assert_eq!(a.table, b.table, "output:\n{out}");
        assert_eq!(a.intents, b.intents, "output:\n{out}");
        assert_eq!(a.partitions, b.partitions, "output:\n{out}");

        // Idempotence: formatting an already-formatted file must change nothing.
        let out2 = render_all(&b);
        assert_eq!(out, out2, "fmt is not idempotent");
    }

    fn module_round_trip(yaml: &str) -> String {
        let a = crate::load_module_str(Path::new("m.yml"), yaml)
            .unwrap_or_else(|e| panic!("the original file failed to load: {e:?}"));
        let out = render_module(&a.id, &a.module, &a.depends_on, a.public_execute);
        let b = crate::load_module_str(Path::new("m.yml"), &out).unwrap_or_else(|e| {
            panic!("the rewritten file does not read back: {e:?}\noutput:\n{out}")
        });
        assert_eq!(a.id, b.id, "output:\n{out}");
        assert_eq!(a.module, b.module, "output:\n{out}");
        assert_eq!(a.depends_on, b.depends_on, "output:\n{out}");
        assert_eq!(a.public_execute, b.public_execute, "output:\n{out}");
        assert_eq!(
            render_module(&b.id, &b.module, &b.depends_on, b.public_execute),
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

    /// So is `public_execute:`, and losing it costs more than an order: the
    /// next plan would take the engine's default `EXECUTE` away from a
    /// routine the declaration had asked to leave open (issue #318). The
    /// closed answer is the absent key, so `fmt` writes nothing for it —
    /// `module_round_trip` is what holds both directions, by loading the
    /// output back and comparing.
    #[test]
    fn a_routines_public_execute_survives_formatting_and_the_default_writes_nothing() {
        let open = module_round_trip(
            "function: app.f(int)\npublic_execute: true\ndefinition: |-\n  (a integer) RETURNS int AS $$ SELECT 1 $$\n",
        );
        assert!(open.contains("public_execute: true"), "{open}");
        let closed = module_round_trip(
            "function: app.f(int)\ndefinition: |-\n  (a integer) RETURNS int AS $$ SELECT 1 $$\n",
        );
        assert!(!closed.contains("public_execute"), "{closed}");
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
    /// Each form of the layout selector reads back as the model value it
    /// names and renders to the same text again (#1178).
    #[test]
    fn every_clustered_layout_round_trips() {
        let base = "table: dbo.t\ncolumns:\n  id: {type: int, nullable: false}\n  code: {type: int, nullable: false}\n\nprimary_key: [id]\n\nunique:\n  uq_code: [code]\n\nindexes:\n  ix_code:\n    columns: [code]\n";
        for (line, expected) in [
            ("clustered: heap", pbps_model::Clustered::Heap),
            (
                "clustered: {unique: uq_code}",
                pbps_model::Clustered::Unique("uq_code".into()),
            ),
            (
                "clustered: {index: ix_code}",
                pbps_model::Clustered::Index("ix_code".into()),
            ),
        ] {
            let yaml = format!("{base}\n{line}\n");
            round_trip(&yaml);
            let t = crate::load_table_str(Path::new("t.yml"), &yaml).unwrap();
            assert_eq!(t.table.clustered, Some(expected), "{yaml}");
            let out = render(&t.name, &t.table, &t.intents, None);
            assert!(out.contains(&format!("\n{line}\n")), "{out}");
        }
        // The default says nothing, so a pulled table in the default layout
        // grows no line.
        let t = crate::load_table_str(Path::new("t.yml"), base).unwrap();
        assert_eq!(t.table.clustered, None);
        assert!(!render(&t.name, &t.table, &t.intents, None).contains("clustered"));
    }

    /// An explicit collation reads back as written and renders again; absent
    /// renders nothing (#1175).
    #[test]
    fn a_column_collation_round_trips_and_its_absence_writes_nothing() {
        let yaml = "table: dbo.t\ncolumns:\n  code:\n    type: varchar(10)\n    collation: Latin1_General_CS_AS\n  note: {type: varchar(10)}\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        assert_eq!(
            t.table.columns["code"].collation,
            Some(pbps_model::Collation::new("latin1_general_cs_as")),
            "compared without case"
        );
        assert_eq!(t.table.columns["note"].collation, None);
        let out = render(&t.name, &t.table, &t.intents, None);
        assert!(
            out.contains("    collation: Latin1_General_CS_AS\n"),
            "{out}"
        );
        assert_eq!(out.matches("collation").count(), 1, "{out}");
    }

    /// A generation expression reads back with its kind and renders again,
    /// and the kind is required: unstated, it would be a virtual column on
    /// PostgreSQL 18 (DEC-1168.1).
    #[test]
    fn a_generated_column_round_trips_and_its_kind_is_required() {
        let yaml = "table: app.t\ncolumns:\n  a: {type: integer}\n  b:\n    type: integer\n    generated: {expression: 'a * 2', stored: true}\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        assert_eq!(
            t.table.columns["b"].generated,
            Some(pbps_model::Generated {
                expression: "a * 2".into(),
                stored: true
            })
        );
        assert_eq!(t.table.columns["b"].default, None);
        assert_eq!(t.table.columns["a"].generated, None);
        let unstated = "table: app.t\ncolumns:\n  a: {type: integer}\n  b:\n    type: integer\n    generated: {expression: 'a * 2'}\n";
        assert!(crate::load_table_str(Path::new("t.yml"), unstated).is_err());
    }

    /// A computed column reads back with its expression and persisted state
    /// and renders again, in its own section: it is not a column, and a
    /// `type` on it is a load error (#1174).
    #[test]
    fn a_computed_column_round_trips_in_its_own_section() {
        let yaml = "table: dbo.t\ncolumns:\n  a: {type: int}\n\ncomputed:\n  c1: {expression: a * 2}\n  c2: {expression: \"concat(a, '-')\", persisted: true}\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        assert_eq!(
            t.table.computed["c2"],
            pbps_model::ComputedColumn {
                expression: "concat(a, '-')".into(),
                persisted: true,
                not_null: false
            }
        );
        assert!(!t.table.computed["c1"].persisted);
        round_trip(
            "table: dbo.t\ncolumns:\n  a: {type: int}\n\ncomputed:\n  c3: {expression: a + 1, persisted: true, not_null: true}\n",
        );
        assert!(!t.table.columns.contains_key("c1"));
        let typed = "table: dbo.t\ncolumns:\n  a: {type: int}\ncomputed:\n  c1: {expression: a * 2, type: int}\n";
        assert!(crate::load_table_str(Path::new("t.yml"), typed).is_err());
    }

    /// A kind the selector does not have is a load error, not a default
    /// layout: a misspelt `clustered: {indx: ix}` read as "absent" would
    /// rebuild the table's key as the clustered one.
    #[test]
    fn an_unknown_clustered_kind_is_rejected() {
        for line in [
            "clustered: {indx: ix_code}",
            "clustered: table",
            "clustered: [ix_code]",
        ] {
            let yaml = format!(
                "table: dbo.t\ncolumns:\n  code: {{type: int}}\nindexes:\n  ix_code:\n    columns: [code]\n{line}\n"
            );
            assert!(
                crate::load_table_str(Path::new("t.yml"), &yaml).is_err(),
                "{line} should not load"
            );
        }
    }

    /// Each form of the replica identity reads back as the model value it
    /// names and renders to the same text again, and the default writes
    /// nothing (#1444).
    #[test]
    fn every_replica_identity_round_trips() {
        use pbps_model::ReplicaIdentity as R;
        let base = "table: public.t\ncolumns:\n  id: {type: int, nullable: false}\n  code: {type: int, nullable: false}\n\nprimary_key: [id]\n\nunique:\n  uq_code: [code]\n\nindexes:\n  ix_code:\n    columns: [code]\n    unique: true\n";
        for (line, expected) in [
            ("replica_identity: full", R::Full),
            ("replica_identity: nothing", R::Nothing),
            ("replica_identity: primary_key", R::PrimaryKey),
            (
                "replica_identity: {unique: uq_code}",
                R::Unique("uq_code".into()),
            ),
            (
                "replica_identity: {index: ix_code}",
                R::Index("ix_code".into()),
            ),
        ] {
            let yaml = format!("{base}\n{line}\n");
            round_trip(&yaml);
            let t = crate::load_table_str(Path::new("t.yml"), &yaml).unwrap();
            assert_eq!(t.table.replica_identity, Some(expected), "{yaml}");
            let out = render(&t.name, &t.table, &t.intents, None);
            assert!(out.contains(&format!("\n{line}\n")), "{out}");
        }
        let t = crate::load_table_str(Path::new("t.yml"), base).unwrap();
        assert_eq!(t.table.replica_identity, None);
        assert!(!render(&t.name, &t.table, &t.intents, None).contains("replica_identity"));
    }

    /// `system_time` reads into the model and renders to the same text, in
    /// each of its shapes: a versioned table with hidden period columns and
    /// a retention, one with neither, and a period alone (#1176).
    #[test]
    fn system_time_round_trips_in_each_shape() {
        let base = "table: dbo.t\ncolumns:\n  id: {type: int, nullable: false}\n  valid_from: {type: datetime2(7), nullable: false}\n  valid_to: {type: datetime2(7), nullable: false}\n\nprimary_key: [id]\n";
        for (block, hidden, history, retention) in [
            (
                "system_time:\n  period: [valid_from, valid_to]\n  hidden: true\n  versioning:\n    history: hist.t_history\n    retention: 6 months\n",
                true,
                Some("hist.t_history"),
                Some("6 months"),
            ),
            (
                "system_time:\n  period: [valid_from, valid_to]\n  versioning:\n    history: dbo.MSSQL_TemporalHistoryFor_42\n",
                false,
                Some("dbo.MSSQL_TemporalHistoryFor_42"),
                None,
            ),
            (
                "system_time:\n  period: [valid_from, valid_to]\n",
                false,
                None,
                None,
            ),
        ] {
            let yaml = format!("{base}\n{block}");
            round_trip(&yaml);
            let t = crate::load_table_str(Path::new("t.yml"), &yaml).unwrap();
            let st = t.table.system_time.clone().expect(block);
            assert_eq!(
                (st.start.as_str(), st.end.as_str()),
                ("valid_from", "valid_to")
            );
            assert_eq!(st.hidden, hidden, "{block}");
            assert_eq!(
                st.versioning.as_ref().map(|v| v.history.to_string()),
                history.map(str::to_owned),
                "{block}"
            );
            assert_eq!(
                st.versioning
                    .as_ref()
                    .and_then(|v| v.retention)
                    .map(|r| r.to_string()),
                retention.map(str::to_owned),
                "{block}"
            );
            let out = render(&t.name, &t.table, &t.intents, None);
            assert!(out.contains(&format!("\n{block}")), "{out}");
        }
        // Negative: absent is an ordinary table, and renders nothing.
        let t = crate::load_table_str(Path::new("t.yml"), base).unwrap();
        assert_eq!(t.table.system_time, None);
        assert!(!render(&t.name, &t.table, &t.intents, None).contains("system_time"));
    }

    /// A `system_time` the loader cannot read is a load error naming what is
    /// wrong, never an ordinary table or an INFINITE retention.
    #[test]
    fn an_unreadable_system_time_is_rejected() {
        for block in [
            "system_time:\n  period: [valid_from]\n",
            "system_time:\n  period: [valid_from, valid_to, extra]\n",
            "system_time:\n  period: [valid_from, valid_to]\n  versioning:\n    history: no_schema\n",
            "system_time:\n  period: [valid_from, valid_to]\n  versioning:\n    history: dbo.h\n    retention: 0 days\n",
            "system_time:\n  period: [valid_from, valid_to]\n  versioning:\n    history: dbo.h\n    retention: forever\n",
            "system_time:\n  period: [valid_from, valid_to]\n  versioning:\n    history: dbo.h\n    retension: 6 months\n",
            "system_time:\n  period: [valid_from, valid_to]\n  history: dbo.h\n",
        ] {
            let yaml = format!(
                "table: dbo.t\ncolumns:\n  valid_from: {{type: datetime2, nullable: false}}\n  valid_to: {{type: datetime2, nullable: false}}\n{block}"
            );
            assert!(
                crate::load_table_str(Path::new("t.yml"), &yaml).is_err(),
                "{block} should not load"
            );
        }
    }

    /// A partition's own checks and indexes are written under its entry, which
    /// becomes a block beside its bound, and read back as its own: the range
    /// and the DEFAULT partition alike (#1577). The parent's stay the parent's.
    #[test]
    fn a_partitions_own_checks_and_indexes_round_trip_under_its_entry() {
        let yaml = "table: app.ev\ncolumns:\n  id: {type: int, nullable: false}\n  ts: {type: date, nullable: false}\n  v: {type: int}\n\nchecks:\n  ev_ck: v > 0\n\nindexes:\n  ev_v:\n    columns: [v]\n\npartition_by: [ts]\n\npartitions:\n  ev_old:\n    from: [MINVALUE]\n    to: [\"2025-01-01\"]\n    checks:\n      old_ck: v < 100\n    indexes:\n      old_v:\n        columns: [v desc]\n        where: v > 1\n  ev_plain: {from: [\"2025-01-01\"], to: [MAXVALUE]}\n  ev_rest:\n    default: true\n    indexes:\n      rest_id:\n        columns: [id]\n        unique: true\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        let own = |n: &str| {
            &t.partitions
                .iter()
                .find(|(name, _)| name.name == n)
                .unwrap_or_else(|| panic!("{n}"))
                .1
        };
        assert_eq!(own("ev_old").checks.keys().collect::<Vec<_>>(), ["old_ck"]);
        assert_eq!(own("ev_old").indexes.keys().collect::<Vec<_>>(), ["old_v"]);
        assert!(own("ev_plain").checks.is_empty() && own("ev_plain").indexes.is_empty());
        assert_eq!(
            own("ev_rest").partition_of.as_ref().map(|p| &p.bound),
            Some(&pbps_model::PartitionBound::Default)
        );
        assert!(own("ev_rest").indexes["rest_id"].unique);
        // The parent's stay on the parent.
        assert_eq!(t.table.checks.keys().collect::<Vec<_>>(), ["ev_ck"]);
        assert_eq!(t.table.indexes.keys().collect::<Vec<_>>(), ["ev_v"]);
        // A partition with nothing of its own stays on one line.
        let partitions: Vec<_> = t.partitions.iter().map(|(n, p)| (n, p)).collect();
        let out = render_partitioned(&t.name, &t.table, &partitions, &[], None);
        assert!(
            out.contains("\n  ev_plain: {from: [\"2025-01-01\"], to: [MAXVALUE]}\n"),
            "{out}"
        );
        assert!(
            out.contains("\n  ev_rest:\n    default: true\n    indexes:\n      rest_id:\n"),
            "{out}"
        );
    }

    /// Negative: an entry is one bound, `from:` and `to:` both or `default:
    /// true` alone, and nothing a partition cannot have of its own.
    #[test]
    fn a_partition_entry_is_one_bound_and_only_what_a_partition_owns() {
        let parent = "table: app.ev\ncolumns:\n  ts: {type: date, nullable: false}\npartition_by: [ts]\npartitions:\n";
        for (entry, expected) in [
            (
                "  p: {from: [\"2025-01-01\"]}\n",
                "needs both `from:` and `to:`",
            ),
            ("  p: {to: [MAXVALUE]}\n", "needs both `from:` and `to:`"),
            (
                "  p: {checks: {c: ts > '2000-01-01'}}\n",
                "needs both `from:` and `to:`",
            ),
            ("  p: {default: false}\n", "needs both `from:` and `to:`"),
            (
                "  p: {default: true, from: [MINVALUE], to: [MAXVALUE]}\n",
                "is `default: true` and also has",
            ),
            (
                "  p: {default: true, columns: {x: {type: int}}}\n",
                "unknown field",
            ),
            (
                "  p: {from: [MINVALUE], to: [MAXVALUE], unique: {u: [ts]}}\n",
                "unknown field",
            ),
        ] {
            let yaml = format!("{parent}{entry}");
            let err = crate::load_table_str(Path::new("t.yml"), &yaml)
                .err()
                .unwrap_or_else(|| panic!("{entry} loaded"));
            let text = format!("{err:?}");
            assert!(text.contains(expected), "{entry}: {text}");
        }
    }

    /// A parent's partitions load as tables of their own, in its schema unless
    /// qualified, with each bound as written, and render back into the
    /// parent's file in name order (#1170).
    #[test]
    fn partitions_load_as_tables_and_render_back_in_the_parent_file() {
        use pbps_model::{BoundDatum as D, PartitionBound as B};
        let yaml = "table: app.ev\ncolumns:\n  id: {type: int, nullable: false}\n  ts: {type: date, nullable: false}\n\nprimary_key: [id, ts]\n\npartition_by: [ts]\n\npartitions:\n  ev_old: {from: [MINVALUE], to: ['2025-01-01']}\n  ev_rest: default\n  hist.ev_2026: {from: ['2026-01-01'], to: [maxvalue]}\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        assert_eq!(
            t.table.partition_by.as_ref().map(|p| p.columns.clone()),
            Some(vec!["ts".to_owned()])
        );
        let bounds: Vec<(String, B)> = t
            .partitions
            .iter()
            .map(|(n, p)| {
                let of = p.partition_of.clone().expect("a partition");
                assert_eq!(of.parent, t.name);
                // A partition declares nothing else.
                assert_eq!(
                    p,
                    &pbps_model::Table {
                        partition_of: Some(of.clone()),
                        ..Default::default()
                    }
                );
                (n.to_string(), of.bound)
            })
            .collect();
        let value = |v: &str| D::Value(v.to_owned());
        assert_eq!(
            bounds,
            [
                (
                    "app.ev_old".to_owned(),
                    B::Range {
                        from: vec![D::MinValue],
                        to: vec![value("2025-01-01")]
                    }
                ),
                ("app.ev_rest".to_owned(), B::Default),
                (
                    "hist.ev_2026".to_owned(),
                    B::Range {
                        from: vec![value("2026-01-01")],
                        to: vec![D::MaxValue]
                    }
                ),
            ]
        );
        let partitions: Vec<_> = t.partitions.iter().map(|(n, p)| (n, p)).collect();
        let out = render_partitioned(&t.name, &t.table, &partitions, &[], None);
        assert!(
            out.contains("\npartition_by: [ts]\n\npartitions:\n  ev_old: {from: [MINVALUE], to: [\"2025-01-01\"]}\n  ev_rest: default\n  hist.ev_2026: {from: [\"2026-01-01\"], to: [MAXVALUE]}\n"),
            "{out}"
        );
        // Every value the engine can print comes back as itself, whatever
        // YAML would make of it bare.
        for v in [
            "010",
            "0x1F",
            "0o17",
            "1e3",
            ".inf",
            "-0",
            "+5",
            "true",
            "null",
            "~",
            "it's",
            "a: b",
            "#x",
            " x",
            "x\ny",
            "back\\slash",
            "\"q\"",
        ] {
            let child = TableName::new("app", "p1");
            let table = pbps_model::Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: TableName::new("app", "t"),
                    bound: B::Range {
                        from: vec![value(v)],
                        to: vec![D::MaxValue],
                    },
                }),
                ..Default::default()
            };
            let parent = pbps_model::Table {
                partition_by: Some(pbps_model::PartitionBy {
                    columns: vec!["a".to_owned()],
                }),
                ..crate::load_table_str(
                    Path::new("t.yml"),
                    "table: app.t\ncolumns:\n  a: {type: text}\n",
                )
                .unwrap()
                .table
            };
            let out = render_partitioned(
                &TableName::new("app", "t"),
                &parent,
                &[(&child, &table)],
                &[],
                None,
            );
            let back = crate::load_table_str(Path::new("t.yml"), &out)
                .unwrap_or_else(|e| panic!("{v:?}: {e:?}\n{out}"));
            assert_eq!(back.partitions, [(child, table)], "{v:?}\n{out}");
        }
        // A whole number may be written bare; it is the engine's text either way.
        let t = crate::load_table_str(
            Path::new("t.yml"),
            "table: app.m\ncolumns:\n  a: {type: int}\npartition_by: [a]\npartitions:\n  m_1: {from: [-5], to: ['10']}\n",
        )
        .unwrap();
        assert_eq!(
            t.partitions[0]
                .1
                .partition_of
                .as_ref()
                .map(|p| p.bound.clone()),
            Some(B::Range {
                from: vec![value("-5")],
                to: vec![value("10")]
            })
        );
        // Negative: no `partitions:` is a parent with none, and no
        // `partition_by:` an ordinary table that renders neither.
        let t = crate::load_table_str(
            Path::new("t.yml"),
            "table: app.t\ncolumns:\n  a: {type: int}\n",
        )
        .unwrap();
        assert!(t.partitions.is_empty() && t.table.partition_by.is_none());
        assert!(!render(&t.name, &t.table, &[], None).contains("partition"));
    }

    /// A partition block the loader cannot read is a load error, never an
    /// ordinary table or a guessed bound (#1170).
    #[test]
    fn an_unreadable_partition_block_is_rejected() {
        for block in [
            // Partitions with no key to divide by.
            "partitions:\n  p1: default\n",
            // A word that is not `default`.
            "partition_by: [a]\npartitions:\n  p1: rest\n",
            // A decimal and a boolean, whose YAML reading is not the text.
            "partition_by: [a]\npartitions:\n  p1: {from: [1.5], to: ['2']}\n",
            "partition_by: [a]\npartitions:\n  p1: {from: [true], to: ['2']}\n",
            // A missing end, and an unknown key.
            "partition_by: [a]\npartitions:\n  p1: {from: ['1']}\n",
            "partition_by: [a]\npartitions:\n  p1: {from: ['1'], to: ['2'], at: ['3']}\n",
            // A name that is not one.
            "partition_by: [a]\npartitions:\n  'a.b.c': default\n",
        ] {
            let yaml = format!("table: app.t\ncolumns:\n  a: {{type: int}}\n{block}");
            assert!(
                crate::load_table_str(Path::new("t.yml"), &yaml).is_err(),
                "{block} should not load"
            );
        }
    }

    /// An array type loads as the element and its marker, renders back as
    /// written, and another array spelling is a load error (#1167).
    #[test]
    fn an_array_type_round_trips_and_other_spellings_are_refused() {
        let yaml = "table: app.t\ncolumns:\n  tags:\n    type: integer[]\n  stamps:\n    type: timestamp(3) with time zone[]\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        assert!(t.table.columns["tags"].ty.is_array());
        assert_eq!(
            t.table.columns["stamps"].ty.to_string(),
            "timestamp(3) with time zone[]"
        );
        for spelling in ["integer[3]", "integer[][]", "integer ARRAY"] {
            let bad = format!("table: app.t\ncolumns:\n  tags:\n    type: {spelling}\n");
            assert!(
                crate::load_table_str(Path::new("t.yml"), &bad).is_err(),
                "{spelling} should not load"
            );
        }
    }

    /// Storage parameters read back in their canonical spelling, whatever
    /// spelling was written, and render to that spelling (#1441).
    #[test]
    fn storage_parameters_read_back_canonical_and_round_trip() {
        let yaml = "table: public.t\ncolumns:\n  id: {type: int}\n\nstorage_parameters:\n  autovacuum_enabled: off\n  autovacuum_vacuum_scale_factor: 1e-2\n  fillfactor: '070'\n  vacuum_index_cleanup: 'TRUE'\n";
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        let read: Vec<(&str, &str)> = t
            .table
            .storage_parameters
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            read,
            [
                ("autovacuum_enabled", "false"),
                ("autovacuum_vacuum_scale_factor", "0.01"),
                ("fillfactor", "56"),
                ("vacuum_index_cleanup", "on"),
            ]
        );
        let out = render(&t.name, &t.table, &t.intents, None);
        round_trip(&out);
        let again = crate::load_table_str(Path::new("t.yml"), &out).unwrap();
        assert_eq!(again.table.storage_parameters, t.table.storage_parameters);
        // A bare number is read as written too: `7e1` for an integer
        // parameter is 70, as the engine reads it.
        let bare = crate::load_table_str(
            Path::new("t.yml"),
            "table: public.t\ncolumns:\n  id: {type: int}\nstorage_parameters:\n  fillfactor: 7e1\n  autovacuum_vacuum_scale_factor: 0.050\n",
        )
        .unwrap();
        assert_eq!(bare.table.storage_parameters["fillfactor"], "70");
        assert_eq!(
            bare.table.storage_parameters["autovacuum_vacuum_scale_factor"],
            "0.05"
        );
        // Negative: an unknown or `toast.*` name, and a value the engine
        // would refuse, are errors, never dropped.
        for line in [
            "  bogus: 1",
            "  toast.autovacuum_enabled: false",
            "  fillfactor: '08'",
            "  vacuum_index_cleanup: tr",
            // Bare, a YAML number would already be 0 before it is checked:
            // the written spelling is what is read (#1477 review).
            "  autovacuum_vacuum_scale_factor: 1e-400",
        ] {
            let yaml = format!(
                "table: public.t\ncolumns:\n  id: {{type: int}}\nstorage_parameters:\n{line}\n"
            );
            assert!(
                crate::load_table_str(Path::new("t.yml"), &yaml).is_err(),
                "{line}"
            );
        }
    }

    /// A key's, a unique constraint's and an index's parameters read back
    /// canonical and render to the same text; the plain forms stay where
    /// there are none, and a parameter of the wrong method is an error
    /// (#1442).
    #[test]
    fn index_storage_parameters_round_trip_in_every_form() {
        let yaml = "table: public.t\ncolumns:\n  id: {type: int, nullable: false}\n  code: {type: int}\n  doc: {type: jsonb}\n\nprimary_key:\n  name: t_pkey\n  columns: [id]\n  storage_parameters: {fillfactor: '070'}\n\nunique:\n  uq_code: {columns: [code], storage_parameters: {deduplicate_items: of}}\n  uq_plain: [code]\n\nindexes:\n  ix_doc:\n    method: gin\n    storage_parameters: {fastupdate: off, gin_pending_list_limit: 7e1}\n    columns: [doc]\n";
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        let pk = t.table.primary_key.as_ref().unwrap();
        assert_eq!(pk.storage_parameters["fillfactor"], "56");
        assert_eq!(
            t.table.unique["uq_code"].storage_parameters["deduplicate_items"],
            "false"
        );
        assert!(t.table.unique["uq_plain"].storage_parameters.is_empty());
        assert_eq!(
            t.table.indexes["ix_doc"].storage_parameters["gin_pending_list_limit"],
            "70"
        );
        let out = render(&t.name, &t.table, &t.intents, None);
        assert!(out.contains("  uq_plain: [code]\n"), "{out}");
        assert!(
            out.contains("  storage_parameters: {fillfactor: 56}\n"),
            "{out}"
        );
        round_trip(&out);
        let again = crate::load_table_str(Path::new("t.yml"), &out).unwrap();
        assert_eq!(again.table, t.table);
        // Negative: a GIN parameter on a B-tree index, and a B-tree one on
        // a key, are errors.
        for bad in [
            yaml.replace("method: gin\n    ", ""),
            yaml.replace("{fillfactor: '070'}", "{fastupdate: off}"),
        ] {
            assert!(
                crate::load_table_str(Path::new("t.yml"), &bad).is_err(),
                "{bad}"
            );
        }
    }

    /// A column named like a YAML boolean (`n`, `y`, `on`) is a column in
    /// every form of a key and a unique constraint: the list form is read as
    /// strings, not buffered untyped (#1442).
    #[test]
    fn a_boolean_looking_column_is_a_column_in_every_key_form() {
        let yaml = "table: public.t\ncolumns:\n  n: {type: int, nullable: false}\n  y: {type: int}\n  on: {type: int}\n\nprimary_key: [n]\n\nunique:\n  uq_y: [y]\n  uq_on: {columns: [on], storage_parameters: {fillfactor: 70}}\n";
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        assert_eq!(t.table.primary_key.as_ref().unwrap().columns, ["n"]);
        assert_eq!(t.table.unique["uq_y"].columns, ["y"]);
        assert_eq!(t.table.unique["uq_on"].columns, ["on"]);
        let named = "table: public.t\ncolumns:\n  n: {type: int, nullable: false}\nprimary_key: {name: t_pkey, columns: [n]}\n";
        let t = crate::load_table_str(Path::new("t.yml"), named).unwrap();
        assert_eq!(
            t.table.primary_key.as_ref().unwrap().name.as_deref(),
            Some("t_pkey")
        );
        // Negative: neither a list nor a mapping.
        let wrong = "table: public.t\ncolumns:\n  n: {type: int}\nunique:\n  uq: n\n";
        assert!(crate::load_table_str(Path::new("t.yml"), wrong).is_err());
    }

    /// `unlogged: true` reads back and renders again; the default, a
    /// permanent table, writes nothing (#1443).
    #[test]
    fn an_unlogged_table_round_trips_and_permanence_writes_nothing() {
        let yaml = "table: public.t\ncolumns:\n  id: {type: int}\n\nunlogged: true\n";
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        assert!(t.table.unlogged);
        let out = render(&t.name, &t.table, &t.intents, None);
        assert!(out.contains("\nunlogged: true\n"), "{out}");
        round_trip(&out);
        let plain = crate::load_table_str(
            Path::new("t.yml"),
            "table: public.t\ncolumns:\n  id: {type: int}\n",
        )
        .unwrap();
        assert!(!plain.table.unlogged);
        assert!(!render(&plain.name, &plain.table, &plain.intents, None).contains("unlogged"));
    }

    /// A misspelt identity is an error, not the default: read as absent,
    /// `replica_identity: ful` would plan the table back to `DEFAULT`.
    #[test]
    fn an_unknown_replica_identity_is_rejected() {
        for line in [
            "replica_identity: ful",
            "replica_identity: default",
            "replica_identity: {indx: ix_code}",
            "replica_identity: [ix_code]",
        ] {
            let yaml = format!(
                "table: public.t\ncolumns:\n  code: {{type: int, nullable: false}}\nindexes:\n  ix_code:\n    columns: [code]\n    unique: true\n{line}\n"
            );
            assert!(
                crate::load_table_str(Path::new("t.yml"), &yaml).is_err(),
                "{line} should not load"
            );
        }
    }

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
