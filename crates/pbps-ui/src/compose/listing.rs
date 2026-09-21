//! What the compose is about to commit, found the way ADR-0015 decision 5's
//! step 1 finds it.
//!
//! `git status` alone is not enough, and that is the whole shape of this
//! module. The index flags a user can set hide a file from the porcelain
//! listing however much it differs: **measured**, an edited tracked `.yml`
//! marked `--assume-unchanged` produced no porcelain record at all while `git
//! ls-files -v` said `h`. So every path the tip holds under the declarations
//! directory or at the ids file is checked *directly* — its tag must be `H`
//! and its bytes are hashed against the tip's entry — and `git status` is used
//! only for what the tip does not hold: the new file, the deleted one, the
//! ignored one.

use std::collections::BTreeMap;

use super::git::Git;
use super::repo_path::{BadPath, RepoPath};

/// What the working tree has done to one input since the recorded tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Unchanged,
    Edited,
    New,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub path: RepoPath,
    pub state: State,
    /// The tip's entry, where it has one.
    pub recorded: Option<(u32, String)>,
    /// The execute bit the working tree's file carries now.
    ///
    /// Carried because `git add` records it and the tip's entry does not know
    /// about it: a declaration whose bit was changed — with the content or
    /// instead of it — committed at the tip's mode leaves `git status`
    /// reporting the path modified after a compose that was supposed to leave
    /// it clean.
    pub executable: Option<bool>,
}

#[derive(Debug)]
pub enum ListingRefusal {
    /// A path the user has told `git` to leave alone is not one the UI should
    /// quietly commit.
    NotOrdinary {
        path: String,
        tag: char,
    },
    /// A declaration matched by `.gitignore`. The intent command would see a
    /// working tree the commit cannot hold, and the shell's `git add` would
    /// refuse the file too.
    Ignored {
        path: String,
    },
    BadName {
        name: String,
        reason: BadPath,
    },
    Git(String),
}

impl std::fmt::Display for ListingRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOrdinary { path, tag } => write!(
                f,
                "`{path}` is marked `{}` in the index; compose does not quietly commit a path \
                 you have told git to leave alone",
                match tag {
                    'S' => "skip-worktree",
                    'h' => "assume-unchanged",
                    's' => "skip-worktree and assume-unchanged",
                    'M' => "unmerged",
                    'R' => "removed",
                    _ => "not an ordinary entry",
                }
            ),
            Self::Ignored { path } => write!(
                f,
                "`{path}` is ignored by this repository's rules, so a commit cannot hold it"
            ),
            Self::BadName { name, reason } => {
                write!(f, "git listed a path named `{name}`: {reason}")
            }
            Self::Git(detail) => write!(f, "{detail}"),
        }
    }
}

/// The two places the CLI reads its inputs from, as the compose resolved them.
pub struct Inputs<'a> {
    /// `None` where the declarations directory *is* the worktree root, which
    /// is what `schema_dir: .` means for a project at the root — a layout
    /// SPEC supports and `RepoPath` cannot spell, since the empty path and
    /// `.` are both refused.
    pub declarations: Option<&'a RepoPath>,
    pub ids_file: &'a RepoPath,
}

impl Inputs<'_> {
    /// Of what `git` returns, keep only the files the CLI itself reads: a path
    /// under the declarations directory whose extension is `.yml` or `.yaml`,
    /// which is what `pbps_load` collects walking that directory, and the ids
    /// file at exactly its path.
    ///
    /// It matters most under the supported `schema_dir: .`, where the pathspec
    /// is the whole project and a `README`, an editor's stray file or a source
    /// file the user has open beside the declarations would otherwise be laid
    /// over the snapshot and committed with the intent.
    pub fn reads(&self, path: &RepoPath) -> bool {
        if path == self.ids_file {
            return true;
        }
        let under = match self.declarations {
            None => true,
            Some(directory) => path.is_under(directory.as_bytes()),
        };
        under && path.extension_is_declaration()
    }

    fn pathspec(&self) -> Vec<String> {
        let declarations = match self.declarations {
            // `.` is what `git` reads as "everything under here", and the
            // commands below all run from the worktree root.
            None => Some(".".to_owned()),
            Some(directory) => directory.to_text().map(str::to_owned),
        };
        declarations
            .into_iter()
            .chain(self.ids_file.to_text().map(str::to_owned))
            .collect()
    }
}

