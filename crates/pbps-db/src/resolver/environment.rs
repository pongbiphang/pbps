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
    /// What a SQL Server scratch database is created with. The fields above
    /// are PostgreSQL's and unused for it; this is absent for PostgreSQL.
    pub sql_server: Option<SqlServerDatabase>,
}

/// How a SQL Server scratch database must be created so that it compares,
/// parses and defaults the way the target's does: its collation, its
/// compatibility level, its containment, and the database-level ANSI options
/// that are a session's fallback and are persisted into what it creates.
/// Derived from the target's facts and never defaulted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct SqlServerDatabase {
    pub collation: String,
    pub compatibility_level: String,
    pub containment: String,
    /// `ALTER DATABASE ... SET <option> ON|OFF`, by option name, `true` for on.
    pub options: BTreeMap<String, bool>,
}

/// The `sys.databases` options a SQL Server recipe carries, as the fact key
/// the engine crate reads them under and the `ALTER DATABASE SET` name.
pub const SQL_SERVER_DATABASE_OPTIONS: &[(&str, &str)] = &[
    ("database_ansi_null_default", "ANSI_NULL_DEFAULT"),
    ("database_ansi_nulls", "ANSI_NULLS"),
    ("database_ansi_padding", "ANSI_PADDING"),
    ("database_ansi_warnings", "ANSI_WARNINGS"),
    ("database_arithabort", "ARITHABORT"),
    (
        "database_concat_null_yields_null",
        "CONCAT_NULL_YIELDS_NULL",
    ),
    ("database_numeric_roundabort", "NUMERIC_ROUNDABORT"),
    ("database_quoted_identifier", "QUOTED_IDENTIFIER"),
];

