//! The file operations of ADR-0015 decision 5's step 2, and only those.
//!
//! Three rules shape everything here, and each was measured before it was
//! believed:
//!
//! 1. **Never reopen by name.** A directory component swapped for a link
//!    between opening a handle and reopening the path by name would have the
//!    UI read one file and `git` another. Every operation is relative to a
//!    directory handle walked from the worktree root.
//! 2. **`O_NOFOLLOW` guards the last component only**, and `RESOLVE_BENEATH`
//!    guards the worktree's boundary, not the path: with `dir/link` a link to
//!    a sibling directory, `openat2` of `dir/link/sub` under both opened the
//!    sibling's `sub`, and only `RESOLVE_NO_SYMLINKS` refused it (measured on
//!    Linux 6.6). So the walk refuses a link at *every* component.
//! 3. **Nothing is overwritten, only swapped.** The exchange makes the write
//!    reversible, so that a refusal anywhere later can put back exactly what
//!    was there, and so that what came out can be inspected before it is let
//!    go.
//!
//! This module is Linux-only (DECISIONS 523). macOS needs `renamex_np` and
//! `acl_get_fd_np`
//! and Windows the relative `NtCreateFile` walk, neither of which this
//! workspace can call while it forbids `unsafe`, and decision 5's own rule
//! for a platform without these calls is to refuse to compose rather than
//! overwrite a file it cannot prove is the one the page saw.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::Path;

use rustix::fs::{
    AtFlags, Mode, OFlags, RenameFlags, ResolveFlags, XattrFlags, fchmod, fchown, fgetxattr,
    flistxattr, fsetxattr, fstat, fsync, linkat, openat, openat2, renameat, renameat_with, statat,
    unlinkat,
};

use super::repo_path::RepoPath;

/// What a directory or a file *is*, as the kernel counts it. Two handles name
/// one object exactly when these agree.
///
/// The comparison is why it exists: a handle is proof of where a directory
/// *was*, and after every exchange and every `link()` the UI compares the
/// handle it acted through with a fresh no-link lookup of the same path. With
/// the handle open, the directory renamed out of the tree and another created
/// at its path, `link()` through the handle succeeded and put the file in the
/// moved directory while the fresh lookup named a different inode (measured).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub device: u64,
    pub inode: u64,
}

/// Everything the exchange has to carry across with a file, because
/// `git`'s `100644`/`100755` carries only the executable bit: a temporary
/// made from the tree mode alone would turn a `0600` file into a `0644` one
/// and drop its extended attributes and its ACL.
///
/// On Linux the POSIX ACL *is* an extended attribute
/// (`system.posix_acl_access`), so the attribute copy carries it and no
/// separate ACL interface is needed here; Darwin keeps one outside the list,
/// which is part of why this module is Linux-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// The permission bits, including setuid, setgid and the sticky bit.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub attributes: Vec<(OsString, Vec<u8>)>,
}

impl Metadata {
    pub fn executable(&self) -> bool {
        self.mode & 0o111 != 0
    }
}

/// A handle on a directory, opened without following a link at any component.
#[derive(Debug)]
pub struct Dir {
    fd: OwnedFd,
}

/// A file opened through a [`Dir`], kept for as long as the protocol needs to
/// be able to speak about *that* object rather than that name.
#[derive(Debug)]
pub struct FileAt {
    fd: OwnedFd,
}

/// A component of a path that is not a directory this UI will walk through.
#[derive(Debug)]
pub enum WalkRefusal {
    /// A component is a symbolic link. Named, because the user has to know
    /// which one: decision 5 refuses the layout rather than following it.
    Link(String),
    /// The path leaves the worktree, or a component is not a directory.
    Outside(String),
    Io(String, io::Error),
}

