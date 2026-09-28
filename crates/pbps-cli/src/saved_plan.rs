//! Shared wire diagnostics; callers retain their validation and attempt gates.

use std::path::Path;

use anyhow::Context as _;
use pbps_model::SavedPlan;

pub(crate) fn decode(raw: &str, path: &Path) -> anyhow::Result<SavedPlan> {
    match serde_json::from_str(raw) {
        Ok(plan) => Ok(plan),
        Err(error) => {
            // An older shape may lack required fields, and a newer one may
            // carry unknown variants. Read only its version for a refusal,
            // never to fill in missing current evidence (DEC-614.1). Keep the
            // successful decode path intact so apply still reports identified
            // artifacts through its existing checksum/attempt-hook boundary.
            #[derive(serde::Deserialize)]
            struct Header {
                version: u32,
            }
            if let Ok(header) = serde_json::from_str::<Header>(raw) {
                require_current(header.version, path)?;
            }
            Err(error).with_context(|| format!("`{}` is not a pbps plan", path.display()))
        }
    }
}

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
