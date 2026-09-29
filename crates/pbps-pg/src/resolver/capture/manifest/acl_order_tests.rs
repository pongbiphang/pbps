//! ACL fingerprints must describe logical grants, not run-local role spelling.
//!
//! The measured PG16/18 ACL includes owner, grantor, grantee, privilege and
//! grant option. PostgreSQL's ACL is a set of entries, whereas unrelated
//! catalog arrays may have meaningful order. The test goes through the same
//! manifest property fingerprint that the resolved producer persists.

use super::*;
use crate::resolver::authorization::{AuthorizationContext, RoleAttributes, RoleMap};
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::environment::DeploymentPrincipal;
use serde_json::json;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

static KEY_FILE: AtomicU64 = AtomicU64::new(0);

fn key() -> EnvironmentFingerprintKey {
    let path = std::env::temp_dir().join(format!(
        "pbps-1274-acl-{}-{}.key",
        std::process::id(),
        KEY_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.write_all(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .unwrap();
    drop(file);
    let key = EnvironmentFingerprintKey::from_file(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    key
}

fn principal(name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: "pg_authid".into(),
        name: vec![name.into()],
        signature: Vec::new(),
    }
}

fn context(extra: bool) -> AuthorizationContext {
    let mut roles = BTreeMap::new();
    for name in std::iter::once("owner".to_owned())
        .chain((0..14).map(|n| format!("reader_{n:02}")))
        .chain(extra.then_some("aaa-padding".to_owned()))
    {
        roles.insert(
            name,
            RoleAttributes {
                superuser: false,
                inherit: true,
                bypass_rls: false,
                can_set: false,
                inherits: true,
            },
        );
    }
    AuthorizationContext {
        principal: DeploymentPrincipal {
            login: "deployer".into(),
            effective: "deployer".into(),
            superuser: false,
        },
        schemas: BTreeMap::new(),
        roles,
        settings: BTreeMap::new(),
    }
}

fn run_name(map: &RoleMap, logical: &str) -> String {
    map.run_local_names()
        .into_iter()
        .find(|name| map.logical_of(name).as_deref() == Some(logical))
        .unwrap()
}

fn grants(map: Option<&RoleMap>, altered_grantor: bool, altered_option: bool) -> Value {
    let name = |logical: &str| map.map_or_else(|| logical.to_owned(), |m| run_name(m, logical));
    let mut entries = (0..14)
        .map(|n| {
            let reader = format!("reader_{n:02}");
            let grantor = if altered_grantor && n == 11 {
                "reader_00"
            } else {
                "owner"
            };
            json!({
                "grantor": principal(&name(grantor)),
                "grantee": principal(&name(&reader)),
                "privilege": "EXECUTE",
                "grant_option": altered_option && n == 11,
            })
        })
        .collect::<Vec<_>>();
    // This is the order of the raw ACL entries handed to the private
    // manifest seal; it is deliberately not their eventual logical order.
    entries.sort_by_cached_key(Value::to_string);
    Value::Array(entries)
}

fn property_fingerprint(
    key: &EnvironmentFingerprintKey,
    acl: Value,
    ordered: Value,
    map: Option<&RoleMap>,
) -> String {
    let object = ObjectIdentity {
        class: "pg_proc".into(),
        name: vec!["app".into(), "f".into()],
        signature: Vec::new(),
    };
    let capture = CapturedInputs {
        baseline: super::super::baseline::Baseline::Absent,
        session: super::super::session::Facts {
            current_role: principal("deployer"),
            session_role: principal("deployer"),
            settings: BTreeMap::new(),
        },
        rule: properties::RULE,
        major: 18,
        scope: CaptureScope {
            retained: BTreeSet::from([object.clone()]),
            candidates: BTreeSet::new(),
        },
        inputs: BTreeMap::from([(
            object,
            Input {
                properties: BTreeMap::from([("proacl".into(), acl), ("ordered".into(), ordered)]),
                bindings: Vec::new(),
            },
        )]),
        role_pinned: BTreeMap::new(),
        attribute_numbers: BTreeMap::new(),
        candidates: BTreeMap::new(),
        limitations: BTreeSet::new(),
        dropped: BTreeMap::new(),
    };
    capture
        .seal_with_roles(key, map, None)
        .unwrap()
        .prerequisites()[0]
        .properties
        .clone()
}

#[test]
fn mapped_acl_order_does_not_change_a_logically_identical_property_fingerprint() {
    let key = key();
    let first = RoleMap::generate(&context(false), &[], "run-a", "token-a");
    let shifted = RoleMap::generate(&context(true), &[], "run-b", "token-b");
    // Lexical role_10 precedes role_2 despite numerical creation order.
    assert!(run_name(&first, "reader_10") < run_name(&first, "reader_02"));
    assert_ne!(
        run_name(&first, "reader_10"),
        run_name(&shifted, "reader_10")
    );
    let target = property_fingerprint(&key, grants(None, false, false), json!([1, 2]), None);
    for map in [&first, &shifted] {
        let equivalent = property_fingerprint(
            &key,
            grants(Some(map), false, false),
            json!([1, 2]),
            Some(map),
        );
        assert_eq!(
            equivalent, target,
            "run-local ACL order is not a grant change"
        );
        assert_ne!(
            property_fingerprint(
                &key,
                grants(Some(map), true, false),
                json!([1, 2]),
                Some(map)
            ),
            target,
            "a different grantor changes the ACL"
        );
        assert_ne!(
            property_fingerprint(
                &key,
                grants(Some(map), false, true),
                json!([1, 2]),
                Some(map)
            ),
            target,
            "a grant option changes the ACL"
        );
        assert_ne!(
            property_fingerprint(
                &key,
                grants(Some(map), false, false),
                json!([2, 1]),
                Some(map)
            ),
            target,
            "unrelated arrays retain their ordered meaning"
        );
    }
}
