//! The versioned analysis-scope compatibility rule for SQL Server.
//!
//! `compare` is a pure function over two sides' facts: no connection, no
//! candidate image, no version-string heuristics. An image tag or an equal
//! `ProductVersion` is not executable identity, so neither can pass the
//! executable check (ADR-0016 cases 5, 23). The rule's coverage is part of
//! its name; a fact outside it is not compared and is not claimed.
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
use pbps_db::resolver::environment::{
    BuildMapping, EnvironmentFacts, FactStatus, RuleVersion, ScopeReport, Side,
};
use std::collections::BTreeMap;

/// Rule `mssql-analysis-scope-v1`, measured on SQL Server 2025 (17.0) for
/// Linux.
pub const RULE: &str = "mssql-analysis-scope-v1";

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

pub fn compare(
    target: &EnvironmentFacts,
    resolver: &EnvironmentFacts,
    mappings: &[BuildMapping],
) -> ScopeReport {
    let mut report = ScopeReport::new(RuleVersion::new(RULE));
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
                target.catalog.observations.get(*key),
                resolver.catalog.observations.get(*key),
            ),
        );
    }
    for key in OPTIONAL_OBSERVATIONS {
        report.facts.insert(
            (*key).into(),
            optional_observation(
                target.catalog.observations.get(*key),
                resolver.catalog.observations.get(*key),
            ),
        );
    }
    editions(target, resolver, &mut report);
    settings(target, resolver, &mut report);
    assemblies(target, resolver, &mut report);
    visibility(target, resolver, &mut report);
    pbps_db::resolver::environment::compare_executables(
        RULE,
        target,
        resolver,
        mappings,
        &mut report,
    );
    report
}

fn value<'a>(facts: &'a EnvironmentFacts, key: &str) -> Option<&'a str> {
    facts.catalog.observations.get(key)?.value()
}

