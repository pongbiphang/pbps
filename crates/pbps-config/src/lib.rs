//! Project configuration (`pbps.yml`) and path resolution.
//!
//! The location of the config file defines the project root, and every other
//! relative path is resolved against it. That is what lets `pbps` run from a
//! subdirectory — the same behaviour as `git` and `cargo`, so users never have to
//! remember which level they are standing on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub mod layout;
pub mod resolver;
pub use layout::{DatabaseList, DatabasePaths};
pub mod test_manifest;

pub const CONFIG_FILE: &str = "pbps.yml";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no {CONFIG_FILE} found (searched upwards from `{start}` to the filesystem root)")]
    NotFound { start: PathBuf },

    /// `reason`, not `source`: the text already ends with it, and a chain
    /// renderer would print it twice (#1018; see `DbError::Connect`).
    #[error("cannot read `{path}`: {reason}")]
    Read {
        path: PathBuf,
        reason: std::io::Error,
    },

    #[error("`{path}` is malformed: {message}")]
    Parse { path: PathBuf, message: String },

    #[error("no environment named `{name}` in {CONFIG_FILE}; it declares: {available}")]
    UnknownEnvironment { name: String, available: String },

    #[error(
        "no resolver profile named `{name}` in {CONFIG_FILE}; it declares: {available}. Configure it under resolvers or select an existing profile with --resolve-with"
    )]
    UnknownResolver { name: String, available: String },

    /// A selection naming one of the measured profiles pbps once had. It is
    /// told apart from an unknown name so the operator learns what replaced
    /// it, not only that it is missing (#1636).
    #[error(
        "`{name}` was a measured resolver profile, which pbps no longer has (#1636). Declare a resolver under resolvers in {CONFIG_FILE}, with kind: docker or kind: server, and select it by its name with --resolve-with"
    )]
    RemovedResolverProfile { name: String },

    /// A resolver entry carrying the `profile` field the measured profiles
    /// were chosen by (#1636).
    #[error(
        "`{path}` is malformed: resolver `{resolver}` names a profile, but the measured resolver profiles were removed (#1636). An entry is the resolver itself, chosen by its kind: docker or server"
    )]
    RemovedResolverProfileField { path: PathBuf, resolver: String },

    /// A resolver entry naming its externals file by the field's old name:
    /// "baseline" is also the ledger's state reset, `pbps baseline` (#2007).
    #[error(
        "`{path}` is malformed: resolver `{resolver}` names `baseline:`, which is now `externals:` (#2007). The file is unchanged: it stages the external objects the declarations read. Rename the field"
    )]
    RenamedResolverBaseline { path: PathBuf, resolver: String },

    #[error(
        "resolver `{resolver}` sets `same_instance`, which applies only to SQL Server server entries; remove it for PostgreSQL"
    )]
    InapplicableSameInstance { resolver: String },

    #[error(
        "resolver profile names must start with an ASCII letter or digit and contain only letters, digits, dots, underscores or hyphens"
    )]
    InvalidResolverName,

    #[error(
        "environment `{name}` reads its connection string from ${var}, which is not set.\n\
         Export it, or pass the connection string directly with --db."
    )]
    MissingConnection { name: String, var: String },

    #[error(
        "environment `{name}` sets both fingerprint_key_env and fingerprint_key_file; \
         keep the one its key is delivered through"
    )]
    AmbiguousFingerprintKey { name: String },

    /// The declarations directory holds the project itself: it is the project
    /// root or a directory above it (#739). Every `.yml` beneath it is read as
    /// a declaration, `pbps.yml` included, so such a layout never loaded; it
    /// is refused here, by name, instead of surfacing as "the YAML in
    /// `pbps.yml`" is not a declaration.
    #[error(
        "`schema_dir` in `{path}` is the project root or a directory above it, so it would \
         read `pbps.yml` as a declaration; declarations live in a subdirectory of the \
         project, such as `schema/` (the default)"
    )]
    SchemaDirHoldsTheProject { path: PathBuf },
}

