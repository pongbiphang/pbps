//! Resolver selection is configuration, not a connection or qualification.
//!
//! Keeping credentials as variable names lets even an unavailable resolver be
//! selected without making ordinary planning depend on its infrastructure.

use crate::{Config, ConfigError};
use serde::Deserialize as _;

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PullPolicy {
    /// Use an image already present on the runner.
    #[default]
    Never,
    /// Permit trusted acquisition only when the selected image is absent.
    IfMissing,
}

/// Only a configured source can be selected; an image suggestion is not an
/// acquisition instruction. Neither variant can represent two backends.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolverProfile {
    Docker {
        /// Explicitly trusted image reference, including an internal registry.
        /// Runtime acquisition must still pin and qualify the actual content.
        #[serde(deserialize_with = "image_reference")]
        #[schemars(regex(
            pattern = "^[A-Za-z0-9_.:-]+(/[A-Za-z0-9_.:-]+)*(@sha256:[A-Fa-f0-9]+)?$"
        ))]
        image: String,
        #[serde(default)]
        pull: PullPolicy,
    },
    Server {
        /// Separate scratch credentials, read only when resolution is needed.
        #[serde(deserialize_with = "variable_name")]
        #[schemars(regex(pattern = "^[A-Za-z_][A-Za-z0-9_]*$"))]
        url_env: String,
        /// The state the scratch database is put into at the start of each
        /// run and back into at release, where it differs from what
        /// `CREATE DATABASE ... TEMPLATE template0` makes (#1708). Omitted, a
        /// run uses that.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        standard: Option<ScratchStandard>,
        /// A SQL file, relative to the project root, that creates on scratch
        /// the objects outside the managed set that managed objects reference
        /// (#1673). It runs on scratch only, before any managed object, and
        /// each object it creates is compared with the target.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "baseline_path"
        )]
        #[schemars(length(min = 1))]
        baseline: Option<std::path::PathBuf>,
    },
}

/// A scratch database's declared standard state. Only what the scratch
/// account can change and `DROP OWNED` does not undo is declarable: the
/// database's settings, comment and connection limit, and `public`'s
/// comment and grants. The database's own ACL is not: the run keeps it as
/// it found it (DEC-1708.1).
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ScratchStandard {
    /// `ALTER DATABASE <scratch> SET`: operational settings such as
    /// timeouts. The settings that decide the resolver's answer come from the
    /// target, as the deployer's session settings, and override these.
    #[serde(
        default,
        skip_serializing_if = "std::collections::BTreeMap::is_empty",
        deserialize_with = "settings"
    )]
    pub settings: std::collections::BTreeMap<String, SettingValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// `-1` for no limit. Never `0`, which would lock the scratch account out
    /// of its own database.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "connection_limit"
    )]
    #[schemars(schema_with = "connection_limit_schema")]
    pub connection_limit: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public: Option<PublicStandard>,
}

/// initdb's `public` schema in the scratch database.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct PublicStandard {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Granted by `public`'s owner, beyond initdb's `USAGE` to `PUBLIC`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<PublicGrant>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct PublicGrant {
    #[serde(deserialize_with = "role_name")]
    #[schemars(length(min = 1))]
    pub to: String,
    #[serde(deserialize_with = "privileges")]
    #[schemars(length(min = 1))]
    pub privileges: Vec<SchemaPrivilege>,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "UPPERCASE")]
pub enum SchemaPrivilege {
    Usage,
    Create,
}

impl SchemaPrivilege {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "USAGE",
            Self::Create => "CREATE",
        }
    }
}

/// A setting's value, written as YAML writes it: a string, a number or a
/// boolean, kept as the text the engine is given.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct SettingValue(pub String);

impl<'de> serde::Deserialize<'de> for SettingValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Scalar {
            Bool(bool),
            Int(i64),
            Real(f64),
            Text(String),
        }
        Ok(Self(
            match Scalar::deserialize(d).map_err(|_| {
                serde::de::Error::custom(
                    "a setting's value must be a string, a number or a boolean",
                )
            })? {
                Scalar::Bool(value) => value.to_string(),
                Scalar::Int(value) => value.to_string(),
                Scalar::Real(value) => value.to_string(),
                Scalar::Text(value) => value,
            },
        ))
    }
}

