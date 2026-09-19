//! What a backend actually executes: the content of its main executable and
//! of the native libraries it has mapped or will load (ADR-0016 case 23).
//!
//! Reported versions are not executable identity. Two builds of the same
//! version, a library replaced on disk under a running process, or a hook
//! library loaded on one side only all change creation-time binding while
//! every version string still matches. So identity here is content:
//!
//! - The engine's own image is read through the `/proc/<pid>/exe` handle the
//!   lease already holds. That handle is the executed file object itself, so
//!   hashing it hashes what runs, even after the path was replaced.
//! - A mapped library's *loaded* content is read through
//!   `/proc/<pid>/map_files/<range>`, which is the mapped file object. That
//!   needs `CAP_SYS_ADMIN` in the initial user namespace (measured: refused
//!   even to a rootless container's owning user), which the root inspector
//!   has. Without it the file at the mapped path is hashed as a **disk
//!   candidate** and labelled so — never passed off as loaded content.
//! - Whether disk still is what was loaded is decided by inode identity:
//!   `maps` records the mapped file's inode, `stat` through the process's
//!   own root gives the path's current one, and a replaced or unlinked file
//!   differs (measured: a rename shows the old inode in `maps` and the new
//!   one on disk). Device numbers are not compared: overlayfs reports the
//!   lower filesystem's in `maps` and its own through `stat`.
//! - A library the scope requires but the process has not mapped yet is a
//!   late-loaded disk candidate, resolved the way the engine's loader would
//!   (`$libdir` is the engine's library directory next to its `bin`).

use super::{ProcessLease, UnqualifiedProcess};
use pbps_db::resolver::environment::{
    CatalogFacts, ExecutableIdentity, ExecutableRole, ExecutableSet, Provenance, guc_list,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

/// Larger than any engine binary or extension library; a mapping past this
/// is not something this profile would run.
const CONTENT_LIMIT: u64 = 512 * 1024 * 1024;

/// The native libraries a scope requires the engine to have loaded or be
/// able to load: those an installed extension's C functions name, and those
/// named by the preload settings. Deduplicated and ordered; these become the
/// late-loaded candidates checked against what the process has mapped.
pub(crate) fn required_libraries(catalog: &CatalogFacts) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for extension in &catalog.extensions {
        names.extend(extension.libraries.iter().cloned());
    }
    for setting in [
        "shared_preload_libraries",
        "session_preload_libraries",
        "local_preload_libraries",
    ] {
        if let Some(fact) = catalog.settings.get(setting) {
            names.extend(library_list(&fact.value).into_iter().map(|entry| {
                // `local_preload_libraries` is the one preload setting
                // that does not search `dynamic_library_path`: the
                // engine loads its names from `$libdir/plugins` alone
                // and refuses a directory component, so a bare `foo`
                // there is `$libdir/plugins/foo`, not `$libdir/foo`
                // (finding on #688).
                if setting == "local_preload_libraries" && !entry.contains('/') {
                    format!("$libdir/plugins/{entry}")
                } else {
                    entry
                }
            }));
        }
    }
    names.into_iter().collect()
}

/// The names in a preload setting, split as the engine's loader splits them
/// (`SplitDirectoriesString`): `"foo,bar", baz` is the two libraries
/// `foo,bar` and `baz`, where a plain split on commas made two names nothing
/// resolves and refused a compatible scope as unknown (finding on #688). A
/// list the engine would reject is kept whole as one name: the engine logs
/// the syntax error and loads nothing, but that log is not readable from
/// here, and a value that could not be read must stay a candidate that
/// fails to resolve, not read as "no libraries".
pub(crate) fn library_list(value: &str) -> Vec<String> {
    guc_list(value).unwrap_or_else(|| vec![value.to_owned()])
}

