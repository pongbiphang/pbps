//! The `pbps test` manifest (ADR-0019 §2): a closed YAML mapping, parsed and
//! checked without touching the filesystem or git.
//!
//! Everything that needs either — a pin naming a real commit, a path staying
//! inside the project, a file being readable — is the caller's, because only
//! the caller knows the project root and the repository. This module answers
//! the questions the text alone can answer, and answers all of them in one
//! pass: a manifest with three mistakes reports three.

use std::num::NonZeroU32;
use std::path::{Component, Path};

use pbps_model::escape::Escaped;

use crate::DialectName;

/// The only manifest version this build reads.
pub const SUPPORTED_VERSION: u32 = 1;

/// Where the manifest is read from when none is named, relative to the
/// project root.
pub const DEFAULT_MANIFEST: &str = "tests/pbps-tests.yml";

/// The candidate spelling that selects the working tree. A branch of that
/// name is selected by its full commit id instead (DEC-1165.4).
pub const WORKING_TREE: &str = "working-tree";

/// A parsed, text-checked manifest. Paths are still the manifest's spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestManifest {
    pub dialect: DialectName,
    pub base: CommitId,
    pub candidate: Candidate,
    pub image: ImageRef,
    pub fixtures: Vec<Fixture>,
    pub assertions: Vec<Assertion>,
    pub timeouts: Timeouts,
}

/// A full object id. A branch, a tag or an abbreviation names whatever the
/// repository calls it today; a pin has to keep naming one commit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitId(String);

impl CommitId {
    /// SHA-1 (40) or SHA-256 (64) hexadecimal. Stored lowercase, which is how
    /// git prints it, so equal ids compare equal however they were typed.
    pub fn parse(text: &str) -> Option<Self> {
        (matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| Self(text.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CommitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Candidate {
    /// The current candidate inputs, read as `plan` reads them.
    WorkingTree,
    /// A git revision such as `HEAD`, resolved by the caller.
    Revision(String),
}

/// `name@sha256:<64 hex>`. A tag cannot be part of it, even beside a digest:
/// the digest alone decides the image, and a tag that disagrees with it is a
/// second claim about which image is meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    name: String,
    digest: String,
}

impl ImageRef {
    fn parse(text: &str) -> Result<Self, &'static str> {
        let (name, digest) = text
            .split_once('@')
            .ok_or("it has no `@sha256:<digest>`; a tag can move, so name the image by digest")?;
        let hex = digest
            .strip_prefix("sha256:")
            .ok_or("the digest must be `sha256:` followed by 64 hexadecimal digits")?;
        if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err("the digest must be `sha256:` followed by 64 lowercase hexadecimal digits");
        }
        let last = name.rsplit('/').next().unwrap_or(name);
        if name.is_empty()
            || name.starts_with(['-', '/'])
            || name.contains('@')
            || name.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err("the repository name is empty or contains characters no image name has");
        }
        if last.contains(':') {
            return Err(
                "it carries a tag as well as a digest; remove the tag, the digest alone names the image",
            );
        }
        Ok(Self {
            name: name.to_owned(),
            digest: digest.to_owned(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `sha256:<hex>`.
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

impl std::fmt::Display for ImageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    pub name: String,
    pub file: String,
    pub at: CommitId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assertion {
    pub name: String,
    pub file: String,
}

/// Positive millisecond bounds (ADR-0019 §2). Every one is always present, so
/// a consumer never has to know the defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Timeouts {
    pub startup_ms: NonZeroU32,
    pub step_ms: NonZeroU32,
    pub fixture_ms: NonZeroU32,
    pub assertion_ms: NonZeroU32,
    pub run_ms: NonZeroU32,
    pub cleanup_ms: NonZeroU32,
}

impl Default for Timeouts {
    fn default() -> Self {
        let ms = |n| NonZeroU32::new(n).expect("a nonzero default");
        Self {
            startup_ms: ms(120_000),
            step_ms: ms(120_000),
            fixture_ms: ms(30_000),
            assertion_ms: ms(10_000),
            run_ms: ms(600_000),
            cleanup_ms: ms(30_000),
        }
    }
}

/// Which list an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Entries {
    Fixtures,
    Assertions,
}

impl std::fmt::Display for Entries {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Entries::Fixtures => "fixtures",
            Entries::Assertions => "assertions",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    /// The text is not a mapping this version reads: a syntax error, a
    /// duplicate or unknown key, a missing required key or a wrong type.
    /// The parser's own message carries the position.
    #[error("{0}")]
    Syntax(String),

    #[error("the manifest has no `version`; this build reads version {SUPPORTED_VERSION}")]
    MissingVersion,

    /// Reported before anything else is judged, so a manifest from a newer
    /// version is refused for its version rather than for the first key this
    /// build does not know.
    #[error("manifest version {0} is not supported; this build reads version {SUPPORTED_VERSION}")]
    UnsupportedVersion(String),

    #[error(
        "`{field}` is `{}`, which is not a full commit id; name the 40- or 64-digit hexadecimal \
         id, since a branch, tag or abbreviation can come to mean another commit",
        Escaped(value)
    )]
    InvalidCommit { field: String, value: String },

    #[error("`candidate` is `{}`: {reason}", Escaped(value))]
    InvalidCandidate { value: String, reason: &'static str },

    #[error("`engine.image` is `{}`: {reason}", Escaped(value))]
    InvalidImage { value: String, reason: &'static str },

    #[error("{list} entry {index} has an empty `{field}`")]
    EmptyField {
        list: Entries,
        index: usize,
        field: &'static str,
    },

    #[error(
        "{list} entries {first} and {second} are both named `{}`",
        Escaped(name)
    )]
    DuplicateName {
        list: Entries,
        name: String,
        first: usize,
        second: usize,
    },

    #[error(
        "{list} entry `{}` names `{}`, which is not relative to the manifest's directory",
        Escaped(name),
        Escaped(path)
    )]
    NotRelative {
        list: Entries,
        name: String,
        path: String,
    },

    #[error("`assertions` is empty; a test with nothing to assert proves nothing")]
    NoAssertions,

    #[error(
        "`timeouts.{field}` is {value}; it must be between 1 and {} milliseconds",
        u32::MAX
    )]
    TimeoutOutOfRange { field: &'static str, value: u64 },
}

