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
struct Live<'a> {
    id: &'static str,
    severity: Severity,
    config: crate::config::RuleConfig,
    policies: &'a Policies,
    today: String,
}

impl Live<'_> {
    fn of<'a>(policies: &'a Policies, id: &'static str, ctx: &Context) -> Option<Live<'a>> {
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

    fn finding(&self, subject: Option<&str>, message: impl Into<String>) -> Option<Finding> {
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
            let subject = name.to_string();
            if !ctx.wants(&subject) {
                continue;
            }
            let names: Vec<(&str, String)> = match what {
                Naming::Table => vec![("table", name.name.clone())],
                Naming::Column => table
                    .columns
                    .keys()
                    .map(|c| ("column", c.clone()))
                    .collect(),
                Naming::Index => table.indexes.keys().map(|i| ("index", i.clone())).collect(),
                Naming::Constraint => table
                    .primary_key
                    .iter()
                    .filter_map(|pk| pk.name.clone())
                    .map(|n| ("primary key", n))
                    .chain(
                        table
                            .unique
                            .keys()
                            .map(|n| ("unique constraint", n.clone())),
                    )
                    .chain(
                        table
                            .foreign_keys
                            .keys()
                            .map(|n| ("foreign key", n.clone())),
                    )
                    .chain(table.checks.keys().map(|n| ("check constraint", n.clone())))
                    .collect(),
            };
            for (kind, n) in names {
                if !re.is_match(&n) {
                    out.extend(live.finding(
                        Some(&subject),
                        format!("{name}: {kind} `{n}` does not match `{pattern}`"),
                    ));
                }
            }
        }
    }

    if let Some(live) = Live::of(policies, rules::DATA_MAX_ROWS, ctx) {
        let max = live.config.rows.unwrap_or(ctx.max_rows);
        for (name, table) in &schema.tables {
            let subject = name.to_string();
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
                        data.rows.len()
                    ),
                ));
            }
        }
    }

    if let Some(live) = Live::of(policies, rules::COLUMN_NO_DEPRECATED_TYPE, ctx) {
        for (name, table) in &schema.tables {
            let subject = name.to_string();
            if !ctx.wants(&subject) {
                continue;
            }
            for (column, spec) in &table.columns {
                if matches!(spec.ty.base.as_str(), "text" | "ntext" | "image") {
                    out.extend(live.finding(
                        Some(&subject),
                        format!(
                            "{name}.{column}: `{}` has been deprecated since SQL Server 2005; \
                             use varchar(max), nvarchar(max) or varbinary(max)",
                            spec.ty
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

/// The plan rules over a change set (the `plan` point). Each finding names
/// the index of the change it attaches to.
pub fn plan(cs: &ChangeSet, policies: &Policies, ctx: &Context) -> Vec<(usize, Finding)> {
    let mut out = Vec::new();

    if let Some(live) = Live::of(policies, rules::CHANGE_EXPAND_CONTRACT, ctx) {
        // Per table: the first add, and the first drop or narrowing.
        let mut adds: BTreeMap<&TableName, usize> = BTreeMap::new();
        let mut contracts: BTreeMap<&TableName, usize> = BTreeMap::new();
        for (i, p) in cs.changes.iter().enumerate() {
            // Named arms, not a wildcard: every other kind of change is
            // neither side of the pattern, and a new kind that is (a column
            // drop spelled differently, say) should have to be placed here.
            let contract = match &p.change {
                Change::AddColumn { table, .. } => {
                    adds.entry(table).or_insert(i);
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
                Change::AlterColumnType { .. }
                | Change::CreateTable { .. }
                | Change::DropTable { .. }
                | Change::RenameTable { .. }
                | Change::RenameColumn { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
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
                | Change::CreateModule { .. }
                | Change::AlterModule { .. }
                | Change::DropModule { .. }
                | Change::CreateRole { .. }
                | Change::DropRole { .. }
                | Change::RenameRole { .. }
                | Change::Grant { .. }
                | Change::Revoke { .. } => None,
            };
            if let Some(table) = contract {
                contracts.entry(table).or_insert(i);
            }
        }
        for (table, at) in contracts {
            if adds.contains_key(table) {
                let subject = table.to_string();
                out.extend(
                    live.finding(
                        Some(&subject),
                        format!(
                            "{table}: this revision both adds and drops or narrows a column; \
                             that usually wants expand/contract staging (SPEC §13.3) — add \
                             first, backfill, then contract in a later revision"
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
                column, from, to, ..
            } = &p.change
                && p.risks.contains(&RiskClass::Narrowing)
            {
                let subject = column.table.to_string();
                out.extend(
                    live.finding(
                        Some(&subject),
                        format!(
                            "{column}: {from} -> {to} depends on what is already stored; the \
                             pre-flight probe counts the rows that would not fit before the \
                             first statement runs"
                        ),
                    )
                    .map(|f| (i, f)),
                );
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
                let subject = format!("role {role}");
                out.extend(
                    live.finding(
                        Some(&subject),
                        format!(
                            "role {role} gains {} on {target}",
                            permissions
                                .iter()
                                .map(|p| p.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
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
        }
    }

    fn schema() -> Schema {
        let mut s = Schema::default();
        let mut t = Table::default();
        t.columns.insert(
            "CustomerId".to_owned(),
            Column::new("int".parse::<ColumnType>().unwrap()),
        );
        t.columns.insert(
            "notes".to_owned(),
            Column::new("text".parse::<ColumnType>().unwrap()),
        );
        t.indexes.insert(
            "IX_customer".to_owned(),
            pbps_model::Index {
                columns: vec![],
                include: vec![],
                unique: false,
                filter: None,
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
}