impl std::fmt::Display for WalkRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Link(component) => write!(
                f,
                "`{component}` is a symbolic link, and this UI does not compose through one"
            ),
            Self::Outside(component) => {
                write!(f, "`{component}` is not a directory inside the worktree")
            }
            Self::Io(component, e) => write!(f, "could not open `{component}`: {e}"),
        }
    }
}

impl Dir {
    /// The worktree root. Opened once; every other handle is walked from it.
    pub fn open_root(path: &Path) -> io::Result<Self> {
        let fd = openat(
            rustix::fs::CWD,
            path,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self { fd })
    }

    /// The directory holding `path`, and the leaf name within it.
    ///
    /// Each component is `lstat`ed and refused if it is a link — that is what
    /// is *checked*, and names the component for the page — and then opened
    /// with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, which is what *enforces*
    /// it against a link created between the two.
    pub fn walk_to_parent(&self, path: &RepoPath) -> Result<(Self, OsString), WalkRefusal> {
        let mut components: Vec<&[u8]> = path.components().collect();
        let leaf = components
            .pop()
            .expect("RepoPath has at least one component");
        let mut here = self
            .reopen()
            .map_err(|e| WalkRefusal::Io(".".to_owned(), e))?;
        for component in components {
            here = here.descend(component)?;
        }
        Ok((here, OsString::from_vec(leaf.to_vec())))
    }

