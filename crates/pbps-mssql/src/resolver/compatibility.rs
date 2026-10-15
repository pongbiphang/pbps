//! The versioned analysis-scope compatibility rule for SQL Server.
//!
//! `compare_reported` is a pure function over the facts each engine reports
//! about itself: no connection, no candidate image, no version-string
//! heuristics, and no executable, which the resolver never observes (#1636).
//! The rule's coverage is part of its name; a fact outside it is not
//! compared and is not claimed.
//!
//! Two things are SQL Server's own. The product *family* is a premise, not a
//! version: Azure SQL Database, Managed Instance, Synapse and Edge report
//! their own version numbers, which name no boxed build, so a hosted or
//! unknown family is refused by name and never mapped to an image by guess.
//! And an *edition* difference inside the boxed family is not a binding
//! difference — a Developer scratch resolves names exactly as a Standard
//! target does — but it is a capability difference, so it is recorded as a
//! limitation: the target's own edition checks stay the authority, and a
//! statement the Developer scratch accepts proves nothing about them
//! (DECISIONS 521).

use super::environment::SETTINGS;
use pbps_db::resolver::Observation;
use pbps_db::resolver::environment::{CatalogFacts, FactStatus, RuleVersion, ScopeReport, Side};
use std::collections::BTreeMap;

/// Rule `mssql-reported-scope-v1`, measured on SQL Server 2025 (17.0) for
/// Linux. It was `mssql-analysis-scope-v1` while it also compared the
/// measured profiles' executables (#1636).
///
/// The identifier moves whenever the set of facts the rule compares does:
/// `OBSERVATIONS`, `OPTIONAL_OBSERVATIONS`, `SETTINGS` or a family of facts
/// `compare_reported` reports. Evidence would seal the identifier, and an older
/// build would accept evidence by it alone, so a changed set under an
/// unchanged identifier would be read by that build as the coverage it
/// required (DEC-2012.1). No evidence seals this rule yet (#620), so
/// nothing reads it and no earlier identifier is refused;
/// `the_compared_facts_move_with_the_rule_identifier` is what keeps the first
/// reader from inheriting a set that moved unnoticed.
pub const REPORTED_RULE: &str = "mssql-reported-scope-v1";

/// The product major versions the rule was measured on. Anything else is
/// unknown, not incompatible.
const MEASURED_MAJORS: std::ops::RangeInclusive<u32> = 17..=17;

/// `EngineEdition` values of the boxed product: Standard (2), Enterprise,
/// Developer and Evaluation (3), Express (4). Everything else — Azure SQL
/// Database (5), Synapse (6, 11), Managed Instance (8), Edge (9), and values
/// not yet assigned — is a family this rule says nothing about.
const BOXED_FAMILY: &[&str] = &["2", "3", "4"];

/// Facts every database reports; a NULL here is unreadable, not absent.
const OBSERVATIONS: &[&str] = &[
    "product_version",
    "product_level",
    "host_platform",
    "server_collation",
    "database_collation",
    "database_compatibility_level",
    "database_containment",
    "database_ansi_null_default",
    "database_ansi_nulls",
    "database_ansi_padding",
    "database_ansi_warnings",
    "database_arithabort",
    "database_concat_null_yields_null",
    "database_numeric_roundabort",
    "database_quoted_identifier",
];

/// Facts that are legitimately NULL: an RTM build has no update level and no
/// update reference. NULL on both sides is a known absence and matches.
const OPTIONAL_OBSERVATIONS: &[&str] = &["product_update_level", "product_update_reference"];

