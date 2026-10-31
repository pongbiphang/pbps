//! `pbps key generate`: a fingerprint key for one environment (DEC-952.1).
//!
//! pbps makes the key and checks it (`pbps doctor`); it does not keep it. The
//! key goes where the environment's connection string already goes — a
//! variable the CI secret store sets, or a file the platform mounts — named in
//! `pbps.yml` by `fingerprint_key_env` or `fingerprint_key_file`.

use std::path::Path;

use pbps_db::fingerprint::FingerprintKey;

/// Prints a new key to stdout, or writes it to `out` readable by its owner
/// only. Its identifier goes to stderr either way, so stdout stays the key
/// alone for a pipe into a secret store.
pub fn cmd_generate(out: Option<&Path>) -> anyhow::Result<()> {
    let text = FingerprintKey::generate();
    let id = FingerprintKey::parse(&text, "the generated key")
        .expect("a generated key parses")
        .id();
    match out {
        Some(path) => {
            write_owner_only(path, &text)?;
            eprintln!(
                "wrote a new fingerprint key to {} (key id {id}).\n\
                 Name it in pbps.yml as the environment's `fingerprint_key_file`, and keep it \
                 out of version control.",
                path.display()
            );
        }
        None => {
            println!("{text}");
            eprintln!(
                "key id {id}. Store it in your secret store and name the variable that carries \
                 it as the environment's `fingerprint_key_env` in pbps.yml."
            );
        }
    }
    Ok(())
}

/// Creates `path` for its owner alone, and never over an existing file: a
/// replaced key silently invalidates every plan made under the old one.
#[cfg(unix)]
fn write_owner_only(path: &Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    create_key_with(
        path,
        |file| file.set_permissions(std::fs::Permissions::from_mode(0o600)),
        |file| writeln!(file, "{text}"),
        |path| std::fs::remove_file(path),
    )
}

/// Keep exclusive creation and cleanup in the production path; the operations
/// are per-call seams so failures can be exercised after a real file is made.
#[cfg(unix)]
fn create_key_with(
    path: &Path,
    restrict: impl FnOnce(&std::fs::File) -> std::io::Result<()>,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
    remove: impl FnOnce(&Path) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| {
            format!(
                "cannot create `{}` (an existing key is never overwritten)",
                path.display()
            )
        })?;
    // A failed create owns nothing to remove. Once it succeeds, both operations
    // must return through cleanup: otherwise a retry sees a partial key as an
    // existing one. This assumes the ordinary user-selected output directory,
    // not an attacker concurrently replacing its entries.
    let result = (|| {
        // open's mode is filtered by the umask; 0777 would leave the key
        // unreadable even to its owner. Restore permissions on the held file.
        restrict(&file)
            .with_context(|| format!("cannot restrict `{}` to its owner", path.display()))?;
        write(&mut file).with_context(|| format!("cannot write `{}`", path.display()))
    })();
    drop(file);
    if let Err(error) = result {
        return match remove(path) {
            Ok(()) => Err(error),
            Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(error),
            Err(cleanup) => Err(error.context(format!(
                "cannot remove `{}` after failed key generation: {cleanup}; an incomplete key may remain",
                path.display()
            ))),
        };
    }
    Ok(())
}

