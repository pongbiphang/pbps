//! Pure required-library association tests for the native target seal.
//!
//! No library is loaded or replaced here. The private runtime resolution is
//! constructed inside its owner module and sealed with a persistent test key.

use super::*;
use pbps_db::fingerprint::{EnvironmentFingerprintKey, FingerprintKey};
use pbps_db::resolver::environment::{ExecutableIdentity, ExecutableRole, Provenance};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

struct Key {
    root: PathBuf,
    selected: EnvironmentFingerprintKey,
}

impl Key {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "pbps-1274-association-{}",
            crate::catalog::probe_token()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("key");
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", FingerprintKey::generate()).unwrap();
        let selected = EnvironmentFingerprintKey::from_file(&path).unwrap();
        Self { root, selected }
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn requirement(name: &str, digest: char) -> ExecutableIdentity {
    ExecutableIdentity {
        role: ExecutableRole::LateLoaded,
        path: format!("/run-local-loader/{name}"),
        digest: Some(digest.to_string().repeat(64)),
        provenance: Provenance::DiskCandidate,
        disk_differs_from_loaded: None,
    }
}

fn two_requirements() -> RuntimeResolution {
    RuntimeInputs {
        libraries: vec!["foo".into(), "bar".into()],
        dynamic_library_path: "$libdir".into(),
    }
    .resolve_native(Path::new("/usr/pgsql/bin/postgres"), Path::new("/data"))
}

#[test]
fn swapping_two_required_contents_changes_the_private_association_seal() {
    let key = Key::new();
    let resolution = two_requirements();
    let direct = [(0, requirement("foo", 'a')), (0, requirement("bar", 'b'))];
    let swapped = [(0, requirement("foo", 'b')), (0, requirement("bar", 'a'))];
    let first = resolution
        .seal_required_associations(&key.selected, &direct)
        .unwrap();
    let second = resolution
        .seal_required_associations(&key.selected, &swapped)
        .unwrap();
    assert_ne!(
        first, second,
        "equal content multisets do not prove the same loader bindings"
    );

    let mut path_only = direct.clone();
    path_only[0].1.path = "/another-run-local-loader/foo".into();
    path_only[1].1.path = "/another-run-local-loader/bar".into();
    assert_eq!(
        first,
        resolution
            .seal_required_associations(&key.selected, &path_only)
            .unwrap(),
        "the private logical requirement, candidate ordinal and content are unchanged"
    );
    assert_eq!(
        first,
        resolution
            .seal_required_associations(&key.selected, &direct)
            .unwrap(),
        "same key and same logical facts are deterministic"
    );
}

#[test]
fn missing_or_invalid_selected_required_content_refuses_instead_of_shortening_the_set() {
    let key = Key::new();
    let resolution = two_requirements();
    assert!(
        resolution
            .seal_required_associations(&key.selected, &[(0, requirement("foo", 'a'))])
            .is_err()
    );
    assert!(
        resolution
            .seal_required_associations(
                &key.selected,
                &[
                    (usize::MAX, requirement("foo", 'a')),
                    (0, requirement("bar", 'b'))
                ],
            )
            .is_err()
    );
}
