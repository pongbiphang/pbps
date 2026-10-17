//! Replay real fixture bytes, without repeating capture and publication.

use super::*;
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Entry {
    Directory(u32),
    File(u32, Vec<u8>),
    Link(PathBuf),
}

fn tree(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn walk(root: &Path, relative: &Path, entries: &mut BTreeMap<PathBuf, Entry>) {
        for entry in fs::read_dir(root.join(relative)).unwrap() {
            let entry = entry.unwrap();
            let path = relative.join(entry.file_name());
            let metadata = entry.path().symlink_metadata().unwrap();
            let value = if metadata.is_symlink() {
                Entry::Link(fs::read_link(entry.path()).unwrap())
            } else if metadata.is_dir() {
                walk(root, &path, entries);
                Entry::Directory(metadata.mode() & 0o777)
            } else {
                assert!(metadata.is_file(), "special fixture entry: {path:?}");
                Entry::File(metadata.mode() & 0o777, fs::read(entry.path()).unwrap())
            };
            entries.insert(path, value);
        }
    }
    let mut entries = BTreeMap::new();
    walk(root, Path::new(""), &mut entries);
    entries
}

fn normalize(entries: &mut BTreeMap<PathBuf, Entry>) {
    for (path, entry) in entries {
        if path.parent() != Some(Path::new(".git/pbps-compose-v2/resources")) {
            continue;
        }
        if let Entry::File(_, bytes) = entry {
            let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            // Recreated snapshots have fresh inode/timestamp observations.
            // Every other field, file byte, permission, ref and receipt must
            // equal the once-captured real fixture, including inventory names.
            if value["inventory"].is_object() {
                value["snapshot"] = serde_json::Value::Null;
                value["inventory"]["root"] = serde_json::Value::Null;
                for entry in value["inventory"]["entries"]
                    .as_object_mut()
                    .unwrap()
                    .values_mut()
                {
                    entry["identity"] = serde_json::Value::Null;
                    entry["modified"] = serde_json::Value::Null;
                    entry["changed"] = serde_json::Value::Null;
                    if entry["directory"] == true {
                        entry["size"] = serde_json::Value::Null;
                    }
                }
            }
            *bytes = serde_json::to_vec(&value).unwrap();
        }
    }
}

fn refresh_inventory(value: &mut serde_json::Value, f: &Fixture) {
    let snapshot = root(f)
        .join("snapshots")
        .join(value["operation"].as_str().unwrap());
    let identity = |path: &Path| {
        let m = fs::metadata(path).unwrap();
        serde_json::json!({"device": m.dev(), "inode": m.ino()})
    };
    value["snapshot"] = identity(&snapshot);
    value["inventory"]["root"] = identity(&snapshot);
    for (name, entry) in value["inventory"]["entries"].as_object_mut().unwrap() {
        let file = snapshot.join(name);
        let m = fs::metadata(&file).unwrap();
        assert_eq!(entry["directory"], m.is_dir());
        entry["identity"] = identity(&file);
        entry["size"] = m.len().into();
        entry["modified"] = serde_json::json!([m.mtime(), m.mtime_nsec()]);
        entry["changed"] = serde_json::json!([m.ctime(), m.ctime_nsec()]);
    }
}

pub(super) struct Pristine {
    source: BTreeMap<PathBuf, Entry>,
    remote: BTreeMap<PathBuf, Entry>,
}

impl Pristine {
    pub(super) fn capture(f: &Fixture) -> Self {
        Self {
            source: tree(&f.repo.root),
            remote: tree(&f.remote),
        }
    }

