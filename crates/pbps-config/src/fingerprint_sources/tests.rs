use super::*;
use crate::{Config, FingerprintKeySource, Project};

fn environments(text: &str) -> BTreeMap<String, Environment> {
    serde_saphyr::from_str(text).unwrap()
}

fn refused_pair(error: ConfigError, expected_kind: &str) {
    let ConfigError::SharedFingerprintKeySource {
        first,
        second,
        kind,
    } = &error
    else {
        panic!("unexpected error: {error}");
    };
    assert_eq!(
        (first.as_str(), second.as_str(), *kind),
        ("a", "b", expected_kind)
    );
}

#[test]
fn two_environments_cannot_name_one_fingerprint_variable() {
    let yaml = "dialect: postgres\nenvironments:\n  c: {url_env: C, fingerprint_key_env: SAME}\n  b: {url_env: B, fingerprint_key_env: SAME}\n  a: {url_env: A, fingerprint_key_env: SAME}\n";
    refused_pair(
        Config::parse(yaml, Path::new("pbps.yml")).unwrap_err(),
        "variable",
    );
    refused_pair(
        Config::parse_project(yaml, Path::new("pbps.yml")).unwrap_err(),
        "variable",
    );
}

#[test]
fn two_environments_cannot_name_one_rooted_fingerprint_file() {
    let yaml = "dialect: postgres\nenvironments:\n  a: {url_env: A, fingerprint_key_file: './keys//./one'}\n  b: {url_env: B, fingerprint_key_file: keys/one}\n";
    refused_pair(
        Config::parse(yaml, Path::new("nested/pbps.yml")).unwrap_err(),
        "file",
    );
}

#[test]
fn relative_config_and_absolute_key_share_one_comparison_root() {
    let base = std::env::temp_dir();
    let absolute = base.join("project/keys/one");
    let mut envs = environments(
        "a: {url_env: A, fingerprint_key_file: keys/one}\nb: {url_env: B, fingerprint_key_file: other}\n",
    );
    envs.get_mut("b").unwrap().fingerprint_key_file = Some(absolute);
    refused_pair(
        validate_with_base(&envs, Path::new("project/pbps.yml"), || Ok(base)).unwrap_err(),
        "file",
    );
}

#[test]
fn absolute_config_needs_no_ambient_base_and_runtime_roots_are_unchanged() {
    let root = std::env::temp_dir().join("source-comparison964");
    let mut envs = environments(
        "a: {url_env: A, fingerprint_key_file: key}\nb: {url_env: B, fingerprint_key_file: other}\n",
    );
    envs.get_mut("b").unwrap().fingerprint_key_file = Some(root.join("key"));
    refused_pair(
        validate_with_base(&envs, &root.join("pbps.yml"), || panic!("unnecessary cwd"))
            .unwrap_err(),
        "file",
    );
    let config = Config::parse("dialect: postgres\nenvironments:\n  a: {url_env: A, fingerprint_key_file: ./missing.key}\n", Path::new("nested/pbps.yml")).unwrap();
    let project = Project {
        root: PathBuf::from("nested"),
        config,
    };
    assert_eq!(project.root, Path::new("nested"));
    assert_eq!(
        project.fingerprint_key_source("a").unwrap(),
        Some(FingerprintKeySource::File(PathBuf::from(
            "nested/./missing.key"
        )))
    );
}

#[test]
fn an_absolute_base_is_loaded_only_when_comparison_needs_it() {
    for text in [
        "{}",
        "a: {url_env: A, fingerprint_key_file: key}",
        "a: {url_env: A, fingerprint_key_env: A}\nb: {url_env: B, fingerprint_key_env: B}",
        "a: {url_env: A, fingerprint_key_file: one}\nb: {url_env: B, fingerprint_key_file: two}",
    ] {
        validate_with_base(&environments(text), Path::new("nested/pbps.yml"), || {
            panic!("unnecessary cwd")
        })
        .unwrap();
    }
    let mut envs = environments(
        "a: {url_env: A, fingerprint_key_file: one}\nb: {url_env: B, fingerprint_key_file: two}",
    );
    envs.get_mut("b").unwrap().fingerprint_key_file = Some(std::env::temp_dir().join("two"));
    let error = validate_with_base(&envs, Path::new("nested/pbps.yml"), || {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "test base unavailable",
        ))
    })
    .unwrap_err();
    assert!(matches!(
        error,
        ConfigError::FingerprintKeyPathContext { .. }
    ));
}

