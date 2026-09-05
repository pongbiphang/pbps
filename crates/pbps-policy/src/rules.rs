//! The catalogue: every rule this tool knows, with its evaluation point and
//! its default (ADR-0008 decision 6).
//!
//! A closed list on purpose. A rule id is what a project re-weights and
//! suppresses by, so it has to be stable and it has to be *known*: a typo in
//! `policies:` is refused by name rather than silently configuring nothing.

use pbps_model::Severity;

/// Where a rule is evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// Over the declarations, by `validate`.
    Declaration,
    /// Over the change set, by `plan` — offline and connected alike.
    Plan,
    /// Over the change set, by `plan --db` only: the rule needs a moment and a
    /// target, which an offline preview does not have.
    ConnectedPlan,
}

/// One rule as the catalogue describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub id: &'static str,
    pub point: Point,
    /// `None` means off unless the project switches it on.
    pub default: Option<Severity>,
    /// What it checks, in the operator's words.
    pub about: &'static str,
    /// The parameter names it accepts; anything else in its config is refused.
    pub params: &'static [&'static str],
    /// The parameters a rule cannot run without. A rule switched on without
    /// one is refused by the block's check rather than evaluated as a no-op:
    /// `naming.table: error` with no `pattern` accepted every name, and
    /// `change.window: error` with no `allow` refused every plan.
    pub required: &'static [&'static str],
}

pub const NAMING_TABLE: &str = "naming.table";
pub const NAMING_COLUMN: &str = "naming.column";
pub const NAMING_INDEX: &str = "naming.index";
pub const NAMING_CONSTRAINT: &str = "naming.constraint";
pub const DATA_MAX_ROWS: &str = "data.max-rows";
pub const COLUMN_NO_DEPRECATED_TYPE: &str = "column.no-deprecated-type";
pub const CHANGE_EXPAND_CONTRACT: &str = "change.expand-contract";
pub const CHANGE_NARROWING_ON_DATA: &str = "change.narrowing-on-data";
pub const GRANT_WIDEN: &str = "grant.widen";
pub const CHANGE_WINDOW: &str = "change.window";

pub const RULES: [Rule; 10] = [
    Rule {
        id: NAMING_TABLE,
        point: Point::Declaration,
        default: None,
        about: "a table's name (without its schema) matches `pattern`",
        params: &["pattern"],
        required: &["pattern"],
    },
    Rule {
        id: NAMING_COLUMN,
        point: Point::Declaration,
        default: None,
        about: "a column's name matches `pattern`",
        params: &["pattern"],
        required: &["pattern"],
    },
    Rule {
        id: NAMING_INDEX,
        point: Point::Declaration,
        default: None,
        about: "an index's name matches `pattern`",
        params: &["pattern"],
        required: &["pattern"],
    },
    Rule {
        id: NAMING_CONSTRAINT,
        point: Point::Declaration,
        default: None,
        about: "a named constraint (primary key, unique, foreign key, check) matches `pattern`",
        params: &["pattern"],
        required: &["pattern"],
    },
    Rule {
        id: DATA_MAX_ROWS,
        point: Point::Declaration,
        default: Some(Severity::Warning),
        about: "a `data:` block declares at most `rows` rows — past that it stops looking like \
                reference data (ADR-0004)",
        params: &["rows"],
        required: &[],
    },
    Rule {
        id: COLUMN_NO_DEPRECATED_TYPE,
        point: Point::Declaration,
        default: Some(Severity::Warning),
        about: "a column does not use a type the engine has deprecated (text, ntext, image)",
        params: &[],
        required: &[],
    },
    Rule {
        id: CHANGE_EXPAND_CONTRACT,
        point: Point::Plan,
        default: Some(Severity::Warning),
        about: "one revision does not both add and drop or narrow in the same table, which \
                usually wants expand/contract staging (SPEC §13.3)",
        params: &[],
        required: &[],
    },
    Rule {
        id: CHANGE_NARROWING_ON_DATA,
        point: Point::Plan,
        default: Some(Severity::Note),
        about: "a type narrowing depends on the data already stored; the pre-flight probe \
                measures it at apply time",
        params: &[],
        required: &[],
    },
    Rule {
        id: GRANT_WIDEN,
        point: Point::Plan,
        default: Some(Severity::Note),
        about: "a grant widens what a role can do (ADR-0005), for a project that wants the \
                label at a severity of its own",
        params: &[],
        required: &[],
    },
    Rule {
        id: CHANGE_WINDOW,
        point: Point::ConnectedPlan,
        default: None,
        about: "a connected plan is computed inside an allowed change window: `offset` \
                (`+08:00`) and `allow` (`mon-fri 09:00-17:00`, ...)",
        params: &["offset", "allow"],
        required: &["allow"],
    },
];

/// The rule with this id, or `None` for an id the catalogue does not have.
pub fn rule(id: &str) -> Option<&'static Rule> {
    RULES.iter().find(|r| r.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rule_has_a_dotted_id_and_an_explanation() {
        let mut seen = std::collections::BTreeSet::new();
        for r in &RULES {
            assert!(r.id.contains('.'), "{}", r.id);
            assert!(r.about.len() > 20, "{} needs a real explanation", r.id);
            assert!(seen.insert(r.id), "{} twice", r.id);
            assert_eq!(rule(r.id), Some(r));
            for req in r.required {
                assert!(
                    r.params.contains(req),
                    "{}: `{req}` is required but not a parameter",
                    r.id
                );
            }
        }
        assert_eq!(rule("naming.tabel"), None);
    }

    /// The change-window rule needs a clock and a target; it must never be
    /// evaluated offline, where "outside the window" would be a lie about a
    /// moment nobody chose.
    #[test]
    fn the_change_window_is_a_connected_rule_and_off_by_default() {
        let r = rule(CHANGE_WINDOW).unwrap();
        assert_eq!(r.point, Point::ConnectedPlan);
        assert_eq!(r.default, None);
    }
}