impl schemars::JsonSchema for SettingValue {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SettingValue".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": ["string", "number", "boolean"] })
    }
}

fn connection_limit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i32>, D::Error> {
    let value = i32::deserialize(d)?;
    if value == -1 || value >= 1 {
        Ok(Some(value))
    } else {
        Err(serde::de::Error::custom(
            "a scratch connection_limit is -1 or at least 1; 0 would lock the scratch account out",
        ))
    }
}

/// The domain [`connection_limit`] admits, so an editor refuses what the
/// loader refuses: `0`, below `-1`, and `null`.
fn connection_limit_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "format": "int32",
        "anyOf": [{ "const": -1 }, { "minimum": 1, "maximum": i32::MAX }]
    })
}

/// A grant of nothing is no grant; the schema says `minItems: 1`, and the
/// loader agrees, so an echoed profile never fails its own schema.
fn privileges<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<SchemaPrivilege>, D::Error> {
    let value = Vec::<SchemaPrivilege>::deserialize(d)?;
    if value.is_empty() {
        Err(serde::de::Error::custom(
            "a grant names at least one privilege",
        ))
    } else {
        Ok(value)
    }
}

/// The engine matches setting names case-insensitively, so `TimeZone` and
/// `timezone` are one setting: declared twice, one value would silently win.
fn settings<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<std::collections::BTreeMap<String, SettingValue>, D::Error> {
    let settings = std::collections::BTreeMap::<String, SettingValue>::deserialize(d)?;
    let mut seen = std::collections::BTreeSet::new();
    for name in settings.keys() {
        if !seen.insert(name.to_lowercase()) {
            return Err(serde::de::Error::custom(format!(
                "the setting {name} is declared twice, in two spellings of one name"
            )));
        }
    }
    Ok(settings)
}

fn baseline_path<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<std::path::PathBuf>, D::Error> {
    // `null` is what the published schema allows for an omitted baseline.
    match Option::<String>::deserialize(d)? {
        Some(value) if value.is_empty() => {
            Err(serde::de::Error::custom("a baseline names its SQL file"))
        }
        value => Ok(value.map(Into::into)),
    }
}

fn role_name<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    if value.is_empty() {
        Err(serde::de::Error::custom("a grant names a role"))
    } else {
        Ok(value)
    }
}

fn image_reference<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    let (name, digest) = value
        .split_once('@')
        .map_or((value.as_str(), None), |(name, digest)| {
            (name, Some(digest))
        });
    let valid_name = name.split('/').all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
    });
    let valid_digest = digest.is_none_or(|digest| {
        digest
            .strip_prefix("sha256:")
            .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
    });
    if valid_name && valid_digest {
        Ok(value)
    } else {
        // Do not echo something that might have been an inline connection URL.
        Err(serde::de::Error::custom(
            "resolver image must be a nonempty image reference, without a URL scheme, whitespace or credentials",
        ))
    }
}

pub(super) fn valid_profile_name(value: &str) -> bool {
    value
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn variable_name<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    let first = value
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_');
    if first
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(
            "resolver url_env must name an environment variable, never a connection string",
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SelectionSource {
    Cli,
    Environment,
    Project,
}

/// There is deliberately no acquired/verified variant in this delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SelectionStatus {
    NotAcquired,
}

/// Advisory command data, never saved-plan evidence.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct ResolverSelection {
    pub name: String,
    pub source: SelectionSource,
    pub profile: ResolverProfile,
    pub status: SelectionStatus,
}