/// The target database dialect.
///
/// A project is bound to exactly one dialect. "Supports multiple databases" means
/// the tool can drive different databases, not that one set of declarations
/// deploys to both (SPEC §1.3).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
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
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
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
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    /// The name of the environment variable holding the ADO.NET connection
    /// string, e.g. `PROD_CONN`.
    pub url_env: String,

    /// Shown by `pbps status`, for humans reading a list of environments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Overrides the project's resolver profile for this deployment target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = "^[A-Za-z0-9][A-Za-z0-9_.-]*$"))]
    pub resolve_with: Option<String>,

    /// The name of the environment variable holding this target's
    /// fingerprint key, base64 as `pbps key generate` prints it (DEC-952.1).
    /// Named, never inline, for the reason `url_env` is. Exclusive with
    /// `fingerprint_key_file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_key_env: Option<String>,

    /// A file holding this target's fingerprint key, readable by its owner
    /// only; relative to the project root. Exclusive with
    /// `fingerprint_key_env`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_key_file: Option<PathBuf>,

    /// `false` turns the connected rehearsal off for this environment: a plan
    /// computed against it is then sealed as not rehearsed, never as
    /// rehearsed (#2379). The default is on whenever a scratch entry is
    /// selected, and `plan --no-rehearse` turns it off for one run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rehearse: Option<bool>,
}

/// Where an environment's fingerprint key is read from. Keys are never shared
/// between environments and have no default: a key one environment's
/// fingerprints were made under is no evidence for another's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FingerprintKeySource {
    /// The environment variable of this name.
    Env(String),
    /// The file at this path, already resolved against the project root.
    File(PathBuf),
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
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    /// Run after a successful `apply`, with the unchanged plan JSON on stdin.
    ///
    /// This is the original hook contract. Use `on_apply_attempt` for a
    /// versioned event that also reports failed attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_apply: Option<String>,

    /// Run after either outcome of an `apply`, with a versioned event JSON on
    /// stdin containing the plan path, checksum, outcome and ledger entry or
    /// error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_apply_attempt: Option<String>,

    /// Run when `verify` finds drift, with the drift report JSON on stdin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_drift: Option<String>,
}

/// The optional throwaway engine of SPEC §9.3.
///
/// # Why it is always optional
///
/// The air-gap promise stands: with no dev database the preview degrades to
/// lightweight normalization and says so. A preview remains available wherever
/// Docker and a database connection are unavailable.
///
/// A dev-verified plan is still a **preview**. Applyable plans come only from
/// `plan --db` against the target, and this does not move that line.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
// The three fields are individually optional in Rust because serde has to read
// the file before it can say which one is present — but `dev::spec` requires
// **exactly one** of `docker`, `url_env` and `resolver`, and refuses `dev: {}`
// and a block naming two. A schema that left the derive's shape alone would
// bless pbps.yml files the CLI then cannot run a plan with, which is the one
// thing generating the schema from these types is supposed to prevent (the
// same reasoning as the module declaration's `oneOf`).
//
// Each branch constrains the other keys rather than merely requiring its own:
// `required` alone would accept a file naming two.
//
// The others are pinned to null rather than to `false`, because that is
// `dev::spec`'s own rule: YAML writes `url_env:` with nothing after it, which
// serde reports as absent, so a block with a real `docker:` beside an empty
// `url_env:` resolves. `false` would refuse that line, a disagreement with the
// tool in the stricter direction.
#[schemars(extend("oneOf" = [
    serde_json::json!({
        "required": ["docker"],
        "properties": {"docker": {"type": "string"}, "url_env": {"type": "null"}, "resolver": {"type": "null"}},
    }),
    serde_json::json!({
        "required": ["url_env"],
        "properties": {"url_env": {"type": "string"}, "docker": {"type": "null"}, "resolver": {"type": "null"}},
    }),
    serde_json::json!({
        "required": ["resolver"],
        "properties": {"resolver": {"type": "string"}, "docker": {"type": "null"}, "url_env": {"type": "null"}},
    }),
]))]
pub struct Dev {
    /// A shared entry under `resolvers`, also selectable by connected planning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver: Option<String>,

    /// The name of an environment variable holding a connection string to a
    /// throwaway server — never the string itself, for the reason
    /// [`Environment`] gives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_env: Option<String>,

    /// A container image to start and throw away, e.g.
    /// `mcr.microsoft.com/mssql/server:2022-latest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
#[schemars(extend("allOf" = serde_json::json!([{"if":{"required":["databases"]},"then":{"properties":{"dialect":{"const":"mssql"},"ids_file":{"const":"schema.ids.json"}}}}])))]
pub struct Config {
    pub dialect: DialectName,

