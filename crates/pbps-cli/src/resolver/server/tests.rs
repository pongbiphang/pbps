use super::*;

const COMPLETE: &str = "profile=linux-dedicated-v1 container=pbps-scratch-1 \
                        daemon=/run/podman/podman.sock user=pbps_admin password=s3cret";

#[test]
fn an_endpoint_needs_every_field_and_never_echoes_its_password() {
    let endpoint = ScratchEndpoint::parse(COMPLETE).unwrap();
    assert_eq!(endpoint.profile, "linux-dedicated-v1");
    assert_eq!(endpoint.container, "pbps-scratch-1");
    assert_eq!(endpoint.daemon, PathBuf::from("/run/podman/podman.sock"));
    assert_eq!(endpoint.user, "pbps_admin");
    let rendered = format!("{endpoint:?}");
    assert!(!rendered.contains("s3cret"), "{rendered}");
    // The daemon has a default; nothing else does.
    let defaulted =
        ScratchEndpoint::parse("profile=linux-dedicated-v1 container=c user=u password=p").unwrap();
    assert_eq!(defaulted.daemon, PathBuf::from(DEFAULT_DAEMON));
    for value in [
        "",
        // One missing field at a time.
        "container=c user=u password=p",
        "profile=linux-dedicated-v1 user=u password=p",
        "profile=linux-dedicated-v1 container=c password=p",
        "profile=linux-dedicated-v1 container=c user=u",
        // A relative daemon path, an option-shaped or empty container name.
        "profile=linux-dedicated-v1 container=c daemon=docker.sock user=u password=p",
        "profile=linux-dedicated-v1 container=-c user=u password=p",
        "profile=linux-dedicated-v1 container=c/../d user=u password=p",
        "profile=linux-dedicated-v1 container=c user= password=p",
        // A host, port, socket or repeated field cannot smuggle in another server.
        "profile=linux-dedicated-v1 container=c user=u password=p host=other",
        "profile=linux-dedicated-v1 container=c user=u password=p socket=/run/s",
        "profile=linux-dedicated-v1 container=c container=d user=u password=p",
        "profile=a b container=c user=u password=p",
    ] {
        assert!(
            matches!(ScratchEndpoint::parse(value), Err(Error::Endpoint)),
            "{value:?} must not parse"
        );
    }
}

/// The profile name reaches a refusal message, so it may not carry anything
/// but an ordinary configured name.
#[test]
fn a_profile_name_is_reported_without_arbitrary_text() {
    assert_eq!(named("linux-dedicated-v1"), "linux-dedicated-v1");
    assert_eq!(named(""), "<unnamed>");
    assert_eq!(named("\n\t drop table"), "droptable");
    assert_eq!(named(&"a".repeat(200)).len(), 64);
    // Anything `named` would have to strip is refused at the endpoint.
    assert!(ScratchEndpoint::parse(&COMPLETE.replace("linux-dedicated-v1", "a/b")).is_err());
}

#[test]
fn generated_scratch_names_are_unique_and_never_collide_with_each_other() {
    let first = generated_names().unwrap();
    let second = generated_names().unwrap();
    assert!(first.database().starts_with("pbps_scratch_"));
    assert!(first.login().starts_with("pbps_run_"));
    assert_ne!(first.database(), second.database());
    assert_ne!(first.login(), second.login());
    assert_ne!(first.password(), second.password());
}

/// Only a `server` profile has an endpoint to read; a Docker profile's
/// image is not a scratch server, however it is spelled.
#[test]
fn only_a_server_profile_yields_an_endpoint() {
    use pbps_config::resolver::{PullPolicy, ResolverProfile};
    assert!(matches!(
        ScratchEndpoint::from_profile(&ResolverProfile::Docker {
            image: "postgres:18".into(),
            pull: PullPolicy::Never,
        }),
        Err(Error::Endpoint)
    ));
    assert!(matches!(
        ScratchEndpoint::from_profile(&ResolverProfile::Server {
            url_env: "PBPS_UNSET_DEDICATED_SERVER_609".into(),
        }),
        Err(Error::Endpoint)
    ));
}

/// A cleanup that succeeded must still say why the run ended, and one that
/// did not must name exactly the two objects a human has to remove.
#[test]
fn a_successful_removal_keeps_the_reason_the_run_ended() {
    let names = generated_names().unwrap();
    for cause in [
        Error::Channel("a reason".into()),
        Error::Exclusivity(super::Signal::SessionList),
        Error::Scratch,
    ] {
        let outcome = removal_outcome(true, cause.clone(), &names);
        assert_eq!(
            std::mem::discriminant(&outcome.cause),
            std::mem::discriminant(&cause),
            "a removed scratch database must not rename the failure"
        );
        assert!(outcome.recovery_names.is_empty());
    }
    let outcome = removal_outcome(false, Error::Channel("a reason".into()), &names);
    assert!(matches!(outcome.cause, Error::Cleanup));
    assert_eq!(
        outcome.recovery_names,
        vec![names.database().to_owned(), names.login().to_owned()]
    );
    assert!(
        !outcome
            .recovery_names
            .iter()
            .any(|name| name == names.password())
    );
}

