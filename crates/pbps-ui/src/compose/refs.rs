//! The ref half of ADR-0015 decision 5: what step 1 records, what step 5
//! writes, and what each of them refuses.
//!
//! The rule that shapes all of it is that **a chain is refused, never
//! chased**. `HEAD` may be symbolic to a ref that is itself symbolic, and
//! every link in such a chain is a ref another worktree can write. Locking the
//! end of the chain leaves every link unlocked: **measured** on git 2.43, with
//! `HEAD.lock` and the end-of-chain branch's lock both held, `symbolic-ref
//! refs/heads/<a> refs/heads/<other>` went through and `rev-parse HEAD` then
//! read the other branch's tip. Holding one tip still would mean locking every
//! link and asking again, so this protocol decides on one branch and refuses a
//! chain outright.
//!
//! That is also why every read here that means "the ref `HEAD` names" takes
//! `--no-recurse`. The bare command dereferences recursively by default and
//! answers the *end* of the chain, which is already a direct ref — so a
//! refusal built against that answer could never fire on the link it exists to
//! catch (**measured**: with `HEAD` symbolic to `a` symbolic to `b`, bare
//! `symbolic-ref HEAD` answered `refs/heads/b` where `--no-recurse` answered
//! `refs/heads/a`).

use super::git::{Failure, Git, Session};

/// What step 1 recorded: the ref `HEAD` names *directly*, and that ref's tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub reference: String,
    pub tip: String,
}

#[derive(Debug)]
pub enum RefRefusal {
    /// `extensions.refStorage` names a backend whose locks are not files.
    /// Refused rather than held, because a file that protects no ref is worse
    /// than no lock at all: it looks like protection.
    Backend(String),
    /// `HEAD` is not symbolic. Composing needs a branch to move.
    Detached,
    /// The ref `HEAD` names is itself symbolic. Refused before anything is
    /// placed, rather than relying on step 5's later refusal and undo.
    Chained {
        reference: String,
        target: String,
    },
    /// The ref has no commit: an unborn branch, or a target no tree can be
    /// read from.
    Unborn(String),
    Unreadable {
        reference: String,
        detail: String,
    },
    /// The compare-and-swap of step 5, or its post-write check, found another
    /// value.
    Moved {
        reference: String,
        expected: String,
        found: String,
    },
    /// `HEAD` no longer names the branch the compose advanced.
    HeadElsewhere {
        expected: String,
        found: String,
    },
    /// `git` answered something this protocol does not understand. Never
    /// treated as success: a read error never satisfies a check.
    Unexpected(String),
}

impl std::fmt::Display for RefRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(name) => write!(
                f,
                "this repository stores refs with the `{name}` backend, whose locks are not files; \
                 compose needs the files backend"
            ),
            Self::Detached => write!(
                f,
                "HEAD is detached, so there is no branch to record the intent on"
            ),
            Self::Chained { reference, target } => write!(
                f,
                "{reference} is itself a symbolic ref, to {target}; \
                 compose decides on one branch and does not follow a chain"
            ),
            Self::Unborn(reference) => {
                write!(f, "{reference} has no commit yet")
            }
            Self::Unreadable { reference, detail } => {
                write!(f, "could not read {reference}: {detail}")
            }
            Self::Moved {
                reference,
                expected,
                found,
            } => write!(f, "{reference} is at {found} but was {expected}"),
            Self::HeadElsewhere { expected, found } => write!(
                f,
                "HEAD names {found} now, not {expected}; this checkout is on another branch"
            ),
            Self::Unexpected(detail) => write!(f, "git answered unexpectedly: {detail}"),
        }
    }
}

impl From<Failure> for RefRefusal {
    fn from(failure: Failure) -> Self {
        Self::Unexpected(failure.to_string())
    }
}

