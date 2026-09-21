//! The attribute checks of ADR-0015 decision 5's step 1, and the reason each
//! is spelled the way it is rather than the obvious way.
//!
//! A `clean` filter is a program neither `core.hooksPath` nor
//! `core.fsmonitor` reaches, and a blob stored around it leaves `git status`
//! reporting the path modified the moment it is committed, since `git`
//! compares through the filter. The transformations built into `git` —
//! `ident`, and the end-of-line conversion `text`, `eol` and `core.autocrlf`
//! ask for — do the same with no program at all.

use super::git::Git;

#[derive(Debug)]
pub enum AttributeRefusal {
    /// The path carries a `filter` attribute. Refused rather than run.
    Filter {
        path: String,
        driver: String,
    },
    /// The working tree's `.gitattributes` and the tip's disagree about this
    /// path.
    Disagree {
        path: String,
    },
    /// A built-in transformation would leave the installed path modified.
    Transformed {
        path: String,
    },
    Git(String),
}

impl std::fmt::Display for AttributeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Filter { path, driver } => write!(
                f,
                "`{path}` carries the filter attribute `{driver}`; \
                 compose does not run a filter program"
            ),
            Self::Disagree { path } => write!(
                f,
                "the working tree's .gitattributes and the recorded tip's disagree about `{path}`; \
                 commit .gitattributes first"
            ),
            Self::Transformed { path } => write!(
                f,
                "`{path}` would be transformed on check-in, which would leave it modified \
                 the moment it is committed"
            ),
            Self::Git(detail) => write!(f, "{detail}"),
        }
    }
}