/// The executable set of one process: its engine image, every file-backed
/// shared object it has mapped, and the `required` libraries (as an engine
/// names them, e.g. `$libdir/hstore`) it has not.
pub(crate) fn executables(
    lease: &ProcessLease,
    required: &[String],
    dynamic_library_path: &str,
) -> Result<ExecutableSet, UnqualifiedProcess> {
    lease.check()?;
    let engine_path = lease.executable_path().to_path_buf();
    let engine = ExecutableIdentity {
        role: ExecutableRole::Engine,
        path: engine_path.to_string_lossy().into_owned(),
        digest: Some(digest_of(lease.executable_file())?),
        provenance: Provenance::LoadedContent,
        disk_differs_from_loaded: Some(disk_differs(
            lease,
            &engine_path,
            lease
                .executable_file()
                .metadata()
                .map_err(|_| UnqualifiedProcess)?
                .ino(),
        )),
    };

    let mapped = mappings(&lease.read_proc("maps", 8 * 1024 * 1024)?);
    let mut libraries = Vec::new();
    for (path, mapping) in &mapped {
        libraries.push(mapped_library(lease, path, mapping));
    }

    let libdir = library_directory(&engine_path);
    for name in required {
        let candidates = resolve(name, &libdir, dynamic_library_path);
        // A candidate already mapped is loaded content, handled above.
        if candidates
            .iter()
            .any(|candidate| mapped.contains_key(candidate))
        {
            continue;
        }
        // Try each candidate in the loader's search order; the first that
        // opens is the one it would load. If none opens, the library is not
        // where the path says it should be.
        let found = candidates.iter().find_map(|candidate| {
            let file = lease.open_in_root(candidate.trim_start_matches('/')).ok()?;
            let digest = digest_of(&file).ok()?;
            Some(ExecutableIdentity {
                role: ExecutableRole::LateLoaded,
                path: candidate.clone(),
                digest: Some(digest),
                provenance: Provenance::DiskCandidate,
                disk_differs_from_loaded: None,
            })
        });
        libraries.push(found.unwrap_or_else(|| {
            unreadable(
                ExecutableRole::LateLoaded,
                candidates
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| name.clone()),
                "required library not found on the library search path",
            )
        }));
    }
    lease.check()?;
    Ok(ExecutableSet { engine, libraries })
}

/// One file-backed mapping as `maps` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mapping {
    pub start: u64,
    pub end: u64,
    pub inode: u64,
    pub deleted: bool,
}

/// File-backed shared objects from a `maps` table, keyed by path, keeping
/// the first (lowest) range of each. Anonymous, device and non-library
/// mappings are skipped; a path is taken verbatim after the inode column, so
/// one containing spaces survives.
pub(crate) fn mappings(maps: &str) -> BTreeMap<String, Mapping> {
    let mut found = BTreeMap::new();
    for line in maps.lines() {
        let mut fields = line.splitn(6, ' ');
        let (Some(range), Some(_perms), Some(_offset), Some(_dev), Some(inode)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        let Some(rest) = fields.next() else { continue };
        let path = rest.trim_start();
        let (path, deleted) = match path.strip_suffix(" (deleted)") {
            Some(path) => (path, true),
            None => (path, false),
        };
        if !path.starts_with('/') || !is_shared_object(path) || inode == "0" {
            continue;
        }
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end), Ok(inode)) = (
            u64::from_str_radix(start, 16),
            u64::from_str_radix(end, 16),
            inode.parse::<u64>(),
        ) else {
            continue;
        };
        found.entry(path.to_owned()).or_insert(Mapping {
            start,
            end,
            inode,
            deleted,
        });
    }
    found
}

fn is_shared_object(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.ends_with(".so") || name.contains(".so.")
}

fn mapped_library(lease: &ProcessLease, path: &str, mapping: &Mapping) -> ExecutableIdentity {
    let differs = mapping.deleted || disk_differs(lease, Path::new(path), mapping.inode);
    // The mapped file object itself, when the inspector may open it.
    if let Ok(file) = lease.open_proc(&format!("map_files/{:x}-{:x}", mapping.start, mapping.end))
        && let Ok(digest) = digest_of(&file)
    {
        return ExecutableIdentity {
            role: ExecutableRole::Preloaded,
            path: path.to_owned(),
            digest: Some(digest),
            provenance: Provenance::LoadedContent,
            disk_differs_from_loaded: Some(differs),
        };
    }
    if differs {
        // The file on disk is not what is mapped, and the mapped content
        // cannot be read: there is nothing honest to hash.
        return unreadable(
            ExecutableRole::Preloaded,
            path.to_owned(),
            "the mapped library was replaced or removed on disk and its loaded content cannot be read",
        );
    }
    match lease
        .open_in_root(path.trim_start_matches('/'))
        .and_then(|file| digest_of(&file))
    {
        Ok(digest) => ExecutableIdentity {
            role: ExecutableRole::Preloaded,
            path: path.to_owned(),
            digest: Some(digest),
            provenance: Provenance::DiskCandidate,
            disk_differs_from_loaded: Some(false),
        },
        Err(_) => unreadable(ExecutableRole::Preloaded, path.to_owned(), "unreadable"),
    }
}

