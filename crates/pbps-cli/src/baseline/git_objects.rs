//! One raw Git object stream for a historical declaration tree (DEC-1793.1).

use anyhow::Context as _;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// The tree mode of a submodule entry.
pub(crate) const SUBMODULE: &str = "160000";
/// The tree mode of a symbolic link.
pub(crate) const SYMLINK: &str = "120000";

struct Entry {
    mode: String,
    oid: String,
    path: String,
}

/// The full-tree listing's entries `wanted` accepts, given each one's path and
/// mode. A wanted entry must be a blob or a submodule commit; whether its mode
/// is a regular file is the consumer's to say, and a submodule is passed on
/// rather than dropped so that a consumer which cares can refuse it.
fn entries(bytes: &[u8], wanted: impl Fn(&str, &str) -> bool) -> anyhow::Result<Vec<Entry>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    anyhow::ensure!(
        bytes.ends_with(b"\0"),
        "Git returned a truncated tree listing"
    );
    let mut result = Vec::new();
    for row in bytes[..bytes.len() - 1].split(|b| *b == 0) {
        let row = std::str::from_utf8(row).context("Git tree paths must be UTF-8")?;
        let (header, path) = row
            .split_once('\t')
            .context("Git returned an invalid tree entry")?;
        let fields: Vec<_> = header.split(' ').collect();
        let [mode, kind, oid] = fields.as_slice() else {
            anyhow::bail!("Git returned an invalid tree header");
        };
        anyhow::ensure!(
            mode.len() == 6
                && mode.bytes().all(|b| (b'0'..=b'7').contains(&b))
                && matches!(oid.len(), 40 | 64)
                && oid.bytes().all(|b| b.is_ascii_hexdigit())
                && !path.is_empty(),
            "Git returned an invalid tree entry for `{path}`"
        );
        if wanted(path, mode) {
            anyhow::ensure!(
                matches!(*kind, "blob" | "commit"),
                "declaration `{path}` is a Git {kind}, not a blob"
            );
            result.push(Entry {
                mode: (*mode).to_owned(),
                oid: (*oid).to_owned(),
                path: path.to_owned(),
            });
        }
    }
    Ok(result)
}

