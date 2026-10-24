//! Filesystem identity for CLI input and output paths.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// Resolves `path` to an absolute path with no `..` left in it.
///
/// `Project::discover` walks this path's ancestors, and a lexical `..` puts
/// directories that are not ancestors on that walk: `pbps --project ../sibling
/// init` run from inside a project would refuse because it "is already inside"
/// the very project it is escaping. `std::path::absolute` does not help — on
/// Unix it keeps `..` on purpose, because dropping it lexically names the wrong
/// directory whenever a component is a symlink. So the deepest existing prefix
/// is canonicalized, which resolves `..` against the real filesystem, and the
/// part that does not exist yet is appended to it.
pub(crate) fn absolute(path: &Path) -> anyhow::Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .context("cannot determine the current directory")?
            .join(path)
    };

    let mut existing = joined.as_path();
    let mut missing = Vec::new();
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            return Ok(missing.iter().rev().fold(resolved, |mut out, name| {
                out.push(name);
                out
            }));
        }
        // `file_name` is None for a path ending in `..`, which can only happen
        // when its parent does not exist either. Nothing can be resolved then,
        // so hand back the joined path rather than invent a normalization.
        let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
            return Ok(joined);
        };
        missing.push(name.to_owned());
        existing = parent;
    }
}

/// Existing aliases are compared by filesystem identity; missing destinations
/// still need a spelling comparison because planning can create the ids file.
pub(crate) fn same_file(a: &Path, b: &Path) -> anyhow::Result<bool> {
    if absolute(a)? == absolute(b)? {
        return Ok(true);
    }
    let metadata = |path: &Path| -> anyhow::Result<Option<std::fs::Metadata>> {
        match std::fs::metadata(path) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("cannot inspect `{}`", path.display()))
            }
        }
    };
    // Unreadable is not absent, including on platforms without a portable
    // hardlink identity API (DEC-1800.1).
    let (a, b) = (metadata(a)?, metadata(b)?);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(matches!((a, b), (Some(a), Some(b)) if a.dev() == b.dev() && a.ino() == b.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        Ok(false)
    }
}

