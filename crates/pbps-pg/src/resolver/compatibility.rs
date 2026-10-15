//! The versioned analysis-scope compatibility rule for PostgreSQL.
//!
//! `compare` is a pure function over two sides' facts: no connection, no
//! candidate, no version-string heuristics. What it does not receive it
//! cannot be swayed by — a `Candidate` image tag or an equal
//! `server_version` string is not an input, so neither can pass a check
//! (ADR-0016 case 5, 23). The rule's coverage is part of its name; a fact
//! outside it is not compared and is not claimed.

use pbps_db::resolver::Observation;
use pbps_db::resolver::environment::{CatalogFacts, FactStatus, RuleVersion, ScopeReport, Side};
use std::collections::BTreeMap;

/// Rule `pg-reported-scope-v2`, measured on PostgreSQL 16 and 18.
///
/// The identifier moves whenever the set of facts the rule compares does:
/// `OBSERVATIONS`, `OPTIONAL_OBSERVATIONS`, `SETTINGS`, `OPTIONAL_SETTINGS` or
/// a family of facts `compare_reported` reports. Evidence seals the identifier,
/// and an older build accepts evidence by it alone, so a changed set under an
/// unchanged identifier is read by that build as the coverage it required
/// (`xmloption` left the set under `pg-reported-scope-v1`, #2012, DEC-2012.1). `the_compared_facts_move_with_the_rule_identifier` fails when they
/// part, naming what to change.
pub const REPORTED_RULE: &str = "pg-reported-scope-v2";

/// The identifiers `validate_evidence` reads: the current one only. An earlier
/// identifier compared another set of facts, so evidence sealed under it is
/// refused by name rather than read as this build's coverage. Nothing but
/// `explain` reads a saved resolved plan today (apply refuses all of them,
/// #616), so the remedy is a new plan.
pub(crate) const ACCEPTED_RULES: &[&str] = &[REPORTED_RULE];

/// The major versions the rule was measured on. Anything else is unknown,
/// not incompatible: PostgreSQL 16 changed role-membership semantics and 15
/// introduced actual collation versions, and the rule has not been run
/// against what came before.
const MEASURED_MAJORS: std::ops::RangeInclusive<u32> = 16..=18;

/// Version, encoding and locale observations every database reports; a NULL
/// here is unreadable, not absent.
const OBSERVATIONS: &[&str] = &[
    "server_version_num",
    "database_encoding",
    "database_collate",
    "database_ctype",
    "database_locale_provider",
];

/// Locale facts that exist only for some providers: `datlocale` and the ICU
/// rules are NULL on a libc database by design (measured on 16 and 18). NULL
/// on both sides is a known absence and matches; a value on one side only is
/// a difference. Only an unreadable fact is unknown.
const OPTIONAL_OBSERVATIONS: &[&str] = &["database_locale", "database_icu_rules"];

/// Effective settings the rule compares: the ambient settings that change
/// binding or loaded code and that no pin covers. Every setting the dialect
/// pins before a deployment statement (`session_pins` in `lib.rs`, DECISIONS
/// 458) is absent from it: a deployment statement never runs under a server's
/// own default for `TimeZone` or `DateStyle`, so two servers whose defaults
/// differ deploy identically, and comparing those defaults refused scopes
/// whose deployments were the same (finding on #688; DECISIONS 520 compares
/// what the dialect does not pin). The list is not a count to keep in step
/// with the pins: `no_setting_the_dialect_or_the_capture_pins_is_compared`
/// derives the pins from the dialect and refuses an overlap.
///
/// `xmloption` left this list when DEC-1634.1 pinned it (#1997), and for two
/// reasons, both measured on 16 and 18 with a target whose value was
/// `document`. The compile and the captures run under `content`, so the
/// reconstruction never sees the target's value and the comparison refused a
/// scope whose deployments were the same. And the capture sets it for its own
/// transaction (`catalog::capture_path`), so observing it as a session input
/// reads `content` inside the read and the database's value after it, and
/// every read on such a target ended as "session inputs changed during target
/// capture" — a database default, of which the scratch is given a copy, was
/// refused as surely as a server-level value, of which it is not. The set
/// changing is what made the rule `pg-reported-scope-v2` (#2012).
pub(crate) const SETTINGS: &[&str] = &[
    "server_encoding",
    "client_encoding",
    "lc_numeric",
    "lc_monetary",
    "lc_time",
    "array_nulls",
    "default_text_search_config",
    "row_security",
    "shared_preload_libraries",
    "session_preload_libraries",
    "local_preload_libraries",
    "dynamic_library_path",
];

