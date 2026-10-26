//! Checked Git evidence for connected planning (DEC-2397.1).
use std::io::Write as _;
use std::path::Path;
use std::process::{Output, Stdio};

pub(crate) const SHALLOW_REMEDY: &str =
    "\nThis is a shallow clone; fetch the missing history. In CI, set `fetch-depth: 0`.";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Evidence {
    Contains,
    NoCheckout,
    GitUnavailable,
    Missing { shallow: bool },
    Diverged,
    Incomplete,
    Unreadable(String),
}

fn run(root: &Path, args: &[&str]) -> std::io::Result<Output> {
    super::git_command(root)
        .env("LC_ALL", "C")
        .args(args)
        .output()
}

fn diagnostic(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.trim().is_empty() {
        format!("git exited with {}", output.status)
    } else {
        stderr.trim().to_owned()
    }
}

fn git_marker(root: &Path) -> std::io::Result<bool> {
    for dir in root.ancestors() {
        match dir.join(".git").symlink_metadata() {
            Ok(_) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(std::env::var_os("GIT_DIR").is_some())
}

pub(crate) fn check(root: &Path, sha: &str) -> Evidence {
    // Relative or symlinked project paths must find the physical repository's
    // ancestors too; failing to resolve them is unreadable evidence, not absence.
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(e) => return Evidence::Unreadable(e.to_string()),
    };
    let root = root.as_path();
    // Exit 128 also covers corrupt metadata and denied access. Only a known
    // non-repository is the no-evidence case; an unreadable repository refuses.
    match run(root, &["rev-parse", "--git-dir"]) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Evidence::GitUnavailable,
        Err(e) => return Evidence::Unreadable(e.to_string()),
        Ok(out) if !out.status.success() => {
            let message = diagnostic(&out);
            return match git_marker(root) {
                Ok(false) if message.starts_with("fatal: not a git repository") => {
                    Evidence::NoCheckout
                }
                Err(e) => Evidence::Unreadable(e.to_string()),
                _ => Evidence::Unreadable(message),
            };
        }
        Ok(_) => {}
    }
    let shallow = match run(root, &["rev-parse", "--is-shallow-repository"]) {
        Ok(out) if out.status.success() => out.stdout.trim_ascii() == b"true",
        Ok(out) => return Evidence::Unreadable(diagnostic(&out)),
        Err(e) => return Evidence::Unreadable(e.to_string()),
    };
    // A recorded object id is data, never revision syntax or a command option.
    if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Evidence::Missing { shallow };
    }
    // Batch-check can report `missing` with success even for a corrupt object;
    // measured Git also writes the inflate/unpack errors to stderr. A clean
    // missing response and an unreadable object store are different evidence.
    let object = (|| -> std::io::Result<Output> {
        let mut child = super::git_command(root)
            .env("LC_ALL", "C")
            .arg("cat-file")
            .arg("--batch-check=%(objecttype)")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let write = writeln!(child.stdin.take().expect("piped stdin"), "{sha}");
        let output = child.wait_with_output()?;
        write?;
        Ok(output)
    })();
    match object {
        Ok(out) if !out.status.success() => return Evidence::Unreadable(diagnostic(&out)),
        Ok(out) if out.stdout.trim_ascii() != b"commit" && !out.stderr.trim_ascii().is_empty() => {
            return Evidence::Unreadable(diagnostic(&out));
        }
        Ok(out) if out.stdout.trim_ascii() != b"commit" => return Evidence::Missing { shallow },
        Err(e) => return Evidence::Unreadable(e.to_string()),
        Ok(_) => {}
    }
    match run(root, &["merge-base", "--is-ancestor", sha, "HEAD"]) {
        Ok(out) if out.status.success() => Evidence::Contains,
        Ok(out) if out.status.code() == Some(1) => {
            // A shallow boundary can hide the path from HEAD to an object that
            // happens to be present. It is still missing ancestry evidence.
            if shallow {
                Evidence::Incomplete
            } else {
                Evidence::Diverged
            }
        }
        Ok(out) => Evidence::Unreadable(diagnostic(&out)),
        Err(e) => Evidence::Unreadable(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("pbps-ancestry-{:032x}", rand::random::<u128>()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let out = super::run(root, args).unwrap();
        assert!(out.status.success(), "{}", diagnostic(&out));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    fn commit(root: &Path, text: &str) -> String {
        std::fs::write(root.join("file"), text).unwrap();
        git(root, &["add", "file"]);
        git(
            root,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t.t",
                "commit",
                "-qm",
                text,
            ],
        );
        git(root, &["rev-parse", "HEAD"])
    }
    fn repo() -> Temp {
        let dir = Temp::new();
        git(dir.path(), &["init", "-q"]);
        dir
    }

    #[test]
    fn equal_and_ancestor_pass_while_siblings_and_descendants_refuse() {
        let dir = repo();
        let a = commit(dir.path(), "a");
        assert_eq!(check(dir.path(), &a), Evidence::Contains);
        let b = commit(dir.path(), "b");
        assert_eq!(check(dir.path(), &a), Evidence::Contains);
        git(dir.path(), &["checkout", "-q", "--detach", &a]);
        assert_eq!(check(dir.path(), &b), Evidence::Diverged);
        commit(dir.path(), "c");
        assert_eq!(check(dir.path(), &b), Evidence::Diverged);
        let nested = dir.path().join("nested/project");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(check(&nested, &a), Evidence::Contains);
    }

    #[test]
    fn missing_commit_shallow_boundary_and_non_commit_are_distinct_from_no_checkout() {
        let dir = repo();
        let a = commit(dir.path(), "a");
        commit(dir.path(), "b");
        let clone = Temp::new();
        git(
            clone.path(),
            &[
                "clone",
                "-q",
                "--depth",
                "1",
                &format!(
                    "file:///{}",
                    dir.path()
                        .to_string_lossy()
                        .replace('\\', "/")
                        .trim_start_matches('/')
                ),
                ".",
            ],
        );
        assert_eq!(check(clone.path(), &a), Evidence::Missing { shallow: true });
        git(clone.path(), &["fetch", "-q", "--depth", "1", "origin", &a]);
        assert_eq!(check(clone.path(), &a), Evidence::Incomplete);
        assert_eq!(
            check(dir.path(), &"0".repeat(40)),
            Evidence::Missing { shallow: false }
        );
        let blob = git(dir.path(), &["rev-parse", "HEAD:file"]);
        assert_eq!(
            check(dir.path(), &blob),
            Evidence::Missing { shallow: false }
        );
        assert_eq!(
            check(dir.path(), "--help\n"),
            Evidence::Missing { shallow: false }
        );
        let plain = Temp::new();
        assert_eq!(check(plain.path(), &a), Evidence::NoCheckout);
    }

    #[test]
    fn relative_nested_projects_preserve_unreadable_git_evidence() {
        const CHILD: &str = "PBPS_TEST_RELATIVE_ANCESTRY_SHA";
        if let Ok(sha) = std::env::var(CHILD) {
            let root = Path::new(".");
            assert_eq!(check(root, &sha), Evidence::Contains);
            std::fs::write("../../.git/HEAD", "broken metadata\n").unwrap();
            let evidence = check(root, &sha);
            assert!(matches!(evidence, Evidence::Unreadable(_)), "{evidence:?}");
            std::fs::rename("../../.git", "../../damaged-git").unwrap();
            assert_eq!(check(root, &sha), Evidence::NoCheckout);
            let evidence = check(Path::new("missing-project"), &sha);
            assert!(matches!(evidence, Evidence::Unreadable(_)), "{evidence:?}");
            return;
        }
        let dir = repo();
        let sha = commit(dir.path(), "a");
        let nested = dir.path().join("nested/project");
        std::fs::create_dir_all(&nested).unwrap();
        // A child owns the relative working directory without changing it for
        // the other tests running in this process.
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "baseline::ancestry::tests::relative_nested_projects_preserve_unreadable_git_evidence",
                "--nocapture",
            ])
            .env(CHILD, sha)
            .current_dir(nested)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn unborn_and_damaged_repositories_never_become_no_evidence() {
        let dir = repo();
        assert!(matches!(
            check(dir.path(), &"0".repeat(40)),
            Evidence::Missing { .. }
        ));
        let a = commit(dir.path(), "a");
        let object = dir.path().join(".git/objects").join(&a[..2]).join(&a[2..]);
        std::fs::remove_file(&object).unwrap();
        std::fs::write(object, "damaged object\n").unwrap();
        assert!(matches!(check(dir.path(), &a), Evidence::Unreadable(_)));
        std::fs::write(dir.path().join(".git/HEAD"), "broken metadata\n").unwrap();
        assert!(matches!(check(dir.path(), &a), Evidence::Unreadable(_)));
    }
}