/// The files backend, or nothing. `extensions.refStorage` is absent in an
/// ordinary repository.
pub fn backend_is_files(git: &Git) -> Result<(), RefRefusal> {
    let answer = git.run(&["config", "--get", "extensions.refStorage"])?;
    match answer.code {
        // Exit 1 is "no such key", which is the ordinary repository.
        Some(1) => Ok(()),
        Some(0) => {
            let name = answer.line()?;
            if name == "files" {
                Ok(())
            } else {
                Err(RefRefusal::Backend(name))
            }
        }
        _ => Err(RefRefusal::Unexpected(
            String::from_utf8_lossy(&answer.stderr).trim().to_owned(),
        )),
    }
}

/// The ref `HEAD` names directly — not the end of a chain.
pub fn head_names(git: &Git) -> Result<String, RefRefusal> {
    let answer = git.run(&["symbolic-ref", "--no-recurse", "HEAD"])?;
    if answer.ok() {
        Ok(answer.line()?)
    } else {
        Err(RefRefusal::Detached)
    }
}

/// A ref that answers `symbolic-ref` is a link, not a branch. `git` answers
/// here rather than failing (**measured**: with `HEAD` symbolic to `a` and `a`
/// to `b`, `symbolic-ref refs/heads/a` answered `refs/heads/b`), which is
/// exactly what makes the check possible.
pub fn must_be_direct(git: &Git, reference: &str) -> Result<(), RefRefusal> {
    let answer = git.run(&["symbolic-ref", "--quiet", reference])?;
    if answer.ok() {
        Err(RefRefusal::Chained {
            reference: reference.to_owned(),
            target: answer.line().unwrap_or_default(),
        })
    } else {
        Ok(())
    }
}

/// The commit a ref names. `^{commit}` so that a tag or a ref naming
/// something no tree can be read from is refused here rather than at
/// `ls-tree`.
pub fn tip_of(git: &Git, reference: &str) -> Result<String, RefRefusal> {
    let argument = format!("{reference}^{{commit}}");
    let answer = git.run(&["rev-parse", "--verify", "--quiet", &argument])?;
    if answer.ok() {
        let tip = answer.line()?;
        if tip.is_empty() {
            Err(RefRefusal::Unborn(reference.to_owned()))
        } else {
            Ok(tip)
        }
    } else {
        Err(RefRefusal::Unborn(reference.to_owned()))
    }
}

/// Step 1, whole: the backend, the ref `HEAD` names directly, that it is not
/// itself symbolic, and its tip.
pub fn lease(git: &Git) -> Result<Lease, RefRefusal> {
    backend_is_files(git)?;
    let reference = head_names(git)?;
    must_be_direct(git, &reference)?;
    let tip = tip_of(git, &reference)?;
    Ok(Lease { reference, tip })
}

/// A transaction that has reached `prepare: ok` and is holding the branch's
/// lock, with the new value already written into it.
#[derive(Debug)]
pub struct Prepared {
    session: Session,
}

/// Open the transaction and prepare the write, without committing it.
///
/// `option no-deref` and the old value together are the compare-and-swap:
/// `prepare` refuses a moved tip. What it does *not* test is the ref's type —
/// its old-value comparison follows symbolic indirection even with
/// `--no-deref`, so a symbolic branch pointing at exactly the recorded tip
/// passed and was overwritten as direct before any post-write check could see
/// it. That is why [`Prepared::still_direct`] exists and why `commit` is never
/// sent before it answers.
pub fn prepare(git: &Git, lease: &Lease, new: &str) -> Result<Prepared, RefRefusal> {
    let mut session = git.interactive(&["update-ref", "--stdin"])?;
    // Each transaction command is acknowledged on its own line, `start`
    // included, so the answers are read one at a time rather than the last one
    // being read as the first. A protocol that read `start: ok` as the
    // preparation's answer would commit an unprepared transaction.
    session.send("start\n")?;
    let started = session.line()?;
    if started.trim() != "start: ok" {
        let run = session.finish()?;
        return Err(RefRefusal::Unexpected(format!(
            "the ref transaction answered `{}`: {}",
            started.trim(),
            String::from_utf8_lossy(&run.stderr).trim()
        )));
    }
    session.send(&format!(
        "option no-deref\nupdate {} {} {}\nprepare\n",
        lease.reference, new, lease.tip
    ))?;
    // A refused preparation does not answer: `git` prints why on `stderr` and
    // exits, so the read ends with the stream closed rather than with a line.
    // That is a refusal, not a broken protocol — the compare-and-swap doing
    // its job, or a ref another `git` holds — and it is reported with `git`'s
    // own words. A deadline is the one thing here that is *not* an answer.
    let acknowledgement = match session.line() {
        Ok(line) => Some(line),
        Err(deadline @ Failure::Deadline { .. }) => return Err(deadline.into()),
        Err(Failure::Unstartable(_) | Failure::Output(_)) => None,
    };
    if acknowledgement.as_deref().map(str::trim) != Some("prepare: ok") {
        let run = session.finish()?;
        let said = String::from_utf8_lossy(&run.stderr).trim().to_owned();
        return Err(RefRefusal::Moved {
            reference: lease.reference.clone(),
            expected: lease.tip.clone(),
            found: if said.is_empty() {
                "something else".to_owned()
            } else {
                said
            },
        });
    }
    Ok(Prepared { session })
}

