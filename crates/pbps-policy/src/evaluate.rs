//! Running the catalogue: the declaration rules over a [`Schema`], the plan
//! rules over a [`ChangeSet`] (ADR-0008 decisions 1 and 6).
//!
//! Every rule reads typed data. The plan rules see the same [`Change`] values
//! the emitter sees and never the SQL it writes — a rule that inspected
//! statements would be string inspection of what constraint 3 keeps in one
//! place.

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{Change, ChangeSet, Finding, RiskClass, Schema, Severity, TableName};

use crate::civil;
use crate::config::Policies;
use crate::rules::{self, Point};

/// What an evaluation needs besides the block.
#[derive(Debug, Clone)]
pub struct Context {
    /// Unix seconds, for the change window and for suppression expiry.
    pub now: i64,
    /// The row count `data.max-rows` uses when the block does not set one —
    /// the project's `max_data_rows`, or the model's default.
    pub max_rows: usize,
    /// Whether this is a connected plan. Off, the connected-only rules are not
    /// evaluated at all: an offline preview has no moment to be inside a
    /// window of.
    pub connected: bool,
    /// Only these objects are evaluated when set (`validate --since`): the
    /// subjects, as a finding names them.
    pub only: Option<BTreeSet<String>>,
    /// The column base types the target engine has deprecated, with the
    /// advice each finding gives — the dialect's answer, handed over by the
    /// caller because this crate holds no dialect knowledge (DECISIONS 187,
    /// DEC-1869.1).
    pub deprecated_types: &'static [(&'static str, &'static str)],
}

impl Context {
    fn today(&self) -> String {
        civil::at(self.now, 0).date()
    }

    fn wants(&self, subject: &str) -> bool {
        self.only.as_ref().is_none_or(|only| only.contains(subject))
    }
}

/// A rule's live setting, with the `off` and the suppression already applied.
pub(crate) struct Live<'a> {
    id: &'static str,
    severity: Severity,
    config: crate::config::RuleConfig,
    policies: &'a Policies,
    today: String,
}

impl Live<'_> {
    pub(crate) fn of<'a>(
        policies: &'a Policies,
        id: &'static str,
        ctx: &Context,
    ) -> Option<Live<'a>> {
        let effective = policies.effective(id);
        let severity = effective.severity?;
        Some(Live {
            id,
            severity,
            config: effective.config,
            policies,
            today: ctx.today(),
        })
    }

    pub(crate) fn finding(
        &self,
        subject: Option<&str>,
        message: impl Into<String>,
    ) -> Option<Finding> {
        if self.policies.suppressed(self.id, subject, &self.today) {
            return None;
        }
        Some(Finding::new(
            self.id,
            self.severity,
            message,
            subject.map(str::to_owned),
        ))
    }
}

/// The declaration rules over the whole schema (the `validate` point).
pub fn declarations(schema: &Schema, policies: &Policies, ctx: &Context) -> Vec<Finding> {
    // These messages are raw comparison keys for partitions extend. The CLI
    // projects them to prose only after comparing the original (id, message).
    let mut out = Vec::new();

    for (id, what) in [
        (rules::NAMING_TABLE, Naming::Table),
        (rules::NAMING_COLUMN, Naming::Column),
        (rules::NAMING_INDEX, Naming::Index),
        (rules::NAMING_CONSTRAINT, Naming::Constraint),
    ] {
        let Some(live) = Live::of(policies, id, ctx) else {
            continue;
        };
        let Some(pattern) = &live.config.pattern else {
            // Switched on with nothing to match against says nothing.
            continue;
        };
        let Ok(re) = regex_lite::Regex::new(&format!("^(?:{pattern})$")) else {
            continue; // `check` has already reported it.
        };
        for (name, table) in &schema.tables {
            let subject = name.raw().to_string();
            if !ctx.wants(&subject) {
                continue;
            }
            let names: Vec<(&str, String)> = match what {
                Naming::Table => vec![("table", name.name.clone())],
                // A SQL Server computed column (#1174) is held beside the
                // columns, and its name is a column name all the same (#1433).
                Naming::Column => table
                    .columns
                    .keys()
                    .map(|c| ("column", c.raw().to_owned()))
                    .chain(
                        table
                            .computed
                            .keys()
                            .map(|c| ("computed column", c.raw().to_owned())),
                    )
                    .collect(),
                Naming::Index => table
                    .indexes
                    .keys()
                    .map(|i| ("index", i.raw().to_owned()))
                    .collect(),
                Naming::Constraint => table
                    .primary_key
                    .iter()
                    .filter_map(|pk| pk.name.as_ref().map(|n| n.raw().to_owned()))
                    .map(|n| ("primary key", n))
                    .chain(
                        table
                            .unique
                            .keys()
                            .map(|n| ("unique constraint", n.raw().to_owned())),
                    )
                    .chain(
                        table
                            .foreign_keys
                            .keys()
                            .map(|n| ("foreign key", n.raw().to_owned())),
                    )
                    .chain(
                        table
                            .checks
                            .keys()
                            .map(|n| ("check constraint", n.raw().to_owned())),
                    )
                    .collect(),
            };
            for (kind, n) in names {
                if !re.is_match(&n) {
                    out.extend(live.finding(
                        Some(&subject),
                        format!(
                            "{name}: {kind} `{n}` does not match `{pattern}`",
                            name = name.raw()
                        ),
                    ));
                }
            }
        }
    }

    if let Some(live) = Live::of(policies, rules::DATA_MAX_ROWS, ctx) {
        let max = live.config.rows.unwrap_or(ctx.max_rows);
        for (name, table) in &schema.tables {
            let subject = name.raw().to_string();
            if !ctx.wants(&subject) {
                continue;
            }
            if let Some(data) = &table.data
                && data.rows.len() > max
            {
                out.extend(live.finding(
                    Some(&subject),
                    format!(
                        "{name}: {} declared rows is above {max} — this does not look like \
                         reference data, and every plan compares it row by row",
                        data.rows.len(),
                        name = name.raw()
                    ),
                ));
            }
        }
    }

    if let Some(live) = Live::of(policies, rules::COLUMN_NO_DEPRECATED_TYPE, ctx) {
        for (name, table) in &schema.tables {
            let subject = name.raw().to_string();
            if !ctx.wants(&subject) {
                continue;
            }
            for (column, spec) in &table.columns {
                if let Some((_, advice)) = ctx
                    .deprecated_types
                    .iter()
                    .find(|(base, _)| *base == spec.ty.base)
                {
                    out.extend(live.finding(
                        Some(&subject),
                        format!(
                            "{name}.{column}: `{}` {advice}",
                            spec.ty.raw(),
                            name = name.raw(),
                            column = column.raw(),
                        ),
                    ));
                }
            }
        }
    }

    out
}