#[derive(serde::Deserialize)]
struct Probe {
    version: Option<Version>,
}

/// Anything at all, so that its being wrong is a version finding rather than
/// a parse error about a key the strict form never gets to read.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum Version {
    Number(u64),
    Other(serde::de::IgnoredAny),
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    /// Judged by the probe; kept so the strict form still names the key.
    #[serde(rename = "version")]
    _version: u64,
    dialect: DialectName,
    base: String,
    candidate: Option<String>,
    engine: RawEngine,
    fixtures: Vec<RawFixture>,
    assertions: Vec<RawAssertion>,
    #[serde(default)]
    timeouts: RawTimeouts,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEngine {
    image: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFixture {
    name: String,
    file: String,
    at: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAssertion {
    name: String,
    file: String,
}

/// Wide integers, so that 0 and 2^32 are reported as out of range with the
/// field's name rather than as a type error about some other number.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawTimeouts {
    startup_ms: Option<u64>,
    step_ms: Option<u64>,
    fixture_ms: Option<u64>,
    assertion_ms: Option<u64>,
    run_ms: Option<u64>,
    cleanup_ms: Option<u64>,
}

impl TestManifest {
    /// Parses `text` (already decoded as UTF-8 by the caller, which owns the
    /// "unreadable" case). Every problem found is returned, not the first.
    pub fn parse(text: &str) -> Result<Self, Vec<ManifestError>> {
        let probe: Probe =
            serde_saphyr::from_str(text).map_err(|e| vec![ManifestError::Syntax(e.to_string())])?;
        match probe.version {
            None => return Err(vec![ManifestError::MissingVersion]),
            Some(Version::Number(n)) if n == u64::from(SUPPORTED_VERSION) => {}
            Some(Version::Number(n)) => {
                return Err(vec![ManifestError::UnsupportedVersion(n.to_string())]);
            }
            Some(Version::Other(_)) => {
                return Err(vec![ManifestError::UnsupportedVersion(
                    "that is not an integer".to_owned(),
                )]);
            }
        }
        let raw: Raw =
            serde_saphyr::from_str(text).map_err(|e| vec![ManifestError::Syntax(e.to_string())])?;
        Self::check(raw)
    }

    fn check(raw: Raw) -> Result<Self, Vec<ManifestError>> {
        let mut problems = Vec::new();
        let mut commit = |field: String, value: &str| {
            CommitId::parse(value).or_else(|| {
                problems.push(ManifestError::InvalidCommit {
                    field,
                    value: value.to_owned(),
                });
                None
            })
        };
        let base = commit("base".to_owned(), &raw.base);
        let fixtures: Vec<_> = raw
            .fixtures
            .iter()
            .enumerate()
            .map(|(i, f)| commit(format!("fixtures[{i}].at"), &f.at))
            .collect();

        let candidate = match raw.candidate.as_deref() {
            None | Some(WORKING_TREE) => Some(Candidate::WorkingTree),
            Some(rev) => match revision_problem(rev) {
                None => Some(Candidate::Revision(rev.to_owned())),
                Some(reason) => {
                    problems.push(ManifestError::InvalidCandidate {
                        value: rev.to_owned(),
                        reason,
                    });
                    None
                }
            },
        };
        let image = match ImageRef::parse(&raw.engine.image) {
            Ok(image) => Some(image),
            Err(reason) => {
                problems.push(ManifestError::InvalidImage {
                    value: raw.engine.image.clone(),
                    reason,
                });
                None
            }
        };

        check_entries(
            Entries::Fixtures,
            raw.fixtures.iter().map(|f| (&f.name, &f.file)),
            &mut problems,
        );
        check_entries(
            Entries::Assertions,
            raw.assertions.iter().map(|a| (&a.name, &a.file)),
            &mut problems,
        );
        if raw.assertions.is_empty() {
            problems.push(ManifestError::NoAssertions);
        }

        let defaults = Timeouts::default();
        let mut timeout = |field, value: Option<u64>, default: NonZeroU32| match value {
            None => default,
            Some(v) => match u32::try_from(v).ok().and_then(NonZeroU32::new) {
                Some(ms) => ms,
                None => {
                    problems.push(ManifestError::TimeoutOutOfRange { field, value: v });
                    default
                }
            },
        };
        let t = &raw.timeouts;
        let timeouts = Timeouts {
            startup_ms: timeout("startup_ms", t.startup_ms, defaults.startup_ms),
            step_ms: timeout("step_ms", t.step_ms, defaults.step_ms),
            fixture_ms: timeout("fixture_ms", t.fixture_ms, defaults.fixture_ms),
            assertion_ms: timeout("assertion_ms", t.assertion_ms, defaults.assertion_ms),
            run_ms: timeout("run_ms", t.run_ms, defaults.run_ms),
            cleanup_ms: timeout("cleanup_ms", t.cleanup_ms, defaults.cleanup_ms),
        };

        if !problems.is_empty() {
            return Err(problems);
        }
        // Every `None` above pushed a problem, so none is left here.
        let (Some(base), Some(candidate), Some(image)) = (base, candidate, image) else {
            unreachable!("a rejected field is reported");
        };
        Ok(Self {
            dialect: raw.dialect,
            base,
            candidate,
            image,
            fixtures: raw
                .fixtures
                .into_iter()
                .zip(fixtures)
                .map(|(f, at)| Fixture {
                    name: f.name,
                    file: f.file,
                    at: at.expect("a rejected pin is reported"),
                })
                .collect(),
            assertions: raw
                .assertions
                .into_iter()
                .map(|a| Assertion {
                    name: a.name,
                    file: a.file,
                })
                .collect(),
            timeouts,
        })
    }
}

/// Why `rev` cannot be handed to git as a revision, if it cannot. It is
/// resolved by the caller, and git alone judges the expression: `HEAD^{/some
/// text}` is a revision with spaces in it. This only keeps it from being read
/// as an option or from spanning lines in a report.
fn revision_problem(rev: &str) -> Option<&'static str> {
    if rev.is_empty() {
        Some("name a revision, or leave `candidate` out to test the working tree")
    } else if rev.starts_with('-') {
        Some("a revision cannot start with `-`; git would read it as an option")
    } else if rev.chars().any(char::is_control) {
        Some("a revision has no control characters")
    } else {
        None
    }
}

/// Names nonempty and unique, paths nonempty and relative. Whether a path
/// stays inside the project, or two spellings are one file, needs the
/// filesystem and is the caller's.
fn check_entries<'a>(
    list: Entries,
    entries: impl Iterator<Item = (&'a String, &'a String)>,
    problems: &mut Vec<ManifestError>,
) {
    let mut seen: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (index, (name, file)) in entries.enumerate() {
        if name.is_empty() {
            problems.push(ManifestError::EmptyField {
                list,
                index,
                field: "name",
            });
        } else if let Some(first) = seen.insert(name, index) {
            problems.push(ManifestError::DuplicateName {
                list,
                name: name.clone(),
                first,
                second: index,
            });
        }
        if file.is_empty() {
            problems.push(ManifestError::EmptyField {
                list,
                index,
                field: "file",
            });
        } else if file.contains('\0')
            || Path::new(file)
                .components()
                .any(|c| matches!(c, Component::RootDir | Component::Prefix(_)))
        {
            problems.push(ManifestError::NotRelative {
                list,
                name: name.clone(),
                path: file.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests;