pub fn compare_reported(target: &CatalogFacts, resolver: &CatalogFacts) -> ScopeReport {
    let mut report = ScopeReport::new(RuleVersion::new(REPORTED_RULE));
    if let Some(gate) = version_gate(target, resolver) {
        report.facts.insert("product_version".into(), gate);
        return report;
    }
    if let Some(gate) = family_gate(target, resolver) {
        report.facts.insert("engine_edition".into(), gate);
        return report;
    }
    for key in OBSERVATIONS {
        report.facts.insert(
            (*key).into(),
            observation(
                target.observations.get(*key),
                resolver.observations.get(*key),
            ),
        );
    }
    for key in OPTIONAL_OBSERVATIONS {
        report.facts.insert(
            (*key).into(),
            optional_observation(
                target.observations.get(*key),
                resolver.observations.get(*key),
            ),
        );
    }
    editions(target, resolver, &mut report);
    settings(target, resolver, &mut report);
    assemblies(target, resolver, &mut report);
    visibility(target, resolver, &mut report);
    report
}

fn value<'a>(facts: &'a CatalogFacts, key: &str) -> Option<&'a str> {
    facts.observations.get(key)?.value()
}

/// Both sides must report a product version inside the measured range, or
/// the rule has nothing to say. An unparseable version is unknown, not zero.
fn version_gate(target: &CatalogFacts, resolver: &CatalogFacts) -> Option<FactStatus> {
    let major = |facts: &CatalogFacts| -> Option<u32> {
        value(facts, "product_version")?
            .split('.')
            .next()?
            .parse::<u32>()
            .ok()
    };
    for (side, major) in [
        (Side::Target, major(target)),
        (Side::Resolver, major(resolver)),
    ] {
        match major {
            None => {
                return Some(FactStatus::Unknown {
                    side,
                    reason: "product_version not reported".into(),
                });
            }
            Some(major) if !MEASURED_MAJORS.contains(&major) => {
                return Some(FactStatus::Unknown {
                    side,
                    reason: format!(
                        "{REPORTED_RULE} was not measured on SQL Server product major {major}"
                    ),
                });
            }
            Some(_) => {}
        }
    }
    None
}

/// A hosted or unknown product family is refused by name before anything
/// else is compared: its version numbers are its own, and no fact below
/// means on it what it means on the boxed product.
fn family_gate(target: &CatalogFacts, resolver: &CatalogFacts) -> Option<FactStatus> {
    for (side, facts) in [(Side::Target, target), (Side::Resolver, resolver)] {
        match value(facts, "engine_edition") {
            None => {
                return Some(FactStatus::Unknown {
                    side,
                    reason: "engine_edition not reported".into(),
                });
            }
            Some(family) if !BOXED_FAMILY.contains(&family) => {
                return Some(FactStatus::Unknown {
                    side,
                    reason: format!(
                        "EngineEdition {family} is a hosted or unknown product family; {REPORTED_RULE} qualifies the boxed product only and maps no hosted version to a boxed build"
                    ),
                });
            }
            Some(_) => {}
        }
    }
    None
}

/// Inside the boxed family an edition difference is recorded, not refused:
/// binding is the same, capability is not, and capability stays the
/// target's to check.
fn editions(target: &CatalogFacts, resolver: &CatalogFacts, report: &mut ScopeReport) {
    report
        .facts
        .insert("engine_edition".into(), FactStatus::Match);
    let status = match (value(target, "edition"), value(resolver, "edition")) {
        (Some(_), Some(_)) => FactStatus::Match,
        (None, None) => unknown(Side::Both, "not reported"),
        (None, _) => unknown(Side::Target, "not reported"),
        (_, None) => unknown(Side::Resolver, "not reported"),
    };
    report.facts.insert("edition".into(), status);
    let differs = value(target, "engine_edition") != value(resolver, "engine_edition")
        || value(target, "edition") != value(resolver, "edition");
    if differs {
        report.limitations.insert(
            "edition".into(),
            format!(
                "the target is {} (EngineEdition {}) and the resolver is {} (EngineEdition {}): name binding is the same across boxed editions, feature availability is not, and the target's own edition and capability checks remain the authority — a statement this resolver accepts proves nothing about them",
                value(target, "edition").unwrap_or("?"),
                value(target, "engine_edition").unwrap_or("?"),
                value(resolver, "edition").unwrap_or("?"),
                value(resolver, "engine_edition").unwrap_or("?"),
            ),
        );
    }
}