    /// SQL Server database coordinates; absent preserves the flat layout.
    /// Layout/loader support precedes instance-wide command planning (DEC-2501.2).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "layout::names"
    )]
    #[schemars(with = "DatabaseList")]
    pub databases: Option<DatabaseList>,

    /// Roles the organization trusts to change evaluated user-type code (#2046).
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub trusted_roles: std::collections::BTreeSet<pbps_model::RoleName>,

    /// Review and pin user-written routines on referenced type input paths (DEC-2181.2).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pin_type_routines: bool,

    // The schema carries the lexical half of the #739 rule, so an editor
    // refuses what `Config::parse` refuses: an empty path, or one made only
    // of `.` and `..`. Both separators count, as they do on Windows; a
    // directory really named `.\` elsewhere is not worth a schema that
    // accepts `..\..`. The filesystem half is the loader's alone.
    #[serde(default = "default_schema_dir")]
    #[schemars(regex(pattern = r"^(?!(?:\.\.?(?:[/\\]+|$))*$)"))]
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

    /// The optional dev database (SPEC §9.3). Absent means previews stay
    /// offline, which is the default and a supported way to work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev: Option<Dev>,

    /// Explicitly trusted sources for the separate target-planning resolver.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(extend("propertyNames" = serde_json::json!({"pattern": "^[A-Za-z0-9][A-Za-z0-9_.-]*$"})))]
    pub resolvers: BTreeMap<String, resolver::ResolverProfile>,

    /// Default profile name. Selection/acquisition stays lazy and target-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = "^[A-Za-z0-9][A-Za-z0-9_.-]*$"))]
    pub resolve_with: Option<String>,

    /// How many rows a `data:` block may declare before `validate` says this
    /// does not look like reference data (ADR-0004).
    ///
    /// A knob rather than a constant because the line between a lookup table
    /// and somebody's business table is a judgement about *this* project — but
    /// it is a warning either way, never a refusal: pbps does not get to decide
    /// that a 1,200-row currency table is illegitimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_data_rows: Option<usize>,

    // #1789, DEC-1789.2.
    /// How far ahead of its rows a RANGE-partitioned table's partitions must
    /// run before `doctor` warns that they are running out. Absent means twice
    /// the newest partition's width.
    ///
    /// A plain number is a count of partitions, for every key type: a number
    /// alone cannot say whether it counts key values or partitions, and a
    /// partition count means the same on a `date` key and a `bigint` one. A
    /// date or time key may instead be given a width in the grammar of
    /// `pbps partitions extend --every`: a number and `day`, `week`, `month`
    /// or `year` (`2month`, `1year`). The text is read where it is used, by
    /// the engine that knows the key, and one it cannot read is reported by
    /// `doctor` rather than ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_runway: Option<PartitionRunway>,

    /// The organization's own rules and their suppressions (ADR-0008). Absent
    /// means every built-in rule at its default severity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policies: Option<pbps_policy::Policies>,

    /// External consumers whose declared uses and writes the plan must protect.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub consumers: pbps_policy::consumers::Consumers,
}

/// The horizon of `partition_runway:`: a count of partitions, or a width.
///
/// Two spellings of one setting, told apart by the YAML itself: an integer is
/// a count, text is a width (`2month`). Text made only of digits is read as a
/// count by the engine, so `"3"` and `3` agree.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(untagged)]
pub enum PartitionRunway {
    /// This many partitions, counting the one the newest rows are in.
    Partitions(std::num::NonZeroU32),
    /// This much of the key's range, as `--every` writes it: `2month`.
    Width(String),
}

impl PartitionRunway {
    /// The setting as the engine reads it: the count's digits, or the width.
    pub fn text(&self) -> String {
        match self {
            Self::Partitions(n) => n.to_string(),
            Self::Width(text) => text.clone(),
        }
    }
}

impl Config {
    /// The policies block, or the defaults when there is none.
    pub fn policies(&self) -> pbps_policy::Policies {
        self.policies.clone().unwrap_or_default()
    }
}

fn default_schema_dir() -> PathBuf {
    PathBuf::from("schema")
}

fn default_ids_file() -> PathBuf {
    PathBuf::from("schema.ids.json")
}

