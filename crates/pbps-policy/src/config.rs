//! The `policies:` block of `pbps.yml` (ADR-0008 decisions 4, 5 and 7).
//!
//! ```yaml
//! policies:
//!   rules:
//!     naming.table:   {severity: error, pattern: "^[a-z][a-z0-9_]*$"}
//!     data.max-rows:  {severity: warning, rows: 500}
//!     change.expand-contract: off
//!     change.window:  {severity: error, offset: "+08:00", allow: ["mon-fri 09:00-17:00"]}
//!   suppress:
//!     - rule: naming.column
//!       on: dbo.legacy_import
//!       reason: inherited from the system this replaces
//!       until: 2026-12-31
//! ```
//!
//! A rule's entry is a bare word (`error`, `warning`, `note`, `off`) or a map
//! with `severity` and the rule's parameters. Nothing here is code: a pattern
//! is a regular expression, the rest are numbers, offsets and windows.

use std::collections::BTreeMap;

use pbps_model::Severity;

use crate::rules;

/// The whole block. Absent means every rule at its default and nothing
/// suppressed.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Policies {
    /// Rule id to its setting.
    ///
    /// A map in Rust, a closed list of keys in the editor schema: see
    /// [`rules_schema`]. `BTreeMap<String, _>` derives to "any key", which
    /// blessed `naming.tabel` in the editor and left the typo to be found by
    /// `pbps validate` — the opposite of what a generated schema is for
    /// (DECISIONS 172).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(schema_with = "rules_schema")]
    pub rules: BTreeMap<String, RuleSetting>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suppress: Vec<Suppression>,
}

/// The catalogue as JSON Schema `enum` members.
fn rule_ids() -> Vec<serde_json::Value> {
    rules::RULES.iter().map(|r| r.id.into()).collect()
}

/// `rules:`, keyed by the catalogue rather than by any string at all.
///
/// Written out rather than derived because the derive has no way to know the
/// keys are closed, and `pbps validate` refusing a typo is not a substitute:
/// an editor that autocompletes and validates against this file is the whole
/// point of publishing it (SPEC §14.1), and one that accepts a misspelt rule
/// id is worse than none — it says the file is right (DECISIONS 172).
///
/// Each key carries the catalogue's own sentence about the rule, so an editor
/// shows what it checks and what it takes without a second description to
/// keep in step.
fn rules_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    // Every rule's own shape, not one shared `RuleSetting`. Shared, the schema
    // accepted `naming.table: {rows: 5}` — a parameter that rule does not
    // take, which `Policies::check` refuses — so the editor blessed a file
    // the loader rejects, which is the whole thing this schema exists not to
    // do (DECISIONS 188).
    let mut properties = serde_json::Map::new();
    for rule in &rules::RULES {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "severity".to_owned(),
            serde_json::json!({
                "type": "string",
                "description": "error, warning, note or off.",
            }),
        );
        for param in rule.params {
            fields.insert((*param).to_owned(), param_schema(param));
        }
        let about = match rule.params {
            [] => format!("Checks {}.", rule.about),
            params => format!("Checks {}. Takes {}.", rule.about, params.join(", ")),
        };
        let entry = serde_json::json!({
            "description": about,
            "anyOf": [
                // `off` as a YAML boolean. Only `false`: `true` says nothing
                // about the severity and `check` refuses it.
                {"const": false},
                // A bare severity word. Left as a string rather than an
                // `enum`, because `Severity::from_str` trims and lowercases,
                // and a schema stricter than the loader is the mirror image
                // of the bug this is fixing (DECISIONS 172).
                {"type": "string"},
                {
                    "type": "object",
                    "properties": fields,
                    "additionalProperties": false,
                },
            ],
        });
        properties.insert(rule.id.to_owned(), entry);
    }
    schemars::Schema::try_from(serde_json::json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    }))
    .expect("an object is a schema")
}

