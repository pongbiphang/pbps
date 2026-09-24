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

/// Larger than any engine binary, extension library or engine package; a
/// mapping past this is not something this profile would run. SQL Server's
/// own package is the measure: `sqlservr.sfp` is 637 MB on 17.0.
const CONTENT_LIMIT: u64 = 2 * 1024 * 1024 * 1024;

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
///
/// The census itself stays on the caller's task, because it borrows the
/// lease; the hashing does not. An engine's code can be more than a gigabyte
/// — SQL Server maps its own from packages — and the runs that call this live
/// on a current-thread runtime beside the watchers that keep their containers:
/// hashed inline, one census stalled them past their request budget and they
/// removed the run's channel as lost (measured in CI on #611). So each file is
/// hashed on the blocking pool while the runtime keeps turning.
pub(crate) async fn executables(
    lease: &ProcessLease,
    required: &[String],
    dynamic_library_path: &str,
    packages: &[&str],
) -> Result<ExecutableSet, UnqualifiedProcess> {
    lease.check()?;
    let engine_path = lease.executable_path().to_path_buf();
    let engine = ExecutableIdentity {
        role: ExecutableRole::Engine,
        path: engine_path.to_string_lossy().into_owned(),
        digest: Some(digest_of(lease.executable_file()).await?),
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

    let mapped = mappings(&lease.read_proc("maps", 8 * 1024 * 1024)?, packages);
    let mut observed = BTreeMap::new();
    for (path, mapping) in &mapped {
        observed.insert(path.clone(), mapped_library(lease, path, mapping).await);
    }
    let libdir = library_directory(&engine_path);
    let cwd = lease.working_directory()?;
    // Resolve required names before reporting mappings, so each can claim
    // the mapping it is loaded through. A mapping is named by the file's
    // resolved path, while
    // the candidates are spelled the way the engine names the library, and
    // the two differ when the library is installed through a symlink
    // (`$libdir/foo.so -> foo.so.1`): matched by path alone, the target
    // reported the mapped file under one name and the candidate under the
    // other, a fresh scratch backend reported the candidate only, and
    // identical builds were refused on the name (finding on #688). So the
    // candidate the loader would open is correlated by inode, size and content:
    // an inode alone can belong to an unrelated filesystem (#712). Each
    // mapping is observed once, so correlation and reporting use the same
    // evidence. A mapping it claims is reported under the
    // candidate's spelling, which both sides share — once per spelling, since
    // two required names can be two links to one loaded file, and a side
    // that has it loaded must still name both, as the side that has not
    // does (finding on #688).
    let mut claimed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut late = Vec::new();
    for name in required {
        let candidates = resolve(name, &libdir, dynamic_library_path, &cwd);
        // A candidate already mapped under its own spelling is loaded
        // content, reported with the mappings below — and recorded as one
        // of that mapping's spellings, so that an alias of the same file in
        // the required set does not take the mapping's name for itself
        // alone (finding on #688).
        if let Some(candidate) = candidates
            .iter()
            .find(|candidate| mapped.contains_key(*candidate))
        {
            claimed
                .entry(candidate.clone())
                .or_default()
                .push(candidate.clone());
            continue;
        }
        // The first candidate that opens is the one the loader would load;
        // a later one is never what runs. None opening means the library is
        // not where the path says it should be.
        let Some((candidate, file)) = candidates.iter().find_map(|candidate| {
            let file = lease.open_in_root(candidate.trim_start_matches('/')).ok()?;
            Some((candidate.clone(), file))
        }) else {
            late.push(unreadable(
                ExecutableRole::LateLoaded,
                candidates
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| name.clone()),
                "required library not found on the library search path",
            ));
            continue;
        };
        let metadata = file.metadata().ok();
        let digest = digest_of(&file).await;
        if let Some(path) = metadata.and_then(|metadata| {
            let digest = digest.as_ref().ok()?;
            mapped
                .iter()
                .find(|(path, mapping)| {
                    let loaded = &observed[*path];
                    mapping.inode == metadata.ino()
                        && loaded.size == Some(metadata.len())
                        && loaded.identity.digest.as_ref() == Some(digest)
                })
                .map(|(path, _)| path.clone())
        }) {
            claimed.entry(path).or_default().push(candidate);
            continue;
        }
        late.push(match digest {
            Ok(digest) => ExecutableIdentity {
                role: ExecutableRole::LateLoaded,
                path: candidate,
                digest: Some(digest),
                provenance: Provenance::DiskCandidate,
                disk_differs_from_loaded: None,
            },
            // Opened but not hashable is not "absent": the loader would load
            // this file, so nothing else stands in for it.
            Err(_) => unreadable(
                ExecutableRole::LateLoaded,
                candidate,
                "the required library could not be read",
            ),
        });
    }
    let mut libraries = Vec::new();
    for (path, observation) in observed {
        let identity = observation.identity;
        match claimed.get(&path) {
            Some(spellings) => libraries.extend(spellings.iter().map(|spelling| {
                let mut aliased = identity.clone();
                aliased.path = spelling.clone();
                aliased
            })),
            None => libraries.push(identity),
        }
    }
    libraries.extend(late);
    lease.check()?;
    Ok(ExecutableSet { engine, libraries })
}

