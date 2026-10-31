//! Keyed fingerprints of captured target inputs (#952, DEC-952.1).
//!
//! A fingerprint over a definition that carries a literal is a guessing
//! oracle when anyone can compute it: knowing the rest of the definition, a
//! reader tests candidate literals against the digest (ADR-0016 decision 5).
//! Hiding the digest was DEC-868.1's answer, and proving who could read it did
//! not converge. Keying it is this module's: under HMAC-SHA256 with a secret
//! key, a reader without the key cannot compute a candidate's fingerprint, so
//! the fingerprint — and every checksum derived from it — stops being a
//! verifier worth hiding.
//!
//! Two kinds of key exist. An environment's key ([`FingerprintKey::from_env`],
//! [`FingerprintKey::from_file`]) makes fingerprints that outlive the process —
//! sealed into a plan and compared again at apply (#614, #616) — so plan and
//! apply must hold the same key, and a plan records its [`KeyId`] to say which.
//! The process key ([`FingerprintKey::process`]) is for fingerprints compared
//! and dropped within one process, where only equality matters: it is random,
//! never leaves memory, and keeps any bare SHA-256 over a private input from
//! existing at all.
//!
//! pbps generates keys ([`FingerprintKey::generate`]) and checks them; it does
//! not store, share or rotate them. The key lives where the connection string
//! does — an environment variable or a file the operator's secret tooling
//! provides — which is why this is not a key-management system.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2_hmac::Sha256;

/// The shortest key accepted, in bytes: HMAC-SHA256's own output length. A
/// shorter secret is guessable before the literal it protects is.
pub const MIN_KEY_BYTES: usize = 32;

/// The length [`FingerprintKey::generate`] produces.
pub const GENERATED_KEY_BYTES: usize = 32;

/// The label the key identifier is computed under; a new derivation needs a
/// new label, never a changed one, so an old identifier keeps its meaning.
const KEY_ID_LABEL: &[u8] = b"pbps/fingerprint-key-id/v1";

/// A secret key for fingerprints. Its bytes are never printed: `Debug` shows
/// the [`KeyId`] only, and there is no `Display`.
#[derive(Clone)]
pub struct FingerprintKey {
    bytes: Vec<u8>,
}

/// A configured environment key, suitable for persisted resolver evidence.
/// A process-local comparison key cannot inhabit this type, even when cloned.
/// The loader chooses the target environment before constructing it.
#[derive(Clone, Debug)]
pub struct EnvironmentFingerprintKey(FingerprintKey);

impl EnvironmentFingerprintKey {
    pub fn from_env(name: &str) -> Result<Self, KeyError> {
        FingerprintKey::from_env(name).map(Self)
    }

    pub fn from_file(path: &Path) -> Result<Self, KeyError> {
        FingerprintKey::from_file(path).map(Self)
    }

    pub fn id(&self) -> KeyId {
        self.0.id()
    }

    pub fn fingerprint(&self, rule: &str, component: &str, input: &[u8]) -> [u8; 32] {
        self.0.fingerprint(rule, component, input)
    }
}

/// A public name for a key: the first 8 bytes of an HMAC under the key, in
/// hex. It tells two keys apart without saying anything usable about either.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyId(String);

impl KeyId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a configured key could not be used. Each names its source and never
/// the key.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("the fingerprint key variable `{0}` is not set")]
    MissingVariable(String),
    #[error(
        "the fingerprint key variable `{0}` is set but is not valid text; it must hold the \
         base64 `pbps key generate` prints"
    )]
    UnreadableVariable(String),
    #[error(
        "the fingerprint key file `{0}` cannot be proved readable by its owner only on this \
         platform; deliver the key through `fingerprint_key_env` instead"
    )]
    UnverifiableFile(PathBuf),
    #[error("the fingerprint key file `{path}` cannot be read: {reason}")]
    UnreadableFile { path: PathBuf, reason: String },
    #[error(
        "the fingerprint key file `{path}` is readable by others (mode {mode:o}); \
         make it owner-only: chmod 600 {path}"
    )]
    OpenFile { path: PathBuf, mode: u32 },
    #[error("the fingerprint key from {source_name} is not base64")]
    NotBase64 { source_name: String },
    #[error(
        "the fingerprint key from {source_name} is {len} bytes; at least {MIN_KEY_BYTES} are \
         needed. Generate one with `pbps key generate`"
    )]
    TooShort { source_name: String, len: usize },
}