impl Config {
    /// Pure precedence lookup. Call only for target planning; offline commands
    /// do not need to resolve a default, much less read its credentials.
    pub fn select_resolver(
        &self,
        cli: Option<&str>,
        environment: Option<&str>,
    ) -> Result<Option<ResolverSelection>, ConfigError> {
        let from_environment = environment
            .and_then(|name| self.environments.get(name))
            .and_then(|env| env.resolve_with.as_deref());
        let selected = cli
            .map(|name| (name, SelectionSource::Cli))
            .or_else(|| from_environment.map(|name| (name, SelectionSource::Environment)))
            .or_else(|| {
                self.resolve_with
                    .as_deref()
                    .map(|name| (name, SelectionSource::Project))
            });
        let Some((name, source)) = selected else {
            return Ok(None);
        };
        if !valid_profile_name(name) {
            return Err(ConfigError::InvalidResolverName);
        }
        let profile = self
            .resolvers
            .get(name)
            .ok_or_else(|| ConfigError::UnknownResolver {
                name: name.to_owned(),
                available: if self.resolvers.is_empty() {
                    "none".to_owned()
                } else {
                    self.resolvers
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                },
            })?;
        Ok(Some(ResolverSelection {
            name: name.to_owned(),
            source,
            profile: profile.clone(),
            status: SelectionStatus::NotAcquired,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn config(extra: &str) -> Config {
        Config::parse(&format!(
            "dialect: postgres\nresolvers:\n  local:\n    kind: docker\n    image: postgres:18\n  internal:\n    kind: docker\n    image: registry.example:5000/team/pg@sha256:abc\n    pull: if_missing\n  scratch:\n    kind: server\n    url_env: PBPS_UNSET_RESOLVER_606\n{extra}"
        ), Path::new("pbps.yml")).unwrap()
    }

    #[test]
    fn explicit_selection_overrides_environment_then_project_without_credentials() {
        let c = config(
            "resolve_with: local\nenvironments:\n  prod:\n    url_env: TARGET\n    resolve_with: scratch\n  stage:\n    url_env: STAGE\n",
        );
        for (cli, env, name, source) in [
            (
                Some("internal"),
                Some("prod"),
                "internal",
                SelectionSource::Cli,
            ),
            (None, Some("prod"), "scratch", SelectionSource::Environment),
            (None, Some("stage"), "local", SelectionSource::Project),
            (None, None, "local", SelectionSource::Project),
        ] {
            let selected = c.select_resolver(cli, env).unwrap().unwrap();
            assert_eq!((selected.name.as_str(), selected.source), (name, source));
            assert_eq!(selected.status, SelectionStatus::NotAcquired);
        }
        assert_eq!(
            c.resolvers["local"],
            ResolverProfile::Docker {
                image: "postgres:18".into(),
                pull: PullPolicy::Never,
            }
        );
        assert!(config("").select_resolver(None, None).unwrap().is_none());
        assert!(c.select_resolver(Some("typo"), Some("prod")).is_err());
        let unknown_default = config("resolve_with: not_here\n");
        assert!(unknown_default.select_resolver(None, None).is_err());
        assert!(unknown_default.select_resolver(Some("local"), None).is_ok());
    }

    #[test]
    fn malformed_profiles_cannot_be_silently_interpreted_as_another_backend() {
        for profile in [
            "{}",
            "{kind: docker, image: postgres:18, url_env: SECRET}",
            "{kind: server, url_env: SECRET, image: postgres:18}",
            "{kind: docker, image: postgres:18, pull: sometimes}",
            "{kind: docker, image: postgres:18, trusted: true}",
            "{kind: server, url: 'postgres://user:secret@host/db'}",
            "{kind: server, url_env: ''}",
            "{kind: server, url_env: 'password=secret'}",
            "{kind: docker, image: ''}",
            "{kind: docker, image: 'postgres:18 --privileged'}",
            "{kind: unknown}",
            "{kind: docker, image: postgres:18, standard: {comment: c}}",
            // Docker stays the measured profile until #1674.
            "{kind: docker, image: postgres:18, baseline: baseline.sql}",
            "{kind: server, url_env: S, baseline: ''}",
        ] {
            assert!(
                Config::parse(
                    &format!("dialect: mssql\nresolvers:\n  bad: {profile}\n"),
                    Path::new("pbps.yml")
                )
                .is_err(),
                "{profile}"
            );
        }
    }

    #[test]
    fn a_server_entry_names_its_baseline_file_relative_to_the_project() {
        let config = Config::parse(
            "dialect: postgres\nresolvers:\n  s: {kind: server, url_env: S, baseline: db/baseline.sql}\n",
            Path::new("pbps.yml"),
        )
        .unwrap();
        let ResolverProfile::Server { baseline, .. } = &config.resolvers["s"] else {
            panic!("a server profile");
        };
        assert_eq!(baseline.as_deref(), Some(Path::new("db/baseline.sql")));
        // Negative: omitted or null, there is none.
        for entry in [
            "{kind: server, url_env: S}",
            "{kind: server, url_env: S, baseline: null}",
        ] {
            let config = Config::parse(
                &format!("dialect: postgres\nresolvers:\n  s: {entry}\n"),
                Path::new("pbps.yml"),
            )
            .unwrap();
            assert!(
                matches!(
                    &config.resolvers["s"],
                    ResolverProfile::Server { baseline: None, .. }
                ),
                "{entry}"
            );
        }
    }

    #[test]
    fn a_server_entry_declares_its_scratch_standard_with_scalar_settings() {
        let parsed = Config::parse(
            "dialect: postgres\nresolvers:\n  scratch:\n    kind: server\n    url_env: S\n    standard:\n      settings:\n        statement_timeout: 5min\n        work_mem: 64\n        jit: false\n        random_page_cost: 1.5\n      comment: pbps scratch\n      connection_limit: 10\n      public:\n        comment: ours\n        grants:\n          - {to: ci_reader, privileges: [USAGE, CREATE]}\n",
            Path::new("pbps.yml"),
        )
        .unwrap();
        let ResolverProfile::Server {
            standard: Some(standard),
            ..
        } = &parsed.resolvers["scratch"]
        else {
            panic!("a server entry with a standard");
        };
        let settings: Vec<(&str, &str)> = standard
            .settings
            .iter()
            .map(|(name, value)| (name.as_str(), value.0.as_str()))
            .collect();
        assert_eq!(
            settings,
            [
                ("jit", "false"),
                ("random_page_cost", "1.5"),
                ("statement_timeout", "5min"),
                ("work_mem", "64"),
            ]
        );
        assert_eq!(standard.connection_limit, Some(10));
        let public = standard.public.as_ref().unwrap();
        assert_eq!(
            public.grants[0].privileges,
            [SchemaPrivilege::Usage, SchemaPrivilege::Create]
        );
        // Omitted: no standard of its own, so the built-in one.
        assert!(matches!(
            config("").resolvers["scratch"],
            ResolverProfile::Server { standard: None, .. }
        ));
    }

    #[test]
    fn a_scratch_standard_refuses_what_it_cannot_mean() {
        for standard in [
            // An unknown key is refused, never ignored (SPEC 4.3).
            "{acl: []}",
            "{public: {owner: me}}",
            "{public: {grants: [{to: r, privileges: [USAGE], option: true}]}}",
            // 0 locks the scratch account out of its own database.
            "{connection_limit: 0}",
            "{connection_limit: -2}",
            "{public: {grants: [{to: '', privileges: [USAGE]}]}}",
            "{public: {grants: [{to: r, privileges: [SELECT]}]}}",
            "{public: {grants: [{to: r, privileges: []}]}}",
            "{connection_limit: null}",
            "{settings: {work_mem: [1, 2]}}",
            // One setting in two spellings: the engine reads them as one.
            "{settings: {TimeZone: UTC, timezone: GMT}}",
        ] {
            assert!(
                Config::parse(
                    &format!(
                        "dialect: postgres\nresolvers:\n  s:\n    kind: server\n    url_env: S\n    standard: {standard}\n"
                    ),
                    Path::new("pbps.yml")
                )
                .is_err(),
                "{standard}"
            );
        }
        // Negative: the limits a standard may hold.
        for limit in ["-1", "1"] {
            assert!(
                Config::parse(
                    &format!(
                        "dialect: postgres\nresolvers:\n  s:\n    kind: server\n    url_env: S\n    standard: {{connection_limit: {limit}}}\n"
                    ),
                    Path::new("pbps.yml")
                )
                .is_ok(),
                "{limit}"
            );
        }
    }
}