/// A setting that exists only on some measured versions. Absent on both
/// sides is a known absence; present on one side only is a difference.
pub(crate) const OPTIONAL_SETTINGS: &[&str] = &["restrict_nonsystem_relation_kind"];

/// Settings `pg_settings` hides from a login that is neither a superuser nor
/// a member of `pg_read_all_settings` (measured on 16 and 18: the rows are
/// simply absent). A least-privilege deployer therefore cannot report them,
/// and the refusal has to say what grant makes them readable rather than
/// "not reported".
const SUPERUSER_ONLY_SETTINGS: &[&str] = &[
    "shared_preload_libraries",
    "session_preload_libraries",
    "dynamic_library_path",
];

fn missing_reason(name: &str) -> String {
    if SUPERUSER_ONLY_SETTINGS.contains(&name) {
        format!(
            "{name} is not visible to this login; it is superuser-only until the login is a member of pg_read_all_settings"
        )
    } else {
        "not reported".into()
    }
}

/// Rule `pg-reported-scope-v2`: the catalog facts both engines report, and
/// no executables. The resolver observes no process and so reads no
/// executable; the operator vouches for the build, and what each engine
/// reports about itself is all that is compared (DEC-1528.1, #1672).
pub fn compare_reported(target: &CatalogFacts, resolver: &CatalogFacts) -> ScopeReport {
    let mut report = ScopeReport::new(RuleVersion::new(REPORTED_RULE));
    if catalog(REPORTED_RULE, target, resolver, &mut report) {
        build_string(target, resolver, &mut report);
    }
    report
}

/// The build string under the reported rule: recorded, never a refusal by
/// itself. It names the packaging as much as the build, so a managed or
/// distribution-packaged target and a container scratch of one version
/// report two strings, which is the common pairing. The version number,
/// extensions, collations and settings, which do decide binding, are compared
/// above; a packaging that patched name resolution apart within one version
/// is what the operator vouches for (maintainer's decision on #1678). An
/// unreadable string is still unknown, never a match.
fn build_string(target: &CatalogFacts, resolver: &CatalogFacts, report: &mut ScopeReport) {
    let read = |facts: &CatalogFacts| {
        facts
            .observations
            .get("server_version")
            .and_then(Observation::value)
            .map(str::to_owned)
    };
    match (read(target), read(resolver)) {
        (Some(t), Some(r)) if t != r => {
            report.limitations.insert(
                "build:server_version".into(),
                format!("the target reports {t} and the scratch {r}; the version number matched"),
            );
        }
        _ => {
            report.facts.insert(
                "server_version".into(),
                observation(
                    target.observations.get("server_version"),
                    resolver.observations.get("server_version"),
                ),
            );
        }
    }
}

/// The catalog facts both rules compare. `false` when the version gate
/// stopped the comparison before anything else was read.
fn catalog(
    rule: &str,
    target: &CatalogFacts,
    resolver: &CatalogFacts,
    report: &mut ScopeReport,
) -> bool {
    if let Some(gate) = version_gate(rule, target, resolver) {
        report.facts.insert("server_version_num".into(), gate);
        return false;
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
    extensions(target, resolver, report);
    collations(target, resolver, report);
    settings(target, resolver, report);
    visibility(target, resolver, report);
    true
}

/// Both sides must report a version inside the measured range, or the rule
/// has nothing to say. An unparseable version is unknown, not zero.
fn version_gate(rule: &str, target: &CatalogFacts, resolver: &CatalogFacts) -> Option<FactStatus> {
    let major = |facts: &CatalogFacts| -> Option<u32> {
        facts
            .observations
            .get("server_version_num")?
            .value()?
            .parse::<u32>()
            .ok()
            .map(|num| num / 10_000)
    };
    let sides = [
        (Side::Target, major(target)),
        (Side::Resolver, major(resolver)),
    ];
    for (side, major) in sides {
        match major {
            None => {
                return Some(FactStatus::Unknown {
                    side,
                    reason: "server_version_num not reported".into(),
                });
            }
            Some(major) if !MEASURED_MAJORS.contains(&major) => {
                return Some(FactStatus::Unknown {
                    side,
                    reason: format!("{rule} was not measured on PostgreSQL {major}"),
                });
            }
            Some(_) => {}
        }
    }
    None
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
            reason: "not reported".into(),
        },
        (None, _) => FactStatus::Unknown {
            side: Side::Target,
            reason: "not reported".into(),
        },
        (_, None) => FactStatus::Unknown {
            side: Side::Resolver,
            reason: "not reported".into(),
        },
    }
}

