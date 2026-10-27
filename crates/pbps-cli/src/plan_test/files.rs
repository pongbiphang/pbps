//! The manifest and the files it names: contained, read once, hashed.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use pbps_config::test_manifest::{Entries, TestManifest};
use sha2::{Digest as _, Sha256};

use super::{Change, ConsumedFile, Problem, Refusal, Revision, Role};

/// One input as it was read. `source` is the path that was asked for and
/// `resolved` what it named then; both are kept so that the recheck can tell
/// a symlink moved from a file edited.
struct Read {
    resolved: PathBuf,
    project_path: String,
    text: String,
    sha256: String,
}

/// A fixture's or assertion's SQL exactly as read, with the facts that
/// identify it.
pub(crate) struct FrozenSql {
    pub name: String,
    pub text: String,
    pub file: ConsumedFile,
    source: PathBuf,
    resolved: PathBuf,
}

/// Every input the manifest names, read once. The runner executes
/// [`FrozenSql::text`] and never reopens the file.
pub(crate) struct Frozen {
    pub manifest: TestManifest,
    pub manifest_file: ConsumedFile,
    /// In manifest order, which is execution order within a pin.
    pub fixtures: Vec<FrozenSql>,
    pub assertions: Vec<FrozenSql>,
    root: PathBuf,
    manifest_source: PathBuf,
    manifest_resolved: PathBuf,
}

/// Reads `path`, which must name a regular file inside `root` (canonical).
///
/// The containment test is on the *resolved* path: a lexical `..` check passes
/// `a/../b` and a symlink inside the project can lead anywhere.
fn read(root: &Path, path: &Path) -> Result<Read, Problem> {
    let unreadable = |e: std::io::Error| Problem::Unreadable(e.to_string());
    let resolved = path.canonicalize().map_err(|e| {
        if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) {
            Problem::Missing
        } else {
            unreadable(e)
        }
    })?;
    let inside = resolved
        .strip_prefix(root)
        .map_err(|_| Problem::OutsideProject)?;
    let project_path = inside
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    match std::fs::metadata(&resolved) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(Problem::NotRegular),
        Err(e) if e.kind() == ErrorKind::NotFound => return Err(Problem::Missing),
        Err(e) => return Err(unreadable(e)),
    }
    let bytes = std::fs::read(&resolved).map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            Problem::Missing
        } else {
            unreadable(e)
        }
    })?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let text = String::from_utf8(bytes).map_err(|_| Problem::NotUtf8)?;
    Ok(Read {
        resolved,
        project_path,
        text,
        sha256,
    })
}

fn input(role: Role, name: Option<&str>, path: &str, problem: Problem) -> Refusal {
    Refusal::Input {
        role,
        name: name.map(str::to_owned),
        path: path.to_owned(),
        problem,
    }
}

