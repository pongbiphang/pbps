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
