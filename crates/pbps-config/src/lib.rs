//! Project configuration (`pbps.yml`) and path resolution.
//!
//! The location of the config file defines the project root, and every other
//! relative path is resolved against it. That is what lets `pbps` run from a
//! subdirectory — the same behaviour as `git` and `cargo`, so users never have to
//! remember which level they are standing on.

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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub dialect: DialectName,

    #[serde(default = "default_schema_dir")]
    pub schema_dir: PathBuf,

    #[serde(default = "default_ids_file")]
    pub ids_file: PathBuf,
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
    fn discovery_reports_where_it_looked() {
        let tmp = std::env::temp_dir().join(format!("pbps-none-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // This directory has no pbps.yml, and nothing above /tmp will either.
        let err = Project::discover(&tmp).unwrap_err();
        assert!(matches!(err, ConfigError::NotFound { .. }));
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