/// Reads the manifest at `manifest_path` and every file it names, inside the
/// project rooted at `project_root`, and returns every refusal found rather
/// than the first: a bad fixture does not hide a missing assertion.
///
/// Entry paths are relative to the manifest's directory. The manifest itself
/// must also be inside the project, or its own path would have no project
/// name to report.
pub(crate) fn freeze(project_root: &Path, manifest_path: &Path) -> Result<Frozen, Vec<Refusal>> {
    let shown = manifest_path.to_string_lossy();
    let root = project_root.canonicalize().map_err(|e| {
        vec![input(
            Role::Manifest,
            None,
            &project_root.to_string_lossy(),
            Problem::Unreadable(format!("the project root: {e}")),
        )]
    })?;
    let manifest_read = read(&root, manifest_path)
        .map_err(|problem| vec![input(Role::Manifest, None, &shown, problem)])?;
    let manifest = TestManifest::parse(&manifest_read.text).map_err(|errors| {
        errors
            .into_iter()
            .map(Refusal::Manifest)
            .collect::<Vec<_>>()
    })?;
    let manifest_file = ConsumedFile {
        path: manifest_read.project_path.clone(),
        revision: Revision::Working,
        role: Role::Manifest,
        sha256: manifest_read.sha256.clone(),
    };
    // Resolved, so a manifest reached through a symlink names the directory
    // its entries are relative to the same way the recheck will.
    let dir = manifest_read
        .resolved
        .parent()
        .expect("a file has a parent")
        .to_owned();

    let mut refusals = Vec::new();
    let mut load = |role, list, entries: Vec<(&str, &str)>| {
        let mut seen: std::collections::BTreeMap<PathBuf, &str> = Default::default();
        let mut out = Vec::new();
        for (name, spelling) in entries {
            let source = dir.join(spelling);
            let read = match read(&root, &source) {
                Ok(read) => read,
                Err(problem) => {
                    refusals.push(input(role, Some(name), spelling, problem));
                    continue;
                }
            };
            if let Some(first) = seen.insert(read.resolved.clone(), name) {
                refusals.push(Refusal::DuplicateFile {
                    list,
                    first: first.to_owned(),
                    second: name.to_owned(),
                    path: read.project_path,
                });
                continue;
            }
            out.push(FrozenSql {
                name: name.to_owned(),
                file: ConsumedFile {
                    path: read.project_path,
                    revision: Revision::Working,
                    role,
                    sha256: read.sha256,
                },
                text: read.text,
                source,
                resolved: read.resolved,
            });
        }
        out
    };
    let fixtures = load(
        Role::Fixture,
        Entries::Fixtures,
        manifest
            .fixtures
            .iter()
            .map(|f| (f.name.as_str(), f.file.as_str()))
            .collect(),
    );
    let assertions = load(
        Role::Assertion,
        Entries::Assertions,
        manifest
            .assertions
            .iter()
            .map(|a| (a.name.as_str(), a.file.as_str()))
            .collect(),
    );
    if !refusals.is_empty() {
        return Err(refusals);
    }
    Ok(Frozen {
        manifest,
        manifest_file,
        fixtures,
        assertions,
        root,
        manifest_source: manifest_path.to_owned(),
        manifest_resolved: manifest_read.resolved,
    })
}

impl Frozen {
    /// Every consumed file, sorted so the same inputs always list the same
    /// way.
    pub(crate) fn consumed(&self) -> Vec<ConsumedFile> {
        let mut all: Vec<_> = std::iter::once(&self.manifest_file)
            .chain(self.fixtures.iter().map(|f| &f.file))
            .chain(self.assertions.iter().map(|a| &a.file))
            .cloned()
            .collect();
        all.sort();
        all
    }

    /// Reads every input again and refuses if any is gone, unreadable, moved
    /// to another file or has other bytes. Run it before a result is
    /// published: a hash taken at the start does not describe a file edited
    /// since, and nothing here mixes the old identity with new content.
    ///
    /// The membership checked is the manifest's own. The candidate's
    /// declaration set is rechecked by its loader.
    pub(crate) fn reverify(&self) -> Result<(), Vec<Refusal>> {
        let recheck = |file: &ConsumedFile, source: &Path, resolved: &Path| {
            let change = match read(&self.root, source) {
                Err(problem) => Change::Unusable(problem),
                Ok(now) if now.resolved != resolved => Change::Retargeted,
                Ok(now) if now.sha256 != file.sha256 => Change::Edited,
                Ok(_) => return None,
            };
            Some(Refusal::Changed {
                path: file.path.clone(),
                change,
            })
        };
        let refusals: Vec<_> = std::iter::once(recheck(
            &self.manifest_file,
            &self.manifest_source,
            &self.manifest_resolved,
        ))
        .chain(
            self.fixtures
                .iter()
                .chain(&self.assertions)
                .map(|f| recheck(&f.file, &f.source, &f.resolved)),
        )
        .flatten()
        .collect();
        if refusals.is_empty() {
            Ok(())
        } else {
            Err(refusals)
        }
    }
}