impl FingerprintKey {
    /// A key from its base64 text, as `pbps key generate` prints it.
    /// Surrounding whitespace is ignored; `source_name` only labels errors.
    pub fn parse(text: &str, source_name: &str) -> Result<Self, KeyError> {
        let bytes = STANDARD
            .decode(text.trim())
            .map_err(|_| KeyError::NotBase64 {
                source_name: source_name.to_owned(),
            })?;
        if bytes.len() < MIN_KEY_BYTES {
            return Err(KeyError::TooShort {
                source_name: source_name.to_owned(),
                len: bytes.len(),
            });
        }
        Ok(Self { bytes })
    }

    /// An environment's key from the variable `name`.
    pub fn from_env(name: &str) -> Result<Self, KeyError> {
        Self::from_variable(name, std::env::var(name))
    }

    /// Unset and set-but-unreadable are different faults with different
    /// remedies; neither may read as the other. Split from [`Self::from_env`]
    /// so the unreadable case is testable without mutating the process
    /// environment.
    fn from_variable(
        name: &str,
        value: Result<String, std::env::VarError>,
    ) -> Result<Self, KeyError> {
        let text = value.map_err(|e| match e {
            std::env::VarError::NotPresent => KeyError::MissingVariable(name.to_owned()),
            std::env::VarError::NotUnicode(_) => KeyError::UnreadableVariable(name.to_owned()),
        })?;
        Self::parse(&text, &format!("`{name}`"))
    }

    /// An environment's key from the file at `path`, which must not be
    /// readable by group or others: a key in a world-readable file is a key
    /// everyone on the host holds.
    #[cfg(unix)]
    pub fn from_file(path: &Path) -> Result<Self, KeyError> {
        // A FIFO can block in open itself, before its type can be checked.
        let file = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| KeyError::UnreadableFile {
            path: path.to_owned(),
            reason: std::io::Error::from(error).to_string(),
        })?;
        Self::from_open_file(path, file.into())
    }

    #[cfg(unix)]
    fn from_open_file(path: &Path, mut file: std::fs::File) -> Result<Self, KeyError> {
        use std::io::Read;
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};

        let unreadable = |e: std::io::Error| KeyError::UnreadableFile {
            path: path.to_owned(),
            reason: e.to_string(),
        };
        // The path may be replaced after open. Both the permission proof and
        // the bytes must belong to this handle, including through a symlink.
        let metadata = file.metadata().map_err(unreadable)?;
        let kind = metadata.file_type();
        if !kind.is_file() {
            let kind = if kind.is_fifo() {
                "FIFO"
            } else if kind.is_dir() {
                "directory"
            } else if kind.is_socket() {
                "socket"
            } else if kind.is_block_device() {
                "block device"
            } else if kind.is_char_device() {
                "character device"
            } else {
                "non-regular file"
            };
            return Err(KeyError::UnreadableFile {
                path: path.to_owned(),
                reason: format!("expected a regular file, found {kind}"),
            });
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(KeyError::OpenFile {
                path: path.to_owned(),
                mode,
            });
        }
        let mut text = String::new();
        file.read_to_string(&mut text).map_err(unreadable)?;
        Self::parse(&text, &format!("`{}`", path.display()))
    }

    /// Where owner-only access cannot be proved — any platform without Unix
    /// permission bits, whose ACLs this does not read — a key file is refused
    /// rather than trusted; the variable form remains.
    #[cfg(not(unix))]
    pub fn from_file(path: &Path) -> Result<Self, KeyError> {
        Err(KeyError::UnverifiableFile(path.to_owned()))
    }

    /// This process's key, random and made once: for fingerprints compared
    /// and dropped within the process, never for one that is kept.
    pub fn process() -> &'static Self {
        static KEY: OnceLock<FingerprintKey> = OnceLock::new();
        KEY.get_or_init(|| Self {
            bytes: rand::random::<[u8; GENERATED_KEY_BYTES]>().to_vec(),
        })
    }

    /// A new random key, as base64 text: what `pbps key generate` prints.
    pub fn generate() -> String {
        STANDARD.encode(rand::random::<[u8; GENERATED_KEY_BYTES]>())
    }

    /// This key's public identifier.
    pub fn id(&self) -> KeyId {
        let mac = self.mac(&[KEY_ID_LABEL]);
        KeyId(mac[..8].iter().map(|b| format!("{b:02x}")).collect())
    }

    /// HMAC-SHA256 over `parts`, each framed by its length so that no two
    /// sequences of parts share a message.
    pub fn mac(&self, parts: &[&[u8]]) -> [u8; 32] {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.bytes)
            .expect("HMAC takes a key of any length");
        for part in parts {
            mac.update(&(part.len() as u64).to_be_bytes());
            mac.update(part);
        }
        mac.finalize().into_bytes().into()
    }

    /// The fingerprint of `input` under a versioned `rule` and a `component`
    /// label: the keyed form of the digest the resolver capture compares.
    pub fn fingerprint(&self, rule: &str, component: &str, input: &[u8]) -> [u8; 32] {
        self.mac(&[rule.as_bytes(), component.as_bytes(), input])
    }
}

