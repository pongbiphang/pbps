//! Where each identity-bearing declaration was read from (#2600).
//!
//! The identity tree mirrors the YAML tree: `schema/<rel>/<name>.yml` is
//! recorded in `<ids_dir>/<rel>/<name>.json` (design-2600 §3.2). The loader
//! already holds every declaration's path for its duplicate checks and
//! diagnostics ([`Loaded::sources`]); this is the view of it the tree needs,
//! taken once from the same successful load rather than rebuilt from a second
//! walk of the directory that could disagree with it.

use std::collections::BTreeMap;
use std::path::Path;

use pbps_model::diagnostic::Declaration;
use pbps_model::{ColumnName, TableName};

use crate::Loaded;

/// One declaration file that owns an identity file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredFile {
    /// The YAML path relative to the declarations directory, `/`-separated
    /// whatever the platform, so the same project gives the same string on
    /// every checkout.
    pub path: String,
    /// A table's columns in the order the YAML gives them, which is the order
    /// the tree's `columns` section keeps; empty for any other kind.
    pub columns: Vec<ColumnName>,
}

/// The declarations that have an identity file, by what they declare.
///
/// Modules and dependencies carry no uid, so they are not here. A partition
/// child is not either: it is declared in its parent's file and recorded in
/// its parent's identity file, so it appears only in [`children`].
///
/// [`children`]: DeclaredPaths::children
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeclaredPaths {
    /// A table, role, type or extension, to the file that declares it.
    pub files: BTreeMap<Declaration, DeclaredFile>,
    /// A partition child, to the parent table whose file declares it.
    pub children: BTreeMap<TableName, TableName>,
}

/// Why a declaration's path cannot name an identity file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeclaredPathError {
    #[error("`{path}` is not inside the declarations directory `{dir}`")]
    Outside { path: String, dir: String },
    /// Refused, as the plan-test loader refuses it, because a name that is not
    /// text cannot be mirrored byte for byte into a JSON path.
    #[error("the declaration path `{path}` is not valid UTF-8")]
    NotUtf8 { path: String },
}

impl Loaded {
    /// The declaration paths the identity tree mirrors, relative to
    /// `schema_dir`.
    ///
    /// `schema_dir` is the directory the load was given. For a load of files
    /// already in memory ([`crate::load_schema_files`]), whose paths are
    /// relative to begin with, pass an empty path.
    pub fn declared_paths(&self, schema_dir: &Path) -> Result<DeclaredPaths, DeclaredPathError> {
        let mut out = DeclaredPaths::default();
        for (declaration, source) in &self.sources {
            let kind_has_ids = match declaration {
                Declaration::Table(_)
                | Declaration::Role(_)
                | Declaration::Type(_)
                | Declaration::Extension(_) => true,
                Declaration::Module(_) => false,
            };
            if !kind_has_ids {
                continue;
            }
            if let Declaration::Table(name) = declaration
                && let Some(of) = self
                    .schema
                    .tables
                    .get(name)
                    .and_then(|t| t.partition_of.as_ref())
            {
                out.children.insert(name.clone(), of.parent.clone());
                continue;
            }
            let shown = || source.display().to_string();
            let rel = source
                .strip_prefix(schema_dir)
                .map_err(|_| DeclaredPathError::Outside {
                    path: shown(),
                    dir: schema_dir.display().to_string(),
                })?;
            let parts: Option<Vec<&str>> = rel.iter().map(|part| part.to_str()).collect();
            let path = parts
                .ok_or_else(|| DeclaredPathError::NotUtf8 { path: shown() })?
                .join("/");
            let columns = match declaration {
                Declaration::Table(name) => self
                    .schema
                    .tables
                    .get(name)
                    .map(|t| t.columns.keys().cloned().collect())
                    .unwrap_or_default(),
                Declaration::Role(_)
                | Declaration::Type(_)
                | Declaration::Extension(_)
                | Declaration::Module(_) => Vec::new(),
            };
            out.files
                .insert(declaration.clone(), DeclaredFile { path, columns });
        }
        Ok(out)
    }
}
