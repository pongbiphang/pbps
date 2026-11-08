//! Read-only location of a test run's journal (DEC-2471.1). Record contents,
//! locks and removal belong to `test_scratch`; the resolver only defers to it.

use std::path::Path;

pub const RUN_LABEL: &str = "pbps.test.run";
pub const LOCATION_LABEL: &str = "pbps.test.journal";

#[cfg(target_os = "linux")]
pub use linux::{location, record_exists};

#[cfg(not(target_os = "linux"))]
pub fn location(_: &Path) -> Result<String, String> {
    Err("the test journal location cannot be established on this platform".into())
}

#[cfg(not(target_os = "linux"))]
pub fn record_exists(_: &str, _: &str) -> Result<bool, String> {
    Err("the test journal location cannot be checked on this platform".into())
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as BASE64};
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::{ffi::OsStringExt as _, fs::MetadataExt as _};
    use std::path::PathBuf;

    fn unread(why: impl std::fmt::Display) -> String {
        format!("the test journal location could not be checked: {why}")
    }

    fn boot() -> Result<String, String> {
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(unread)?;
        let boot = boot.trim();
        if boot.is_empty() {
            return Err(unread("the boot identity is empty"));
        }
        Ok(boot.to_owned())
    }

    /// Identifies the existing directory, including paths not representable as
    /// UTF-8. A shared daemon does not imply a shared client filesystem.
    pub fn location(directory: &Path) -> Result<String, String> {
        let path = fs::canonicalize(directory).map_err(unread)?;
        let metadata = fs::symlink_metadata(&path).map_err(unread)?;
        if !metadata.is_dir() {
            return Err(unread("the journal location is not a directory"));
        }
        Ok(format!(
            "{}:{}:{}:{}",
            boot()?,
            metadata.dev(),
            metadata.ino(),
            BASE64.encode(path.as_os_str().as_encoded_bytes())
        ))
    }

    /// Only an absent entry in the identified directory permits the resolver's
    /// sweep. Missing, inaccessible or replaced directories prove nothing.
    pub fn record_exists(location: &str, run: &str) -> Result<bool, String> {
        let invalid = || unread("the journal location or run identity is invalid");
        if run.len() != 32
            || !run
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid());
        }
        let mut parts = location.splitn(4, ':');
        if parts.next() != Some(boot()?.as_str()) {
            return Err(unread("the creating host boot cannot be identified here"));
        }
        let device = parts
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(invalid)?;
        let inode = parts
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(invalid)?;
        let bytes = BASE64
            .decode(parts.next().ok_or_else(invalid)?)
            .map_err(unread)?;
        let path = PathBuf::from(OsString::from_vec(bytes));
        if !path.is_absolute() {
            return Err(invalid());
        }
        let metadata = fs::symlink_metadata(&path).map_err(unread)?;
        if !metadata.is_dir() || metadata.dev() != device || metadata.ino() != inode {
            return Err(unread("the journal directory cannot be identified here"));
        }
        match fs::symlink_metadata(path.join(format!("{run}.json"))) {
            Ok(_) => Ok(true),
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(why) => Err(unread(why)),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::{ffi::OsStringExt as _, fs::PermissionsExt as _};
    use std::path::PathBuf;

    const RUN: &str = "0123456789abcdef0123456789abcdef";

    struct Directory(PathBuf);

    impl Directory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("pbps-journal-{:032x}", rand::random::<u128>()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn only_an_absent_record_in_the_identified_directory_permits_a_sweep() {
        let dir = Directory::new();
        let label = location(&dir.0).unwrap();
        assert_eq!(record_exists(&label, RUN), Ok(false));
        let record = dir.0.join(format!("{RUN}.json"));
        fs::write(&record, "not JSON").unwrap();
        fs::set_permissions(&record, fs::Permissions::from_mode(0o0)).unwrap();
        assert_eq!(
            record_exists(&label, RUN),
            Ok(true),
            "contents are never read"
        );
        fs::remove_file(&record).unwrap();
        std::os::unix::fs::symlink("missing", &record).unwrap();
        assert_eq!(
            record_exists(&label, RUN),
            Ok(true),
            "a broken link is not absence"
        );
        fs::remove_file(record).unwrap();
        assert_eq!(record_exists(&label, RUN), Ok(false));
        for invalid in ["", "../foreign", "A123456789abcdef0123456789abcdef"] {
            assert!(record_exists(&label, invalid).is_err());
        }
    }

    #[test]
    fn an_unreadable_or_replaced_location_cannot_supply_absence() {
        let dir = Directory::new();
        let label = location(&dir.0).unwrap();
        // CI and development run unprivileged; even a privileged invocation
        // still exercises malformed and replaced locations below.
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o0)).unwrap();
        let unreadable = record_exists(&label, RUN);
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o700)).unwrap();
        if !rustix::process::geteuid().is_root() {
            assert!(unreadable.is_err());
        }
        let saved = dir.0.with_extension("saved");
        fs::rename(&dir.0, &saved).unwrap();
        assert!(
            record_exists(&label, RUN).is_err(),
            "missing directory is unknown"
        );
        fs::create_dir(&dir.0).unwrap();
        assert!(
            record_exists(&label, RUN).is_err(),
            "same path, different inode"
        );
        fs::remove_dir(&dir.0).unwrap();
        std::os::unix::fs::symlink(&saved, &dir.0).unwrap();
        assert!(
            record_exists(&label, RUN).is_err(),
            "a redirected directory is unknown"
        );
        fs::remove_file(&dir.0).unwrap();
        fs::rename(saved, &dir.0).unwrap();
        assert_eq!(record_exists(&label, RUN), Ok(false));
        for invalid in [
            "",
            "elsewhere:0:0:Lw",
            &format!("{}:bad", label.split(':').next().unwrap()),
        ] {
            assert!(record_exists(invalid, RUN).is_err());
        }
    }

    #[test]
    fn locations_preserve_non_utf8_paths_and_do_not_search_other_registries() {
        let parent = Directory::new();
        let directory = parent
            .0
            .join(std::ffi::OsString::from_vec(b"runs-\xff".to_vec()));
        fs::create_dir(&directory).unwrap();
        let label = location(&directory).unwrap();
        fs::write(directory.join(format!("{RUN}.json")), "pending").unwrap();
        assert_eq!(record_exists(&label, RUN), Ok(true));
        let elsewhere = Directory::new();
        assert_eq!(
            record_exists(&location(&elsewhere.0).unwrap(), RUN),
            Ok(false)
        );
    }
}
