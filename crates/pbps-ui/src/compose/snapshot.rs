//! Writing a tree out as files, for the two places ADR-0015 decision 5 needs
//! one: the snapshot the intent command runs in, and the snapshot `pbps
//! validate` is run on.
//!
//! Written by the UI rather than by `git checkout-index --prefix` or `git
//! archive`, because both run the `smudge` program of a path's `filter`
//! attribute and `cat-file` runs none (**measured**: under `*.json
//! filter=up` with a `smudge` that left a marker, `checkout-index -a
//! --prefix` and `archive` both left it, `cat-file --batch` and `cat-file
//! blob` did not). A snapshot with a smudge program's output in it is not the
//! tree the commit will hold.
//!
//! Only the project's subtree: a monorepo's other gigabytes are not the
//! project's inputs, and `ls-tree` scoped by a path still names each entry
//! from the worktree root (**measured**), so the names need no adjusting.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use super::fsx::Dir;
use super::git::Git;
use super::repo_path::{BadPath, RepoPath};

/// One regular-file entry of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub mode: u32,
    pub blob: String,
    pub path: RepoPath,
}

#[derive(Debug)]
pub enum SnapshotRefusal {
    /// A `120000` or `160000` entry. `pbps_load` walks the declarations
    /// directory with `read_dir` and reads what it finds *through* the link,
    /// so a snapshot that dropped the entry would give the intent command and
    /// `validate` a schema the committed tree does not have, and one that
    /// wrote the link's text as a file would give them a declaration nobody
    /// wrote. Refusing is the whole of the support.
    NotARegularFile {
        path: String,
        mode: String,
    },
    /// A name from a tree is bytes `git` stores, not a path it has checked.
    BadName {
        name: String,
        reason: BadPath,
    },
    Git(String),
    Io {
        path: String,
        detail: String,
    },
}

impl std::fmt::Display for SnapshotRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotARegularFile { path, mode } => write!(
                f,
                "`{path}` is a {} in the recorded tree, and this UI does not compose for a project \
                 whose subtree holds one",
                match mode.as_str() {
                    "120000" => "symbolic link".to_owned(),
                    "160000" => "submodule".to_owned(),
                    other => format!("`{other}` entry"),
                }
            ),
            Self::BadName { name, reason } => {
                write!(f, "the tree holds an entry named `{name}`: {reason}")
            }
            Self::Git(detail) => write!(f, "{detail}"),
            Self::Io { path, detail } => write!(f, "could not write `{path}`: {detail}"),
        }
    }
}

/// `git ls-tree -r -z <tree> -- <project>`, parsed as bytes.
///
/// `-z` because `schéma.json` came back C-quoted without it (**measured**),
/// and every entry checked for a mode that is a regular file before anything
/// is written.
pub fn entries(git: &Git, tree_ish: &str, project: &str) -> Result<Vec<Entry>, SnapshotRefusal> {
    let answer = git
        .run(&["ls-tree", "-r", "-z", tree_ish, "--", project])
        .map_err(|e| SnapshotRefusal::Git(e.to_string()))?;
    if !answer.ok() {
        return Err(SnapshotRefusal::Git(
            String::from_utf8_lossy(&answer.stderr).trim().to_owned(),
        ));
    }
    let mut entries = Vec::new();
    for record in answer.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| SnapshotRefusal::Git("ls-tree printed an entry with no name".into()))?;
        let (head, name) = record.split_at(tab);
        let name = &name[1..];
        let fields: Vec<&[u8]> = head.split(|b| *b == b' ').collect();
        let [mode, _kind, blob] = fields.as_slice() else {
            return Err(SnapshotRefusal::Git(
                "ls-tree printed an entry this protocol does not understand".into(),
            ));
        };
        let mode = String::from_utf8_lossy(mode).into_owned();
        let path = RepoPath::new(name).map_err(|reason| SnapshotRefusal::BadName {
            name: String::from_utf8_lossy(name).into_owned(),
            reason,
        })?;
        if mode != "100644" && mode != "100755" {
            return Err(SnapshotRefusal::NotARegularFile {
                path: path.to_string(),
                mode,
            });
        }
        entries.push(Entry {
            mode: u32::from_str_radix(&mode, 8).unwrap_or(0o100644),
            blob: String::from_utf8_lossy(blob).into_owned(),
            path,
        });
    }
    Ok(entries)
}