    fn descend(&self, component: &[u8]) -> Result<Self, WalkRefusal> {
        let name = OsStr::from_bytes(component);
        let shown = || String::from_utf8_lossy(component).into_owned();
        let stat = statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|e| WalkRefusal::Io(shown(), e.into()))?;
        if stat.st_mode & S_IFMT == S_IFLNK {
            return Err(WalkRefusal::Link(shown()));
        }
        let fd = openat2(
            &self.fd,
            name,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|e| match e {
            rustix::io::Errno::LOOP | rustix::io::Errno::XDEV => WalkRefusal::Outside(shown()),
            other => WalkRefusal::Io(shown(), other.into()),
        })?;
        Ok(Self { fd })
    }

    /// A second handle on the same directory, for a walk that starts here.
    pub fn reopened(&self) -> io::Result<Self> {
        self.reopen()
    }

    /// A directory this process has just created inside `self`, opened the
    /// same no-link way as every other component: a snapshot's names came out
    /// of a tree, and a tree's names are bytes `git` stores rather than paths
    /// it has checked.
    pub fn open_directory(&self, name: &OsStr) -> io::Result<Self> {
        let fd = openat2(
            &self.fd,
            name,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
        )?;
        Ok(Self { fd })
    }

    fn reopen(&self) -> io::Result<Self> {
        let fd = openat(
            &self.fd,
            ".",
            OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self { fd })
    }

    pub fn identity(&self) -> io::Result<Identity> {
        identity_of(&self.fd)
    }

    /// `lstat` of a name in this directory: `None` where it is absent, which
    /// is the "a new declaration" case and not an error.
    pub fn look(&self, name: &OsStr) -> io::Result<Option<Look>> {
        match statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(Look {
                is_link: stat.st_mode & S_IFMT == S_IFLNK,
                is_regular: stat.st_mode & S_IFMT == S_IFREG,
                mode: stat.st_mode & 0o7777,
                identity: Identity {
                    device: stat.st_dev,
                    inode: stat.st_ino,
                },
            })),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Open a file in this directory without following a link at the leaf.
    /// `git hash-object` follows a link and hashes the target's content, so a
    /// linked declaration would commit its destination's bytes.
    pub fn open_file(&self, name: &OsStr) -> io::Result<FileAt> {
        let fd = openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(FileAt { fd })
    }

    /// Create a file that must not exist, for the temporary an exchange or a
    /// `link()` is made from.
    ///
    /// Opened for reading as well as writing: the protocol reads the
    /// temporary back through *this* descriptor, and after the exchange that
    /// descriptor is bound to the inode now at the path, which is how the UI
    /// checks what went in without reopening a name.
    pub fn create_new(&self, name: &OsStr, mode: u32) -> io::Result<FileAt> {
        let fd = openat(
            &self.fd,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode),
        )?;
        Ok(FileAt { fd })
    }

    /// `renameat2(RENAME_EXCHANGE)`: the two names swap what they hold, in one
    /// step, with nothing overwritten and nothing missing in between.
    pub fn exchange(&self, one: &OsStr, other: &OsStr) -> io::Result<()> {
        renameat_with(&self.fd, one, &self.fd, other, RenameFlags::EXCHANGE)?;
        Ok(())
    }

    /// `link()`: creates the name only if it does not exist, and refuses with
    /// `EEXIST` if something made it first, so a file that appeared in the
    /// window is never replaced.
    pub fn link(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        linkat(&self.fd, from, &self.fd, to, AtFlags::empty())?;
        Ok(())
    }

    /// The only way the UI takes a name away in a rollback. Never `unlinkat`:
    /// the entry may be an editor's inode by then — after a save-by-rename the
    /// path held a different inode with a link count of one (measured) — and
    /// an unlink would take its only name with it.
    pub fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        renameat(&self.fd, from, &self.fd, to)?;
        Ok(())
    }

    /// The one name the UI ever unlinks: the temporary its own `link()` was
    /// made from, after step 5 has moved the branch.
    pub fn unlink(&self, name: &OsStr) -> io::Result<()> {
        unlinkat(&self.fd, name, AtFlags::empty())?;
        Ok(())
    }

    pub fn create_directory(&self, name: &OsStr) -> io::Result<()> {
        match rustix::fs::mkdirat(&self.fd, name, Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Flushed before a phase that claims the tree on disk is the one the
    /// record describes: a machine that stopped with the branch moved and the
    /// placements still in a cache would be recovered into a checkout holding
    /// the old bytes.
    pub fn sync(&self) -> io::Result<()> {
        fsync(&self.fd)?;
        Ok(())
    }

    /// The identity check after every placement: this handle against a fresh
    /// no-link lookup of the same path from the root.
    pub fn still_at(&self, root: &Dir, path: &RepoPath) -> Result<bool, WalkRefusal> {
        let (fresh, _) = root.walk_to_parent(path)?;
        let mine = self
            .identity()
            .map_err(|e| WalkRefusal::Io(path.to_string(), e))?;
        let theirs = fresh
            .identity()
            .map_err(|e| WalkRefusal::Io(path.to_string(), e))?;
        Ok(mine == theirs)
    }
}

/// What an `lstat` of a name answered.
#[derive(Debug, Clone, Copy)]
pub struct Look {
    pub is_link: bool,
    pub is_regular: bool,
    pub mode: u32,
    pub identity: Identity,
}

impl FileAt {
    pub fn identity(&self) -> io::Result<Identity> {
        identity_of(&self.fd)
    }

    pub fn read(&self) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut offset = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = rustix::io::pread(&self.fd, &mut buffer, offset)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            offset += read as u64;
        }
        Ok(bytes)
    }

    pub fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < bytes.len() {
            written += rustix::io::pwrite(&self.fd, &bytes[written..], written as u64)?;
        }
        Ok(())
    }

    pub fn flush(&self) -> io::Result<()> {
        fsync(&self.fd)?;
        Ok(())
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        let stat = fstat(&self.fd)?;
        Ok(Metadata {
            mode: stat.st_mode & 0o7777,
            uid: stat.st_uid,
            gid: stat.st_gid,
            attributes: self.attributes()?,
        })
    }

    /// The order is the measured part: `fchown` clears a setuid or setgid bit
    /// even when it sets the owner the file already has, so the owner is
    /// copied *first* and the permission bits after it, or the bits would be
    /// gone by the time anything read them back.
    pub fn apply(&self, wanted: &Metadata) -> io::Result<()> {
        let uid = rustix::fs::Uid::from_raw(wanted.uid);
        let gid = rustix::fs::Gid::from_raw(wanted.gid);
        // A process that may not set the owner is the ordinary case for a
        // file it already owns; a failure here is only fatal when it leaves
        // the read-back disagreeing, which is what decides below.
        let _ = fchown(&self.fd, Some(uid), Some(gid));
        fchmod(&self.fd, Mode::from_raw_mode(wanted.mode))?;
        for (name, value) in &wanted.attributes {
            fsetxattr(&self.fd, name.as_os_str(), value, XattrFlags::empty())?;
        }
        Ok(())
    }

    fn attributes(&self) -> io::Result<Vec<(OsString, Vec<u8>)>> {
        let mut probe: [u8; 0] = [];
        let size = flistxattr(&self.fd, &mut probe)?;
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut names = vec![0u8; size];
        let filled = flistxattr(&self.fd, &mut names)?;
        names.truncate(filled);
        let mut attributes = Vec::new();
        for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
            let name = OsStr::from_bytes(name);
            let mut probe: [u8; 0] = [];
            let size = fgetxattr(&self.fd, name, &mut probe)?;
            let mut value = vec![0u8; size];
            let filled = fgetxattr(&self.fd, name, &mut value)?;
            value.truncate(filled);
            attributes.push((name.to_owned(), value));
        }
        attributes.sort();
        Ok(attributes)
    }
}