/// Step 1's listing, whole.
pub fn inputs(
    git: &Git,
    tip: &str,
    inputs: &Inputs<'_>,
    mut look_at: impl FnMut(&RepoPath) -> Option<(String, bool)>,
) -> Result<Vec<Input>, ListingRefusal> {
    let mut found: BTreeMap<RepoPath, Input> = BTreeMap::new();

    // The tracked side, found by hashing rather than by asking `status`.
    let recorded = tracked_entries(git, tip, inputs)?;
    let tags = index_tags(git, inputs)?;
    for entry in recorded {
        if !inputs.reads(&entry.path) {
            continue;
        }
        let tag = tags.get(&entry.path).copied().unwrap_or('H');
        if tag != 'H' {
            return Err(ListingRefusal::NotOrdinary {
                path: entry.path.to_string(),
                tag,
            });
        }
        let found_now = look_at(&entry.path);
        // A mode-only change is a change. Comparing the blob alone reads a
        // declaration whose execute bit was flipped as unchanged, leaves it
        // out of the commit, and leaves the checkout dirty afterwards.
        let state = match &found_now {
            None => State::Deleted,
            Some((blob, executable))
                if *blob == entry.blob && *executable == (entry.mode == 0o100755) =>
            {
                State::Unchanged
            }
            Some(_) => State::Edited,
        };
        found.insert(
            entry.path.clone(),
            Input {
                path: entry.path,
                state,
                recorded: Some((entry.mode, entry.blob)),
                executable: found_now.map(|(_, executable)| executable),
            },
        );
    }

    // And `status` for what the tip does not hold.
    for (code, path) in porcelain(git, inputs)? {
        if !inputs.reads(&path) {
            continue;
        }
        if code == "!!" {
            return Err(ListingRefusal::Ignored {
                path: path.to_string(),
            });
        }
        let executable = look_at(&path).map(|(_, executable)| executable);
        found.entry(path.clone()).or_insert(Input {
            path,
            state: State::New,
            recorded: None,
            executable,
        });
    }

    Ok(found.into_values().collect())
}

/// `git ls-tree` over both pathspecs, for the entries the tip holds.
fn tracked_entries(
    git: &Git,
    tip: &str,
    inputs: &Inputs<'_>,
) -> Result<Vec<super::snapshot::Entry>, ListingRefusal> {
    let mut arguments = vec![
        "ls-tree".to_owned(),
        "-r".to_owned(),
        "-z".to_owned(),
        tip.to_owned(),
        "--".to_owned(),
    ];
    arguments.extend(inputs.pathspec());
    let answer = git
        .run(&arguments)
        .map_err(|e| ListingRefusal::Git(e.to_string()))?;
    if !answer.ok() {
        return Err(ListingRefusal::Git(
            String::from_utf8_lossy(&answer.stderr).trim().to_owned(),
        ));
    }
    let mut entries = Vec::new();
    for record in answer.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let Some(tab) = record.iter().position(|b| *b == b'\t') else {
            continue;
        };
        let (head, name) = record.split_at(tab);
        let name = &name[1..];
        let fields: Vec<&[u8]> = head.split(|b| *b == b' ').collect();
        let [mode, kind, blob] = fields.as_slice() else {
            continue;
        };
        if kind != b"blob" {
            continue;
        }
        let path = RepoPath::new(name).map_err(|reason| ListingRefusal::BadName {
            name: String::from_utf8_lossy(name).into_owned(),
            reason,
        })?;
        entries.push(super::snapshot::Entry {
            mode: u32::from_str_radix(&String::from_utf8_lossy(mode), 8).unwrap_or(0o100644),
            blob: String::from_utf8_lossy(blob).into_owned(),
            path,
        });
    }
    Ok(entries)
}

/// `git ls-files -v -z`, whose tag is the question `status` cannot answer.
fn index_tags(git: &Git, inputs: &Inputs<'_>) -> Result<BTreeMap<RepoPath, char>, ListingRefusal> {
    let mut arguments = vec![
        "ls-files".to_owned(),
        "-v".to_owned(),
        "-z".to_owned(),
        "--".to_owned(),
    ];
    arguments.extend(inputs.pathspec());
    let answer = git
        .run(&arguments)
        .map_err(|e| ListingRefusal::Git(e.to_string()))?;
    if !answer.ok() {
        return Err(ListingRefusal::Git(
            String::from_utf8_lossy(&answer.stderr).trim().to_owned(),
        ));
    }
    let mut tags = BTreeMap::new();
    for record in answer.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let [tag, b' ', name @ ..] = record else {
            continue;
        };
        let path = RepoPath::new(name).map_err(|reason| ListingRefusal::BadName {
            name: String::from_utf8_lossy(name).into_owned(),
            reason,
        })?;
        tags.insert(path, *tag as char);
    }
    Ok(tags)
}

