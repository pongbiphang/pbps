//! Analysis-scope compatibility report shapes (ADR-0016 cases 5, 14, 16, 21
//! and 23; SPEC §9.3.3).
//!
//! These are engine-neutral shapes only. The queries that fill them and the
//! versioned rule that compares them live in the engine crates, and only the
//! CLI's resolver lifecycle can turn a report into a qualification bound to
//! an actual backend and session. Nothing here is plan evidence on its own.
//!
//! Three verdicts are kept apart on purpose. `Match` is the only good news.
//! `Mismatch` is a finding the deployer can act on (SPEC §9.8: exit 2).
//! `Unknown` is unanswerable — a fact one side could not report or read —
//! and refuses the way a provisioning failure does (exit 1). Folding the last
//! two together would let "could not tell" read as "different", and folding
//! `Unknown` into `Match` is the failure this tool exists to prevent.

use super::Observation;
use std::collections::BTreeMap;

/// Which side of the comparison a fact could not be established on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Side {
    Target,
    Resolver,
    Both,
}

/// One fact's verdict between the deployment target and the resolver.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum FactStatus {
    Match,
    Mismatch { target: String, resolver: String },
    Unknown { side: Side, reason: String },
}

/// The name of the versioned rule a report was produced under.
///
/// Compatibility is never claimed in the abstract: a report says which rule
/// measured which facts, so a later rule with wider coverage is a different
/// verdict and not a silent upgrade of an old one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, schemars::JsonSchema)]
pub struct RuleVersion(String);

impl RuleVersion {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The comparison of one analysis scope: every fact the rule measured and
/// every limit it names about what it did not.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct ScopeReport {
    pub rule: RuleVersion,
    /// Keyed by fact (`server_version_num`, `extension:hstore`,
    /// `setting:DateStyle`, `executable:engine`, ...). Ordered so the report
    /// is stable across runs.
    pub facts: BTreeMap<String, FactStatus>,
    /// What the rule deliberately did not measure, or measured and cannot
    /// treat as a verdict, named explicitly rather than folded into `facts`.
    pub limitations: BTreeMap<String, String>,
}

/// The whole-scope verdict a report supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Verified,
    /// The facts that differ.
    Mismatch(Vec<String>),
    /// The facts that could not be established; wins over any mismatch.
    Unknown(Vec<String>),
}

impl ScopeReport {
    pub fn new(rule: RuleVersion) -> Self {
        Self {
            rule,
            facts: BTreeMap::new(),
            limitations: BTreeMap::new(),
        }
    }

    /// `Unknown` beats `Mismatch` beats `Match`, and an empty report is not
    /// verified: nothing measured is not everything matching. A caller that
    /// wants to publish evidence needs `Verified` and nothing else.
    pub fn verdict(&self) -> Verdict {
        if self.facts.is_empty() {
            return Verdict::Unknown(vec!["scope".into()]);
        }
        let unknown: Vec<String> = self
            .facts
            .iter()
            .filter(|(_, status)| matches!(status, FactStatus::Unknown { .. }))
            .map(|(key, _)| key.clone())
            .collect();
        if !unknown.is_empty() {
            return Verdict::Unknown(unknown);
        }
        let mismatch: Vec<String> = self
            .facts
            .iter()
            .filter(|(_, status)| matches!(status, FactStatus::Mismatch { .. }))
            .map(|(key, _)| key.clone())
            .collect();
        if !mismatch.is_empty() {
            return Verdict::Mismatch(mismatch);
        }
        Verdict::Verified
    }
}

/// An installed extension as one side reports it, with the native libraries
/// its C functions name. The library list is what a resolver has to load to
/// reproduce the extension's binding behaviour; a name and version are not.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct ExtensionFact {
    pub name: String,
    pub version: String,
    pub schema: String,
    /// Extensions this one requires, by name.
    pub requires: Vec<String>,
    /// `probin` values of the extension's C-language functions, as written
    /// (`$libdir/hstore`), before any resolution against the library path.
    pub libraries: Vec<String>,
}