/// The daemon's record is pinned by everything a restart or a replacement
/// changes, and a record that is missing any of it is not pinned at all.
#[test]
fn a_restarted_or_replaced_container_is_not_the_pinned_one() {
    let record = |pid: u64, started: &str, id: &str| {
        serde_json::json!({
            "Id": id,
            "Image": "sha256:abc",
            "State": {"Running": true, "Restarting": false, "Paused": false,
                      "Pid": pid, "StartedAt": started},
        })
    };
    let id = "a".repeat(64);
    let pinned = pin(&record(4242, "2026-09-17T10:00:00Z", &id)).unwrap();
    assert_eq!(pinned.pid, 4242);
    assert_ne!(
        pin(&record(4243, "2026-09-17T10:00:00Z", &id)).unwrap(),
        pinned
    );
    assert_ne!(
        pin(&record(4242, "2026-09-17T10:05:00Z", &id)).unwrap(),
        pinned
    );
    assert_ne!(
        pin(&record(4242, "2026-09-17T10:00:00Z", &"b".repeat(64))).unwrap(),
        pinned
    );
    for (path, value) in [
        (["State", "Running"], serde_json::Value::Bool(false)),
        (["State", "Pid"], serde_json::Value::from(0)),
        (["State", "StartedAt"], serde_json::Value::from("")),
    ] {
        let mut state = record(4242, "2026-09-17T10:00:00Z", &id);
        state[path[0]][path[1]] = value;
        assert!(matches!(pin(&state), Err(Error::Container(_))));
    }
    let mut short = record(4242, "2026-09-17T10:00:00Z", &id);
    short["Id"] = "abc".into();
    assert!(matches!(pin(&short), Err(Error::Container(_))));
}

/// A refusal the adapter raises is a credential problem, not an intrusion.
#[test]
fn an_unreadable_engine_answer_is_never_reported_as_a_moved_counter() {
    assert_eq!(
        signal(&pbps_db::DbError::Refused("no privilege".into())),
        Signal::Unreadable
    );
}

#[test]
fn a_forwarder_left_behind_is_named_without_replacing_the_reason() {
    let names = generated_names().unwrap();
    let reason = Error::Exclusivity(Signal::SessionList);
    // The scratch objects were removed; one forwarder could not be confirmed.
    let outcome = report(
        removal_outcome(true, reason.clone(), &names),
        vec!["pbps-resolver-a".into(), "pbps-resolver-a".into()],
    );
    assert!(matches!(
        outcome.cause,
        Error::Exclusivity(Signal::SessionList)
    ));
    assert_eq!(outcome.recovery_names, vec!["pbps-resolver-a".to_owned()]);
    // The scratch objects were not removed: that, not the forwarder, is why the
    // cause becomes `Cleanup`, and both kinds of name are reported once.
    let outcome = report(
        removal_outcome(false, reason, &names),
        vec!["pbps-resolver-a".into(), names.login().to_owned()],
    );
    assert!(matches!(outcome.cause, Error::Cleanup));
    assert_eq!(outcome.recovery_names.len(), 3);
    assert!(
        outcome
            .recovery_names
            .contains(&names.database().to_owned())
    );
    assert!(
        outcome
            .recovery_names
            .contains(&"pbps-resolver-a".to_owned())
    );
}

#[test]
fn expected_visibility_is_pg_catalog_then_the_usable_path_schemas_in_order() {
    use pbps_pg::resolver::authorization::{AuthorizationContext, SchemaAuthorization};
    use std::collections::BTreeMap;
    let schema = |usage: bool| SchemaAuthorization {
        owner: "o".into(),
        privileges: [("USAGE".to_owned(), usage), ("CREATE".to_owned(), false)]
            .into_iter()
            .collect(),
        acl: BTreeMap::new(),
    };
    let context = AuthorizationContext {
        principal: pbps_db::resolver::environment::DeploymentPrincipal {
            login: "d".into(),
            effective: "d".into(),
            superuser: false,
        },
        schemas: [
            ("app".to_owned(), schema(true)),
            ("secret".to_owned(), schema(false)),
            ("ext".to_owned(), schema(true)),
        ]
        .into_iter()
        .collect(),
        roles: BTreeMap::new(),
        settings: BTreeMap::new(),
    };
    let extras = vec!["ext".to_owned()];
    let visibility =
        super::expected_visibility(&context, &["app".to_owned(), "secret".to_owned()], &extras);
    // app is usable and ext (an extra) is usable, in path order after pg_catalog.
    assert_eq!(
        visibility["app"],
        pbps_db::resolver::Observation::reported(Some(r#"["pg_catalog","app","ext"]"#))
    );
    // secret is not usable, so only pg_catalog and the usable extra remain.
    assert_eq!(
        visibility["secret"],
        pbps_db::resolver::Observation::reported(Some(r#"["pg_catalog","ext"]"#))
    );
}
