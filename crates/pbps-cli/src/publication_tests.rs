use super::*;
use std::cell::RefCell;
use std::collections::BTreeMap;

thread_local! {
    static FAULTS: RefCell<Vec<(&'static str, usize)>> = const { RefCell::new(Vec::new()) };
    #[cfg(unix)]
    static ACCESS_BEFORE_WRITE: RefCell<Vec<(u32, u32, u64)>> = const { RefCell::new(Vec::new()) };
}

// Scoped to the test thread: no product flag or parallel-test interference.
pub(super) fn fault(event: &str, _path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if event == "write" {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = fs::metadata(_path)?;
        ACCESS_BEFORE_WRITE.with(|v| {
            v.borrow_mut()
                .push((metadata.gid(), metadata.mode() & 0o7777, metadata.len()))
        });
    }
    FAULTS.with(|faults| {
        for (kind, left) in faults.borrow_mut().iter_mut() {
            if *kind == event {
                if *left == 0 {
                    *left = usize::MAX;
                    return Err(io::Error::other(format!("injected {event} failure")));
                }
                *left -= 1;
            }
        }
        Ok(())
    })
}

pub(crate) fn with_fault<T>(faults: &[(&'static str, usize)], run: impl FnOnce() -> T) -> T {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            FAULTS.with(|f| f.borrow_mut().clear());
        }
    }
    FAULTS.with(|f| {
        assert!(f.borrow().is_empty());
        f.borrow_mut().extend_from_slice(faults);
    });
    let _clear = Clear;
    run()
}

struct Fixture {
    _root: crate::adopt::Stage,
    dir: PathBuf,
    copies: Vec<(PathBuf, PathBuf)>,
    old: Vec<PathBuf>,
}

impl Fixture {
    fn new(count: usize) -> Self {
        let root = crate::adopt::Stage::new("publication2351").unwrap();
        let dir = root.path().join("schema");
        let stage = root.path().join("staged");
        fs::create_dir(&dir).unwrap();
        fs::create_dir(&stage).unwrap();
        let mut schema = pbps_model::Schema::default();
        let mut old = Vec::new();
        for i in 0..count {
            let mut table = pbps_model::Table::default();
            table
                .columns
                .insert("id".into(), pbps_model::Column::new("int".parse().unwrap()));
            schema
                .tables
                .insert(format!("dbo.t{i}").parse().unwrap(), table);
            let path = dir.join(format!("old-{i}.yml"));
            fs::write(&path, format!("old {i}\n")).unwrap();
            old.push(path);
        }
        let staged =
            crate::adopt::write_declarations(&stage, &schema, &Default::default()).unwrap();
        let copies = staged
            .into_iter()
            .map(|p| {
                let output = dir.join(p.strip_prefix(&stage).unwrap());
                (p, output)
            })
            .collect();
        Self {
            _root: root,
            dir,
            copies,
            old,
        }
    }

    fn publish(&self) -> anyhow::Result<Option<String>> {
        publish(&self.dir, &self.old, &self.copies)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    kind: u8,
    bytes: Vec<u8>,
    #[cfg(unix)]
    identity_and_access: (u64, u64, u32, u32),
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Entry>) {
        for item in fs::read_dir(dir).unwrap() {
            let path = item.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let (kind, bytes) = if metadata.is_symlink() {
                (
                    2,
                    fs::read_link(&path)
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec(),
                )
            } else if metadata.is_dir() {
                walk(root, &path, out);
                (0, Vec::new())
            } else {
                (1, fs::read(&path).unwrap())
            };
            #[cfg(unix)]
            let identity_and_access = {
                use std::os::unix::fs::MetadataExt as _;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.mode(),
                    metadata.gid(),
                )
            };
            out.insert(
                path.strip_prefix(root).unwrap().to_owned(),
                Entry {
                    kind,
                    bytes,
                    #[cfg(unix)]
                    identity_and_access,
                },
            );
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn a_write_failure_after_half_the_outputs_restores_every_entry_and_new_parent() {
    let mut f = Fixture::new(4);
    f.copies[0].1 = f.dir.join("new/nested/first.yml");
    let before = snapshot(&f.dir);
    let error = with_fault(&[("write", 2)], || f.publish()).unwrap_err();
    assert!(format!("{error:#}").contains("injected write failure"));
    assert!(format!("{error:#}").contains("the project is unchanged"));
    assert_eq!(
        snapshot(&f.dir),
        before,
        "write failure damaged the directory"
    );
}

#[test]
fn a_failed_rename_restores_the_entries_already_moved() {
    let f = Fixture::new(3);
    let before = snapshot(&f.dir);
    let error = with_fault(&[("rename", 1)], || f.publish()).unwrap_err();
    assert!(format!("{error:#}").contains("injected rename failure"));
    assert_eq!(snapshot(&f.dir), before);
}

#[cfg(unix)]
#[test]
fn a_dangling_parent_failure_restores_old_entries_and_preceding_outputs() {
    let mut f = Fixture::new(2);
    std::os::unix::fs::symlink("missing", f.dir.join("roles")).unwrap();
    f.copies[1].1 = f.dir.join("roles/r.yml");
    let before = snapshot(&f.dir);
    assert!(format!("{:#}", f.publish().unwrap_err()).contains("the project is unchanged"));
    assert_eq!(
        snapshot(&f.dir),
        before,
        "parent failure damaged the directory"
    );
}

#[cfg(unix)]
#[test]
fn an_unlisted_uppercase_alias_is_left_intact_when_creation_refuses() {
    let f = Fixture::new(2);
    let uppercase = f.dir.join("dbo.t1.YML");
    let output = &f.copies[1].1;
    fs::write(&uppercase, "unlisted entry\n").unwrap();
    // The lane is case-sensitive. Simulate lookup, without putting the simulator
    // in the removal list: a case-insensitive filesystem has only the .YML entry.
    if !output.exists() {
        std::os::unix::fs::symlink(uppercase.file_name().unwrap(), output).unwrap();
    }
    let before = snapshot(&f.dir);
    let error = f.publish().unwrap_err();
    assert!(format!("{error:#}").contains("cannot create declaration"));
    assert_eq!(
        snapshot(&f.dir),
        before,
        "uppercase collision damaged the directory"
    );
}

#[cfg(unix)]
#[test]
fn an_unlisted_hardlink_collision_restores_the_old_links_and_bytes() {
    let f = Fixture::new(2);
    fs::hard_link(&f.old[1], &f.copies[1].1).unwrap();
    let before = snapshot(&f.dir);
    assert!(f.publish().is_err());
    assert_eq!(
        snapshot(&f.dir),
        before,
        "hardlink collision damaged the directory"
    );
    assert!(crate::paths::same_file(&f.old[1], &f.copies[1].1).unwrap());
}

#[cfg(unix)]
#[test]
fn restored_entries_keep_symlink_targets_and_hardlink_relationships() {
    let mut f = Fixture::new(3);
    let symlink = f.dir.join("alias.yml");
    let hard = f.dir.join("hard.yml");
    std::os::unix::fs::symlink(f.old[0].file_name().unwrap(), &symlink).unwrap();
    fs::hard_link(&f.old[0], &hard).unwrap();
    f.old.extend([symlink, hard]);
    let before = snapshot(&f.dir);
    assert!(with_fault(&[("write", 1)], || f.publish()).is_err());
    assert_eq!(snapshot(&f.dir), before);
}

#[cfg(unix)]
#[test]
fn successful_alias_replacement_leaves_one_loadable_declaration() {
    for hardlink in [false, true] {
        let mut f = Fixture::new(1);
        let cased = f.dir.join("dbo.T0.yml");
        fs::rename(&f.old[0], &cased).unwrap();
        f.old[0] = cased.clone();
        let output = &f.copies[0].1;
        if !output.exists() {
            if hardlink {
                fs::hard_link(&cased, output).unwrap();
            } else {
                std::os::unix::fs::symlink(cased.file_name().unwrap(), output).unwrap();
            }
            f.old.push(output.clone());
        }
        assert!(f.publish().unwrap().is_none());
        assert_eq!(
            pbps_load::schema_files(&f.dir).unwrap().as_slice(),
            std::slice::from_ref(output)
        );
        assert_eq!(
            pbps_load::load_schema_dir(&f.dir)
                .unwrap()
                .schema
                .tables
                .len(),
            1
        );
        assert!(!fs::symlink_metadata(output).unwrap().is_symlink());
        assert!(!f.dir.join(BACKUP).exists());
    }
}

#[cfg(unix)]
#[test]
fn restricted_replacements_set_access_before_content_and_refuse_metadata_failures() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let mut f = Fixture::new(1);
    let path = &f.copies[0].1;
    fs::rename(&f.old[0], path).unwrap();
    f.old[0] = path.clone();
    fs::set_permissions(path, fs::Permissions::from_mode(0o640)).unwrap();
    let gid = rustix::process::getgroups()
        .unwrap()
        .into_iter()
        .find(|g| *g != rustix::process::getegid());
    if let Some(gid) = gid {
        rustix::fs::chown(path, None, Some(gid)).unwrap();
    }
    let before = fs::metadata(path).unwrap();
    ACCESS_BEFORE_WRITE.with(|v| v.borrow_mut().clear());
    assert!(f.publish().unwrap().is_none());
    ACCESS_BEFORE_WRITE
        .with(|v| assert_eq!(*v.borrow(), vec![(before.gid(), before.mode() & 0o7777, 0)]));
    let after = fs::metadata(path).unwrap();
    assert_eq!(
        (after.gid(), after.mode() & 0o7777),
        (before.gid(), before.mode() & 0o7777)
    );
    let restored = snapshot(&f.dir);
    let error = with_fault(&[("metadata", 0)], || f.publish()).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("cannot keep the mode or group"));
    assert!(message.contains("dbo.t0.yml") && message.contains("before retrying"));
    assert_eq!(snapshot(&f.dir), restored);
}

#[test]
fn a_missing_source_restores_the_original_declarations() {
    let f = Fixture::new(2);
    fs::remove_file(&f.copies[1].0).unwrap();
    let before = snapshot(&f.dir);
    assert!(f.publish().is_err());
    assert_eq!(snapshot(&f.dir), before);
}

#[test]
fn new_schema_directories_disappear_again_after_a_failed_first_publication() {
    let mut f = Fixture::new(1);
    fs::remove_dir_all(&f.dir).unwrap();
    f.dir = f.dir.join("nested");
    f.copies[0].1 = f.dir.join("dbo.t0.yml");
    f.old.clear();
    assert!(with_fault(&[("write", 0)], || f.publish()).is_err());
    assert!(!f._root.path().join("schema").exists());
    assert!(f.publish().unwrap().is_none());
    assert_eq!(
        pbps_load::load_schema_dir(&f.dir)
            .unwrap()
            .schema
            .tables
            .len(),
        1
    );
}

#[test]
fn an_existing_backup_is_retained_and_never_read_as_declarations() {
    let f = Fixture::new(1);
    let backup = f.dir.join(BACKUP);
    fs::create_dir(&backup).unwrap();
    fs::write(backup.join("saved.yml.bak"), "saved old bytes\n").unwrap();
    let before = snapshot(&f.dir);
    let message = format!("{:#}", f.publish().unwrap_err());
    assert!(message.contains("already exists") && message.contains("restore or archive"));
    assert_eq!(snapshot(&f.dir), before);
    assert_eq!(pbps_load::schema_files(&f.dir).unwrap(), f.old);
}

#[cfg(unix)]
#[test]
fn a_backup_symlink_is_not_adopted_or_deleted() {
    let f = Fixture::new(1);
    std::os::unix::fs::symlink("missing", f.dir.join(BACKUP)).unwrap();
    let before = snapshot(&f.dir);
    assert!(f.publish().is_err());
    assert_eq!(snapshot(&f.dir), before);
}

#[test]
fn failed_restoration_names_each_unsaved_path_and_retains_remaining_backups() {
    let f = Fixture::new(3);
    let message = format!(
        "{:#}",
        with_fault(&[("write", 1), ("restore", 0)], || f.publish()).unwrap_err()
    );
    assert!(!message.contains("the project is unchanged"));
    assert!(
        message.contains("injected write failure") && message.contains("injected restore failure")
    );
    assert!(message.contains("old-2.yml") && message.contains("2.bak"));
    assert_eq!(
        fs::read(f.dir.join(BACKUP).join("2.bak")).unwrap(),
        b"old 2\n"
    );
    assert_eq!(fs::read(&f.old[0]).unwrap(), b"old 0\n");
    assert_eq!(fs::read(&f.old[1]).unwrap(), b"old 1\n");
    assert!(!f.old[2].exists());
}

#[test]
fn backup_cleanup_failure_is_success_with_an_actionable_leftover_note() {
    let f = Fixture::new(1);
    let note = with_fault(&[("cleanup", 0)], || f.publish())
        .unwrap()
        .unwrap();
    assert!(note.contains("were published") && note.contains(BACKUP));
    assert!(note.contains("inspect and remove") && note.contains("injected cleanup failure"));
    assert_eq!(
        pbps_load::load_schema_dir(&f.dir)
            .unwrap()
            .schema
            .tables
            .len(),
        1
    );
    assert!(f.dir.join(BACKUP).join("0.bak").exists());
}

#[test]
fn restoration_does_not_overwrite_an_occupied_original_path() {
    let f = Fixture::new(1);
    let backup = f.dir.join(BACKUP);
    fs::create_dir(&backup).unwrap();
    let saved = backup.join("0.bak");
    fs::rename(&f.old[0], &saved).unwrap();
    fs::write(&f.old[0], "editor replacement").unwrap();
    let failed = restore(
        &[],
        &[(f.old[0].clone(), saved.clone())],
        &[],
        &backup,
        true,
    );
    assert_eq!(failed.len(), 1);
    assert!(failed[0].contains("destination is occupied"));
    assert!(failed[0].contains("0.bak"));
    assert_eq!(fs::read(&f.old[0]).unwrap(), b"editor replacement");
    assert_eq!(fs::read(saved).unwrap(), b"old 0\n");
}

#[test]
fn failed_backup_cleanup_during_restoration_never_claims_unchanged() {
    let f = Fixture::new(1);
    let message = format!(
        "{:#}",
        with_fault(&[("write", 0), ("cleanup", 0)], || f.publish()).unwrap_err()
    );
    assert!(message.contains("injected write failure") && message.contains("cannot remove backup"));
    assert!(!message.contains("the project is unchanged"));
    assert_eq!(fs::read(&f.old[0]).unwrap(), b"old 0\n");
    assert!(f.dir.join(BACKUP).is_dir());
}

#[cfg(unix)]
#[test]
fn replacement_keeps_special_mode_bits_that_writing_clears() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    for mode in [0o2750, 0o4640] {
        let mut f = Fixture::new(1);
        let output = f.copies[0].1.clone();
        fs::rename(&f.old[0], &output).unwrap();
        f.old[0] = output.clone();
        fs::set_permissions(&output, fs::Permissions::from_mode(mode)).unwrap();
        assert!(f.publish().unwrap().is_none());
        assert_eq!(fs::metadata(&output).unwrap().mode() & 0o7777, mode);
    }
}