/// One parameter, as the rule that takes it means it.
///
/// The names come from the catalogue and the shapes from [`RuleConfig`],
/// which is still the one place a parameter's type is written down — the
/// per-rule schemas say *which* parameters, never what they are
/// (DECISIONS 188).
fn param_schema(param: &str) -> serde_json::Value {
    match param {
        "pattern" => serde_json::json!({
            "type": "string",
            "description": "A regular expression the name must match in full.",
        }),
        "rows" => serde_json::json!({
            "type": "integer",
            "minimum": 0,
            "description": "The row count above which a block is reported.",
        }),
        "offset" => serde_json::json!({
            "type": "string",
            "description": "The UTC offset the windows are written in (`+08:00`).",
        }),
        "allow" => serde_json::json!({
            "type": "array",
            "items": {"type": "string"},
            "description": "Windows such as `mon-fri 09:00-17:00`.",
        }),
        // A parameter added to the catalogue without a shape here would
        // otherwise silently accept anything; an empty schema accepts
        // anything too, but the test below refuses to let it happen.
        _ => serde_json::json!({}),
    }
}

/// One rule id: the same closed list, in the place a suppression names one.
///
/// The same shape as [`rules_schema`] and swept with it — a suppression of
/// `naming.tabel` suppresses nothing, and `check` refuses it for exactly that
/// reason (DECISIONS 172).
fn rule_id_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::Schema::try_from(serde_json::json!({
        "type": "string",
        "enum": rule_ids(),
    }))
    .expect("an object is a schema")
}

/// `error` / `warning` / `note` / `off`, or the detailed form.
///
/// `off` is a YAML boolean (so are `on`, `yes` and `no`), and a project that
/// writes `naming.table: off` means the rule is off, not a type error. The
/// boolean is accepted as a switch: `false` is off; `true` is refused by
/// [`Policies::check`], because "on" says nothing about the severity.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(untagged)]
pub enum RuleSetting {
    Switch(bool),
    Word(String),
    Detailed(RuleConfig),
}

/// The detailed form of one rule's setting.
///
/// The parameters of every rule are fields here, and a rule refuses the ones
/// that are not its own (see [`Policies::check`]): one flat shape keeps the
/// editor schema honest without a variant per rule.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    /// `error`, `warning`, `note` or `off`. Absent keeps the rule's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,

    /// `naming.*`: the regular expression the name must match in full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,

    /// `data.max-rows`: the row count above which a block is reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<usize>,

    /// `change.window`: the UTC offset the windows are written in (`+08:00`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<String>,

    /// `change.window`: `mon-fri 09:00-17:00`, `sat 00:00-24:00`, ...
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
}

/// One suppression: a rule that does not fire, where, why, and until when.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Suppression {
    #[schemars(schema_with = "rule_id_schema")]
    pub rule: String,

    /// The object it applies to (`dbo.customer`, `role app_reader`). Absent
    /// means the whole project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,

    /// Why. The field an audit reads; blank is refused.
    pub reason: String,

    /// `YYYY-MM-DD`. From that day on the suppression stops applying and the
    /// finding comes back at its configured severity (ADR-0008 decision 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

/// What one rule is set to, once the block has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    /// `None` is off.
    pub severity: Option<Severity>,
    pub config: RuleConfig,
}