/// Both sides must report a product version inside the measured range, or
/// the rule has nothing to say. An unparseable version is unknown, not zero.
fn version_gate(target: &EnvironmentFacts, resolver: &EnvironmentFacts) -> Option<FactStatus> {
    let major = |facts: &EnvironmentFacts| -> Option<u32> {
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
                    reason: format!("{RULE} was not measured on SQL Server product major {major}"),
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
fn family_gate(target: &EnvironmentFacts, resolver: &EnvironmentFacts) -> Option<FactStatus> {
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
                        "EngineEdition {family} is a hosted or unknown product family; {RULE} qualifies the boxed product only and maps no hosted version to a boxed build"
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
fn editions(target: &EnvironmentFacts, resolver: &EnvironmentFacts, report: &mut ScopeReport) {
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
fn settings(target: &EnvironmentFacts, resolver: &EnvironmentFacts, report: &mut ScopeReport) {
    for name in SETTINGS {
        let status = match (
            target.catalog.settings.get(*name),
            resolver.catalog.settings.get(*name),
        ) {
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
    let persisted = |facts: &EnvironmentFacts| -> Option<String> {
        let get = |name: &str| facts.catalog.settings.get(name).map(|s| s.value.as_str());
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
fn assemblies(target: &EnvironmentFacts, resolver: &EnvironmentFacts, report: &mut ScopeReport) {
    let describe = |e: &pbps_db::resolver::environment::ExtensionFact| {
        format!(
            "{} {} {}",
            e.version,
            e.schema,
            e.libraries.first().map_or("-", String::as_str)
        )
    };
    let by_name = |facts: &EnvironmentFacts| -> BTreeMap<String, String> {
        facts
            .catalog
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
fn visibility(target: &EnvironmentFacts, resolver: &EnvironmentFacts, report: &mut ScopeReport) {
    for (schema, seen) in &target.catalog.visibility {
        let status = match resolver.catalog.visibility.get(schema) {
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
    use pbps_db::resolver::environment::{
        CatalogFacts, ExecutableIdentity, ExecutableRole, ExecutableSet, ExtensionFact, Provenance,
        SettingFact, Verdict,
    };

    fn side(version: &str, engine_digest: &str) -> EnvironmentFacts {
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
        EnvironmentFacts {
            catalog: CatalogFacts {
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
            },
            executables: ExecutableSet {
                engine: ExecutableIdentity {
                    role: ExecutableRole::Engine,
                    path: "/opt/mssql/bin/sqlservr".into(),
                    digest: Some(engine_digest.into()),
                    provenance: Provenance::LoadedContent,
                    disk_differs_from_loaded: Some(false),
                },
                libraries: Vec::new(),
            },
        }
    }

    fn set(facts: &mut EnvironmentFacts, key: &str, value: &str) {
        facts
            .catalog
            .observations
            .insert(key.into(), Observation::reported(Some(value)));
    }

    #[test]
    fn a_mapped_disk_candidate_keeps_the_engine_scope_unknown_on_either_side() {
        let mut loaded = side("17.0.4075.5", "e1");
        loaded.executables.libraries = vec![ExecutableIdentity {
            role: ExecutableRole::Preloaded,
            path: "/opt/mssql/lib/engine.sfp".into(),
            digest: Some("same-content".into()),
            provenance: Provenance::LoadedContent,
            disk_differs_from_loaded: Some(false),
        }];
        assert_eq!(compare(&loaded, &loaded, &[]).verdict(), Verdict::Verified);
        let mut candidate = loaded.clone();
        candidate.executables.libraries[0].provenance = Provenance::DiskCandidate;
        for (target, resolver, side) in [
            (&candidate, &loaded, Side::Target),
            (&loaded, &candidate, Side::Resolver),
        ] {
            let report = compare(target, resolver, &[]);
            assert_eq!(
                report.facts["library:/opt/mssql/lib/engine.sfp"],
                FactStatus::Unknown {
                    side,
                    reason: "mapped content not readable, disk candidate only".into(),
                }
            );
            assert_eq!(
                report.verdict(),
                Verdict::Unknown(vec!["library:/opt/mssql/lib/engine.sfp".into()])
            );
        }
        candidate.executables.libraries[0].role = ExecutableRole::LateLoaded;
        assert_eq!(
            compare(&loaded, &candidate, &[]).verdict(),
            Verdict::Verified
        );
        assert_eq!(
            compare(&candidate, &loaded, &[]).verdict(),
            Verdict::Verified
        );
    }

    #[test]
    fn identical_sides_verify_under_the_named_rule() {
        let report = compare(&side("17.0.4075.5", "e1"), &side("17.0.4075.5", "e1"), &[]);
        assert_eq!(report.rule.as_str(), RULE);
        assert_eq!(report.verdict(), Verdict::Verified, "{report:?}");
        assert!(report.limitations.is_empty());
    }

    /// Case 23: the same advertised build with different engine content is
    /// not the same engine, and a mapping is honoured only for this rule,
    /// this scope and exactly this pair.
    #[test]
    fn the_same_build_with_different_engine_content_is_a_mismatch() {
        let (target, resolver) = (side("17.0.4075.5", "e1"), side("17.0.4075.5", "e2"));
        assert_eq!(
            compare(&target, &resolver, &[]).verdict(),
            Verdict::Mismatch(vec!["executable:engine".into()])
        );
        let mapping = |rule: &str, scope: &str| BuildMapping {
            rule: RuleVersion::new(rule),
            target: "e1".into(),
            resolver: "e2".into(),
            scope: scope.into(),
            measured: "measured pair".into(),
        };
        let accepted = compare(&target, &resolver, &[mapping(RULE, "engine")]);
        assert_eq!(accepted.verdict(), Verdict::Verified);
        assert!(accepted.limitations.contains_key("mapping:engine"));
        for foreign in [
            mapping("pg-analysis-scope-v1", "engine"),
            mapping(RULE, "/opt/mssql/lib/other.so"),
        ] {
            assert_eq!(
                compare(&target, &resolver, &[foreign]).verdict(),
                Verdict::Mismatch(vec!["executable:engine".into()])
            );
        }
    }

    #[test]
    fn a_different_build_or_update_is_a_mismatch_and_an_unmeasured_major_is_unknown() {
        let target = side("17.0.4075.5", "e1");
        assert_eq!(
            compare(&target, &side("17.0.4065.1", "e1"), &[]).verdict(),
            Verdict::Mismatch(vec!["product_version".into()])
        );
        let mut updated = side("17.0.4075.5", "e1");
        set(&mut updated, "product_update_level", "CU8");
        assert_eq!(
            compare(&target, &updated, &[]).verdict(),
            Verdict::Mismatch(vec!["product_update_level".into()])
        );
        for unmeasured in ["16.0.4135.4", "18.0.1000.1", "seventeen", ""] {
            let report = compare(&side(unmeasured, "e1"), &target, &[]);
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
        let boxed = side("17.0.4075.5", "e1");
        for hosted in ["5", "6", "8", "9", "11", "12", ""] {
            let mut target = side("17.0.4075.5", "e1");
            set(&mut target, "engine_edition", hosted);
            for report in [compare(&target, &boxed, &[]), compare(&boxed, &target, &[])] {
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
        let mut express = side("17.0.4075.5", "e1");
        set(&mut express, "engine_edition", "4");
        set(&mut express, "edition", "Express Edition (64-bit)");
        let report = compare(&express, &boxed, &[]);
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
            let mut resolver = side("17.0.4075.5", "e1");
            set(&mut resolver, key, other);
            assert_eq!(
                compare(&side("17.0.4075.5", "e1"), &resolver, &[]).verdict(),
                Verdict::Mismatch(vec![key.into()]),
                "{key}"
            );
        }
    }

    #[test]
    fn a_platform_a_login_cannot_read_is_unknown_with_the_views_reason() {
        let mut target = side("17.0.4075.5", "e1");
        target.catalog.observations.insert(
            "host_platform".into(),
            Observation::Unknown {
                reason: "sys.dm_os_host_info is not readable by this login".into(),
            },
        );
        let report = compare(&target, &side("17.0.4075.5", "e1"), &[]);
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
        let mut resolver = side("17.0.4075.5", "e1");
        resolver.catalog.settings.get_mut("language").unwrap().value = "Deutsch".into();
        resolver
            .catalog
            .settings
            .get_mut("dateformat")
            .unwrap()
            .value = "dmy".into();
        assert_eq!(
            compare(&side("17.0.4075.5", "e1"), &resolver, &[]).verdict(),
            Verdict::Mismatch(vec!["setting:dateformat".into(), "setting:language".into()])
        );
        let mut missing = side("17.0.4075.5", "e1");
        missing.catalog.settings.remove("arithabort");
        assert_eq!(
            compare(&missing, &side("17.0.4075.5", "e1"), &[]).facts["setting:arithabort"],
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
            let mut facts = side("17.0.4075.5", "e1");
            facts.catalog.settings.get_mut(name).unwrap().value = "0".into();
            facts
        };
        for name in ["quoted_identifier", "ansi_nulls"] {
            let report = compare(&off(name), &off(name), &[]);
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
            let mut facts = side("17.0.4075.5", "e1");
            facts.catalog.extensions.push(assembly(digest));
            facts
        };
        let plain = side("17.0.4075.5", "e1");
        assert_eq!(
            compare(&with("aa"), &with("aa"), &[]).verdict(),
            Verdict::Verified
        );
        for (target, resolver) in [
            (with("aa"), with("bb")),
            (with("aa"), plain.clone()),
            (plain.clone(), with("aa")),
        ] {
            assert_eq!(
                compare(&target, &resolver, &[]).verdict(),
                Verdict::Mismatch(vec!["assembly:geo".into()])
            );
        }
    }

    #[test]
    fn a_schema_missing_on_the_resolver_changes_what_a_bare_name_binds_through() {
        let mut resolver = side("17.0.4075.5", "e1");
        resolver
            .catalog
            .visibility
            .insert("app".into(), Observation::reported(Some(r#"["dbo"]"#)));
        assert_eq!(
            compare(&side("17.0.4075.5", "e1"), &resolver, &[]).verdict(),
            Verdict::Mismatch(vec!["visibility:app".into()])
        );
        let mut unasked = side("17.0.4075.5", "e1");
        unasked.catalog.visibility.clear();
        assert_eq!(
            compare(&side("17.0.4075.5", "e1"), &unasked, &[]).verdict(),
            Verdict::Mismatch(vec!["visibility:app".into()])
        );
    }
}