impl Config {
    /// Legacy single-database command input. Named projects need the new
    /// layout/loader APIs until instance-wide ordering is available.
    pub fn parse(yaml: &str, path: &Path) -> Result<Self, ConfigError> {
        let config = Self::parse_project(yaml, path)?;
        if config.databases.is_some() {
            return Err(ConfigError::Parse { path: path.to_owned(), message: "this command does not yet support instance-wide projects (databases:); use a single-database project".into() });
        }
        Ok(config)
    }

    /// Parse either layout without handing it to a legacy flat consumer.
    pub fn parse_project(yaml: &str, path: &Path) -> Result<Self, ConfigError> {
        let config: Self = serde_saphyr::from_str(yaml).map_err(|e| {
            // Read again only on failure, so every other error keeps the
            // typed parse's own position.
            if let Some(resolver) = resolver::entry_carrying(yaml, "profile") {
                ConfigError::RemovedResolverProfileField {
                    path: path.to_owned(),
                    resolver,
                }
            } else if let Some(resolver) = resolver::entry_carrying(yaml, "baseline") {
                ConfigError::RenamedResolverBaseline {
                    path: path.to_owned(),
                    resolver,
                }
            } else {
                ConfigError::Parse {
                    path: path.to_owned(),
                    message: e.to_string(),
                }
            }
        })?;
        if config
            .resolvers
            .keys()
            .map(String::as_str)
            .chain(config.resolve_with.as_deref())
            .chain(
                config
                    .environments
                    .values()
                    .filter_map(|env| env.resolve_with.as_deref()),
            )
            .any(|name| !resolver::valid_profile_name(name))
        {
            return Err(ConfigError::InvalidResolverName);
        }
        if config.dialect == DialectName::Postgres {
            for (name, profile) in &config.resolvers {
                if matches!(
                    profile,
                    resolver::ResolverProfile::Server {
                        same_instance: Some(_),
                        ..
                    }
                ) {
                    return Err(ConfigError::InapplicableSameInstance {
                        resolver: name.clone(),
                    });
                }
            }
        }
        if let Some((name, _)) = config.environments.iter().find(|(_, env)| {
            env.fingerprint_key_env.is_some() && env.fingerprint_key_file.is_some()
        }) {
            return Err(ConfigError::AmbiguousFingerprintKey { name: name.clone() });
        }
        if config.schema_dir.is_relative() && only_dot_segments(&config.schema_dir) {
            return Err(ConfigError::SchemaDirHoldsTheProject {
                path: path.to_owned(),
            });
        }
        config
            .check_databases()
            .map_err(|message| ConfigError::Parse {
                path: path.to_owned(),
                message,
            })?;
        Ok(config)
    }
}