fn unknown(side: Side, reason: &str) -> FactStatus {
    FactStatus::Unknown {
        side,
        reason: reason.into(),
    }
}

fn reason(observation: Option<&Observation>) -> String {
    match observation {
        Some(Observation::Unknown { reason }) => reason.clone(),
        Some(Observation::Observed { .. } | Observation::NotReported) | None => {
            "not reported".into()
        }
    }
}

fn observation(target: Option<&Observation>, resolver: Option<&Observation>) -> FactStatus {
    match (
        target.and_then(Observation::value),
        resolver.and_then(Observation::value),
    ) {
        (Some(t), Some(r)) if t == r => FactStatus::Match,
        (Some(t), Some(r)) => FactStatus::Mismatch {
            target: t.into(),
            resolver: r.into(),
        },
        (None, None) => FactStatus::Unknown {
            side: Side::Both,
            reason: reason(target),
        },
        (None, _) => FactStatus::Unknown {
            side: Side::Target,
            reason: reason(target),
        },
        (_, None) => FactStatus::Unknown {
            side: Side::Resolver,
            reason: reason(resolver),
        },
    }
}

/// `NotReported` on both sides is a known absence; only a fact that could
/// not be read at all makes the comparison unanswerable.
fn optional_observation(
    target: Option<&Observation>,
    resolver: Option<&Observation>,
) -> FactStatus {
    let readable = |o: Option<&Observation>| {
        matches!(
            o,
            Some(Observation::Observed { .. } | Observation::NotReported)
        )
    };
    match (readable(target), readable(resolver)) {
        (false, false) => unknown(Side::Both, "not read"),
        (false, true) => unknown(Side::Target, "not read"),
        (true, false) => unknown(Side::Resolver, "not read"),
        (true, true) => match (
            target.and_then(Observation::value),
            resolver.and_then(Observation::value),
        ) {
            (t, r) if t == r => FactStatus::Match,
            (t, r) => FactStatus::Mismatch {
                target: t.unwrap_or("not set").into(),
                resolver: r.unwrap_or("not set").into(),
            },
        },
    }
}

/// The session's effective statement settings must be the same on both
/// sides, and the two a module persists must be what this tool manages:
/// `QUOTED_IDENTIFIER` and `ANSI_NULLS` on. A module created with either off
/// is one the pull treats as unmanaged, so a deployment session that would
/// create them that way is not one the resolver can stand in for, even when
/// the scratch session is off in the same way.
fn settings(target: &CatalogFacts, resolver: &CatalogFacts, report: &mut ScopeReport) {
    for name in SETTINGS {
        let status = match (target.settings.get(*name), resolver.settings.get(*name)) {
            (Some(t), Some(r)) if t.value == r.value => FactStatus::Match,
            (Some(t), Some(r)) => FactStatus::Mismatch {
                target: t.value.clone(),
                resolver: r.value.clone(),
            },
            (None, None) => unknown(Side::Both, "not reported"),
            (None, Some(_)) => unknown(Side::Target, "not reported"),
            (Some(_), None) => unknown(Side::Resolver, "not reported"),
        };
        report.facts.insert(format!("setting:{name}"), status);
    }
    let persisted = |facts: &CatalogFacts| -> Option<String> {
        let get = |name: &str| facts.settings.get(name).map(|s| s.value.as_str());
        Some(format!(
            "ansi_nulls={} quoted_identifier={}",
            get("ansi_nulls")?,
            get("quoted_identifier")?
        ))
    };
    const MANAGED: &str = "ansi_nulls=1 quoted_identifier=1";
    let status = match (persisted(target), persisted(resolver)) {
        (Some(t), Some(r)) if t == MANAGED && r == MANAGED => FactStatus::Match,
        (Some(t), Some(r)) => FactStatus::Mismatch {
            target: t,
            resolver: r,
        },
        (None, None) => unknown(Side::Both, "not reported"),
        (None, Some(_)) => unknown(Side::Target, "not reported"),
        (Some(_), None) => unknown(Side::Resolver, "not reported"),
    };
    report
        .facts
        .insert("persisted-module-settings".into(), status);
}