impl Policies {
    /// Every problem with the block, all of them, in a stable order.
    ///
    /// Checked by `validate` and before any evaluation, because a misspelled
    /// rule id that configured nothing would be the silent failure this whole
    /// mechanism exists to replace.
    pub fn check(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for (id, setting) in &self.rules {
            let Some(rule) = rules::rule(id) else {
                problems.push(format!(
                    "policies: `{id}` is not a rule this pbps knows; the catalogue is {}",
                    rules::RULES
                        .iter()
                        .map(|r| r.id)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                continue;
            };
            let config = match setting {
                RuleSetting::Switch(false) => continue,
                RuleSetting::Switch(true) => {
                    problems.push(format!(
                        "policies: `{id}` is switched on without a severity; use error, \
                         warning or note (`on` and `yes` are YAML booleans)"
                    ));
                    continue;
                }
                RuleSetting::Word(w) => {
                    if !is_severity_word(w) {
                        problems.push(format!(
                            "policies: `{id}` is set to `{w}`; use error, warning, note or off"
                        ));
                    } else if !w.eq_ignore_ascii_case("off") && !rule.required.is_empty() {
                        // Switched on by a bare word, and the rule cannot run
                        // on a word alone: evaluated, it would check nothing
                        // (a naming rule) or refuse everything (a window).
                        problems.push(format!(
                            "policies: `{id}` needs `{}`; write it as a map, e.g. \
                             `{id}: {{severity: {w}, {}: ...}}`",
                            rule.required.join("`, `"),
                            rule.required[0]
                        ));
                    }
                    continue;
                }
                RuleSetting::Detailed(c) => c,
            };
            // Switched on — by its own severity, or by the catalogue's default
            // when it names none — the rule has to have what it runs on.
            let on = match &config.severity {
                Some(w) => !w.eq_ignore_ascii_case("off"),
                None => rule.default.is_some(),
            };
            if on {
                let has = |param: &str| match param {
                    "pattern" => config.pattern.is_some(),
                    "rows" => config.rows.is_some(),
                    "offset" => config.offset.is_some(),
                    "allow" => !config.allow.is_empty(),
                    _ => false,
                };
                for req in rule.required {
                    if !has(req) {
                        problems.push(format!(
                            "policies: `{id}` is switched on with no `{req}`, which it cannot \
                             run without"
                        ));
                    }
                }
            }
            if let Some(w) = &config.severity
                && !is_severity_word(w)
            {
                problems.push(format!(
                    "policies: `{id}` has severity `{w}`; use error, warning, note or off"
                ));
            }
            let given: Vec<&str> = [
                ("pattern", config.pattern.is_some()),
                ("rows", config.rows.is_some()),
                ("offset", config.offset.is_some()),
                ("allow", !config.allow.is_empty()),
            ]
            .into_iter()
            .filter(|(_, present)| *present)
            .map(|(name, _)| name)
            .collect();
            for param in &given {
                if !rule.params.contains(param) {
                    problems.push(format!(
                        "policies: `{id}` does not take `{param}`{}",
                        if rule.params.is_empty() {
                            String::new()
                        } else {
                            format!("; it takes {}", rule.params.join(", "))
                        }
                    ));
                }
            }
            if let Some(p) = &config.pattern
                && let Err(e) = regex_lite::Regex::new(p)
            {
                problems.push(format!("policies: `{id}` pattern `{p}` is not valid: {e}"));
            }
            if let Some(o) = &config.offset
                && let Err(e) = crate::civil::parse_offset(o)
            {
                problems.push(format!("policies: `{id}`: {e}"));
            }
            for w in &config.allow {
                if let Err(e) = crate::evaluate::parse_window(w) {
                    problems.push(format!("policies: `{id}` window `{w}`: {e}"));
                }
            }
        }
        for s in &self.suppress {
            if rules::rule(&s.rule).is_none() {
                problems.push(format!(
                    "policies: suppression of `{}` names no rule this pbps knows",
                    s.rule
                ));
            }
            if s.reason.trim().is_empty() {
                problems.push(format!(
                    "policies: the suppression of `{}`{} has no reason, and an audit asks for one",
                    s.rule,
                    s.on.as_deref()
                        .map(|o| format!(" on `{o}`"))
                        .unwrap_or_default()
                ));
            }
            if let Some(u) = &s.until
                && let Err(e) = crate::civil::parse_date(u)
            {
                problems.push(format!("policies: suppression of `{}`: {e}", s.rule));
            }
        }
        problems
    }

    /// The effective setting of one rule: the project's, else the default.
    pub fn effective(&self, id: &str) -> Effective {
        let rule = rules::rule(id);
        let default = rule.and_then(|r| r.default);
        match self.rules.get(id) {
            None => Effective {
                severity: default,
                config: RuleConfig::default(),
            },
            // `true` never reaches here: `check` refuses it before any
            // evaluation, and a refused block is not evaluated.
            Some(RuleSetting::Switch(on)) => Effective {
                severity: if *on { default } else { None },
                config: RuleConfig::default(),
            },
            Some(RuleSetting::Word(w)) => Effective {
                severity: severity_word(w).unwrap_or(default),
                config: RuleConfig::default(),
            },
            Some(RuleSetting::Detailed(c)) => Effective {
                severity: match &c.severity {
                    Some(w) => severity_word(w).unwrap_or(default),
                    None => default,
                },
                config: c.clone(),
            },
        }
    }

    /// Whether a finding is suppressed on `today` (`YYYY-MM-DD`).
    ///
    /// A suppression matches by rule id, and by subject when it names one; an
    /// expired one is simply not there any more.
    pub fn suppressed(&self, id: &str, subject: Option<&str>, today: &str) -> bool {
        self.suppress.iter().any(|s| {
            s.rule == id
                && s.on.as_deref().is_none_or(|on| Some(on) == subject)
                // Compared as it was validated: `parse_date` trims, and a
                // leading space would sort a future date before today.
                && s.until.as_deref().is_none_or(|until| today < until.trim())
        })
    }
}

fn is_severity_word(w: &str) -> bool {
    w.eq_ignore_ascii_case("off") || w.parse::<Severity>().is_ok()
}

/// `Some(Some(s))` for a severity, `Some(None)` for `off`, `None` for a word
/// that is neither — which `check` has already reported.
fn severity_word(w: &str) -> Option<Option<Severity>> {
    if w.eq_ignore_ascii_case("off") {
        return Some(None);
    }
    w.parse::<Severity>().ok().map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(yaml: &str) -> Policies {
        serde_json::from_value(serde_yaml_like(yaml)).unwrap()
    }

    /// The tests write JSON, which is YAML enough for this shape and keeps
    /// the YAML crate out of this crate (ADR-0001).
    fn serde_yaml_like(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_rule_reads_as_a_word_or_a_map_and_the_default_fills_the_rest() {
        let p = block(
            r#"{"rules": {"naming.table": {"severity": "error", "pattern": "^[a-z_]+$"},
                          "change.expand-contract": "off",
                          "grant.widen": "warning"}}"#,
        );
        assert_eq!(p.check(), Vec::<String>::new());
        assert_eq!(p.effective("naming.table").severity, Some(Severity::Error));
        assert_eq!(
            p.effective("naming.table").config.pattern.as_deref(),
            Some("^[a-z_]+$")
        );
        assert_eq!(p.effective("change.expand-contract").severity, None);
        assert_eq!(p.effective("grant.widen").severity, Some(Severity::Warning));
        // Untouched: the catalogue's default.
        assert_eq!(
            p.effective("data.max-rows").severity,
            Some(Severity::Warning)
        );
        assert_eq!(p.effective("naming.column").severity, None);
    }

    /// A YAML loader reads `off` as `false`; the block must mean the same by
    /// it. `on` says nothing about a severity, so it is refused rather than
    /// guessed.
    #[test]
    fn the_yaml_boolean_off_switches_a_rule_off_and_on_is_refused() {
        let p = block(r#"{"rules": {"data.max-rows": false}}"#);
        assert_eq!(p.check(), Vec::<String>::new());
        assert_eq!(p.effective("data.max-rows").severity, None);

        let p = block(r#"{"rules": {"data.max-rows": true}}"#);
        let problems = p.check();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("without a severity"), "{problems:?}");
    }

    #[test]
    fn a_misspelled_rule_a_foreign_parameter_and_a_bad_pattern_are_refused_by_name() {
        let p = block(
            r#"{"rules": {"naming.tabel": "error",
                          "data.max-rows": {"pattern": "x"},
                          "naming.column": {"pattern": "("},
                          "grant.widen": "loud"}}"#,
        );
        // In rule-id order, so the report is the same on every run.
        let problems = p.check();
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems[0].contains("data.max-rows") && problems[0].contains("`pattern`"));
        assert!(problems[1].contains("loud"), "{problems:?}");
        assert!(problems[2].contains("not valid"), "{problems:?}");
        assert!(problems[3].contains("naming.tabel"), "{problems:?}");
    }

    #[test]
    fn a_suppression_needs_a_known_rule_a_reason_and_a_real_date() {
        let p = block(
            r#"{"suppress": [
                {"rule": "naming.column", "on": "dbo.legacy", "reason": "inherited"},
                {"rule": "naming.column", "reason": "  "},
                {"rule": "naming.what", "reason": "x"},
                {"rule": "naming.table", "reason": "x", "until": "soon"}]}"#,
        );
        let problems = p.check();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("no reason"), "{problems:?}");
        assert!(problems[1].contains("naming.what"), "{problems:?}");
        assert!(problems[2].contains("soon"), "{problems:?}");
    }

    /// The scope and the clock: an object-scoped suppression covers that
    /// object only, a project-wide one everything, and an expired one nothing.
    #[test]
    fn a_suppression_applies_where_it_says_and_until_when_it_says() {
        let p = block(
            r#"{"suppress": [
                {"rule": "naming.column", "on": "dbo.legacy", "reason": "x", "until": "2026-12-31"},
                {"rule": "grant.widen", "reason": "reviewed in the MR"}]}"#,
        );
        assert!(p.suppressed("naming.column", Some("dbo.legacy"), "2026-09-03"));
        assert!(!p.suppressed("naming.column", Some("dbo.other"), "2026-09-03"));
        assert!(!p.suppressed("naming.column", None, "2026-09-03"));
        assert!(
            !p.suppressed("naming.column", Some("dbo.legacy"), "2026-12-31"),
            "from the day itself the suppression is gone"
        );
        assert!(p.suppressed("grant.widen", Some("role r"), "2030-01-01"));
        assert!(p.suppressed("grant.widen", None, "2030-01-01"));
        // Whitespace around the date passes validation, which trims; the
        // comparison has to see the same text, or a leading space makes a
        // future suppression expire today and a trailing one outlive its day.
        let padded = block(
            r#"{"suppress": [
                {"rule": "naming.column", "reason": "x", "until": " 2026-12-31 "}]}"#,
        );
        assert!(padded.check().is_empty());
        assert!(padded.suppressed("naming.column", None, "2026-09-03"));
        assert!(!padded.suppressed("naming.column", None, "2026-12-31"));
    }

    /// A rule switched on without what it runs on is refused, in either
    /// spelling: `naming.table: error` checked no name at all, and
    /// `change.window: error` refused every plan.
    #[test]
    fn a_rule_switched_on_without_its_required_parameter_is_refused() {
        let p = block(r#"{"rules": {"naming.table": "error", "change.window": "warning"}}"#);
        let problems = p.check();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("change.window") && problems[0].contains("`allow`"));
        assert!(problems[1].contains("naming.table") && problems[1].contains("`pattern`"));
        let p = block(r#"{"rules": {"naming.table": {"severity": "error"}}}"#);
        let problems = p.check();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("no `pattern`"), "{problems:?}");
        // Off, in either spelling, needs nothing.
        let p =
            block(r#"{"rules": {"naming.table": "off", "change.window": {"severity": "off"}}}"#);
        assert_eq!(p.check(), Vec::<String>::new());
        // And a rule whose default is on needs nothing more than it has.
        let p = block(r#"{"rules": {"data.max-rows": {"rows": 50}}}"#);
        assert_eq!(p.check(), Vec::<String>::new());
    }

    /// `OFF` is accepted as a severity everywhere else; the window check
    /// compared it case-sensitively and demanded an `allow` for a rule that
    /// is off.
    #[test]
    fn a_change_window_switched_off_in_any_case_needs_no_allow_window() {
        for word in ["off", "OFF", "Off"] {
            let p = block(&format!(
                r#"{{"rules": {{"change.window": {{"severity": "{word}", "offset": "+08:00"}}}}}}"#
            ));
            assert_eq!(p.check(), Vec::<String>::new(), "{word}");
            assert_eq!(p.effective("change.window").severity, None);
        }
    }

    #[test]
    fn a_change_window_that_allows_nothing_is_refused_before_it_refuses_every_plan() {
        let p = block(r#"{"rules": {"change.window": {"severity": "error", "offset": "+08:00"}}}"#);
        let problems = p.check();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("no `allow`"), "{problems:?}");
        let p = block(
            r#"{"rules": {"change.window": {"offset": "+08:00", "allow": ["mon-fri 09:00-17:00"]}}}"#,
        );
        assert_eq!(p.check(), Vec::<String>::new());
    }
}