/// One collation, including the database default, with the version the
/// engine recorded when it was created and the one its provider reports now.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct CollationFact {
    /// `default` for the database's own, otherwise the qualified name.
    pub key: String,
    /// `c` (libc), `i` (ICU) or `b` (builtin), as the catalog spells it.
    pub provider: String,
    pub locale: Observation,
    pub rules: Observation,
    pub recorded_version: Observation,
    /// What the provider reports for the locale right now. This, not the
    /// recorded version, is what sorting and comparison actually use.
    pub actual_version: Observation,
}

/// An effective setting with where it came from. `source` is what tells a
/// deployment setting from one the planning session set on itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct SettingFact {
    pub value: String,
    pub source: String,
    pub context: String,
}

/// Why an executable is in scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutableRole {
    /// The engine's main executable.
    Engine,
    /// Mapped into the postmaster or the session's backend already.
    Preloaded,
    /// Required by an in-scope extension or setting and loaded on first use.
    LateLoaded,
}

/// Where an executable's content came from. Only `LoadedContent` proves what
/// a running process executes; a `DiskCandidate` is the file a loader would
/// open next, and `Unreadable` is neither.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Provenance {
    LoadedContent,
    DiskCandidate,
    Unreadable { reason: String },
}

/// One executable's identity: its content, not its reported version.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct ExecutableIdentity {
    pub role: ExecutableRole,
    /// The path as the process's own mount namespace sees it.
    pub path: String,
    /// Lowercase hex SHA-256 of the content named by `provenance`; `None`
    /// exactly when the provenance is `Unreadable`.
    pub digest: Option<String>,
    pub provenance: Provenance,
    /// For loaded content, whether the file at `path` now differs from what
    /// is mapped — a library replaced on disk under a running process.
    pub disk_differs_from_loaded: Option<bool>,
}

/// The executables one side runs or would load for the scope.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct ExecutableSet {
    pub engine: ExecutableIdentity,
    pub libraries: Vec<ExecutableIdentity>,
}

/// What one side's catalog reports for an analysis scope: everything a
/// connection can read with SQL. The engine crates fill this; they know
/// nothing about processes, so the executables are not here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct CatalogFacts {
    /// Version, encoding and locale facts, keyed as the engine's query names
    /// them.
    pub observations: BTreeMap<String, Observation>,
    pub extensions: Vec<ExtensionFact>,
    /// Extension versions this side could install, by name. The resolver's
    /// answer to "can it have what the target has".
    pub available_extensions: BTreeMap<String, Vec<String>>,
    pub collations: Vec<CollationFact>,
    pub settings: BTreeMap<String, SettingFact>,
    /// The engine's effective schema search order for each in-scope schema,
    /// as the current principal — the visibility filter a deployer's grants
    /// impose, computed by the engine rather than re-derived from ACLs.
    /// Keyed by the schema the write path starts with.
    pub visibility: BTreeMap<String, Observation>,
}

/// Everything one side reports for an analysis scope: its catalog facts and
/// the executables its processes actually run. Serializable so the same
/// facts can be re-read as non-snapshot inputs by later capture and apply
/// checks, which is where a fact that changed in between is caught.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct EnvironmentFacts {
    pub catalog: CatalogFacts,
    pub executables: ExecutableSet,
}

/// The locale provider a database was created with, as the catalog spells
/// it. Anything else is a provider this rule was not measured on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LocaleProvider {
    Libc,
    Icu,
    Builtin,
}

/// How the resolver's scratch database must be created so that it sorts,
/// compares and encodes the way the target does. Derived from the target's
/// facts and never defaulted: a locale the target did not report is a
/// database the resolver cannot reproduce.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct DatabaseRecipe {
    pub encoding: String,
    pub provider: LocaleProvider,
    pub collate: String,
    pub ctype: String,
    /// The ICU or builtin locale; absent for libc, whose locale is `collate`
    /// and `ctype`.
    pub locale: Option<String>,
    pub icu_rules: Option<String>,
}