/// Every existing destination must be one of the entries removed before copying.
/// A lowercase-extension listing can miss an uppercase alias on a case-insensitive
/// filesystem; spelling membership would also reject aliases it did list (#2311).
pub(crate) fn require_removable_output(output: &Path, removal: &[PathBuf]) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(output) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot inspect `{}`",
                    pbps_model::Text::new(output.display().to_string()).shown()
                )
            });
        }
    }
    for old in removal {
        if same_file(output, old)? {
            return Ok(());
        }
    }
    anyhow::bail!(
        "`{}` already exists outside the declaration replacement set; move it before retrying",
        pbps_model::Text::new(output.display().to_string()).shown()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);

    impl Directory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("pbps-paths-{:016x}", rand::random::<u64>()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preflight_refuses_an_uppercase_extension_alias_omitted_from_removal() {
        let dir = Directory::new();
        let old = dir.0.join("custom.yml");
        let uppercase = dir.0.join("country.YML");
        let output = dir.0.join("country.yml");
        std::fs::write(&old, "keep old declaration").unwrap();
        std::fs::write(&uppercase, "keep uppercase entry").unwrap();
        // Simulate case-insensitive lookup with a symlink. Exclude that simulator
        // entry from the removal list: the real filesystem has only country.YML,
        // which schema_files omits. The lane's filesystem is case-sensitive.
        if !output.exists() {
            std::os::unix::fs::symlink("country.YML", &output).unwrap();
        }
        let present: Vec<_> = pbps_load::schema_files(&dir.0)
            .unwrap()
            .into_iter()
            .filter(|path| path != &output)
            .collect();
        assert_eq!(present.as_slice(), std::slice::from_ref(&old));
        let publish = || -> anyhow::Result<()> {
            require_removable_output(&output, &present)?;
            for path in &present {
                std::fs::remove_file(path)?;
            }
            std::fs::File::create_new(&output)?;
            Ok(())
        };
        let result = publish();
        assert!(
            old.is_file(),
            "unlisted output failure deleted a declaration"
        );
        assert_eq!(std::fs::read(&old).unwrap(), b"keep old declaration");
        assert_eq!(std::fs::read(&uppercase).unwrap(), b"keep uppercase entry");
        assert!(result.unwrap_err().to_string().contains("move it"));
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preflight_admits_listed_file_identities_without_spelling_membership() {
        let dir = Directory::new();
        let old = dir.0.join("Country.yml");
        let output = dir.0.join("country.yml");
        std::fs::write(&old, "old declaration").unwrap();
        if !output.exists() {
            std::os::unix::fs::symlink("Country.yml", &output).unwrap();
        }
        require_removable_output(&output, std::slice::from_ref(&old)).unwrap();
        let hardlink = dir.0.join("hard.yml");
        std::fs::hard_link(&old, &hardlink).unwrap();
        require_removable_output(&hardlink, &[old]).unwrap();
        require_removable_output(&dir.0.join("absent.yml"), &[]).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preflight_distinguishes_unlisted_dangling_and_unreadable_entries() {
        let dir = Directory::new();
        let regular = dir.0.join("country.yml");
        let dangling = dir.0.join("dangling.yml");
        let unreadable = dir.0.join("loop.yml");
        std::fs::write(&regular, "not scheduled for removal").unwrap();
        std::os::unix::fs::symlink("missing", &dangling).unwrap();
        std::os::unix::fs::symlink("loop.yml", &unreadable).unwrap();
        for path in [&regular, &dangling] {
            let error = require_removable_output(path, &[]).unwrap_err();
            assert!(error.to_string().contains("move it"));
            // A listed dangling entry is removable even though metadata follows
            // its missing target; symlink_metadata establishes its presence.
            require_removable_output(path, std::slice::from_ref(path)).unwrap();
        }
        let error = require_removable_output(&unreadable, &[regular]).unwrap_err();
        assert!(error.to_string().contains("cannot inspect"));
    }

    #[test]
    fn equivalent_spellings_match_before_and_after_creation() {
        let dir = Directory::new();
        std::fs::create_dir(dir.0.join("nested")).unwrap();
        let file = dir.0.join("x");
        for exists in [false, true] {
            if exists {
                std::fs::write(&file, "input").unwrap();
            }
            for alias in ["x", "./x", "nested/../x"] {
                assert!(same_file(&file, &dir.0.join(alias)).unwrap(), "{alias}");
            }
        }
        assert!(same_file(&dir.0.join("missing/x"), &dir.0.join("missing/./x")).unwrap());
        assert!(!same_file(&dir.0.join("missing/x"), &dir.0.join("missing/y")).unwrap());
    }

    #[test]
    fn equal_bytes_do_not_make_distinct_files_aliases() {
        let dir = Directory::new();
        for name in ["x", "y"] {
            std::fs::write(dir.0.join(name), "same bytes").unwrap();
        }
        assert!(!same_file(&dir.0.join("x"), &dir.0.join("y")).unwrap());
        assert!(!same_file(&dir.0.join("x"), &dir.0.join("missing")).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_aliases_match_without_lexically_erasing_symlink_parents() {
        use std::os::unix::fs::symlink;
        let dir = Directory::new();
        let input = dir.0.join("input");
        std::fs::write(&input, "input").unwrap();
        symlink(&input, dir.0.join("sym")).unwrap();
        std::fs::hard_link(&input, dir.0.join("hard")).unwrap();
        assert!(same_file(&input, &dir.0.join("sym")).unwrap());
        assert!(same_file(&input, &dir.0.join("hard")).unwrap());
        std::fs::create_dir_all(dir.0.join("real/child")).unwrap();
        symlink(dir.0.join("real/child"), dir.0.join("link")).unwrap();
        // `link/..` is `real`, not the directory containing `link`.
        let spelling = dir.0.join("link/../new");
        assert!(same_file(&spelling, &dir.0.join("real/new")).unwrap());
        assert!(!same_file(&spelling, &dir.0.join("new")).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn an_uninspectable_path_is_an_error_not_a_distinct_file() {
        let dir = Directory::new();
        std::os::unix::fs::symlink("loop", dir.0.join("loop")).unwrap();
        assert!(same_file(&dir.0.join("loop"), &dir.0.join("missing")).is_err());
    }
}
