//! A path as `git` gives it: bytes, relative to the worktree root.
//!
//! ADR-0015 decision 5 parses every path-printing command's `-z` output as
//! bytes, because without it `git` C-quotes a non-ASCII name — `schéma.json`
//! came back quoted from `ls-tree` when it was measured. A `String` here would
//! either lose such a name or invite the lossy conversion that loses it
//! quietly, so the bytes are kept and converted only where a syscall needs
//! them.
//!
//! The validation is the other half. A tree's names are bytes `git` stores,
//! not paths it has checked: `mktree` accepts an entry named `..`, and
//! `ls-tree -r -z` then hands back `../file`, so a snapshot writer that joined
//! such a name to its directory would write outside it before any guard of
//! step 1 ran.

use std::fmt;

/// A worktree-relative path that has passed the component rules below.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(Vec<u8>);

/// Why a name from a tree, a listing or the page is not a path this UI will
/// act on. Each variant names the component so the page can show it.
#[derive(Debug, PartialEq, Eq)]
pub enum BadPath {
    Empty,
    Absolute,
    EmptyComponent,
    CurrentDirectory,
    ParentDirectory,
    Nul,
    /// Reserved on Windows, where `\` is a separator and a device name is a
    /// file whatever directory it is opened from.
    WindowsReserved(String),
}

impl fmt::Display for BadPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "the path is empty"),
            Self::Absolute => write!(f, "the path is absolute"),
            Self::EmptyComponent => write!(f, "the path has an empty component"),
            Self::CurrentDirectory => write!(f, "the path has a `.` component"),
            Self::ParentDirectory => write!(f, "the path has a `..` component"),
            Self::Nul => write!(f, "the path contains a NUL byte"),
            Self::WindowsReserved(name) => {
                write!(f, "`{name}` is a reserved name on Windows")
            }
        }
    }
}

