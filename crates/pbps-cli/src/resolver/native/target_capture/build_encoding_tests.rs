//! Pure final native-build encoding oracles, below the public capture API.

use super::*;
use pbps_db::fingerprint::FingerprintKey;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

struct Key {
    root: std::path::PathBuf,
    selected: EnvironmentFingerprintKey,
}

impl Key {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "pbps-1274-final-build-{:032x}",
            rand::random::<u128>()
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

fn identity(role: ExecutableRole, path: &str, digest: char) -> ExecutableIdentity {
    ExecutableIdentity {
        role,
        path: path.into(),
        digest: Some(digest.to_string().repeat(64)),
        provenance: Provenance::LoadedContent,
        disk_differs_from_loaded: Some(false),
    }
}

#[test]
fn final_build_fingerprint_distinguishes_equal_inventories_with_different_required_bindings() {
    let key = Key::new();
    let engine = identity(ExecutableRole::Engine, "/engine/bin/postgres", 'e');
    let libraries = [
        identity(ExecutableRole::Preloaded, "/engine/lib/foo", 'a'),
        identity(ExecutableRole::Preloaded, "/engine/lib/bar", 'b'),
    ];
    // The nested PostgreSQL test proves these correspond to foo->A/bar->B
    // and foo->B/bar->A. This final stage must retain their distinct digests
    // even though the measured executable inventory multiset is identical.
    let foo_a_bar_b = "a".repeat(64);
    let foo_b_bar_a = "b".repeat(64);
    let first =
        fingerprint_qualified_build(&key.selected, &engine, &libraries, &foo_a_bar_b).unwrap();
    let second =
        fingerprint_qualified_build(&key.selected, &engine, &libraries, &foo_b_bar_a).unwrap();
    assert_ne!(
        first, second,
        "dropping the required-association digest collapses the build seal"
    );
    assert_eq!(
        first,
        fingerprint_qualified_build(
            &key.selected,
            &engine,
            &[libraries[1].clone(), libraries[0].clone()],
            &foo_a_bar_b,
        )
        .unwrap(),
        "the full inventory order is not a loader-content association"
    );
}