/// A fact that is legitimately NULL for some providers. `NotReported` on both
/// sides is a known absence, not an unknown: both sides answered the query
/// and the column was NULL. Only an `Unknown` observation — a fact that could
/// not be read at all — makes the comparison unanswerable.
fn optional_observation(
    target: Option<&Observation>,
    resolver: Option<&Observation>,
) -> FactStatus {
    let not_read = |side| FactStatus::Unknown {
        side,
        reason: "not read".into(),
    };
    match (target, resolver) {
        (None, None) => not_read(Side::Both),
        (None, Some(_)) => not_read(Side::Target),
        (Some(_), None) => not_read(Side::Resolver),
        (Some(Observation::Unknown { reason }), Some(_)) => FactStatus::Unknown {
            side: Side::Target,
            reason: reason.clone(),
        },
        (Some(_), Some(Observation::Unknown { reason })) => FactStatus::Unknown {
            side: Side::Resolver,
            reason: reason.clone(),
        },
        (Some(Observation::NotReported), Some(Observation::NotReported)) => FactStatus::Match,
        (Some(Observation::NotReported), Some(Observation::Observed { value })) => {
            FactStatus::Mismatch {
                target: "absent".into(),
                resolver: value.clone(),
            }
        }
        (Some(Observation::Observed { value }), Some(Observation::NotReported)) => {
            FactStatus::Mismatch {
                target: value.clone(),
                resolver: "absent".into(),
            }
        }
        (Some(Observation::Observed { value: t }), Some(Observation::Observed { value: r })) => {
            if t == r {
                FactStatus::Match
            } else {
                FactStatus::Mismatch {
                    target: t.clone(),
                    resolver: r.clone(),
                }
            }
        }
    }
}

/// Every extension the target has, at its version, must be installable on
/// the resolver, and so must everything it requires. The resolver's scratch
/// database is fresh, so what it has installed is not compared.
fn extensions(target: &CatalogFacts, resolver: &CatalogFacts, report: &mut ScopeReport) {
    for extension in &target.extensions {
        let key = format!("extension:{}", extension.name);
        let available = resolver
            .available_extensions
            .get(&extension.name)
            .is_some_and(|versions| versions.contains(&extension.version));
        let missing_requirement = extension
            .requires
            .iter()
            .find(|name| !resolver.available_extensions.contains_key(*name));
        let status = match (available, missing_requirement) {
            (true, None) => FactStatus::Match,
            (false, _) => FactStatus::Mismatch {
                target: format!("{} {}", extension.name, extension.version),
                resolver: "not available at that version".into(),
            },
            (true, Some(required)) => FactStatus::Mismatch {
                target: format!("{} requires {required}", extension.name),
                resolver: format!("{required} not available"),
            },
        };
        report.facts.insert(key, status);
    }
}

/// Provider, locale, rules and the provider's *actual* version must agree.
/// A target whose recorded version no longer matches its actual one is in
/// PostgreSQL's own collation-warning state; that is recorded as a named
/// limitation, not a resolver mismatch.
fn collations(target: &CatalogFacts, resolver: &CatalogFacts, report: &mut ScopeReport) {
    let by_key: BTreeMap<_, _> = resolver
        .collations
        .iter()
        .map(|collation| (collation.key.as_str(), collation))
        .collect();
    for collation in &target.collations {
        // Only the database's own default collation is required to be present:
        // it is fixed at CREATE DATABASE and reproduced by the recipe. A
        // user-defined collation is object DDL the compilation creates, not
        // something a fresh scratch database must already contain, and its
        // reproducibility follows from the resolver's build (compared through
        // the executables), so requiring it here would refuse an otherwise
        // compatible server (finding on #688).
        if collation.key != "default" {
            continue;
        }
        let key = format!("collation:{}", collation.key);
        let Some(other) = by_key.get(collation.key.as_str()) else {
            report.facts.insert(
                key,
                FactStatus::Mismatch {
                    target: collation.provider.clone(),
                    resolver: "absent".into(),
                },
            );
            continue;
        };
        if let (Some(recorded), Some(actual)) = (
            collation.recorded_version.value(),
            collation.actual_version.value(),
        ) && recorded != actual
        {
            report.limitations.insert(
                format!("{key}:recorded-version"),
                format!("the target recorded {recorded} but its provider now reports {actual}"),
            );
        }
        let status = if collation.provider != other.provider {
            FactStatus::Mismatch {
                target: collation.provider.clone(),
                resolver: other.provider.clone(),
            }
        } else {
            [
                (&collation.locale, &other.locale),
                (&collation.rules, &other.rules),
                (&collation.actual_version, &other.actual_version),
            ]
            .into_iter()
            .map(|(t, r)| optional_observation(Some(t), Some(r)))
            .find(|status| *status != FactStatus::Match)
            .unwrap_or(FactStatus::Match)
        };
        report.facts.insert(key, status);
    }
}