impl Prepared {
    /// Asked of the **live** ref while the transaction holds its lock, before
    /// `commit` is sent.
    ///
    /// **Measured** on git 2.43's files backend: after `prepare`, the live ref
    /// still held its symbolic spelling while the branch lock held the new
    /// oid, so this check can see it; `abort` then preserved the ref byte for
    /// byte.
    pub fn still_direct(&self, git: &Git, lease: &Lease) -> Result<(), RefRefusal> {
        must_be_direct(git, &lease.reference)?;
        let tip = tip_of(git, &lease.reference)?;
        if tip == lease.tip {
            Ok(())
        } else {
            Err(RefRefusal::Moved {
                reference: lease.reference.clone(),
                expected: lease.tip.clone(),
                found: tip,
            })
        }
    }

    pub fn commit(mut self) -> Result<(), RefRefusal> {
        self.session.send("commit\n")?;
        let acknowledgement = self.session.line()?;
        let run = self.session.finish()?;
        if acknowledgement.trim() == "commit: ok" && run.code == Some(0) {
            Ok(())
        } else {
            Err(RefRefusal::Unexpected(format!(
                "the ref transaction answered `{}`: {}",
                acknowledgement.trim(),
                String::from_utf8_lossy(&run.stderr).trim()
            )))
        }
    }

    /// Never `commit`: a symbolic result, a failed read, or an unexpected
    /// response ends the transaction here, and the commit built in step 4
    /// stays unreferenced and is pruned as garbage.
    pub fn abort(mut self) -> Result<(), RefRefusal> {
        self.session.send("abort\n")?;
        let _ = self.session.line();
        let _ = self.session.finish()?;
        Ok(())
    }
}

