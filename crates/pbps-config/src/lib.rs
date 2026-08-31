//! Project configuration (`pbps.yml`) and path resolution.
//!
//! The location of the config file defines the project root, and every other
//! relative path is resolved against it. That is what lets `pbps` run from a
//! subdirectory — the same behaviour as `git` and `cargo`, so users never have to
//! remember which level they are standing on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "pbps.yml";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no {CONFIG_FILE} found (searched upwards from `{start}` to the filesystem root)")]
    NotFound { start: PathBuf },

    #[error("cannot read `{path}`: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("`{path}` is malformed: {message}")]
    Parse { path: PathBuf, message: String },

    #[error("no environment named `{name}` in {CONFIG_FILE}; it declares: {available}")]
    UnknownEnvironment { name: String, available: String },

    #[error(
        "environment `{name}` reads its connection string from ${var}, which is not set.\n\
         Export it, or pass the connection string directly with --db."
    )]
    MissingConnection { name: String, var: String },
}

/// The target database dialect.
///
/// A project is bound to exactly one dialect. "Supports multiple databases" means
/// the tool can drive different databases, not that one set of declarations
/// deploys to both (SPEC §1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DialectName {
    Mssql,
    Postgres,
}

impl DialectName {
    pub const fn as_str(self) -> &'static str {
        match self {
            DialectName::Mssql => "mssql",
            DialectName::Postgres => "postgres",
        }
    }
}

impl std::fmt::Display for DialectName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How to treat objects in the database that the declarations do not mention
/// (SPEC §8.2).
///
/// `Ignore` is the default because it is the precondition for gradual adoption:
/// pbps has to be able to share a database with tooling that was there first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unmanaged {
    #[default]
    Ignore,
    Warn,
    Error,
}

/// One deployment target.
///
/// # Why there is no `url:` field
///
/// A connection string carries a password, and `pbps.yml` is committed to git.
/// The config therefore names the **environment variable** that holds the
/// string, never the string. This is not an inconvenience to be worked around
/// with a second, undocumented field: an inline `url:` would be a credential in
/// version control, and the one thing worse than not having the feature is
/// having it and being surprised by it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    /// The name of the environment variable holding the ADO.NET connection
    /// string, e.g. `PROD_CONN`.
    pub url_env: String,

    /// Shown by `pbps status`, for humans reading a list of environments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Environment {
    /// Reads the connection string from the environment.
    pub fn connection_string(&self, name: &str) -> Result<String, ConfigError> {
        std::env::var(&self.url_env).map_err(|_| ConfigError::MissingConnection {
            name: name.to_owned(),
            var: self.url_env.clone(),
        })
    }
}

/// Exec points (SPEC §9.4): pbps runs a command, and the command does the
/// talking.
///
/// There are no Slack or Teams integrations here on purpose. An exec hook
/// outlives any chat API, holds no credentials of its own, and lets a team
/// deliver drift alerts through whatever they already run.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    /// Run after a successful `apply`, with the plan JSON on stdin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_apply: Option<String>,

    /// Run when `verify` finds drift, with the drift report JSON on stdin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_drift: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub dialect: DialectName,

    #[serde(default = "default_schema_dir")]
    pub schema_dir: PathBuf,

    #[serde(default = "default_ids_file")]
    pub ids_file: PathBuf,

    /// Deployment targets, by name. Optional: a project that only ever passes
    /// `--db` explicitly needs none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environments: BTreeMap<String, Environment>,

    #[serde(default)]
    pub hooks: Hooks,

    #[serde(default)]
    pub unmanaged: Unmanaged,
}

fn default_schema_dir() -> PathBuf {
    PathBuf::from("schema")
}

fn default_ids_file() -> PathBuf {
    PathBuf::from("schema.ids.json")
}

impl Config {
    pub fn parse(yaml: &str, path: &Path) -> Result<Self, ConfigError> {
        serde_saphyr::from_str(yaml).map_err(|e| ConfigError::Parse {
            path: path.to_owned(),
            message: e.to_string(),
        })
    }
}

/// A located project: its root directory plus its configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub root: PathBuf,
    pub config: Config,
}

impl Project {
    /// Searches upwards from `start` for `pbps.yml`.
    pub fn discover(start: &Path) -> Result<Self, ConfigError> {
        let mut dir = Some(start);
        while let Some(d) = dir {
            let candidate = d.join(CONFIG_FILE);
            if candidate.is_file() {
                return Self::load(&candidate);
            }
            dir = d.parent();
        }
        Err(ConfigError::NotFound {
            start: start.to_owned(),
        })
    }

    /// Loads a specific config file directly.
    pub fn load(config_path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(config_path).map_err(|source| ConfigError::Read {
            path: config_path.to_owned(),
            source,
        })?;
        let config = Config::parse(&text, config_path)?;
        // A config file is always inside some directory; the only way to have no
        // parent is to have been handed an empty path.
        let root = config_path.parent().unwrap_or(Path::new(".")).to_owned();
        Ok(Self { root, config })
    }

    /// Path to the declarations directory, absolute or relative to the caller's
    /// working directory.
    pub fn schema_dir(&self) -> PathBuf {
        self.root.join(&self.config.schema_dir)
    }

    pub fn ids_file(&self) -> PathBuf {
        self.root.join(&self.config.ids_file)
    }