/// What kept a recipe from being derived: the fact the target did not report.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the target did not report {0}, so an equivalent scratch database cannot be created")]
pub struct RecipeUnavailable(pub &'static str);

impl DatabaseRecipe {
    /// A placeholder recipe for an engine whose scratch database creation
    /// does not consume one yet (SQL Server; reproducing its collation is
    /// #611). It is never rendered into a PostgreSQL `CREATE DATABASE`.
    pub fn neutral() -> Self {
        Self {
            encoding: "UTF8".into(),
            provider: LocaleProvider::Libc,
            collate: "C".into(),
            ctype: "C".into(),
            locale: None,
            icu_rules: None,
        }
    }

    pub fn from_catalog(catalog: &CatalogFacts) -> Result<Self, RecipeUnavailable> {
        let observed = |key: &'static str| -> Result<String, RecipeUnavailable> {
            catalog
                .observations
                .get(key)
                .and_then(Observation::value)
                .map(str::to_owned)
                .ok_or(RecipeUnavailable(key))
        };
        let optional = |key: &'static str| -> Result<Option<String>, RecipeUnavailable> {
            match catalog.observations.get(key) {
                Some(Observation::Observed { value }) => Ok(Some(value.clone())),
                Some(Observation::NotReported) => Ok(None),
                Some(Observation::Unknown { .. }) | None => Err(RecipeUnavailable(key)),
            }
        };
        let provider = match observed("database_locale_provider")?.as_str() {
            "c" => LocaleProvider::Libc,
            "i" => LocaleProvider::Icu,
            "b" => LocaleProvider::Builtin,
            _ => return Err(RecipeUnavailable("a known locale provider")),
        };
        let locale = optional("database_locale")?;
        if provider != LocaleProvider::Libc && locale.is_none() {
            return Err(RecipeUnavailable("database_locale"));
        }
        Ok(Self {
            encoding: observed("database_encoding")?,
            provider,
            collate: observed("database_collate")?,
            ctype: observed("database_ctype")?,
            locale,
            icu_rules: optional("database_icu_rules")?,
        })
    }
}

/// A separately measured equivalence between two different builds for one
/// scope. Data, versioned with the rule that trusts it: the mechanism ships
/// with an empty table, and a mapping is never inferred from version strings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct BuildMapping {
    pub rule: RuleVersion,
    /// Hex SHA-256 of the target-side content.
    pub target: String,
    /// Hex SHA-256 of the resolver-side content.
    pub resolver: String,
    /// The executable the mapping is about (`engine` or a library path).
    pub scope: String,
    /// Where and how the equivalence was measured.
    pub measured: String,
}

/// The principal a deployment runs as, read from the planning connection,
/// which is the login apply will use. Never the scratch administrator and
/// never a discovery session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct DeploymentPrincipal {
    /// `session_user`: who authenticated.
    pub login: String,
    /// `current_user`: who statements run as, after any role switch.
    pub effective: String,
    pub superuser: bool,
}

