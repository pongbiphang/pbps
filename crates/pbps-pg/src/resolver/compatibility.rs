//! The versioned analysis-scope compatibility rule for PostgreSQL.
//!
//! `compare` is a pure function over two sides' facts: no connection, no
//! candidate, no version-string heuristics. What it does not receive it
//! cannot be swayed by — a `Candidate` image tag or an equal
//! `server_version` string is not an input, so neither can pass a check
//! (ADR-0016 case 5, 23). The rule's coverage is part of its name; a fact
//! outside it is not compared and is not claimed.

use pbps_db::resolver::Observation;
use pbps_db::resolver::environment::{
    BuildMapping, EnvironmentFacts, ExecutableIdentity, ExecutableRole, FactStatus, Provenance,
    RuleVersion, ScopeReport, Side,
};
use std::collections::BTreeMap;

/// Rule `pg-analysis-scope-v1`, measured on PostgreSQL 16 and 18.
pub const RULE: &str = "pg-analysis-scope-v1";

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

/// Effective settings the rule compares. The first nine are what the dialect
/// pins before every deployment statement (DECISIONS 458), read back so the
/// pin is proven rather than assumed; the rest are ambient settings that
/// change binding or loaded code and that no pin covers.
pub(crate) const SETTINGS: &[&str] = &[
    "standard_conforming_strings",
    "check_function_bodies",
    "DateStyle",
    "TimeZone",
    "IntervalStyle",
    "timezone_abbreviations",
    "transform_null_equals",
    "bytea_output",
    "extra_float_digits",
    "server_encoding",
    "client_encoding",
    "lc_numeric",
    "lc_monetary",
    "lc_time",
    "array_nulls",
    "xmloption",
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

pub fn compare(
    target: &EnvironmentFacts,
    resolver: &EnvironmentFacts,
    mappings: &[BuildMapping],
) -> ScopeReport {
    let mut report = ScopeReport::new(RuleVersion::new(RULE));
    if let Some(gate) = version_gate(target, resolver) {
        report.facts.insert("server_version_num".into(), gate);
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
    extensions(target, resolver, &mut report);
    collations(target, resolver, &mut report);
    settings(target, resolver, &mut report);
    visibility(target, resolver, &mut report);
    executables(target, resolver, mappings, &mut report);
    report
}

/// Both sides must report a version inside the measured range, or the rule
/// has nothing to say. An unparseable version is unknown, not zero.
fn version_gate(target: &EnvironmentFacts, resolver: &EnvironmentFacts) -> Option<FactStatus> {
    let major = |facts: &EnvironmentFacts| -> Option<u32> {
        facts
            .catalog
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
                    reason: format!("{RULE} was not measured on PostgreSQL {major}"),
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
fn extensions(target: &EnvironmentFacts, resolver: &EnvironmentFacts, report: &mut ScopeReport) {
    for extension in &target.catalog.extensions {
        let key = format!("extension:{}", extension.name);
        let available = resolver
            .catalog
            .available_extensions
            .get(&extension.name)
            .is_some_and(|versions| versions.contains(&extension.version));
        let missing_requirement = extension
            .requires
            .iter()
            .find(|name| !resolver.catalog.available_extensions.contains_key(*name));
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
fn collations(target: &EnvironmentFacts, resolver: &EnvironmentFacts, report: &mut ScopeReport) {
    let by_key: BTreeMap<_, _> = resolver
        .catalog
        .collations
        .iter()
        .map(|collation| (collation.key.as_str(), collation))
        .collect();
    for collation in &target.catalog.collations {
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
fn settings(target: &EnvironmentFacts, resolver: &EnvironmentFacts, report: &mut ScopeReport) {
    let compare = |name: &str, optional: bool| -> Option<FactStatus> {
        let key = format!("setting:{name}");
        let (t, r) = (
            target.catalog.settings.get(name),
            resolver.catalog.settings.get(name),
        );
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

/// Content identity, not version. Equal digests match; different digests
/// match only through a mapping measured for exactly this pair and scope;
/// anything unreadable is unknown. A library present on one side only is a
/// difference in both directions — a resolver-only preload changes binding
/// as surely as a missing one.
fn executables(
    target: &EnvironmentFacts,
    resolver: &EnvironmentFacts,
    mappings: &[BuildMapping],
    report: &mut ScopeReport,
) {
    let engine = identity(
        "engine",
        Some(&target.executables.engine),
        Some(&resolver.executables.engine),
        mappings,
        report,
    );
    report.facts.insert("executable:engine".into(), engine);
    let (t, r) = (
        by_path(&target.executables.libraries),
        by_path(&resolver.executables.libraries),
    );
    let paths: std::collections::BTreeSet<&str> = t.keys().chain(r.keys()).copied().collect();
    for path in paths {
        let status = identity(
            path,
            t.get(path).copied(),
            r.get(path).copied(),
            mappings,
            report,
        );
        report.facts.insert(format!("library:{path}"), status);
    }
}

fn by_path(set: &[ExecutableIdentity]) -> BTreeMap<&str, &ExecutableIdentity> {
    set.iter()
        .map(|library| (library.path.as_str(), library))
        .collect()
}

fn identity(
    scope: &str,
    target: Option<&ExecutableIdentity>,
    resolver: Option<&ExecutableIdentity>,
    mappings: &[BuildMapping],
    report: &mut ScopeReport,
) -> FactStatus {
    let (Some(t), Some(r)) = (target, resolver) else {
        let (side, present) = match (target, resolver) {
            (None, Some(r)) => (Side::Target, r),
            (Some(t), None) => (Side::Resolver, t),
            _ => unreachable!("at least one side names every compared path"),
        };
        let role = format!("{:?}", present.role).to_lowercase();
        return match side {
            Side::Target => FactStatus::Mismatch {
                target: "absent".into(),
                resolver: format!("{role} library present"),
            },
            Side::Resolver | Side::Both => FactStatus::Mismatch {
                target: format!("{role} library present"),
                resolver: "absent".into(),
            },
        };
    };
    for (side, identity) in [(Side::Target, t), (Side::Resolver, r)] {
        if let Provenance::Unreadable { reason } = &identity.provenance {
            return FactStatus::Unknown {
                side,
                reason: reason.clone(),
            };
        }
        if identity.disk_differs_from_loaded == Some(true) {
            return FactStatus::Mismatch {
                target: describe(t),
                resolver: describe(r),
            };
        }
    }
    let (Some(td), Some(rd)) = (&t.digest, &r.digest) else {
        return FactStatus::Unknown {
            side: Side::Both,
            reason: "no digest for readable content".into(),
        };
    };
    if td == rd {
        return FactStatus::Match;
    }
    if let Some(mapping) = mappings.iter().find(|m| {
        m.rule.as_str() == RULE && m.scope == scope && m.target == *td && m.resolver == *rd
    }) {
        report.limitations.insert(
            format!("mapping:{scope}"),
            format!(
                "different builds accepted through a measured mapping: {}",
                mapping.measured
            ),
        );
        return FactStatus::Match;
    }
    FactStatus::Mismatch {
        target: describe(t),
        resolver: describe(r),
    }
}

fn describe(identity: &ExecutableIdentity) -> String {
    let digest = identity.digest.as_deref().unwrap_or("-");
    let short = &digest[..digest.len().min(12)];
    match identity.disk_differs_from_loaded {
        Some(true) => format!("{short} (loaded content differs from the file on disk)"),
        _ => match identity.role {
            ExecutableRole::LateLoaded if identity.provenance == Provenance::DiskCandidate => {
                format!("{short} (disk candidate)")
            }
            ExecutableRole::Engine | ExecutableRole::Preloaded | ExecutableRole::LateLoaded => {
                short.to_owned()
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_db::resolver::environment::{
        CatalogFacts, CollationFact, ExecutableSet, ExtensionFact, SettingFact, Verdict,
    };

    fn observed(value: &str) -> Observation {
        Observation::reported(Some(value))
    }

    fn engine(digest: &str) -> ExecutableIdentity {
        ExecutableIdentity {
            role: ExecutableRole::Engine,
            path: "/usr/lib/postgresql/18/bin/postgres".into(),
            digest: Some(digest.into()),
            provenance: Provenance::LoadedContent,
            disk_differs_from_loaded: Some(false),
        }
    }

    fn library(path: &str, digest: &str, role: ExecutableRole) -> ExecutableIdentity {
        ExecutableIdentity {
            role,
            path: path.into(),
            digest: Some(digest.into()),
            provenance: if role == ExecutableRole::LateLoaded {
                Provenance::DiskCandidate
            } else {
                Provenance::LoadedContent
            },
            disk_differs_from_loaded: None,
        }
    }

    /// A side that matches itself on every fact the rule measures.
    fn side(version: &str, digest: &str) -> EnvironmentFacts {
        let observations = [
            ("server_version_num", version),
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
        EnvironmentFacts {
            catalog: CatalogFacts {
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
            },
            executables: ExecutableSet {
                engine: engine(digest),
                libraries: vec![library(
                    "/usr/lib/postgresql/18/lib/hstore.so",
                    "aa",
                    ExecutableRole::LateLoaded,
                )],
            },
        }
    }

    #[test]
    fn identical_sides_verify_and_the_report_names_the_rule() {
        let report = compare(&side("180006", "e1"), &side("180006", "e1"), &[]);
        assert_eq!(report.verdict(), Verdict::Verified, "{report:?}");
        assert_eq!(report.rule.as_str(), RULE);
        assert!(report.limitations.is_empty());
    }

    #[test]
    fn an_equal_version_string_with_different_engine_content_is_a_mismatch() {
        // Same version everywhere; only the executable's content differs.
        let report = compare(&side("180006", "e1"), &side("180006", "e2"), &[]);
        assert_eq!(
            report.verdict(),
            Verdict::Mismatch(vec!["executable:engine".into()])
        );
    }

    #[test]
    fn a_measured_mapping_accepts_different_builds_and_is_recorded_as_a_limitation() {
        let mapping = BuildMapping {
            rule: RuleVersion::new(RULE),
            target: "e1".into(),
            resolver: "e2".into(),
            scope: "engine".into(),
            measured: "vendor build pair measured on 2026-09-18".into(),
        };
        let report = compare(
            &side("180006", "e1"),
            &side("180006", "e2"),
            std::slice::from_ref(&mapping),
        );
        assert_eq!(report.verdict(), Verdict::Verified);
        assert!(report.limitations.contains_key("mapping:engine"));
        // The mapping is specific: another rule, another scope or the reverse
        // pair does not apply.
        for other in [
            BuildMapping {
                rule: RuleVersion::new("pg-analysis-scope-v0"),
                ..mapping.clone()
            },
            BuildMapping {
                scope: "library".into(),
                ..mapping.clone()
            },
            BuildMapping {
                target: "e2".into(),
                resolver: "e1".into(),
                ..mapping.clone()
            },
        ] {
            assert_eq!(
                compare(&side("180006", "e1"), &side("180006", "e2"), &[other]).verdict(),
                Verdict::Mismatch(vec!["executable:engine".into()])
            );
        }
    }

    #[test]
    fn unreadable_content_is_unknown_on_the_side_that_could_not_read_it() {
        let mut resolver = side("180006", "e1");
        resolver.executables.engine.digest = None;
        resolver.executables.engine.provenance = Provenance::Unreadable {
            reason: "exe handle lost".into(),
        };
        let report = compare(&side("180006", "e1"), &resolver, &[]);
        assert_eq!(
            report.facts["executable:engine"],
            FactStatus::Unknown {
                side: Side::Resolver,
                reason: "exe handle lost".into()
            }
        );
        assert!(matches!(report.verdict(), Verdict::Unknown(_)));
    }

    #[test]
    fn a_library_replaced_under_the_running_process_is_a_mismatch() {
        let mut target = side("180006", "e1");
        target.executables.libraries[0].disk_differs_from_loaded = Some(true);
        let report = compare(&target, &side("180006", "e1"), &[]);
        assert!(matches!(
            report.facts["library:/usr/lib/postgresql/18/lib/hstore.so"],
            FactStatus::Mismatch { .. }
        ));
    }

    #[test]
    fn a_library_present_on_one_side_only_is_a_mismatch_in_either_direction() {
        let hook = library(
            "/usr/lib/postgresql/18/lib/hook.so",
            "hh",
            ExecutableRole::Preloaded,
        );
        let mut resolver_only = side("180006", "e1");
        resolver_only.executables.libraries.push(hook.clone());
        assert_eq!(
            compare(&side("180006", "e1"), &resolver_only, &[]).verdict(),
            Verdict::Mismatch(vec!["library:/usr/lib/postgresql/18/lib/hook.so".into()])
        );
        let mut target_only = side("180006", "e1");
        target_only.executables.libraries.push(hook);
        assert_eq!(
            compare(&target_only, &side("180006", "e1"), &[]).verdict(),
            Verdict::Mismatch(vec!["library:/usr/lib/postgresql/18/lib/hook.so".into()])
        );
    }

    #[test]
    fn a_version_the_rule_was_not_measured_on_is_unknown_not_incompatible() {
        for version in ["150013", "190000"] {
            let report = compare(&side(version, "e1"), &side(version, "e1"), &[]);
            assert!(matches!(
                report.facts["server_version_num"],
                FactStatus::Unknown { .. }
            ));
            assert_eq!(report.facts.len(), 1, "nothing else is compared");
        }
        let mut garbage = side("180006", "e1");
        garbage
            .catalog
            .observations
            .insert("server_version_num".into(), observed("eighteen"));
        assert!(matches!(
            compare(&garbage, &side("180006", "e1"), &[]).facts["server_version_num"],
            FactStatus::Unknown {
                side: Side::Target,
                ..
            }
        ));
    }

    #[test]
    fn a_different_patch_level_is_a_mismatch() {
        let report = compare(&side("180006", "e1"), &side("180005", "e1"), &[]);
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
        let mut resolver = side("180006", "e1");
        resolver
            .catalog
            .available_extensions
            .insert("hstore".into(), vec!["1.7".into()]);
        assert_eq!(
            compare(&side("180006", "e1"), &resolver, &[]).verdict(),
            Verdict::Mismatch(vec!["extension:hstore".into()])
        );
        let mut target = side("180006", "e1");
        target.catalog.extensions[0].requires.push("plperl".into());
        assert_eq!(
            compare(&target, &side("180006", "e1"), &[]).verdict(),
            Verdict::Mismatch(vec!["extension:hstore".into()])
        );
    }

    #[test]
    fn collation_provider_locale_and_actual_version_must_agree() {
        let mut resolver = side("180006", "e1");
        resolver.catalog.collations[0].actual_version = observed("2.39");
        assert_eq!(
            compare(&side("180006", "e1"), &resolver, &[]).verdict(),
            Verdict::Mismatch(vec!["collation:default".into()])
        );
        let mut icu = side("180006", "e1");
        icu.catalog.collations[0].provider = "i".into();
        assert_eq!(
            compare(&side("180006", "e1"), &icu, &[]).verdict(),
            Verdict::Mismatch(vec!["collation:default".into()])
        );
    }

    #[test]
    fn a_target_whose_recorded_collation_version_drifted_is_a_limitation_not_a_mismatch() {
        let mut target = side("180006", "e1");
        target.catalog.collations[0].recorded_version = observed("2.36");
        let report = compare(&target, &side("180006", "e1"), &[]);
        assert_eq!(report.verdict(), Verdict::Verified);
        assert!(
            report
                .limitations
                .contains_key("collation:default:recorded-version")
        );
    }

    #[test]
    fn a_setting_the_planning_session_set_on_itself_is_unknown() {
        let mut target = side("180006", "e1");
        target.catalog.settings.get_mut("DateStyle").unwrap().source = "session".into();
        let report = compare(&target, &side("180006", "e1"), &[]);
        assert!(matches!(
            report.facts["setting:DateStyle"],
            FactStatus::Unknown {
                side: Side::Target,
                ..
            }
        ));
    }

    #[test]
    fn a_differing_setting_is_a_mismatch_and_an_optional_one_may_be_absent_on_both_sides() {
        let mut resolver = side("180006", "e1");
        resolver
            .catalog
            .settings
            .get_mut("lc_numeric")
            .unwrap()
            .value = "de_DE".into();
        assert_eq!(
            compare(&side("180006", "e1"), &resolver, &[]).verdict(),
            Verdict::Mismatch(vec!["setting:lc_numeric".into()])
        );
        let mut t = side("180006", "e1");
        let mut r = side("180006", "e1");
        t.catalog
            .settings
            .remove("restrict_nonsystem_relation_kind");
        r.catalog
            .settings
            .remove("restrict_nonsystem_relation_kind");
        let report = compare(&t, &r, &[]);
        assert!(
            !report
                .facts
                .contains_key("setting:restrict_nonsystem_relation_kind")
        );
        assert_eq!(report.verdict(), Verdict::Verified);
        // Present on one side only is a difference, not a known absence.
        assert_eq!(
            compare(&side("180006", "e1"), &r, &[]).verdict(),
            Verdict::Mismatch(vec!["setting:restrict_nonsystem_relation_kind".into()])
        );
    }

    #[test]
    fn a_provider_dependent_fact_null_on_both_sides_matches_but_not_on_one_side_or_unread() {
        // Both libc: locale and rules are NULL on both sides and that is agreement.
        let report = compare(&side("180006", "e1"), &side("180006", "e1"), &[]);
        assert_eq!(report.facts["database_locale"], FactStatus::Match);
        assert_eq!(report.facts["collation:default"], FactStatus::Match);
        // An ICU resolver has a locale where the libc target has none.
        let mut icu = side("180006", "e1");
        icu.catalog
            .observations
            .insert("database_locale".into(), observed("en-US"));
        assert_eq!(
            compare(&side("180006", "e1"), &icu, &[]).facts["database_locale"],
            FactStatus::Mismatch {
                target: "absent".into(),
                resolver: "en-US".into()
            }
        );
        // NULL is not the same as "could not read": an unread fact stays unknown.
        let mut unread = side("180006", "e1");
        unread.catalog.observations.insert(
            "database_icu_rules".into(),
            Observation::Unknown {
                reason: "catalog column unreadable".into(),
            },
        );
        assert!(matches!(
            compare(&unread, &side("180006", "e1"), &[]).facts["database_icu_rules"],
            FactStatus::Unknown {
                side: Side::Target,
                ..
            }
        ));
        let mut unread_collation = side("180006", "e1");
        unread_collation.catalog.collations[0].actual_version = Observation::Unknown {
            reason: "pg_collation_actual_version failed".into(),
        };
        assert!(matches!(
            compare(&unread_collation, &side("180006", "e1"), &[]).facts["collation:default"],
            FactStatus::Unknown {
                side: Side::Target,
                ..
            }
        ));
    }

    #[test]
    fn the_deployers_effective_schema_order_must_agree_for_every_in_scope_schema() {
        // The resolver's principal can see a schema the target's deployer cannot.
        let mut resolver = side("180006", "e1");
        resolver
            .catalog
            .visibility
            .insert("app".into(), observed("{pg_catalog,shadow,app,pg_temp}"));
        assert_eq!(
            compare(&side("180006", "e1"), &resolver, &[]).verdict(),
            Verdict::Mismatch(vec!["visibility:app".into()])
        );
        // A schema the resolver was never asked about is a difference.
        let mut target = side("180006", "e1");
        target
            .catalog
            .visibility
            .insert("audit".into(), observed("{pg_catalog,audit,pg_temp}"));
        assert_eq!(
            compare(&target, &side("180006", "e1"), &[]).verdict(),
            Verdict::Mismatch(vec!["visibility:audit".into()])
        );
        // Unreadable visibility is unknown, never an empty path that matches.
        let mut unread = side("180006", "e1");
        unread.catalog.visibility.insert(
            "app".into(),
            Observation::Unknown {
                reason: "current_schemas failed".into(),
            },
        );
        assert!(matches!(
            compare(&unread, &side("180006", "e1"), &[]).facts["visibility:app"],
            FactStatus::Unknown {
                side: Side::Target,
                ..
            }
        ));
    }

    #[test]
    fn a_setting_hidden_from_the_deployer_names_the_grant_that_would_reveal_it() {
        let mut target = side("180006", "e1");
        target.catalog.settings.remove("session_preload_libraries");
        let report = compare(&target, &side("180006", "e1"), &[]);
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
        let mut plain = side("180006", "e1");
        plain.catalog.settings.remove("lc_numeric");
        assert_eq!(
            compare(&plain, &side("180006", "e1"), &[]).facts["setting:lc_numeric"],
            FactStatus::Unknown {
                side: Side::Target,
                reason: "not reported".into()
            }
        );
    }

    #[test]
    fn a_fact_neither_side_reports_is_unknown_never_an_empty_match() {
        let mut t = side("180006", "e1");
        let mut r = side("180006", "e1");
        t.catalog.settings.remove("TimeZone");
        r.catalog.settings.remove("TimeZone");
        assert_eq!(
            compare(&t, &r, &[]).facts["setting:TimeZone"],
            FactStatus::Unknown {
                side: Side::Both,
                reason: "not reported".into()
            }
        );
        t.catalog
            .observations
            .insert("database_encoding".into(), Observation::NotReported);
        assert!(matches!(
            compare(&t, &r, &[]).facts["database_encoding"],
            FactStatus::Unknown {
                side: Side::Target,
                ..
            }
        ));
    }
}
