//! A client can be gone before PostgreSQL stops counting its backend (#2245).
use super::*;
use std::time::Duration;

// The witness releases its blocked old backend only after the real server
// rejects a reconnect. Elapsed sleep cannot prove that the race occurred.
tokio::task_local! {
    static RETRIES: tokio::sync::mpsc::UnboundedSender<()>;
}

pub(super) async fn connect(server: &str) -> Conn {
    retry(server, Duration::from_secs(5))
        .await
        .expect("fixture reconnect did not finish within five seconds")
        .unwrap()
}

async fn retry(
    server: &str,
    limit: Duration,
) -> Result<Result<Conn, pbps_db::DbError>, tokio::time::error::Elapsed> {
    tokio::time::timeout(limit, async {
        loop {
            let answer = Conn::connect(Driver::Postgres, server).await;
            if answer
                .as_ref()
                .is_err_and(|error| error.server_error_code().as_deref() == Some("53300"))
            {
                let _ = RETRIES.try_with(|events| events.send(()));
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            // Authentication, configuration and transport failures are not
            // evidence that an earlier fixture connection is still exiting.
            return answer;
        }
    })
    .await
}

async fn backend_is_waiting(admin: &mut Conn, pid: i64) -> bool {
    !admin
        .query(&format!(
            "SELECT 1 FROM pg_catalog.pg_stat_activity \
             WHERE pid = {pid} AND wait_event_type = 'Lock'"
        ))
        .await
        .unwrap()
        .is_empty()
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn observation_reconnect_waits_for_the_old_backend_to_exit() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let (login, database) = fixture.confined().await;
        let url = fixture.as_login(&database, &login);
        let mut first = Conn::connect(Driver::Postgres, &url).await.unwrap();
        let expected = pbps_pg::resolver::standard::verify(&mut first, &Standard::default(), None)
            .await
            .unwrap();
        let pid = first
            .query("SELECT pg_backend_pid()::bigint AS pid")
            .await
            .unwrap()[0]
            .try_get::<i64>("pid")
            .unwrap()
            .unwrap();
        first
            .execute("SET client_connection_check_interval = 0")
            .await
            .unwrap();
        // Advisory locks are database-local, so the controlling connection
        // enters the same database as the confined login.
        let mut admin = Conn::connect(Driver::Postgres, &fixture.on(&database))
            .await
            .unwrap();
        let key = rand::random::<i64>();
        admin
            .execute(&format!("SELECT pg_advisory_lock({key}::bigint)"))
            .await
            .unwrap();
        let task = tokio::spawn(async move {
            first
                .execute(&format!("SELECT pg_advisory_lock({key}::bigint)"))
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !backend_is_waiting(&mut admin, pid).await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("old backend never reached the controlled lock wait");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(backend_is_waiting(&mut admin, pid).await);

        let (events, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let standard = Standard::default();
        let reconnect = RETRIES.scope(
            events,
            not_standard(&fixture, &database, &login, &standard, None),
        );
        let release = async {
            tokio::time::timeout(Duration::from_secs(5), observed.recv())
                .await
                .expect("the real 53300 denial was not observed")
                .expect("reconnect ended without observing the occupied role");
            assert!(backend_is_waiting(&mut admin, pid).await);
            admin
                .execute(&format!("SELECT pg_advisory_unlock({key}::bigint)"))
                .await
                .unwrap();
        };
        let (actual, ()) = tokio::join!(reconnect, release);
        drop(admin);
        fixture.drop().await;
        assert_eq!(
            actual, expected,
            "{server}: the ordinary observation completed"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn reconnect_is_bounded_when_the_prior_connection_never_exits() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let (login, database) = fixture.confined().await;
        let url = fixture.as_login(&database, &login);
        let first = Conn::connect(Driver::Postgres, &url).await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            retry(&url, Duration::from_millis(100)),
        )
        .await
        .expect("the bounded retry must finish while the prior backend remains");
        drop(first);
        fixture.drop().await;
        assert!(result.is_err(), "{server}: an occupied role must time out");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn reconnect_preserves_authentication_failures() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let (login, database) = fixture.confined().await;
        let wrong = (login.0.clone(), "not-the-fixture-password".to_owned());
        let result = retry(&fixture.as_login(&database, &wrong), Duration::from_secs(5)).await;
        fixture.drop().await;
        let Err(error) = result.expect("an authentication failure must not be retried") else {
            panic!("{server}: the wrong password was accepted")
        };
        assert_eq!(
            error.server_error_code().as_deref(),
            Some("28P01"),
            "{server}"
        );
    }
}
