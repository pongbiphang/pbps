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
    let _ = writeln!(s, "table: {}", scalar(&name.raw().to_string()));

    if let Some(d) = &table.description {
        let _ = writeln!(s, "description: {}", scalar(d.raw()));
    }

    if let Some(Intent::RenameTable { from, .. }) = intents
        .iter()
        .find(|i| matches!(i, Intent::RenameTable { to, .. } if to == name))
    {
        let _ = writeln!(s, "renamed_from: {}", scalar(&from.raw().to_string()));
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
        let _ = writeln!(s, "  {}:", scalar(col_name.raw()));
        // A column with an owned sequence is written as the `serial` it was
        // declared as, which also says NOT NULL (DEC-1631.1).
        let serial = match &c.value_source {
            pbps_model::ValueSource::Sequence(_) => serial_spelling(&c.ty),
            pbps_model::ValueSource::None
            | pbps_model::ValueSource::Default(_)
            | pbps_model::ValueSource::Identity(_)
            | pbps_model::ValueSource::Generated(_) => None,
        };
        match serial {
            Some(serial) => {
                let _ = writeln!(s, "    type: {serial}");
                if c.nullable {
                    s.push_str("    nullable: true\n");
                }
            }
            None => {
                let _ = writeln!(s, "    type: {}", type_scalar(&c.ty));
                if !c.nullable {
                    s.push_str("    nullable: false\n");
                }
            }
        }
        match &c.value_source {
            pbps_model::ValueSource::None => {}
            pbps_model::ValueSource::Default(d) => {
                let _ = writeln!(s, "    default: {}", scalar(d));
            }
            // The pair, unless the sequence has options to say (#1921).
            pbps_model::ValueSource::Identity(id) if id.options.is_default() => {
                let _ = writeln!(s, "    identity: [{}, {}]", id.seed, id.increment);
            }
            pbps_model::ValueSource::Identity(id) => {
                let mut fields = vec![
                    format!("seed: {}", id.seed),
                    format!("increment: {}", id.increment),
                ];
                fields.extend(option_fields(&id.options));
                let _ = writeln!(s, "    identity: {{{}}}", fields.join(", "));
            }
            // Only what `serial` does not say (DEC-1631.1).
            pbps_model::ValueSource::Sequence(seq) => {
                let mut fields = Vec::new();
                if seq.start != 1 {
                    fields.push(format!("start: {}", seq.start));
                }
                if seq.increment != 1 {
                    fields.push(format!("increment: {}", seq.increment));
                }
                if let Some(n) = &seq.name {
                    fields.push(format!("name: {}", scalar(n)));
                }
                fields.extend(option_fields(&seq.options));
                if !fields.is_empty() {
                    let _ = writeln!(s, "    sequence: {{{}}}", fields.join(", "));
                }
            }
            pbps_model::ValueSource::Generated(g) => {
                let _ = writeln!(
                    s,
                    "    generated: {{expression: {}, stored: {}}}",
                    scalar(&g.expression),
                    g.stored
                );
            }
        }
        if let Some(collation) = &c.collation {
            let _ = writeln!(s, "    collation: {}", scalar(collation.as_str()));
        }
        if let Some(d) = &c.description {
            let _ = writeln!(s, "    description: {}", scalar(d.raw()));
        }
        if let Some(d) = &c.deprecated {
            let _ = writeln!(s, "    deprecated: {}", scalar(d.raw()));
        }
        if let Some(Intent::RenameColumn { from, .. }) = intents.iter().find(|i| {
            matches!(i, Intent::RenameColumn { table: t, to, .. } if t == name && to == col_name)
        }) {
            let _ = writeln!(s, "    renamed_from: {}", scalar(from.raw()));
        }
    }

    if !table.computed.is_empty() {
        s.push_str("\ncomputed:\n");
        for (col_name, c) in &table.computed {
            let _ = writeln!(
                s,
                "  {}: {{expression: {}{}{}}}",
                scalar(col_name.raw()),
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

    render_foreign_keys(&mut s, table, "", "\n");
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
        // RANGE as the list it always was; LIST and HASH named (#1632).
        let _ = match by.strategy {
            pbps_model::PartitionStrategy::Range => {
                writeln!(s, "\npartition_by: {}", seq(&by.columns))
            }
            pbps_model::PartitionStrategy::List => {
                writeln!(s, "\npartition_by: {{list: {}}}", seq(&by.columns))
            }
            pbps_model::PartitionStrategy::Hash => {
                writeln!(s, "\npartition_by: {{hash: {}}}", seq(&by.columns))
            }
        };
        // Another tool's partitions are not listed (#1533): a caller handing
        // some over with the setting has two sources of truth, and the
        // setting wins, as the loader reads it.
        if by.external {
            s.push_str("\npartitions: external\n");
        } else if !partitions.is_empty() {
            s.push_str("\npartitions:\n");
            let mut sorted: Vec<&(&TableName, &Table)> = partitions.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            for (child, t) in sorted {
                let label = if child.schema == name.schema {
                    child.name.clone()
                } else {
                    child.raw().to_string()
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
                let values = |d: &std::collections::BTreeSet<pbps_model::ListDatum>| {
                    d.iter()
                        .map(|v| match v {
                            pbps_model::ListDatum::Value(text) => bound_value(text),
                            pbps_model::ListDatum::Null => "NULL".to_owned(),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let bound = t.partition_of.as_ref().map(|p| &p.bound);
                let own_columns = t.partition_of.as_ref().map(|p| &p.columns);
                // On one line while the partition is its bound alone; a block
                // once it has checks, indexes (#1577), columns (#1578), a
                // persistence or storage parameters (#1580), or foreign keys
                // (#1632) of its own.
                if t.checks.is_empty()
                    && t.indexes.is_empty()
                    && t.foreign_keys.is_empty()
                    && own_columns.is_none_or(std::collections::BTreeMap::is_empty)
                    && !t.unlogged
                    && t.storage_parameters.is_empty()
                {
                    let bound = match bound {
                        Some(pbps_model::PartitionBound::Range { from, to }) => {
                            format!("{{from: [{}], to: [{}]}}", list(from), list(to))
                        }
                        Some(pbps_model::PartitionBound::List { values: v }) => {
                            format!("{{in: [{}]}}", values(v))
                        }
                        Some(pbps_model::PartitionBound::Hash { modulus, remainder }) => {
                            format!("{{modulus: {modulus}, remainder: {remainder}}}")
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
                        Some(pbps_model::PartitionBound::List { values: v }) => {
                            let _ = writeln!(s, "    in: [{}]", values(v));
                        }
                        Some(pbps_model::PartitionBound::Hash { modulus, remainder }) => {
                            let _ =
                                writeln!(s, "    modulus: {modulus}\n    remainder: {remainder}");
                        }
                        Some(pbps_model::PartitionBound::Default) | None => {
                            s.push_str("    default: true\n");
                        }
                    }
                    // After the bound, before what names them, as a table's
                    // columns come first.
                    if let Some(columns) = own_columns.filter(|c| !c.is_empty()) {
                        s.push_str("    columns:\n");
                        for (column, own) in columns {
                            let mut fields = Vec::new();
                            if let Some(d) = &own.default {
                                fields.push(format!("default: {}", scalar(d)));
                            }
                            if own.not_null {
                                fields.push("nullable: false".to_owned());
                            }
                            let _ = writeln!(
                                s,
                                "      {}: {{{}}}",
                                scalar(column.raw()),
                                fields.join(", ")
                            );
                        }
                    }
                    render_foreign_keys(&mut s, t, "    ", "");
                    render_checks_and_indexes(&mut s, t, "    ", "");
                    // As a table's, last (#1580).
                    if t.unlogged {
                        s.push_str("    unlogged: true\n");
                    }
                    if !t.storage_parameters.is_empty() {
                        s.push_str("    storage_parameters:\n");
                        for (name, value) in &t.storage_parameters {
                            let _ = writeln!(s, "      {}: {value}", scalar(name));
                        }
                    }
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
                scalar(&v.history.raw().to_string())
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

    if !table.row_security.is_default() {
        let _ = writeln!(
            s,
            "\nrow_security: {{enabled: {}, forced: {}}}",
            table.row_security.enabled, table.row_security.forced
        );
    }
    if !table.row_policies.is_empty() {
        s.push_str("\nrow_policies:\n");
        for (name, policy) in &table.row_policies {
            let _ = writeln!(s, "  {}:", scalar(name));
            let _ = writeln!(s, "    command: {}", policy.command.as_str());
            let _ = writeln!(s, "    restrictive: {}", policy.restrictive);
            match &policy.to {
                pbps_model::PolicyRoles::Public => s.push_str("    to: public\n"),
                pbps_model::PolicyRoles::Roles(roles) => {
                    let roles: Vec<_> = roles.iter().cloned().collect();
                    let _ = writeln!(s, "    to: {}", seq(&roles));
                }
            }
            if let Some(expression) = &policy.using {
                let _ = writeln!(s, "    using: {}", scalar(expression.raw()));
            }
            if let Some(expression) = &policy.with_check {
                let _ = writeln!(s, "    with_check: {}", scalar(expression.raw()));
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
                .map(|(c, v)| format!("{}: {}", scalar(c.raw()), value(v)))
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
/// Definitions normally use a literal block scalar (`|-`). Characters that
/// cannot stay literal use the ordinary double-quoted escapes instead, keeping
/// the complete definition intact rather than normalizing its whitespace.
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
            ObjectName::new(on.schema.clone(), name.clone())
                .raw()
                .to_string(),
            Some(on.raw().to_string()),
        ),
        other @ (ModuleId::Named(_) | ModuleId::Routine(_)) => (other.raw().to_string(), None),
    };
    let _ = writeln!(s, "{}: {}", module.kind.as_str(), scalar(&declared));

    if let Some(d) = &module.description {
        let _ = writeln!(s, "description: {}", scalar(d.raw()));
    }
    if let Some(on) = on.as_deref() {
        let _ = writeln!(s, "on: {}", scalar(on));
    }
    if !depends_on.is_empty() {
        let names: Vec<String> = depends_on.iter().map(|id| id.raw().to_string()).collect();
        let _ = writeln!(s, "depends_on: {}", seq(&names));
    }
    // Written only when it is `true`. `false` is what the absent key means,
    // and a formatter that spelled the default out would put the line into
    // every view and trigger file in the project, where the loader refuses it.
    if public_execute {
        let _ = writeln!(s, "public_execute: true");
    }

    // Quotes and backslashes need no escaping in a literal block, and tabs
    // and LF keep their meaning there. Every other escape follows the shared
    // scalar policy: literal controls can invalidate YAML or change on reload.
    if module
        .definition
        .chars()
        .any(|c| needs_escape(c) && !matches!(c, '\t' | '\n' | '\\' | '"'))
    {
        let _ = writeln!(s, "\ndefinition: {}", quoted(&module.definition));
        return s;
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
pub fn render_role(
    name: &pbps_model::RoleName,
    role: &pbps_model::Role,
    pending: &[Intent],
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "role: {}", scalar(name.raw()));
    if let Some(d) = &role.description {
        let _ = writeln!(s, "description: {}", scalar(d.raw()));
    }
    for i in pending {
        if let Intent::RenameRole { from, to } = i
            && to == name
        {
            let _ = writeln!(s, "renamed_from: {}", scalar(from.raw()));
        }
    }
    let grants: Vec<_> = role.declarable_grants().collect();
    if !grants.is_empty() {
        s.push_str("\ngrants:\n");
        for (target, permissions) in grants {
            let names: Vec<String> = permissions.iter().map(|p| p.as_str().to_owned()).collect();
            let _ = writeln!(
                s,
                "  {}: {}",
                scalar(&target.raw().to_string()),
                seq(&names)
            );
        }
    }
    s
}

fn action(a: pbps_model::ReferentialAction) -> &'static str {
    use pbps_model::ReferentialAction as R;
    match a {
        R::NoAction => "no_action",
        R::Restrict => "restrict",
        R::Cascade => "cascade",
        R::SetNull => "set_null",
        R::SetDefault => "set_default",
    }
}

fn seq(items: &[impl std::borrow::Borrow<str>]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| scalar(s.borrow()))
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
        quoted(s)
    } else {
        s.to_owned()
    }
}

/// A column's type as a YAML scalar. A type reference may hold double quotes
/// (`"App"."Mood"`, #1523), which `scalar` would escape one by one; single
/// quotes keep the name as it is written, and YAML has nothing to escape inside
/// them but the single quote itself. A name with a character that cannot sit
/// raw on one line takes `scalar`'s escapes instead. A type with no `"` is
/// `scalar`'s, exactly as before.
fn type_scalar(ty: &pbps_model::ColumnType) -> String {
    let text = ty.to_string();
    let raw_ok = !text.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}'
            )
    });
    if text.contains('"') && raw_ok {
        format!("'{}'", text.replace('\'', "''"))
    } else {
        scalar(&text)
    }
}

/// A bound value, always double-quoted: it is the engine's text for a value
/// of any key type, and bare it would be read by YAML's rules, `0x1F` as 31
/// and `MINVALUE` as the unbounded end (#1170).
fn bound_value(s: &str) -> String {
    quoted(s)
}

fn needs_escape(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\\' | '"' | '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}'
        )
}

/// A single physical line works in block values, flow mappings and keys.
/// Raw line breaks inside quotes are folded by YAML; a literal block cannot
/// replace them in a flow mapping or preserve a bare carriage return (#1795).
fn quoted(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\0' => out.push_str("\\0"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            '\x0c' => out.push_str("\\f"),
            '\x1b' => out.push_str("\\e"),
            '\u{85}' => out.push_str("\\N"),
            '\u{2028}' => out.push_str("\\L"),
            '\u{2029}' => out.push_str("\\P"),
            '\u{feff}' | '\u{fffe}' | '\u{ffff}' => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c if c.is_control() => {
                let _ = write!(out, "\\x{:02X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn needs_quotes(s: &str) -> bool {
    if s.is_empty() || s != s.trim() || s.chars().any(needs_escape) {
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
    ) || !reads_back_bare(s)
}

/// Flow items and typeless cells are the strictest contexts this writer uses.
/// Ask the loader instead of duplicating its implicit scalar grammar: radix
/// integers, digit separators and non-finite floats exceed Rust's parsers.
/// A parse failure or any different value needs quotes (DEC-1801.1).
fn reads_back_bare(s: &str) -> bool {
    matches!(
        serde_saphyr::from_str::<Vec<crate::dto::ValueDto>>(&format!("[{s}]")).as_deref(),
        Ok([crate::dto::ValueDto::Text(text)]) if text == s
    )
}

/// A table's `checks:` and `indexes:`, each line under `pad`: the top level
/// of a table's file, or a partition's entry under its parent's `partitions:`
/// (#1577). `lead` goes before each section's first line, the blank line
/// between top-level sections.
/// A table's foreign keys, or a partition's own under its entry (#1632).
fn render_foreign_keys(s: &mut String, table: &Table, pad: &str, lead: &str) {
    if table.foreign_keys.is_empty() {
        return;
    }
    let _ = writeln!(s, "{lead}{pad}foreign_keys:");
    for (n, fk) in &table.foreign_keys {
        let _ = writeln!(s, "{pad}  {}:", scalar(n));
        let _ = writeln!(s, "{pad}    columns: {}", seq(&fk.columns));
        let _ = writeln!(
            s,
            "{pad}    references: {}",
            scalar(&format!(
                "{}({})",
                fk.references_table.raw(),
                fk.references_columns
                    .iter()
                    .map(pbps_model::ColumnName::raw)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        );
        if fk.on_delete != Default::default() {
            let _ = writeln!(s, "{pad}    on_delete: {}", action(fk.on_delete));
        }
        if fk.on_update != Default::default() {
            let _ = writeln!(s, "{pad}    on_update: {}", action(fk.on_update));
        }
    }
}

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
                        pbps_model::IndexKey::Column(name) => ("column", name.raw()),
                        pbps_model::IndexKey::Expression(text) => ("expression", text.as_str()),
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

/// A sequence's options as declaration fields, only those it sets (#1921).
fn option_fields(options: &pbps_model::SequenceOptions) -> Vec<String> {
    let mut fields = Vec::new();
    if let Some(min) = options.min {
        fields.push(format!("min: {min}"));
    }
    if let Some(max) = options.max {
        fields.push(format!("max: {max}"));
    }
    if options.cycle {
        fields.push("cycle: true".to_owned());
    }
    if let Some(cache) = options.cache {
        fields.push(format!("cache: {cache}"));
    }
    fields
}

/// The `serial` spelling of an owned sequence's integer type, or `None` for a
/// type no serial stands for, which validation refuses and which is then
/// written as it is held.
fn serial_spelling(ty: &pbps_model::ColumnType) -> Option<&'static str> {
    if ty.is_array() || !ty.args.is_empty() {
        return None;
    }
    match ty.base.as_str() {
        "smallint" | "int2" => Some("smallserial"),
        "integer" | "int" | "int4" => Some("serial"),
        "bigint" | "int8" => Some("bigserial"),
        _ => None,
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

    #[test]
    fn module_definitions_escape_controls_without_changing_their_text() {
        let controls = (0..=0x1f)
            .chain(0x7f..=0x9f)
            .map(|c| char::from_u32(c).unwrap())
            .filter(|c| !matches!(c, '\t' | '\n'))
            .chain(['\u{2028}', '\u{2029}', '\u{feff}', '\u{fffe}', '\u{ffff}']);
        for c in controls {
            for definition in [
                format!("SELECT 'before{c}after' AS note"),
                format!("{c}SELECT 1"),
                format!("SELECT 1{c}"),
                format!("\nSELECT 'first  \n{c}\"second\\last' AS note\r\n \t\n"),
            ] {
                for header in [
                    "view: dbo.v",
                    "function: dbo.f",
                    "procedure: dbo.p",
                    "trigger: dbo.trg\non: dbo.t",
                ] {
                    let out = module_round_trip(&format!(
                        "{header}\ndefinition: {}\n",
                        quoted(&definition)
                    ));
                    assert!(out.contains("\ndefinition: \""), "{c:?}: {out:?}");
                }
            }
        }
    }

    #[test]
    fn ordinary_module_definitions_keep_their_literal_block_bytes() {
        for (definition, block) in [
            ("SELECT 1", "  SELECT 1\n"),
            (
                "SELECT 'first  \nsecond' AS note\n\nFROM dbo.t",
                "  SELECT 'first  \n  second' AS note\n\n  FROM dbo.t\n",
            ),
            ("BEGIN\n\tSELECT 1;\nEND", "  BEGIN\n  \tSELECT 1;\n  END\n"),
            (
                "SELECT 'quoted: #value', \"a\\b\" -- comment",
                "  SELECT 'quoted: #value', \"a\\b\" -- comment\n",
            ),
            ("SELECT '繁體中文 é 😀'", "  SELECT '繁體中文 é 😀'\n"),
        ] {
            for header in [
                "view: dbo.v",
                "function: dbo.f",
                "procedure: dbo.p",
                "trigger: dbo.trg\non: dbo.t",
            ] {
                let out =
                    module_round_trip(&format!("{header}\ndefinition: {}\n", quoted(definition)));
                assert_eq!(out, format!("{header}\n\ndefinition: |-\n{block}"));
            }
        }
    }

    fn round_trip(yaml: &str) {
        let a = crate::load_table_str(Path::new("t.yml"), yaml)
            .unwrap_or_else(|e| panic!("the original file failed to load: {e:?}"));
        round_trip_loaded(&a, "loaded table");
    }

    fn round_trip_loaded(a: &crate::LoadedTable, context: &str) {
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
        let out = render_all(a);
        let b = crate::load_table_str(Path::new("t.yml"), &out).unwrap_or_else(|e| {
            panic!("{context}: the rewritten file does not read back: {e:?}\noutput:\n{out}")
        });
        assert_eq!(a, &b, "{context}: output:\n{out}");

        // Idempotence: formatting an already-formatted file must change nothing.
        let out2 = render_all(&b);
        assert_eq!(out, out2, "{context}: fmt is not idempotent");
    }

    fn scalar_corpus() -> Vec<String> {
        let alphabet = [
            'a', '0', '-', '?', '&', '[', ']', '{', '}', ':', '#', ',', ' ', '\t', '\r', '\n',
            '\0', '\x07', '\x08', '\x0b', '\x0c', '\x1b', '\x7f', '\u{85}', '\u{9f}', '\u{2028}',
            '\u{2029}', '\u{feff}', '\u{fffe}', '\u{ffff}', '"', '\'', '\\', '!', '|', '>', '%',
            '@', '`', 'é',
        ];
        let mut values: Vec<String> = [
            "",
            "First line\nSecond line",
            "a\rb",
            "a\r\nb",
            "a\tb",
            " leading",
            "trailing ",
            " both ",
            "literal\\n",
            "a\"b\\c",
            "no",
            "null",
            "0123",
            "1.50",
            "plain_name",
            "繁體中文",
            "0x1f",
            "0X1F",
            "-0x1f",
            "+0X1F",
            "0o17",
            "0O17",
            "-0o17",
            "+0O17",
            "0b101",
            "0B101",
            "-0b101",
            "+0b1",
            "1_000",
            "-1_0",
            "+1_000",
            "-0X1F",
            "+0x1f",
            "-0O17",
            "+0o17",
            "-0B101",
            "+0B101",
            "+.INF",
            "-.INF",
            ".NAN",
            "+.nan",
            "-.nan",
            "+.NaN",
            "-.NaN",
            ".inf",
            ".INF",
            "+.inf",
            "-.inf",
            ".nan",
            ".NaN",
            "001",
            "1e3",
            "2026-10-11",
            "12:30:00",
            "yes",
            "~",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        for a in alphabet {
            values.push(a.to_string());
            for b in alphabet {
                values.push(format!("{a}{b}"));
            }
        }
        // Interior controls must force quoting too, even when trim changes nothing.
        for c in (0..=31).chain(127..=159) {
            values.push(format!("before{}after", char::from_u32(c).unwrap()));
        }
        values
    }

    // scalar()/needs_quotes() inspect only their scalar, with no width,
    // neighbour-dependent style or per-declaration state. Each context gets a
    // permutation of the same corpus. Distinct values in the nine positions
    // prevent a field mix-up from comparing equal, while one declaration pays
    // for its surrounding names/types only once (issue #2116).
    fn scalar_batch(base: &crate::LoadedTable, cases: &[String]) -> String {
        let make = |cases: &[String]| {
            let mut expected = base.clone();
            for i in 0..cases.len() {
                let text = |offset: usize| &cases[(i + offset) % cases.len()];
                let key = i.to_string();
                let mut column = base.table.columns["v"].clone();
                column.description = Some(text(0).as_str().into());
                column.value_source = pbps_model::ValueSource::Default(text(1).clone());
                column.deprecated = Some(text(2).as_str().into());
                expected.table.columns.insert(key.clone().into(), column);
                let mut generated = base.table.columns["g"].clone();
                match &mut generated.value_source {
                    pbps_model::ValueSource::Generated(g) => g.expression = text(3).clone(),
                    pbps_model::ValueSource::None
                    | pbps_model::ValueSource::Default(_)
                    | pbps_model::ValueSource::Identity(_)
                    | pbps_model::ValueSource::Sequence(_) => panic!("generated fixture"),
                }
                expected
                    .table
                    .columns
                    .insert(format!("+{i}").into(), generated);
                let mut check = base.table.checks["ck"].clone();
                check.expression = text(4).clone();
                expected.table.checks.insert(key.clone(), check);
                let mut index = base.table.indexes["ix"].clone();
                index.filter = Some(text(5).clone());
                expected.table.indexes.insert(key, index);
                let mut computed = base.table.computed["c"].clone();
                computed.expression = text(6).clone();
                expected
                    .table
                    .computed
                    .insert(format!("-{i}").into(), computed);
                let mut row = base
                    .table
                    .data
                    .as_ref()
                    .unwrap()
                    .rows
                    .values()
                    .next()
                    .unwrap()
                    .clone();
                row.0
                    .insert("v".into(), pbps_model::Value::Text(text(8).clone()));
                assert!(
                    expected
                        .table
                        .data
                        .as_mut()
                        .unwrap()
                        .rows
                        .insert(text(7).as_str().into(), row)
                        .is_none()
                );
            }
            expected
        };
        let expected = make(cases);
        let out = render(&expected.name, &expected.table, &[], None);
        if cases == ["no", "plain_name"] {
            // Both spellings must survive together, even though both load as
            // strings when unnecessarily quoted. Model equality alone misses it.
            assert!(
                out.contains("default: \"no\"\n"),
                "quoted neighbour no: {out}"
            );
            assert!(
                out.contains("default: plain_name\n"),
                "bare neighbour plain_name: {out}"
            );
        }
        let back = crate::load_table_str(Path::new("t.yml"), &out).unwrap_or_else(|error| {
            for text in cases {
                round_trip_loaded(
                    &make(std::slice::from_ref(text)),
                    &format!("scalar contexts: {text:?}"),
                );
            }
            panic!("only the combined scalar batch failed: {cases:?}: {error:?}");
        });
        for i in 0..cases.len() {
            let text = |offset: usize| &cases[(i + offset) % cases.len()];
            let key = i.to_string();
            let a = &expected.table.columns[key.as_str()];
            let b = back.table.columns.get(key.as_str()).unwrap_or_else(|| {
                panic!(
                    "missing column for description {:?}, default {:?}, deprecated {:?}",
                    text(0),
                    text(1),
                    text(2)
                )
            });
            assert_eq!(
                a.description,
                b.description,
                "column description: {:?}",
                text(0)
            );
            assert_eq!(a.value_source, b.value_source, "default: {:?}", text(1));
            assert_eq!(a.deprecated, b.deprecated, "deprecated: {:?}", text(2));
            assert_eq!(
                expected.table.columns.get(format!("+{i}").as_str()),
                back.table.columns.get(format!("+{i}").as_str()),
                "generated: {:?}",
                text(3)
            );
            assert_eq!(
                expected.table.checks.get(&key),
                back.table.checks.get(&key),
                "check: {:?}",
                text(4)
            );
            assert_eq!(
                expected.table.indexes.get(&key),
                back.table.indexes.get(&key),
                "filter: {:?}",
                text(5)
            );
            assert_eq!(
                expected.table.computed.get(format!("-{i}").as_str()),
                back.table.computed.get(format!("-{i}").as_str()),
                "computed: {:?}",
                text(6)
            );
            let key = text(7).as_str().into();
            assert_eq!(
                expected.table.data.as_ref().unwrap().rows.get(&key),
                back.table
                    .data
                    .as_ref()
                    .and_then(|data| data.rows.get(&key)),
                "row key: {:?}, cell: {:?}",
                text(7),
                text(8)
            );
        }
        assert_eq!(expected, back, "scalar batch surrounding fields");
        let out2 = render(&back.name, &back.table, &[], None);
        if out != out2 {
            for text in cases {
                round_trip_loaded(
                    &make(std::slice::from_ref(text)),
                    &format!("scalar contexts: {text:?}"),
                );
            }
            assert_eq!(out, out2, "scalar batch {cases:?} is not idempotent");
        }
        out
    }

    // Duplicate row keys must start another batch rather than overwrite a case.
    fn unique_batches(cases: &[String]) -> Vec<&[String]> {
        let mut batches = Vec::new();
        let mut start = 0;
        let mut seen = std::collections::BTreeSet::new();
        for (i, text) in cases.iter().enumerate() {
            if i - start == 512 || seen.contains(text) {
                batches.push(&cases[start..i]);
                start = i;
                seen.clear();
            }
            seen.insert(text);
        }
        if start < cases.len() {
            batches.push(&cases[start..]);
        }
        batches
    }

    #[test]
    fn every_scalar_context_keeps_its_exact_text_and_format() {
        let base = crate::load_table_str(
            Path::new("t.yml"),
            "table: app.t\ncolumns:\n  id: {type: varchar(100), nullable: false}\n  v: {type: varchar(100)}\n  g: {type: int, generated: {expression: '1', stored: true}}\ncomputed:\n  c: {expression: '1'}\nprimary_key: [id]\nchecks: {ck: '1 = 1'}\nindexes:\n  ix: {columns: [v], where: v IS NOT NULL}\ndata:\n  mode: exact\n  rows:\n    row: {v: text}\n",
        ).unwrap();
        scalar_batch(&base, &["no".into(), "plain_name".into()]);
        // Keep each original single-field shape beside its unchanged fields.
        // Corpus permutations below cover the spelling in every context; these
        // controls also catch a field accidentally nested under another field.
        type Position = (&'static str, fn(&mut Table));
        let positions: [Position; 9] = [
            ("column description", |t| {
                t.columns["v"].description = Some("no".into())
            }),
            ("default", |t| {
                t.columns["v"].value_source = pbps_model::ValueSource::Default("no".into())
            }),
            ("deprecated", |t| {
                t.columns["v"].deprecated = Some("no".into())
            }),
            ("check", |t| {
                t.checks.get_mut("ck").unwrap().expression = "no".into()
            }),
            ("filter", |t| {
                t.indexes.get_mut("ix").unwrap().filter = Some("no".into())
            }),
            ("generated", |t| {
                if let pbps_model::ValueSource::Generated(g) = &mut t.columns["g"].value_source {
                    g.expression = "no".into();
                } else {
                    panic!("generated fixture");
                }
            }),
            ("computed", |t| {
                t.computed.get_mut("c").unwrap().expression = "no".into()
            }),
            ("row key", |t| {
                let rows = &mut t.data.as_mut().unwrap().rows;
                let (_, row) = rows.pop_first().unwrap();
                rows.insert("no".into(), row);
            }),
            ("cell", |t| {
                t.data
                    .as_mut()
                    .unwrap()
                    .rows
                    .values_mut()
                    .next()
                    .unwrap()
                    .0
                    .insert("v".into(), pbps_model::Value::Text("no".into()));
            }),
        ];
        for (position, set) in positions {
            let mut isolated = base.clone();
            set(&mut isolated.table);
            round_trip_loaded(&isolated, &format!("isolated {position}: no"));
        }
        let corpus = scalar_corpus();
        assert_eq!(corpus.len(), 1761);
        let batches = unique_batches(&corpus);
        assert_eq!(
            batches.iter().map(|batch| batch.len()).sum::<usize>(),
            corpus.len()
        );
        for batch in batches {
            assert!(
                batch.len() >= 9,
                "context permutations need distinct values"
            );
            scalar_batch(&base, batch);
        }
        // A description cannot repeat in one table. The other contexts above
        // retain the full fixture; this one needs only its surrounding mapping.
        let mut description = crate::load_table_str(
            Path::new("t.yml"),
            "table: app.t\ncolumns: {id: {type: int}}\n",
        )
        .unwrap();
        let mut full = base.clone();
        full.table.description = Some("no".into());
        round_trip_loaded(&full, "table description beside every other context");
        for s in corpus {
            description.table.description = Some(s.as_str().into());
            round_trip_loaded(&description, &format!("table description: {s:?}"));
            let module = Module {
                kind: pbps_model::ModuleKind::View,
                description: Some(s.as_str().into()),
                definition: "SELECT 'first  \nsecond' AS note".into(),
            };
            let id = ModuleId::Named("app.v".parse().unwrap());
            let out = render_module(&id, &module, &Default::default(), false);
            let back = crate::load_module_str(Path::new("v.yml"), &out).unwrap();
            assert_eq!(back.module, module, "module description: {s:?}");
            assert_eq!(
                render_module(
                    &back.id,
                    &back.module,
                    &back.depends_on,
                    back.public_execute
                ),
                out
            );

            let role = pbps_model::Role {
                description: Some(s.as_str().into()),
                ..Default::default()
            };
            let out = render_role(&pbps_model::RoleName::from("reader"), &role, &[]);
            let back = crate::load_role_str(Path::new("r.yml"), &out).unwrap();
            assert_eq!(back.role, role, "role description: {s:?}");
            assert_eq!(render_role(&back.name, &back.role, &back.intents), out);
        }
    }

    #[test]
    fn quoted_controls_do_not_change_non_text_cells_or_plain_names() {
        let yaml = "table: app.t\ncolumns:\n  id: {type: int, nullable: false}\n  n: {type: int}\n  b: {type: bit}\n  v: {type: varchar(20)}\nprimary_key: [id]\ndata:\n  mode: exact\n  rows:\n    '1': {n: -7, b: true, v: null}\n";
        round_trip(yaml);
        let loaded = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        let out = render(&loaded.name, &loaded.table, &[], None);
        assert!(out.starts_with("table: app.t\n"), "{out}");
        assert!(out.contains("{b: true, \"n\": -7, v: null}"), "{out}");
        assert_eq!(scalar("plain_name"), "plain_name");
    }

    #[test]
    fn multiline_names_survive_mapping_keys_sequences_and_rename_annotations() {
        let mut loaded = crate::load_table_str(
            Path::new("t.yml"),
            "table: app.t\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\n",
        )
        .unwrap();
        loaded.name = TableName::new("app", "table\nname");
        let column = loaded.table.columns.shift_remove("id").unwrap();
        loaded.table.columns.insert("column\rname".into(), column);
        loaded.table.primary_key.as_mut().unwrap().columns = vec!["column\rname".into()];
        loaded.intents = vec![
            Intent::RenameTable {
                from: TableName::new("app", "old\ttable"),
                to: loaded.name.clone(),
            },
            Intent::RenameColumn {
                table: loaded.name.clone(),
                from: "old\ncolumn".into(),
                to: "column\rname".into(),
            },
        ];
        round_trip_loaded(&loaded, "names and rename annotations");

        let role = pbps_model::Role::default();
        let pending = vec![Intent::RenameRole {
            from: "old\rrole".into(),
            to: "role\nname".into(),
        }];
        let out = render_role(&pbps_model::RoleName::from("role\nname"), &role, &pending);
        let back = crate::load_role_str(Path::new("r.yml"), &out).unwrap();
        assert_eq!(back.name, "role\nname".into());
        assert_eq!(back.intents, pending);
        assert_eq!(render_role(&back.name, &back.role, &back.intents), out);
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
            let out = render_role(
                &pbps_model::RoleName::from(name),
                &pbps_model::Role::default(),
                &[],
            );
            let back = crate::load_role_str(Path::new("r.yml"), &out)
                .unwrap_or_else(|e| panic!("the rendered file failed to load: {e:?}\n{out}"));
            assert_eq!(back.name, name.into(), "output:\n{out}");
        }

        // The rename intent names the *old* spelling, and it is the same kind
        // of name: trimmed, it would ask the database to rename a role that is
        // not there.
        let out = render_role(
            &pbps_model::RoleName::from("kept"),
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
        let out = render_role(
            &pbps_model::RoleName::from("   "),
            &pbps_model::Role::default(),
            &[],
        );
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
    fn rendered_roles_keep_declared_access_and_omit_serial_markers() {
        use pbps_model::{Permission, Role};
        let role = Role {
            grants: [
                (
                    "app.t".parse().unwrap(),
                    [Permission::Insert, Permission::SerialSequences].into(),
                ),
                (
                    "app.hidden".parse().unwrap(),
                    [Permission::SerialSequences].into(),
                ),
                ("schema::app".parse().unwrap(), [Permission::Usage].into()),
            ]
            .into(),
            ..Role::default()
        };
        let out = render_role(&pbps_model::RoleName::from("writer"), &role, &[]);
        assert!(!out.contains("serial-sequences"), "{out}");
        assert!(!out.contains("hidden"), "{out}");
        let loaded = crate::load_role_str(Path::new("r.yml"), &out).unwrap();
        assert_eq!(
            loaded.role.grants[&"app.t".parse().unwrap()],
            [Permission::Insert].into()
        );
        assert_eq!(
            loaded.role.grants[&"schema::app".parse().unwrap()],
            [Permission::Usage].into()
        );
        let only = Role {
            grants: [(
                "app.t".parse().unwrap(),
                [Permission::SerialSequences].into(),
            )]
            .into(),
            ..Role::default()
        };
        assert_eq!(
            render_role(&pbps_model::RoleName::from("writer"), &only, &[]),
            "role: writer\n"
        );
        assert!(
            crate::load_role_str(
                Path::new("r.yml"),
                "role: writer\ngrants:\n  app.t: [serial-sequences]\n"
            )
            .is_err()
        );
    }

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

        let out = render_role(&pbps_model::RoleName::from("r"), &role, &[]);
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

    /// #1633: `restrict` is written back as itself on either side.
    #[test]
    fn a_restrict_foreign_key_action_round_trips() {
        round_trip(
            "table: dbo.t\ncolumns:\n  a: {type: int}\nforeign_keys:\n  fk_p:\n    columns: [a]\n    references: dbo.p(id)\n    on_delete: restrict\n    on_update: restrict\n",
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
        assert_eq!(scalar("2026-10-11"), "2026-10-11");

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

    #[test]
    fn numeric_looking_column_names_keys_and_cells_keep_their_text() {
        let base = crate::load_table_str(
            Path::new("t.yml"),
            "table: app.t\ncolumns:\n  id: {type: varchar(100), nullable: false}\n  v: {type: varchar(100)}\nprimary_key: [id]\ndata:\n  mode: exact\n  rows:\n    row: {v: text}\n",
        ).unwrap();
        let mut cases = scalar_corpus();
        let alphabet = "01xXoObB_fF+-.eE";
        for a in alphabet.chars() {
            cases.push(a.to_string());
            for b in alphabet.chars() {
                cases.push(format!("{a}{b}"));
                for c in alphabet.chars() {
                    cases.push(format!("{a}{b}{c}"));
                }
            }
        }
        assert_eq!(cases.len(), 6129);
        // As scalar_batch documents, scalar() has no neighbour-dependent state.
        // Every spelling still occupies its original three contexts; duplicate
        // keys start a new batch so no earlier case can be overwritten.
        let make = |cases: &[String]| {
            let mut expected = base.clone();
            expected.table.data.as_mut().unwrap().rows.clear();
            for text in cases {
                // Dots remain valid row keys/cells but cannot name a column.
                let name = if !text.is_empty()
                    && text != "id"
                    && pbps_model::check_segment(text).is_ok()
                {
                    text.clone()
                } else {
                    "v".into()
                };
                expected
                    .table
                    .columns
                    .insert(name.clone().into(), base.table.columns["v"].clone());
                let mut row = base
                    .table
                    .data
                    .as_ref()
                    .unwrap()
                    .rows
                    .values()
                    .next()
                    .unwrap()
                    .clone();
                row.0.clear();
                row.0
                    .insert(name.into(), pbps_model::Value::Text(text.clone()));
                assert!(
                    expected
                        .table
                        .data
                        .as_mut()
                        .unwrap()
                        .rows
                        .insert(text.clone().into(), row)
                        .is_none()
                );
            }
            expected
        };
        let neighbours = make(&["0x1f".into(), "plain_name".into()]);
        let out = render(&neighbours.name, &neighbours.table, &[], None);
        assert!(
            out.contains("    \"0x1f\": {\"0x1f\": \"0x1f\"}"),
            "quoted numeric neighbour 0x1f: {out}"
        );
        assert!(
            out.contains("    plain_name: {plain_name: plain_name}"),
            "bare numeric neighbour plain_name: {out}"
        );
        round_trip_loaded(&neighbours, "numeric neighbours 0x1f / plain_name");
        let batches = unique_batches(&cases);
        assert_eq!(
            batches.iter().map(|batch| batch.len()).sum::<usize>(),
            cases.len()
        );
        for batch in batches {
            let expected = make(batch);
            let out = render(&expected.name, &expected.table, &[], None);
            let back = crate::load_table_str(Path::new("t.yml"), &out).unwrap_or_else(|error| {
                for text in batch {
                    round_trip_loaded(
                        &make(std::slice::from_ref(text)),
                        &format!("numeric spelling: {text:?}"),
                    );
                }
                panic!("only the combined numeric batch failed: {batch:?}: {error:?}");
            });
            for text in batch {
                let key = text.as_str().into();
                assert_eq!(
                    expected.table.data.as_ref().unwrap().rows.get(&key),
                    back.table
                        .data
                        .as_ref()
                        .and_then(|data| data.rows.get(&key)),
                    "numeric spelling: {text:?}"
                );
                let name = if !text.is_empty()
                    && text != "id"
                    && pbps_model::check_segment(text).is_ok()
                {
                    text.as_str()
                } else {
                    "v"
                };
                assert_eq!(
                    expected.table.columns.get(name),
                    back.table.columns.get(name),
                    "numeric column: {text:?}"
                );
            }
            assert_eq!(expected, back, "numeric batch surrounding fields");
            let out2 = render(&back.name, &back.table, &[], None);
            if out != out2 {
                for text in batch {
                    round_trip_loaded(
                        &make(std::slice::from_ref(text)),
                        &format!("numeric spelling: {text:?}"),
                    );
                }
                assert_eq!(out, out2, "numeric batch {batch:?} is not idempotent");
            }
        }
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
            t.table.columns["b"].value_source,
            pbps_model::ValueSource::Generated(pbps_model::Generated {
                expression: "a * 2".into(),
                stored: true
            })
        );
        assert_eq!(
            t.table.columns["a"].value_source,
            pbps_model::ValueSource::None
        );
        let unstated = "table: app.t\ncolumns:\n  a: {type: integer}\n  b:\n    type: integer\n    generated: {expression: 'a * 2'}\n";
        assert!(crate::load_table_str(Path::new("t.yml"), unstated).is_err());
    }

    /// A `serial` loads as its integer type and an owned sequence, NOT NULL
    /// as the engine makes it, and renders back as the `serial` it was, with
    /// `sequence:` only for what `serial` does not say (#1631, DEC-1631.1).
    #[test]
    fn a_serial_column_round_trips_as_serial() {
        let yaml = "table: app.t\ncolumns:\n  id: {type: serial}\n  big:\n    type: bigserial\n    sequence: {start: 100, increment: 5, name: legacy_seq}\n  small: {type: smallserial, nullable: true}\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        let id = &t.table.columns["id"];
        assert_eq!(id.ty.to_string(), "integer");
        assert!(
            !id.nullable,
            "a serial is NOT NULL unless it says otherwise"
        );
        assert_eq!(
            id.value_source,
            pbps_model::ValueSource::Sequence(pbps_model::OwnedSequence::serial())
        );
        let big = &t.table.columns["big"];
        assert_eq!(big.ty.to_string(), "bigint");
        assert_eq!(
            big.value_source,
            pbps_model::ValueSource::Sequence(pbps_model::OwnedSequence {
                start: 100,
                increment: 5,
                name: Some("legacy_seq".into()),
                options: Default::default(),
            })
        );
        // Said, the nullability is kept, for validation to refuse.
        assert!(t.table.columns["small"].nullable);
        let out = render(&t.name, &t.table, &t.intents, None);
        assert!(out.contains("  id:\n    type: serial\n  big:"), "{out}");
        assert!(
            out.contains(
                "    type: bigserial\n    sequence: {start: 100, increment: 5, name: legacy_seq}\n"
            ),
            "{out}"
        );
        assert!(
            out.contains("    type: smallserial\n    nullable: true\n"),
            "{out}"
        );
        assert!(!out.contains("nullable: false"), "{out}");
    }

    /// A sequence's bounds, cycle and cache load from a `sequence:` beside a
    /// `serial` and from an `identity:` written as a mapping, and render back
    /// only where they are set: an identity without them keeps its
    /// `[seed, increment]` pair (#1921, DEC-1921.1). A misspelt option is
    /// refused by its own name, not as "matches no variant".
    #[test]
    fn sequence_options_round_trip_and_an_identity_keeps_its_pair_without_them() {
        let yaml = "table: app.t\ncolumns:\n  id:\n    type: integer\n    nullable: false\n    identity: {seed: 5, increment: 1, min: 5, max: 1000, cycle: true, cache: 20}\n  n:\n    type: bigserial\n    sequence: {start: 10, max: 99999, cache: 50}\n  plain:\n    type: integer\n    nullable: false\n    identity: [1, 1]\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        let options = |c: &str| match &t.table.columns[c].value_source {
            pbps_model::ValueSource::Identity(id) => (id.seed, id.options),
            pbps_model::ValueSource::Sequence(s) => (s.start, s.options),
            other @ (pbps_model::ValueSource::None
            | pbps_model::ValueSource::Default(_)
            | pbps_model::ValueSource::Generated(_)) => panic!("{c}: {other:?}"),
        };
        assert_eq!(
            options("id"),
            (
                5,
                pbps_model::SequenceOptions {
                    min: Some(5),
                    max: Some(1000),
                    cycle: true,
                    cache: Some(20),
                }
            )
        );
        assert_eq!(
            options("n"),
            (
                10,
                pbps_model::SequenceOptions {
                    max: Some(99999),
                    cache: Some(50),
                    ..Default::default()
                }
            )
        );
        assert_eq!(
            options("plain"),
            (1, pbps_model::SequenceOptions::default())
        );
        let out = render(&t.name, &t.table, &t.intents, None);
        assert!(
            out.contains(
                "identity: {seed: 5, increment: 1, min: 5, max: 1000, cycle: true, cache: 20}"
            ),
            "{out}"
        );
        assert!(
            out.contains("sequence: {start: 10, max: 99999, cache: 50}"),
            "{out}"
        );
        assert!(out.contains("identity: [1, 1]"), "{out}");

        let misspelt = "table: app.t\ncolumns:\n  id:\n    type: integer\n    nullable: false\n    identity: {seed: 1, maximum: 5}\n";
        let err = format!(
            "{:?}",
            crate::load_table_str(Path::new("t.yml"), misspelt).unwrap_err()
        );
        assert!(err.contains("maximum"), "{err}");
        assert!(!err.contains("variant"), "{err}");
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
            assert_eq!((st.start.raw(), st.end.raw()), ("valid_from", "valid_to"));
            assert_eq!(st.hidden, hidden, "{block}");
            assert_eq!(
                st.versioning.as_ref().map(|v| v.history.raw().to_string()),
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

    /// A partition's own foreign keys are written under its entry, before its
    /// checks and indexes as a table's are, and read back as its own, with
    /// their actions (#1632). The parent's stay the parent's, and a partition
    /// with none of its own stays on one line.
    #[test]
    fn a_partitions_own_foreign_keys_round_trip_under_its_entry() {
        let yaml = "table: app.ev\ncolumns:\n  ts: {type: date, nullable: false}\n  u: {type: int}\n  w: {type: int}\n\nforeign_keys:\n  ev_u_fk:\n    columns: [u]\n    references: app.users(id)\n\npartition_by: [ts]\n\npartitions:\n  ev_old:\n    from: [MINVALUE]\n    to: [\"2025-01-01\"]\n    foreign_keys:\n      old_w_fk:\n        columns: [w]\n        references: app.archive(id)\n        on_delete: cascade\n    checks:\n      old_ck: w > 0\n  ev_plain: {from: [\"2025-01-01\"], to: [MAXVALUE]}\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        let own = |n: &str| {
            &t.partitions
                .iter()
                .find(|(name, _)| name.name == n)
                .unwrap_or_else(|| panic!("{n}"))
                .1
        };
        let fk = &own("ev_old").foreign_keys["old_w_fk"];
        assert_eq!(fk.columns, [pbps_model::ColumnName::from("w")]);
        assert_eq!(fk.references_table.raw().to_string(), "app.archive");
        assert_eq!(fk.on_delete, pbps_model::ReferentialAction::Cascade);
        assert!(own("ev_plain").foreign_keys.is_empty());
        assert_eq!(t.table.foreign_keys.keys().collect::<Vec<_>>(), ["ev_u_fk"]);
        let partitions: Vec<_> = t.partitions.iter().map(|(n, p)| (n, p)).collect();
        let out = render_partitioned(&t.name, &t.table, &partitions, &[], None);
        assert!(
            out.contains("\n  ev_plain: {from: [\"2025-01-01\"], to: [MAXVALUE]}\n"),
            "{out}"
        );
        // Negative: a foreign key whose reference does not parse is refused
        // under a partition as under a table.
        let bad = yaml.replace("app.archive(id)", "app.archive");
        assert!(crate::load_table_str(Path::new("t.yml"), &bad).is_err());
    }

    /// A partition's persistence and storage parameters are written under
    /// its entry, last, and read back as its own, spelled as a table's
    /// (#1580). One with neither stays on one line.
    #[test]
    fn a_partitions_own_persistence_and_storage_round_trip_under_its_entry() {
        let yaml = "table: app.ev\ncolumns:\n  ts: {type: date, nullable: false}\n\npartition_by: [ts]\n\npartitions:\n  ev_old:\n    from: [MINVALUE]\n    to: [\"2025-01-01\"]\n    unlogged: true\n    storage_parameters:\n      autovacuum_vacuum_scale_factor: 0.05\n      fillfactor: 70\n  ev_plain: {from: [\"2025-01-01\"], to: [MAXVALUE]}\n  ev_rest:\n    default: true\n    unlogged: true\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        let own = |n: &str| {
            &t.partitions
                .iter()
                .find(|(name, _)| name.name == n)
                .unwrap_or_else(|| panic!("{n}"))
                .1
        };
        assert!(own("ev_old").unlogged);
        assert_eq!(own("ev_old").storage_parameters["fillfactor"], "70");
        assert!(own("ev_rest").unlogged && own("ev_rest").storage_parameters.is_empty());
        // Negative: the parent and a partition with neither declare none.
        assert!(!own("ev_plain").unlogged && own("ev_plain").storage_parameters.is_empty());
        assert!(!t.table.unlogged && t.table.storage_parameters.is_empty());
        let partitions: Vec<_> = t.partitions.iter().map(|(n, p)| (n, p)).collect();
        let out = render_partitioned(&t.name, &t.table, &partitions, &[], None);
        assert!(
            out.contains("\n  ev_plain: {from: [\"2025-01-01\"], to: [MAXVALUE]}\n"),
            "{out}"
        );
        assert!(
            out.contains("\n  ev_rest:\n    default: true\n    unlogged: true\n"),
            "{out}"
        );
        // Negative: a TOAST parameter, or a value the engine would refuse,
        // is refused as on a table.
        for bad in ["toast.autovacuum_enabled: false", "fillfactor: '08'"] {
            let yaml = format!(
                "table: app.ev\ncolumns:\n  ts: {{type: date}}\n\npartition_by: [ts]\n\npartitions:\n  p:\n    default: true\n    storage_parameters:\n      {bad}\n"
            );
            assert!(
                crate::load_table_str(Path::new("t.yml"), &yaml).is_err(),
                "{bad} should not load"
            );
        }
    }

    /// A partition's own column defaults and NOT NULLs are written under its
    /// entry's `columns:`, after the bound, and read back as its own; the
    /// parent's columns stay the parent's (#1578).
    #[test]
    fn a_partitions_own_column_defaults_and_not_nulls_round_trip_under_its_entry() {
        let yaml = "table: app.ev\ncolumns:\n  ts: {type: date, nullable: false}\n  v: {type: int}\n  w: {type: text, default: \"'x'\"}\n\npartition_by: [ts]\n\npartitions:\n  ev_old:\n    from: [MINVALUE]\n    to: [\"2025-01-01\"]\n    columns:\n      v: {default: \"7\", nullable: false}\n      w: {default: \"'y'::text\"}\n    checks:\n      old_ck: v < 100\n  ev_rest:\n    default: true\n    columns:\n      v: {nullable: false}\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        let own = |n: &str| {
            t.partitions
                .iter()
                .find(|(name, _)| name.name == n)
                .and_then(|(_, p)| p.partition_of.clone())
                .unwrap_or_else(|| panic!("{n}"))
                .columns
        };
        let old = own("ev_old");
        assert_eq!(
            old.keys().cloned().collect::<Vec<_>>(),
            [
                pbps_model::ColumnName::from("v"),
                pbps_model::ColumnName::from("w")
            ]
        );
        assert_eq!(old["v"].default.as_deref(), Some("7"));
        assert!(old["v"].not_null);
        assert_eq!(old["w"].default.as_deref(), Some("'y'::text"));
        assert!(!old["w"].not_null);
        let rest = own("ev_rest");
        assert_eq!(rest["v"].default, None);
        assert!(rest["v"].not_null);
        // The partition declares no column of its own.
        assert!(t.partitions.iter().all(|(_, p)| p.columns.is_empty()));
        let partitions: Vec<_> = t.partitions.iter().map(|(n, p)| (n, p)).collect();
        let out = render_partitioned(&t.name, &t.table, &partitions, &[], None);
        assert!(
            out.contains("\n    to: [\"2025-01-01\"]\n    columns:\n      v: {default: \"7\", nullable: false}\n"),
            "{out}"
        );
        assert!(
            out.contains(
                "\n  ev_rest:\n    default: true\n    columns:\n      v: {nullable: false}\n"
            ),
            "{out}"
        );
        // Negative: a column with nothing of its own, `nullable: true`, or a
        // type, is refused by name.
        let parent = "table: app.ev\ncolumns:\n  ts: {type: date, nullable: false}\n  v: {type: int}\npartition_by: [ts]\npartitions:\n  p:\n    default: true\n    columns:\n";
        for (column, expected) in [
            ("      v: {}\n", "declares nothing of its own"),
            ("      v: {default: }\n", "declares nothing of its own"),
            ("      v: {nullable: true}\n", "is `nullable: true`"),
            ("      v: {type: int}\n", "unknown field"),
        ] {
            let yaml = format!("{parent}{column}");
            let err = crate::load_table_str(Path::new("t.yml"), &yaml)
                .err()
                .unwrap_or_else(|| panic!("{column} loaded"));
            let text = format!("{err:?}");
            assert!(text.contains(expected), "{column}: {text}");
        }
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
            Some(vec![pbps_model::ColumnName::from("ts")])
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
                (n.raw().to_string(), of.bound)
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
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(scalar_corpus())
        {
            let child = TableName::new("app", "p1");
            let table = pbps_model::Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: TableName::new("app", "t"),
                    bound: B::Range {
                        from: vec![value(&v)],
                        to: vec![D::MaxValue],
                    },
                    columns: Default::default(),
                }),
                ..Default::default()
            };
            let parent = pbps_model::Table {
                partition_by: Some(pbps_model::PartitionBy {
                    external: false,
                    columns: vec!["a".into()],
                    strategy: Default::default(),
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

    /// A LIST or HASH parent (#1632) is `partition_by: {list: [..]}` or
    /// `{hash: [..]}`, its partitions `{in: [..]}` and `{modulus, remainder}`;
    /// each loads as written and renders back as itself. YAML `null` is SQL
    /// NULL, a quoted `'NULL'` the text; a whole number or a boolean may be
    /// written bare and is the engine's text either way.
    #[test]
    fn list_and_hash_partitions_load_as_written_and_render_back() {
        use pbps_model::{ListDatum as L, PartitionBound as B, PartitionStrategy as S};
        let yaml = "table: app.l\ncolumns:\n  k: {type: text}\n\npartition_by: {list: [k]}\n\npartitions:\n  l_ab: {in: [a, \"it's\"]}\n  l_null: {in: [NULL, \"NULL\"]}\n  l_rest: default\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        assert_eq!(
            t.table.partition_by.as_ref().map(|p| p.strategy),
            Some(S::List)
        );
        let bound = |i: usize| {
            t.partitions[i]
                .1
                .partition_of
                .as_ref()
                .map(|p| p.bound.clone())
        };
        let v = |s: &str| L::Value(s.to_owned());
        assert_eq!(
            bound(0),
            Some(B::List {
                values: vec![v("a"), v("it's")].into_iter().collect()
            })
        );
        assert_eq!(
            bound(1),
            Some(B::List {
                values: vec![L::Null, v("NULL")].into_iter().collect()
            })
        );
        assert_eq!(bound(2), Some(B::Default));

        let yaml = "table: app.h\ncolumns:\n  a: {type: int}\n  b: {type: text}\n\npartition_by: {hash: [a, b]}\n\npartitions:\n  h0: {modulus: 2, remainder: 0}\n  h1: {modulus: 2, remainder: 1}\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        assert_eq!(
            t.table.partition_by.as_ref().map(|p| p.strategy),
            Some(S::Hash)
        );
        assert_eq!(
            t.partitions[1]
                .1
                .partition_of
                .as_ref()
                .map(|p| p.bound.clone()),
            Some(B::Hash {
                modulus: 2,
                remainder: 1
            })
        );

        // The values are a set: written in another order, or with one
        // repeated, the bound is the same one and renders the same, so a
        // reordered list plans nothing (#1632 review).
        let load = |values: &str| {
            crate::load_table_str(
                Path::new("t.yml"),
                &format!(
                    "table: app.l\ncolumns:\n  k: {{type: text}}\npartition_by: {{list: [k]}}\npartitions:\n  l1: {{in: [{values}]}}\n"
                ),
            )
            .unwrap()
        };
        let (written, reordered) = (load("a, b"), load("b, a, b"));
        assert_eq!(written.partitions, reordered.partitions);
        let render_of = |t: &crate::LoadedTable| {
            let partitions: Vec<_> = t.partitions.iter().map(|(n, p)| (n, p)).collect();
            render_partitioned(&t.name, &t.table, &partitions, &[], None)
        };
        assert_eq!(render_of(&written), render_of(&reordered));
        // Negative: another value is another bound.
        assert_ne!(written.partitions, load("a, c").partitions);

        // Bare numbers and booleans are their text.
        let t = crate::load_table_str(
            Path::new("t.yml"),
            "table: app.n\ncolumns:\n  a: {type: int}\npartition_by: {list: [a]}\npartitions:\n  n1: {in: [-5, 7, true]}\n",
        )
        .unwrap();
        assert_eq!(
            t.partitions[0]
                .1
                .partition_of
                .as_ref()
                .map(|p| p.bound.clone()),
            Some(B::List {
                values: vec![v("-5"), v("7"), v("true")].into_iter().collect()
            })
        );

        // Negative: a decimal whose YAML reading is not the text, a hash entry
        // missing half or mixed with a list's, and an unknown strategy. A
        // bound of the other kind loads and is `check_partitions`'s to refuse.
        for block in [
            "partition_by: {list: [a]}\npartitions:\n  p1: {in: [1.5]}\n",
            "partition_by: {hash: [a]}\npartitions:\n  p1: {modulus: 2}\n",
            "partition_by: {hash: [a]}\npartitions:\n  p1: {modulus: 2, remainder: 0, in: ['1']}\n",
            "partition_by: {range: [a]}\n",
            "partition_by: {list: [a], hash: [a]}\n",
        ] {
            let yaml = format!("table: app.t\ncolumns:\n  a: {{type: int}}\n{block}");
            assert!(
                crate::load_table_str(Path::new("t.yml"), &yaml).is_err(),
                "{block} should not load"
            );
        }
    }

    /// `partitions: external` (#1533) declares the parent alone: no
    /// partition is loaded, the key says so, and it renders back as written.
    /// Any other word, and the setting without a key, are refused by name.
    #[test]
    fn external_partitions_are_a_setting_on_the_parent_alone() {
        let yaml = "table: app.ev\ncolumns:\n  ts: {type: date, nullable: false}\n\npartition_by: \
                    [ts]\n\npartitions: external\n";
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        assert!(t.partitions.is_empty());
        assert_eq!(
            t.table.partition_by,
            Some(pbps_model::PartitionBy {
                columns: vec!["ts".into()],
                external: true,
                strategy: Default::default(),
            })
        );
        let out = render(&t.name, &t.table, &[], None);
        assert!(
            out.ends_with("\npartition_by: [ts]\n\npartitions: external\n"),
            "{out}"
        );
        let back = crate::load_table_str(Path::new("t.yml"), &out).unwrap();
        assert_eq!(back.table, t.table, "{out}");
        assert!(back.partitions.is_empty());
        // Negative: a word that is not the setting.
        let errs = crate::load_table_str(Path::new("t.yml"), &yaml.replace("external", "externl"))
            .unwrap_err();
        assert!(
            errs.iter().any(|e| e
                .to_string()
                .contains("`partitions:` is `externl`: write `external`")),
            "{errs:?}"
        );
        // Negative: the setting without a key.
        let errs = crate::load_table_str(
            Path::new("t.yml"),
            "table: app.ev\ncolumns:\n  ts: {type: date}\npartitions: external\n",
        )
        .unwrap_err();
        assert!(
            errs.iter().any(|e| e
                .to_string()
                .contains("`partitions: external` is declared without `partition_by:`")),
            "{errs:?}"
        );
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

    /// A type reference with quoted parts needs a YAML scalar that reads back
    /// as the same reference, and `fmt` writes one that is single-quoted so the
    /// name stays readable (#1523). The same reference written another way is
    /// rewritten to the canonical spelling, and the output does not move again.
    #[test]
    fn a_quoted_type_reference_is_written_as_a_scalar_that_reads_back() {
        let yaml = "table: app.t\ncolumns:\n  a:\n    type: '\"App\".\"Mood\"'\n  b:\n    type: '\"App\".\"Mood\"[]'\n  c:\n    type: APP.Mood\n  d:\n    type: '\"it''s\".\"a\"\"b\"'\n  e:\n    type: \"app.\\\"x\\ty\\\"\"\n";
        round_trip(yaml);
        let t = crate::load_table_str(Path::new("t.yml"), yaml).unwrap();
        assert_eq!(t.table.columns["a"].ty.to_string(), "\"App\".\"Mood\"");
        assert!(t.table.columns["b"].ty.is_array());
        assert_eq!(t.table.columns["c"].ty.to_string(), "app.mood");
        assert_eq!(
            t.table.columns["d"]
                .ty
                .type_ref()
                .map(|r| (r.schema(), r.name())),
            Some(("it's", "a\"b"))
        );
        let out = render(&t.name, &t.table, &t.intents, t.strategy.as_ref());
        assert!(out.contains("type: '\"App\".\"Mood\"'"), "{out}");
        assert!(out.contains("type: '\"App\".\"Mood\"[]'"), "{out}");
        assert!(out.contains("type: app.mood\n"), "{out}");
        assert!(out.contains("type: '\"it''s\".\"a\"\"b\"'"), "{out}");
        // A character that cannot sit raw on one line keeps the escapes.
        assert!(out.contains("type: \"app.\\\"x\\ty\\\"\""), "{out}");
        // Negative: a reference no dialect could hold does not load.
        for bad in ["a.b.c", "app.", "app.mood(3)", "app.\"\""] {
            let yaml = format!("table: app.t\ncolumns:\n  a:\n    type: '{bad}'\n");
            assert!(
                crate::load_table_str(Path::new("t.yml"), &yaml).is_err(),
                "{bad} should not load"
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
        assert_eq!(
            t.table.primary_key.as_ref().unwrap().columns,
            [pbps_model::ColumnName::from("n")]
        );
        assert_eq!(
            t.table.unique["uq_y"].columns,
            [pbps_model::ColumnName::from("y")]
        );
        assert_eq!(
            t.table.unique["uq_on"].columns,
            [pbps_model::ColumnName::from("on")]
        );
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