/// Every attribute set on a path, as `check-attr --all -z` lists them.
///
/// `--all` and not `check-attr filter`: the obvious query answers
/// `unspecified` for no attribute *and* for a driver that happens to be named
/// `unspecified` (**measured**: under `*.json filter=unspecified` with
/// `filter.unspecified.clean` configured, `check-attr filter` printed
/// `filter: unspecified` exactly as it does for no attribute, and
/// `hash-object --stdin --path` ran the clean program all the same).
fn listed(
    git: &Git,
    path: &str,
    source: Option<&str>,
) -> Result<Vec<(String, String)>, AttributeRefusal> {
    let mut arguments = vec!["check-attr".to_owned(), "--all".to_owned(), "-z".to_owned()];
    if let Some(tree) = source {
        arguments.push(format!("--source={tree}"));
    }
    arguments.push("--".to_owned());
    arguments.push(path.to_owned());
    let answer = git
        .run(&arguments)
        .map_err(|e| AttributeRefusal::Git(e.to_string()))?;
    if !answer.ok() {
        return Err(AttributeRefusal::Git(
            String::from_utf8_lossy(&answer.stderr).trim().to_owned(),
        ));
    }
    // `<path>\0<attribute>\0<value>\0` per record.
    let fields: Vec<String> = answer
        .stdout
        .split(|b| *b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect();
    let mut set = Vec::new();
    for record in fields.chunks(3) {
        if let [_path, attribute, value] = record {
            set.push((attribute.clone(), value.clone()));
        }
    }
    set.sort();
    Ok(set)
}

/// Step 1's attribute half for one path.
///
/// Taken *after* step 2 has placed the file and confirmed the directory's
/// identity, in the same instant as that confirmation, because both of these
/// answer by path name: `--path` and `check-attr` read the `.gitattributes`
/// files along the name as it is at that moment.
pub fn refuse_transformations(
    git: &Git,
    path: &str,
    tip: &str,
    current: &[u8],
    replacement: &[u8],
) -> Result<(), AttributeRefusal> {
    let working = listed(git, path, None)?;
    if let Some((_, driver)) = working.iter().find(|(name, _)| name == "filter") {
        return Err(AttributeRefusal::Filter {
            path: path.to_owned(),
            driver: driver.clone(),
        });
    }
    // Every attribute check here reads the working tree's `.gitattributes`,
    // while the commit is built on the tip and carries the tip's. A `filter`
    // or `ident` rule removed locally would pass the working-tree checks and
    // leave the commit transforming the path in every other checkout
    // (**measured**: with `*.json ident` at the tip and the working-tree file
    // emptied, `check-attr --all` listed nothing and `--source HEAD` listed
    // `ident: set`).
    let recorded = listed(git, path, Some(tip))?;
    if working != recorded {
        return Err(AttributeRefusal::Disagree {
            path: path.to_owned(),
        });
    }
    // With no filter program left to run, what is left is what `git` does
    // itself. `--path` hashes what check-in would store for that name and
    // `--no-filters` hashes the bytes; any difference is a transformation.
    // Taken of the replacement too, because an attribute that leaves the old
    // content alone can still act on the new: a file without a marker passed,
    // and the same file with `$Id: forged $` written into it did not.
    for bytes in [current, replacement] {
        if named(git, path, bytes)? != raw(git, bytes)? {
            return Err(AttributeRefusal::Transformed {
                path: path.to_owned(),
            });
        }
    }
    Ok(())
}

/// What check-in would store for this name. Never handed the path as a file to
/// open: a name handed to `git` is walked again from the root, through
/// whatever a component has become since the handle was opened.
fn named(git: &Git, path: &str, bytes: &[u8]) -> Result<String, AttributeRefusal> {
    let answer = git
        .run_with_input(&["hash-object", "--stdin", "--path", path], bytes)
        .map_err(|e| AttributeRefusal::Git(e.to_string()))?;
    answer
        .line()
        .map_err(|e| AttributeRefusal::Git(e.to_string()))
}

fn raw(git: &Git, bytes: &[u8]) -> Result<String, AttributeRefusal> {
    let answer = git
        .run_with_input(&["hash-object", "--stdin", "--no-filters"], bytes)
        .map_err(|e| AttributeRefusal::Git(e.to_string()))?;
    answer
        .line()
        .map_err(|e| AttributeRefusal::Git(e.to_string()))
}

/// `git hash-object --no-filters --stdin`, for every hash the compose takes of
/// working-tree bytes.
///
/// Without the flag `hash-object` runs the path's `clean` filter like `git
/// add` does (**measured**: under `*.json filter=up` with an upper-casing
/// `clean`, `hash-object -w` stored the upper-cased text and `--no-filters`
/// stored the file).
pub fn hash(git: &Git, bytes: &[u8]) -> Result<String, AttributeRefusal> {
    raw(git, bytes)
}

/// The same, storing the object. Step 2's "each file is then stored as a blob
/// exactly as written, from the bytes the UI holds".
pub fn store(git: &Git, bytes: &[u8]) -> Result<String, AttributeRefusal> {
    let answer = git
        .run_with_input(&["hash-object", "-w", "--no-filters", "--stdin"], bytes)
        .map_err(|e| AttributeRefusal::Git(e.to_string()))?;
    if !answer.ok() {
        return Err(AttributeRefusal::Git(
            String::from_utf8_lossy(&answer.stderr).trim().to_owned(),
        ));
    }
    answer
        .line()
        .map_err(|e| AttributeRefusal::Git(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::scratch_repo::Scratch;

    #[test]
    fn a_driver_named_unspecified_is_seen_where_the_obvious_query_cannot_see_it() {
        // The measured trap: `check-attr filter` prints `filter: unspecified`
        // both for no attribute and for a driver actually named
        // `unspecified`, while the clean program runs all the same.
        let scratch = Scratch::new("unspecified");
        scratch.write("a.yml", b"table: a\n");
        scratch.write(".gitattributes", b"*.yml filter=unspecified\n");
        scratch.git(&["config", "filter.unspecified.clean", "tr a-z A-Z"]);
        let tip = scratch.commit("one");
        let git = scratch.runner();

        let refusal =
            refuse_transformations(&git, "a.yml", &tip, b"table: a\n", b"table: b\n").unwrap_err();
        let AttributeRefusal::Filter { path, driver } = &refusal else {
            panic!("expected the filter to be refused, got {refusal}");
        };
        assert_eq!(path, "a.yml");
        assert_eq!(driver, "unspecified");

        // The control that shows why `--all` was needed: the obvious query
        // gives the same answer here as it does for a path with no attribute.
        let asked = git
            .run(&["check-attr", "filter", "--", "a.yml"])
            .unwrap()
            .line()
            .unwrap();
        assert_eq!(asked, "a.yml: filter: unspecified");
    }

    #[test]
    fn an_uncommitted_gitattributes_edit_makes_the_two_questions_disagree() {
        // With `*.yml ident` at the tip and the working-tree file emptied, the
        // working-tree check sees nothing and the tip's sees `ident: set`; the
        // commit would carry the tip's and transform the path in every other
        // checkout.
        let scratch = Scratch::new("attrs-disagree");
        scratch.write("a.yml", b"table: a\n");
        scratch.write(".gitattributes", b"*.yml ident\n");
        let tip = scratch.commit("one");
        scratch.write(".gitattributes", b"");
        let git = scratch.runner();

        let refusal =
            refuse_transformations(&git, "a.yml", &tip, b"table: a\n", b"table: b\n").unwrap_err();
        assert!(
            matches!(refusal, AttributeRefusal::Disagree { .. }),
            "got {refusal}"
        );
    }

    #[test]
    fn a_marker_the_replacement_introduces_is_caught_where_the_old_content_passes() {
        // An attribute that leaves the old content alone can still act on the
        // new, which is why both sets of bytes are hashed twice.
        let scratch = Scratch::new("ident");
        scratch.write("a.yml", b"table: a\n");
        scratch.write(".gitattributes", b"*.yml ident\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();

        refuse_transformations(&git, "a.yml", &tip, b"table: a\n", b"table: b\n")
            .expect("neither version carries a marker");

        let refusal = refuse_transformations(
            &git,
            "a.yml",
            &tip,
            b"table: a\n",
            b"table: b $Id: forged $\n",
        )
        .unwrap_err();
        assert!(
            matches!(refusal, AttributeRefusal::Transformed { .. }),
            "got {refusal}"
        );
    }

    #[test]
    fn a_hash_of_working_tree_bytes_never_runs_the_clean_program() {
        let scratch = Scratch::new("clean");
        scratch.write("a.yml", b"table: a\n");
        scratch.write(".gitattributes", b"*.yml filter=up\n");
        scratch.git(&["config", "filter.up.clean", "tr a-z A-Z"]);
        scratch.commit("one");
        let git = scratch.runner();

        let ours = hash(&git, b"table: a\n").unwrap();
        let plain = scratch.git(&["hash-object", "--no-filters", "--", "a.yml"]);
        assert_eq!(ours, plain, "the bytes, not the program's output");

        // The control: without the flag, `hash-object` runs the clean program
        // exactly as `git add` would, and the two ids differ.
        let filtered = scratch.git(&["hash-object", "--", "a.yml"]);
        assert_ne!(
            filtered, plain,
            "the clean program changes the id, which is why the flag is not optional"
        );

        let stored = store(&git, b"table: a\n").unwrap();
        assert_eq!(stored, ours);
        assert_eq!(
            scratch.git(&["cat-file", "blob", &stored]),
            "table: a",
            "and what is in the object store is the file"
        );
    }

    #[test]
    fn an_ordinary_declaration_carries_no_transformation_and_is_accepted() {
        let scratch = Scratch::new("ordinary");
        scratch.write("a.yml", b"table: a\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        refuse_transformations(&git, "a.yml", &tip, b"table: a\n", b"table: b\n").unwrap();
    }
}