/// One file identity and its file-backed ranges as `maps` reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mapping {
    pub start: u64,
    pub end: u64,
    pub alternatives: Vec<(u64, u64)>,
    pub device: (u32, u32),
    pub inode: u64,
    pub deleted: bool,
}

/// File-backed shared objects from a `maps` table, keyed by path, keeping
/// each range of the first file identity at that path. Anonymous, device and
/// non-library mappings are skipped; a path is taken verbatim after the inode
/// column, so one containing spaces survives.
pub(crate) fn mappings(maps: &str, packages: &[&str]) -> BTreeMap<String, Mapping> {
    let mut found = BTreeMap::new();
    for line in maps.lines() {
        let mut fields = line.splitn(6, ' ');
        let (Some(range), Some(_perms), Some(_offset), Some(device), Some(inode)) = (
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
        if !path.starts_with('/') || !is_engine_code(path, packages) || inode == "0" {
            continue;
        }
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let Some((major, minor)) = device.split_once(':') else {
            continue;
        };
        let (Ok(start), Ok(end), Ok(inode), Ok(major), Ok(minor)) = (
            u64::from_str_radix(start, 16),
            u64::from_str_radix(end, 16),
            inode.parse::<u64>(),
            u32::from_str_radix(major, 16),
            u32::from_str_radix(minor, 16),
        ) else {
            continue;
        };
        let mapping = found.entry(path.to_owned()).or_insert_with(|| Mapping {
            start,
            end,
            alternatives: Vec::new(),
            device: (major, minor),
            inode,
            deleted,
        });
        // Inodes are unique only within one device. This compares two maps
        // rows, not maps with stat: overlayfs reports a different device even
        // through an opened map_files handle (measured for #776).
        if mapping.device == (major, minor)
            && mapping.inode == inode
            && mapping.deleted == deleted
            && (mapping.start, mapping.end) != (start, end)
        {
            mapping.alternatives.push((start, end));
        }
    }
    found
}

/// A mapped file that is code the engine runs: a shared object, or a file
/// with one of the suffixes the engine's router names as its own packages.
/// SQL Server for Linux maps its Windows binaries out of `.sfp` packages, so
/// for it the ELF is only the loader and these are the engine (#611;
/// DECISIONS 521).
fn is_engine_code(path: &str, packages: &[&str]) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.ends_with(".so")
        || name.contains(".so.")
        || packages.iter().any(|suffix| name.ends_with(suffix))
}

// Keep the measured size beside the identity from that same file handle.
// A mapping's virtual range is only part of the file, not its content length.
struct MappedLibrary {
    identity: ExecutableIdentity,
    size: Option<u64>,
}

