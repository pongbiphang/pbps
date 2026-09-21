//! Steps 3 and 4 of ADR-0015 decision 5: the tree the commit will hold, the
//! locked copy of the user's index that matches it, and the commit itself.
//!
//! The commit is built with plumbing, and the reason is the record of a design
//! that was given up. The first shape was the porcelain `git commit --only -m
//! <message> -- <paths>`, with the commit read back and pushed only if it
//! matched the preview, and every step of it was measured before it was
//! abandoned: a `pre-commit` hook that ran `git add b` widened `--only -- a`
//! to a commit of `a` and `b`; one that rewrote `a` committed the rewritten
//! content; one that ran `chmod +x` left the blob id equal and turned the tree
//! entry from `100644` to `100755`; a `commit-msg` hook appending a line
//! changed the message `-m` had asked for. Checks were written for all four.
//! The fifth measurement showed the shape of the mistake: a `post-commit` hook
//! that pushes runs *before* `git commit` returns, so the remote held both
//! files before the UI could run its first read-back. Once a hook can publish,
//! a check after the fact is a check too late.
//!
//! What is built here is instead what was previewed **by construction** —
//! those paths, those blobs, that parent, that message — and there is nothing
//! to read back.

use std::path::Path;

use super::git::Git;
use super::repo_path::RepoPath;

/// One entry to write into an index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub mode: u32,
    pub blob: String,
    pub path: RepoPath,
}

#[derive(Debug)]
pub enum TreeRefusal {
    /// `update-index` refused the entry rather than replace the user's. A
    /// staged entry of the user's own can occupy a new path's directory as a
    /// file, or its name as a directory (**measured**: with `schema/new`
    /// staged as a file, adding `schema/new/x.json` to a copy of that index
    /// failed with `appears as both a file and as a directory`, while the same
    /// entry went into an index read from the tip).
    IndexRefused {
        path: String,
        detail: String,
    },
    /// A path whose name `git` cannot be given on a command line.
    Unnameable {
        path: String,
    },
    Git(String),
}

impl std::fmt::Display for TreeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IndexRefused { path, detail } => write!(
                f,
                "your index already stages something that occupies `{path}`: {detail}. \
                 Commit or reset that first; compose does not drop staged work"
            ),
            Self::Unnameable { path } => write!(
                f,
                "`{path}` cannot be spelled on a git command line; compose needs a text name"
            ),
            Self::Git(detail) => write!(f, "{detail}"),
        }
    }
}

fn text(path: &RepoPath) -> Result<&str, TreeRefusal> {
    path.to_text().ok_or_else(|| TreeRefusal::Unnameable {
        path: path.to_string(),
    })
}

/// Write the entries into an index of the compose's own and answer the tree.
///
/// `git read-tree <tip>` first, so the user's own index is not read here: the
/// tree is the recorded tip's plus exactly what the intent command changed.
pub fn write_tree(
    git: &Git,
    index: &Path,
    tip: &str,
    entries: &[CacheEntry],
    removals: &[RepoPath],
) -> Result<String, TreeRefusal> {
    let _ = std::fs::remove_file(index);
    run(git, index, &["read-tree".to_owned(), tip.to_owned()])?;
    apply_entries(git, index, entries, removals)?;
    let written = run(git, index, &["write-tree".to_owned()])?;
    written.line().map_err(|e| TreeRefusal::Git(e.to_string()))
}

/// The same entries, into the locked copy of the user's index.
///
/// Prepared here rather than in step 6 because *this* write can fail where the
/// temporary index's did not, and it has to fail before step 5 moves the
/// branch. `--replace` would let the entries through by dropping the user's
/// staged ones, which is the work step 1 refuses to touch.
pub fn prepare_index(
    git: &Git,
    index_lock: &Path,
    entries: &[CacheEntry],
    removals: &[RepoPath],
) -> Result<(), TreeRefusal> {
    apply_entries(git, index_lock, entries, removals)
}

