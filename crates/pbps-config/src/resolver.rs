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
    },
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
}