/// True when the path, seen through the process's own root, no longer names
/// the inode that is mapped — replaced, or gone.
fn disk_differs(lease: &ProcessLease, path: &Path, mapped_inode: u64) -> bool {
    let relative = path.to_string_lossy();
    match lease
        .open_in_root(relative.trim_start_matches('/'))
        .and_then(|file| file.metadata().map_err(|_| UnqualifiedProcess))
    {
        Ok(metadata) => metadata.ino() != mapped_inode,
        Err(UnqualifiedProcess) => true,
    }
}

fn unreadable(role: ExecutableRole, path: String, reason: &str) -> ExecutableIdentity {
    ExecutableIdentity {
        role,
        path,
        digest: None,
        provenance: Provenance::Unreadable {
            reason: reason.into(),
        },
        disk_differs_from_loaded: None,
    }
}

/// The engine's library directory: `<prefix>/lib` next to `<prefix>/bin`,
/// which is where `$libdir` points in every measured layout
/// (`/usr/lib/postgresql/<major>/{bin,lib}`).
pub(crate) fn library_directory(engine: &Path) -> PathBuf {
    engine
        .parent()
        .and_then(Path::parent)
        .map(|prefix| prefix.join("lib"))
        .unwrap_or_else(|| PathBuf::from("/lib"))
}

/// Resolves a library name the way the engine's loader does: `$libdir` is
/// the library directory, a bare name lives there, and a name without an
/// extension gets `.so`.
pub(crate) fn resolve(name: &str, libdir: &Path, dynamic_library_path: &str) -> Vec<String> {
    // The loader tries the name exactly as given before appending the
    // platform suffix (`expand_dynamic_library_name`): a bare name is looked
    // for in every directory of the path as is, then in every directory with
    // `.so`; a name with a directory is tried as is, then suffixed. Emitting
    // the suffixed form alone read a valid `/opt/plugin` as unreadable and
    // could hash an unrelated `/opt/plugin.so` beside it (finding on #688).
    let bases: Vec<PathBuf> = if let Some(rest) = name.strip_prefix("$libdir/") {
        vec![libdir.join(rest)]
    } else if name.contains('/') {
        vec![PathBuf::from(name)]
    } else {
        // A bare name is searched along `dynamic_library_path`, `$libdir`
        // expanding to the engine's library directory; the default is just
        // `$libdir`. Each directory is a candidate, in order.
        let dirs: Vec<String> = dynamic_library_path
            .split(':')
            .map(str::trim)
            .filter(|dir| !dir.is_empty())
            .map(|dir| match dir.strip_prefix("$libdir") {
                Some(rest) => format!("{}{rest}", libdir.to_string_lossy()),
                None => dir.to_owned(),
            })
            .collect();
        let dirs = if dirs.is_empty() {
            vec![libdir.to_string_lossy().into_owned()]
        } else {
            dirs
        };
        dirs.into_iter()
            .map(|dir| PathBuf::from(dir).join(name))
            .collect()
    };
    let exact: Vec<String> = bases
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let suffixed: Vec<String> = exact.iter().map(|path| format!("{path}.so")).collect();
    exact.into_iter().chain(suffixed).collect()
}

