//! Source-bearing loader names stay with the engine that interpreted them.
//! The source capability's owner supplies a held root and mapping inventory;
//! only a mapping index or an opaque reader crosses back. These operations
//! observe source, so ordinary captured evidence cannot acquire this capability
//! (DEC-974.1). Paths and raw handles remain opaque (DEC-876.1).

use super::{RuntimeInputs, Uncovered};
use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

/// Private candidate spellings can be compared inside one capture interval,
/// but cannot be read, formatted, serialized or constructed by a caller.
/// ```compile_fail,E0616
/// use pbps_pg::resolver::capture::RuntimeResolution;
/// fn expose(resolved: RuntimeResolution) -> Vec<Vec<String>> { resolved.candidates }
/// ```
/// ```compile_fail,E0277
/// use pbps_pg::resolver::capture::RuntimeResolution;
/// fn report(resolved: RuntimeResolution) -> String { format!("{resolved:?}") }
/// ```
/// ```compile_fail,E0277
/// use pbps_pg::resolver::capture::RuntimeResolution;
/// fn report(resolved: RuntimeResolution) { let _ = serde_json::to_string(&resolved); }
/// ```
#[derive(PartialEq, Eq)]
pub struct RuntimeResolution {
    candidates: Vec<Vec<String>>,
}

/// The selected candidate's position preserves loader precedence without
/// disclosing its spelling. Mapping indices refer only to the inventory the
/// caller already supplied. Readers expose bytes, never a path or raw handle.
pub enum NativeLibrary {
    Mapped {
        candidate: usize,
        mapping: usize,
    },
    Candidate {
        candidate: usize,
        reader: NativeLibraryReader,
    },
}

/// A reader deliberately has no Debug, AsFd/AsRawFd or File getter: even
/// File's ordinary Debug implementation can reveal a retained source path.
/// ```compile_fail,E0616
/// use pbps_pg::resolver::capture::NativeLibraryReader;
/// fn expose(reader: NativeLibraryReader) -> std::fs::File { reader.file }
/// ```
/// ```compile_fail,E0277
/// use pbps_pg::resolver::capture::NativeLibraryReader;
/// fn report(reader: NativeLibraryReader) -> String { format!("{reader:?}") }
/// ```
/// ```compile_fail,E0599
/// use pbps_pg::resolver::capture::NativeLibraryReader;
/// use std::os::fd::AsRawFd;
/// fn expose(reader: NativeLibraryReader) -> i32 { reader.as_raw_fd() }
/// ```
pub struct NativeLibraryReader {
    file: File,
}

impl NativeLibraryReader {
    pub fn read_at(&self, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
        self.file.read_at(buffer, offset)
    }

    pub fn read_to_end(&self, buffer: &mut Vec<u8>) -> std::io::Result<usize> {
        use std::io::Read;
        let mut file = &self.file;
        file.read_to_end(buffer)
    }

    /// The same non-path facts the lifecycle uses to correlate a candidate
    /// with already observed loaded content; not a content certification.
    pub fn identity(&self) -> std::io::Result<(u64, u64)> {
        let metadata = self.file.metadata()?;
        Ok((metadata.ino(), metadata.len()))
    }
}

impl RuntimeInputs {
    /// Prepare engine-specific loader candidates using already held process
    /// observations. All I/O still requires the lifecycle's root capability.
    pub fn resolve_native(&self, engine: &Path, cwd: &Path) -> RuntimeResolution {
        let libdir = engine
            .parent()
            .and_then(Path::parent)
            .map(|prefix| prefix.join("lib"))
            .unwrap_or_else(|| PathBuf::from("/lib"));
        RuntimeResolution {
            candidates: self
                .libraries
                .iter()
                .map(|name| {
                    native_library_candidates(name, &libdir, &self.dynamic_library_path, cwd)
                })
                .collect(),
        }
    }
}

impl RuntimeResolution {
    pub fn len(&self) -> usize {
        self.candidates.len()
    }
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// Resolve exactly one requirement so the lifecycle can hash and release
    /// its reader before opening another. This keeps the previous FD bound.
    /// This is a source-authorized operation, not a public captured-result
    /// accessor. Its capability holder must still bracket it with process/root
    /// lease checks; an arbitrary File is not native runtime admission.
    pub fn open(
        &self,
        index: usize,
        root: &File,
        mapped: &[String],
    ) -> Result<NativeLibrary, Uncovered> {
        let refusal = || {
            Uncovered::class(
                "native-library",
                "required executable content is unreadable or unqualified",
            )
        };
        let names = self.candidates.get(index).ok_or_else(refusal)?;
        // A mapped object still carries its loaded content after unlink. The
        // existing native qualifier gives that observation precedence too.
        for (candidate, name) in names.iter().enumerate() {
            if let Some(mapping) = mapped.iter().position(|path| path == name) {
                return Ok(NativeLibrary::Mapped { candidate, mapping });
            }
        }
        for (candidate, name) in names.iter().enumerate() {
            // IN_ROOT gives absolute symlinks the held process's root. This
            // operation never falls back to the inspector's filesystem.
            let mut remaining = 7;
            let opened = loop {
                let result = rustix::fs::openat2(
                    root,
                    name.trim_start_matches('/'),
                    rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                    rustix::fs::ResolveFlags::IN_ROOT,
                );
                match result {
                    Err(rustix::io::Errno::AGAIN) if remaining > 0 => remaining -= 1,
                    result => break result,
                }
            };
            if let Ok(fd) = opened {
                return Ok(NativeLibrary::Candidate {
                    candidate,
                    reader: NativeLibraryReader {
                        file: File::from(fd),
                    },
                });
            }
        }
        Err(refusal())
    }
}

