//! Shared wire diagnostics; callers retain their validation and attempt gates.

use std::path::Path;

use anyhow::Context as _;
use pbps_model::SavedPlan;

pub(crate) fn decode(raw: &str, path: &Path) -> anyhow::Result<SavedPlan> {
    match serde_json::from_str(raw) {
        Ok(plan) => Ok(plan),
        Err(error) => {
            // Versions 1 through 12 share this envelope. Require it before
            // calling a failed decode an unsupported plan: an ids file also
            // has a version. Leave change payloads opaque so a future enum
            // variant cannot hide a future plan's version. This only selects
            // a refusal; it never fills in missing current evidence (DEC-614.1).
            // Direct decoding above retains apply's checksum/attempt boundary.
            #[allow(dead_code)]
            #[derive(serde::Deserialize)]
            struct Header {
                version: u32,
                origin: String,
                dialect: String,
                created_at: String,
                baseline: Baseline,
                changes: Changes,
            }
            #[allow(dead_code)]
            #[derive(serde::Deserialize)]
            struct Baseline {
                description: String,
                checksum: String,
            }
            #[allow(dead_code)]
            #[derive(serde::Deserialize)]
            struct Changes {
                changes: Vec<serde::de::IgnoredAny>,
            }
            if let Ok(header) = serde_json::from_str::<Header>(raw) {
                require_current(header.version, path)?;
            }
            Err(anyhow::Error::new(Echoed(error.to_string())))
                .with_context(|| format!("`{}` is not a pbps plan", path.display()))
        }
    }
}

/// The decoder's own sentence about a file it could not read.
///
/// serde repeats the text it stopped at — an unknown variant, an unknown field,
/// a name a type refused — and that text is the file's. The sentence is one line
/// by construction, so every control character in it came from the artifact and
/// is shown escaped, like the other values a refusal takes from a plan
/// (DEC-2032.1), and so is its backslash: serde repeats some of that text raw
/// and some behind `Debug` escapes, and only a doubled backslash tells a line
/// break from the two characters `\n` in both (#2038). Held as text rather than
/// as `serde_json::Error` because nothing downcasts the cause, and the error's
/// `Display` is the only thing printed.
#[derive(Debug, thiserror::Error)]
#[error("{}", pbps_model::escape::Escaped(.0))]
struct Echoed(String);

pub(crate) fn require_current(version: u32, path: &Path) -> anyhow::Result<()> {
    if version != pbps_model::plan::CURRENT_VERSION {
        anyhow::bail!(
            "`{}` is a version {} plan and this tool understands version {}.\n\
             Recompute it with `pbps plan --db ... --out ...` and review the new artifact for approval.",
            path.display(),
            version,
            pbps_model::plan::CURRENT_VERSION
        );
    }
    Ok(())
}
