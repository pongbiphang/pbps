//! `pbps key generate`: a fingerprint key for one environment (DEC-952.1).
//!
//! pbps makes the key and checks it (`pbps doctor`); it does not keep it. The
//! key goes where the environment's connection string already goes — a
//! variable the CI secret store sets, or a file the platform mounts — named in
//! `pbps.yml` by `fingerprint_key_env` or `fingerprint_key_file`.

use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;
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
fn write_owner_only(path: &Path, text: &str) -> anyhow::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).with_context(|| {
        format!(
            "cannot create `{}` (an existing key is never overwritten)",
            path.display()
        )
    })?;
    writeln!(file, "{text}").with_context(|| format!("cannot write `{}`", path.display()))?;
    Ok(())
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

    #[test]
    fn a_written_key_is_owner_only_and_loads_back() {
        let dir = dir();
        let path = dir.join("prod.key");
        cmd_generate(Some(&path)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
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
}