/// User CLR assemblies are code the engine loads for the database, compared
/// by content in both directions: one the resolver lacks cannot be bound
/// through, and one only the resolver has binds names the target does not.
fn assemblies(target: &CatalogFacts, resolver: &CatalogFacts, report: &mut ScopeReport) {
    let describe = |e: &pbps_db::resolver::environment::ExtensionFact| {
        format!(
            "{} {} {}",
            e.version,
            e.schema,
            e.libraries.first().map_or("-", String::as_str)
        )
    };
    let by_name = |facts: &CatalogFacts| -> BTreeMap<String, String> {
        facts
            .extensions
            .iter()
            .map(|e| (e.name.clone(), describe(e)))
            .collect()
    };
    let (t, r) = (by_name(target), by_name(resolver));
    let names: std::collections::BTreeSet<&String> = t.keys().chain(r.keys()).collect();
    for name in names {
        let status = match (t.get(name), r.get(name)) {
            (Some(a), Some(b)) if a == b => FactStatus::Match,
            (a, b) => FactStatus::Mismatch {
                target: a.cloned().unwrap_or_else(|| "absent".into()),
                resolver: b.cloned().unwrap_or_else(|| "absent".into()),
            },
        };
        report.facts.insert(format!("assembly:{name}"), status);
    }
}