/// Where owner-only cannot be set it cannot be checked either, and `pbps.yml`
/// would then refuse the very file this wrote.
#[cfg(not(unix))]
fn write_owner_only(path: &Path, _text: &str) -> anyhow::Result<()> {
    anyhow::bail!(
        "cannot make `{}` readable by its owner only on this platform; run \
         `pbps key generate` without --out and store the key in your secret store, named \
         by `fingerprint_key_env`",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pbps-key-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn a_written_key_is_owner_only_and_loads_back() {
        let dir = dir();
        let path = dir.join("prod.key");
        cmd_generate(Some(&path)).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(FingerprintKey::from_file(&path).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_existing_key_file_is_never_overwritten() {
        let dir = dir();
        let path = dir.join("prod.key");
        std::fs::write(&path, "kept").unwrap();
        assert!(cmd_generate(Some(&path)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(not(unix))]
    #[test]
    fn a_key_file_is_not_written_where_owner_only_cannot_be_set() {
        let dir = dir();
        let path = dir.join("prod.key");
        assert!(cmd_generate(Some(&path)).is_err());
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    mod failure_cleanup {
        use super::*;
        use std::fs::File;
        use std::io::{self, Write as _};
        use std::os::unix::fs::PermissionsExt as _;

        // Public synthetic fixture, never an operator's key.
        const TEXT: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        const PREFIX: &[u8] = b"AAAAAAA";

        struct Fixture(std::path::PathBuf);

        impl Fixture {
            fn new() -> Self {
                Self(dir())
            }

            fn path(&self) -> std::path::PathBuf {
                self.0.join("prod.key")
            }

            fn assert_absent_and_retry(&self) {
                let path = self.path();
                assert_eq!(
                    std::fs::symlink_metadata(&path).unwrap_err().kind(),
                    io::ErrorKind::NotFound
                );
                cmd_generate(Some(&path)).unwrap();
                assert!(FingerprintKey::from_file(&path).is_ok());
                assert_eq!(
                    std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }

        fn restrict(file: &File) -> io::Result<()> {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
        }

        fn partial_write(file: &mut File) -> io::Result<()> {
            file.write_all(PREFIX)?;
            assert_eq!(file.metadata()?.len(), PREFIX.len() as u64);
            Err(io::Error::other("write fault marker"))
        }

        #[test]
        fn a_permission_failure_removes_the_created_key_before_retry() {
            let fixture = Fixture::new();
            let error = create_key_with(
                &fixture.path(),
                |_| Err(io::Error::other("permission fault marker")),
                |_| panic!("writing must follow successful permission restoration"),
                |path| std::fs::remove_file(path),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("permission fault marker"));
            fixture.assert_absent_and_retry();
        }

        #[test]
        fn a_zero_byte_write_failure_leaves_no_key_and_retry_succeeds() {
            let fixture = Fixture::new();
            let error = create_key_with(
                &fixture.path(),
                restrict,
                |file| {
                    assert_eq!(file.metadata()?.len(), 0);
                    Err(io::Error::other("zero write fault marker"))
                },
                |path| std::fs::remove_file(path),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("zero write fault marker"));
            fixture.assert_absent_and_retry();
        }

        #[test]
        fn a_partial_write_failure_leaves_no_key_and_retry_succeeds() {
            let fixture = Fixture::new();
            let error = create_key_with(&fixture.path(), restrict, partial_write, |path| {
                std::fs::remove_file(path)
            })
            .unwrap_err();
            assert!(format!("{error:#}").contains("write fault marker"));
            fixture.assert_absent_and_retry();
        }

        #[test]
        fn failed_cleanup_reports_both_faults_and_possible_residue_without_the_key() {
            let fixture = Fixture::new();
            let path = fixture.path();
            let error = create_key_with(&path, restrict, partial_write, |_| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cleanup fault marker",
                ))
            })
            .unwrap_err();
            assert_eq!(std::fs::read(&path).unwrap(), PREFIX);
            // main displays anyhow errors with this exact alternate Display.
            for message in [format!("{error:#}"), format!("{error:?}")] {
                assert!(message.contains("write fault marker"));
                assert!(message.contains("cleanup fault marker"));
                assert!(message.contains("an incomplete key may remain"));
                assert!(message.contains(path.to_str().unwrap()));
                assert!(!message.contains(TEXT));
                assert!(!message.contains(std::str::from_utf8(PREFIX).unwrap()));
            }
            assert!(
                error
                    .chain()
                    .any(|cause| cause.to_string() == "write fault marker")
            );
        }

        #[test]
        fn already_removed_output_preserves_the_write_fault_without_a_residue_claim() {
            let fixture = Fixture::new();
            let error = create_key_with(&fixture.path(), restrict, partial_write, |path| {
                std::fs::remove_file(path)?;
                // The real entry is gone before NotFound is reported.
                std::fs::remove_file(path)
            })
            .unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("write fault marker"));
            assert!(!message.contains("may remain"));
            fixture.assert_absent_and_retry();
        }

        #[test]
        fn failed_exclusive_creation_never_calls_post_create_operations_or_cleanup() {
            use std::os::unix::fs::symlink;
            for kind in ["file", "directory", "symlink", "dangling symlink"] {
                let fixture = Fixture::new();
                let path = fixture.path();
                let target = fixture.0.join("target");
                match kind {
                    "file" => {
                        std::fs::write(&path, "kept").unwrap();
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
                            .unwrap();
                    }
                    "directory" => {
                        std::fs::create_dir(&path).unwrap();
                        std::fs::write(path.join("kept"), "kept").unwrap();
                    }
                    "symlink" => {
                        std::fs::write(&target, "kept target").unwrap();
                        symlink(&target, &path).unwrap();
                    }
                    "dangling symlink" => symlink(&target, &path).unwrap(),
                    _ => unreachable!(),
                }
                let before = std::fs::symlink_metadata(&path).unwrap();
                let error = create_key_with(
                    &path,
                    |_| panic!("failed exclusive create must not restrict permissions"),
                    |_| panic!("failed exclusive create must not write"),
                    |_| panic!("failed exclusive create must not remove an existing entry"),
                )
                .unwrap_err();
                assert!(format!("{error:#}").contains("an existing key is never overwritten"));
                let after = std::fs::symlink_metadata(&path).unwrap();
                assert_eq!(before.file_type(), after.file_type());
                assert_eq!(before.permissions().mode(), after.permissions().mode());
                match kind {
                    "file" => assert_eq!(std::fs::read(&path).unwrap(), b"kept"),
                    "directory" => assert_eq!(std::fs::read(path.join("kept")).unwrap(), b"kept"),
                    "symlink" => {
                        assert_eq!(std::fs::read_link(&path).unwrap(), target);
                        assert_eq!(std::fs::read(&target).unwrap(), b"kept target");
                    }
                    "dangling symlink" => {
                        assert_eq!(std::fs::read_link(&path).unwrap(), target);
                        assert_eq!(
                            std::fs::symlink_metadata(&target).unwrap_err().kind(),
                            io::ErrorKind::NotFound
                        );
                    }
                    _ => unreachable!(),
                }
            }
        }

        #[test]
        fn a_complete_key_is_retained_without_calling_cleanup() {
            let fixture = Fixture::new();
            let path = fixture.path();
            create_key_with(
                &path,
                restrict,
                |file| writeln!(file, "{TEXT}"),
                |_| panic!("successful generation must not clean up its key"),
            )
            .unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{TEXT}\n"));
            assert!(FingerprintKey::from_file(&path).is_ok());
        }
    }
}