    pub(super) fn minimal_snapshot(&self, operation: &str, file: &Path) -> Self {
        let snapshot = Path::new(".git/pbps-compose-v2/snapshots").join(operation);
        let mut source = self.source.clone();
        source.retain(|path, _| {
            path.strip_prefix(&snapshot)
                .map_or(true, |relative| file.starts_with(relative))
        });
        let record = Path::new(".git/pbps-compose-v2/resources").join(format!("{operation}.json"));
        let Entry::File(_, bytes) = source.get_mut(&record).unwrap() else {
            panic!("real resource fixture");
        };
        let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        // Retain only existing acknowledged entries from the real delivery.
        // Their shape/content is never invented; restore refreshes metadata.
        value["inventory"]["entries"]
            .as_object_mut()
            .unwrap()
            .retain(|name, _| file.starts_with(Path::new(name)));
        *bytes = serde_json::to_vec(&value).unwrap();
        Self {
            source,
            remote: self.remote.clone(),
        }
    }

    pub(super) fn assert(&self, f: &Fixture) {
        let mut expected = self.source.clone();
        let mut actual = tree(&f.repo.root);
        for (path, entry) in &actual {
            if path.parent() == Some(Path::new(".git/pbps-compose-v2/resources")) {
                let Entry::File(_, bytes) = entry else {
                    panic!("nonregular fixture resource");
                };
                let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                if value["inventory"].is_object() {
                    let mut refreshed = value.clone();
                    refresh_inventory(&mut refreshed, f);
                    assert_eq!(value, refreshed, "snapshot metadata is stale: {path:?}");
                }
            }
        }
        normalize(&mut expected);
        normalize(&mut actual);
        assert_eq!(
            actual, expected,
            "a previous case changed the source files, refs or records"
        );
        assert_eq!(
            tree(&f.remote),
            self.remote,
            "a previous case changed the remote"
        );
    }

    pub(super) fn restore(&self, f: &Fixture) {
        fn restore(root: &Path, expected: &BTreeMap<PathBuf, Entry>) {
            let current = tree(root);
            let mut paths: Vec<_> = current.keys().collect();
            paths.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
            for path in paths {
                let same_kind = expected.get(path).is_some_and(|entry| {
                    std::mem::discriminant(entry) == std::mem::discriminant(&current[path])
                });
                if !same_kind {
                    match &current[path] {
                        Entry::Directory(_) => fs::remove_dir(root.join(path)).unwrap(),
                        Entry::File(_, _) | Entry::Link(_) => {
                            fs::remove_file(root.join(path)).unwrap()
                        }
                    }
                }
            }
            // Preserve the source/common directory inodes recorded by delivery.
            for (path, entry) in expected {
                match entry {
                    Entry::Directory(mode) => {
                        fs::create_dir_all(root.join(path)).unwrap();
                        if current.get(path) != Some(entry) {
                            fs::set_permissions(root.join(path), fs::Permissions::from_mode(*mode))
                                .unwrap();
                        }
                    }
                    Entry::File(mode, bytes) => {
                        if current.get(path) != Some(entry) {
                            fs::write(root.join(path), bytes).unwrap();
                            fs::set_permissions(root.join(path), fs::Permissions::from_mode(*mode))
                                .unwrap();
                        }
                    }
                    Entry::Link(target) => {
                        if current.get(path) != Some(entry) {
                            if root.join(path).symlink_metadata().is_ok() {
                                fs::remove_file(root.join(path)).unwrap();
                            }
                            symlink(target, root.join(path)).unwrap();
                        }
                    }
                }
            }
        }
        restore(&f.repo.root, &self.source);
        restore(&f.remote, &self.remote);
        for (path, entry) in &self.source {
            if path.parent() != Some(Path::new(".git/pbps-compose-v2/resources")) {
                continue;
            }
            let Entry::File(_, bytes) = entry else {
                continue;
            };
            let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            if !value["inventory"].is_object() {
                continue;
            }
            refresh_inventory(&mut value, f);
            fs::write(f.repo.root.join(path), serde_json::to_vec(&value).unwrap()).unwrap();
        }
    }

    pub(super) fn assert_same(&self, other: &Self) {
        let (mut a, mut b) = (self.source.clone(), other.source.clone());
        normalize(&mut a);
        normalize(&mut b);
        assert_eq!(
            a, b,
            "real and replayed faults left different source evidence"
        );
        assert_eq!(
            self.remote, other.remote,
            "real and replayed faults left different remote evidence"
        );
    }
}
