//! Database coordinates and globally reserved identities (DEC-2501.1).
//! These in-memory collections do not change Schema or saved-plan bytes.
use crate::{IdsFile, Schema, Uid, UidKind, escape::Escaped};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

/// A database name that can also be one declaration-directory component.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct DatabaseName(String);
impl DatabaseName {
    pub fn raw(&self) -> &str {
        &self.0
    }
    pub fn shown(&self) -> Escaped<'_> {
        Escaped(&self.0)
    }
}
impl FromStr for DatabaseName {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || matches!(value, "." | "..")
            || value
                .chars()
                .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
        {
            return Err(format!(
                "database `{}` must be one nonempty directory component without separators, a drive prefix or control characters",
                Escaped(value)
            ));
        }
        Ok(Self(value.into()))
    }
}
impl<'de> serde::Deserialize<'de> for DatabaseName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // deserialize_any keeps YAML numbers and booleans from being coerced
        // into directory names that the published string schema refuses.
        struct Name;
        impl serde::de::Visitor<'_> for Name {
            type Value = DatabaseName;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a database name as text")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(E::custom)
            }
        }
        deserializer.deserialize_any(Name)
    }
}
impl TryFrom<String> for DatabaseName {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}
impl From<DatabaseName> for String {
    fn from(value: DatabaseName) -> Self {
        value.0
    }
}
impl schemars::JsonSchema for DatabaseName {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "DatabaseName".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"string", "minLength":1, "pattern":r"^(?!\.{1,2}$)[^\u0000-\u001f\u007f-\u009f/\\:]+$"})
    }
}

/// A named resource cannot be constructed without its database coordinate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct InDatabase<T> {
    database: DatabaseName,
    value: T,
}
impl<T> InDatabase<T> {
    pub fn new(database: DatabaseName, value: T) -> Self {
        Self { database, value }
    }
    pub fn database(&self) -> &DatabaseName {
        &self.database
    }
    pub fn value(&self) -> &T {
        &self.value
    }
    pub fn into_parts(self) -> (DatabaseName, T) {
        (self.database, self.value)
    }
}

/// A nonempty, unambiguous project-level map; no wire representation yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseMap<T>(BTreeMap<DatabaseName, T>);
impl<T> DatabaseMap<T> {
    pub fn new(entries: impl IntoIterator<Item = (DatabaseName, T)>) -> Result<Self, String> {
        let mut map = BTreeMap::new();
        for (name, value) in entries {
            if map.insert(name.clone(), value).is_some() {
                return Err(format!(
                    "database `{}` is declared more than once",
                    name.shown()
                ));
            }
        }
        if map.is_empty() {
            return Err("databases must contain at least one database".into());
        }
        Ok(Self(map))
    }
    pub fn iter(&self) -> impl Iterator<Item = (&DatabaseName, &T)> {
        self.0.iter()
    }
    pub fn get(&self, database: &DatabaseName) -> Option<&T> {
        self.0.get(database)
    }
    pub fn keys(&self) -> impl Iterator<Item = &DatabaseName> {
        self.0.keys()
    }
    pub fn into_inner(self) -> BTreeMap<DatabaseName, T> {
        self.0
    }
    pub fn map<U>(&self, mut convert: impl FnMut(&DatabaseName, &T) -> U) -> DatabaseMap<U> {
        DatabaseMap(
            self.0
                .iter()
                .map(|(db, value)| (db.clone(), convert(db, value)))
                .collect(),
        )
    }
    pub fn try_map<U, E>(
        &self,
        mut convert: impl FnMut(&DatabaseName, &T) -> Result<U, E>,
    ) -> Result<DatabaseMap<U>, E> {
        self.0
            .iter()
            .map(|(db, value)| Ok((db.clone(), convert(db, value)?)))
            .collect::<Result<BTreeMap<_, _>, E>>()
            .map(DatabaseMap)
    }
}

pub type ProjectSchemas = DatabaseMap<Schema>;

/// Loaded live identities and tombstones reserve one global UID namespace.
/// No flattened IdsFile is used as an ownership view (DEC-2501.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UidReservations {
    held: BTreeSet<Uid>,
}
impl UidReservations {
    pub fn from_files(files: &DatabaseMap<IdsFile>) -> Result<Self, String> {
        let mut owners = BTreeMap::new();
        for (db, ids) in files.iter() {
            ids.validate()
                .map_err(|e| format!("database `{}`: {e}", db.shown()))?;
            for uid in ids_uids(ids) {
                if let Some(other) = owners.insert(uid.clone(), db) {
                    return Err(format!(
                        "identity {uid} occurs in both databases `{}` and `{}`",
                        other.shown(),
                        db.shown()
                    ));
                }
            }
        }
        Ok(Self {
            held: owners.into_keys().collect(),
        })
    }
    pub fn contains(&self, uid: &Uid) -> bool {
        self.held.contains(uid)
    }
    pub fn iter(&self) -> impl Iterator<Item = &Uid> {
        self.held.iter()
    }
    pub fn reserve_file(&mut self, ids: &IdsFile) {
        self.held.extend(ids_uids(ids).cloned());
    }
    pub fn allocate(&mut self, kind: UidKind) -> Uid {
        self.allocate_with(|| Uid::generate(kind))
    }
    fn allocate_with(&mut self, mut draw: impl FnMut() -> Uid) -> Uid {
        loop {
            let uid = draw();
            if self.held.insert(uid.clone()) {
                return uid;
            }
        }
    }
}
fn ids_uids(ids: &IdsFile) -> impl Iterator<Item = &Uid> {
    ids.tables
        .keys()
        .chain(ids.columns.keys())
        .chain(ids.roles.keys())
        .chain(ids.types.keys())
        .chain(ids.tombstones.keys())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allocation_redraws_a_foreign_database_uid_and_reserves_the_result() {
        let held: Uid = "t_aaaaaa".parse().unwrap();
        let fresh: Uid = "t_bbbbbb".parse().unwrap();
        let files = DatabaseMap::new([(
            "a".parse().unwrap(),
            IdsFile {
                tables: [(held.clone(), "dbo.t".parse().unwrap())].into(),
                ..Default::default()
            },
        )])
        .unwrap();
        let mut pool = UidReservations::from_files(&files).unwrap();
        let mut draws = [held, fresh.clone()].into_iter();
        assert_eq!(pool.allocate_with(|| draws.next().unwrap()), fresh);
        assert!(pool.contains(&fresh));
    }
}
