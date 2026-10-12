//! The sealed resolver evidence of a resolved plan, as a reviewer reads it
//! (#1517). Everything here is read from the plan file: no project, no
//! credentials and no resolver. A fingerprint's presence is shown, never its
//! value, and a surface's verdict is what its sealed bindings say.

use pbps_model::resolver::{
    ObjectIdentity, OrderReason, OutsideWrites, ResolverEvidence, ResolverRuntime, ScratchLayout,
    ScratchSource, Surface, SurfaceResolution,
};

/// What `explain` shows of a resolved plan's evidence.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct ResolverDetail {
    /// The `pbps.yml` resolver entry that ran.
    pub entry: String,
    /// `server` or `docker`.
    pub scratch: &'static str,
    /// A docker scratch's image reference as configured: unverified, since
    /// the content behind a tag can change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// `run-owned`: a login and database the run created and dropped.
    /// `supplied`: the database the entry names, emptied after the run.
    pub layout: &'static str,
    /// Whether the baseline's writes outside the scratch database were
    /// `refused` or `allowed` (and then not checked).
    pub outside_writes: &'static str,
    /// The compatibility rule the scratch was qualified under.
    pub rule: String,
    /// The identifier of the fingerprint key the evidence is keyed with.
    pub key_id: String,
    /// The facts the qualification seals as keyed fingerprints: named,
    /// never their values.
    pub sealed: Vec<&'static str>,
    /// Differences the rule accepted rather than refused (#1700).
    pub limitations: Vec<Limitation>,
    /// Each managed surface the resolver answered for, with its verdict.
    pub surfaces: Vec<SurfaceVerdict>,
    /// The order the evidence proves, edge by edge.
    pub order: Vec<OrderStep>,
    /// The external inputs the bindings read, by identity.
    pub prerequisites: Vec<PrerequisiteDetail>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct Limitation {
    pub name: String,
    pub description: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SurfaceVerdict {
    /// Its kind and name, such as `default app.t.c`.
    pub surface: String,
    /// `unaffected`, `rebuilt`, `created` or `removed`.
    pub verdict: &'static str,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct OrderStep {
    /// The 1-based position, in the plan's changes, of the change that goes
    /// first.
    pub before: usize,
    /// The position of the change that must follow it.
    pub after: usize,
    /// Why: `structural`, `binding`, `identity`, `data`, `authorization` or
    /// `restoration`.
    pub reason: &'static str,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct PrerequisiteDetail {
    /// The catalog class and name, such as `pg_proc app.f(numeric)`.
    pub object: String,
    /// Whether a keyed fingerprint of its properties is sealed. One the plan
    /// itself installs has none.
    pub fingerprinted: bool,
}

pub fn detail(evidence: &ResolverEvidence) -> ResolverDetail {
    let q = evidence.qualification();
    let ResolverRuntime::Scratch {
        entry,
        scratch,
        layout,
        outside_writes,
    } = &q.runtime;
    let (scratch, image) = match scratch {
        ScratchSource::Server {} => ("server", None),
        ScratchSource::Docker { image } => ("docker", Some(image.clone())),
    };
    let mut limitations: Vec<_> = q
        .limitations
        .iter()
        .map(|(name, description)| Limitation {
            name: name.clone(),
            description: description.clone(),
        })
        .collect();
    limitations.sort_by(|a, b| a.name.cmp(&b.name));
    ResolverDetail {
        entry: entry.clone(),
        scratch,
        image,
        layout: match layout {
            ScratchLayout::RunOwned => "run-owned",
            ScratchLayout::Supplied => "supplied",
        },
        outside_writes: match outside_writes {
            OutsideWrites::Refused => "refused",
            OutsideWrites::Allowed => "allowed",
        },
        rule: q.rule.clone(),
        key_id: evidence.before().key_id().to_owned(),
        sealed: vec![
            "target environment",
            "target environment after the plan's grants",
            "resolver environment",
            "target build",
            "resolver build",
            "channels",
            "target cluster identity",
        ],
        limitations,
        surfaces: evidence.surfaces().iter().map(surface_verdict).collect(),
        order: evidence
            .ordering()
            .edges()
            .iter()
            .map(|edge| OrderStep {
                before: edge.before + 1,
                after: edge.after + 1,
                reason: reason(edge.reason),
            })
            .collect(),
        prerequisites: evidence
            .before()
            .prerequisites()
            .iter()
            .map(|p| PrerequisiteDetail {
                object: identity(&p.object),
                fingerprinted: p.fingerprinted(),
            })
            .collect(),
    }
}

/// The verdict the sealed sides say: a surface only the desired side holds
/// is created, one only the current side holds is removed, and one both
/// hold is rebuilt exactly when its bindings differ.
fn surface_verdict(resolution: &SurfaceResolution) -> SurfaceVerdict {
    let verdict = match (&resolution.current, &resolution.desired) {
        (None, Some(_)) => "created",
        (Some(_), None) => "removed",
        (Some(current), Some(desired)) if current.bindings == desired.bindings => "unaffected",
        (Some(_), Some(_)) => "rebuilt",
        // `validate` refuses a surface neither side holds; named if one
        // were ever read.
        (None, None) => "absent",
    };
    SurfaceVerdict {
        surface: surface(&resolution.surface),
        verdict,
    }
}

fn surface(surface: &Surface) -> String {
    match surface {
        Surface::Namespace(name) => format!("schema {name}"),
        Surface::Table(table) => format!("table {table}"),
        Surface::Column(column) => format!("column {column}"),
        Surface::Default(column) => format!("default {column}"),
        Surface::Check { table, name } => format!("check {table}.{name}"),
        Surface::Index { table, name } => format!("index {table}.{name}"),
        Surface::Module(module) => format!("module {module}"),
    }
}

fn reason(reason: OrderReason) -> &'static str {
    match reason {
        OrderReason::Structural => "structural",
        OrderReason::Binding => "binding",
        OrderReason::Identity => "identity",
        OrderReason::Data => "data",
        OrderReason::Authorization => "authorization",
        OrderReason::Restoration => "restoration",
    }
}