/// The listing remains NUL-delimited, full-tree and untrimmed (DECISIONS 180).
/// Object IDs bind the stream to that listing even if a revision name moves.
pub(super) fn read(
    root: &Path,
    rev: &str,
    rel: &str,
    mut consume: impl FnMut(&str, &str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    read_with(
        root,
        rev,
        rel,
        |path, _mode| path.ends_with(".yml") || path.ends_with(".yaml"),
        // Retain the existing Git reader's text decoding. Only paths, which
        // select objects, must not silently change spelling through replacement.
        |path, mode, bytes| {
            // Said as the listing said it before submodules were passed on:
            // which directory at which revision could not be read.
            anyhow::ensure!(
                mode != SUBMODULE,
                "cannot read `{rel}` at `{rev}`: declaration `{path}` is a Git commit, not a blob"
            );
            consume(path, &String::from_utf8_lossy(bytes))
        },
    )
}

/// [`read`] for a consumer that needs the raw bytes and the entry's mode: a
/// hash is of the bytes, not of their lossy decoding, and a symlink or
/// submodule entry is not a file whose bytes mean anything.
pub(crate) fn read_with(
    root: &Path,
    rev: &str,
    rel: &str,
    wanted: impl Fn(&str, &str) -> bool,
    mut consume: impl FnMut(&str, &str, &[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let listing = super::git_command(root)
        .args(["ls-tree", "-r", "--full-tree", "-z", rev, "--", rel])
        .output()
        .context("cannot run git")?;
    anyhow::ensure!(
        listing.status.success(),
        "cannot read `{rel}` at `{rev}`: {}",
        String::from_utf8_lossy(&listing.stderr).trim()
    );
    let listing = entries(&listing.stdout, wanted)
        .with_context(|| format!("cannot read `{rel}` at `{rev}`"))?;
    if listing.is_empty() {
        return Ok(());
    }
    let mut command = super::git_command(root);
    // No --buffer, filters or text conversion: one request yields one raw
    // response without a second producer thread or project-controlled command.
    command.args(["cat-file", "--batch"]);
    let mut reader = BlobReader::start(command)?;
    for entry in listing {
        if entry.mode == SUBMODULE {
            // A gitlink names a commit in another repository, not an object
            // this one can give; the consumer decides what that means.
            consume(&entry.path, &entry.mode, &[])?;
            continue;
        }
        let bytes = reader
            .read(&entry.oid)
            .with_context(|| format!("cannot read `{}` at `{rev}`", entry.path))?;
        consume(&entry.path, &entry.mode, &bytes)?;
    }
    reader
        .finish()
        .with_context(|| format!("cannot finish reading `{rel}` at `{rev}`"))
}

fn reply(reader: &mut impl BufRead, expected: &str) -> anyhow::Result<Vec<u8>> {
    let mut header = Vec::new();
    (&mut *reader).take(256).read_until(b'\n', &mut header)?;
    let header = header
        .strip_suffix(b"\n")
        .context("Git omitted or truncated an object header")?;
    let header = std::str::from_utf8(header).context("Git returned an invalid object header")?;
    let fields: Vec<_> = header.split(' ').collect();
    let [oid, "blob", size] = fields.as_slice() else {
        anyhow::bail!("Git did not return the requested blob: {header}");
    };
    anyhow::ensure!(
        *oid == expected,
        "Git returned a different object: {header}"
    );
    anyhow::ensure!(
        !size.is_empty() && size.bytes().all(|b| b.is_ascii_digit()),
        "Git returned an invalid blob length"
    );
    let size: u64 = size
        .parse()
        .context("Git returned an invalid blob length")?;
    // Grow only for bytes actually received, never preallocate a claimed size.
    let mut body = Vec::new();
    (&mut *reader).take(size).read_to_end(&mut body)?;
    anyhow::ensure!(body.len() as u64 == size, "Git truncated an object body");
    let mut newline = [0];
    reader
        .read_exact(&mut newline)
        .context("Git omitted an object's trailing LF")?;
    anyhow::ensure!(
        newline == *b"\n",
        "Git returned an invalid object terminator"
    );
    Ok(body)
}

fn eof(reader: &mut impl Read) -> anyhow::Result<()> {
    let mut byte = [0];
    anyhow::ensure!(
        reader.read(&mut byte)? == 0,
        "Git returned unsolicited object bytes"
    );
    Ok(())
}

struct BlobReader {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    errors: Stderr,
    reaped: bool,
}

impl BlobReader {
    fn start(mut command: Command) -> anyhow::Result<Self> {
        let errors = Stderr::new()?;
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(
                errors
                    .file
                    .as_ref()
                    .expect("owned stderr file")
                    .try_clone()?,
            )
            .spawn()
            .context("cannot run git cat-file")?;
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(Self {
            child,
            input,
            output,
            errors,
            reaped: false,
        })
    }

    fn read(&mut self, oid: &str) -> anyhow::Result<Vec<u8>> {
        let input = self
            .input
            .as_mut()
            .context("Git object reader is already closed")?;
        writeln!(input, "{oid}").context("cannot request a Git object")?;
        input.flush()?;
        reply(&mut self.output, oid)
    }

    fn finish(mut self) -> anyhow::Result<()> {
        self.input.take();
        if let Err(error) = eof(&mut self.output) {
            self.stop();
            return Err(error.context(self.errors.text()?));
        }
        let status = self.child.wait()?;
        self.reaped = true;
        anyhow::ensure!(
            status.success(),
            "git cat-file failed ({status}): {}",
            self.errors.text()?
        );
        Ok(())
    }

    fn stop(&mut self) {
        if !self.reaped {
            self.input.take();
            let _ = self.child.kill();
            self.reaped = self.child.wait().is_ok();
        }
    }
}

impl Drop for BlobReader {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A pipe drained only at finish can deadlock a lockstep stdout reader.
/// An owned file avoids that backpressure without retaining all diagnostics
/// in memory or changing inherited Git tracing/configuration.
struct Stderr {
    file: Option<File>,
    path: PathBuf,
}

impl Stderr {
    fn new() -> anyhow::Result<Self> {
        let path =
            std::env::temp_dir().join(format!("pbps-git-{:032x}.stderr", rand::random::<u128>()));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .context("cannot create Git diagnostic capture")?;
        Ok(Self {
            file: Some(file),
            path,
        })
    }

    fn text(&mut self) -> anyhow::Result<String> {
        let file = self.file.as_mut().expect("owned stderr file");
        file.rewind()?;
        let mut bytes = Vec::new();
        file.take(64 * 1024).read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
    }
}

impl Drop for Stderr {
    fn drop(&mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    const OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn object_replies_preserve_empty_and_binary_blobs_and_refuse_broken_framing() {
        let mut input = Cursor::new(format!("{OID} blob 0\n\n{OID} blob 3\nx\0y\n").into_bytes());
        assert_eq!(reply(&mut input, OID).unwrap(), b"");
        assert_eq!(reply(&mut input, OID).unwrap(), b"x\0y");
        eof(&mut input).unwrap();
        for bad in [
            String::new(),
            format!("{OID} missing\n"),
            format!("{OID} tree 0\n\n"),
            format!("{} blob 0\n\n", "b".repeat(40)),
            format!("{OID} blob 3\nx"),
            format!("{OID} blob 1\nx"),
            format!("{OID} blob 1\nx!"),
            format!("{OID} blob -1\n"),
            format!("{OID} blob 18446744073709551616\n"),
            format!("{OID} blob 1"),
            "x".repeat(300),
        ] {
            assert!(
                reply(&mut Cursor::new(bad.as_bytes()), OID).is_err(),
                "{bad:?}"
            );
        }
        assert!(eof(&mut Cursor::new(b"extra")).is_err());
    }

    fn yaml(path: &str, _mode: &str) -> bool {
        path.ends_with(".yml") || path.ends_with(".yaml")
    }

    #[test]
    fn tree_entries_preserve_path_bytes_and_refuse_truncation_and_non_blob_declarations() {
        let path = "schema/é space\tline\n'quote.yaml";
        let rows = entries(format!("100644 blob {OID}\t{path}\0").as_bytes(), yaml).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, path);
        assert_eq!(rows[0].oid, OID);
        assert!(entries(b"", yaml).unwrap().is_empty());
        let submodule = format!("160000 commit {OID}\tschema/submodule\0");
        assert!(entries(submodule.as_bytes(), yaml).unwrap().is_empty());
        // A consumer that names the mode sees the submodule instead of losing it.
        let kept = entries(submodule.as_bytes(), |_, mode| mode == SUBMODULE).unwrap();
        assert_eq!((kept.len(), kept[0].mode.as_str()), (1, SUBMODULE));
        for bad in [
            format!("100644 blob {OID}\tschema/t.yml").into_bytes(),
            format!("040000 tree {OID}\tschema/t.yml\0").into_bytes(),
            b"100644 blob bad\tschema/t.yml\0".to_vec(),
            format!("100644 blob {OID}\t\0").into_bytes(),
            b"invalid\0".to_vec(),
            [
                format!("100644 blob {OID}\tschema/").as_bytes(),
                b"\xff.yml\0",
            ]
            .concat(),
        ] {
            assert!(entries(&bad, yaml).is_err(), "{bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn batch_finish_drains_noisy_diagnostics_and_refuses_failed_or_extra_output() {
        for (script, success, message) in [
            ("head -c 1048576 /dev/zero >&2; cat >/dev/null", true, ""),
            (
                "printf 'fixture failure' >&2; exit 7",
                false,
                "fixture failure",
            ),
            ("printf extra", false, "unsolicited"),
        ] {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            let reader = BlobReader::start(command).unwrap();
            let path = reader.errors.path.clone();
            let result = reader.finish();
            assert_eq!(result.is_ok(), success, "{result:?}");
            if !success {
                assert!(format!("{:#}", result.unwrap_err()).contains(message));
            }
            assert!(!path.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn abandoning_a_batch_kills_and_reaps_its_child_and_removes_its_capture() {
        let mut command = Command::new("sh");
        command.args(["-c", "while :; do :; done"]);
        let reader = BlobReader::start(command).unwrap();
        let pid = reader.child.id().to_string();
        let path = reader.errors.path.clone();
        drop(reader);
        let alive = Command::new("kill")
            .args(["-0", &pid])
            .output()
            .unwrap()
            .status
            .success();
        if alive {
            // A failing regression control must not leave a busy child behind.
            Command::new("kill").args(["-KILL", &pid]).output().unwrap();
        }
        assert!(!alive, "the child must have been killed and reaped");
        assert!(!path.exists());
    }

    /// The baseline reader still refuses a submodule that looks like a
    /// declaration and still ignores one that does not: it is the old caller's
    /// behaviour, kept while plan tests ask for the mode.
    #[test]
    fn the_baseline_reader_refuses_a_submodule_named_like_a_declaration() {
        let dir = std::env::temp_dir().join(format!("pbps-gitobj-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t.t"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "one"]);
        let head = git(&["rev-parse", "HEAD"]);
        let link = |path: &str| {
            git(&[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{head},{path}"),
            ])
        };
        link("schema/vendor");
        git(&["commit", "-q", "-m", "plain submodule"]);
        read(&dir, "HEAD", "schema", |_, _| Ok(())).unwrap();
        link("schema/vendored.yml");
        git(&["commit", "-q", "-m", "yaml-named submodule"]);
        let refused = read(&dir, "HEAD", "schema", |_, _| Ok(())).unwrap_err();
        assert!(
            format!("{refused:#}").contains("cannot read `schema` at `HEAD`: declaration `schema/vendored.yml` is a Git commit, not a blob"),
            "{refused:#}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