/// The bytes of every blob named, in the order named.
///
/// One `cat-file --batch` rather than one command per file: a project's
/// declarations are many small files, and a subprocess each would make the
/// snapshot the slowest part of a compose.
pub fn blobs(git: &Git, entries: &[Entry]) -> Result<Vec<Vec<u8>>, SnapshotRefusal> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let request: String = entries
        .iter()
        .map(|entry| format!("{}\n", entry.blob))
        .collect();
    let answer = git
        .run_with_input(&["cat-file", "--batch"], request.as_bytes())
        .map_err(|e| SnapshotRefusal::Git(e.to_string()))?;
    if !answer.ok() {
        return Err(SnapshotRefusal::Git(
            String::from_utf8_lossy(&answer.stderr).trim().to_owned(),
        ));
    }
    let mut rest = answer.stdout.as_slice();
    let mut blobs = Vec::with_capacity(entries.len());
    for entry in entries {
        let newline = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| SnapshotRefusal::Git("cat-file ended mid-record".into()))?;
        let header = String::from_utf8_lossy(&rest[..newline]).into_owned();
        let mut fields = header.split(' ');
        let (Some(oid), Some(kind), Some(size)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(SnapshotRefusal::Git(format!(
                "cat-file answered `{header}` for {}",
                entry.path
            )));
        };
        if oid != entry.blob || kind != "blob" {
            return Err(SnapshotRefusal::Git(format!(
                "cat-file answered `{header}` where {} was asked for",
                entry.blob
            )));
        }
        let size: usize = size
            .parse()
            .map_err(|_| SnapshotRefusal::Git(format!("cat-file gave the size as `{size}`")))?;
        rest = &rest[newline + 1..];
        if rest.len() < size + 1 {
            return Err(SnapshotRefusal::Git("cat-file ended mid-blob".into()));
        }
        blobs.push(rest[..size].to_vec());
        // The record ends with a newline `git` adds and the blob does not own.
        rest = &rest[size + 1..];
    }
    Ok(blobs)
}

