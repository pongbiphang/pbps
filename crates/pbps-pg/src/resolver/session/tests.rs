use super::*;
use pbps_db::resolver::environment::DeploymentPrincipal;
use std::collections::BTreeMap;

fn context(settings: &[(&str, &str)]) -> AuthorizationContext {
    AuthorizationContext {
        principal: DeploymentPrincipal {
            login: "login".into(),
            effective: "deployer".into(),
            superuser: false,
        },
        schemas: BTreeMap::new(),
        roles: BTreeMap::new(),
        settings: settings
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect(),
    }
}

fn recipe(intent: &Intent, reset: Reset) -> Vec<String> {
    intent
        .restoration(reset)
        .iter()
        .map(|step| match step {
            Step::Settings(_) => "overlay".into(),
            Step::Backend => "checked backend".into(),
            Step::Reset(_) | Step::Path | Step::Role(_) | Step::Pins(_) | Step::Keeper => {
                step.statement().unwrap()
            }
        })
        .collect()
}

#[test]
fn the_session_path_precedes_the_overlay_and_target_pins() {
    for settings in [
        None,
        Some(context(&[])),
        Some(context(&[("database:array_nulls", "off")])),
    ] {
        for zone in [None, Some(SessionZone::new("Asia/Taipei").unwrap())] {
            let intent = Intent {
                role: Some("a\"b".into()),
                settings: settings.clone(),
                pins: zone.clone(),
            };
            let all = recipe(&intent, Reset::All);
            assert_eq!(
                &all[..3],
                ["RESET ALL", "SET search_path = ''", "checked backend"]
            );
            assert!(!all.iter().any(|sql| sql.starts_with("SET ROLE")));
            assert_eq!(
                all.iter().position(|s| s == "overlay"),
                settings.as_ref().map(|_| 4)
            );
            let pin = zone.as_ref().map(crate::session_pins);
            assert_eq!(
                all.len(),
                4 + usize::from(settings.is_some()) + usize::from(zone.is_some())
            );
            if let Some(pin) = &pin {
                assert_eq!(&all[all.len() - 1], pin);
                assert!(pin.contains("SET TimeZone = 'Asia/Taipei'"));
            }
            assert_eq!(all[3], scratch::KEEPER_SETTINGS);
            let discard = recipe(&intent, Reset::Discard);
            assert_eq!(
                &discard[..4],
                [
                    "DISCARD ALL",
                    "SET search_path = ''",
                    "checked backend",
                    "SET ROLE \"a\"\"b\""
                ]
            );
            assert_eq!(&discard[4..], &all[3..]);
        }
    }
}

#[test]
fn an_uninstalled_overlay_differs_from_an_installed_empty_one() {
    let absent = Intent {
        role: None,
        settings: None,
        pins: None,
    };
    let installed = Intent {
        settings: Some(context(&[])),
        ..absent.clone()
    };
    assert!(!recipe(&absent, Reset::All).contains(&"overlay".into()));
    assert!(recipe(&installed, Reset::All).contains(&"overlay".into()));
    assert_eq!(recipe(&absent, Reset::Discard)[3], "SET ROLE NONE");
}