async fn mapped_library(lease: &ProcessLease, path: &str, mapping: &Mapping) -> MappedLibrary {
    let differs = mapping.deleted || disk_differs(lease, Path::new(path), mapping.inode);
    // A package can acquire and retire individual mappings without changing
    // its content. Reading only its lowest range made a departed range turn
    // stable loaded content into DiskCandidate at the next scope check
    // (#774). Try the same file's other observed ranges before weakening the
    // evidence; DECISIONS 520/521 still require identifying loaded content.
    for (start, end) in
        std::iter::once((mapping.start, mapping.end)).chain(mapping.alternatives.iter().copied())
    {
        if let Ok(file) = lease.open_proc(&format!("map_files/{start:x}-{end:x}"))
            && let Ok(metadata) = file.metadata()
            && metadata.ino() == mapping.inode
            && let Ok(digest) = digest_of(&file).await
        {
            return MappedLibrary {
                identity: ExecutableIdentity {
                    role: ExecutableRole::Preloaded,
                    path: path.to_owned(),
                    digest: Some(digest),
                    provenance: Provenance::LoadedContent,
                    disk_differs_from_loaded: Some(differs),
                },
                size: Some(metadata.len()),
            };
        }
    }
    if differs {
        // The file on disk is not what is mapped, and the mapped content
        // cannot be read: there is nothing honest to hash.
        return MappedLibrary {
            identity: unreadable(
                ExecutableRole::Preloaded,
                path.to_owned(),
                "the mapped library was replaced or removed on disk and its loaded content cannot be read",
            ),
            size: None,
        };
    }
    let on_disk = match lease.open_in_root(path.trim_start_matches('/')) {
        Ok(file) => match file.metadata() {
            Ok(metadata) => digest_of(&file)
                .await
                .map(|digest| (digest, metadata.len())),
            Err(_) => Err(UnqualifiedProcess),
        },
        Err(error) => Err(error),
    };
    match on_disk {
        Ok((digest, size)) => MappedLibrary {
            identity: ExecutableIdentity {
                role: ExecutableRole::Preloaded,
                path: path.to_owned(),
                digest: Some(digest),
                provenance: Provenance::DiskCandidate,
                disk_differs_from_loaded: Some(false),
            },
            size: Some(size),
        },
        Err(_) => MappedLibrary {
            identity: unreadable(ExecutableRole::Preloaded, path.to_owned(), "unreadable"),
            size: None,
        },
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
pub(crate) fn resolve(
    name: &str,
    libdir: &Path,
    dynamic_library_path: &str,
    cwd: &Path,
) -> Vec<String> {
    // The loader tries the name exactly as given before appending the
    // platform suffix (`expand_dynamic_library_name`): a bare name is looked
    // for in every directory of the path as is, then in every directory with
    // `.so`; a name with a directory is tried as is, then suffixed. Emitting
    // the suffixed form alone read a valid `/opt/plugin` as unreadable and
    // could hash an unrelated `/opt/plugin.so` beside it (finding on #688).
    let bases: Vec<PathBuf> = if let Some(rest) = name.strip_prefix("$libdir/") {
        vec![libdir.join(rest)]
    } else if name.contains('/') {
        // A name with a directory is used as given; a relative one is
        // relative to the backend's working directory, the data directory
        // (measured on 18: `CREATE FUNCTION ... AS 'plugins/relhstore'`
        // loads `$PGDATA/plugins/relhstore.so` and stores the name as
        // written). Anchored at the root instead, `plugins/foo` read
        // `/plugins/foo`, absent or another file (finding on #688).
        vec![cwd.join(name)]
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

/// SHA-256 of a file's content, computed off the runtime's thread. The
/// handle is duplicated rather than moved because the lease keeps its own;
/// a duplicate shares the cursor, which the positional read never touches.
async fn digest_of(file: &File) -> Result<String, UnqualifiedProcess> {
    let file = file.try_clone().map_err(|_| UnqualifiedProcess)?;
    tokio::task::spawn_blocking(move || digest_blocking(&file))
        .await
        .map_err(|_| UnqualifiedProcess)?
}

/// SHA-256 of a file's content, read positionally so a handle shared with
/// the lease keeps its own cursor untouched.
fn digest_blocking(file: &File) -> Result<String, UnqualifiedProcess> {
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
    fn only_file_backed_code_is_mapped_and_same_file_ranges_are_retained() {
        let found = mappings(MAPS, &[]);
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
        assert_eq!(libc.device, (8, 0x30));
        assert_eq!(libc.alternatives, [(0x71e6956b2000, 0x71e69583a000)]);
        // Another file at the same path, or a removed version of it, cannot
        // supply the surviving range of the identity captured first.
        let mixed = format!(
            "{MAPS}\
71e696000000-71e696001000 r--p 00000000 08:30 999999 /usr/lib/x86_64-linux-gnu/libc.so.6\n\
71e696002000-71e696003000 r--p 00000000 08:31 510827 /usr/lib/x86_64-linux-gnu/libc.so.6\n\
71e696001000-71e696002000 r--p 00000000 08:30 510827 /usr/lib/x86_64-linux-gnu/libc.so.6 (deleted)\n"
        );
        assert_eq!(
            mappings(&mixed, &[])["/usr/lib/x86_64-linux-gnu/libc.so.6"],
            *libc
        );
        // The engine binary is not a library; the data file and anonymous
        // and stack mappings are not code the loader placed.
        assert!(!found.contains_key("/usr/lib/postgresql/18/bin/postgres"));
        assert!(!found.contains_key("/usr/share/odd name/data.bin"));
    }

    #[test]
    fn malformed_mapping_devices_do_not_alias_valid_file_identities() {
        for device in [
            "08",
            ":30",
            "08:",
            "08:30:01",
            "gg:30",
            "100000000:30",
            "08:100000000",
        ] {
            let line = format!("1000-2000 r--p 00000000 {device} 42 /lib/a.so");
            assert!(mappings(&line, &[]).is_empty(), "{device}");
        }
        let rows = "1000-2000 r--p 00000000 08:30 42 /lib/a.so\n\
                    3000-4000 r--p 00000000 0008:0030 42 /lib/a.so";
        assert_eq!(
            mappings(rows, &[])["/lib/a.so"].alternatives,
            [(0x3000, 0x4000)]
        );
    }

    /// An engine's own packages are code when the router names their suffix,
    /// and only then: the same row is a data file to an engine that does not.
    #[test]
    fn a_mapped_engine_package_is_code_only_for_the_engine_that_names_it() {
        const SQL_SERVER: &str = "\
7f0000000000-7f0000100000 r--p 00000000 08:30 700001                     /opt/mssql/lib/sqlservr.sfp
7f0000100000-7f0000200000 r--p 00100000 08:30 700001                     /opt/mssql/lib/sqlservr.sfp
7f0000300000-7f0000400000 r--p 00000000 08:30 700002                     /opt/mssql/lib/system.common.sfp
7f0000500000-7f0000600000 r-xp 00000000 08:30 700003                     /opt/mssql/lib/libsqlvdi.so
7f0000700000-7f0000800000 r--s 00000000 08:30 700004                     /var/opt/mssql/data/master.mdf
";
        assert_eq!(
            mappings(SQL_SERVER, &[".sfp"])
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            [
                "/opt/mssql/lib/libsqlvdi.so",
                "/opt/mssql/lib/sqlservr.sfp",
                "/opt/mssql/lib/system.common.sfp",
            ]
        );
        let package = &mappings(SQL_SERVER, &[".sfp"])["/opt/mssql/lib/sqlservr.sfp"];
        assert_eq!((package.start, package.inode), (0x7f00_0000_0000, 700_001));
        assert_eq!(
            mappings(SQL_SERVER, &[])
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["/opt/mssql/lib/libsqlvdi.so"]
        );
    }

    #[test]
    fn a_library_unlinked_under_the_process_is_marked_deleted() {
        let found = mappings(MAPS, &[]);
        assert!(found["/usr/lib/x86_64-linux-gnu/libz.so.1.3.1"].deleted);
        assert_eq!(
            found["/usr/lib/x86_64-linux-gnu/libz.so.1.3.1"].inode,
            510908
        );
    }

    #[tokio::test]
    #[ignore = "requires the root-owned mapping helper from live-resolver-namespace.py"]
    async fn a_surviving_mapping_keeps_loaded_content_after_the_first_range_exits() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let helper = std::env::var("PBPS_MAPPING_FIXTURE_HELPER").unwrap();
        let path = std::env::var("PBPS_MAPPING_FIXTURE_FILE").unwrap();
        let content = vec![b'a'; 65536];
        std::fs::write(&path, &content).unwrap();
        let mut child = OwnedChild(
            Command::new(helper)
                .args(["mapping-ranges", &path])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut input = child.0.stdin.take().unwrap();
        let mut output = BufReader::new(child.0.stdout.take().unwrap());
        let mut acknowledge = |expected: &str| {
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            assert_eq!(line.trim(), expected);
        };
        acknowledge("ready");
        let lease = ProcessLease::capture(child.0.id()).unwrap();
        let maps = lease.read_proc("maps", 8 * 1024 * 1024).unwrap();
        assert_eq!(maps.lines().filter(|line| line.ends_with(&path)).count(), 2);
        let mapping = mappings(&maps, &[]).remove(&path).unwrap();
        let initial = mapped_library(&lease, &path, &mapping).await.identity;
        assert_eq!(initial.provenance, Provenance::LoadedContent);
        assert_eq!(
            initial.digest,
            Some(format!("{:x}", Sha256::digest(&content)))
        );

        // The same shared policy used by both engine adapters must distinguish
        // a live map_files observation from equal bytes at a disk candidate.
        let engine = executables(&lease, &[], "$libdir", &[])
            .await
            .unwrap()
            .engine;
        let compare = |target: &ExecutableIdentity, resolver: &ExecutableIdentity| {
            use pbps_db::resolver::environment::{
                CatalogFacts, EnvironmentFacts, RuleVersion, ScopeReport, compare_executables,
            };
            let facts = |library: &ExecutableIdentity| EnvironmentFacts {
                catalog: CatalogFacts {
                    observations: BTreeMap::new(),
                    extensions: vec![],
                    available_extensions: BTreeMap::new(),
                    collations: vec![],
                    settings: BTreeMap::new(),
                    visibility: BTreeMap::new(),
                },
                executables: ExecutableSet {
                    engine: engine.clone(),
                    libraries: vec![library.clone()],
                },
            };
            let mut report = ScopeReport::new(RuleVersion::new("mapping-fixture-v1"));
            compare_executables(
                "mapping-fixture-v1",
                &facts(target),
                &facts(resolver),
                &[],
                &mut report,
            );
            report.verdict()
        };
        use pbps_db::resolver::environment::Verdict;
        assert_eq!(compare(&initial, &initial), Verdict::Verified);

        input.write_all(b"1").unwrap();
        acknowledge("one");
        assert!(
            lease
                .open_proc(&format!("map_files/{:x}-{:x}", mapping.start, mapping.end))
                .is_err()
        );
        let surviving = mapped_library(&lease, &path, &mapping).await.identity;
        assert_eq!(
            surviving.provenance,
            Provenance::LoadedContent,
            "a surviving same-file mapping must preserve loaded-content evidence"
        );
        assert_eq!(surviving, initial);
        assert_eq!(compare(&initial, &surviving), Verdict::Verified);

        input.write_all(b"2").unwrap();
        acknowledge("none");
        let gone = mapped_library(&lease, &path, &mapping).await.identity;
        assert_eq!(gone.provenance, Provenance::DiskCandidate);
        assert_eq!(gone.digest, initial.digest);
        for (target, resolver) in [(&initial, &gone), (&gone, &initial)] {
            assert_eq!(
                compare(target, resolver),
                Verdict::Unknown(vec![format!("library:{path}")]),
                "a retired mapping must not verify through equal disk bytes"
            );
        }
        std::fs::remove_file(&path).unwrap();
        let absent = mapped_library(&lease, &path, &mapping).await.identity;
        assert!(matches!(absent.provenance, Provenance::Unreadable { .. }));
        assert!(absent.digest.is_none());
        input.write_all(b"q").unwrap();
    }

    #[tokio::test]
    #[ignore = "requires the root-owned mapping helper from live-resolver-namespace.py"]
    async fn a_departed_mapping_cannot_use_the_same_inode_on_another_device() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let helper = std::env::var("PBPS_MAPPING_FIXTURE_HELPER").unwrap();
        let directory = std::env::var("PBPS_MAPPING_FIXTURE_DIRECTORY").unwrap();
        let path = format!("{directory}/mapping.so");
        let mut child = OwnedChild(
            Command::new(helper)
                .args(["mapping-devices", &directory])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut input = child.0.stdin.take().unwrap();
        let mut output = BufReader::new(child.0.stdout.take().unwrap());
        let mut acknowledge = |expected: &str| {
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            assert_eq!(line.trim(), expected);
        };
        acknowledge("ready");
        let lease = ProcessLease::capture(child.0.id()).unwrap();
        let maps = lease.read_proc("maps", 8 * 1024 * 1024).unwrap();
        let rows: Vec<_> = maps
            .lines()
            .filter(|line| line.ends_with(&path))
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][4], rows[1][4], "the fixture must reuse an inode");
        assert_ne!(rows[0][3], rows[1][3], "the fixture must use two devices");
        let mapping = mappings(&maps, &[]).remove(&path).unwrap();
        let initial = mapped_library(&lease, &path, &mapping).await.identity;
        assert_eq!(initial.provenance, Provenance::LoadedContent);
        let first_file = lease
            .open_proc(&format!("map_files/{}", rows[0][0]))
            .unwrap();
        let other_file = lease
            .open_proc(&format!("map_files/{}", rows[1][0]))
            .unwrap();
        assert_eq!(initial.digest, Some(digest_of(&first_file).await.unwrap()));
        assert_ne!(initial.digest, Some(digest_of(&other_file).await.unwrap()));

        input.write_all(b"1").unwrap();
        acknowledge("one");
        assert!(
            lease
                .open_proc(&format!("map_files/{}", rows[0][0]))
                .is_err()
        );
        assert!(
            lease
                .open_proc(&format!("map_files/{}", rows[1][0]))
                .is_ok()
        );
        let departed = mapped_library(&lease, &path, &mapping).await.identity;
        assert_eq!(
            departed.provenance,
            Provenance::DiskCandidate,
            "another device's same inode cannot stand in for the departed mapping"
        );
        assert_eq!(departed.disk_differs_from_loaded, Some(false));
        assert_eq!(
            departed.digest,
            Some(format!("{:x}", Sha256::digest(vec![b'b'; 65536])))
        );
        input.write_all(b"q").unwrap();
        assert!(child.0.wait().unwrap().success());
        // Both tmpfs mounts belonged only to the child's mount namespace.
        assert!(!Path::new(&path).exists());
    }

    #[tokio::test]
    #[ignore = "requires the root-owned mapping helper from live-resolver-namespace.py"]
    async fn an_inode_collision_cannot_lend_mapped_content_to_a_required_candidate() {
        use pbps_db::resolver::environment::{
            EnvironmentFacts, RuleVersion, ScopeReport, Verdict, compare_executables,
        };
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let helper = std::env::var("PBPS_MAPPING_FIXTURE_HELPER").unwrap();
        let directory = std::env::var("PBPS_MAPPING_FIXTURE_DIRECTORY").unwrap();
        let unrelated = format!("{directory}/a/mapping.so");
        let candidate = format!("{directory}/z/mapping.so");
        let aliases = [
            format!("{directory}/alias.so"),
            format!("{directory}/other.so"),
        ];
        let mut child = OwnedChild(
            Command::new(helper)
                .args(["candidate-inodes", &directory])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut input = child.0.stdin.take().unwrap();
        let mut output = BufReader::new(child.0.stdout.take().unwrap());
        let mut acknowledge = |expected: &str| {
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            assert_eq!(line.trim(), expected);
        };
        acknowledge("ready");
        for alias in &aliases {
            std::os::unix::fs::symlink(&candidate, alias).unwrap();
        }
        let lease = ProcessLease::capture(child.0.id()).unwrap();
        let mapped = mappings(&lease.read_proc("maps", 8 * 1024 * 1024).unwrap(), &[]);
        assert!(!mapped.contains_key(&candidate));
        let mapping = &mapped[&unrelated];
        let loaded = lease
            .open_proc(&format!("map_files/{:x}-{:x}", mapping.start, mapping.end))
            .unwrap();
        let disk = lease
            .open_in_root(aliases[0].trim_start_matches('/'))
            .unwrap();
        let loaded_meta = loaded.metadata().unwrap();
        let disk_meta = disk.metadata().unwrap();
        assert_eq!(mapping.inode, disk_meta.ino(), "a real inode collision");
        assert_ne!(loaded_meta.dev(), disk_meta.dev(), "two actual filesystems");
        assert_eq!(
            loaded_meta.len(),
            disk_meta.len(),
            "size alone is insufficient"
        );
        assert_ne!(
            digest_of(&loaded).await.unwrap(),
            digest_of(&disk).await.unwrap()
        );

        let compare = |target: &ExecutableSet, resolver: &ExecutableSet| {
            let facts = |executables: &ExecutableSet| EnvironmentFacts {
                catalog: CatalogFacts {
                    observations: BTreeMap::new(),
                    extensions: vec![],
                    available_extensions: BTreeMap::new(),
                    collations: vec![],
                    settings: BTreeMap::new(),
                    visibility: BTreeMap::new(),
                },
                executables: executables.clone(),
            };
            let mut report = ScopeReport::new(RuleVersion::new("candidate-fixture-v1"));
            compare_executables(
                "candidate-fixture-v1",
                &facts(target),
                &facts(resolver),
                &[],
                &mut report,
            );
            report.verdict()
        };
        let required = &aliases[..1];
        let first = executables(&lease, required, "$libdir", &[]).await.unwrap();
        input.write_all(b"c").unwrap();
        acknowledge("changed");
        let changed = executables(&lease, required, "$libdir", &[]).await.unwrap();
        assert_eq!(
            compare(&first, &changed),
            Verdict::Mismatch(vec![format!("library:{}", aliases[0])]),
            "different required bytes must not verify through an unrelated mapping"
        );
        for (set, byte) in [(&first, b'b'), (&changed, b'c')] {
            let identity = set.libraries.iter().find(|l| l.path == aliases[0]).unwrap();
            assert_eq!(identity.role, ExecutableRole::LateLoaded);
            assert_eq!(identity.provenance, Provenance::DiskCandidate);
            assert_eq!(
                identity.digest,
                Some(format!("{:x}", Sha256::digest(vec![byte; 65536])))
            );
            assert!(set.libraries.iter().any(|l| l.path == unrelated));
        }
        input.write_all(b"s").unwrap();
        acknowledge("changed");
        let shorter = executables(&lease, required, "$libdir", &[]).await.unwrap();
        let identity = shorter
            .libraries
            .iter()
            .find(|l| l.path == aliases[0])
            .unwrap();
        assert_eq!(identity.role, ExecutableRole::LateLoaded);
        assert_eq!(
            identity.digest,
            Some(format!("{:x}", Sha256::digest(vec![b'b'; 32768])))
        );

        // A genuine mapping sorts after the unrelated inode collision. Keep
        // searching, and preserve both aliases and the direct-plus-alias case.
        input.write_all(b"r").unwrap();
        acknowledge("changed");
        input.write_all(b"2").unwrap();
        acknowledge("mapped");
        let now_loaded = executables(&lease, required, "$libdir", &[]).await.unwrap();
        assert_eq!(compare(&first, &now_loaded), Verdict::Verified);
        for names in [
            aliases.to_vec(),
            vec![candidate.clone(), aliases[0].clone()],
        ] {
            let set = executables(&lease, &names, "$libdir", &[]).await.unwrap();
            assert!(set.libraries.iter().any(|l| l.path == unrelated));
            for name in &names {
                let identities: Vec<_> = set.libraries.iter().filter(|l| l.path == *name).collect();
                assert_eq!(identities.len(), 1);
                assert_eq!(identities[0].role, ExecutableRole::Preloaded);
                assert_eq!(identities[0].provenance, Provenance::LoadedContent);
                assert_eq!(
                    identities[0].digest,
                    Some(format!("{:x}", Sha256::digest(vec![b'b'; 65536])))
                );
            }
        }
        input.write_all(b"q").unwrap();
        assert!(child.0.wait().unwrap().success());
        assert!(!Path::new(&unrelated).exists() && !Path::new(&candidate).exists());
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
        let cwd = Path::new("/var/lib/postgresql/18/main");
        let names = |list: &[&str]| list.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        // The default library path is `$libdir`; a `$libdir/` name and an
        // absolute name are each tried as given and then with the platform
        // suffix, in that order, as the loader tries them (finding on #688).
        assert_eq!(
            resolve("$libdir/hstore", &libdir, "$libdir", cwd),
            names(&[
                "/usr/lib/postgresql/18/lib/hstore",
                "/usr/lib/postgresql/18/lib/hstore.so"
            ])
        );
        assert_eq!(
            resolve("auto_explain", &libdir, "$libdir", cwd),
            names(&[
                "/usr/lib/postgresql/18/lib/auto_explain",
                "/usr/lib/postgresql/18/lib/auto_explain.so"
            ])
        );
        // A name that already carries a suffix is still tried suffixed again
        // second, exactly as the loader does; the exact file comes first.
        assert_eq!(
            resolve("/opt/hooks/hook.so", &libdir, "$libdir", cwd),
            names(&["/opt/hooks/hook.so", "/opt/hooks/hook.so.so"])
        );
        assert_eq!(
            resolve("$libdir/plugins/x.so.1", &libdir, "$libdir", cwd),
            names(&[
                "/usr/lib/postgresql/18/lib/plugins/x.so.1",
                "/usr/lib/postgresql/18/lib/plugins/x.so.1.so"
            ])
        );
        // A relative name with a directory is the backend's working
        // directory's, the data directory, not the root's (finding on #688).
        assert_eq!(
            resolve("plugins/foo", &libdir, "$libdir", cwd),
            names(&[
                "/var/lib/postgresql/18/main/plugins/foo",
                "/var/lib/postgresql/18/main/plugins/foo.so"
            ])
        );
        // A bare name searches every directory of a custom path for the exact
        // name, then every directory for the suffixed one, with `$libdir`
        // expanded to the engine's library directory.
        assert_eq!(
            resolve("auto_explain", &libdir, "/opt/pg/lib:$libdir", cwd),
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
    #[tokio::test]
    async fn a_running_process_reports_its_executed_content_and_mapped_libraries() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        // Let the exec happen before the lease looks at the executable.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let lease = ProcessLease::capture(child.id()).unwrap();
        let set = executables(&lease, &["$libdir/no_such_library".into()], "$libdir", &[])
            .await
            .unwrap();
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

        // A required library installed through a symlink to a file the
        // process has mapped is that mapping, reported once and under the
        // spelling the engine names it by, not a second, late-loaded copy
        // beside the mapping's own path (finding on #688).
        let libc_path = libc.path.clone();
        let dir = std::env::temp_dir().join(format!("pbps-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Two required names that are two links to the one loaded file are
        // two identities, one per spelling, and none under the resolved
        // path (finding on #688).
        let aliases: Vec<String> = ["libc_alias.so", "libc_other.so"]
            .iter()
            .map(|name| {
                let alias = dir.join(name);
                let _ = std::fs::remove_file(&alias);
                std::os::unix::fs::symlink(&libc_path, &alias).unwrap();
                alias.to_string_lossy().into_owned()
            })
            .collect();
        let set = executables(&lease, &aliases, "$libdir", &[]).await.unwrap();
        for alias_name in &aliases {
            let through_alias: Vec<_> = set
                .libraries
                .iter()
                .filter(|l| l.path == *alias_name)
                .collect();
            assert_eq!(through_alias.len(), 1, "{alias_name}: {:?}", set.libraries);
            assert_eq!(through_alias[0].role, ExecutableRole::Preloaded);
            assert!(through_alias[0].digest.is_some());
            assert_eq!(through_alias[0].disk_differs_from_loaded, Some(false));
        }
        assert!(
            !set.libraries.iter().any(|l| l.path == libc_path),
            "the mapping is reported under the aliases alone: {:?}",
            set.libraries
        );
        // A relative name with a directory resolves against the process's
        // working directory — the test's own, which `sleep` inherited — and
        // is reported at that absolute path (finding on #688).
        let relative = executables(&lease, &["src/lib.rs".into()], "$libdir", &[])
            .await
            .unwrap();
        let expected = std::env::current_dir().unwrap().join("src/lib.rs");
        let found = relative
            .libraries
            .iter()
            .find(|l| l.path == expected.to_string_lossy())
            .expect("the relative name is anchored at the working directory");
        assert_eq!(found.role, ExecutableRole::LateLoaded);
        assert!(found.digest.is_some(), "{found:?}");
        // Required under its own path *and* an alias: both spellings, once
        // each, the mapping's own name not displaced by the alias (finding
        // on #688).
        let both = [libc_path.clone(), aliases[0].clone()];
        let set = executables(&lease, &both, "$libdir", &[]).await.unwrap();
        for spelling in &both {
            assert_eq!(
                set.libraries.iter().filter(|l| l.path == *spelling).count(),
                1,
                "{spelling}: {:?}",
                set.libraries
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// The negative controls for "disk is what runs", on a process whose
    /// executable the lease admits (root-owned, as every engine binary is):
    /// a path that names another inode, or none, differs from what is
    /// mapped, and a mapped library whose inode moved is reported as
    /// replaced rather than hashed from disk. Replacing a binary under a
    /// running engine end to end is the root fixture's case.
    #[tokio::test]
    async fn a_path_that_no_longer_names_the_mapped_inode_is_reported_as_differing() {
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
        let (libc_path, libc) = mappings(&maps, &[])
            .into_iter()
            .find(|(path, _)| path.contains("libc.so"))
            .expect("libc is mapped");
        let intact = mapped_library(&lease, &libc_path, &libc).await.identity;
        assert_eq!(intact.disk_differs_from_loaded, Some(false));
        assert!(intact.digest.is_some());
        // The same library with the inode the mapping would carry after a
        // replacement: nothing honest to hash without the mapped file object.
        let moved = Mapping {
            inode: libc.inode + 1,
            ..libc
        };
        let replaced = mapped_library(&lease, &libc_path, &moved).await.identity;
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

    #[tokio::test]
    async fn a_digest_is_of_the_content_and_independent_of_the_handles_cursor() {
        let dir = std::env::temp_dir().join(format!("pbps-exe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lib.so");
        std::fs::write(&path, b"hello, loader").unwrap();
        let mut file = File::open(&path).unwrap();
        let mut skip = [0u8; 5];
        file.read_exact(&mut skip).unwrap();
        let digest = digest_of(&file).await.unwrap();
        assert_eq!(digest, format!("{:x}", Sha256::digest(b"hello, loader")));
        std::fs::write(&path, b"hello, loader!").unwrap();
        assert_ne!(
            digest_of(&File::open(&path).unwrap()).await.unwrap(),
            digest
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The runs that call the census live on a current-thread runtime beside
    /// the watchers that keep their containers alive, and an engine's code
    /// can be over a gigabyte. Hashed on the runtime's own thread, one file
    /// stops every other task for as long as it takes — long enough, measured
    /// in CI on #611, for a watcher to miss its request budget and remove the
    /// run's channel. A sparse file costs no disk and still has to be read
    /// and hashed in full.
    #[tokio::test]
    async fn hashing_a_large_file_does_not_stop_the_runtimes_other_tasks() {
        let dir = std::env::temp_dir().join(format!("pbps-exe-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("engine.sfp");
        let file = File::create(&path).unwrap();
        file.set_len(192 * 1024 * 1024).unwrap();
        let file = File::open(&path).unwrap();

        let ticks = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = ticks.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
        let digest = digest_of(&file).await.unwrap();
        let turned = ticks.load(std::sync::atomic::Ordering::Relaxed);
        ticker.abort();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(digest.len(), 64);
        assert!(
            turned >= 5,
            "the ticker ran {turned} times while 192 MiB were hashed"
        );
    }
}