impl RepoPath {
    /// The only way to make one. Every name that reaches a syscall, a
    /// `--cacheinfo` entry or a snapshot directory comes through here.
    pub fn new(bytes: &[u8]) -> Result<Self, BadPath> {
        if bytes.is_empty() {
            return Err(BadPath::Empty);
        }
        if bytes.contains(&0) {
            return Err(BadPath::Nul);
        }
        if bytes[0] == b'/' {
            return Err(BadPath::Absolute);
        }
        for component in bytes.split(|b| *b == b'/') {
            match component {
                b"" => return Err(BadPath::EmptyComponent),
                b"." => return Err(BadPath::CurrentDirectory),
                b".." => return Err(BadPath::ParentDirectory),
                _ => {}
            }
            // Checked on every platform, not only when running on Windows: a
            // repository is shared, and a path this UI commits on Linux is one
            // a colleague checks out on Windows, where `a\b` is two components
            // and `NUL` is a device whatever directory it sits in.
            if component.contains(&b'\\') {
                return Err(BadPath::WindowsReserved(
                    String::from_utf8_lossy(component).into_owned(),
                ));
            }
            if is_windows_device(component) {
                return Err(BadPath::WindowsReserved(
                    String::from_utf8_lossy(component).into_owned(),
                ));
            }
        }
        Ok(Self(bytes.to_vec()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.0.split(|b| *b == b'/')
    }

    /// The path as `git` will be given it, after `--`. Compose refuses a
    /// declaration whose name is not UTF-8 rather than guessing: the command
    /// line is the one place the bytes cannot be kept.
    pub fn to_text(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    /// True when this path is the given directory or lies under it. Used to
    /// keep the listing inside the declarations directory and to refuse a
    /// `doctor` answer that resolves outside the project.
    pub fn is_under(&self, directory: &[u8]) -> bool {
        if directory.is_empty() || directory == b"." {
            return true;
        }
        let directory = directory.strip_suffix(b"/").unwrap_or(directory);
        self.0.starts_with(directory) && matches!(self.0.get(directory.len()), Some(b'/'))
    }

    pub fn extension_is_declaration(&self) -> bool {
        let last = self.0.rsplit(|b| *b == b'/').next().unwrap_or_default();
        let lower: Vec<u8> = last.to_ascii_lowercase();
        lower.ends_with(b".yml") || lower.ends_with(b".yaml")
    }
}

/// `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`, with or without
/// an extension, in any case.
fn is_windows_device(component: &[u8]) -> bool {
    let stem = match component.iter().position(|b| *b == b'.') {
        Some(dot) => &component[..dot],
        None => component,
    };
    let stem = stem.to_ascii_uppercase();
    matches!(stem.as_slice(), b"CON" | b"PRN" | b"AUX" | b"NUL")
        || matches!(stem.split_first(), Some((b'C', rest)) if rest.starts_with(b"OM") && is_device_digit(&rest[2..]))
        || matches!(stem.split_first(), Some((b'L', rest)) if rest.starts_with(b"PT") && is_device_digit(&rest[2..]))
}

fn is_device_digit(rest: &[u8]) -> bool {
    matches!(rest, [d] if d.is_ascii_digit() && *d != b'0')
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(&self.0))
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(&self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tree_entry_named_dotdot_is_refused_before_it_can_be_joined_to_a_directory() {
        // The measured case: a tree holding a subtree named `..` is accepted
        // by `mktree`, and `ls-tree -r` prints `../file`. A snapshot writer
        // that joined that to its directory would write into the repository.
        assert_eq!(RepoPath::new(b"../file"), Err(BadPath::ParentDirectory));
        assert_eq!(RepoPath::new(b"a/../b"), Err(BadPath::ParentDirectory));
        assert_eq!(RepoPath::new(b".."), Err(BadPath::ParentDirectory));
    }

    #[test]
    fn the_shapes_that_escape_a_directory_are_each_refused_by_name() {
        assert_eq!(RepoPath::new(b""), Err(BadPath::Empty));
        assert_eq!(RepoPath::new(b"/etc/passwd"), Err(BadPath::Absolute));
        assert_eq!(RepoPath::new(b"a//b"), Err(BadPath::EmptyComponent));
        assert_eq!(RepoPath::new(b"a/./b"), Err(BadPath::CurrentDirectory));
        assert_eq!(RepoPath::new(b"a/b\0c"), Err(BadPath::Nul));
        assert!(matches!(
            RepoPath::new(b"a\\b"),
            Err(BadPath::WindowsReserved(_))
        ));
        assert!(matches!(
            RepoPath::new(b"schema/NUL.yml"),
            Err(BadPath::WindowsReserved(_))
        ));
        assert!(matches!(
            RepoPath::new(b"schema/com1"),
            Err(BadPath::WindowsReserved(_))
        ));
    }

    #[test]
    fn an_ordinary_declaration_path_is_accepted_whatever_its_bytes_spell() {
        // Not UTF-8, and still a path: `ls-tree -z` hands these back and the
        // compose has to be able to carry them as far as the refusal that
        // names them.
        let latin1 = RepoPath::new(b"schema/sch\xe9ma.yml").expect("bytes are a path");
        assert!(latin1.to_text().is_none());
        assert!(latin1.extension_is_declaration());
        let ordinary = RepoPath::new(b"schema/customer.yml").unwrap();
        assert_eq!(ordinary.to_text(), Some("schema/customer.yml"));
        assert!(ordinary.is_under(b"schema"));
        assert!(!ordinary.is_under(b"schemas"));
        assert!(ordinary.is_under(b"."));
    }

    #[test]
    fn only_the_extensions_the_loader_collects_count_as_declarations() {
        // `pbps_load` walks the declarations directory and collects `.yml` and
        // `.yaml`; a README or an editor's stray file beside them is neither
        // laid over the snapshot nor committed with the intent.
        for name in ["a.yml", "a.YAML", "dir/b.yaml"] {
            assert!(
                RepoPath::new(name.as_bytes())
                    .unwrap()
                    .extension_is_declaration()
            );
        }
        for name in ["README", "a.yml.swp", "a.json", "yml"] {
            assert!(
                !RepoPath::new(name.as_bytes())
                    .unwrap()
                    .extension_is_declaration()
            );
        }
    }

    #[test]
    fn a_prefix_that_is_not_a_directory_boundary_is_not_under_the_directory() {
        // `schema-old/x.yml` starts with `schema` and is not under it; the
        // listing's pathspec is the declarations directory and the ids file,
        // and a filter that missed this would commit a neighbour's file.
        let sibling = RepoPath::new(b"schema-old/x.yml").unwrap();
        assert!(!sibling.is_under(b"schema"));
        assert!(sibling.is_under(b"schema-old"));
        assert!(!RepoPath::new(b"schema").unwrap().is_under(b"schema"));
    }
}
