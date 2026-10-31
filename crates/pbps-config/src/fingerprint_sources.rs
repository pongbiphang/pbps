//! Comparison addresses only: provisioning and historical config parsing need
//! no key files. Keep runtime Project roots and source paths unchanged.

use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::{ConfigError, Environment};

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Variable(String),
    File(PathBuf),
}

pub(super) fn validate(
    environments: &BTreeMap<String, Environment>,
    config: &Path,
) -> Result<(), ConfigError> {
    validate_with_base(environments, config, std::env::current_dir)
}

fn validate_with_base(
    environments: &BTreeMap<String, Environment>,
    config: &Path,
    base: impl FnOnce() -> io::Result<PathBuf>,
) -> Result<(), ConfigError> {
    let root = config.parent().unwrap_or(Path::new("."));
    let mut sources: BTreeMap<String, Source> = environments
        .iter()
        .filter_map(|(name, env)| {
            let source = if let Some(variable) = &env.fingerprint_key_env {
                Source::Variable(variable.clone())
            } else if let Some(file) = &env.fingerprint_key_file {
                Source::File(without_leading_dot(&root.join(file)))
            } else {
                return None;
            };
            Some((name.clone(), source))
        })
        .collect();
    // Exact repeated addresses need no ambient context, even if a later pair
    // would require one. This also avoids a cwd lookup for known refusals.
    check_sources(&sources)?;
    let files: Vec<_> = sources
        .values()
        .filter_map(|source| match source {
            Source::File(path) => Some(path),
            Source::Variable(_) => None,
        })
        .collect();
    // Ordinary relative addresses share the same root. Windows rooted or
    // drive-relative forms can instead replace part of that root, so they
    // must not silently read as independent sources without a usable base.
    let needs_base = files.len() > 1
        && ((files.iter().any(|p| p.is_absolute()) && files.iter().any(|p| p.is_relative()))
            || files.iter().any(|p| {
                p.is_relative()
                    && (p.has_root() || matches!(p.components().next(), Some(Component::Prefix(_))))
            }));
    if !needs_base {
        return Ok(());
    }
    let base = base().map_err(|reason| ConfigError::FingerprintKeyPathContext { reason })?;
    if !base.is_absolute() {
        return Err(context_error());
    }
    for source in sources.values_mut() {
        if let Source::File(path) = source
            && path.is_relative()
        {
            let address = base.join(&*path);
            // A different drive's relative prefix may still need that drive's
            // ambient directory. Refuse instead of guessing its identity.
            if !address.is_absolute() {
                return Err(context_error());
            }
            *path = without_leading_dot(&address);
        }
    }
    check_sources(&sources)
}

fn check_sources(sources: &BTreeMap<String, Source>) -> Result<(), ConfigError> {
    let mut seen = BTreeMap::new();
    for (name, source) in sources {
        if let Some(first) = seen.insert(source, name) {
            return Err(ConfigError::SharedFingerprintKeySource {
                first: first.clone(),
                second: name.clone(),
                kind: match source {
                    Source::Variable(_) => "variable",
                    Source::File(_) => "file",
                },
            });
        }
    }
    Ok(())
}

fn context_error() -> ConfigError {
    ConfigError::FingerprintKeyPathContext {
        reason: io::Error::new(
            io::ErrorKind::InvalidInput,
            "an absolute comparison base is required",
        ),
    }
}

fn without_leading_dot(path: &Path) -> PathBuf {
    let mut components = path.components();
    if matches!(components.next(), Some(Component::CurDir)) {
        components.as_path().to_owned()
    } else {
        path.to_owned()
    }
    // Path equality already compares native components, ignoring interior dots
    // and repeated separators. Never cancel ParentDir across a possible symlink
    // or round-trip through lossy display text (DEC-964.1).
}

#[cfg(test)]
mod tests;
