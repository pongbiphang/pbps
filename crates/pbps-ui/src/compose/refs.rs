//! Fresh output refs only. A prepared Git transaction owns the deciding lock.

use serde::Serialize;

use super::{
    Error, Result,
    git::{Git, text},
    process::Transaction,
    record::oid,
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum RefEvidence {
    Absent,
    Direct(String),
    Symbolic,
    Unreadable,
}

pub(super) fn observe(git: &Git, reference: &str) -> RefEvidence {
    let read = || -> Result<RefEvidence> {
        let symbolic = git.output(&["symbolic-ref", "-q", reference], &[], None)?;
        if symbolic.status.success() {
            return Ok(RefEvidence::Symbolic);
        }
        if symbolic.status.code() != Some(1)
            || !symbolic.stdout.is_empty()
            || !symbolic.stderr.is_empty()
        {
            return Ok(RefEvidence::Unreadable);
        }
        let output = git.output(
            &[
                "for-each-ref",
                "--format=%(refname)%00%(objectname)%00%(symref)",
                "--",
                reference,
            ],
            &[],
            None,
        )?;
        if !output.status.success() || !output.stderr.is_empty() {
            return Ok(RefEvidence::Unreadable);
        }
        if output.stdout.is_empty() {
            return Ok(RefEvidence::Absent);
        }
        let value = text(output.stdout)?;
        let mut found = None;
        for line in value.lines() {
            let parts: Vec<_> = line.split('\0').collect();
            if parts.len() != 3 || !oid(parts[1]) {
                return Ok(RefEvidence::Unreadable);
            }
            if parts[0] == reference {
                found = Some(if parts[2].is_empty() {
                    RefEvidence::Direct(parts[1].into())
                } else {
                    RefEvidence::Symbolic
                });
            }
        }
        // A child ref prevents creating the desired namespace too.
        Ok(found.unwrap_or(RefEvidence::Unreadable))
    };
    read().unwrap_or(RefEvidence::Unreadable)
}

fn not_checked_out(git: &Git, reference: &str) -> Result<()> {
    let output = git.output(&["worktree", "list", "--porcelain", "-z"], &[], None)?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(Error::new("Cannot inspect checked-out branches"));
    }
    let data = String::from_utf8(output.stdout)
        .map_err(|_| Error::new("Unsupported worktree metadata"))?;
    for entry in data.split("\0\0").filter(|s| !s.is_empty()) {
        let fields: Vec<_> = entry.split('\0').collect();
        if !fields.first().is_some_and(|s| s.starts_with("worktree ")) {
            return Err(Error::new("Unreadable worktree membership"));
        }
        if fields.iter().any(|s| *s == format!("branch {reference}")) {
            return Err(Error::new("The output branch is checked out"));
        }
        if !fields.contains(&"bare") {
            let head = fields.iter().find_map(|s| s.strip_prefix("HEAD "));
            let branch = fields.iter().find_map(|s| s.strip_prefix("branch "));
            if head.is_none_or(|s| !oid(s))
                || (head.is_some_and(|s| s.bytes().all(|b| b == b'0')) && branch.is_none())
            {
                return Err(Error::new("A worktree HEAD is unreadable"));
            }
        }
    }
    Ok(())
}

pub(super) struct Prepared(Transaction);

impl Prepared {
    pub fn delete_owned(git: &Git, reference: &str, value: &str) -> Result<()> {
        let mut command = git.command();
        command.args([
            "-c",
            "core.fsync=all",
            "-c",
            "core.fsyncMethod=fsync",
            "update-ref",
            "--stdin",
        ]);
        let mut transaction = Transaction::start(command, git.deadline)?;
        transaction.exchange("start\n", b"start: ok\n")?;
        transaction.exchange(
            &format!("option no-deref\ndelete {reference} {value}\nprepare\n"),
            b"prepare: ok\n",
        )?;
        if observe(git, reference) != RefEvidence::Direct(value.to_owned()) {
            transaction.exchange("abort\n", b"abort: ok\n")?;
            transaction.finish()?;
            return Err(Error::new("The owned private pin changed; preserve it"));
        }
        transaction.exchange("commit\n", b"commit: ok\n")?;
        transaction.finish()
    }

    pub fn create(git: &Git, reference: &str, commit: &str) -> Result<Self> {
        let mut command = git.command();
        command.args([
            "-c",
            "core.fsync=all",
            "-c",
            "core.fsyncMethod=fsync",
            "update-ref",
            "--stdin",
        ]);
        let mut transaction = Transaction::start(command, git.deadline)?;
        transaction.exchange("start\n", b"start: ok\n")?;
        transaction.exchange(
            &format!("option no-deref\ncreate {reference} {commit}\nprepare\n"),
            b"prepare: ok\n",
        )?;
        // Expected-zero alone overwrites a dangling symbolic ref on Git 2.43.
        // These observations happen while the exact target lock is held.
        let admission = if observe(git, reference) != RefEvidence::Absent {
            Err(Error::new("The fresh output ref collides or is unreadable"))
        } else {
            not_checked_out(git, reference)
        };
        if let Err(error) = admission {
            transaction.exchange("abort\n", b"abort: ok\n")?;
            transaction.finish()?;
            return Err(error);
        }
        Ok(Self(transaction))
    }

    pub fn abort(mut self) -> Result<()> {
        self.0.exchange("abort\n", b"abort: ok\n")?;
        self.0.finish()
    }

    pub fn commit(mut self) -> Result<()> {
        self.0.exchange("commit\n", b"commit: ok\n")?;
        self.0.finish()
    }
}