#[derive(Clone, Copy)]
enum Naming {
    Table,
    Column,
    Index,
    Constraint,
}

/// Each change's table as one identity across the plan's table renames, or
/// `None` for a change to no table. A change is addressed by the name its
/// table has where it runs: a drop moved ahead of its table's rename names
/// the source, and the add after it the destination (`object_order`,
/// `rename_order`). Walking the plan in order, a rename hands its source's
/// identity to its destination, and a table that takes the source name
/// afterwards is a new one.
fn table_identities(cs: &ChangeSet) -> Vec<Option<usize>> {
    let mut current: BTreeMap<&TableName, usize> = BTreeMap::new();
    let mut next = 0;
    cs.changes
        .iter()
        .map(|p| {
            let name = p.change.table()?;
            let id = *current.entry(name).or_insert_with(|| {
                next += 1;
                next - 1
            });
            if let Change::RenameTable { from, to, .. } = &p.change {
                current.remove(from);
                current.insert(to, id);
            }
            Some(id)
        })
        .collect()
}

/// The plan rules over a change set (the `plan` point). Each finding names
/// the index of the change it attaches to.
pub fn plan(cs: &ChangeSet, policies: &Policies, ctx: &Context) -> Vec<(usize, Finding)> {
    let mut out = Vec::new();

    if let Some(live) = Live::of(policies, rules::CHANGE_EXPAND_CONTRACT, ctx) {
        // Per table: the first add, and the first drop or narrowing, with
        // the name the table has there. A table is one identity across the
        // plan's renames, since a drop is addressed by the name its table
        // has where it runs (DEC-1433.1).
        let tables = table_identities(cs);
        let mut adds: BTreeMap<usize, usize> = BTreeMap::new();
        let mut contracts: BTreeMap<usize, (usize, &TableName)> = BTreeMap::new();
        // The column names this change set adds and drops, per table, of
        // either kind. A computed column dropped and added back under one
        // name is an expression rebuild, and one converted to or from a
        // stored column keeps its name too: readers find it there
        // throughout, so neither half counts (DEC-1433.1).
        let mut added: BTreeSet<(usize, &str)> = BTreeSet::new();
        let mut dropped: BTreeSet<(usize, &str)> = BTreeSet::new();
        for (p, table) in cs.changes.iter().zip(&tables) {
            let Some(table) = *table else { continue };
            if let Change::AddColumn { name, .. } | Change::AddComputedColumn { name, .. } =
                &p.change
            {
                added.insert((table, name.raw()));
            }
            if let Change::DropColumn { column, .. } = &p.change {
                dropped.insert((table, column.name.raw()));
            }
            if let Change::DropComputedColumn { name, .. } = &p.change {
                dropped.insert((table, name.raw()));
            }
        }
        for (i, p) in cs.changes.iter().enumerate() {
            let id = tables[i];
            // Named arms, not a wildcard: every other kind of change is
            // neither side of the pattern, and a new kind that is (a column
            // drop spelled differently, say) should have to be placed here.
            let contract = match &p.change {
                Change::RebuildPartitionedTable(_)
                | Change::BuildPartitionedIndex(_)
                | Change::CopyTableRows(_)
                | Change::AnalyzeTableTree(_) => None,
                Change::AddColumn { .. } => {
                    if let Some(id) = id {
                        adds.entry(id).or_insert(i);
                    }
                    None
                }
                Change::DropColumn { column, .. } => Some(&column.table),
                // Narrowing, and tightening to NOT NULL: the rule's own words
                // are "drop **or narrow**", and a column that stops accepting
                // NULL accepts less than it did — `intrinsic_risks` calls it
                // "the same data hazard as tightening an existing nullable
                // column". A type change folds a nullability change into
                // itself (§12), so it carries `NotNull` rather than
                // `Narrowing` when that is all it does (DECISIONS 173).
                Change::AlterColumnType {
                    column,
                    from_nullable,
                    to_nullable,
                    ..
                } if p.risks.contains(&RiskClass::Narrowing)
                    || (*from_nullable && !*to_nullable) =>
                {
                    Some(&column.table)
                }
                // Keyed on the change, not on `RiskClass::NotNull`: a NOT NULL
                // column *addition* with no value source carries that risk too
                // (SPEC §7.1), and it is an add. Asking the risk would make one
                // added column both sides of the pattern and fire the rule on
                // it alone.
                Change::AlterColumnNullability {
                    column,
                    to_nullable: false,
                    ..
                } => Some(&column.table),
                // A partition's own NOT NULL narrows its column the same way
                // (#1581).
                Change::SetPartitionNotNull {
                    table,
                    not_null: true,
                    ..
                } => Some(table),
                // A computed column is a reader-facing name like any other:
                // dropping one and adding another moves readers (DEC-1433.1).
                // Under one name it is a rebuild, which `added`/`dropped`
                // leave out.
                Change::AddComputedColumn { name, .. } => {
                    if let Some(id) = id
                        && !dropped.contains(&(id, name.raw()))
                    {
                        adds.entry(id).or_insert(i);
                    }
                    None
                }
                Change::DropComputedColumn { table, name, .. } => id
                    .is_none_or(|id| !added.contains(&(id, name.raw())))
                    .then_some(table),
                Change::AlterColumnType { .. }
                | Change::CreateTable { .. }
                | Change::DetachPartition { .. }
                | Change::AttachPartition { .. }
                | Change::DropTable { .. }
                | Change::RenameTable { .. }
                | Change::RenameColumn { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::SetPartitionDefault { .. }
                | Change::SetPartitionNotNull { .. }
                | Change::AlterColumnExpression { .. }
                | Change::AlterColumnNumbering { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
                | Change::SetIndexStorageParameters { .. }
                | Change::SetTablePersistence { .. }
                | Change::SetStorageParameters { .. }
                | Change::DropPolicy { .. }
                | Change::CreatePolicy { .. }
                | Change::SetRowSecurity { .. }
                | Change::SetReplicaIdentity { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::AddForeignKey { .. }
                | Change::DropForeignKey { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::AddIndex { .. }
                | Change::DropIndex { .. }
                | Change::InsertRow { .. }
                | Change::UpdateRow { .. }
                | Change::DeleteRow { .. }
                | Change::SetDataMode { .. }
                | Change::SetEnsureKeys { .. }
                | Change::CreateModule { .. }
                | Change::AlterModule { .. }
                | Change::DropModule { .. }
                | Change::AddEnumValue { .. }
                | Change::AlterDomain { .. }
                | Change::RenameEnumValue { .. }
                | Change::EnumRebuild { .. }
                | pbps_model::catalog_change!()
                | Change::Grant { .. }
                | Change::Revoke { .. }
                | Change::PublicExecution { .. } => None,
            };
            if let (Some(table), Some(id)) = (contract, id) {
                contracts.entry(id).or_insert((i, table));
            }
        }
        for (id, (at, table)) in contracts {
            if adds.contains_key(&id) {
                let subject = table.raw().to_string();
                out.extend(
                    live.finding(
                        Some(&subject),
                        format!(
                            "{table}: this revision both adds and drops or narrows a column; \
                             that usually wants expand/contract staging (SPEC §13.3) — add \
                             first, backfill, then contract in a later revision",
                            table = table.shown()
                        ),
                    )
                    .map(|f| (at, f)),
                );
            }
        }
    }

    if let Some(live) = Live::of(policies, rules::CHANGE_NARROWING_ON_DATA, ctx) {
        for (i, p) in cs.changes.iter().enumerate() {
            if let Change::AlterColumnType {
                column,
                from,
                to,
                using,
                ..
            } = &p.change
                && p.risks.contains(&RiskClass::Narrowing)
            {
                let subject = column.table.raw().to_string();
                // A recorded conversion (#2331) is read over every row by the
                // pre-flight probe, an array's included, so the array
                // sentence below, which says the plan is refused, would be
                // false for it.
                //
                // An array's element change with no record has no probe: it
                // is supported only where every element keeps its value, and
                // refused when the plan is written against a database
                // otherwise (#1167).
                let message = if using.is_some() {
                    format!(
                        "{column}: {from} -> {to} is converted by the recorded expression; \
                         the pre-flight probe evaluates it over every row before the first \
                         statement runs",
                        column = column.shown(),
                        from = from.shown(),
                        to = to.shown(),
                    )
                } else if from.is_array() || to.is_array() {
                    format!(
                        "{column}: {from} -> {to} narrows or converts an array's elements, \
                         which pbps does not do: the plan is refused when it is computed \
                         against a database",
                        column = column.shown(),
                        from = from.shown(),
                        to = to.shown(),
                    )
                } else {
                    format!(
                        "{column}: {from} -> {to} depends on what is already stored; the \
                         pre-flight probe counts the rows that would not fit before the \
                         first statement runs",
                        column = column.shown(),
                        from = from.shown(),
                        to = to.shown(),
                    )
                };
                out.extend(live.finding(Some(&subject), message).map(|f| (i, f)));
            }
        }
    }

    if let Some(live) = Live::of(policies, rules::GRANT_WIDEN, ctx) {
        for (i, p) in cs.changes.iter().enumerate() {
            if let Change::Grant {
                role,
                target,
                permissions,
            } = &p.change
            {
                let subject = format!("role {}", role.raw());
                out.extend(
                    live.finding(
                        Some(&subject),
                        format!(
                            "role {role} gains {} on {target}",
                            permissions
                                .iter()
                                .map(|p| p.as_str())
                                .collect::<Vec<_>>()
                                .join(", "),
                            role = role.shown(),
                            target = target.shown(),
                        ),
                    )
                    .map(|f| (i, f)),
                );
            }
        }
    }

    if ctx.connected
        && !cs.changes.is_empty()
        && let Some(live) = Live::of(policies, rules::CHANGE_WINDOW, ctx)
    {
        debug_assert_eq!(
            rules::rule(rules::CHANGE_WINDOW).map(|r| r.point),
            Some(Point::ConnectedPlan)
        );
        let offset = live
            .config
            .offset
            .as_deref()
            .and_then(|o| civil::parse_offset(o).ok())
            .unwrap_or(0);
        let now = civil::at(ctx.now, offset);
        let inside = live
            .config
            .allow
            .iter()
            .filter_map(|w| parse_window(w).ok())
            .any(|w| w.contains(&now));
        if !inside {
            out.extend(
                live.finding(
                    None,
                    format!(
                        "computed at {} {:02}:{:02} (UTC{}), outside every allowed change \
                         window: {}",
                        now.date(),
                        now.hour,
                        now.minute,
                        live.config.offset.as_deref().unwrap_or("+00:00"),
                        live.config.allow.join(", ")
                    ),
                )
                .map(|f| (0, f)),
            );
        }
    }

    out
}

/// One allowed window: some weekdays, and a time range within each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Monday-based.
    pub days: BTreeSet<u32>,
    /// Minutes since midnight, `from` inclusive, `to` exclusive.
    pub from: u32,
    pub to: u32,
}

impl Window {
    fn contains(&self, at: &civil::Civil) -> bool {
        self.days.contains(&at.weekday) && (self.from..self.to).contains(&at.minute_of_day())
    }
}

/// `mon-fri 09:00-17:00`, `sat,sun 00:00-24:00`, `wed 22:00-24:00`.
pub fn parse_window(s: &str) -> Result<Window, String> {
    let (days, hours) = s
        .trim()
        .split_once(char::is_whitespace)
        .ok_or_else(|| "a window is `<days> <from>-<to>`, e.g. `mon-fri 09:00-17:00`".to_owned())?;
    let mut set = BTreeSet::new();
    for part in days.split(',') {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (civil::parse_weekday(a)?, civil::parse_weekday(b)?);
                if a <= b {
                    set.extend(a..=b);
                } else {
                    set.extend((a..=6).chain(0..=b));
                }
            }
            None => {
                set.insert(civil::parse_weekday(part)?);
            }
        }
    }
    let (from, to) = hours
        .trim()
        .split_once('-')
        .ok_or_else(|| "the time range is `<from>-<to>`, e.g. `09:00-17:00`".to_owned())?;
    let (from, to) = (civil::parse_hhmm(from)?, civil::parse_hhmm(to)?);
    if from >= to {
        return Err(format!("`{}` ends before it starts", hours.trim()));
    }
    Ok(Window {
        days: set,
        from,
        to,
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn enum_labels_do_not_become_table_expand_contract_operations() {
        let add = PlannedChange::new(Change::AddEnumValue {
            uid: "y_000001".parse().unwrap(),
            name: "app.e".parse().unwrap(),
            value: "blue".into(),
            placement: None,
            ordinal: 0,
            definition: Box::new(pbps_model::ManagedType::Enum {
                values: vec!["blue".into()],
            }),
        });
        let drop = drop_column("old");
        assert!(
            plan(
                &cs(vec![add.clone(), drop.clone()]),
                &Policies::default(),
                &ctx()
            )
            .is_empty()
        );
        let findings = plan(
            &cs(vec![add, add_column("new"), drop]),
            &Policies::default(),
            &ctx(),
        );
        assert!(
            findings
                .iter()
                .any(|(_, f)| f.id == "change.expand-contract")
        );
    }

    #[test]
    fn grant_policy_keys_stay_raw_while_shared_sentences_show_names_once() {
        let name = "role\nline\\path\u{202e}名前";
        let change = PlannedChange::new(Change::Grant {
            role: name.into(),
            target: pbps_model::GrantTarget::Schema(name.into()),
            permissions: [pbps_model::Permission::Usage].into(),
        });
        let changes = cs(vec![change]);
        let context = ctx();
        let findings = plan(&changes, &Policies::default(), &context);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].1.subject.as_deref(),
            Some(format!("role {name}").as_str())
        );
        assert_eq!(
            findings[0].1.message,
            format!(
                "role {shown} gains usage on schema::{shown}",
                shown = pbps_model::escape::Escaped(name)
            )
        );
        let suppressed = |on: &str| {
            serde_json::from_value::<Policies>(serde_json::json!({"suppress": [
                {"rule": "grant.widen", "on": on, "reason": "approved"}
            ]}))
            .unwrap()
        };
        assert!(plan(&changes, &suppressed(&format!("role {name}")), &context).is_empty());
        assert_eq!(
            plan(
                &changes,
                &suppressed(r"role role\nline\\path\u{202e}名前"),
                &context
            )
            .len(),
            1
        );
    }
    #[test]
    fn declaration_policy_subjects_match_raw_changed_subject_keys() {
        let part = "a\nb\\c\u{200b}";
        let name = pbps_model::TableName::new("app", part);
        let mut schema = schema();
        let mut table = schema.tables.pop_first().unwrap().1;
        table.data = Some(pbps_model::TableData {
            mode: pbps_model::DataMode::Ensure,
            rows: [(pbps_model::RowKey::from("1"), Default::default())].into(),
        });
        schema.tables.insert(name, table);
        let mut context = ctx();
        context.only = Some([format!("app.{part}")].into());
        for config in [
            Policies::default(),
            policies(r#"{"rules":{"naming.table":{"pattern":"never"}}}"#),
            policies(r#"{"rules":{"data.max-rows":{"rows":0}}}"#),
        ] {
            let findings = declarations(&schema, &config, &context);
            assert!(!findings.is_empty(), "{findings:?}");
            assert!(
                findings
                    .iter()
                    .all(|f| f.subject.as_deref() == Some(format!("app.{part}").as_str())),
                "{findings:?}"
            );
        }
        context.only = Some([r"app.a\nb\c\u{200b}".into()].into());
        assert!(declarations(&schema, &Policies::default(), &context).is_empty());
    }

    #[test]
    fn plan_policy_subjects_match_raw_changed_subject_keys() {
        let part = "a\nb\\c\u{200b}";
        let name = pbps_model::TableName::new("app", part);
        let narrow = PlannedChange::new(Change::AlterColumnType {
            uid: "c_aaaaaa".parse().unwrap(),
            column: name.column(part),
            from: "bigint".parse().unwrap(),
            to: "int".parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
            through_external_partitions: false,
            from_collation: None,
            to_collation: None,
            using: None,
        })
        .with_risk(RiskClass::Narrowing);
        let add = PlannedChange::new(Change::AddColumn {
            uid: "c_bbbbbb".parse().unwrap(),
            table: name,
            name: "new".into(),
            column: Box::new(Column::new("int".parse().unwrap())),
        });
        let changes = cs(vec![narrow, add]);
        let mut context = ctx();
        context.only = Some([format!("app.{part}")].into());
        let findings = plan(&changes, &Policies::default(), &context);
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(
            findings
                .iter()
                .all(|(_, f)| f.subject.as_deref() == Some(format!("app.{part}").as_str()))
        );
        let suppressed = |on: &str| {
            serde_json::from_value(serde_json::json!({"suppress": [
                {"rule": "change.expand-contract", "on": on, "reason": "test"},
                {"rule": "change.narrowing-on-data", "on": on, "reason": "test"}
            ]}))
            .unwrap()
        };
        assert_eq!(
            plan(&changes, &suppressed(r"app.a\nb\c\u{200b}"), &context).len(),
            2
        );
        assert!(plan(&changes, &suppressed(&format!("app.{part}")), &context).is_empty());
    }

    use super::*;
    use pbps_model::{Column, ColumnType, PlannedChange, Table};

    fn policies(json: &str) -> Policies {
        serde_json::from_str(json).unwrap()
    }

    fn ctx() -> Context {
        Context {
            // 2026-09-03 10:00 UTC, a Thursday.
            now: 1_788_429_600,
            max_rows: 1000,
            connected: false,
            only: None,
            deprecated_types: SQL_SERVER_LOBS,
        }
    }

    /// SQL Server's list, as its dialect states it; the crate cannot depend on
    /// the dialect, so the tests spell it out.
    const SQL_SERVER_LOBS: &[(&str, &str)] = &[
        (
            "text",
            "has been deprecated since SQL Server 2005; use varchar(max)",
        ),
        (
            "ntext",
            "has been deprecated since SQL Server 2005; use nvarchar(max)",
        ),
        (
            "image",
            "has been deprecated since SQL Server 2005; use varbinary(max)",
        ),
    ];

    fn schema() -> Schema {
        let mut s = Schema::default();
        let mut t = Table::default();
        t.columns.insert(
            "CustomerId".into(),
            Column::new("int".parse::<ColumnType>().unwrap()),
        );
        t.columns.insert(
            "notes".into(),
            Column::new("text".parse::<ColumnType>().unwrap()),
        );
        t.indexes.insert(
            "IX_customer".into(),
            pbps_model::Index {
                columns: vec![],
                include: vec![],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        s.tables.insert("dbo.customer".parse().unwrap(), t);
        s
    }

    fn ids(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.id.as_str()).collect()
    }

    #[test]
    fn with_no_block_only_the_default_on_rules_fire() {
        let f = declarations(&schema(), &Policies::default(), &ctx());
        assert_eq!(ids(&f), ["column.no-deprecated-type"], "{f:?}");
        assert_eq!(f[0].severity, Severity::Warning);
        assert_eq!(f[0].subject.as_deref(), Some("dbo.customer"));
    }

    /// The finding quotes the dialect's advice, not a fixed engine's.
    #[test]
    fn a_deprecated_type_finding_gives_the_dialects_advice() {
        let f = declarations(&schema(), &Policies::default(), &ctx());
        assert_eq!(
            f[0].message,
            "dbo.customer.notes: `text` has been deprecated since SQL Server 2005; \
             use varchar(max)"
        );
    }

    /// `text` is PostgreSQL's ordinary string: a target engine that
    /// deprecates nothing gets no finding, however the rule is configured
    /// (#1869).
    #[test]
    fn a_type_the_target_engine_never_deprecated_is_not_reported() {
        let none = Context {
            deprecated_types: &[],
            ..ctx()
        };
        for p in [
            Policies::default(),
            policies(r#"{"rules": {"column.no-deprecated-type": "error"}}"#),
        ] {
            let f = declarations(&schema(), &p, &none);
            assert!(f.is_empty(), "{f:?}");
        }
        // The same declarations against a list that names the type are
        // reported, so the empty result above is the list's doing.
        let p = policies(r#"{"rules": {"column.no-deprecated-type": "error"}}"#);
        let f = declarations(&schema(), &p, &ctx());
        assert_eq!(ids(&f), ["column.no-deprecated-type"], "{f:?}");
        assert_eq!(f[0].severity, Severity::Error);
    }

    #[test]
    fn a_naming_rule_matches_the_whole_name_and_says_which_name() {
        let p = policies(
            r#"{"rules": {"naming.column": {"severity": "error", "pattern": "[a-z_]+"},
                          "naming.index": {"severity": "warning", "pattern": "ix_.*"},
                          "column.no-deprecated-type": "off"}}"#,
        );
        let f = declarations(&schema(), &p, &ctx());
        assert_eq!(ids(&f), ["naming.column", "naming.index"], "{f:?}");
        assert!(f[0].message.contains("`CustomerId`"), "{f:?}");
        assert_eq!(f[0].severity, Severity::Error);
        assert!(f[1].message.contains("`IX_customer`"), "{f:?}");
        // The negative case: a partial match is not a match. `notes` matched,
        // `CustomerId` would have matched `[a-z_]+` somewhere inside it.
        assert!(!f.iter().any(|x| x.message.contains("`notes`")));
    }

    /// A SQL Server computed column is held beside the columns (#1174), and
    /// its name is a column name all the same (#1433).
    #[test]
    fn a_computed_columns_name_is_held_to_the_column_naming_rule() {
        let mut s = schema();
        let t = s.tables.get_mut(&"dbo.customer".parse().unwrap()).unwrap();
        for name in ["GrossTotal", "net_total"] {
            t.computed.insert(
                name.into(),
                pbps_model::ComputedColumn {
                    expression: "[a] + 1".into(),
                    persisted: false,
                    not_null: false,
                },
            );
        }
        let p = policies(
            r#"{"rules": {"naming.column": {"severity": "error", "pattern": "[a-z_]+"},
                          "column.no-deprecated-type": "off"}}"#,
        );
        let f = declarations(&s, &p, &ctx());
        let messages: Vec<&str> = f.iter().map(|f| f.message.as_str()).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("computed column `GrossTotal`")),
            "{f:?}"
        );
        // The negative case: a matching computed name is not a finding.
        assert!(!messages.iter().any(|m| m.contains("`net_total`")), "{f:?}");
        assert_eq!(ids(&f), ["naming.column", "naming.column"], "{f:?}");
    }

    #[test]
    fn a_suppression_removes_the_finding_and_an_expired_one_does_not() {
        let mut p = policies(
            r#"{"suppress": [{"rule": "column.no-deprecated-type", "on": "dbo.customer",
                              "reason": "legacy", "until": "2030-01-01"}]}"#,
        );
        assert!(declarations(&schema(), &p, &ctx()).is_empty());
        p.suppress[0].until = Some("2026-09-03".to_owned());
        assert_eq!(declarations(&schema(), &p, &ctx()).len(), 1);
        p.suppress[0].until = None;
        p.suppress[0].on = Some("dbo.other".to_owned());
        assert_eq!(declarations(&schema(), &p, &ctx()).len(), 1);
    }

    #[test]
    fn only_the_named_objects_are_evaluated_under_since() {
        let mut c = ctx();
        c.only = Some(["dbo.other".to_owned()].into_iter().collect());
        assert!(declarations(&schema(), &Policies::default(), &c).is_empty());
        c.only = Some(["dbo.customer".to_owned()].into_iter().collect());
        assert_eq!(declarations(&schema(), &Policies::default(), &c).len(), 1);
    }

    #[test]
    fn max_rows_takes_the_projects_number_and_the_rules_own_over_it() {
        let mut s = schema();
        let t = s.tables.get_mut(&"dbo.customer".parse().unwrap()).unwrap();
        t.data = Some(pbps_model::TableData {
            mode: pbps_model::DataMode::Exact,
            rows: (0..3)
                .map(|i| {
                    (
                        pbps_model::RowKey::from(i.to_string()),
                        pbps_model::Row::default(),
                    )
                })
                .collect(),
        });
        let quiet = policies(r#"{"rules": {"column.no-deprecated-type": "off"}}"#);
        let mut c = ctx();
        c.max_rows = 2;
        let f = declarations(&s, &quiet, &c);
        assert_eq!(ids(&f), ["data.max-rows"], "{f:?}");
        let p = policies(
            r#"{"rules": {"column.no-deprecated-type": "off", "data.max-rows": {"rows": 5}}}"#,
        );
        assert!(declarations(&s, &p, &c).is_empty());
    }

    fn cs(changes: Vec<PlannedChange>) -> ChangeSet {
        ChangeSet { changes }
    }

    fn table() -> TableName {
        "dbo.customer".parse().unwrap()
    }

    #[test]
    fn adding_and_dropping_in_one_table_is_the_expand_contract_lint() {
        let add = PlannedChange::new(Change::AddColumn {
            uid: "c_aaaaaa".parse().unwrap(),
            table: table(),
            name: "full_name".into(),
            column: Box::new(Column::new("int".parse().unwrap())),
        });
        let drop = PlannedChange::new(Change::DropColumn {
            uid: "c_bbbbbb".parse().unwrap(),
            column: table().column("name"),
        });
        let f = plan(
            &cs(vec![add.clone(), drop.clone()]),
            &Policies::default(),
            &ctx(),
        );
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].0, 1, "attached to the drop");
        assert_eq!(f[0].1.id, "change.expand-contract");
        // The negative cases: one of the two alone is not a contraction.
        assert!(plan(&cs(vec![add]), &Policies::default(), &ctx()).is_empty());
        assert!(plan(&cs(vec![drop]), &Policies::default(), &ctx()).is_empty());
    }

    fn add_computed(name: &str) -> PlannedChange {
        PlannedChange::new(Change::AddComputedColumn {
            table: table(),
            name: name.into(),
            computed: pbps_model::ComputedColumn {
                expression: "[a] + 1".into(),
                persisted: false,
                not_null: false,
            },
        })
    }

    fn drop_computed(name: &str) -> PlannedChange {
        PlannedChange::new(Change::DropComputedColumn {
            table: table(),
            name: name.into(),
            computed: pbps_model::ComputedColumn {
                expression: "[a] + 2".into(),
                persisted: false,
                not_null: false,
            },
        })
    }

    fn add_column(name: &str) -> PlannedChange {
        PlannedChange::new(Change::AddColumn {
            uid: "c_aaaaaa".parse().unwrap(),
            table: table(),
            name: name.into(),
            column: Box::new(Column::new("int".parse().unwrap())),
        })
    }

    fn drop_column(name: &str) -> PlannedChange {
        PlannedChange::new(Change::DropColumn {
            uid: "c_bbbbbb".parse().unwrap(),
            column: table().column(name),
        })
    }

    /// A computed column is a reader-facing name (#1433): replacing one with
    /// another, of either kind, moves readers. Under one name, as an
    /// expression rebuild or a conversion to or from a stored column, it
    /// does not.
    #[test]
    fn a_computed_column_takes_either_side_of_expand_contract_unless_its_name_stays() {
        let fires = |changes: Vec<PlannedChange>| {
            let f = plan(&cs(changes), &Policies::default(), &ctx());
            assert!(
                f.iter().all(|(_, f)| f.id == "change.expand-contract"),
                "{f:?}"
            );
            f.iter().map(|(at, _)| *at).collect::<Vec<_>>()
        };
        // A new name in, an old name out: the finding sits on the drop.
        assert_eq!(
            fires(vec![add_computed("net"), drop_computed("gross")]),
            [1]
        );
        assert_eq!(fires(vec![add_computed("net"), drop_column("gross")]), [1]);
        assert_eq!(fires(vec![add_column("net"), drop_computed("gross")]), [1]);

        // The name stays: a rebuild, and both conversions.
        assert!(fires(vec![drop_computed("total"), add_computed("total")]).is_empty());
        assert!(fires(vec![drop_computed("total"), add_column("total")]).is_empty());
        assert!(fires(vec![drop_column("total"), add_computed("total")]).is_empty());

        // Neither half alone is the pattern.
        assert!(fires(vec![add_computed("net")]).is_empty());
        assert!(fires(vec![drop_computed("gross")]).is_empty());
    }

    /// A drop moved ahead of its table's rename is addressed by the source
    /// name, and the add after it by the destination (`object_order`). One
    /// table under two names is still one table: a computed rebuild across
    /// the rename is not an add beside another drop, and a computed swap
    /// across it is the pattern (DEC-1433.1 review).
    #[test]
    fn a_table_is_one_table_across_its_rename() {
        let old: TableName = "dbo.customer_old".parse().unwrap();
        let rename = |from: &TableName, to: &TableName| {
            PlannedChange::new(Change::RenameTable {
                uid: "t_cccccc".parse().unwrap(),
                from: from.clone(),
                to: to.clone(),
                defaults: Vec::new(),
            })
        };
        let at = |mut p: PlannedChange, t: &TableName| {
            if let Change::DropComputedColumn { table, .. } = &mut p.change {
                *table = t.clone();
            } else if let Change::DropColumn { column, .. } = &mut p.change {
                column.table = t.clone();
            } else {
                panic!("not a drop: {:?}", p.change);
            }
            p
        };
        let fires = |changes: Vec<PlannedChange>| {
            plan(&cs(changes), &Policies::default(), &ctx())
                .iter()
                .map(|(at, _)| *at)
                .collect::<Vec<_>>()
        };

        // A rebuild across the rename, beside a stored drop and no add.
        assert!(
            fires(vec![
                at(drop_computed("total"), &old),
                rename(&old, &table()),
                add_computed("total"),
                drop_column("legacy"),
            ])
            .is_empty()
        );
        // A computed name swapped across the rename: the drop under the
        // source name is the contraction.
        assert_eq!(
            fires(vec![
                at(drop_computed("gross"), &old),
                rename(&old, &table()),
                add_computed("net"),
            ]),
            [0]
        );
        // A table that takes the source name after the rename is another
        // table: its drop does not pair with the renamed table's add.
        assert!(
            fires(vec![
                rename(&old, &table()),
                add_column("net"),
                at(drop_column("gross"), &old),
            ])
            .is_empty()
        );
    }

    /// A column that stops accepting NULL accepts less than it did, which is
    /// the "narrow" half of the rule's own words. Both spellings of it were
    /// missing: the change of its own, and the type change that folds one in
    /// and so carries `NotNull` rather than `Narrowing` (DECISIONS 173).
    #[test]
    fn tightening_to_not_null_is_the_contraction_half_too() {
        let add = PlannedChange::new(Change::AddColumn {
            uid: "c_aaaaaa".parse().unwrap(),
            table: table(),
            name: "full_name".into(),
            column: Box::new(Column::new("int".parse().unwrap())),
        });
        let tighten = PlannedChange::new(Change::AlterColumnNullability {
            uid: "c_bbbbbb".parse().unwrap(),
            column: table().column("name"),
            ty: "int".parse().unwrap(),
            to_nullable: false,
            through_external_partitions: false,
            collation: None,
        });
        // A widening type change that also tightens: `Narrowing` is not among
        // its risks, and it is still a contraction.
        let widen_and_tighten = PlannedChange::new(Change::AlterColumnType {
            uid: "c_cccccc".parse().unwrap(),
            column: table().column("n"),
            from: "int".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_nullable: true,
            to_nullable: false,
            through_external_partitions: false,
            from_collation: None,
            to_collation: None,
            using: None,
        });
        assert!(
            !widen_and_tighten.risks.contains(&RiskClass::Narrowing),
            "the premise: {:?}",
            widen_and_tighten.risks
        );

        for contraction in [tighten.clone(), widen_and_tighten] {
            let f = plan(
                &cs(vec![add.clone(), contraction]),
                &Policies::default(),
                &ctx(),
            );
            assert_eq!(f.len(), 1, "{f:?}");
            assert_eq!(f[0].1.id, "change.expand-contract", "{f:?}");
        }

        // Loosening is not: a column that starts accepting NULL accepts more.
        let loosen = PlannedChange::new(Change::AlterColumnNullability {
            uid: "c_bbbbbb".parse().unwrap(),
            column: table().column("name"),
            ty: "int".parse().unwrap(),
            to_nullable: true,
            through_external_partitions: false,
            collation: None,
        });
        assert!(plan(&cs(vec![add.clone(), loosen]), &Policies::default(), &ctx()).is_empty());

        // And neither half alone is the pattern. A NOT NULL column *addition*
        // carries `RiskClass::NotNull` as well (SPEC §7.1) — asking the risk
        // rather than the change would make that one change both sides and
        // fire on it alone.
        let add_not_null = PlannedChange::new(Change::AddColumn {
            uid: "c_dddddd".parse().unwrap(),
            table: table(),
            name: "full_name".into(),
            column: Box::new({
                let mut c = Column::new("int".parse().unwrap());
                c.nullable = false;
                c
            }),
        });
        assert!(
            add_not_null.risks.contains(&RiskClass::NotNull),
            "the premise: {:?}",
            add_not_null.risks
        );
        assert!(plan(&cs(vec![add_not_null]), &Policies::default(), &ctx()).is_empty());
        assert!(plan(&cs(vec![tighten]), &Policies::default(), &ctx()).is_empty());
    }

    #[test]
    fn a_narrowing_and_a_grant_are_noted_at_the_severity_the_project_chooses() {
        let narrow = PlannedChange::new(Change::AlterColumnType {
            uid: "c_aaaaaa".parse().unwrap(),
            column: table().column("n"),
            from: "bigint".parse().unwrap(),
            to: "int".parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
            through_external_partitions: false,
            from_collation: None,
            to_collation: None,
            using: None,
        })
        .with_risk(RiskClass::Narrowing);
        let grant = PlannedChange::new(Change::Grant {
            role: "r".into(),
            target: "dbo.customer".parse().unwrap(),
            permissions: [pbps_model::Permission::Select].into_iter().collect(),
        });
        let f = plan(&cs(vec![narrow, grant]), &Policies::default(), &ctx());
        let got: Vec<(usize, &str, Severity)> = f
            .iter()
            .map(|(i, f)| (*i, f.id.as_str(), f.severity))
            .collect();
        assert_eq!(
            got,
            [
                (0, "change.narrowing-on-data", Severity::Note),
                (1, "grant.widen", Severity::Note)
            ]
        );
        let p = policies(r#"{"rules": {"grant.widen": "error"}}"#);
        let f = plan(&cs(vec![]), &p, &ctx());
        assert!(f.is_empty(), "nothing to grant, nothing to say");
    }

    #[test]
    fn the_change_window_is_asked_only_of_a_connected_plan_and_only_when_outside() {
        let p = policies(
            r#"{"rules": {"change.window": {"severity": "error", "offset": "+08:00",
                                            "allow": ["mon-fri 09:00-17:00"]}}}"#,
        );
        let change = PlannedChange::new(Change::DropColumn {
            uid: "c_bbbbbb".parse().unwrap(),
            column: table().column("name"),
        });
        // 10:00 UTC Thursday is 18:00 in +08:00: outside.
        let mut c = ctx();
        assert!(
            plan(&cs(vec![change.clone()]), &p, &c).is_empty(),
            "offline: never"
        );
        c.connected = true;
        let f = plan(&cs(vec![change.clone()]), &p, &c);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].1.id, "change.window");
        assert!(f[0].1.message.contains("18:00"), "{f:?}");
        // 02:00 UTC Thursday is 10:00 in +08:00: inside.
        c.now -= 8 * 3600;
        assert!(plan(&cs(vec![change.clone()]), &p, &c).is_empty());
        // Saturday 10:00 +08:00: outside, and a second window lets it in.
        c.now += 2 * 86_400;
        assert_eq!(plan(&cs(vec![change.clone()]), &p, &c).len(), 1);
        let p = policies(
            r#"{"rules": {"change.window": {"severity": "error", "offset": "+08:00",
                                            "allow": ["mon-fri 09:00-17:00", "sat 00:00-24:00"]}}}"#,
        );
        assert!(plan(&cs(vec![change]), &p, &c).is_empty());
        // An empty plan is inside every window: there is nothing to refuse.
        c.now += 86_400;
        assert!(plan(&cs(vec![]), &p, &c).is_empty());
    }

    #[test]
    fn windows_parse_across_the_week_boundary_and_refuse_a_backwards_range() {
        let w = parse_window("fri-mon 22:00-24:00").unwrap();
        assert_eq!(w.days, [4, 5, 6, 0].into_iter().collect());
        assert_eq!((w.from, w.to), (1320, 1440));
        let w = parse_window("sat,sun 00:00-24:00").unwrap();
        assert_eq!(w.days, [5, 6].into_iter().collect());
        assert!(parse_window("mon 17:00-09:00").is_err());
        assert!(parse_window("weekdays").is_err());
        assert!(parse_window("mon-fri 9-17").is_err());
    }

    #[test]
    fn a_recorded_conversion_is_not_described_as_refused() {
        for (using, refused) in [(None, true), (Some("n::integer[]".to_owned()), false)] {
            let mut change = PlannedChange::new(Change::AlterColumnType {
                uid: "c_aaaaaa".parse().unwrap(),
                column: table().column("n"),
                from: ColumnType::simple("bigint").into_array(),
                to: ColumnType::simple("integer").into_array(),
                from_nullable: true,
                to_nullable: true,
                from_collation: None,
                to_collation: None,
                through_external_partitions: false,
                using,
            });
            change.risks.insert(RiskClass::Narrowing);
            let findings = plan(&cs(vec![change]), &Policies::default(), &ctx());
            let message = &findings
                .iter()
                .find(|(_, f)| f.id == "change.narrowing-on-data")
                .unwrap()
                .1
                .message;
            assert_eq!(message.contains("is refused"), refused, "{message}");
            assert_eq!(
                message.contains("recorded expression"),
                !refused,
                "{message}"
            );
        }
    }

    #[test]
    fn policy_type_messages_show_names_without_changing_saved_plan_data() {
        let raw = "type\n\\\u{1b}\"名前";
        let canonical = "app.\"type\n\\\u{1b}\"\"名前\"";
        for array in [false, true] {
            let from = ColumnType::simple("integer");
            let to = ColumnType::reference(pbps_model::TypeRef::new("app", raw).unwrap());
            let (from, to, suffix) = if array {
                (from.into_array(), to.into_array(), "[]")
            } else {
                (from, to, "")
            };
            let mut change = PlannedChange::new(Change::AlterColumnType {
                uid: "c_aaaaaa".parse().unwrap(),
                column: table().column("n"),
                from,
                to,
                from_nullable: true,
                to_nullable: true,
                from_collation: None,
                to_collation: None,
                through_external_partitions: false,
                using: None,
            });
            change.risks.insert(RiskClass::Narrowing);
            let findings = plan(&cs(vec![change.clone()]), &Policies::default(), &ctx());
            let finding = &findings
                .iter()
                .find(|(_, f)| f.id == "change.narrowing-on-data")
                .unwrap()
                .1;
            let reason = if array {
                "narrows or converts an array's elements, which pbps does not do: the plan is refused when it is computed against a database"
            } else {
                "depends on what is already stored; the pre-flight probe counts the rows that would not fit before the first statement runs"
            };
            assert_eq!(
                finding.message,
                format!(
                    "dbo.customer.n: integer{suffix} -> {canonical}{suffix} {reason}",
                    canonical = pbps_model::escape::Escaped(canonical)
                )
            );
            assert!(!finding.message.chars().any(char::is_control));
            change.risks.remove(&RiskClass::Narrowing);
            assert!(
                !plan(&cs(vec![change]), &Policies::default(), &ctx())
                    .iter()
                    .any(|(_, f)| f.id == "change.narrowing-on-data")
            );
        }
    }

    #[test]
    fn naming_regexes_match_the_original_program_and_reject_literal_lookalikes() {
        for (pattern, matching, other) in [
            (r"item\d+", "item12", r"item\d"),
            (r"item\\tail", r"item\tail", "itemtail"),
        ] {
            let policies: Policies = serde_json::from_value(serde_json::json!({
                "rules": {"naming.table": {"severity": "error", "pattern": pattern}}
            }))
            .unwrap();
            assert!(policies.check().is_empty());
            let evaluate = |name: &str| {
                let mut schema = Schema::default();
                schema.tables.insert(
                    pbps_model::TableName::new("app", name),
                    pbps_model::Table::default(),
                );
                declarations(&schema, &policies, &ctx())
                    .into_iter()
                    .filter(|finding| finding.id == "naming.table")
                    .collect::<Vec<_>>()
            };
            assert!(evaluate(matching).is_empty(), "{pattern}: {matching}");
            let rejected = evaluate(other);
            assert_eq!(rejected.len(), 1, "{pattern}: {other}");
            assert_eq!(
                rejected[0].message,
                format!("app.{other}: table `{other}` does not match `{pattern}`")
            );
        }
    }

    #[test]
    fn declaration_comparison_keys_retain_the_original_name_bytes() {
        let policies: Policies = serde_json::from_value(serde_json::json!({
            "rules": {
                "naming.table": {"severity": "error", "pattern": "ordinary"},
                "data.max-rows": {"rows": 0}
            }
        }))
        .unwrap();
        for name in [
            "Bad\n\u{1b}[31m\u{9b}\\\u{202e}名前",
            r"Bad\n",
            "名前_ünï❤️",
        ] {
            let mut table = pbps_model::Table::default();
            table.columns.insert(
                name.into(),
                pbps_model::Column::new(pbps_model::ColumnType::simple("image")),
            );
            table.data = Some(pbps_model::TableData {
                mode: pbps_model::DataMode::Ensure,
                rows: [(pbps_model::RowKey::from("1"), Default::default())].into(),
            });
            let mut schema = Schema::default();
            schema
                .tables
                .insert(pbps_model::TableName::new("app", name), table);
            let mut context = ctx();
            context.deprecated_types = &[("image", "advice")];
            let findings = declarations(&schema, &policies, &context);
            assert_eq!(findings.len(), 3);
            for (rule, expected) in [
                (
                    "naming.table",
                    format!("app.{name}: table `{name}` does not match `ordinary`"),
                ),
                (
                    "data.max-rows",
                    format!(
                        "app.{name}: 1 declared rows is above 0 — this does not look like reference data, and every plan compares it row by row"
                    ),
                ),
                (
                    "column.no-deprecated-type",
                    format!("app.{name}.{name}: `image` advice"),
                ),
            ] {
                let finding = findings.iter().find(|finding| finding.id == rule).unwrap();
                assert_eq!(
                    finding.subject.as_deref(),
                    Some(format!("app.{name}").as_str())
                );
                assert_eq!(finding.message, expected);
            }
        }
    }
}