#[test]
fn backup_names_do_not_collide_with_valid_declaration_parent_directories() {
    let mut f = Fixture::new(2);
    let first = f.dir.join("tables.yml");
    let nested = f.dir.join("tables.yml.bak/other.yml");
    fs::create_dir(nested.parent().unwrap()).unwrap();
    for (i, path) in [first, nested].into_iter().enumerate() {
        fs::rename(&f.old[i], &path).unwrap();
        fs::copy(&f.copies[i].0, &path).unwrap();
        f.old[i] = path;
    }
    assert_eq!(
        pbps_load::load_schema_dir(&f.dir)
            .unwrap()
            .schema
            .tables
            .len(),
        2
    );
    assert!(f.publish().unwrap().is_none());
    assert_eq!(
        pbps_load::load_schema_dir(&f.dir)
            .unwrap()
            .schema
            .tables
            .len(),
        2
    );
    assert!(f.old.iter().all(|path| !path.exists()));
    assert!(!f.dir.join(BACKUP).exists());
}

#[cfg(unix)]
#[test]
fn backup_names_fit_when_declaration_filenames_are_at_the_filesystem_limit() {
    let mut f = Fixture::new(1);
    let path = f.dir.join(format!("{}.yml", "x".repeat(251)));
    fs::rename(&f.old[0], &path).unwrap();
    fs::copy(&f.copies[0].0, &path).unwrap();
    f.old[0] = path;
    assert_eq!(
        pbps_load::load_schema_dir(&f.dir)
            .unwrap()
            .schema
            .tables
            .len(),
        1
    );
    assert!(f.publish().unwrap().is_none());
    assert_eq!(
        pbps_load::load_schema_dir(&f.dir)
            .unwrap()
            .schema
            .tables
            .len(),
        1
    );
    assert!(!f.old[0].exists());
    assert!(!f.dir.join(BACKUP).exists());
}
