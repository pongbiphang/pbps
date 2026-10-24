//! Restorable publication of already validated declarations (DEC-2351.1).

use std::fs::{self, File, Metadata};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use pbps_model::Text;

const BACKUP: &str = ".pbps-publication-backup";

/// A success note means publication finished but backup cleanup needs attention.
/// Identity writes remain the caller's responsibility (DEC-1790.1).
pub(crate) fn publish(
    dir: &Path,
    present: &[PathBuf],
    copies: &[(PathBuf, PathBuf)],
) -> anyhow::Result<Option<String>> {
    let backup = dir.join(BACKUP);
    let mut moved = Vec::new();
    let mut outputs = Vec::new();
    let mut directories = Vec::new();
    let mut owns_backup = false;
    let result = (|| -> anyhow::Result<()> {
        match fs::symlink_metadata(&backup) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "cannot inspect publication backup `{}`",
                        Text::new(backup.display().to_string()).shown()
                    )
                });
            }
            Ok(_) => bail!(
                concat!(
                    "publication backup `{}` already exists; ",
                    "inspect and restore or archive it before retrying"
                ),
                Text::new(backup.display().to_string()).shown()
            ),
        }
        // Read each destination once, before a renamed symlink loses its target.
        // No output scans the old-entry set (#2352).
        let metadata = copies
            .iter()
            .map(|(_, path)| match fs::metadata(path) {
                Ok(m) => Ok(Some(m)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).with_context(|| {
                    format!(
                        "cannot inspect `{}`",
                        Text::new(path.display().to_string()).shown()
                    )
                }),
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        create_directories(dir, &mut directories).with_context(|| {
            format!(
                "cannot create declaration directory `{}`; repair its parent path before retrying",
                Text::new(dir.display().to_string()).shown()
            )
        })?;
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt as _;
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder
        };
        #[cfg(not(unix))]
        let builder = fs::DirBuilder::new();
        builder.create(&backup).with_context(|| {
            format!(
                "cannot create publication backup `{}`",
                Text::new(backup.display().to_string()).shown()
            )
        })?;
        owns_backup = true;
        for path in present {
            let mut saved = backup.join(path.strip_prefix(dir)?);
            // A crashed run must not leave backup YAML for the loader to read.
            saved.as_mut_os_string().push(".bak");
            if let Some(parent) = saved.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "cannot prepare backup `{}`",
                        Text::new(saved.display().to_string()).shown()
                    )
                })?;
            }
            #[cfg(test)]
            tests::fault("rename", path)?;
            fs::rename(path, &saved).with_context(|| {
                format!(
                    "cannot save old declaration `{}`",
                    Text::new(path.display().to_string()).shown()
                )
            })?;
            moved.push((path.clone(), saved));
        }
        for ((source, path), metadata) in copies.iter().zip(&metadata) {
            if let Some(parent) = path.parent() {
                create_directories(parent, &mut directories).with_context(|| {
                    format!(
                        concat!(
                            "cannot create output parent `{}`; ",
                            "repair the directory or link before retrying"
                        ),
                        Text::new(parent.display().to_string()).shown()
                    )
                })?;
            }
            let mut input = File::open(source).with_context(|| {
                format!(
                    "cannot read staged declaration `{}`",
                    Text::new(source.display().to_string()).shown()
                )
            })?;
            let mut output = File::create_new(path).with_context(|| {
                format!(
                    concat!(
                        "cannot create declaration `{}`; ",
                        "move any existing file, directory or link before retrying"
                    ),
                    Text::new(path.display().to_string()).shown()
                )
            })?;
            // Track immediately: metadata and copying can fail with this file present.
            outputs.push(path.clone());
            let access = match metadata {
                Some(m) => m.clone(),
                None => input.metadata()?,
            };
            let preserve = |output: &File, group| {
                set_access(output, &access, group).with_context(|| {
                    format!(
                        concat!(
                            "cannot keep the mode or group of `{}`; ",
                            "restore access to its group and permissions, ",
                            "or deliberately adjust the original file before retrying"
                        ),
                        Text::new(path.display().to_string()).shown()
                    )
                })
            };
            preserve(&output, metadata.is_some())?;
            #[cfg(test)]
            tests::fault("write", path)?;
            io::copy(&mut input, &mut output).with_context(|| {
                format!(
                    "cannot write declaration `{}`",
                    Text::new(path.display().to_string()).shown()
                )
            })?;
            // A write can clear set-ID bits even after chmod succeeded. Restore
            // them without ever broadening access before the content was copied.
            preserve(&output, false)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let failures = restore(&outputs, &moved, &directories, &backup, owns_backup);
        return if failures.is_empty() {
            Err(error.context("the project is unchanged"))
        } else {
            Err(error.context(format!(
                "restoration incomplete:\n{}\nInspect remaining backups at `{}` before retrying",
                failures.join("\n"),
                Text::new(backup.display().to_string()).shown()
            )))
        };
    }
    // Cleanup is after the commit point: some backups may already be deleted.
    // Its failure cannot truthfully be reported as a restored publication.
    match remove_backup(&backup) {
        Ok(()) => Ok(None),
        Err(error) => Ok(Some(format!(
            concat!(
                "declarations were published, but backup `{}` could not be deleted: {error}; ",
                "inspect and remove the leftover before retrying"
            ),
            Text::new(backup.display().to_string()).shown(),
            error = error
        ))),
    }
}

