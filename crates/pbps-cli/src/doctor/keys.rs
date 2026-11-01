use std::collections::{BTreeMap, BTreeSet};

use pbps_config::{FingerprintKeySource, Project};
use pbps_db::fingerprint::{FingerprintKey, KeyId};

use super::Requested;
use crate::output::Finding;

enum Loaded {
    Key(KeyId),
    Finding(Finding),
}

pub(super) fn for_request(project: &Project, one: Option<&Requested>) -> Vec<Finding> {
    for_request_with(project, one, |name| load(project, name))
}

fn for_request_with(
    project: &Project,
    one: Option<&Requested>,
    loader: impl FnMut(&str) -> Loaded,
) -> Vec<Finding> {
    let names: Vec<String> = match one {
        Some(Requested { name, .. }) => name.iter().cloned().collect(),
        None => project.config.environments.keys().cloned().collect(),
    };
    classify(&names, loader)
}

#[cfg(all(test, unix))]
pub(super) fn fingerprint_key_findings(project: &Project, names: &[String]) -> Vec<Finding> {
    classify(names, |name| load(project, name))
}

fn load(project: &Project, name: &str) -> Loaded {
    let source = match project.fingerprint_key_source(name) {
        Ok(source) => source,
        Err(e) => {
            return Loaded::Finding(Finding::error("environment.fingerprint-key", e.to_string()));
        }
    };
    let loaded =
        match source {
            None => return Loaded::Finding(
                Finding::note(
                    "environment.no-fingerprint-key",
                    format!(
                        "environment `{name}` configures no fingerprint key; a PostgreSQL plan \
                     needs one when it pins routines (DEC-319.1), and so will \
                     engine-assisted planning (--resolve-with)"
                    ),
                )
                .remedy(format!(
                    "pbps key generate --out {}   # then set `fingerprint_key_file` in pbps.yml",
                    crate::report::shell_arg(&format!("{name}.fingerprint.key"))
                        .unwrap_or_else(|| crate::report::placeholder("key path")),
                )),
            ),
            Some(FingerprintKeySource::Env(variable)) => FingerprintKey::from_env(&variable),
            Some(FingerprintKeySource::File(path)) => FingerprintKey::from_file(&path),
        };
    match loaded {
        // Retain only the public identifier, never the loaded secret.
        Ok(key) => Loaded::Key(key.id()),
        Err(e) => Loaded::Finding(Finding::error(
            "environment.fingerprint-key",
            format!("environment `{name}`: {e}"),
        )),
    }
}

fn classify(names: &[String], mut load: impl FnMut(&str) -> Loaded) -> Vec<Finding> {
    let mut seen = BTreeSet::new();
    let observed: Vec<_> = names
        .iter()
        .filter(|name| seen.insert(name.as_str()))
        .map(|name| (name, load(name)))
        .collect();
    let mut groups: BTreeMap<&KeyId, BTreeSet<&str>> = BTreeMap::new();
    for (name, loaded) in &observed {
        if let Loaded::Key(id) = loaded {
            groups.entry(id).or_default().insert(name);
        }
    }
    observed.iter().map(|(name, loaded)| match loaded {
        Loaded::Finding(finding) => finding.clone(),
        Loaded::Key(id) if groups[id].len() > 1 => Finding::error(
            "environment.shared-fingerprint-key",
            format!("environment `{name}` shares fingerprint key {id} with environments {}; \
                     each environment needs an independent key (DEC-964.1)",
                    groups[id].iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ")),
        ).remedy("Deliver an independent fingerprint key to each environment; after rotating a key, regenerate plans sealed under its old identifier."),
        Loaded::Key(id) => Finding::note("environment.fingerprint-key", format!("environment `{name}` has fingerprint key {id}")),
    }).collect()
}

#[cfg(test)]
mod tests;
