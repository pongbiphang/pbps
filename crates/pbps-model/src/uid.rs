//! Identity anchors.
//!
//! A UID is the only thing the tool uses internally to answer "is this the same
//! column?". **Users never type one and never see one** — it appears only in the
//! identity file and in `__pbps_state` (see SPEC §5.2).
//!
//! They are random rather than sequential so that two branches adding a column
//! each cannot be handed the same number.

use std::fmt;
use std::str::FromStr;

/// The easily misread `i`, `l`, `o` and `u` are dropped, leaving 32 characters.
/// Nobody types a UID by hand, but UIDs do show up in error messages and git
/// diffs, so legibility is still worth something.
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Length of the random part. 32^6 ≈ 1.07e9; for the number of columns in a
/// single project (10^3 to 10^4) the birthday-problem estimate puts the
/// collision probability below 10^-3, and allocation checks existing UIDs
/// anyway, so a collision only causes a redraw, never a wrong identity.
const LEN: usize = 6;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UidError {
    #[error("UID `{0}` is missing the `t_` or `c_` prefix")]
    BadPrefix(String),

    #[error("UID `{0}` should be {LEN} characters long, excluding the prefix")]
    BadLength(String),

    #[error("UID `{0}` contains a character outside the alphabet")]
    BadChar(String),
}

/// The kind of object a UID points at. The prefix makes it obvious at a glance
/// whether a line in the identity file is about a table or a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UidKind {
    Table,
    Column,
}

impl UidKind {
    pub const fn prefix(self) -> &'static str {
        match self {
            UidKind::Table => "t_",
            UidKind::Column => "c_",
        }
    }
}

/// Of the form `c_k7x2mq` / `t_a9k2mq`.
///
/// `Ord` is plain string order, so UIDs of the same kind sort together and the
/// identity file's diff reads better.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct Uid(String);

impl Uid {
    /// Allocates a new UID.
    ///
    /// The caller is responsible for checking the result against existing UIDs.
    /// A collision is very rare but not impossible, and silently reusing an
    /// identity would directly produce a wrong rename decision.
    pub fn generate(kind: UidKind) -> Self {
        let mut s = String::with_capacity(2 + LEN);
        s.push_str(kind.prefix());
        for _ in 0..LEN {
            let i = (next_random() % ALPHABET.len() as u64) as usize;
            s.push(ALPHABET[i] as char);
        }
        Self(s)
    }

    /// A UID derived from a name rather than drawn at random.
    ///
    /// **This is not for minting identities.** Real UIDs must be random, or two
    /// branches adding a same-named column would be handed the same one and a
    /// merge would silently fuse two different columns into one.
    ///
    /// It exists for objects that are *observed* rather than declared: a column
    /// somebody added to a database by hand has no recorded identity, and the
    /// drift comparison still has to be able to talk about it. Deriving it from
    /// the name keeps a drift report byte-identical across runs, which a random
    /// UID would not — and a report whose payload changes every time cannot be
    /// deduplicated by whatever the `on_drift` hook feeds.
    ///
    /// `salt` lets a caller step past a collision with an identity that already
    /// exists; see `pbps_diff::observed_ids`.
    pub fn derived(kind: UidKind, seed: &str, salt: u32) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(kind.prefix().as_bytes());
        hasher.update(seed.as_bytes());
        hasher.update(salt.to_be_bytes());
        let digest = hasher.finalize();