/// Write the project's subtree of `tree_ish` into `into`.
///
/// Every directory and file is created relative to a handle on `into`
/// (`mkdirat`, `openat` with `O_CREAT | O_EXCL | O_NOFOLLOW`), never by
/// joining a name to a path, so that a name which passed the component check
/// still cannot reach past the handle.
pub fn write_out(
    git: &Git,
    tree_ish: &str,
    project: &str,
    into: &Path,
) -> Result<Vec<Entry>, SnapshotRefusal> {
    let entries = entries(git, tree_ish, project)?;
    let contents = blobs(git, &entries)?;
    std::fs::create_dir_all(into).map_err(|e| SnapshotRefusal::Io {
        path: into.display().to_string(),
        detail: e.to_string(),
    })?;
    let root = Dir::open_root(into).map_err(|e| SnapshotRefusal::Io {
        path: into.display().to_string(),
        detail: e.to_string(),
    })?;
    for (entry, blob) in entries.iter().zip(contents.iter()) {
        let mut here = root.reopened().map_err(|e| SnapshotRefusal::Io {
            path: entry.path.to_string(),
            detail: e.to_string(),
        })?;
        let mut components: Vec<&[u8]> = entry.path.components().collect();
        let leaf = components.pop().expect("a path has a leaf");
        for component in components {
            let name = OsString::from(std::ffi::OsStr::from_bytes(component));
            here = here
                .create_directory(&name)
                .and_then(|()| here.open_directory(&name))
                .map_err(|e| SnapshotRefusal::Io {
                    path: entry.path.to_string(),
                    detail: e.to_string(),
                })?;
        }
        let name = OsString::from(std::ffi::OsStr::from_bytes(leaf));
        let file = here
            .create_new(&name, if entry.mode == 0o100755 { 0o700 } else { 0o600 })
            .map_err(|e| SnapshotRefusal::Io {
                path: entry.path.to_string(),
                detail: e.to_string(),
            })?;
        file.write_all(blob).map_err(|e| SnapshotRefusal::Io {
            path: entry.path.to_string(),
            detail: e.to_string(),
        })?;
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::scratch_repo::Scratch;

    #[test]
    fn the_projects_subtree_is_written_out_and_the_rest_of_the_repository_is_not() {
        let scratch = Scratch::new("subtree");
        scratch.write("project/pbps.yml", b"schema_dir: schema\n");
        scratch.write("project/schema/customer.yml", b"table: customer\n");
        scratch.write("elsewhere/huge.bin", b"not the project's inputs");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let into = scratch.path(".git/pbps-ui/intent/one");

        let written = write_out(&git, &tip, "project", &into).unwrap();

        assert_eq!(written.len(), 2);
        assert_eq!(
            std::fs::read(into.join("project/schema/customer.yml")).unwrap(),
            b"table: customer\n"
        );
        assert!(!into.join("elsewhere").exists());
    }

    #[test]
    fn a_smudge_program_does_not_reach_the_snapshot() {
        // The reason the UI writes the snapshot itself: `checkout-index
        // --prefix` and `archive` both run the `smudge` program and
        // `cat-file` runs none. This test is that difference, measured here
        // rather than quoted.
        let scratch = Scratch::new("smudge");
        scratch.write("project/schema/a.yml", b"table: a\n");
        scratch.write("project/.gitattributes", b"*.yml filter=marker\n");
        scratch.git(&["config", "filter.marker.smudge", "sed s/table/SMUDGED/"]);
        scratch.git(&["config", "filter.marker.clean", "cat"]);
        let tip = scratch.commit("one");
        let git = scratch.runner();

        let into = scratch.path(".git/pbps-ui/intent/two");
        write_out(&git, &tip, "project", &into).unwrap();
        assert_eq!(
            std::fs::read(into.join("project/schema/a.yml")).unwrap(),
            b"table: a\n",
            "cat-file runs no smudge program"
        );

        // The control: the command the UI does not use does run it.
        let other = scratch.path("checkout-index-output");
        std::fs::create_dir_all(&other).unwrap();
        scratch.git(&[
            "checkout-index",
            "-a",
            "-f",
            "--prefix",
            &format!("{}/", other.display()),
        ]);
        assert!(
            std::fs::read_to_string(other.join("project/schema/a.yml"))
                .unwrap()
                .contains("SMUDGED"),
            "checkout-index would have put the program's output in the snapshot"
        );
    }

    #[test]
    fn a_linked_declaration_is_refused_with_the_entry_named() {
        // `pbps_load` follows a link, so a snapshot cannot hold one without
        // either dropping a declaration or inventing one. Decision 5 refuses
        // the layout and names the entry.
        let scratch = Scratch::new("linked");
        scratch.write("project/schema/real.yml", b"table: real\n");
        std::os::unix::fs::symlink("real.yml", scratch.path("project/schema/linked.yml")).unwrap();
        let tip = scratch.commit("one");
        let git = scratch.runner();

        let refusal = entries(&git, &tip, "project").unwrap_err();
        let SnapshotRefusal::NotARegularFile { path, mode } = &refusal else {
            panic!("expected the link to be refused, got {refusal}");
        };
        assert_eq!(path, "project/schema/linked.yml");
        assert_eq!(mode, "120000");
    }

    #[test]
    fn an_entry_named_dotdot_is_refused_before_a_name_is_joined_to_a_directory() {
        // `mktree` accepts a subtree named `..`, `ls-tree -r` then prints
        // `../file`, and only `fsck` complains. A writer that joined that name
        // to its directory would write into the repository.
        let scratch = Scratch::new("dotdot");
        scratch.write("project/a.yml", b"table: a\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let inner = scratch.git(&["rev-parse", &format!("{tip}:project")]);
        let hostile = scratch.git_stdin(&["mktree"], &format!("040000 tree {inner}\t..\n"));
        let wrapper = scratch.git_stdin(&["mktree"], &format!("040000 tree {hostile}\tproject\n"));

        let refusal = entries(&git, &wrapper, "project").unwrap_err();
        let SnapshotRefusal::BadName { name, .. } = &refusal else {
            panic!("expected the name to be refused, got {refusal}");
        };
        assert!(name.contains(".."), "{name}");
    }

    #[test]
    fn a_declaration_whose_name_is_not_utf8_is_carried_as_far_as_the_snapshot() {
        let scratch = Scratch::new("bytes");
        scratch.write("project/sch\u{00e9}ma.yml", b"table: a\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let into = scratch.path(".git/pbps-ui/intent/three");

        let written = write_out(&git, &tip, "project", &into).unwrap();
        assert_eq!(written.len(), 1);
        assert!(into.join("project/sch\u{00e9}ma.yml").exists());
    }
}