/// Pure loader rules for already public caller-supplied names. This does not
/// inspect RuntimeInputs or export any captured source. PostgreSQL tries all
/// exact names before suffixed names; relative paths use the backend cwd.
pub fn native_library_candidates(
    name: &str,
    libdir: &Path,
    search: &str,
    cwd: &Path,
) -> Vec<String> {
    let bases: Vec<PathBuf> = if let Some(rest) = name.strip_prefix("$libdir/") {
        vec![libdir.join(rest)]
    } else if name.contains('/') {
        vec![cwd.join(name)]
    } else {
        let dirs: Vec<String> = search
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("pbps-inputs-876-{}", crate::catalog::probe_token()));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn write(&self, name: &str, bytes: &[u8]) {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        fn open(&self) -> File {
            File::open(&self.0).unwrap()
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn inputs(name: &str, search: &str) -> RuntimeResolution {
        RuntimeInputs {
            libraries: vec![name.into()],
            dynamic_library_path: search.into(),
        }
        .resolve_native(Path::new("/usr/pgsql/bin/postgres"), Path::new("/data"))
    }
    fn bytes(library: NativeLibrary) -> (usize, Vec<u8>) {
        let NativeLibrary::Candidate { candidate, reader } = library else {
            panic!("expected unopened candidate");
        };
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        (candidate, bytes)
    }

    #[test]
    fn exact_names_precede_suffixes_and_the_selected_position_is_retained() {
        let root = Root::new();
        root.write("first/hook.so", b"suffix");
        root.write("second/hook", b"exact");
        let resolution = inputs("hook", "/first:/second");
        assert_eq!(
            bytes(resolution.open(0, &root.open(), &[]).unwrap()),
            (1, b"exact".to_vec())
        );
        std::fs::remove_file(root.0.join("second/hook")).unwrap();
        assert_eq!(
            bytes(resolution.open(0, &root.open(), &[]).unwrap()),
            (2, b"suffix".to_vec())
        );
        assert!(resolution.open(1, &root.open(), &[]).is_err());
    }

    #[test]
    fn absolute_links_and_relative_names_stay_inside_the_held_root() {
        let root = Root::new();
        root.write("private/source_library", b"held root");
        std::fs::create_dir(root.0.join("data")).unwrap();
        std::os::unix::fs::symlink("/private/source_library", root.0.join("data/alias")).unwrap();
        assert_eq!(
            bytes(
                inputs("./alias", "$libdir")
                    .open(0, &root.open(), &[])
                    .unwrap()
            )
            .1,
            b"held root"
        );
        let error = match inputs("./missing_private_name", "/secret_search_path").open(
            0,
            &root.open(),
            &[],
        ) {
            Ok(_) => panic!("missing content was accepted"),
            Err(error) => error,
        };
        for text in [format!("{error}"), format!("{error:?}")] {
            assert!(!text.contains("missing_private_name") && !text.contains("secret_search_path"));
        }
    }

    #[test]
    fn an_opened_unreadable_candidate_cannot_fall_through_to_another_file() {
        let root = Root::new();
        std::fs::create_dir_all(root.0.join("first/hook")).unwrap();
        root.write("second/hook", b"must not substitute");
        let NativeLibrary::Candidate { candidate, reader } = inputs("hook", "/first:/second")
            .open(0, &root.open(), &[])
            .unwrap()
        else {
            panic!("expected candidate");
        };
        assert_eq!(candidate, 0);
        assert!(reader.read_at(&mut [0; 8], 0).is_err());
    }

    #[test]
    fn already_mapped_candidates_do_not_require_a_surviving_disk_name() {
        let root = Root::new();
        let resolution = inputs("$libdir/private_hook", "$libdir");
        assert!(matches!(
            resolution
                .open(0, &root.open(), &["/usr/pgsql/lib/private_hook.so".into()])
                .unwrap(),
            NativeLibrary::Mapped {
                candidate: 1,
                mapping: 0
            }
        ));
        assert!(resolution.open(0, &root.open(), &[]).is_err());
        assert!(resolution != inputs("$libdir/other_hook", "$libdir"));
    }
}