impl fmt::Debug for FingerprintKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FingerprintKey({})", self.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> FingerprintKey {
        FingerprintKey {
            bytes: vec![byte; 32],
        }
    }

    /// RFC 4231 test case 2 pins the primitive itself: an HMAC-SHA256 that
    /// drifted from the standard would still be deterministic and pass every
    /// other test here.
    #[test]
    fn the_mac_is_standard_hmac_sha256() {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(b"Jefe").unwrap();
        mac.update(b"what do ya want for nothing?");
        let hex: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn a_fingerprint_depends_on_the_key_the_rule_the_component_and_the_input() {
        let base = key(1).fingerprint("rule/1", "properties", b"definition");
        assert_eq!(
            base,
            key(1).fingerprint("rule/1", "properties", b"definition")
        );
        for other in [
            key(2).fingerprint("rule/1", "properties", b"definition"),
            key(1).fingerprint("rule/2", "properties", b"definition"),
            key(1).fingerprint("rule/1", "bindings", b"definition"),
            key(1).fingerprint("rule/1", "properties", b"definitioN"),
        ] {
            assert_ne!(base, other);
        }
    }

    /// The property the key exists for: the fingerprint is not the bare
    /// SHA-256 a reader without the key could compute from a guess.
    #[test]
    fn a_fingerprint_is_not_the_unkeyed_digest_of_its_input() {
        use sha2_hmac::Digest;
        let input = b"CREATE FUNCTION f() RETURNS text AS $$ SELECT 'secret' $$";
        let bare: [u8; 32] = Sha256::digest(input).into();
        assert_ne!(key(1).fingerprint("r", "c", input), bare);
        assert_ne!(key(1).mac(&[input]), bare);
    }

    /// Length framing: moving a boundary between parts changes the message.
    #[test]
    fn parts_cannot_be_reframed_into_one_another() {
        let k = key(3);
        assert_ne!(k.mac(&[b"ab", b"c"]), k.mac(&[b"a", b"bc"]));
        assert_ne!(k.mac(&[b"abc"]), k.mac(&[b"ab", b"c"]));
    }

    #[test]
    fn a_generated_key_parses_and_has_a_stable_id_that_is_not_the_key() {
        let text = FingerprintKey::generate();
        let k = FingerprintKey::parse(&format!("  {text}\n"), "test").unwrap();
        assert_eq!(k.bytes.len(), GENERATED_KEY_BYTES);
        assert_eq!(k.id(), k.id());
        assert_eq!(k.id().as_str().len(), 16);
        assert!(!text.contains(k.id().as_str()));
        assert_ne!(k.id(), key(9).id());
        // Nothing prints the key.
        assert!(!format!("{k:?}").contains(&text));
        // Two generations differ.
        assert_ne!(FingerprintKey::generate(), text);
    }

    #[test]
    fn a_short_or_malformed_key_is_refused_by_its_source() {
        let short = STANDARD.encode([7u8; MIN_KEY_BYTES - 1]);
        assert_eq!(
            FingerprintKey::parse(&short, "`K`").unwrap_err(),
            KeyError::TooShort {
                source_name: "`K`".into(),
                len: MIN_KEY_BYTES - 1
            }
        );
        assert!(matches!(
            FingerprintKey::parse("not base64 !!", "`K`"),
            Err(KeyError::NotBase64 { .. })
        ));
        assert_eq!(
            FingerprintKey::from_env("PBPS_TEST_SURELY_UNSET_FINGERPRINT_KEY").unwrap_err(),
            KeyError::MissingVariable("PBPS_TEST_SURELY_UNSET_FINGERPRINT_KEY".into())
        );
    }

    #[test]
    fn a_set_but_unreadable_variable_is_not_reported_as_unset() {
        let unreadable = std::env::VarError::NotUnicode(std::ffi::OsString::from("x"));
        assert_eq!(
            FingerprintKey::from_variable("K", Err(unreadable)).unwrap_err(),
            KeyError::UnreadableVariable("K".into())
        );
        assert_eq!(
            FingerprintKey::from_variable("K", Err(std::env::VarError::NotPresent)).unwrap_err(),
            KeyError::MissingVariable("K".into())
        );
        let key = FingerprintKey::generate();
        assert!(FingerprintKey::from_variable("K", Ok(key)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_key_file_must_be_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pbps-fp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, FingerprintKey::generate()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            FingerprintKey::from_file(&path),
            Err(KeyError::OpenFile { mode: 0o644, .. })
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(FingerprintKey::from_file(&path).is_ok());
        assert!(matches!(
            FingerprintKey::from_file(&dir.join("absent")),
            Err(KeyError::UnreadableFile { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    struct KeyDirectory(PathBuf);

    #[cfg(unix)]
    impl KeyDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("pbps-key-956-{:032x}", rand::random::<u128>()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    #[cfg(unix)]
    impl Drop for KeyDirectory {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[cfg(unix)]
    fn write_key(path: &Path, text: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_key_path_does_not_replace_the_checked_bytes() {
        let dir = KeyDirectory::new();
        let path = dir.0.join("key");
        let replacement = dir.0.join("replacement");
        let original = STANDARD.encode([1_u8; MIN_KEY_BYTES]);
        let public = STANDARD.encode([2_u8; MIN_KEY_BYTES]);
        write_key(&path, &original, 0o600);
        write_key(&replacement, &public, 0o644);
        let opened = std::fs::File::open(&path).unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        let loaded = FingerprintKey::from_open_file(&path, opened).unwrap();
        assert_eq!(
            loaded.id(),
            FingerprintKey::parse(&original, "fixture").unwrap().id()
        );
        assert_ne!(
            loaded.id(),
            FingerprintKey::parse(&public, "fixture").unwrap().id()
        );
        assert!(matches!(
            FingerprintKey::from_file(&path),
            Err(KeyError::OpenFile { mode: 0o644, .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_private_replacement_cannot_bless_an_opened_public_key() {
        let dir = KeyDirectory::new();
        let path = dir.0.join("key");
        let replacement = dir.0.join("replacement");
        let public = STANDARD.encode([3_u8; MIN_KEY_BYTES]);
        write_key(&path, &public, 0o644);
        write_key(&replacement, &FingerprintKey::generate(), 0o600);
        let opened = std::fs::File::open(&path).unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        let error = FingerprintKey::from_open_file(&path, opened).unwrap_err();
        assert!(matches!(&error, KeyError::OpenFile { mode: 0o644, .. }));
        assert!(!error.to_string().contains(&public));
        assert!(FingerprintKey::from_file(&path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn only_a_regular_owner_only_file_can_supply_the_key() {
        let dir = KeyDirectory::new();
        let path = dir.0.join("key");
        let alias = dir.0.join("alias");
        let text = FingerprintKey::generate();
        write_key(&path, &text, 0o600);
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert_eq!(
            FingerprintKey::from_file(&alias).unwrap().id(),
            FingerprintKey::parse(&text, "fixture").unwrap().id()
        );
        assert!(matches!(
            FingerprintKey::from_file(&dir.0),
            Err(KeyError::UnreadableFile { reason, .. }) if reason.contains("directory")
        ));
        // Following a symlink does not waive the target handle's permission check.
        write_key(&path, &text, 0o644);
        assert!(matches!(
            FingerprintKey::from_file(&alias),
            Err(KeyError::OpenFile { mode: 0o644, .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_key_is_refused_without_waiting_for_a_writer() {
        const CHILD_PATH: &str = "PBPS_TEST_956_FIFO_KEY_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let error = FingerprintKey::from_file(Path::new(&path)).unwrap_err();
            assert!(
                matches!(&error, KeyError::UnreadableFile { reason, .. } if reason.contains("FIFO")),
                "{error}"
            );
            return;
        }

        let dir = KeyDirectory::new();
        let path = dir.0.join("key.fifo");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        // Bound even an opener that blocks: a timeout thread cannot cancel open(2).
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fingerprint::tests::a_fifo_key_is_refused_without_waiting_for_a_writer",
                "--nocapture",
            ])
            .env(CHILD_PATH, &path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let timed_out = loop {
            if child.try_wait().unwrap().is_some() {
                break false;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                break true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let output = child.wait_with_output().unwrap();
        assert!(
            !timed_out,
            "key loading waited for a FIFO writer instead of refusing its type"
        );
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn a_key_file_is_refused_where_owner_only_cannot_be_proved() {
        let path = std::env::temp_dir().join("pbps-fp-unverifiable");
        assert!(matches!(
            FingerprintKey::from_file(&path),
            Err(KeyError::UnverifiableFile(_))
        ));
    }

    #[test]
    fn the_process_key_is_one_key() {
        assert_eq!(
            FingerprintKey::process().id(),
            FingerprintKey::process().id()
        );
    }
}