fn apply_entries(
    git: &Git,
    index: &Path,
    entries: &[CacheEntry],
    removals: &[RepoPath],
) -> Result<(), TreeRefusal> {
    for entry in entries {
        let path = text(&entry.path)?;
        let cacheinfo = format!("{:06o},{},{}", entry.mode, entry.blob, path);
        // `--add` because a new path is not in the tree that was read
        // (**measured**: without it, `cannot add to the index - missing --add
        // option?`, exit 128).
        let answer = run(
            git,
            index,
            &[
                "update-index".to_owned(),
                "--add".to_owned(),
                "--cacheinfo".to_owned(),
                cacheinfo,
            ],
        );
        if let Err(TreeRefusal::Git(detail)) = answer {
            return Err(TreeRefusal::IndexRefused {
                path: path.to_owned(),
                detail,
            });
        }
        answer?;
    }
    for path in removals {
        let name = text(path)?;
        // `--force-remove` is the form that removes the entry whatever the
        // working tree holds (**measured**: after it the written tree lacked
        // the path, and with the commit on the branch and the prepared index
        // installed, `git status` was clean).
        run(
            git,
            index,
            &[
                "update-index".to_owned(),
                "--force-remove".to_owned(),
                "--".to_owned(),
                name.to_owned(),
            ],
        )?;
    }
    Ok(())
}

fn run(git: &Git, index: &Path, arguments: &[String]) -> Result<super::git::Run, TreeRefusal> {
    let answer = git
        .run_with_index(index, arguments)
        .map_err(|e| TreeRefusal::Git(e.to_string()))?;
    if answer.ok() {
        Ok(answer)
    } else {
        Err(TreeRefusal::Git(
            String::from_utf8_lossy(&answer.stderr).trim().to_owned(),
        ))
    }
}

/// Step 4. The commit is signed exactly when the shell's would be, and that
/// takes one step porcelain does by itself: `commit-tree` does *not* read
/// `commit.gpgSign` (**measured**: with it set and a key that cannot sign,
/// `git commit` failed, `commit-tree` succeeded unsigned, and `commit-tree -S`
/// failed the way `git commit` had).
pub fn commit_tree(
    git: &Git,
    tree: &str,
    parent: &str,
    message: &str,
    sign: bool,
) -> Result<String, TreeRefusal> {
    let mut arguments = vec![
        "commit-tree".to_owned(),
        tree.to_owned(),
        "-p".to_owned(),
        parent.to_owned(),
        "-m".to_owned(),
        message.to_owned(),
    ];
    if sign {
        arguments.push("-S".to_owned());
    }
    let answer = git
        .run(&arguments)
        .map_err(|e| TreeRefusal::Git(e.to_string()))?;
    if !answer.ok() {
        return Err(TreeRefusal::Git(
            String::from_utf8_lossy(&answer.stderr).trim().to_owned(),
        ));
    }
    answer.line().map_err(|e| TreeRefusal::Git(e.to_string()))
}

/// Whether the shell's commit here would be signed. The key, the format and
/// the program are left to the user's configuration: ADR-0006's "signed
/// commit" is the organization's signing policy applied by that
/// configuration, not a guarantee this UI adds.
pub fn signing_wanted(git: &Git) -> bool {
    git.run(&["config", "--type=bool", "commit.gpgSign"])
        .ok()
        .filter(super::git::Run::ok)
        .and_then(|answer| answer.line().ok())
        .is_some_and(|value| value == "true")
}

/// What the page shows beside the commit: `git log -1 --format=%G?`.
pub fn signature_state(git: &Git, commit: &str) -> String {
    git.run(&["log", "-1", "--format=%G?", commit])
        .ok()
        .and_then(|answer| answer.line().ok())
        .unwrap_or_default()
}