/// `git status --porcelain -z --untracked-files=all --ignored`.
///
/// `--untracked-files=all` because `status.showUntrackedFiles=no` in the
/// user's configuration would otherwise hide the new side of a file-based
/// rename (**measured**: under it the listing held the deleted old path alone,
/// and with the flag both).
///
/// `--ignored`, which is `--ignored=traditional`, because a new declaration
/// matched by `.gitignore` is listed by neither flag alone, and because with
/// `--untracked-files=all` this mode names the ignored *files* where
/// `--ignored=matching` names the ignored directory and stops there — a rule
/// like `schema/generated/` would otherwise hide every declaration under it
/// behind one record the filter then discards (**measured**).
fn porcelain(git: &Git, inputs: &Inputs<'_>) -> Result<Vec<(String, RepoPath)>, ListingRefusal> {
    let mut arguments = vec![
        "status".to_owned(),
        "--porcelain".to_owned(),
        "-z".to_owned(),
        "--untracked-files=all".to_owned(),
        "--ignored".to_owned(),
        "--".to_owned(),
    ];
    arguments.extend(inputs.pathspec());
    let answer = git
        .run(&arguments)
        .map_err(|e| ListingRefusal::Git(e.to_string()))?;
    if !answer.ok() {
        return Err(ListingRefusal::Git(
            String::from_utf8_lossy(&answer.stderr).trim().to_owned(),
        ));
    }
    let mut listed = Vec::new();
    let mut records = answer.stdout.split(|b| *b == 0).filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 4 {
            continue;
        }
        let code = String::from_utf8_lossy(&record[..2]).into_owned();
        let name = &record[3..];
        // A rename's record is followed by the original path in its own field.
        if code.starts_with('R') || code.starts_with('C') {
            let _ = records.next();
        }
        let path = RepoPath::new(name).map_err(|reason| ListingRefusal::BadName {
            name: String::from_utf8_lossy(name).into_owned(),
            reason,
        })?;
        listed.push((code, path));
    }
    Ok(listed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::scratch_repo::Scratch;

    fn path(text: &str) -> RepoPath {
        RepoPath::new(text.as_bytes()).unwrap()
    }

    fn read_hashes(scratch: &Scratch) -> impl FnMut(&RepoPath) -> Option<(String, bool)> + '_ {
        move |wanted: &RepoPath| {
            use std::os::unix::fs::PermissionsExt as _;
            let file = scratch.path(wanted.to_text().unwrap());
            let bytes = std::fs::read(&file).ok()?;
            let executable = std::fs::metadata(&file).ok()?.permissions().mode() & 0o111 != 0;
            let git = scratch.runner();
            Some((
                super::super::attributes::hash(&git, &bytes).unwrap(),
                executable,
            ))
        }
    }

    #[test]
    fn declarations_at_the_worktree_root_are_a_place_and_not_a_missing_one() {
        // `schema_dir: .` resolves the declarations directory to the project
        // itself, and for a project at the worktree root that is a path no
        // `RepoPath` can spell — the empty path and `.` are both refused, and
        // rightly, since neither is a name. Representing it as `None` is what
        // keeps "the root" from being read as "outside the project".
        let ids_file = path("schema.ids.json");
        let root = Inputs {
            declarations: None,
            ids_file: &ids_file,
        };
        assert!(root.reads(&path("customer.yml")));
        assert!(root.reads(&path("nested/deeper/order.yaml")));
        assert!(root.reads(&ids_file));
        assert!(!root.reads(&path("README")));
        assert_eq!(root.pathspec(), vec![".", "schema.ids.json"]);

        // And a declarations directory that *is* named still bounds it.
        let named = path("schema");
        let under = Inputs {
            declarations: Some(&named),
            ids_file: &ids_file,
        };
        assert!(under.reads(&path("schema/customer.yml")));
        assert!(!under.reads(&path("customer.yml")));
        assert_eq!(under.pathspec(), vec!["schema", "schema.ids.json"]);
    }

    #[test]
    fn an_assume_unchanged_declaration_is_refused_where_status_says_nothing_at_all() {
        // The measured trap this module exists for: the flags that hid
        // `pbps.yml` hide a declaration just as well, and the porcelain
        // listing shows nothing.
        let scratch = Scratch::new("assume-unchanged");
        scratch.write("schema/a.yml", b"table: a\n");
        scratch.write("ids.json", b"{}\n");
        let tip = scratch.commit("one");
        scratch.write("schema/a.yml", b"table: edited\n");
        scratch.git(&["update-index", "--assume-unchanged", "schema/a.yml"]);
        let git = scratch.runner();

        assert_eq!(
            scratch.git(&["status", "--porcelain"]),
            "",
            "status is silent about it, which is the point"
        );

        let declarations = path("schema");
        let ids_file = path("ids.json");
        let refusal = inputs(
            &git,
            &tip,
            &Inputs {
                declarations: Some(&declarations),
                ids_file: &ids_file,
            },
            read_hashes(&scratch),
        )
        .unwrap_err();
        let ListingRefusal::NotOrdinary { path: shown, tag } = &refusal else {
            panic!("expected the flag to be refused, got {refusal}");
        };
        assert_eq!(shown, "schema/a.yml");
        assert_eq!(*tag, 'h');
    }

    #[test]
    fn an_edit_a_new_file_and_a_deletion_are_three_different_states() {
        let scratch = Scratch::new("states");
        scratch.write("schema/kept.yml", b"table: kept\n");
        scratch.write("schema/edited.yml", b"table: before\n");
        scratch.write("schema/gone.yml", b"table: gone\n");
        scratch.write("ids.json", b"{}\n");
        let tip = scratch.commit("one");
        scratch.write("schema/edited.yml", b"table: after\n");
        std::fs::remove_file(scratch.path("schema/gone.yml")).unwrap();
        scratch.write("schema/added.yml", b"table: added\n");
        // Neither of these is a declaration, and neither may be committed.
        scratch.write("schema/README", b"not a declaration\n");
        scratch.write("schema/.edited.yml.swp", b"an editor's own\n");
        let git = scratch.runner();

        let declarations = path("schema");
        let ids_file = path("ids.json");
        let found = inputs(
            &git,
            &tip,
            &Inputs {
                declarations: Some(&declarations),
                ids_file: &ids_file,
            },
            read_hashes(&scratch),
        )
        .unwrap();

        let states: Vec<(String, State)> = found
            .iter()
            .map(|input| (input.path.to_string(), input.state))
            .collect();
        assert_eq!(
            states,
            vec![
                ("ids.json".to_owned(), State::Unchanged),
                ("schema/added.yml".to_owned(), State::New),
                ("schema/edited.yml".to_owned(), State::Edited),
                ("schema/gone.yml".to_owned(), State::Deleted),
                ("schema/kept.yml".to_owned(), State::Unchanged),
            ]
        );
    }

    #[test]
    fn an_ignored_declaration_is_refused_with_the_file_named() {
        // A commit cannot hold it and the shell's `git add` would refuse it
        // too, so the compose says so rather than quietly leaving it out of a
        // schema the intent command was about to resolve.
        let scratch = Scratch::new("ignored");
        scratch.write("schema/a.yml", b"table: a\n");
        scratch.write("ids.json", b"{}\n");
        scratch.write(".gitignore", b"schema/generated/\n");
        let tip = scratch.commit("one");
        scratch.write("schema/generated/new.yml", b"table: generated\n");
        let git = scratch.runner();

        let declarations = path("schema");
        let ids_file = path("ids.json");
        let refusal = inputs(
            &git,
            &tip,
            &Inputs {
                declarations: Some(&declarations),
                ids_file: &ids_file,
            },
            read_hashes(&scratch),
        )
        .unwrap_err();
        let ListingRefusal::Ignored { path: shown } = &refusal else {
            panic!("expected the ignored file to be refused, got {refusal}");
        };
        assert_eq!(
            shown, "schema/generated/new.yml",
            "the file, not the directory the traditional mode would stop at"
        );
    }

    #[test]
    fn a_configuration_that_hides_untracked_files_does_not_hide_the_new_side_of_a_rename() {
        // `status.showUntrackedFiles=no` left the listing holding the deleted
        // old path alone; with `--untracked-files=all` it holds both, and a
        // compose that missed the new side would resolve a rename as a drop.
        let scratch = Scratch::new("untracked-no");
        scratch.write("schema/old.yml", b"table: customer\n");
        scratch.write("ids.json", b"{}\n");
        let tip = scratch.commit("one");
        scratch.git(&["config", "status.showUntrackedFiles", "no"]);
        std::fs::rename(
            scratch.path("schema/old.yml"),
            scratch.path("schema/new.yml"),
        )
        .unwrap();
        let git = scratch.runner();

        let declarations = path("schema");
        let ids_file = path("ids.json");
        let found = inputs(
            &git,
            &tip,
            &Inputs {
                declarations: Some(&declarations),
                ids_file: &ids_file,
            },
            read_hashes(&scratch),
        )
        .unwrap();

        let states: Vec<(String, State)> = found
            .iter()
            .map(|input| (input.path.to_string(), input.state))
            .collect();
        assert!(
            states.contains(&("schema/new.yml".to_owned(), State::New)),
            "the new side is listed: {states:?}"
        );
        assert!(states.contains(&("schema/old.yml".to_owned(), State::Deleted)));
    }
}