fn identity(object: &ObjectIdentity) -> String {
    let mut out = format!("{} {}", object.class, object.name.join("."));
    if !object.signature.is_empty() {
        let arguments: Vec<String> = object
            .signature
            .iter()
            .map(|argument| argument.name.join("."))
            .collect();
        out.push_str(&format!("({})", arguments.join(", ")));
    }
    out
}

/// The one line `explain` prints for the resolver: `<entry> (<kind>
/// scratch, <layout>)`, with an unverified image and an allowed outside
/// write said after it.
pub fn line(detail: &ResolverDetail) -> String {
    let mut out = format!(
        "{} ({} scratch, {})",
        detail.entry, detail.scratch, detail.layout
    );
    if let Some(image) = &detail.image {
        out.push_str(&format!("; image {image}, as configured, not verified"));
    }
    if detail.outside_writes == "allowed" {
        out.push_str(
            "; the baseline's writes outside the scratch database were allowed, not checked",
        );
    }
    out
}

/// The evidence section of the human output, after the plan's summary.
pub fn render(detail: &ResolverDetail, changes: &pbps_model::ChangeSet) -> String {
    let mut out = String::from("\nResolver evidence\n");
    out.push_str(&format!("  rule        {}\n", detail.rule));
    out.push_str(&format!("  key id      {}\n", detail.key_id));
    out.push_str(&format!(
        "  sealed      {} (keyed fingerprints; values not shown)\n",
        detail.sealed.join(", ")
    ));
    if detail.limitations.is_empty() {
        out.push_str("  limitations none\n");
    } else {
        out.push_str("  limitations accepted, not refused:\n");
        for limitation in &detail.limitations {
            out.push_str(&format!(
                "    {}: {}\n",
                limitation.name, limitation.description
            ));
        }
    }
    out.push_str("  surfaces\n");
    for surface in &detail.surfaces {
        out.push_str(&format!(
            "    {:<11} {}\n",
            surface.verdict, surface.surface
        ));
    }
    if !detail.order.is_empty() {
        out.push_str("  order\n");
        // Its kind and subject, as the resolver's refusals name a change.
        let subject = |position: usize| {
            changes
                .changes
                .get(position - 1)
                .map(|p| {
                    let op = serde_json::to_value(&p.change)
                        .ok()
                        .and_then(|v| v.get("op").and_then(|op| op.as_str()).map(str::to_owned))
                        .unwrap_or_default();
                    format!("{op} {}", p.change.subject())
                })
                .unwrap_or_default()
        };
        for step in &detail.order {
            out.push_str(&format!(
                "    #{} {} before #{} {} ({})\n",
                step.before,
                subject(step.before),
                step.after,
                subject(step.after),
                step.reason
            ));
        }
    }
    // Counted by catalog class, not listed: the bindings read the whole
    // captured scope, system objects included, thousands of records a
    // reviewer would scroll past. The JSON lists each one.
    if !detail.prerequisites.is_empty() {
        let mut classes: std::collections::BTreeMap<&str, usize> = Default::default();
        for prerequisite in &detail.prerequisites {
            let class = prerequisite.object.split(' ').next().unwrap_or_default();
            *classes.entry(class).or_default() += 1;
        }
        let installed = detail
            .prerequisites
            .iter()
            .filter(|p| !p.fingerprinted)
            .count();
        let counts: Vec<String> = classes
            .iter()
            .map(|(class, count)| format!("{class} {count}"))
            .collect();
        out.push_str(&format!(
            "  prerequisites {} catalog records, by class: {}\n",
            detail.prerequisites.len(),
            counts.join(", ")
        ));
        out.push_str(&format!(
            "              {} fingerprinted; {installed} installed by this plan\n",
            detail.prerequisites.len() - installed
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(scratch: &'static str, image: Option<&str>, writes: &'static str) -> ResolverDetail {
        ResolverDetail {
            entry: "ci".into(),
            scratch,
            image: image.map(str::to_owned),
            layout: "run-owned",
            outside_writes: writes,
            rule: String::new(),
            key_id: String::new(),
            sealed: Vec::new(),
            limitations: Vec::new(),
            surfaces: Vec::new(),
            order: Vec::new(),
            prerequisites: Vec::new(),
        }
    }

    /// A reviewer reads which entry ran, its kind of scratch and its
    /// layout, and whether a baseline's writes outside it went unchecked
    /// (#1674, #1517).
    #[test]
    fn the_runtime_line_names_the_entry_its_scratch_and_layout() {
        let docker = line(&detail("docker", Some("postgres:18"), "allowed"));
        assert!(
            docker.starts_with("ci (docker scratch, run-owned)"),
            "{docker}"
        );
        assert!(docker.contains("postgres:18"), "{docker}");
        assert!(docker.contains("not verified"), "{docker}");
        assert!(docker.contains("allowed, not checked"), "{docker}");
        // Negative: the default says nothing of outside writes or images.
        let server = line(&detail("server", None, "refused"));
        assert_eq!(server, "ci (server scratch, run-owned)");
        // Negative: neither says "vouched" or "measured" any more (#1636).
        for line in [&docker, &server] {
            assert!(
                !line.contains("vouched") && !line.contains("measured"),
                "{line}"
            );
        }
    }
}
