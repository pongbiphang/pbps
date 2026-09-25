//! Raw Git objects and private indexes; never the user's index or worktree.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use super::{Error, Result, process};

pub(super) struct Git {
    pub root: PathBuf,
    pub hooks: PathBuf,
    pub deadline: Duration,
}

/// The names of the variables the UI inherited. Each value is dropped here
/// and never leaves: this is the only environment read the UI makes, and
/// the exception is scoped to it (ADR-0015 decision 4, DEC-1050.1).
#[expect(
    clippy::disallowed_methods,
    reason = "reads only the names of inherited variables and discards every value"
)]
fn inherited_names() -> impl Iterator<Item = OsString> {
    std::env::vars_os().map(|(name, _)| name)
}

impl Git {
    pub fn command(&self) -> Command {
        let mut command = Command::new("git");
        // Keep the user's authentication mechanism, but never repository,
        // config-injection or trace-output overrides inherited by the UI.
        for name in inherited_names() {
            if name.to_string_lossy().starts_with("GIT_")
                && !matches!(
                    name.to_str(),
                    Some("GIT_SSH" | "GIT_SSH_COMMAND" | "GIT_SSH_VARIANT")
                )
            {
                command.env_remove(name);
            }
        }
        let mut hooks = OsString::from("core.hooksPath=");
        hooks.push(&self.hooks);
        command
            .current_dir(&self.root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .args([
                OsStr::new("--no-replace-objects"),
                OsStr::new("--literal-pathspecs"),
                OsStr::new("-c"),
                &hooks,
            ])
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
            ]);
        command
    }

    pub fn output<S: AsRef<OsStr>>(
        &self,
        args: &[S],
        input: &[u8],
        index: Option<&Path>,
    ) -> Result<Output> {
        let mut command = self.command();
        command.args(args);
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        process::run(command, input, self.deadline)
    }

    pub fn bytes<S: AsRef<OsStr>>(
        &self,
        args: &[S],
        input: &[u8],
        index: Option<&Path>,
    ) -> Result<Vec<u8>> {
        let result = self.output(args, input, index)?;
        if !result.status.success() {
            // Git/helper output may include authentication material even when
            // the arguments did not. Callers add safe operation context only.
            return Err(Error::new("Git refused the compose operation"));
        }
        Ok(result.stdout)
    }

    pub fn line(&self, args: &[&str]) -> Result<String> {
        text(self.bytes(args, &[], None)?)
    }

    pub fn config(&self, key: &str) -> Result<Option<String>> {
        let result = self.output(&["config", "--get", key], &[], None)?;
        match result.status.code() {
            Some(0) => Ok(Some(text(result.stdout)?)),
            Some(1) if result.stdout.is_empty() && result.stderr.is_empty() => Ok(None),
            _ => Err(Error::new("Could not read the compose Git configuration")),
        }
    }

    pub fn converts_line_endings(&self) -> Result<bool> {
        let Some(value) = self.config("core.autocrlf")? else {
            return Ok(false);
        };
        if value.eq_ignore_ascii_case("input") {
            return Ok(true);
        }
        // Git owns boolean spelling, including bare config keys and aliases.
        // `input` is the only non-boolean value admitted by core.autocrlf.
        match self
            .line(&["config", "--type=bool", "--get", "core.autocrlf"])?
            .as_str()
        {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(Error::new(
                "Could not read Git's line-ending conversion policy",
            )),
        }
    }

    pub fn store(&self, bytes: &[u8]) -> Result<String> {
        text(self.bytes(
            &["hash-object", "-w", "--no-filters", "--stdin"],
            bytes,
            None,
        )?)
    }

    pub fn ignored(&self, name: &str) -> Result<bool> {
        let mut command = self.command();
        // check-ignore accepts literal pathname records but rejects Git's
        // global literal-pathspec magic. No browser value becomes an option.
        // Let Git honor index membership: a force-added declaration is an
        // explicit tracked input. Its content still comes from capture, not
        // from the user's index; genuinely ignored untracked paths refuse.
        command.args(["--no-literal-pathspecs", "check-ignore", "-z", "--stdin"]);
        let input = format!("{name}\0");
        let result = process::run(command, input.as_bytes(), self.deadline)?;
        match result.status.code() {
            Some(0) => Ok(true),
            Some(1) if result.stdout.is_empty() && result.stderr.is_empty() => Ok(false),
            _ => Err(Error::new(
                "Could not read a new declaration's ignore status",
            )),
        }
    }
}

pub(super) fn text(bytes: Vec<u8>) -> Result<String> {
    let mut value = String::from_utf8(bytes)
        .map_err(|_| Error::new("Git returned unsupported non-text metadata"))?;
    // These Git commands append one LF. Any preceding CR/LF belongs to the
    // value; stripping it would silently change paths or destination identity.
    if value.pop() != Some('\n') {
        return Err(Error::new("Git returned unterminated metadata"));
    }
    Ok(value)
}