/// SHA-256 of a file's content, read positionally so a handle shared with
/// the lease keeps its own cursor untouched.
fn digest_of(file: &File) -> Result<String, UnqualifiedProcess> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut offset = 0u64;
    loop {
        let read = match file.read_at(&mut buffer, offset) {
            Ok(0) => break,
            Ok(n) => n,
            // `map_files` and some pseudo files refuse positional reads;
            // fall back to a fresh sequential read of the same handle.
            Err(error) if error.raw_os_error() == Some(libc_espipe()) && offset == 0 => {
                let mut whole = Vec::new();
                let mut handle = file;
                handle
                    .read_to_end(&mut whole)
                    .map_err(|_| UnqualifiedProcess)?;
                if whole.len() as u64 > CONTENT_LIMIT {
                    return Err(UnqualifiedProcess);
                }
                return Ok(format!("{:x}", Sha256::digest(&whole)));
            }
            Err(_) => return Err(UnqualifiedProcess),
        };
        hasher.update(&buffer[..read]);
        offset += read as u64;
        if offset > CONTENT_LIMIT {
            return Err(UnqualifiedProcess);
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn libc_espipe() -> i32 {
    29
}

#[cfg(test)]
mod tests {
    use super::*;

    // A line as measured from a containerized postmaster read on the host:
    // the path is the process's own view, the device the lower filesystem's.
    const MAPS: &str = "\
71e69568a000-71e6956b2000 r--p 00000000 08:30 510827                     /usr/lib/x86_64-linux-gnu/libc.so.6
71e6956b2000-71e69583a000 r-xp 00028000 08:30 510827                     /usr/lib/x86_64-linux-gnu/libc.so.6
5a1c00000000-5a1c00a10000 r-xp 00000000 08:30 515192                     /usr/lib/postgresql/18/bin/postgres
71e695900000-71e695901000 r--p 00000000 08:30 510908                     /usr/lib/x86_64-linux-gnu/libz.so.1.3.1 (deleted)
71e695a00000-71e695a21000 rw-p 00000000 00:00 0
7ffd1e2d0000-7ffd1e2f1000 rw-p 00000000 00:00 0                          [stack]
71e695b00000-71e695b10000 r--s 00000000 08:30 600001                     /usr/share/odd name/data.bin
71e695c00000-71e695c10000 r-xp 00000000 08:30 600002                     /opt/hooks/my hook.so
";

    #[test]
    fn only_file_backed_shared_objects_are_mapped_and_the_first_range_is_kept() {
        let found = mappings(MAPS);
        assert_eq!(
            found.keys().cloned().collect::<Vec<_>>(),
            vec![
                "/opt/hooks/my hook.so",
                "/usr/lib/x86_64-linux-gnu/libc.so.6",
                "/usr/lib/x86_64-linux-gnu/libz.so.1.3.1",
            ]
        );
        let libc = &found["/usr/lib/x86_64-linux-gnu/libc.so.6"];
        assert_eq!(
            (libc.start, libc.end, libc.inode),
            (0x71e69568a000, 0x71e6956b2000, 510827)
        );
        assert!(!libc.deleted);
        // The engine binary is not a library; the data file and anonymous
        // and stack mappings are not code the loader placed.
        assert!(!found.contains_key("/usr/lib/postgresql/18/bin/postgres"));
        assert!(!found.contains_key("/usr/share/odd name/data.bin"));
    }

    #[test]
    fn a_library_unlinked_under_the_process_is_marked_deleted() {
        let found = mappings(MAPS);
        assert!(found["/usr/lib/x86_64-linux-gnu/libz.so.1.3.1"].deleted);
        assert_eq!(
            found["/usr/lib/x86_64-linux-gnu/libz.so.1.3.1"].inode,
            510908
        );
    }

    #[test]
    fn a_preload_list_is_split_as_the_engine_splits_it() {
        let names = |list: &[&str]| list.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        // Measured on 18: this is how the engine renders
        // `SET session_preload_libraries = 'foo,bar', baz, 'q"x', ' sp ace '`
        // and the names it then tries to load, in order.
        assert_eq!(
            library_list(r#""foo,bar", baz, "q""x", " sp ace ""#),
            names(&["foo,bar", "baz", "q\"x", " sp ace "])
        );
        // Unquoted elements lose their surrounding whitespace; a bare list
        // and an empty one stay what they were.
        assert_eq!(
            library_list("  auto_explain ,$libdir/hstore  "),
            names(&["auto_explain", "$libdir/hstore"])
        );
        assert_eq!(library_list(""), Vec::<String>::new());
        assert_eq!(library_list("   "), Vec::<String>::new());
        // What the engine rejects as list syntax is one unreadable name, not
        // a shorter list: an unclosed quote, an empty element, text after a
        // closing quote.
        for broken in [r#""foo"#, "a,,b", "a,", r#""a"b"#] {
            assert_eq!(library_list(broken), names(&[broken]), "{broken:?}");
        }
    }

    #[test]
    fn required_libraries_gathers_extension_and_preload_names_deduplicated() {
        use pbps_db::resolver::Observation;
        use pbps_db::resolver::environment::{CatalogFacts, ExtensionFact, SettingFact};
        use std::collections::BTreeMap;
        let setting = |value: &str| SettingFact {
            value: value.into(),
            source: "default".into(),
            context: "postmaster".into(),
        };
        let catalog = CatalogFacts {
            observations: BTreeMap::new(),
            extensions: vec![
                ExtensionFact {
                    name: "hstore".into(),
                    version: "1.8".into(),
                    schema: "public".into(),
                    requires: vec![],
                    libraries: vec!["$libdir/hstore".into()],
                },
                ExtensionFact {
                    name: "plpgsql".into(),
                    version: "1.0".into(),
                    schema: "pg_catalog".into(),
                    requires: vec![],
                    libraries: vec![],
                },
            ],
            available_extensions: BTreeMap::new(),
            collations: vec![],
            settings: [
                // A shared object named twice, and an empty preload list.
                (
                    "shared_preload_libraries".to_owned(),
                    setting("auto_explain, $libdir/hstore"),
                ),
                // A local preload is loaded from `$libdir/plugins`, so the
                // same bare name as the shared one is a different library
                // (finding on #688).
                (
                    "local_preload_libraries".to_owned(),
                    setting("auto_explain, plugin_hook"),
                ),
                // A quoted element is one library whatever it contains, as
                // the engine renders and loads it (finding on #688).
                (
                    "session_preload_libraries".to_owned(),
                    setting(r#""foo,bar", auto_explain"#),
                ),
            ]
            .into_iter()
            .collect(),
            visibility: BTreeMap::new(),
        };
        let _ = Observation::NotReported;
        assert_eq!(
            required_libraries(&catalog),
            vec![
                "$libdir/hstore".to_owned(),
                "$libdir/plugins/auto_explain".to_owned(),
                "$libdir/plugins/plugin_hook".to_owned(),
                "auto_explain".to_owned(),
                "foo,bar".to_owned()
            ]
        );
    }

    #[test]
    fn library_names_resolve_like_the_engines_loader() {
        let libdir = library_directory(Path::new("/usr/lib/postgresql/18/bin/postgres"));
        assert_eq!(libdir, PathBuf::from("/usr/lib/postgresql/18/lib"));
        let names = |list: &[&str]| list.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        // The default library path is `$libdir`; a `$libdir/` name and an
        // absolute name are each tried as given and then with the platform
        // suffix, in that order, as the loader tries them (finding on #688).
        assert_eq!(
            resolve("$libdir/hstore", &libdir, "$libdir"),
            names(&[
                "/usr/lib/postgresql/18/lib/hstore",
                "/usr/lib/postgresql/18/lib/hstore.so"
            ])
        );
        assert_eq!(
            resolve("auto_explain", &libdir, "$libdir"),
            names(&[
                "/usr/lib/postgresql/18/lib/auto_explain",
                "/usr/lib/postgresql/18/lib/auto_explain.so"
            ])
        );
        // A name that already carries a suffix is still tried suffixed again
        // second, exactly as the loader does; the exact file comes first.
        assert_eq!(
            resolve("/opt/hooks/hook.so", &libdir, "$libdir"),
            names(&["/opt/hooks/hook.so", "/opt/hooks/hook.so.so"])
        );
        assert_eq!(
            resolve("$libdir/plugins/x.so.1", &libdir, "$libdir"),
            names(&[
                "/usr/lib/postgresql/18/lib/plugins/x.so.1",
                "/usr/lib/postgresql/18/lib/plugins/x.so.1.so"
            ])
        );
        // A bare name searches every directory of a custom path for the exact
        // name, then every directory for the suffixed one, with `$libdir`
        // expanded to the engine's library directory.
        assert_eq!(
            resolve("auto_explain", &libdir, "/opt/pg/lib:$libdir"),
            names(&[
                "/opt/pg/lib/auto_explain",
                "/usr/lib/postgresql/18/lib/auto_explain",
                "/opt/pg/lib/auto_explain.so",
                "/usr/lib/postgresql/18/lib/auto_explain.so",
            ])
        );
    }

    /// A live process this test owns: what runs is what is hashed, the
    /// libraries it mapped are reported with content, and a required library
    /// that does not exist is unreadable rather than absent-and-fine.
    #[test]
    fn a_running_process_reports_its_executed_content_and_mapped_libraries() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        // Let the exec happen before the lease looks at the executable.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let lease = ProcessLease::capture(child.id()).unwrap();
        let set = executables(&lease, &["$libdir/no_such_library".into()], "$libdir").unwrap();
        let on_disk = std::fs::read(lease.executable_path()).unwrap();
        assert_eq!(
            set.engine.digest.as_deref(),
            Some(format!("{:x}", Sha256::digest(&on_disk)).as_str())
        );
        assert_eq!(set.engine.provenance, Provenance::LoadedContent);
        assert_eq!(set.engine.disk_differs_from_loaded, Some(false));
        let libc = set
            .libraries
            .iter()
            .find(|l| l.path.contains("libc.so"))
            .expect("libc is mapped");
        assert!(libc.digest.is_some(), "{libc:?}");
        assert_eq!(libc.disk_differs_from_loaded, Some(false));
        assert!(matches!(
            libc.provenance,
            Provenance::LoadedContent | Provenance::DiskCandidate
        ));
        let missing = set
            .libraries
            .iter()
            // Reported at the first candidate the loader would try, the
            // exact name, which is also the name the engine's own error
            // names (finding on #688).
            .find(|l| l.path.ends_with("/no_such_library"))
            .expect("the required library is reported");
        assert_eq!(missing.role, ExecutableRole::LateLoaded);
        assert!(matches!(missing.provenance, Provenance::Unreadable { .. }));
        assert!(missing.digest.is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// The negative controls for "disk is what runs", on a process whose
    /// executable the lease admits (root-owned, as every engine binary is):
    /// a path that names another inode, or none, differs from what is
    /// mapped, and a mapped library whose inode moved is reported as
    /// replaced rather than hashed from disk. Replacing a binary under a
    /// running engine end to end is the root fixture's case.
    #[test]
    fn a_path_that_no_longer_names_the_mapped_inode_is_reported_as_differing() {
        let mut child = std::process::Command::new(which_sleep())
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let lease = ProcessLease::capture(child.id()).unwrap();
        let exe = lease.executable_path().to_path_buf();
        let mapped_inode = lease.executable_file().metadata().unwrap().ino();
        assert!(!disk_differs(&lease, &exe, mapped_inode));
        assert!(disk_differs(
            &lease,
            Path::new("/usr/bin/true"),
            mapped_inode
        ));
        assert!(disk_differs(
            &lease,
            Path::new("/nonexistent/lib.so"),
            mapped_inode
        ));

        let maps = lease.read_proc("maps", 8 * 1024 * 1024).unwrap();
        let (libc_path, libc) = mappings(&maps)
            .into_iter()
            .find(|(path, _)| path.contains("libc.so"))
            .expect("libc is mapped");
        let intact = mapped_library(&lease, &libc_path, &libc);
        assert_eq!(intact.disk_differs_from_loaded, Some(false));
        assert!(intact.digest.is_some());
        // The same library with the inode the mapping would carry after a
        // replacement: nothing honest to hash without the mapped file object.
        let moved = Mapping {
            inode: libc.inode + 1,
            ..libc
        };
        let replaced = mapped_library(&lease, &libc_path, &moved);
        match replaced.provenance {
            Provenance::LoadedContent => {
                // A root inspector read the mapped object itself and still
                // reports the disk difference.
                assert_eq!(replaced.disk_differs_from_loaded, Some(true));
            }
            Provenance::Unreadable { ref reason } => {
                assert!(reason.contains("replaced or removed"), "{reason}");
                assert!(replaced.digest.is_none());
            }
            Provenance::DiskCandidate => panic!("disk must not stand in for moved content"),
        }
        child.kill().unwrap();
        child.wait().unwrap();
    }

    fn which_sleep() -> PathBuf {
        for candidate in ["/usr/bin/sleep", "/bin/sleep"] {
            if Path::new(candidate).exists() {
                return PathBuf::from(candidate);
            }
        }
        panic!("no sleep binary");
    }

    #[test]
    fn a_digest_is_of_the_content_and_independent_of_the_handles_cursor() {
        let dir = std::env::temp_dir().join(format!("pbps-exe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lib.so");
        std::fs::write(&path, b"hello, loader").unwrap();
        let mut file = File::open(&path).unwrap();
        let mut skip = [0u8; 5];
        file.read_exact(&mut skip).unwrap();
        let digest = digest_of(&file).unwrap();
        assert_eq!(digest, format!("{:x}", Sha256::digest(b"hello, loader")));
        std::fs::write(&path, b"hello, loader!").unwrap();
        assert_ne!(digest_of(&File::open(&path).unwrap()).unwrap(), digest);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