fn set_access(output: &File, metadata: &Metadata, preserve_group: bool) -> io::Result<()> {
    #[cfg(test)]
    tests::fault("metadata", Path::new("metadata"))?;
    #[cfg(unix)]
    if preserve_group {
        use std::os::unix::fs::MetadataExt as _;
        rustix::fs::fchown(
            output,
            None,
            Some(rustix::fs::Gid::from_raw(metadata.gid())),
        )?;
    }
    #[cfg(not(unix))]
    let _ = preserve_group;
    // Changing group can clear mode bits, so set mode afterwards, still empty.
    output.set_permissions(metadata.permissions())
}

fn create_directories(path: &Path, created: &mut Vec<PathBuf>) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    match fs::create_dir(path) {
        Ok(()) => created.push(path.to_owned()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                create_directories(parent, created)?;
            }
            fs::create_dir(path)?;
            created.push(path.to_owned());
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if !fs::metadata(path)?.is_dir() {
                return Err(io::Error::new(io::ErrorKind::NotADirectory, e));
            }
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

fn restore(
    outputs: &[PathBuf],
    moved: &[(PathBuf, PathBuf)],
    directories: &[PathBuf],
    backup: &Path,
    owns_backup: bool,
) -> Vec<String> {
    let mut failures = Vec::new();
    for path in outputs.iter().rev() {
        if let Err(e) = fs::remove_file(path)
            && e.kind() != io::ErrorKind::NotFound
        {
            failures.push(format!(
                "cannot remove new declaration `{}`: {e}",
                Text::new(path.display().to_string()).shown()
            ));
        }
    }
    for (path, saved) in moved.iter().rev() {
        let undo = || -> io::Result<()> {
            #[cfg(test)]
            tests::fault("restore", path)?;
            // Do not silently overwrite a new entry somebody else left there.
            match fs::symlink_metadata(path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => fs::rename(saved, path),
                Err(e) => Err(e),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "destination is occupied",
                )),
            }
        };
        if let Err(e) = undo() {
            failures.push(format!(
                "cannot restore `{}` from `{}`: {e}",
                Text::new(path.display().to_string()).shown(),
                Text::new(saved.display().to_string()).shown()
            ));
        }
    }
    if owns_backup
        && failures.is_empty()
        && let Err(e) = remove_backup(backup)
    {
        failures.push(format!(
            "cannot remove backup `{}`: {e}",
            Text::new(backup.display().to_string()).shown()
        ));
    }
    for path in directories.iter().rev() {
        if let Err(e) = fs::remove_dir(path) {
            failures.push(format!(
                "cannot remove new directory `{}`: {e}",
                Text::new(path.display().to_string()).shown()
            ));
        }
    }
    failures
}

fn remove_backup(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    tests::fault("cleanup", path)?;
    fs::remove_dir_all(path)
}

#[cfg(test)]
#[path = "publication_tests.rs"]
pub(crate) mod tests;