/// The schemas a bare name binds through must exist the same way on both
/// sides. A schema the resolver was not asked about is a difference, not a
/// gap.
fn visibility(target: &CatalogFacts, resolver: &CatalogFacts, report: &mut ScopeReport) {
    for (schema, seen) in &target.visibility {
        let status = match resolver.visibility.get(schema) {
            None => FactStatus::Mismatch {
                target: seen.value().unwrap_or("?").to_owned(),
                resolver: "schema not evaluated".into(),
            },
            Some(other) => observation(Some(seen), Some(other)),
        };
        report.facts.insert(format!("visibility:{schema}"), status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::environment::DATABASE_FIELDS;
    use pbps_db::resolver::environment::{ExtensionFact, SettingFact, Verdict};

    fn side(version: &str) -> CatalogFacts {
        let mut observations: BTreeMap<String, Observation> = DATABASE_FIELDS
            .iter()
            .map(|field| ((*field).to_owned(), Observation::reported(Some("0"))))
            .collect();
        for (key, value) in [
            ("product_version", version),
            ("product_level", "RTM"),
            ("edition", "Enterprise Developer Edition (64-bit)"),
            ("engine_edition", "3"),
            ("server_collation", "SQL_Latin1_General_CP1_CI_AS"),
            ("database_collation", "Latin1_General_100_CS_AS"),
            ("database_compatibility_level", "170"),
            ("database_containment", "NONE"),
            ("host_platform", "Linux"),
        ] {
            observations.insert(key.into(), Observation::reported(Some(value)));
        }
        observations.insert("product_update_level".into(), Observation::NotReported);
        observations.insert("product_update_reference".into(), Observation::NotReported);
        let settings = SETTINGS
            .iter()
            .map(|name| {
                let value = match *name {
                    "language" => "us_english",
                    "dateformat" => "mdy",
                    "datefirst" => "7",
                    "numeric_roundabort" => "0",
                    _ => "1",
                };
                (
                    (*name).to_owned(),
                    SettingFact {
                        value: value.into(),
                        source: "effective".into(),
                        context: "session".into(),
                    },
                )
            })
            .collect();
        CatalogFacts {
            observations,
            extensions: Vec::new(),
            available_extensions: BTreeMap::new(),
            collations: Vec::new(),
            settings,
            visibility: [(
                "app".to_owned(),
                Observation::reported(Some(r#"["app","dbo"]"#)),
            )]
            .into_iter()
            .collect(),
        }
    }

    fn set(facts: &mut CatalogFacts, key: &str, value: &str) {
        facts
            .observations
            .insert(key.into(), Observation::reported(Some(value)));
    }

    /// Every identifier the rule has had, oldest first, each with the names of
    /// the facts a matching pair is compared on (`assembly:` and `visibility:`
    /// stand for one fact per name the fixture carries). A change to what is
    /// compared appends an entry and bumps `REPORTED_RULE`; an earlier entry is
    /// a record and is never edited (DEC-2012.1). The third field names the fact
    /// whose verdict the entry changed under the same name, `None` for an entry
    /// that changes the set of names (#2047).
    const RULE_HISTORY: &[(&str, &[&str], Option<&str>)] = &[(
        "mssql-reported-scope-v1",
        &[
            "assembly:geo",
            "database_ansi_null_default",
            "database_ansi_nulls",
            "database_ansi_padding",
            "database_ansi_warnings",
            "database_arithabort",
            "database_collation",
            "database_compatibility_level",
            "database_concat_null_yields_null",
            "database_containment",
            "database_numeric_roundabort",
            "database_quoted_identifier",
            "edition",
            "engine_edition",
            "host_platform",
            "persisted-module-settings",
            "product_level",
            "product_update_level",
            "product_update_reference",
            "product_version",
            "server_collation",
            "setting:ansi_null_dflt_on",
            "setting:ansi_nulls",
            "setting:ansi_padding",
            "setting:ansi_warnings",
            "setting:arithabort",
            "setting:concat_null_yields_null",
            "setting:datefirst",
            "setting:dateformat",
            "setting:language",
            "setting:numeric_roundabort",
            "setting:quoted_identifier",
            "visibility:app",
        ],
        None,
    )];

    /// Why a rule history is not a valid record, if it is not. Each entry's names
    /// are read as a set: the list order is only how the history is written, so
    /// a later entry that reorders the names before it compares nothing new
    /// and would invalidate every saved plan under the earlier identifier for
    /// no change (#2034). An entry over the same set is a new rule only by
    /// declaring the fact whose verdict it changed, and that fact has to be one
    /// both it and the entry before it list: the name alone cannot say "same
    /// facts, changed verdict" (#2047, DEC-2012.1).
    fn refuse_history(history: &[(&str, &[&str], Option<&str>)]) -> Result<(), String> {
        let sets: Vec<std::collections::BTreeSet<&str>> = history
            .iter()
            .map(|(identifier, names, _)| {
                let set: std::collections::BTreeSet<&str> = names.iter().copied().collect();
                if set.len() == names.len() {
                    Ok(set)
                } else {
                    Err(format!("{identifier} lists a name twice"))
                }
            })
            .collect::<Result<_, _>>()?;
        if let Some((identifier, _, Some(fact))) = history.first() {
            return Err(format!(
                "{identifier} revises {fact}, but no entry before it compared it"
            ));
        }
        for (pair, sets) in history.windows(2).zip(sets.windows(2)) {
            if pair[0].0 == pair[1].0 {
                return Err(format!("{} is reused", pair[1].0));
            }
            match pair[1].2 {
                Some(fact) if sets[0].contains(fact) && sets[1].contains(fact) => {}
                Some(fact) => {
                    return Err(format!(
                        "{} revises {fact}, which {} and {} do not both list",
                        pair[1].0, pair[0].0, pair[1].0
                    ));
                }
                None if sets[0] == sets[1] => {
                    return Err(format!(
                        "{} compares what {} compared and declares no revised fact",
                        pair[1].0, pair[0].0
                    ));
                }
                None => {}
            }
        }
        Ok(())
    }

    #[test]
    fn a_history_entry_that_only_reorders_or_repeats_names_is_refused() {
        // Negative: the same facts in another order are not a new rule, and
        // neither is a name listed twice or an identifier used twice.
        assert!(refuse_history(&[("a", &["x", "y"], None), ("b", &["y", "x"], None)]).is_err());
        assert!(
            refuse_history(&[("a", &["x", "y"], None), ("b", &["x", "x", "y"], None)]).is_err()
        );
        assert!(refuse_history(&[("a", &["x", "x"], None), ("b", &["y"], None)]).is_err());
        assert!(refuse_history(&[("a", &["x"], None), ("a", &["y"], None)]).is_err());
        // Positive: a changed set is accepted in either direction, whatever
        // the order it is written in.
        assert!(refuse_history(&[("a", &["x", "y"], None), ("b", &["y"], None)]).is_ok());
        assert!(refuse_history(&[("a", &["y"], None), ("b", &["z", "y"], None)]).is_ok());
        assert!(refuse_history(&[("a", &["x"], None)]).is_ok());
    }

    #[test]
    fn a_history_entry_over_the_same_names_is_accepted_only_for_a_declared_revision() {
        // Positive: the same names under a new identifier are a new rule when
        // the entry declares which fact's verdict changed, whatever the order.
        assert!(
            refuse_history(&[
                ("a", &["x", "y"], None),
                ("b", &["y", "x"], Some("x")),
                ("c", &["x", "y"], Some("y")),
            ])
            .is_ok()
        );
        // A declaration beside a changed set is fine for a fact both list.
        assert!(refuse_history(&[("a", &["x", "y"], None), ("b", &["x"], Some("x"))]).is_ok());
        // Negative: no declaration, so the entry still compares nothing new.
        assert!(refuse_history(&[("a", &["x", "y"], None), ("b", &["x", "y"], None)]).is_err());
        // Negative: the declared fact is not one the entry lists.
        assert!(
            refuse_history(&[("a", &["x", "y"], None), ("b", &["x", "y"], Some("z"))]).is_err()
        );
        // Negative: nor one the entry before it compared, so nothing was
        // revised; and the oldest entry has nothing to revise.
        assert!(refuse_history(&[("a", &["x"], None), ("b", &["x", "z"], Some("z"))]).is_err());
        assert!(refuse_history(&[("a", &["x"], Some("x"))]).is_err());
    }

    #[test]
    fn the_compared_facts_move_with_the_rule_identifier() {
        let (current, pinned, _) = RULE_HISTORY.last().unwrap();
        // A pair that carries an assembly too, or its family would go unpinned.
        let mut facts = side("17.0.4075.5");
        facts.extensions.push(ExtensionFact {
            name: "geo".into(),
            version: "geo, version=1.0.0.0".into(),
            schema: "SAFE_ACCESS".into(),
            requires: Vec::new(),
            libraries: vec!["digest".into()],
        });
        let report = compare_reported(&facts, &facts);
        let compared: std::collections::BTreeSet<&str> =
            report.facts.keys().map(String::as_str).collect();
        let pinned_set: std::collections::BTreeSet<&str> = pinned.iter().copied().collect();
        assert_eq!(
            REPORTED_RULE, *current,
            "REPORTED_RULE is not the newest entry of RULE_HISTORY: a bump appends the new \
             identifier with the facts it compares"
        );
        assert_eq!(
            compared,
            pinned_set,
            "the facts compared under {REPORTED_RULE} changed (only here: {:?}; only in the \
             record: {:?}): evidence is read by the identifier alone, so give the rule a new \
             one. Bump REPORTED_RULE, append the new identifier and its facts to RULE_HISTORY \
             without editing the entry above it, and record it in DEC-2012.1",
            compared.difference(&pinned_set).collect::<Vec<_>>(),
            pinned_set.difference(&compared).collect::<Vec<_>>(),
        );
        // A bump that compares the same facts is not a new rule unless it
        // declares the fact whose verdict changed, an identifier is not reused
        // for another set, and no entry lists a name twice.
        assert_eq!(refuse_history(RULE_HISTORY), Ok(()));
    }

    #[test]
    fn identical_sides_verify_under_the_named_rule() {
        let report = compare_reported(&side("17.0.4075.5"), &side("17.0.4075.5"));
        assert_eq!(report.rule.as_str(), REPORTED_RULE);
        assert_eq!(report.verdict(), Verdict::Verified, "{report:?}");
        assert!(report.limitations.is_empty());
    }

    #[test]
    fn a_different_build_or_update_is_a_mismatch_and_an_unmeasured_major_is_unknown() {
        let target = side("17.0.4075.5");
        assert_eq!(
            compare_reported(&target, &side("17.0.4065.1")).verdict(),
            Verdict::Mismatch(vec!["product_version".into()])
        );
        let mut updated = side("17.0.4075.5");
        set(&mut updated, "product_update_level", "CU8");
        assert_eq!(
            compare_reported(&target, &updated).verdict(),
            Verdict::Mismatch(vec!["product_update_level".into()])
        );
        for unmeasured in ["16.0.4135.4", "18.0.1000.1", "seventeen", ""] {
            let report = compare_reported(&side(unmeasured), &target);
            assert_eq!(
                report.verdict(),
                Verdict::Unknown(vec!["product_version".into()]),
                "{unmeasured}"
            );
            // The gate stops the comparison: nothing below it is claimed.
            assert_eq!(report.facts.len(), 1);
        }
    }

    /// A hosted family is refused by name on either side, whatever its
    /// version says; an edition difference inside the boxed family is
    /// verified with the limitation that capability stays the target's.
    #[test]
    fn a_hosted_family_is_unknown_and_a_boxed_edition_difference_is_a_named_limitation() {
        let boxed = side("17.0.4075.5");
        for hosted in ["5", "6", "8", "9", "11", "12", ""] {
            let mut target = side("17.0.4075.5");
            set(&mut target, "engine_edition", hosted);
            for report in [
                compare_reported(&target, &boxed),
                compare_reported(&boxed, &target),
            ] {
                assert_eq!(
                    report.verdict(),
                    Verdict::Unknown(vec!["engine_edition".into()]),
                    "{hosted}"
                );
                match &report.facts["engine_edition"] {
                    FactStatus::Unknown { reason, .. } => {
                        assert!(
                            reason.contains("hosted or unknown product family"),
                            "{reason}"
                        )
                    }
                    FactStatus::Match | FactStatus::Mismatch { .. } => panic!("{report:?}"),
                }
            }
        }
        let mut express = side("17.0.4075.5");
        set(&mut express, "engine_edition", "4");
        set(&mut express, "edition", "Express Edition (64-bit)");
        let report = compare_reported(&express, &boxed);
        assert_eq!(report.verdict(), Verdict::Verified);
        let limitation = &report.limitations["edition"];
        assert!(
            limitation.contains("Express Edition") && limitation.contains("remain the authority"),
            "{limitation}"
        );
    }

    #[test]
    fn a_collation_compatibility_level_platform_or_database_option_difference_is_a_mismatch() {
        for (key, other) in [
            ("server_collation", "Latin1_General_100_CI_AS"),
            ("database_collation", "SQL_Latin1_General_CP1_CI_AS"),
            ("database_compatibility_level", "160"),
            ("database_containment", "PARTIAL"),
            ("database_ansi_padding", "1"),
            ("host_platform", "Windows"),
        ] {
            let mut resolver = side("17.0.4075.5");
            set(&mut resolver, key, other);
            assert_eq!(
                compare_reported(&side("17.0.4075.5"), &resolver).verdict(),
                Verdict::Mismatch(vec![key.into()]),
                "{key}"
            );
        }
    }

    #[test]
    fn a_platform_a_login_cannot_read_is_unknown_with_the_views_reason() {
        let mut target = side("17.0.4075.5");
        target.observations.insert(
            "host_platform".into(),
            Observation::Unknown {
                reason: "sys.dm_os_host_info is not readable by this login".into(),
            },
        );
        let report = compare_reported(&target, &side("17.0.4075.5"));
        assert_eq!(
            report.facts["host_platform"],
            FactStatus::Unknown {
                side: Side::Target,
                reason: "sys.dm_os_host_info is not readable by this login".into()
            }
        );
        assert_eq!(
            report.verdict(),
            Verdict::Unknown(vec!["host_platform".into()])
        );
    }

    #[test]
    fn a_session_setting_difference_is_a_mismatch_and_a_missing_one_is_unknown() {
        let mut resolver = side("17.0.4075.5");
        resolver.settings.get_mut("language").unwrap().value = "Deutsch".into();
        resolver.settings.get_mut("dateformat").unwrap().value = "dmy".into();
        assert_eq!(
            compare_reported(&side("17.0.4075.5"), &resolver).verdict(),
            Verdict::Mismatch(vec!["setting:dateformat".into(), "setting:language".into()])
        );
        let mut missing = side("17.0.4075.5");
        missing.settings.remove("arithabort");
        assert_eq!(
            compare_reported(&missing, &side("17.0.4075.5")).facts["setting:arithabort"],
            FactStatus::Unknown {
                side: Side::Target,
                reason: "not reported".into()
            }
        );
    }

    /// Equal is not enough for the two settings a module persists: a session
    /// that would create modules with either off is refused even when the
    /// scratch session is off in the same way.
    #[test]
    fn module_settings_off_on_both_sides_are_still_a_mismatch() {
        let off = |name: &str| {
            let mut facts = side("17.0.4075.5");
            facts.settings.get_mut(name).unwrap().value = "0".into();
            facts
        };
        for name in ["quoted_identifier", "ansi_nulls"] {
            let report = compare_reported(&off(name), &off(name));
            assert_eq!(report.facts[&format!("setting:{name}")], FactStatus::Match);
            assert_eq!(
                report.verdict(),
                Verdict::Mismatch(vec!["persisted-module-settings".into()]),
                "{name}"
            );
        }
    }

    #[test]
    fn a_clr_assembly_is_compared_by_content_in_both_directions() {
        let assembly = |digest: &str| ExtensionFact {
            name: "geo".into(),
            version: "geo, version=1.0.0.0".into(),
            schema: "SAFE_ACCESS".into(),
            requires: Vec::new(),
            libraries: vec![digest.into()],
        };
        let with = |digest: &str| {
            let mut facts = side("17.0.4075.5");
            facts.extensions.push(assembly(digest));
            facts
        };
        let plain = side("17.0.4075.5");
        assert_eq!(
            compare_reported(&with("aa"), &with("aa")).verdict(),
            Verdict::Verified
        );
        for (target, resolver) in [
            (with("aa"), with("bb")),
            (with("aa"), plain.clone()),
            (plain.clone(), with("aa")),
        ] {
            assert_eq!(
                compare_reported(&target, &resolver).verdict(),
                Verdict::Mismatch(vec!["assembly:geo".into()])
            );
        }
    }

    #[test]
    fn a_schema_missing_on_the_resolver_changes_what_a_bare_name_binds_through() {
        let mut resolver = side("17.0.4075.5");
        resolver
            .visibility
            .insert("app".into(), Observation::reported(Some(r#"["dbo"]"#)));
        assert_eq!(
            compare_reported(&side("17.0.4075.5"), &resolver).verdict(),
            Verdict::Mismatch(vec!["visibility:app".into()])
        );
        let mut unasked = side("17.0.4075.5");
        unasked.visibility.clear();
        assert_eq!(
            compare_reported(&side("17.0.4075.5"), &unasked).verdict(),
            Verdict::Mismatch(vec!["visibility:app".into()])
        );
    }
}