impl AsFd for FileAt {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

fn identity_of<F: AsFd>(fd: F) -> io::Result<Identity> {
    let stat = fstat(fd)?;
    Ok(Identity {
        device: stat.st_dev,
        inode: stat.st_ino,
    })
}

/// The file-type bits of `st_mode`. Named here rather than reached for
/// through a C constant, so that the two questions this module asks of a
/// `stat` — is it a link, is it a regular file — read the same way in both
/// places.
const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;
const S_IFREG: u32 = 0o100000;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn scratch(name: &str) -> std::path::PathBuf {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let directory = std::env::temp_dir().join(format!(
            "pbps-fsx-{name}-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch directory");
        directory
    }

    fn path(text: &str) -> RepoPath {
        RepoPath::new(text.as_bytes()).expect("a path the tests spell correctly")
    }

    #[test]
    fn a_link_at_any_component_is_refused_and_the_component_is_named() {
        // `O_NOFOLLOW` guards the last component only and `RESOLVE_BENEATH`
        // the worktree's boundary, so a link *inside* the worktree is followed
        // under both (measured on Linux 6.6). This is the check that catches
        // it, and the user has to be told which component it was.
        let root = scratch("link-component");
        std::fs::create_dir_all(root.join("schema/real")).unwrap();
        std::fs::write(root.join("schema/real/a.yml"), b"x").unwrap();
        std::os::unix::fs::symlink("real", root.join("schema/hop")).unwrap();
        let dir = Dir::open_root(&root).unwrap();

        let refusal = dir.walk_to_parent(&path("schema/hop/a.yml")).unwrap_err();
        match &refusal {
            WalkRefusal::Link(component) => assert_eq!(component, "hop"),
            other @ (WalkRefusal::Outside(_) | WalkRefusal::Io(..)) => {
                panic!("expected the link to be named, got {other:?}")
            }
        }
        // The control: the same file reached without the hop is walked.
        assert!(dir.walk_to_parent(&path("schema/real/a.yml")).is_ok());
    }

    #[test]
    fn the_leaf_itself_is_never_followed_either() {
        // `git hash-object` follows a link and hashes the target's content, so
        // a linked declaration would commit its destination's bytes.
        let root = scratch("link-leaf");
        std::fs::write(root.join("target.yml"), b"secret").unwrap();
        std::os::unix::fs::symlink("target.yml", root.join("a.yml")).unwrap();
        let dir = Dir::open_root(&root).unwrap();
        let (parent, leaf) = dir.walk_to_parent(&path("a.yml")).unwrap();

        let look = parent.look(&leaf).unwrap().expect("the name is there");
        assert!(look.is_link, "the leaf is a link and must be seen as one");
        assert!(parent.open_file(&leaf).is_err(), "and must not be opened");
    }

    #[test]
    fn an_exchange_swaps_the_names_and_the_handle_follows_the_inode_to_the_path() {
        // The measured claim step 2 rests on: after the exchange, `fstat` of
        // the descriptor the temporary was written through names the inode now
        // at the path, and reads the new bytes back. Without it there would be
        // no way to read back what went *in* other than by reopening the name.
        let root = scratch("exchange");
        std::fs::write(root.join("a.yml"), b"old").unwrap();
        let dir = Dir::open_root(&root).unwrap();
        let (parent, leaf) = dir.walk_to_parent(&path("a.yml")).unwrap();
        let temporary = OsString::from("a.yml.pbps-ui");
        let replacement = parent.create_new(&temporary, 0o600).unwrap();
        replacement.write_all(b"new").unwrap();

        parent.exchange(&leaf, &temporary).unwrap();

        assert_eq!(replacement.read().unwrap(), b"new");
        assert_eq!(
            replacement.identity().unwrap(),
            parent.look(&leaf).unwrap().unwrap().identity,
            "the handle names the inode the path now holds"
        );
        assert_eq!(std::fs::read(root.join("a.yml")).unwrap(), b"new");
        assert_eq!(
            std::fs::read(root.join("a.yml.pbps-ui")).unwrap(),
            b"old",
            "and what came out is beside the path, intact, to be compared"
        );

        // Reversible, which is the whole reason for an exchange: the refusal
        // path puts both back with the same call.
        parent.exchange(&leaf, &temporary).unwrap();
        assert_eq!(std::fs::read(root.join("a.yml")).unwrap(), b"old");
    }

    #[test]
    fn the_metadata_copy_carries_what_a_tree_mode_does_not() {
        // `git`'s `100644`/`100755` carries only the executable bit, so a
        // temporary made from the tree mode alone would turn a `0600` file
        // into a `0644` one and drop its extended attributes.
        let root = scratch("metadata");
        std::fs::write(root.join("a.yml"), b"old").unwrap();
        let dir = Dir::open_root(&root).unwrap();
        let (parent, leaf) = dir.walk_to_parent(&path("a.yml")).unwrap();
        let original = parent.open_file(&leaf).unwrap();
        original
            .apply(&Metadata {
                mode: 0o600,
                uid: original.metadata().unwrap().uid,
                gid: original.metadata().unwrap().gid,
                attributes: Vec::new(),
            })
            .unwrap();
        let wanted = original.metadata().unwrap();
        assert_eq!(wanted.mode, 0o600);

        let temporary = OsString::from("a.yml.pbps-ui");
        // The default an editor would give it, deliberately different.
        let replacement = parent.create_new(&temporary, 0o644).unwrap();
        replacement.write_all(b"new").unwrap();
        replacement.apply(&wanted).unwrap();

        let copied = replacement.metadata().unwrap();
        assert_eq!(
            copied, wanted,
            "the read-back is what decides, not the call"
        );
    }

    #[test]
    fn the_owner_is_copied_before_the_bits_so_the_setuid_and_setgid_bits_survive() {
        // `fchown` clears a setuid or setgid bit even when it sets the owner
        // the file already has, so bits copied first would be gone. `apply`
        // fixes the order; this is the test that fails if it is reversed.
        //
        // The fixture is `04710` and not `02600`, and that is the whole test:
        // **measured** on Linux 6.6, `chown` to the file's own owner left
        // `02600` alone — setgid without group-execute is the mandatory-lock
        // combination the kernel does not clear — while `04600`, `04700` and
        // `02710` were all cleared. A fixture in that one exempt shape passes
        // whichever order `apply` uses, which is a test passing for the wrong
        // reason rather than a protocol that works.
        let root = scratch("setid");
        std::fs::write(root.join("a.yml"), b"x").unwrap();
        let dir = Dir::open_root(&root).unwrap();
        let (parent, leaf) = dir.walk_to_parent(&path("a.yml")).unwrap();
        let file = parent.open_file(&leaf).unwrap();
        let mut wanted = file.metadata().unwrap();
        wanted.mode = 0o4710;

        let temporary = OsString::from("a.yml.pbps-ui");
        let replacement = parent.create_new(&temporary, 0o600).unwrap();
        replacement.apply(&wanted).unwrap();

        assert_eq!(
            replacement.metadata().unwrap().mode,
            0o4710,
            "the set-user-id bit is still there after the copy"
        );
    }

    #[test]
    fn a_name_something_else_created_in_the_window_is_refused_not_replaced() {
        // A path that is absent has nothing to exchange with and is placed
        // with `link()`, which creates the name only if it does not exist.
        let root = scratch("link-exists");
        let dir = Dir::open_root(&root).unwrap();
        let (parent, leaf) = dir.walk_to_parent(&path("new.yml")).unwrap();
        let temporary = OsString::from("new.yml.pbps-ui");
        parent
            .create_new(&temporary, 0o644)
            .unwrap()
            .write_all(b"ours")
            .unwrap();

        // Something made the name first.
        std::fs::write(root.join("new.yml"), b"theirs").unwrap();

        let refused = parent.link(&temporary, &leaf).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(root.join("new.yml")).unwrap(), b"theirs");
    }