        let mut s = String::with_capacity(2 + LEN);
        s.push_str(kind.prefix());
        for byte in digest.iter().take(LEN) {
            s.push(ALPHABET[(*byte as usize) % ALPHABET.len()] as char);
        }
        Self(s)
    }

    pub fn kind(&self) -> UidKind {
        if self.0.starts_with("t_") {
            UidKind::Table
        } else {
            UidKind::Column
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Uid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Uid {
    type Err = UidError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s
            .strip_prefix("t_")
            .or_else(|| s.strip_prefix("c_"))
            .ok_or_else(|| UidError::BadPrefix(s.to_owned()))?;

        if rest.len() != LEN {
            return Err(UidError::BadLength(s.to_owned()));
        }
        if !rest.bytes().all(|b| ALPHABET.contains(&b)) {
            return Err(UidError::BadChar(s.to_owned()));
        }
        Ok(Self(s.to_owned()))
    }
}

impl TryFrom<String> for Uid {
    type Error = UidError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Uid> for String {
    fn from(v: Uid) -> String {
        v.0
    }
}

/// The source of randomness for UIDs.
///
/// **`rand` is deliberately not used.** A UID is an identity marker, not a
/// secret — guessing someone else's UID buys nothing — so cryptographic quality
/// is not required, only that two branches never draw the same number. Pulling
/// in a dependency for that (along with `getrandom` beneath it and its platform
/// FFI) is a bad trade: this tool gets audited in regulated environments, and a
/// shorter dependency tree is worth more.
///
/// The seed comes from `RandomState` (OS-provided, different per process) and
/// the wall clock, then advances with SplitMix64.
fn next_random() -> u64 {
    use std::cell::Cell;
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }

    STATE.with(|st| {
        let mut x = st.get();
        if x == 0 {
            let mut h = RandomState::new().build_hasher();
            h.write_u64(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0),
            );
            // Each RandomState instance has a different key, so the hash
            // carries OS entropy.
            x = h.finish() | 1;
        }
        // SplitMix64
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        st.set(x);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// A drift report is fed to a hook; a payload that changes on every run
    /// cannot be deduplicated by whatever the hook talks to.
    #[test]
    fn derived_uids_are_stable_and_well_formed() {
        let a = Uid::derived(UidKind::Column, "dbo.customer.nickname", 0);
        let b = Uid::derived(UidKind::Column, "dbo.customer.nickname", 0);
        assert_eq!(a, b);
        assert_eq!(a.kind(), UidKind::Column);
        assert_eq!(a.as_str().parse::<Uid>().unwrap(), a);
    }

    #[test]
    fn derived_uids_separate_names_kinds_and_salts() {
        let base = Uid::derived(UidKind::Column, "dbo.customer.nickname", 0);
        assert_ne!(base, Uid::derived(UidKind::Column, "dbo.customer.note", 0));
        assert_ne!(
            base,
            Uid::derived(UidKind::Column, "dbo.customer.nickname", 1)
        );
        assert_ne!(
            Uid::derived(UidKind::Table, "dbo.customer", 0).to_string(),
            Uid::derived(UidKind::Column, "dbo.customer", 0).to_string()
        );
    }

    /// Derivation must never be mistaken for allocation: two branches adding a
    /// same-named column would otherwise be handed one identity and a clean
    /// merge would fuse two different columns.
    #[test]
    fn generation_is_not_derivation() {
        let derived = Uid::derived(UidKind::Column, "dbo.customer.email", 0);
        let mut drawn = BTreeSet::new();
        for _ in 0..50 {
            drawn.insert(Uid::generate(UidKind::Column));
        }
        assert!(drawn.len() > 40, "generation must not be deterministic");
        assert!(!drawn.contains(&derived));
    }

    #[test]
    fn generated_uids_round_trip() {
        for kind in [UidKind::Table, UidKind::Column] {
            let u = Uid::generate(kind);
            assert_eq!(u.kind(), kind);
            assert_eq!(u.as_str().parse::<Uid>().unwrap(), u);
        }
    }

    #[test]
    fn generated_uids_avoid_ambiguous_letters() {
        for _ in 0..500 {
            let u = Uid::generate(UidKind::Column);
            let body = &u.as_str()[2..];
            assert!(
                !body.contains(['i', 'l', 'o', 'u']),
                "the alphabet must not emit easily misread characters: {u}"
            );
        }
    }

    /// Not a demand for cryptographic quality, just a check that this was not
    /// written as a constant.
    #[test]
    fn generation_is_not_constant() {
        let set: BTreeSet<_> = (0..200).map(|_| Uid::generate(UidKind::Column)).collect();
        assert!(
            set.len() > 190,
            "randomness is clearly inadequate: 200 draws produced only {} distinct values",
            set.len()
        );
    }

    #[test]
    fn malformed_uids_are_rejected() {
        assert!("k7x2mq".parse::<Uid>().is_err(), "missing prefix");
        assert!("c_k7x2m".parse::<Uid>().is_err(), "too short");
        assert!("c_k7x2mqq".parse::<Uid>().is_err(), "too long");
        assert!(
            "c_k7x2mi".parse::<Uid>().is_err(),
            "contains i, outside the alphabet"
        );
        assert!(
            "c_K7X2MQ".parse::<Uid>().is_err(),
            "uppercase is not in the alphabet"
        );
    }
}