/// A digest over the canonical deployment authorization context, so a
/// changed grant or a non-equivalent apply session is a different value.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct AuthorizationFingerprint {
    pub rule: RuleVersion,
    /// Lowercase hex SHA-256.
    pub digest: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(facts: &[(&str, FactStatus)]) -> ScopeReport {
        let mut report = ScopeReport::new(RuleVersion::new("test-v1"));
        for (key, status) in facts {
            report.facts.insert((*key).into(), status.clone());
        }
        report
    }

    #[test]
    fn a_report_with_nothing_measured_is_not_verified() {
        assert_eq!(
            report(&[]).verdict(),
            Verdict::Unknown(vec!["scope".into()])
        );
    }

    #[test]
    fn one_unknown_fact_makes_the_scope_unanswerable_however_many_match() {
        let verdict = report(&[
            ("a", FactStatus::Match),
            ("b", FactStatus::Match),
            (
                "c",
                FactStatus::Mismatch {
                    target: "1".into(),
                    resolver: "2".into(),
                },
            ),
            (
                "d",
                FactStatus::Unknown {
                    side: Side::Target,
                    reason: "not reported".into(),
                },
            ),
        ])
        .verdict();
        // Unknown wins over the mismatch: the caller must not learn "different"
        // about a scope it could not fully read.
        assert_eq!(verdict, Verdict::Unknown(vec!["d".into()]));
    }

    #[test]
    fn a_mismatch_is_a_finding_and_only_all_matches_verify() {
        assert_eq!(
            report(&[
                ("a", FactStatus::Match),
                (
                    "b",
                    FactStatus::Mismatch {
                        target: "x".into(),
                        resolver: "y".into(),
                    },
                ),
            ])
            .verdict(),
            Verdict::Mismatch(vec!["b".into()])
        );
        assert_eq!(
            report(&[("a", FactStatus::Match), ("b", FactStatus::Match)]).verdict(),
            Verdict::Verified
        );
    }

    #[test]
    fn an_unknown_fact_never_serializes_as_a_match() {
        let json = serde_json::to_value(FactStatus::Unknown {
            side: Side::Both,
            reason: "unreadable".into(),
        })
        .unwrap();
        assert_eq!(json["status"], "unknown");
        assert_eq!(json["side"], "both");
        assert_eq!(
            serde_json::to_value(FactStatus::Match).unwrap()["status"],
            "match"
        );
    }

    #[test]
    fn an_unreadable_executable_carries_no_digest() {
        let identity = ExecutableIdentity {
            role: ExecutableRole::Engine,
            path: "/usr/lib/postgresql/18/bin/postgres".into(),
            digest: None,
            provenance: Provenance::Unreadable {
                reason: "map_files refused".into(),
            },
            disk_differs_from_loaded: None,
        };
        let json = serde_json::to_value(&identity).unwrap();
        assert!(json["digest"].is_null());
        assert_eq!(json["provenance"]["status"], "unreadable");
    }

    fn catalog_with(observations: &[(&str, Observation)]) -> CatalogFacts {
        CatalogFacts {
            observations: observations
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
            extensions: vec![],
            available_extensions: BTreeMap::new(),
            collations: vec![],
            settings: BTreeMap::new(),
            visibility: BTreeMap::new(),
        }
    }

    #[test]
    fn a_recipe_is_derived_from_reported_facts_and_never_defaulted() {
        let observed = |v: &str| Observation::reported(Some(v));
        let libc = catalog_with(&[
            ("database_encoding", observed("UTF8")),
            ("database_locale_provider", observed("c")),
            ("database_collate", observed("en_US.utf8")),
            ("database_ctype", observed("en_US.utf8")),
            ("database_locale", Observation::NotReported),
            ("database_icu_rules", Observation::NotReported),
        ]);
        let recipe = DatabaseRecipe::from_catalog(&libc).unwrap();
        assert_eq!(recipe.provider, LocaleProvider::Libc);
        assert_eq!(recipe.locale, None);
        let mut icu = libc.clone();
        icu.observations
            .insert("database_locale_provider".into(), observed("i"));
        // An ICU database without a reported ICU locale cannot be reproduced.
        assert_eq!(
            DatabaseRecipe::from_catalog(&icu),
            Err(RecipeUnavailable("database_locale"))
        );
        icu.observations
            .insert("database_locale".into(), observed("en-US"));
        assert_eq!(
            DatabaseRecipe::from_catalog(&icu)
                .unwrap()
                .locale
                .as_deref(),
            Some("en-US")
        );
        // An encoding that could not be read is not UTF8 by assumption.
        let mut unread = libc.clone();
        unread.observations.insert(
            "database_encoding".into(),
            Observation::Unknown {
                reason: "hidden".into(),
            },
        );
        assert_eq!(
            DatabaseRecipe::from_catalog(&unread),
            Err(RecipeUnavailable("database_encoding"))
        );
        let mut odd = libc;
        odd.observations
            .insert("database_locale_provider".into(), observed("x"));
        assert!(DatabaseRecipe::from_catalog(&odd).is_err());
    }
}