/// A setting the planning session set on itself is not a deployment
/// setting: `source = session` on the target is unknown, because the value
/// apply will run under is whatever that session did not set.
fn settings(target: &CatalogFacts, resolver: &CatalogFacts, report: &mut ScopeReport) {
    let compare = |name: &str, optional: bool| -> Option<FactStatus> {
        let key = format!("setting:{name}");
        let (t, r) = (target.settings.get(name), resolver.settings.get(name));
        let status = match (t, r) {
            (None, None) if optional => return None,
            (None, None) => FactStatus::Unknown {
                side: Side::Both,
                reason: missing_reason(name),
            },
            (None, Some(_)) if optional => FactStatus::Mismatch {
                target: "absent".into(),
                resolver: "present".into(),
            },
            (Some(_), None) if optional => FactStatus::Mismatch {
                target: "present".into(),
                resolver: "absent".into(),
            },
            (None, Some(_)) => FactStatus::Unknown {
                side: Side::Target,
                reason: missing_reason(name),
            },
            (Some(_), None) => FactStatus::Unknown {
                side: Side::Resolver,
                reason: missing_reason(name),
            },
            (Some(t), Some(_)) if t.source == "session" => FactStatus::Unknown {
                side: Side::Target,
                reason: "set in the planning session, not a deployment setting".into(),
            },
            (Some(t), Some(r)) if t.value == r.value => FactStatus::Match,
            (Some(t), Some(r)) => FactStatus::Mismatch {
                target: t.value.clone(),
                resolver: r.value.clone(),
            },
        };
        let _ = key;
        Some(status)
    };
    for name in SETTINGS {
        if let Some(status) = compare(name, false) {
            report.facts.insert(format!("setting:{name}"), status);
        }
    }
    for name in OPTIONAL_SETTINGS {
        if let Some(status) = compare(name, true) {
            report.facts.insert(format!("setting:{name}"), status);
        }
    }
}