async fn shown(conn: &mut pbps_db::Conn, name: &str) -> String {
    let rows = conn.query(&format!("SHOW {name}")).await.unwrap();
    rows[0].try_get::<&str>(name).unwrap().unwrap().into()
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn repeated_restores_keep_the_registered_role_overlay_zone_and_session_path() {
    use pbps_db::{Conn, Driver};
    for server in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let url = std::env::var(server).unwrap();
        let mut conn = Conn::connect(Driver::Postgres, &url).await.unwrap();
        let role = format!(
            "pbps_session2133_{:016x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        conn.execute(&format!("CREATE ROLE {role}")).await.unwrap();
        let mut state = SessionState::new(Some(role.clone().into()));
        let mut overlay = context(&[
            ("database:array_nulls", "on"),
            ("role:array_nulls", "off"),
            ("database-role:array_nulls", "off"),
            ("role:TimeZone", "UTC"),
            ("role:search_path", "public"),
            ("role:client_encoding", "LATIN1"),
            ("role:session_preload_libraries", "no_such_library"),
            ("role:standard_conforming_strings", "off"),
            ("role:tcp_keepalives_idle", "61"),
        ]);
        overlay.principal.effective = role.clone().into();
        conn.execute(&format!("SET ROLE {role}")).await.unwrap();
        state.install_settings(&mut conn, &overlay).await.unwrap();
        state.compiling(&SessionZone::new("Asia/Taipei").unwrap());
        for reset in [Reset::All, Reset::Discard, Reset::All, Reset::Discard] {
            conn.execute("SET array_nulls = on; SET TimeZone = 'UTC'; SET search_path = public; SET standard_conforming_strings = off").await.unwrap();
            if reset == Reset::All {
                conn.execute("BEGIN").await.unwrap();
            }
            reset_and_restore(&mut conn, &state, reset).await.unwrap();
            if reset == Reset::All {
                conn.execute("COMMIT").await.unwrap();
            }
            assert_eq!(
                shown(&mut conn, "search_path").await,
                "\"\"",
                "{server}: after commit"
            );
            assert_eq!(shown(&mut conn, "array_nulls").await, "off", "{server}");
            assert_eq!(
                shown(&mut conn, "TimeZone").await,
                "Asia/Taipei",
                "{server}"
            );
            assert_eq!(
                shown(&mut conn, "standard_conforming_strings").await,
                "on",
                "pins win over overlay"
            );
            assert_eq!(
                shown(&mut conn, "client_encoding").await,
                "UTF8",
                "startup setting wins"
            );
            assert_eq!(
                shown(&mut conn, "tcp_keepalives_idle").await,
                "61",
                "the installed overlay keeps its existing precedence over keeper defaults"
            );
            let rows = conn
                .query("SELECT current_user::text AS name")
                .await
                .unwrap();
            assert_eq!(rows[0].try_get::<&str>("name").unwrap().unwrap(), role);
        }
        // Observation discards the compilation overlay and pins on both reads.
        let observation = state.observation();
        reset_and_restore(&mut conn, &observation, Reset::All)
            .await
            .unwrap();
        let before = shown(&mut conn, "TimeZone").await;
        assert_eq!(
            shown(&mut conn, "search_path").await,
            "\"\"",
            "observation has a session-level path pin without compilation pins"
        );
        assert_eq!(shown(&mut conn, "array_nulls").await, "on");
        assert_eq!(
            shown(&mut conn, "tcp_keepalives_idle").await,
            "30",
            "clean observation has keeper defaults without the compilation overlay"
        );
        conn.execute("SET TimeZone = 'Asia/Taipei'").await.unwrap();
        reset_and_restore(&mut conn, &observation, Reset::All)
            .await
            .unwrap();
        assert_eq!(shown(&mut conn, "TimeZone").await, before);
        reset_and_restore(&mut conn, &state, Reset::All)
            .await
            .unwrap();
        assert_eq!(shown(&mut conn, "array_nulls").await, "off");
        conn.execute("SET ROLE NONE").await.unwrap();
        let cleanup = SessionState::checked(scratch::backend(&mut conn).await.unwrap(), None);
        conn.execute(&format!("SET ROLE {role}")).await.unwrap();
        // An observation preserves a script-selected role even when that role
        // cannot read the authenticating login's backend_start.
        reset_and_restore(&mut conn, &cleanup.observation(), Reset::All)
            .await
            .unwrap();
        let rows = conn
            .query("SELECT current_user::text AS name")
            .await
            .unwrap();
        assert_eq!(rows[0].try_get::<&str>("name").unwrap().unwrap(), role);
        reset_and_restore(&mut conn, &cleanup, Reset::Discard)
            .await
            .unwrap();
        conn.execute(&format!("DROP ROLE {role}")).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_failed_restore_stops_at_its_stage_and_rejects_another_backend() {
    use pbps_db::{Conn, Driver};
    for server in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let url = std::env::var(server).unwrap();
        let mut conn = Conn::connect(Driver::Postgres, &url).await.unwrap();
        let mut state = SessionState::checked(scratch::backend(&mut conn).await.unwrap(), None);
        state
            .install_settings(&mut conn, &context(&[("role:array_nulls", "off")]))
            .await
            .unwrap();
        state.compiling(&SessionZone::new("Asia/Taipei").unwrap());
        for reset in [Reset::All, Reset::Discard] {
            for fail in 0..state.intent.restoration(reset).len() {
                let mut visited = 0;
                let result = restore_steps(&mut conn, &state, reset, |index| {
                    visited += 1;
                    if index == fail {
                        Err(DbError::Refused("injected restoration failure".into()))
                    } else {
                        Ok(())
                    }
                })
                .await;
                assert!(result.is_err(), "{server}: {reset:?} stage {fail}");
                assert_eq!(visited, fail + 1, "no later restoration stage runs");
            }
        }
        reset_and_restore(&mut conn, &state, Reset::All)
            .await
            .unwrap();
        let mut other = Conn::connect(Driver::Postgres, &url).await.unwrap();
        assert!(matches!(
            reset_and_restore(&mut other, &state, Reset::All).await,
            Err(RestoreError::MovedBackend)
        ));
        assert_eq!(
            shown(&mut other, "array_nulls").await,
            "on",
            "no overlay on a replacement backend"
        );
        let mut uninstalled = state.observation();
        let invalid = context(&[("role:no_such_setting_2133", "bad")]);
        assert!(
            uninstalled
                .install_settings(&mut conn, &invalid)
                .await
                .is_err()
        );
        assert!(
            uninstalled.intent.settings.is_none(),
            "failed replay is not an installed empty map"
        );
        let mut broken = state.clone();
        broken.intent.settings = Some(invalid);
        assert!(
            reset_and_restore(&mut conn, &broken, Reset::All)
                .await
                .is_err()
        );
        assert_ne!(
            shown(&mut conn, "TimeZone").await,
            "Asia/Taipei",
            "pins did not run after overlay failure"
        );
        broken.intent.settings = None;
        broken.intent.pins = Some(SessionZone::new("no_such_zone_2133").unwrap());
        assert!(
            reset_and_restore(&mut conn, &broken, Reset::All)
                .await
                .is_err()
        );
        reset_and_restore(&mut conn, &state.observation(), Reset::Discard)
            .await
            .unwrap();
    }
}