/// What kept a recipe from being derived: the fact the target did not report.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the target did not report {0}, so an equivalent scratch database cannot be created")]
pub struct RecipeUnavailable(pub &'static str);

impl DatabaseRecipe {
    /// A recipe that asks for nothing: the PostgreSQL fields a SQL Server
    /// recipe leaves unused, and the database a SQL Server scratch gets when
    /// no target has been read for one. It is never rendered into a
    /// PostgreSQL `CREATE DATABASE`.
    pub fn neutral() -> Self {
        Self {
            encoding: "UTF8".into(),
            provider: LocaleProvider::Libc,
            collate: "C".into(),
            ctype: "C".into(),
            locale: None,
            icu_rules: None,
            sql_server: None,
        }
    }

    /// The recipe for a SQL Server scratch database, from the target's
    /// catalog facts. A fact the target did not report is a database the
    /// resolver cannot reproduce.
    pub fn from_sql_server_catalog(catalog: &CatalogFacts) -> Result<Self, RecipeUnavailable> {
        let observed = |key: &'static str| -> Result<String, RecipeUnavailable> {
            catalog
                .observations
                .get(key)
                .and_then(Observation::value)
                .map(str::to_owned)
                .ok_or(RecipeUnavailable(key))
        };
        let mut options = BTreeMap::new();
        for (key, option) in SQL_SERVER_DATABASE_OPTIONS {
            let on = match observed(key)?.as_str() {
                "1" => true,
                "0" => false,
                _ => return Err(RecipeUnavailable(key)),
            };
            options.insert((*option).to_owned(), on);
        }
        Ok(Self {
            sql_server: Some(SqlServerDatabase {
                collation: observed("database_collation")?,
                compatibility_level: observed("database_compatibility_level")?,
                containment: observed("database_containment")?,
                options,
            }),
            ..Self::neutral()
        })
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
            sql_server: None,
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

/// The executable half of an analysis-scope rule, shared by every engine: the
/// process facts are the same shape whichever engine runs, and only the rule
/// name a build mapping must carry differs.
///
/// Content identity, not version. Equal digests match; different digests
/// match only through a mapping measured for exactly this pair and scope;
/// anything unreadable is unknown. A library present on one side only is a
/// difference in both directions — a resolver-only preload changes binding
/// as surely as a missing one.
pub fn compare_executables(
    rule: &str,
    target: &EnvironmentFacts,
    resolver: &EnvironmentFacts,
    mappings: &[BuildMapping],
    report: &mut ScopeReport,
) {
    let engine = identity(
        rule,
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
            rule,
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
    rule: &str,
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
        // A disk digest cannot establish what an already mapped library runs,
        // even when it matches a peer or a measured build mapping (#698).
        // A required library not yet loaded still needs its disk candidate.
        if identity.role == ExecutableRole::Preloaded
            && identity.provenance == Provenance::DiskCandidate
        {
            return FactStatus::Unknown {
                side,
                reason: "mapped content not readable, disk candidate only".into(),
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
        m.rule.as_str() == rule && m.scope == scope && m.target == *td && m.resolver == *rd
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

/// The rendering of one write path's effective schema order, shared by the
/// reader and the predictor: a JSON array of the schema names in order. Both
/// sides produce it from the same elements, so a name containing a comma or a
/// quote, or spelling the array NULL sentinel, needs no emulation of the
/// engine's array text, whose quoting rules two renderers can disagree on
/// (finding on #688).
pub fn render_visibility(elements: &[String]) -> String {
    serde_json::to_string(elements).expect("a list of strings serializes")
}

/// The elements of a list-valued GUC as PostgreSQL's own splitter reads them
/// (`SplitIdentifierString` / `SplitDirectoriesString`, measured on 18):
/// separated by commas and trimmed, a double-quoted element keeping its
/// commas and spaces with `""` standing for one quote. This is the form the
/// engine itself renders — `SET search_path = '$user', public` reads back as
/// `"$user", public` — so a plain split on commas, or a whole value replayed
/// as one string literal, turns one list into another (findings on #688).
/// `None` is the syntax the engine rejects: an unclosed quote, an empty
/// unquoted element, text after a closing quote.
pub fn guc_list(value: &str) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut rest = value.trim_start();
    if rest.is_empty() {
        return Some(names);
    }
    loop {
        let name;
        if let Some(quoted) = rest.strip_prefix('"') {
            let mut text = String::new();
            let mut after = quoted;
            loop {
                let end = after.find('"')?;
                text.push_str(&after[..end]);
                after = &after[end + 1..];
                if let Some(more) = after.strip_prefix('"') {
                    text.push('"');
                    after = more;
                } else {
                    break;
                }
            }
            name = text;
            rest = after;
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            name = rest[..end].trim_end().to_owned();
            if name.is_empty() {
                return None;
            }
            rest = &rest[end..];
        }
        names.push(name);
        rest = rest.trim_start();
        match rest.strip_prefix(',') {
            Some(more) => rest = more.trim_start(),
            None if rest.is_empty() => return Some(names),
            None => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guc_list_is_split_as_the_engine_splits_it() {
        let names = |list: &[&str]| list.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        // Measured on 18: how the engine renders
        // `SET session_preload_libraries = 'foo,bar', baz, 'q"x', ' sp ace '`
        // and the names it then loads, in order.
        assert_eq!(
            guc_list(r#""foo,bar", baz, "q""x", " sp ace ""#),
            Some(names(&["foo,bar", "baz", "q\"x", " sp ace "]))
        );
        assert_eq!(
            guc_list(r#""$user", public, "odd name""#),
            Some(names(&["$user", "public", "odd name"]))
        );
        // Unquoted elements lose their surrounding whitespace; an empty
        // value is an empty list, and a quoted empty element is one name.
        assert_eq!(
            guc_list("  auto_explain ,$libdir/hstore  "),
            Some(names(&["auto_explain", "$libdir/hstore"]))
        );
        assert_eq!(guc_list(""), Some(Vec::new()));
        assert_eq!(guc_list("   "), Some(Vec::new()));
        assert_eq!(guc_list(r#""""#), Some(names(&[""])));
        // What the engine rejects as list syntax is not a shorter list.
        for broken in [r#""foo"#, "a,,b", "a,", r#""a"b"#] {
            assert_eq!(guc_list(broken), None, "{broken:?}");
        }
    }

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
    fn a_visibility_rendering_keeps_every_name_whole_and_in_order() {
        let names = ["pg_catalog", "x,pg_temp_3,y", "null", "odd\"name"].map(str::to_owned);
        assert_eq!(
            render_visibility(&names),
            r#"["pg_catalog","x,pg_temp_3,y","null","odd\"name"]"#
        );
        assert_eq!(render_visibility(&[]), "[]");
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

    fn executable_facts(role: ExecutableRole, provenance: Provenance) -> EnvironmentFacts {
        EnvironmentFacts {
            catalog: catalog_with(&[]),
            executables: ExecutableSet {
                engine: ExecutableIdentity {
                    role: ExecutableRole::Engine,
                    path: "/engine".into(),
                    digest: Some("engine-content".into()),
                    provenance: Provenance::LoadedContent,
                    disk_differs_from_loaded: Some(false),
                },
                libraries: vec![ExecutableIdentity {
                    role,
                    path: "/library.so".into(),
                    digest: Some("library-content".into()),
                    provenance,
                    disk_differs_from_loaded: Some(false),
                }],
            },
        }
    }

    #[test]
    fn disk_candidates_cannot_prove_mapped_content_by_digest_or_build_mapping() {
        for candidate_side in [Side::Target, Side::Resolver] {
            for use_mapping in [false, true] {
                let mut target =
                    executable_facts(ExecutableRole::Preloaded, Provenance::LoadedContent);
                let mut resolver = target.clone();
                let candidate = match candidate_side {
                    Side::Target => &mut target,
                    Side::Resolver => &mut resolver,
                    Side::Both => unreachable!(),
                };
                candidate.executables.libraries[0].provenance = Provenance::DiskCandidate;
                let mappings = if use_mapping {
                    resolver.executables.libraries[0].digest = Some("other-content".into());
                    vec![BuildMapping {
                        rule: RuleVersion::new("test-v1"),
                        scope: "/library.so".into(),
                        target: "library-content".into(),
                        resolver: "other-content".into(),
                        measured: "known loaded build pair".into(),
                    }]
                } else {
                    vec![]
                };
                let mut report = ScopeReport::new(RuleVersion::new("test-v1"));
                compare_executables("test-v1", &target, &resolver, &mappings, &mut report);
                assert_eq!(
                    report.facts["library:/library.so"],
                    FactStatus::Unknown {
                        side: candidate_side,
                        reason: "mapped content not readable, disk candidate only".into(),
                    },
                    "candidate side {candidate_side:?}, build mapping {use_mapping}"
                );
                assert_eq!(
                    report.verdict(),
                    Verdict::Unknown(vec!["library:/library.so".into()])
                );
                assert!(
                    report.limitations.is_empty(),
                    "an unreadable mapping was not applied"
                );
            }
        }
    }

    #[test]
    fn readable_loaded_and_required_unloaded_content_can_match_in_either_direction() {
        for target_loaded in [false, true] {
            for resolver_loaded in [false, true] {
                let side = |loaded| {
                    if loaded {
                        executable_facts(ExecutableRole::Preloaded, Provenance::LoadedContent)
                    } else {
                        executable_facts(ExecutableRole::LateLoaded, Provenance::DiskCandidate)
                    }
                };
                let mut report = ScopeReport::new(RuleVersion::new("test-v1"));
                compare_executables(
                    "test-v1",
                    &side(target_loaded),
                    &side(resolver_loaded),
                    &[],
                    &mut report,
                );
                assert_eq!(report.verdict(), Verdict::Verified, "{report:?}");
            }
        }
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
