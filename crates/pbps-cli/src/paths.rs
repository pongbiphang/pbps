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
    #[cfg(test)]
    inspections::CALLS.with(|n| n.set(n.get() + 1));
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

/// An existing declaration candidate is also a publication destination. Reject
/// foreign aliases before decoding it (DEC-2447.3), and recheck all destinations
/// before publication; protected inputs remain protected under force (DEC-2351.1).
pub(crate) fn refuse_declaration_path(
    path: &Path,
    root: &Path,
    protected: &[PathBuf],
) -> anyhow::Result<()> {
    for input in protected {
        anyhow::ensure!(
            !same_file(path, input)?,
            "`{}` aliases protected project input `{}`",
            pbps_model::Text::new(path.display().to_string()).shown(),
            pbps_model::Text::new(input.display().to_string()).shown()
        );
    }
    anyhow::ensure!(
        absolute(path)?.starts_with(root),
        "`{}` resolves outside the declaration directory",
        pbps_model::Text::new(path.display().to_string()).shown()
    );
    Ok(())
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

#[cfg(test)]
pub(crate) mod inspections {
    thread_local! {
        pub(crate) static CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
}
