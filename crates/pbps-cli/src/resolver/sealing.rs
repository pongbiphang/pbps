//! Persisted-evidence policy shared by planning and artifact readers. Loading a
//! key does not acquire a resolver or qualify any database connection.

use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_model::resolver::ResolverRuntime;

#[derive(Debug, thiserror::Error)]
#[error(
    "resolver evidence requires the target environment's fingerprint key: {0}. Generate one with `pbps key generate`, configure `fingerprint_key_env` or `fingerprint_key_file`, and plan with `--env` again"
)]
pub struct KeyRequired(String);

pub fn environment_key(
    project: &pbps_config::Project,
    environment: Option<&str>,
) -> Result<EnvironmentFingerprintKey, KeyRequired> {
    let environment = environment
        .ok_or_else(|| KeyRequired("a bare --db target names no configured environment".into()))?;
    match project
        .fingerprint_key_source(environment)
        .map_err(|e| KeyRequired(e.to_string()))?
    {
        Some(pbps_config::FingerprintKeySource::Env(name)) => {
            EnvironmentFingerprintKey::from_env(&name)
        }
        Some(pbps_config::FingerprintKeySource::File(path)) => {
            EnvironmentFingerprintKey::from_file(&path)
        }
        None => {
            return Err(KeyRequired(format!(
                "environment `{environment}` configures no key"
            )));
        }
    }
    .map_err(|e| KeyRequired(e.to_string()))
}

/// The target-only closing checker (#616) uses this exact encoding for
/// both phases. The phase selects which observed facts to compare, not a
/// different HMAC domain that would make equal facts compare unequal.
#[cfg(target_os = "linux")]
pub(crate) fn target_catalog_fingerprint(
    key: &EnvironmentFingerprintKey,
    facts: &pbps_db::resolver::environment::CatalogFacts,
) -> Result<String, &'static str> {
    let mut canonical = facts.clone();
    // SQL readers order these inventories, but sorting here makes the
    // persisted encoding independent of row delivery order. Duplicates are
    // unknown coverage, not an excuse to silently discard an observation.
    canonical.extensions.sort_by(|a, b| a.name.cmp(&b.name));
    if canonical
        .extensions
        .windows(2)
        .any(|w| w[0].name == w[1].name)
        || canonical
            .extensions
            .iter()
            .any(|e| e.name.is_empty() || e.version.is_empty())
    {
        return Err("the target extension inventory is incomplete");
    }
    for extension in &mut canonical.extensions {
        extension.requires.sort();
        extension.libraries.sort();
        if extension.requires.windows(2).any(|w| w[0] == w[1])
            || extension.libraries.windows(2).any(|w| w[0] == w[1])
        {
            return Err("the target extension inventory is ambiguous");
        }
    }
    canonical.collations.sort_by(|a, b| a.key.cmp(&b.key));
    if canonical
        .collations
        .windows(2)
        .any(|w| w[0].key == w[1].key)
        || canonical.collations.iter().any(|c| c.key.is_empty())
    {
        return Err("the target collation inventory is incomplete");
    }
    for versions in canonical.available_extensions.values_mut() {
        versions.sort();
        if versions.windows(2).any(|w| w[0] == w[1]) {
            return Err("the target available-extension inventory is ambiguous");
        }
    }
    if canonical.visibility.values().any(|observed| {
        observed
            .value()
            .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
            .is_none()
    }) {
        return Err("the target effective visibility is unreadable");
    }
    let bytes =
        serde_json::to_vec(&canonical).map_err(|_| "the target catalog facts cannot be encoded")?;
    Ok(hex(key.fingerprint(
        "pbps/pg-target-catalog/v1",
        "catalog-facts",
        &bytes,
    )))
}

#[cfg(target_os = "linux")]
pub(crate) fn hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Recognizing a saved profile version is independent of the reader's host:
/// offline explanation of a Linux artifact must work on Windows too. This
/// validates the recorded contract, not the runtime's present-day condition.
pub fn validate_runtime(runtime: &ResolverRuntime) -> Result<(), &'static str> {
    match runtime {
        ResolverRuntime::Container {
            platform, profile, ..
        } if platform == "linux/amd64" && profile == "linux-amd64-v1" => Ok(()),
        ResolverRuntime::Supplied { profile, .. }
            if matches!(
                profile.as_str(),
                "linux-dedicated-v1" | "linux-dedicated-pg16-v1"
            ) =>
        {
            Ok(())
        }
        // Nothing to enforce: the operator vouches for it, and the evidence
        // says so (DEC-1528.1).
        ResolverRuntime::Vouched => Ok(()),
        ResolverRuntime::Container { .. } | ResolverRuntime::Supplied { .. } => {
            Err("this build cannot enforce the recorded resolver runtime profile")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_support_is_versioned_and_never_accepts_an_unknown_runtime() {
        assert!(
            validate_runtime(&ResolverRuntime::Supplied {
                profile: "linux-dedicated-v1".into(),
                identity: "00".repeat(32)
            })
            .is_ok()
        );
        assert!(
            validate_runtime(&ResolverRuntime::Supplied {
                profile: "linux-dedicated-pg16-v1".into(),
                identity: "00".repeat(32)
            })
            .is_ok()
        );
        for profile in ["linux-dedicated-pg16-v2", "linux-dedicated-pg16-v1-extra"] {
            assert!(
                validate_runtime(&ResolverRuntime::Supplied {
                    profile: profile.into(),
                    identity: "00".repeat(32)
                })
                .is_err(),
                "saved runtimes require an exact implemented profile"
            );
        }
        assert!(
            validate_runtime(&ResolverRuntime::Supplied {
                profile: "linux-dedicated-v2".into(),
                identity: "00".repeat(32)
            })
            .is_err()
        );
        assert!(
            validate_runtime(&ResolverRuntime::Container {
                image_digest: format!("sha256:{}", "00".repeat(32)),
                platform: "linux/arm64".into(),
                profile: "linux-amd64-v1".into()
            })
            .is_err()
        );
    }
    #[test]
    fn missing_environment_keys_refuse_with_the_key_generation_remedy() {
        use std::io::Write;
        let path =
            std::env::temp_dir().join(format!("pbps-614-config-{}.yml", rand::random::<u64>()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(
            b"dialect: postgres\nenvironments:\n  prod:\n    url_env: PBPS614_UNUSED_CONNECTION\n",
        )
        .unwrap();
        let project = pbps_config::Project::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        for environment in [None, Some("prod")] {
            let error = environment_key(&project, environment)
                .unwrap_err()
                .to_string();
            assert!(error.contains("pbps key generate"));
            assert!(error.contains("fingerprint_key_env"));
        }
    }
}
