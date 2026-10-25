//! What `explain` shows of a plan's sealed rehearsal record (#2379).

use pbps_model::resolver::{ScratchLayout, ScratchSource};
use pbps_model::{Rehearsal, Text};

/// The sealed rehearsal record, flattened for `--format json`.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct RehearsalDetail {
    /// `converged`: the plan's statements ran on a scratch engine and it
    /// matched the declarations. `off`: the operator turned the rehearsal off,
    /// so the plan was not rehearsed.
    pub outcome: &'static str,
    /// The `pbps.yml` resolver entry that supplied the scratch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<Text>,
    /// `server` or `docker`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scratch: Option<&'static str>,
    /// A docker scratch's image reference as configured: unverified, since
    /// the content behind a tag can change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<Text>,
    /// `run-owned` or `supplied`, as for the resolver evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<&'static str>,
    /// Differences that are only the engine's own spelling, as it stored them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spelling: Vec<Text>,
}

pub fn detail(record: &Rehearsal) -> RehearsalDetail {
    match record {
        Rehearsal::Off => RehearsalDetail {
            outcome: "off",
            entry: None,
            scratch: None,
            image: None,
            layout: None,
            spelling: Vec::new(),
        },
        Rehearsal::Converged {
            entry,
            scratch,
            layout,
            spelling,
        } => {
            let (scratch, image) = match scratch {
                ScratchSource::Server {} => ("server", None),
                ScratchSource::Docker { image } => ("docker", Some(image.clone())),
            };
            RehearsalDetail {
                outcome: "converged",
                entry: Some(entry.clone()),
                scratch: Some(scratch),
                image,
                layout: Some(match layout {
                    ScratchLayout::RunOwned => "run-owned",
                    ScratchLayout::Supplied => "supplied",
                }),
                spelling: spelling.clone(),
            }
        }
    }
}

/// The one line in the plan's summary.
pub fn line(detail: &RehearsalDetail) -> String {
    let (Some(entry), Some(scratch), Some(layout)) = (&detail.entry, detail.scratch, detail.layout)
    else {
        return "off (this plan was not rehearsed)".to_owned();
    };
    let mut out = format!(
        "converged on {} ({scratch} scratch, {layout})",
        entry.shown()
    );
    if let Some(image) = &detail.image {
        out.push_str(&format!(
            "; image {}, as configured, not verified",
            image.shown()
        ));
    }
    match detail.spelling.len() {
        0 => {}
        1 => out.push_str("; 1 spelling difference"),
        n => out.push_str(&format!("; {n} spelling differences")),
    }
    out
}

/// The spellings, after the plan's summary: they are what the engine stored,
/// which no offline normalization can produce.
pub fn render(detail: &RehearsalDetail) -> String {
    if detail.spelling.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nRehearsal\n");
    out.push_str(
        "  These differ only in how the engine spells them. The plan converged on them:\n",
    );
    for spelling in &detail.spelling {
        out.push_str(&format!("    {}\n", spelling.shown()));
    }
    out
}