/// Whether a relative path is made only of `.` and `..`: the base itself or
/// a directory above it, whatever symlinks lie on the way, since `..` from a
/// real directory is always its physical parent.
///
/// Not `a/..`: a name and a `..` cancel only lexically, and when `a` is a
/// symlink the pair leads somewhere else entirely (review on #1548). Such a
/// path is judged where it can be, against the filesystem in
/// [`Project::load`], and lexically only where git's own tree is all there
/// is (`baseline`, which reads configs from history).
fn only_dot_segments(path: &Path) -> bool {
    use std::path::Component;
    path.components()
        .all(|c| matches!(c, Component::CurDir | Component::ParentDir))
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
        Self::load_with(config_path, Config::parse)
    }

    fn load_with(
        config_path: &Path,
        parse: fn(&str, &Path) -> Result<Config, ConfigError>,
    ) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(config_path).map_err(|reason| ConfigError::Read {
            path: config_path.to_owned(),
            reason,
        })?;
        let config = parse(&text, config_path)?;
        // A config file is always inside some directory; the only way to have no
        // parent is to have been handed an empty path.
        let root = config_path.parent().unwrap_or(Path::new(".")).to_owned();
        // Any spelling can still reach the root through the filesystem: an
        // absolute path, `../<project>`, or a symlink. `parse` judges the
        // spelling alone (it also reads configs from git, where there is no
        // filesystem); here the directory that exists is compared with the
        // root itself (review on #1548). One that does not exist yet cannot
        // be the root, which does.
        let real = |p: &Path| {
            std::fs::canonicalize(if p.as_os_str().is_empty() {
                Path::new(".")
            } else {
                p
            })
        };
        // The root itself, or any directory above it: either holds `pbps.yml`.
        if let (Ok(dir), Ok(base)) = (real(&root.join(&config.schema_dir)), real(&root))
            && base.starts_with(&dir)
        {
            return Err(ConfigError::SchemaDirHoldsTheProject {
                path: config_path.to_owned(),
            });
        }
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

    /// The directory holding `pbps.yml`. Every path in the config is relative
    /// to it, and so is anything asked of git about this project.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to this project's `pbps.yml`.
    ///
    /// Discovery searches upwards, so a user two directories down cannot
    /// otherwise tell which file the command is obeying — which is exactly the
    /// question `pbps doctor` exists to answer.
    pub fn config_file(&self) -> PathBuf {
        self.root.join(CONFIG_FILE)
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

    /// Where a named environment's fingerprint key is read from, or `None`
    /// when it configures none. A relative file is the project root's.
    pub fn fingerprint_key_source(
        &self,
        name: &str,
    ) -> Result<Option<FingerprintKeySource>, ConfigError> {
        let env = self.environment(name)?;
        Ok(
            match (&env.fingerprint_key_env, &env.fingerprint_key_file) {
                (Some(_), Some(_)) => {
                    return Err(ConfigError::AmbiguousFingerprintKey {
                        name: name.to_owned(),
                    });
                }
                (Some(var), None) => Some(FingerprintKeySource::Env(var.clone())),
                (None, Some(file)) => Some(FingerprintKeySource::File(self.root.join(file))),
                (None, None) => None,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unreadable file is named with its reason once: inside the error's
    /// own text, not again as its source, which a chain renderer would print
    /// twice (#1018).
    #[test]
    fn an_unreadable_config_names_its_reason_once() {
        let missing = std::env::temp_dir().join("pbps-1018-no-such-dir/pbps.yml");
        let error = Project::load(&missing).unwrap_err();
        assert!(matches!(error, ConfigError::Read { .. }), "{error:?}");
        assert!(std::error::Error::source(&error).is_none(), "{error:?}");
        assert!(error.to_string().starts_with("cannot read `"), "{error}");
    }

    fn project_with(environment: &str) -> Result<Project, ConfigError> {
        let yaml = format!("dialect: postgres\nenvironments:\n  prod:\n{environment}");
        Ok(Project {
            root: PathBuf::from("/srv/app"),
            config: Config::parse(&yaml, Path::new("pbps.yml"))?,
        })
    }

    #[test]
    fn a_fingerprint_key_comes_from_one_named_source_per_environment() {
        let env = project_with("    url_env: PROD_DB\n    fingerprint_key_env: PROD_FP\n").unwrap();
        assert_eq!(
            env.fingerprint_key_source("prod").unwrap(),
            Some(FingerprintKeySource::Env("PROD_FP".into()))
        );
        let file =
            project_with("    url_env: PROD_DB\n    fingerprint_key_file: keys/prod\n").unwrap();
        assert_eq!(
            file.fingerprint_key_source("prod").unwrap(),
            Some(FingerprintKeySource::File(PathBuf::from(
                "/srv/app/keys/prod"
            )))
        );
        // Negative: none configured is `None`, not a default.
        let none = project_with("    url_env: PROD_DB\n").unwrap();
        assert_eq!(none.fingerprint_key_source("prod").unwrap(), None);
        // Negative: both is refused when the file is read, naming the environment.
        let both = project_with(
            "    url_env: PROD_DB\n    fingerprint_key_env: PROD_FP\n    fingerprint_key_file: k\n",
        );
        assert!(matches!(
            both,
            Err(ConfigError::AmbiguousFingerprintKey { ref name }) if name == "prod"
        ));
    }

    /// `partition_runway:` is a count of partitions or a width, and nothing
    /// else reads: a typo in the unit is the engine's to name, but a list, a
    /// negative number, a fraction or a zero count never reaches it.
    #[test]
    fn partition_runway_is_a_count_or_a_width_and_absent_by_default() {
        let parse = |value: &str| {
            Config::parse(
                &format!("dialect: postgres\npartition_runway: {value}\n"),
                Path::new("pbps.yml"),
            )
        };
        let runway = |value: &str| parse(value).unwrap().partition_runway.unwrap();
        assert_eq!(
            runway("3"),
            PartitionRunway::Partitions(std::num::NonZeroU32::new(3).unwrap())
        );
        assert_eq!(runway("2month"), PartitionRunway::Width("2month".into()));
        assert_eq!(runway("\"3\""), PartitionRunway::Width("3".into()));
        assert_eq!(runway("3").text(), "3");
        assert_eq!(runway("\"3 weeks\"").text(), "3 weeks");
        let none = Config::parse("dialect: postgres\n", Path::new("pbps.yml")).unwrap();
        assert_eq!(none.partition_runway, None);
        // Negative: none of these is a count or a width.
        for refused in ["0", "-1", "1.5", "[1, 2]", "{a: 1}", "true"] {
            assert!(parse(refused).is_err(), "{refused} must be refused");
        }
    }

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
    /// #739: the declarations directory is never the project root. Every
    /// spelling of the root is refused by name, while a subdirectory, a
    /// sibling and a path that only passes through `..` are not.
    #[test]
    fn a_schema_dir_holding_the_project_is_refused() {
        for root in [".", "./", "", "./.", "..", "../..", "./../."] {
            let text = format!("dialect: mssql\nschema_dir: \"{root}\"\n");
            let err = Config::parse(&text, Path::new("pbps.yml")).unwrap_err();
            assert!(
                matches!(err, ConfigError::SchemaDirHoldsTheProject { .. }),
                "{root:?}: {err}"
            );
            assert!(err.to_string().contains("schema/"), "{err}");
        }
        // `a/..` is not judged by spelling: a symlink can make it lead
        // elsewhere, so `Project::load` judges it on disk.
        for fine in [
            "schema",
            "./schema",
            "db/tables",
            "../shared/schema",
            "a/../b",
            "a/..",
        ] {
            let text = format!("dialect: mssql\nschema_dir: \"{fine}\"\n");
            assert!(
                Config::parse(&text, Path::new("pbps.yml")).is_ok(),
                "{fine:?}"
            );
        }
        // An absolute path is judged where the file is: the root itself and
        // a directory above it are refused, a directory inside it is not.
        // The paths go in single-quoted YAML, which has no escapes: a double-
        // quoted Windows path reads `\U` in `C:\Users` as a broken escape.
        let tmp = std::env::temp_dir().join(format!("pbps-cfg739-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("schema")).unwrap();
        let config = tmp.join("pbps.yml");
        std::fs::write(
            &config,
            format!("dialect: mssql\nschema_dir: '{}'\n", tmp.display()),
        )
        .unwrap();
        let err = Project::load(&config).unwrap_err();
        assert!(
            matches!(err, ConfigError::SchemaDirHoldsTheProject { .. }),
            "{err}"
        );
        std::fs::write(
            &config,
            format!(
                "dialect: mssql\nschema_dir: '{}'\n",
                tmp.parent().unwrap().display()
            ),
        )
        .unwrap();
        let err = Project::load(&config).unwrap_err();
        assert!(
            matches!(err, ConfigError::SchemaDirHoldsTheProject { .. }),
            "{err}"
        );
        std::fs::write(
            &config,
            format!(
                "dialect: mssql\nschema_dir: '{}'\n",
                tmp.join("schema").display()
            ),
        )
        .unwrap();
        assert!(Project::load(&config).is_ok());
        // A relative spelling that climbs out and back in, and a symlink to
        // the root, reach the root through the filesystem (review on #1548).
        let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        std::fs::write(
            &config,
            format!("dialect: mssql\nschema_dir: \"../{name}\"\n"),
        )
        .unwrap();
        let err = Project::load(&config).unwrap_err();
        assert!(
            matches!(err, ConfigError::SchemaDirHoldsTheProject { .. }),
            "{err}"
        );
        // A real directory and its `..` lead back to the root on disk too.
        std::fs::create_dir_all(tmp.join("a")).unwrap();
        std::fs::write(&config, "dialect: mssql\nschema_dir: \"a/..\"\n").unwrap();
        let err = Project::load(&config).unwrap_err();
        assert!(
            matches!(err, ConfigError::SchemaDirHoldsTheProject { .. }),
            "{err}"
        );
        #[cfg(unix)]
        {
            // But a symlink into a subdirectory makes `alias/..` that
            // subdirectory's parent, `schema`, a valid declarations directory.
            std::fs::create_dir_all(tmp.join("schema/nested")).unwrap();
            std::os::unix::fs::symlink(tmp.join("schema/nested"), tmp.join("alias")).unwrap();
            std::fs::write(&config, "dialect: mssql\nschema_dir: \"alias/..\"\n").unwrap();
            assert!(Project::load(&config).is_ok());
            std::os::unix::fs::symlink(".", tmp.join("here")).unwrap();
            std::fs::write(&config, "dialect: mssql\nschema_dir: here\n").unwrap();
            let err = Project::load(&config).unwrap_err();
            assert!(
                matches!(err, ConfigError::SchemaDirHoldsTheProject { .. }),
                "{err}"
            );
        }
        std::fs::remove_dir_all(&tmp).unwrap();
    }

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
                "  on_apply_attempt: ./scripts/audit-apply.sh\n",
                "  on_drift: ./scripts/alert.sh\n",
            ),
            Path::new("pbps.yml"),
        )
        .unwrap();
        assert_eq!(c.unmanaged, Unmanaged::Warn);
        assert_eq!(c.environments["prod"].url_env, "PROD_CONN");
        assert_eq!(c.environments["staging"].description, None);
        assert_eq!(
            c.hooks.on_apply_attempt.as_deref(),
            Some("./scripts/audit-apply.sh")
        );
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
            resolve_with: None,
            fingerprint_key_env: None,
            fingerprint_key_file: None,
            rehearse: None,
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

    /// The dev database is optional, and a project that never mentions one has
    /// to keep working exactly as before — the air-gap promise of §9.1.
    #[test]
    fn the_dev_database_is_absent_unless_declared() {
        let c = Config::parse("dialect: mssql\n", Path::new("pbps.yml")).unwrap();
        assert!(c.dev.is_none());

        let c = Config::parse(
            "dialect: mssql\ndev:\n  docker: mcr.microsoft.com/mssql/server:2022-latest\n",
            Path::new("pbps.yml"),
        )
        .unwrap();
        assert_eq!(
            c.dev.unwrap().docker.as_deref(),
            Some("mcr.microsoft.com/mssql/server:2022-latest")
        );
    }

    #[test]
    fn dev_and_resolver_select_one_configured_entry() {
        let c = Config::parse("dialect: mssql\nresolvers:\n  scratch: {kind: server, url_env: SCRATCH, externals: externals.sql}\nresolve_with: scratch\ndev: {resolver: scratch}\n", Path::new("pbps.yml")).unwrap();
        let dev = c.dev.as_ref().unwrap().resolver.as_deref().unwrap();
        assert_eq!(
            c.select_resolver(Some(dev), None).unwrap().unwrap().profile,
            c.select_resolver(None, None).unwrap().unwrap().profile
        );
        assert!(c.select_resolver(Some("missing"), None).is_err());
    }

    /// A connection string in the file would be a credential in version
    /// control; the dev block names the variable, like every other target.
    #[test]
    fn the_dev_block_rejects_an_inline_url() {
        assert!(
            Config::parse(
                "dialect: mssql\ndev:\n  url: Server=localhost\n",
                Path::new("pbps.yml")
            )
            .is_err()
        );
    }
    #[test]
    fn trusted_role_names_are_recorded_as_a_set_and_do_not_admit_unknown_settings() {
        let config = Config::parse(
            "dialect: postgres\ntrusted_roles: [platform, platform, data_team]\n",
            Path::new("pbps.yml"),
        )
        .unwrap();
        assert_eq!(
            config.trusted_roles,
            std::collections::BTreeSet::from(["data_team".into(), "platform".into()])
        );
        assert!(
            Config::parse(
                "dialect: postgres\ntrusted_role: [platform]\n",
                Path::new("pbps.yml")
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod type_routine_tests {
    use super::*;

    #[test]
    fn type_routine_review_is_an_explicit_boolean_opt_in() {
        let parse = |extra: &str| {
            Config::parse(
                &format!("dialect: postgres\n{extra}"),
                Path::new("pbps.yml"),
            )
        };
        assert!(!parse("").unwrap().pin_type_routines);
        assert!(
            !parse("pin_type_routines: false\n")
                .unwrap()
                .pin_type_routines
        );
        assert!(
            parse("pin_type_routines: true\n")
                .unwrap()
                .pin_type_routines
        );
        assert!(parse("pin_type_routines: 7\n").is_err());
    }
}