/// The preview: the recorded tip against the tree the UI built, exact by
/// construction.
///
/// `--no-ext-diff` and `--no-textconv` because a `diff.external` or `textconv`
/// driver can show two blobs as one text (**measured**: a driver printing a
/// constant hid a rewritten file), and `--text` because a `.gitattributes`
/// line can mark the declarations `-diff` and the two flags do not override
/// that (**measured**: with `*.json -diff` the preview said `Binary files …
/// differ`; with `--text` it showed the hunk).
pub fn preview(git: &Git, tip: &str, tree: &str) -> Result<Vec<u8>, TreeRefusal> {
    let answer = git
        .run(&[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--text",
            tip,
            tree,
        ])
        .map_err(|e| TreeRefusal::Git(e.to_string()))?;
    // `git diff` answers 0 for no difference and 1 for a difference only with
    // `--exit-code`, which is not passed here; anything but 0 is a failure.
    if answer.ok() {
        Ok(answer.stdout)
    } else {
        Err(TreeRefusal::Git(
            String::from_utf8_lossy(&answer.stderr).trim().to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::scratch_repo::Scratch;

    fn path(text: &str) -> RepoPath {
        RepoPath::new(text.as_bytes()).unwrap()
    }

    #[test]
    fn the_tree_is_the_tip_plus_exactly_what_the_intent_changed() {
        let scratch = Scratch::new("tree-build");
        scratch.write("schema/a.yml", b"table: a\n");
        scratch.write("schema/b.yml", b"table: b\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let blob = scratch.git_stdin(&["hash-object", "-w", "--stdin"], "table: a2\n");
        let index = scratch.path(".git/pbps-ui-test-index");

        let tree = write_tree(
            &git,
            &index,
            &tip,
            &[CacheEntry {
                mode: 0o100644,
                blob: blob.clone(),
                path: path("schema/a.yml"),
            }],
            &[],
        )
        .unwrap();

        assert_eq!(
            scratch.git(&["ls-tree", "-r", "--name-only", &tree]),
            "schema/a.yml\nschema/b.yml"
        );
        assert_eq!(
            scratch.git(&["cat-file", "blob", &format!("{tree}:schema/a.yml")]),
            "table: a2"
        );
        assert_eq!(
            scratch.git(&["cat-file", "blob", &format!("{tree}:schema/b.yml")]),
            "table: b",
            "everything else is the tip's"
        );
    }

    #[test]
    fn a_deleted_declaration_leaves_the_tree_without_the_path() {
        // A table or a role is dropped or renamed by deleting or renaming its
        // declaration file, so the removal has to reach the tree.
        let scratch = Scratch::new("tree-remove");
        scratch.write("schema/a.yml", b"table: a\n");
        scratch.write("schema/gone.yml", b"table: gone\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let index = scratch.path(".git/pbps-ui-test-index");

        let tree = write_tree(&git, &index, &tip, &[], &[path("schema/gone.yml")]).unwrap();

        assert_eq!(
            scratch.git(&["ls-tree", "-r", "--name-only", &tree]),
            "schema/a.yml"
        );
    }

    #[test]
    fn a_staged_entry_of_the_users_own_refuses_the_prepared_index_before_the_branch_moves() {
        // The measured case: with `schema/new` staged as a file, adding
        // `schema/new/x.yml` to a copy of that index fails, while the same
        // entry goes into an index read from the tip. Step 1's check covers
        // the edited paths' own entries, not their neighbours, so the refusal
        // has to come from the write itself — and before step 5.
        let scratch = Scratch::new("tree-staged");
        scratch.write("schema/a.yml", b"table: a\n");
        let tip = scratch.commit("one");
        scratch.write("schema/new", b"a file where a directory is wanted\n");
        scratch.git(&["add", "schema/new"]);
        let git = scratch.runner();
        let blob = scratch.git_stdin(&["hash-object", "-w", "--stdin"], "table: nested\n");
        let entries = [CacheEntry {
            mode: 0o100644,
            blob,
            path: path("schema/new/x.yml"),
        }];

        // An index read from the tip takes it.
        let fresh = scratch.path(".git/pbps-ui-test-index");
        write_tree(&git, &fresh, &tip, &entries, &[]).expect("the tip's index has no such entry");

        // The copy of the user's index does not.
        let copy = scratch.path(".git/pbps-ui-test-index-lock");
        std::fs::copy(scratch.path(".git/index"), &copy).unwrap();
        let refusal = prepare_index(&git, &copy, &entries, &[]).unwrap_err();
        let TreeRefusal::IndexRefused {
            path: shown,
            detail,
        } = &refusal
        else {
            panic!("expected the staged entry to refuse it, got {refusal}");
        };
        assert_eq!(shown, "schema/new/x.yml");
        assert!(
            detail.contains("both a file and as a directory"),
            "{detail}"
        );
    }

    #[test]
    fn commit_tree_holds_those_paths_that_parent_and_that_message_and_runs_no_hook() {
        let scratch = Scratch::new("tree-commit");
        scratch.write("schema/a.yml", b"table: a\n");
        let tip = scratch.commit("one");
        // The hooks the porcelain route could not be protected from.
        let hooks = scratch.path(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        for (name, body) in [
            (
                "pre-commit",
                "#!/bin/sh\necho widened > widened.txt\ngit add widened.txt\n",
            ),
            (
                "commit-msg",
                "#!/bin/sh\necho 'appended by a hook' >> \"$1\"\n",
            ),
            ("post-commit", "#!/bin/sh\necho ran > post-commit-ran.txt\n"),
        ] {
            let script = hooks.join(name);
            std::fs::write(&script, body).unwrap();
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let git = scratch.runner();
        let blob = scratch.git_stdin(&["hash-object", "-w", "--stdin"], "table: a2\n");
        let index = scratch.path(".git/pbps-ui-test-index");
        let tree = write_tree(
            &git,
            &index,
            &tip,
            &[CacheEntry {
                mode: 0o100644,
                blob,
                path: path("schema/a.yml"),
            }],
            &[],
        )
        .unwrap();

        let commit = commit_tree(
            &git,
            &tree,
            &tip,
            "rename dbo.customer.customer_name full_name",
            false,
        )
        .unwrap();

        assert_eq!(
            scratch.git(&["log", "-1", "--format=%s", &commit]),
            "rename dbo.customer.customer_name full_name",
            "the message is the one asked for"
        );
        assert_eq!(scratch.git(&["rev-parse", &format!("{commit}^")]), tip);
        assert_eq!(
            scratch.git(&["ls-tree", "-r", "--name-only", &commit]),
            "schema/a.yml",
            "and no hook widened it"
        );
        assert!(!scratch.path("post-commit-ran.txt").exists());
        assert!(!scratch.path("widened.txt").exists());
    }

    #[test]
    fn the_preview_shows_a_rewritten_file_a_presentation_driver_would_have_hidden() {
        let scratch = Scratch::new("tree-preview");
        scratch.write("schema/a.yml", b"table: before\n");
        scratch.write(".gitattributes", b"*.yml -diff diff=constant\n");
        scratch.git(&["config", "diff.constant.textconv", "echo constant"]);
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let blob = scratch.git_stdin(&["hash-object", "-w", "--stdin"], "table: after\n");
        let index = scratch.path(".git/pbps-ui-test-index");
        let tree = write_tree(
            &git,
            &index,
            &tip,
            &[CacheEntry {
                mode: 0o100644,
                blob,
                path: path("schema/a.yml"),
            }],
            &[],
        )
        .unwrap();

        let shown = String::from_utf8(preview(&git, &tip, &tree).unwrap()).unwrap();
        assert!(shown.contains("-table: before"), "{shown}");
        assert!(shown.contains("+table: after"), "{shown}");

        // The control: the same diff without the flags is one line of a
        // driver's output, or nothing at all.
        let hidden = scratch.git(&["diff", &tip, &tree]);
        assert!(
            !hidden.contains("table: after"),
            "the driver hid the change: {hidden}"
        );
    }
}
