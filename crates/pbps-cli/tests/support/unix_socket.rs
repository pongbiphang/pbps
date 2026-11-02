//! Short, owned Unix socket paths for test synchronization and fake endpoints.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

const MAX_TMPDIR_BYTES: usize = 80;
const MAX_SOCKET_BYTES: usize = 92;

fn socket_path(base: &Path, nonce: u32) -> PathBuf {
    assert!(
        base.as_os_str().as_bytes().len() <= MAX_TMPDIR_BYTES,
        "Unix socket fixtures support TMPDIR up to {MAX_TMPDIR_BYTES} bytes: {base:?}"
    );
    // `/sXXXXXXXX/s` adds 12 bytes, independently of fixture names or depth.
    let path = base.join(format!("s{nonce:08x}")).join("s");
    assert!(path.as_os_str().as_bytes().len() <= MAX_SOCKET_BYTES);
    path
}

struct Directory(PathBuf);

impl Drop for Directory {
    fn drop(&mut self) {
        // Cleanup must also run while an assertion is unwinding.
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) struct Socket {
    listener: UnixListener,
    path: PathBuf,
    // Field order closes the listener before removing its owned directory.
    _directory: Directory,
}

impl Socket {
    pub(super) fn bind() -> Self {
        let base = std::env::temp_dir();
        for _ in 0..100 {
            let path = socket_path(&base, rand::random());
            let parent = path.parent().unwrap();
            match fs::DirBuilder::new().mode(0o700).create(parent) {
                Ok(()) => {}
                // Never remove a name we did not acquire, even on collision.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create socket directory {parent:?}: {error}"),
            }
            let directory = Directory(parent.to_owned());
            let listener = UnixListener::bind(&path)
                .unwrap_or_else(|error| panic!("could not bind test socket {path:?}: {error}"));
            return Self {
                listener,
                path,
                _directory: directory,
            };
        }
        panic!("could not acquire a unique socket directory under {base:?}");
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn listener(&self) -> &UnixListener {
        &self.listener
    }
}

#[test]
fn supported_tmpdir_lengths_keep_socket_paths_within_92_bytes() {
    for bytes in 1..=MAX_TMPDIR_BYTES {
        let base = PathBuf::from(format!("/{}", "x".repeat(bytes - 1)));
        for nonce in [0, u32::MAX] {
            let path = socket_path(&base, nonce);
            assert!(path.as_os_str().as_bytes().len() <= bytes + 12);
            assert!(path.as_os_str().as_bytes().len() <= MAX_SOCKET_BYTES);
        }
    }
    let base = PathBuf::from(format!("/{}x", "é".repeat(39)));
    assert_eq!(base.as_os_str().as_bytes().len(), 80);
    assert_eq!(socket_path(&base, 0).as_os_str().as_bytes().len(), 92);
}

#[test]
#[should_panic(expected = "Unix socket fixtures support TMPDIR up to 80 bytes")]
fn an_over_budget_tmpdir_is_refused_in_bytes_not_characters() {
    let base = PathBuf::from(format!("/{}", "é".repeat(40)));
    socket_path(&base, 0);
}

#[test]
fn sockets_are_distinct_connectable_and_removed_after_drop() {
    let first = Socket::bind();
    let second = Socket::bind();
    let first_path = first.path().to_owned();
    let second_path = second.path().to_owned();
    assert_ne!(first_path, second_path);
    let client = std::os::unix::net::UnixStream::connect(&first_path).unwrap();
    let accepted = first.listener().accept().unwrap();
    drop((client, accepted, first, second));
    assert!(!first_path.parent().unwrap().try_exists().unwrap());
    assert!(!second_path.parent().unwrap().try_exists().unwrap());
}

#[test]
fn a_panicking_socket_owner_still_removes_its_directory() {
    let mut path = None;
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let socket = Socket::bind();
        path = Some(socket.path().to_owned());
        panic!("owner assertion failed");
    }));
    assert!(panic.is_err());
    assert!(!path.unwrap().parent().unwrap().try_exists().unwrap());
}