/// The engine's effective schema order for each in-scope schema must be the
/// same on both sides as the deployer: a schema the deployer cannot see on
/// the target but can on the resolver (or the reverse) binds a competing
/// object differently. A schema the resolver was not asked about is a
/// difference, not a gap.
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
    use pbps_db::resolver::environment::{CollationFact, ExtensionFact, SettingFact, Verdict};

    fn observed(value: &str) -> Observation {
        Observation::reported(Some(value))
    }

    /// A side that matches itself on every fact the rule measures.
    fn side(version: &str) -> CatalogFacts {
        let observations = [
            ("server_version_num", version),
            ("server_version", "18.6 (Debian 18.6-1.pgdg13+2)"),
            ("database_encoding", "UTF8"),
            ("database_collate", "en_US.utf8"),
            ("database_ctype", "en_US.utf8"),
            ("database_locale_provider", "c"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), observed(v)))
        .chain([
            // A libc database: datlocale and the ICU rules are NULL by design.
            ("database_locale".to_owned(), Observation::NotReported),
            ("database_icu_rules".to_owned(), Observation::NotReported),
        ])
        .collect();
        let settings = SETTINGS
            .iter()
            .chain(OPTIONAL_SETTINGS)
            .map(|name| {
                (
                    (*name).to_owned(),
                    SettingFact {
                        value: "v".into(),
                        source: "default".into(),
                        context: "user".into(),
                    },
                )
            })
            .collect();
        CatalogFacts {
            observations,
            visibility: [("app".to_owned(), observed("{pg_catalog,app,pg_temp}"))]
                .into_iter()
                .collect(),
            extensions: vec![ExtensionFact {
                name: "hstore".into(),
                version: "1.8".into(),
                schema: "public".into(),
                requires: vec![],
                libraries: vec!["$libdir/hstore".into()],
            }],
            available_extensions: [("hstore".to_owned(), vec!["1.8".to_owned()])]
                .into_iter()
                .collect(),
            collations: vec![CollationFact {
                key: "default".into(),
                provider: "c".into(),
                locale: observed("en_US.utf8"),
                rules: Observation::NotReported,
                recorded_version: observed("2.41"),
                actual_version: observed("2.41"),
            }],
            settings,
        }
    }

    #[test]
    fn the_reported_rule_compares_every_catalog_fact_and_no_executable() {
        // Different engine content, which the measured rule refuses, is not
        // an input to the reported one: it observes no process (#1672).
        let report = compare_reported(&side("180006"), &side("180006"));
        assert_eq!(report.verdict(), Verdict::Verified, "{report:?}");
        assert_eq!(report.rule.as_str(), REPORTED_RULE);
        assert!(!report.facts.keys().any(|key| key.starts_with("executable")));
        // Negative: every catalog fact still counts, the version gate too.
        let mut resolver = side("180006");
        resolver.settings.get_mut("lc_numeric").unwrap().value = "de_DE".into();
        assert_eq!(
            compare_reported(&side("180006"), &resolver).verdict(),
            Verdict::Mismatch(vec!["setting:lc_numeric".into()])
        );
        assert_eq!(
            compare_reported(&side("180006"), &side("180005")).verdict(),
            Verdict::Mismatch(vec!["server_version_num".into()])
        );
        // Another packaging of the same version is recorded, not refused.
        let mut repackaged = side("180006");
        repackaged
            .observations
            .insert("server_version".into(), observed("18.6 (Ubuntu 18.6-1)"));
        let report = compare_reported(&side("180006"), &repackaged);
        assert_eq!(report.verdict(), Verdict::Verified, "{report:?}");
        assert!(
            report.limitations["build:server_version"].contains("18.6 (Ubuntu 18.6-1)"),
            "{report:?}"
        );
        // Negative: an unreadable build string is unknown, never a match.
        repackaged.observations.remove("server_version");
        assert!(matches!(
            compare_reported(&side("180006"), &repackaged).verdict(),
            Verdict::Unknown(facts) if facts == ["server_version"]
        ));
        let gated = compare_reported(&side("150013"), &side("150013"));
        assert!(matches!(
            &gated.facts["server_version_num"],
            FactStatus::Unknown { reason, .. } if reason.contains(REPORTED_RULE)
        ));
    }

    #[test]
    fn a_version_the_rule_was_not_measured_on_is_unknown_not_incompatible() {
        for version in ["150013", "190000"] {
            let report = compare_reported(&side(version), &side(version));
            assert!(matches!(
                report.facts["server_version_num"],
                FactStatus::Unknown { .. }
            ));
            assert_eq!(report.facts.len(), 1, "nothing else is compared");
        }
        let mut garbage = side("180006");
        garbage
            .observations
            .insert("server_version_num".into(), observed("eighteen"));
        assert!(matches!(
            compare_reported(&garbage, &side("180006")).facts["server_version_num"],
            FactStatus::Unknown {
                side: Side::Target,
                ..
            }
        ));
    }

    #[test]
    fn a_different_patch_level_is_a_mismatch() {
        let report = compare_reported(&side("180006"), &side("180005"));
        assert_eq!(
            report.facts["server_version_num"],
            FactStatus::Mismatch {
                target: "180006".into(),
                resolver: "180005".into()
            }
        );
    }

    #[test]
    fn an_extension_the_resolver_cannot_install_is_a_mismatch() {
        let mut resolver = side("180006");
        resolver
            .available_extensions
            .insert("hstore".into(), vec!["1.7".into()]);
        assert_eq!(
            compare_reported(&side("180006"), &resolver).verdict(),
            Verdict::Mismatch(vec!["extension:hstore".into()])
        );
        let mut target = side("180006");
        target.extensions[0].requires.push("plperl".into());
        assert_eq!(
            compare_reported(&target, &side("180006")).verdict(),
            Verdict::Mismatch(vec!["extension:hstore".into()])
        );
    }

    #[test]
    fn collation_provider_locale_and_actual_version_must_agree() {
        let mut resolver = side("180006");
        resolver.collations[0].actual_version = observed("2.39");
        assert_eq!(
            compare_reported(&side("180006"), &resolver).verdict(),
            Verdict::Mismatch(vec!["collation:default".into()])
        );
        let mut icu = side("180006");
        icu.collations[0].provider = "i".into();
        assert_eq!(
            compare_reported(&side("180006"), &icu).verdict(),
            Verdict::Mismatch(vec!["collation:default".into()])
        );
    }

    #[test]
    fn a_target_whose_recorded_collation_version_drifted_is_a_limitation_not_a_mismatch() {
        let mut target = side("180006");
        target.collations[0].recorded_version = observed("2.36");
        let report = compare_reported(&target, &side("180006"));
        assert_eq!(report.verdict(), Verdict::Verified);
        assert!(
            report
                .limitations
                .contains_key("collation:default:recorded-version")
        );
    }

    #[test]
    fn no_setting_the_dialect_or_the_capture_pins_is_compared() {
        // The pins are read from the dialect, not copied: a setting pinned
        // later leaves the compared list by failing here, which is how
        // `xmloption` stayed in it after DEC-1634.1 pinned it (#1997).
        let write = crate::session_pins(&pbps_model::SessionZone::utc());
        let read = crate::catalog::capture_path();
        let mut pinned: std::collections::BTreeSet<String> = write
            .split(';')
            .filter_map(|statement| statement.trim().strip_prefix("SET "))
            .filter_map(|statement| statement.split_whitespace().next())
            .map(str::to_lowercase)
            .collect();
        pinned.extend(
            read.split("set_config('")
                .skip(1)
                .filter_map(|call| call.split('\'').next())
                .map(str::to_lowercase),
        );
        // Not vacuous: the parsers found what they are known to carry.
        for known in ["timezone", "datestyle", "xmloption", "search_path", "jit"] {
            assert!(pinned.contains(known), "{known} in {pinned:?}");
        }
        for name in SETTINGS.iter().chain(OPTIONAL_SETTINGS) {
            assert!(
                !pinned.contains(&name.to_lowercase()),
                "{name} is pinned by the dialect or the capture, so it is not compared"
            );
        }
    }

    /// Every identifier the rule has had, oldest first, each with the names of
    /// the facts a matching pair is compared on (`extension:`, `collation:` and
    /// `visibility:` stand for one fact per name the fixture carries). A change
    /// to what is compared appends an entry and bumps `REPORTED_RULE`; an
    /// earlier entry is a record and is never edited, because evidence sealed
    /// under it is refused by that name (`ACCEPTED_RULES`, DEC-2012.1).
    const RULE_HISTORY: &[(&str, &[&str])] = &[
        // Through #2010, which removed `xmloption` from the settings.
        (
            "pg-reported-scope-v1",
            &[
                "collation:default",
                "database_collate",
                "database_ctype",
                "database_encoding",
                "database_icu_rules",
                "database_locale",
                "database_locale_provider",
                "extension:hstore",
                "server_version",
                "server_version_num",
                "setting:array_nulls",
                "setting:client_encoding",
                "setting:default_text_search_config",
                "setting:dynamic_library_path",
                "setting:lc_monetary",
                "setting:lc_numeric",
                "setting:lc_time",
                "setting:local_preload_libraries",
                "setting:restrict_nonsystem_relation_kind",
                "setting:row_security",
                "setting:server_encoding",
                "setting:session_preload_libraries",
                "setting:shared_preload_libraries",
                "setting:xmloption",
                "visibility:app",
            ],
        ),
        (
            "pg-reported-scope-v2",
            &[
                "collation:default",
                "database_collate",
                "database_ctype",
                "database_encoding",
                "database_icu_rules",
                "database_locale",
                "database_locale_provider",
                "extension:hstore",
                "server_version",
                "server_version_num",
                "setting:array_nulls",
                "setting:client_encoding",
                "setting:default_text_search_config",
                "setting:dynamic_library_path",
                "setting:lc_monetary",
                "setting:lc_numeric",
                "setting:lc_time",
                "setting:local_preload_libraries",
                "setting:restrict_nonsystem_relation_kind",
                "setting:row_security",
                "setting:server_encoding",
                "setting:session_preload_libraries",
                "setting:shared_preload_libraries",
                "visibility:app",
            ],
        ),
    ];

    /// Why a rule history is not a valid record, if it is not. Each entry's names
    /// are read as a set: the list order is only how the history is written, so
    /// a later entry that reorders the names before it compares nothing new
    /// and would invalidate every saved plan under the earlier identifier for
    /// no change (#2034).
    fn refuse_history(history: &[(&str, &[&str])]) -> Result<(), String> {
        let sets: Vec<std::collections::BTreeSet<&str>> = history
            .iter()
            .map(|(identifier, names)| {
                let set: std::collections::BTreeSet<&str> = names.iter().copied().collect();
                if set.len() == names.len() {
                    Ok(set)
                } else {
                    Err(format!("{identifier} lists a name twice"))
                }
            })
            .collect::<Result<_, _>>()?;
        for (pair, sets) in history.windows(2).zip(sets.windows(2)) {
            if pair[0].0 == pair[1].0 {
                return Err(format!("{} is reused", pair[1].0));
            }
            if sets[0] == sets[1] {
                return Err(format!(
                    "{} compares what {} compared",
                    pair[1].0, pair[0].0
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn a_history_entry_that_only_reorders_or_repeats_names_is_refused() {
        // Negative: the same facts in another order are not a new rule, and
        // neither is a name listed twice or an identifier used twice.
        assert!(refuse_history(&[("a", &["x", "y"]), ("b", &["y", "x"])]).is_err());
        assert!(refuse_history(&[("a", &["x", "y"]), ("b", &["x", "x", "y"])]).is_err());
        assert!(refuse_history(&[("a", &["x", "x"]), ("b", &["y"])]).is_err());
        assert!(refuse_history(&[("a", &["x"]), ("a", &["y"])]).is_err());
        // Positive: a changed set is accepted in either direction, whatever
        // the order it is written in.
        assert!(refuse_history(&[("a", &["x", "y"]), ("b", &["y"])]).is_ok());
        assert!(refuse_history(&[("a", &["y"]), ("b", &["z", "y"])]).is_ok());
        assert!(refuse_history(&[("a", &["x"])]).is_ok());
    }

    #[test]
    fn the_compared_facts_move_with_the_rule_identifier() {
        let (current, pinned) = RULE_HISTORY.last().unwrap();
        let report = compare_reported(&side("180006"), &side("180006"));
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
             record: {:?}): an older build reads evidence by the identifier alone, so give the \
             rule a new one. Bump REPORTED_RULE, append the new identifier and its facts to \
             RULE_HISTORY without editing the entry above it, and record it in DEC-2012.1",
            compared.difference(&pinned_set).collect::<Vec<_>>(),
            pinned_set.difference(&compared).collect::<Vec<_>>(),
        );
        // A bump that compares the same facts is not a new rule, an identifier
        // is not reused for another set, and no entry lists a name twice.
        assert_eq!(refuse_history(RULE_HISTORY), Ok(()));
        // Only the newest identifier is read; each earlier one is refused by
        // name, whatever it compared.
        assert_eq!(ACCEPTED_RULES, [REPORTED_RULE]);
        for (earlier, _) in &RULE_HISTORY[..RULE_HISTORY.len() - 1] {
            assert!(!ACCEPTED_RULES.contains(earlier), "{earlier}");
        }
    }

    #[test]
    fn a_target_whose_xmloption_differs_is_not_refused_for_it() {
        // Not compared, so not a fact either, whatever either side reports
        // (#1997); a compared setting beside it still counts.
        let document = SettingFact {
            value: "document".into(),
            source: "configuration file".into(),
            context: "user".into(),
        };
        let mut target = side("180006");
        target.settings.insert("xmloption".into(), document);
        let report = compare_reported(&target, &side("180006"));
        assert_eq!(report.verdict(), Verdict::Verified, "{report:?}");
        assert!(!report.facts.contains_key("setting:xmloption"));
        target.settings.get_mut("array_nulls").unwrap().value = "off".into();
        assert_eq!(
            compare_reported(&target, &side("180006")).verdict(),
            Verdict::Mismatch(vec!["setting:array_nulls".into()])
        );
    }

    #[test]
    fn a_setting_the_planning_session_set_on_itself_is_unknown() {
        let mut target = side("180006");
        target
            .settings
            .get_mut("default_text_search_config")
            .unwrap()
            .source = "session".into();
        let report = compare_reported(&target, &side("180006"));
        assert!(matches!(
            report.facts["setting:default_text_search_config"],
            FactStatus::Unknown {
                side: Side::Target,
                ..
            }
        ));
    }

    #[test]
    fn a_differing_setting_is_a_mismatch_and_an_optional_one_may_be_absent_on_both_sides() {
        let mut resolver = side("180006");
        resolver.settings.get_mut("lc_numeric").unwrap().value = "de_DE".into();
        assert_eq!(
            compare_reported(&side("180006"), &resolver).verdict(),
            Verdict::Mismatch(vec!["setting:lc_numeric".into()])
        );
        let mut t = side("180006");
        let mut r = side("180006");
        t.settings.remove("restrict_nonsystem_relation_kind");
        r.settings.remove("restrict_nonsystem_relation_kind");
        let report = compare_reported(&t, &r);
        assert!(
            !report
                .facts
                .contains_key("setting:restrict_nonsystem_relation_kind")
        );
        assert_eq!(report.verdict(), Verdict::Verified);
        // Present on one side only is a difference, not a known absence.
        assert_eq!(
            compare_reported(&side("180006"), &r).verdict(),
            Verdict::Mismatch(vec!["setting:restrict_nonsystem_relation_kind".into()])
        );
    }

    #[test]
    fn a_provider_dependent_fact_null_on_both_sides_matches_but_not_on_one_side_or_unread() {
        // Both libc: locale and rules are NULL on both sides and that is agreement.
        let report = compare_reported(&side("180006"), &side("180006"));
        assert_eq!(report.facts["database_locale"], FactStatus::Match);
        assert_eq!(report.facts["collation:default"], FactStatus::Match);
        // An ICU resolver has a locale where the libc target has none.
        let mut icu = side("180006");
        icu.observations
            .insert("database_locale".into(), observed("en-US"));
        assert_eq!(
            compare_reported(&side("180006"), &icu).facts["database_locale"],
            FactStatus::Mismatch {
                target: "absent".into(),
                resolver: "en-US".into()
            }
        );
        // NULL is not the same as "could not read": an unread fact stays unknown.
        let mut unread = side("180006");
        unread.observations.insert(
            "database_icu_rules".into(),
            Observation::Unknown {
                reason: "catalog column unreadable".into(),
            },
        );
        assert!(matches!(
            compare_reported(&unread, &side("180006")).facts["database_icu_rules"],
            FactStatus::Unknown {
                side: Side::Target,
                ..
            }
        ));
        let mut unread_collation = side("180006");
        unread_collation.collations[0].actual_version = Observation::Unknown {
            reason: "pg_collation_actual_version failed".into(),
        };
        assert!(matches!(
            compare_reported(&unread_collation, &side("180006")).facts["collation:default"],
            FactStatus::Unknown {
                side: Side::Target,
                ..
            }
        ));
    }

    #[test]
    fn the_deployers_effective_schema_order_must_agree_for_every_in_scope_schema() {
        // The resolver's principal can see a schema the target's deployer cannot.
        let mut resolver = side("180006");
        resolver
            .visibility
            .insert("app".into(), observed("{pg_catalog,shadow,app,pg_temp}"));
        assert_eq!(
            compare_reported(&side("180006"), &resolver).verdict(),
            Verdict::Mismatch(vec!["visibility:app".into()])
        );
        // A schema the resolver was never asked about is a difference.
        let mut target = side("180006");
        target
            .visibility
            .insert("audit".into(), observed("{pg_catalog,audit,pg_temp}"));
        assert_eq!(
            compare_reported(&target, &side("180006")).verdict(),
            Verdict::Mismatch(vec!["visibility:audit".into()])
        );
        // Unreadable visibility is unknown, never an empty path that matches.
        let mut unread = side("180006");
        unread.visibility.insert(
            "app".into(),
            Observation::Unknown {
                reason: "current_schemas failed".into(),
            },
        );
        assert!(matches!(
            compare_reported(&unread, &side("180006")).facts["visibility:app"],
            FactStatus::Unknown {
                side: Side::Target,
                ..
            }
        ));
    }

    #[test]
    fn a_setting_hidden_from_the_deployer_names_the_grant_that_would_reveal_it() {
        let mut target = side("180006");
        target.settings.remove("session_preload_libraries");
        let report = compare_reported(&target, &side("180006"));
        match &report.facts["setting:session_preload_libraries"] {
            FactStatus::Unknown { side, reason } => {
                assert_eq!(*side, Side::Target);
                assert!(reason.contains("pg_read_all_settings"), "{reason}");
            }
            other @ (FactStatus::Match | FactStatus::Mismatch { .. }) => {
                panic!("expected unknown, got {other:?}")
            }
        }
        // A setting that is not superuser-only is simply unreported.
        let mut plain = side("180006");
        plain.settings.remove("lc_numeric");
        assert_eq!(
            compare_reported(&plain, &side("180006")).facts["setting:lc_numeric"],
            FactStatus::Unknown {
                side: Side::Target,
                reason: "not reported".into()
            }
        );
    }

    #[test]
    fn a_fact_neither_side_reports_is_unknown_never_an_empty_match() {
        let mut t = side("180006");
        let mut r = side("180006");
        t.settings.remove("row_security");
        r.settings.remove("row_security");
        assert_eq!(
            compare_reported(&t, &r).facts["setting:row_security"],
            FactStatus::Unknown {
                side: Side::Both,
                reason: "not reported".into()
            }
        );
        t.observations
            .insert("database_encoding".into(), Observation::NotReported);
        assert!(matches!(
            compare_reported(&t, &r).facts["database_encoding"],
            FactStatus::Unknown {
                side: Side::Target,
                ..
            }
        ));
    }
}
