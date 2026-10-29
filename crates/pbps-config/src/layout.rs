//! The named-database layout; legacy commands keep their flat input contract.
use crate::{Config, ConfigError, DialectName, Project};
use pbps_model::{DatabaseMap, DatabaseName};
use serde::{Deserialize, Deserializer};
use std::path::PathBuf;

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

impl Config {
    pub(crate) fn check_databases(&self) -> Result<(), String> {
        if self.databases.is_some() {
            if self.dialect != DialectName::Mssql {
                return Err("databases: is supported only for SQL Server; PostgreSQL projects use the single-database layout".into());
            }
            if self.ids_file.as_os_str() != "schema.ids.json" {
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
            (
                name,
                DatabasePaths {
                    schema_dir,
                    ids_file,
                },
            )
        }))
        .map_err(fail)
    }
}
