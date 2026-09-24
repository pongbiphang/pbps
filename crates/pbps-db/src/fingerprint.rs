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
        let text = std::env::var(name).map_err(|_| KeyError::MissingVariable(name.to_owned()))?;
        Self::parse(&text, &format!("`{name}`"))
    }

    /// An environment's key from the file at `path`, which must not be
    /// readable by group or others: a key in a world-readable file is a key
    /// everyone on the host holds.
    pub fn from_file(path: &Path) -> Result<Self, KeyError> {
        let unreadable = |e: std::io::Error| KeyError::UnreadableFile {
            path: path.to_owned(),
            reason: e.to_string(),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .map_err(unreadable)?
                .permissions()
                .mode()
                & 0o777;
            if mode & 0o077 != 0 {
                return Err(KeyError::OpenFile {
                    path: path.to_owned(),
                    mode,
                });
            }
        }
        let text = std::fs::read_to_string(path).map_err(unreadable)?;
        Self::parse(&text, &format!("`{}`", path.display()))
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

    #[test]
    fn the_process_key_is_one_key() {
        assert_eq!(
            FingerprintKey::process().id(),
            FingerprintKey::process().id()
        );
    }
}