    #[test]
    fn a_handle_is_proof_of_where_a_directory_was_and_says_so_when_it_moves() {
        // With the handle open, the directory renamed out of the tree and
        // another created at its path, an operation through the handle still
        // succeeds and puts the file in the moved directory (measured). The
        // identity comparison is what catches it.
        let root = scratch("moved-ancestor");
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::write(root.join("schema/a.yml"), b"x").unwrap();
        let dir = Dir::open_root(&root).unwrap();
        let (parent, _) = dir.walk_to_parent(&path("schema/a.yml")).unwrap();
        assert!(parent.still_at(&dir, &path("schema/a.yml")).unwrap());

        std::fs::rename(root.join("schema"), root.join("moved-away")).unwrap();
        std::fs::create_dir(root.join("schema")).unwrap();

        assert!(
            !parent.still_at(&dir, &path("schema/a.yml")).unwrap(),
            "the handle and a fresh lookup no longer name one directory"
        );
    }

    #[test]
    fn a_rollback_renames_a_name_away_rather_than_unlinking_an_editors_inode() {
        // After an editor's save-by-rename the path holds a different inode
        // with a link count of one (measured), and an `unlinkat` of the path
        // would take the editor's only name with it. A rename moves whatever
        // the entry holds and needs no check of whose inode that is.
        let root = scratch("rollback-rename");
        let dir = Dir::open_root(&root).unwrap();
        let (parent, leaf) = dir.walk_to_parent(&path("new.yml")).unwrap();
        parent
            .create_new(&OsString::from("new.yml.pbps-ui"), 0o644)
            .unwrap();
        parent
            .link(&OsString::from("new.yml.pbps-ui"), &leaf)
            .unwrap();

        // The editor saves by writing a temporary and renaming it over.
        std::fs::write(root.join(".editor-swap"), b"theirs").unwrap();
        std::fs::rename(root.join(".editor-swap"), root.join("new.yml")).unwrap();

        parent
            .rename(&leaf, &OsString::from("new.yml.rolled-back"))
            .unwrap();

        assert!(!root.join("new.yml").exists(), "the path is absent again");
        assert_eq!(
            std::fs::read(root.join("new.yml.rolled-back")).unwrap(),
            b"theirs",
            "and the editor's bytes are still under a name they can find"
        );
    }
}
