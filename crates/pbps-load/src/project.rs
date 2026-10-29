//! Per-database declarations and ownership stay separate (DEC-2501.1).
use crate::{LoadError, Loaded, load_schema_dir};
use pbps_model::{DatabaseMap, DatabaseName, IdsFile, ProjectSchemas, UidReservations};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum ProjectLoadError {
    #[error("cannot read `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid database layout: {0}")]
    Layout(String),
    #[error("database `{}` has {} declaration problem(s)", .database.shown(), .errors.len())]
    Declarations {
        database: DatabaseName,
        errors: Vec<LoadError>,
    },
    #[error("identity file `{path}` is invalid: {message}")]
    Identities { path: PathBuf, message: String },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedDatabases {
    loaded: DatabaseMap<Loaded>,
    ids: DatabaseMap<IdsFile>,
    reservations: UidReservations,
}
impl LoadedDatabases {
    pub fn loaded(&self) -> &DatabaseMap<Loaded> {
        &self.loaded
    }
    pub fn ids(&self) -> &DatabaseMap<IdsFile> {
        &self.ids
    }
    pub fn schemas(&self) -> ProjectSchemas {
        self.loaded.map(|_, loaded| loaded.schema.clone())
    }
    pub fn reservations(&mut self) -> &mut UidReservations {
        &mut self.reservations
    }
}
fn io(path: &Path, source: std::io::Error) -> ProjectLoadError {
    ProjectLoadError::Io {
        path: path.to_owned(),
        source,
    }
}
fn canonical(path: &Path) -> Result<PathBuf, ProjectLoadError> {
    std::fs::canonicalize(path).map_err(|e| io(path, e))
}

/// Load `schema/<database>/` using the same single-schema loader for each DB.
/// `schema.ids.json` is optional only when genuinely absent, never unreadable.
pub fn load_database_dirs(
    directories: &DatabaseMap<PathBuf>,
) -> Result<LoadedDatabases, ProjectLoadError> {
    let (_, first) = directories.iter().next().expect("DatabaseMap is nonempty");
    let parent = first.parent().ok_or_else(|| {
        ProjectLoadError::Layout("a database directory must have a parent".into())
    })?;
    let root = canonical(parent)?;
    let names: BTreeSet<_> = directories.keys().map(DatabaseName::raw).collect();
    // A loose declaration or unlisted directory would otherwise disappear
    // just because the new layout was selected.
    for entry in std::fs::read_dir(&root).map_err(|e| io(&root, e))? {
        let entry = entry.map_err(|e| io(&root, e))?;
        let path = entry.path();
        let name = entry.file_name();
        let wanted = name.to_str().is_some_and(|name| names.contains(name));
        let kind = entry.file_type().map_err(|e| io(&path, e))?;
        if !wanted
            && (kind.is_dir()
                || kind.is_symlink()
                || path
                    .extension()
                    .is_some_and(|ext| ext == "yml" || ext == "yaml"))
        {
            return Err(ProjectLoadError::Layout(format!(
                "undeclared database directory or flat declaration at `{}`",
                path.display()
            )));
        }
    }
    let mut seen = BTreeSet::new();
    for (db, path) in directories.iter() {
        if path.file_name() != Some(std::ffi::OsStr::new(db.raw()))
            || canonical(path.parent().unwrap_or(Path::new(".")))? != root
        {
            return Err(ProjectLoadError::Layout(format!(
                "database `{}` must occupy its own directory beneath the same declarations root",
                db.shown()
            )));
        }
        let real = canonical(path)?;
        if real.parent() != Some(root.as_path()) || !seen.insert(real) {
            return Err(ProjectLoadError::Layout(format!(
                "database `{}` escapes the declarations root or aliases another database",
                db.shown()
            )));
        }
    }
    // Finish all identity reads before an allocator can be obtained.
    let ids = directories.try_map(|_, path| read_ids(&path.join("schema.ids.json")))?;
    let reservations = UidReservations::from_files(&ids).map_err(ProjectLoadError::Layout)?;
    let loaded = directories.try_map(|db, path| {
        load_schema_dir(path).map_err(|errors| ProjectLoadError::Declarations {
            database: db.clone(),
            errors,
        })
    })?;
    Ok(LoadedDatabases {
        loaded,
        ids,
        reservations,
    })
}
fn read_ids(path: &Path) -> Result<IdsFile, ProjectLoadError> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(IdsFile::default()),
        Err(e) => return Err(io(path, e)),
        Ok(meta) if !meta.is_file() => {
            return Err(ProjectLoadError::Layout(format!(
                "per-database identity path `{}` must be a regular file",
                path.display()
            )));
        }
        Ok(_) => {}
    }
    let text = std::fs::read_to_string(path).map_err(|e| io(path, e))?;
    serde_json::from_str(&text).map_err(|e| ProjectLoadError::Identities {
        path: path.to_owned(),
        message: e.to_string(),
    })
}