/// The three-part check step 5 runs after the transaction, with `HEAD`'s lock
/// and the branch's own lock both retaken.
///
/// The immediate `HEAD` read is the part that has to be `--no-recurse`: the
/// bare command would accept `HEAD -> c -> <branch>`, leaving `c` free to move
/// under the two locks. A read error never satisfies a check.
pub fn post_write(git: &Git, lease: &Lease, commit: &str) -> Result<(), RefRefusal> {
    let head = head_names(git)?;
    if head != lease.reference {
        return Err(RefRefusal::HeadElsewhere {
            expected: lease.reference.clone(),
            found: head,
        });
    }
    must_be_direct(git, &lease.reference)?;
    let tip = tip_of(git, &lease.reference)?;
    if tip == commit {
        Ok(())
    } else {
        Err(RefRefusal::Moved {
            reference: lease.reference.clone(),
            expected: commit.to_owned(),
            found: tip,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::scratch_repo::Scratch;

    #[test]
    fn step_one_records_the_ref_head_names_directly_and_refuses_a_chain() {
        // The case ADR-0015's Limits section names for this step: start with
        // `HEAD -> a -> b` and assert refusal before anything is placed.
        let scratch = Scratch::new("lease");
        scratch.write("a.txt", b"one");
        let tip = scratch.commit("one");
        let git = scratch.runner();

        let lease = lease(&git).expect("an ordinary checkout leases");
        assert_eq!(lease.reference, "refs/heads/main");
        assert_eq!(lease.tip, tip);

        scratch.git(&["symbolic-ref", "refs/heads/hop", "refs/heads/main"]);
        scratch.git(&["symbolic-ref", "HEAD", "refs/heads/hop"]);

        let refusal = super::lease(&git).unwrap_err();
        let RefRefusal::Chained { reference, target } = &refusal else {
            panic!("expected the chain to be refused, got {refusal}");
        };
        assert_eq!(reference, "refs/heads/hop");
        assert_eq!(target, "refs/heads/main");
    }

    #[test]
    fn the_recursive_head_read_would_have_recorded_the_end_of_the_chain() {
        // The control the Limits section asks for, kept as a test rather than
        // a claim: restoring step 1's former recursive read makes it record
        // `b` and reach the placement checkpoint, which is precisely why the
        // refusal has to be built on `--no-recurse`.
        let scratch = Scratch::new("recursive");
        scratch.write("a.txt", b"one");
        scratch.commit("one");
        let git = scratch.runner();
        scratch.git(&["symbolic-ref", "refs/heads/hop", "refs/heads/main"]);
        scratch.git(&["symbolic-ref", "HEAD", "refs/heads/hop"]);

        let recursive = git.run(&["symbolic-ref", "HEAD"]).unwrap().line().unwrap();
        let direct = head_names(&git).unwrap();

        assert_eq!(recursive, "refs/heads/main", "the end of the chain");
        assert_eq!(direct, "refs/heads/hop", "the link the refusal fires on");
        assert!(
            must_be_direct(&git, &recursive).is_ok(),
            "a refusal built on the recursive answer could never fire"
        );
        assert!(must_be_direct(&git, &direct).is_err());
    }

    #[test]
    fn a_detached_head_and_an_unborn_branch_are_two_different_refusals() {
        let scratch = Scratch::new("detached");
        scratch.write("a.txt", b"one");
        let tip = scratch.commit("one");
        let git = scratch.runner();

        scratch.git(&["checkout", "-q", "--detach", &tip]);
        assert!(matches!(lease(&git).unwrap_err(), RefRefusal::Detached));

        scratch.git(&["symbolic-ref", "HEAD", "refs/heads/never-born"]);
        let refusal = lease(&git).unwrap_err();
        let RefRefusal::Unborn(reference) = &refusal else {
            panic!("expected an unborn branch, got {refusal}");
        };
        assert_eq!(reference, "refs/heads/never-born");
    }

    #[test]
    fn head_pointed_at_a_tag_over_a_blob_is_refused_rather_than_read_as_a_tree() {
        // `HEAD` takes any ref under `refs/`, and a tag naming a blob as
        // readily as one naming a commit; `ls-tree` then fails with `not a
        // tree object`. `^{commit}` refuses it here instead.
        let scratch = Scratch::new("blob-tag");
        scratch.write("a.txt", b"one");
        scratch.commit("one");
        let git = scratch.runner();
        let blob = scratch.git(&["hash-object", "-w", "a.txt"]);
        scratch.git(&["tag", "blobtag", &blob]);
        scratch.git(&["symbolic-ref", "HEAD", "refs/tags/blobtag"]);

        assert!(matches!(lease(&git).unwrap_err(), RefRefusal::Unborn(_)));
    }

    #[test]
    fn a_prepared_transaction_refuses_a_same_tip_symbolic_rewrite_and_preserves_it() {
        // #385's case. The old single-shot `update-ref --no-deref` did not
        // test type: its old-value comparison follows symbolic indirection, so
        // a symbolic branch pointing at exactly the tip passed and was
        // overwritten as direct. `prepare`, ask, then `commit` is what sees
        // it.
        let scratch = Scratch::new("same-tip");
        scratch.write("a.txt", b"one");
        let tip = scratch.commit("one");
        scratch.write("a.txt", b"two");
        let next = scratch.commit("two");
        scratch.git(&["reset", "-q", "--hard", &tip]);
        let git = scratch.runner();
        let lease = lease(&git).unwrap();

        // Between step 1 and step 5 the branch becomes symbolic, at the same
        // tip: `sibling` is exactly where `main` was.
        scratch.git(&["branch", "sibling", &tip]);
        scratch.git(&["symbolic-ref", "refs/heads/main", "refs/heads/sibling"]);

        let prepared = prepare(&git, &lease, &next).expect("the tip still matches");
        let refusal = prepared
            .still_direct(&git, &lease)
            .expect_err("the branch is symbolic now");
        assert!(matches!(refusal, RefRefusal::Chained { .. }));
        prepared.abort().unwrap();

        let spelling = std::fs::read_to_string(scratch.path(".git/refs/heads/main")).unwrap();
        assert_eq!(
            spelling.trim(),
            "ref: refs/heads/sibling",
            "the symbolic spelling survives byte for byte"
        );
        assert_eq!(
            scratch.git(&["rev-parse", "refs/heads/sibling"]),
            tip,
            "and the sibling did not move"
        );
    }

    #[test]
    fn a_prepared_transaction_commits_an_ordinary_branch_and_refuses_a_moved_one() {
        let scratch = Scratch::new("commit");
        scratch.write("a.txt", b"one");
        let tip = scratch.commit("one");
        scratch.write("a.txt", b"two");
        let next = scratch.commit("two");
        scratch.git(&["reset", "-q", "--hard", &tip]);
        let git = scratch.runner();
        let lease = lease(&git).unwrap();

        let prepared = prepare(&git, &lease, &next).unwrap();
        prepared.still_direct(&git, &lease).unwrap();
        prepared.commit().unwrap();
        assert_eq!(scratch.git(&["rev-parse", "refs/heads/main"]), next);
        post_write(&git, &lease, &next).expect("HEAD still names the branch it moved");

        // A stale tip never reaches `prepare: ok`.
        let stale = Lease {
            reference: "refs/heads/main".to_owned(),
            tip,
        };
        assert!(matches!(
            prepare(&git, &stale, &next).unwrap_err(),
            RefRefusal::Moved { .. }
        ));
    }

    #[test]
    fn a_head_hop_inserted_after_the_commit_fails_the_post_write_check() {
        // #386's case, in the window between the transaction releasing its
        // locks and the UI retaking its own: the bare `symbolic-ref HEAD`
        // would accept `HEAD -> c -> <branch>`, leaving `c` free to move under
        // the two locks.
        let scratch = Scratch::new("head-hop");
        scratch.write("a.txt", b"one");
        let tip = scratch.commit("one");
        scratch.write("a.txt", b"two");
        let next = scratch.commit("two");
        scratch.git(&["reset", "-q", "--hard", &tip]);
        let git = scratch.runner();
        let lease = lease(&git).unwrap();

        let prepared = prepare(&git, &lease, &next).unwrap();
        prepared.still_direct(&git, &lease).unwrap();
        prepared.commit().unwrap();

        scratch.git(&["symbolic-ref", "refs/heads/hop", "refs/heads/main"]);
        scratch.git(&["symbolic-ref", "HEAD", "refs/heads/hop"]);

        let refusal = post_write(&git, &lease, &next).unwrap_err();
        let RefRefusal::HeadElsewhere { expected, found } = &refusal else {
            panic!("expected the hop to be refused, got {refusal}");
        };
        assert_eq!(expected, "refs/heads/main");
        assert_eq!(found, "refs/heads/hop");
        // The control: the recursive read this protocol does not use would
        // have accepted it.
        assert_eq!(
            git.run(&["symbolic-ref", "HEAD"]).unwrap().line().unwrap(),
            "refs/heads/main"
        );
    }

    #[test]
    fn a_ref_backend_whose_locks_are_not_files_is_refused_rather_than_locked() {
        let scratch = Scratch::new("backend");
        scratch.write("a.txt", b"one");
        scratch.commit("one");
        let git = scratch.runner();
        assert!(backend_is_files(&git).is_ok());

        scratch.git(&["config", "extensions.refStorage", "reftable"]);
        let refusal = backend_is_files(&git).unwrap_err();
        let RefRefusal::Backend(name) = &refusal else {
            panic!("expected the backend to be named, got {refusal}");
        };
        assert_eq!(name, "reftable");
    }
}