#[test]
fn unrelated_or_absent_sources_do_not_require_keys_to_exist() {
    let yaml = "dialect: postgres\nenvironments:\n  a: {url_env: URL, fingerprint_key_env: missing.key}\n  b: {url_env: URL, fingerprint_key_file: missing.key}\n  c: {url_env: URL, fingerprint_key_env: OTHER}\n  d: {url_env: URL, fingerprint_key_file: other-missing.key}\n  e: {url_env: URL}\n  f: {url_env: URL, fingerprint_key_file: null}\n";
    Config::parse(yaml, Path::new("nested/pbps.yml")).unwrap();
}

#[test]
fn existing_resolver_and_same_environment_ambiguity_take_precedence() {
    let base = "dialect: postgres\nenvironments:\n  a: {url_env: A, fingerprint_key_env: SAME, fingerprint_key_file: one}\n  b: {url_env: B, fingerprint_key_env: SAME}\n";
    assert!(matches!(
        Config::parse(base, Path::new("pbps.yml")),
        Err(ConfigError::AmbiguousFingerprintKey { .. })
    ));
    let yaml = format!("{base}resolve_with: 'bad name'\n");
    assert!(matches!(
        Config::parse(&yaml, Path::new("pbps.yml")),
        Err(ConfigError::InvalidResolverName)
    ));
}

#[cfg(unix)]
#[test]
fn parent_components_do_not_merge_distinct_symlink_targets() {
    let root = std::env::temp_dir().join(format!(
        "pbps-source964-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
    ));
    std::fs::create_dir_all(root.join("outside/child")).unwrap();
    std::os::unix::fs::symlink(root.join("outside/child"), root.join("link")).unwrap();
    std::fs::write(root.join("outside/key"), "one").unwrap();
    std::fs::write(root.join("key"), "two").unwrap();
    assert_ne!(
        std::fs::read(root.join("link/../key")).unwrap(),
        std::fs::read(root.join("key")).unwrap()
    );
    let envs = environments(
        "a: {url_env: A, fingerprint_key_file: link/../key}\nb: {url_env: B, fingerprint_key_file: key}",
    );
    validate(&envs, &root.join("pbps.yml")).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn non_utf8_addresses_do_not_compare_by_lossy_display() {
    use std::os::unix::ffi::OsStringExt;
    let mut envs = environments(
        "a: {url_env: A, fingerprint_key_file: one}\nb: {url_env: B, fingerprint_key_file: two}",
    );
    envs.get_mut("a").unwrap().fingerprint_key_file =
        Some(std::ffi::OsString::from_vec(vec![0x80]).into());
    envs.get_mut("b").unwrap().fingerprint_key_file =
        Some(std::ffi::OsString::from_vec(vec![0x81]).into());
    validate(&envs, Path::new("pbps.yml")).unwrap();
}

#[cfg(windows)]
#[test]
fn rooted_windows_paths_use_the_prefix_and_unresolved_drives_refuse() {
    let mut envs = environments(
        "a: {url_env: A, fingerprint_key_file: key}\nb: {url_env: B, fingerprint_key_file: other}",
    );
    envs.get_mut("b").unwrap().fingerprint_key_file = Some(PathBuf::from(r"C:\nested\key"));
    refused_pair(
        validate_with_base(&envs, Path::new(r"nested\pbps.yml"), || {
            Ok(PathBuf::from(r"C:\"))
        })
        .unwrap_err(),
        "file",
    );
    envs.get_mut("a").unwrap().fingerprint_key_file = Some(PathBuf::from(r"D:key"));
    assert!(matches!(
        validate_with_base(&envs, Path::new("pbps.yml"), || Ok(PathBuf::from(r"C:\"))),
        Err(ConfigError::FingerprintKeyPathContext { .. })
    ));
}

#[test]
fn public_parse_anchors_a_relative_config_to_its_parent() {
    let root = std::env::current_dir().unwrap();
    let make = |absolute: &Path| {
        format!(
            "dialect: postgres\nenvironments:\n  a: {{url_env: A, fingerprint_key_file: keys/one}}\n  b: {{url_env: B, fingerprint_key_file: {:?}}}\n",
            absolute
        )
    };
    let path = Path::new("nested/pbps.yml");
    refused_pair(
        Config::parse(&make(&root.join("nested/keys/one")), path).unwrap_err(),
        "file",
    );
    Config::parse(&make(&root.join("keys/one")), path).unwrap();
}
