//! The named-database layout; legacy commands keep their flat input contract.
use crate::{Config, ConfigError, DialectName, Project};
use pbps_model::{DatabaseMap, DatabaseName};
use serde::{Deserialize, Deserializer};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "Vec<DatabaseName>", into = "Vec<DatabaseName>")]
pub struct DatabaseList(Vec<DatabaseName>);
impl DatabaseList {
    pub fn iter(&self) -> impl Iterator<Item = &DatabaseName> {
        self.0.iter()
    }
}
impl TryFrom<Vec<DatabaseName>> for DatabaseList {
    type Error = String;
    fn try_from(names: Vec<DatabaseName>) -> Result<Self, String> {
        DatabaseMap::new(names.iter().cloned().map(|name| (name, ())))?;
        Ok(Self(names))
    }
}
impl From<DatabaseList> for Vec<DatabaseName> {
    fn from(value: DatabaseList) -> Self {
        value.0
    }
}
impl schemars::JsonSchema for DatabaseList {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "DatabaseList".into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let item = generator.subschema_for::<DatabaseName>();
        schemars::json_schema!({"type":"array","minItems":1,"uniqueItems":true,"items":item})
    }
}
pub(crate) fn names<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DatabaseList>, D::Error> {
    DatabaseList::deserialize(d).map(Some)
}

/// The path as its normal segments, or `None` when `..`, a root or a prefix
/// makes the answer depend on the file system (judged by [`Project::load`]).
fn lexical(path: &Path) -> Option<Vec<&std::ffi::OsStr>> {
    path.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| match c {
            Component::Normal(name) => Some(name),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => None,
        })
        .collect()
}

impl Config {
    /// The identity tree's directory as the config spells it, relative to the
    /// project root.
    pub fn ids_dir(&self) -> PathBuf {
        self.ids_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(crate::DEFAULT_IDS_DIR))
    }

    /// The spelling half of the #739 rule for `ids_dir`: not the project root
    /// or above it, and not `schema_dir` or a directory holding it. Both would
    /// put identity files where declarations are read, or scatter declarations
    /// among them. The file system half is [`Project::load`]'s.
    pub(crate) fn check_ids_dir(&self) -> Result<(), String> {
        let Some(dir) = &self.ids_dir else {
            return Ok(());
        };
        // `ids_dir` replaces `ids_file` (DEC-2600.7). A config that moved the
        // file and also names the tree has no one meaning. The default value
        // is tolerated: a typed config always carries it, and one written out
        // and read back must stay valid.
        if self.ids_file.as_os_str() != crate::DEFAULT_IDS_FILE {
            return Err("`ids_dir` replaces `ids_file`; name only one of them. A project that names `ids_file` keeps reading that file until it moves to `ids_dir`".into());
        }
        if dir.is_relative() && crate::only_dot_segments(dir) {
            return Err("`ids_dir` is the project root or a directory above it; identity files live in a subdirectory, such as `.pbps/ids` (the default)".into());
        }
        if let (Some(ids), Some(schema)) = (lexical(dir), lexical(&self.schema_dir))
            && schema.starts_with(&ids)
        {
            return Err("`ids_dir` is `schema_dir` or a directory holding it; identity files must live apart from the declarations".into());
        }
        Ok(())
    }

    pub(crate) fn check_databases(&self) -> Result<(), String> {
        if self.databases.is_some() {
            if self.dialect != DialectName::Mssql {
                return Err("databases: is supported only for SQL Server; PostgreSQL projects use the single-database layout".into());
            }
            if self.ids_file.as_os_str() != crate::DEFAULT_IDS_FILE {
                return Err("databases: keeps schema.ids.json inside each database directory; remove the project-level ids_file override".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabasePaths {
    pub schema_dir: PathBuf,
    pub ids_file: PathBuf,
    /// This database's identity tree: `<ids_dir>/<database>`. The path rule
    /// mirrors the YAML path, which already starts with the database name
    /// (design-2600 §3.2), so no case of its own is needed.
    pub ids_dir: PathBuf,
}
impl Project {
    /// A1's configuration/layout entry point; it grants no deployment authority.
    pub fn load_project(config: &std::path::Path) -> Result<Self, ConfigError> {
        let project = Self::load_with(config, Config::parse_project)?;
        if project.config.databases.is_some() {
            let path = project.ids_file();
            match std::fs::symlink_metadata(&path) {
                Ok(_) => return Err(ConfigError::Parse { path: config.to_owned(), message: "databases: is ambiguous beside a project-level schema.ids.json; keep one identity file inside each declared database directory".into() }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                Err(reason) => return Err(ConfigError::Read { path, reason }),
            }
        }
        Ok(project)
    }
    pub fn database_paths(&self) -> Result<DatabaseMap<DatabasePaths>, ConfigError> {
        let fail = |message| ConfigError::Parse {
            path: self.config_file(),
            message,
        };
        let names = self
            .config
            .databases
            .as_ref()
            .ok_or_else(|| fail("this project uses the single-database layout".into()))?;
        DatabaseMap::new(names.iter().cloned().map(|name| {
            let schema_dir = self.schema_dir().join(name.raw());
            let ids_file = schema_dir.join("schema.ids.json");
            let ids_dir = self.ids_dir().join(name.raw());
            (
                name,
                DatabasePaths {
                    schema_dir,
                    ids_file,
                    ids_dir,
                },
            )
        }))
        .map_err(fail)
    }
}