    /// Looks up a named environment, listing what does exist when it does not.
    ///
    /// A typo in an environment name would otherwise deploy nothing and say
    /// only "not found" — while the user is watching a release window.
    pub fn environment(&self, name: &str) -> Result<&Environment, ConfigError> {
        self.config.environments.get(name).ok_or_else(|| {
            let names: Vec<&str> = self
                .config
                .environments
                .keys()
                .map(String::as_str)
                .collect();
            ConfigError::UnknownEnvironment {
                name: name.to_owned(),
                available: if names.is_empty() {
                    "none".to_owned()
                } else {
                    names.join(", ")
                },
            }
        })
    }

    /// Resolves a named environment to a connection string.
    pub fn connection_string(&self, name: &str) -> Result<String, ConfigError> {
        self.environment(name)?.connection_string(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_uses_defaults() {
        let c = Config::parse("dialect: mssql\n", Path::new("pbps.yml")).unwrap();
        assert_eq!(c.dialect, DialectName::Mssql);
        assert_eq!(c.schema_dir, PathBuf::from("schema"));
        assert_eq!(c.ids_file, PathBuf::from("schema.ids.json"));
    }

    #[test]
    fn paths_can_be_overridden() {
        let c = Config::parse(
            "dialect: postgres\nschema_dir: db/tables\nids_file: db/ids.json\n",
            Path::new("pbps.yml"),
        )
        .unwrap();
        assert_eq!(c.dialect, DialectName::Postgres);
        assert_eq!(c.schema_dir, PathBuf::from("db/tables"));
    }

    /// `dialect` has no default: guessing wrong produces SQL that is
    /// syntactically valid and semantically wrong.
    #[test]
    fn dialect_is_required() {
        assert!(Config::parse("schema_dir: schema\n", Path::new("pbps.yml")).is_err());
    }

    #[test]
    fn unknown_dialect_is_rejected() {
        assert!(Config::parse("dialect: oracle\n", Path::new("pbps.yml")).is_err());
    }

    /// A misspelled field name must be rejected, never silently defaulted.
    #[test]
    fn unknown_fields_are_rejected() {
        let err =
            Config::parse("dialect: mssql\nschema_dirs: x\n", Path::new("pbps.yml")).unwrap_err();
        assert!(
            err.to_string().contains("schema_dirs"),
            "the error should name the misspelled field: {err}"
        );
    }

    #[test]
    fn discovery_walks_up_from_a_subdirectory() {
        let tmp = std::env::temp_dir().join(format!("pbps-cfg-{}", std::process::id()));
        let nested = tmp.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(tmp.join(CONFIG_FILE), "dialect: mssql\n").unwrap();

        let p = Project::discover(&nested).unwrap();
        assert_eq!(p.root, tmp);
        assert_eq!(p.schema_dir(), tmp.join("schema"));
        assert_eq!(p.ids_file(), tmp.join("schema.ids.json"));

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn environments_and_hooks_are_optional() {
        let c = Config::parse("dialect: mssql\n", Path::new("pbps.yml")).unwrap();
        assert!(c.environments.is_empty());
        assert_eq!(c.hooks, Hooks::default());
        assert_eq!(c.unmanaged, Unmanaged::Ignore);
    }

    #[test]
    fn environments_and_hooks_parse() {
        let c = Config::parse(
            concat!(
                "dialect: mssql\n",
                "unmanaged: warn\n",
                "environments:\n",
                "  prod:\n",
                "    url_env: PROD_CONN\n",
                "    description: the one that must not break\n",
                "  staging:\n",
                "    url_env: STAGING_CONN\n",
                "hooks:\n",
                "  on_drift: ./scripts/alert.sh\n",
            ),
            Path::new("pbps.yml"),
        )
        .unwrap();
        assert_eq!(c.unmanaged, Unmanaged::Warn);
        assert_eq!(c.environments["prod"].url_env, "PROD_CONN");
        assert_eq!(c.environments["staging"].description, None);
        assert_eq!(c.hooks.on_drift.as_deref(), Some("./scripts/alert.sh"));
        assert_eq!(c.hooks.on_apply, None);
    }

    /// A connection string in `pbps.yml` would be a password in git. The field
    /// does not exist, and a user reaching for it must be told so rather than
    /// have it silently ignored.
    #[test]
    fn an_inline_connection_string_is_rejected() {
        let err = Config::parse(
            "dialect: mssql\nenvironments:\n  prod:\n    url: Server=x;Password=hunter2\n",
            Path::new("pbps.yml"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("url"), "{err}");
    }

    #[test]
    fn an_unknown_environment_lists_the_ones_that_exist() {
        let project = Project {
            root: PathBuf::from("."),
            config: Config::parse(
                "dialect: mssql\nenvironments:\n  prod:\n    url_env: PROD_CONN\n",
                Path::new("pbps.yml"),
            )
            .unwrap(),
        };
        let err = project.environment("prd").unwrap_err();
        assert!(err.to_string().contains("prod"), "{err}");
    }

    /// The remedy has to name the variable: "not set" alone leaves the user
    /// guessing which of several it was.
    #[test]
    fn a_missing_connection_variable_names_itself() {
        let env = Environment {
            url_env: "PBPS_DEFINITELY_UNSET_9137".into(),
            description: None,
        };
        let err = env.connection_string("prod").unwrap_err();
        assert!(
            err.to_string().contains("PBPS_DEFINITELY_UNSET_9137"),
            "{err}"
        );
    }

    #[test]
    fn discovery_reports_where_it_looked() {
        let tmp = std::env::temp_dir().join(format!("pbps-none-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // This directory has no pbps.yml, and nothing above /tmp will either.
        let err = Project::discover(&tmp).unwrap_err();
        assert!(matches!(err, ConfigError::NotFound { .. }));
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
