//! A scratch container's keeper, per engine (#2410, DEC-2410.3): the session a
//! run holds in its own server, the persistent mark that it connected, and the
//! reads that tell whether it is still there. The SQL is the engine crates'.

use pbps_db::resolver::Kept;
use pbps_db::{Conn, DbError, Driver};

/// The keeper's session in its own server, named by the run's token, and the
/// mark that it connected. The mark is written once the session exists. The
/// caller holds the session as long as the container lives.
pub(in crate::resolver::server) async fn keep_and_mark(
    driver: Driver,
    connection: &str,
    token: &str,
) -> Result<Conn, DbError> {
    let mut conn = Conn::connect_named(driver, connection, &format!("pbps-{token}")).await?;
    match driver {
        Driver::Postgres => {
            pbps_pg::resolver::scratch::keep(&mut conn).await?;
            pbps_pg::resolver::recovery::mark_kept(&mut conn).await?;
        }
        Driver::Mssql => {
            pbps_mssql::resolver::scratch::mark(&mut conn, token).await?;
            pbps_mssql::resolver::scratch::mark_kept(&mut conn, token).await?;
        }
    }
    Ok(conn)
}

/// What the server at `connection` shows of the keeper of `token`, read by a
/// session of its own named for `own`; `None` when that session cannot see
/// itself, which proves nothing about the keeper. Each read takes the mark
/// first and the session second: a keeper that appears between them is then
/// alive and not yet kept, never gone and kept.
pub(in crate::resolver::server) async fn keeper_seen(
    driver: Driver,
    connection: &str,
    own: &str,
    token: &str,
) -> Result<Option<Kept>, DbError> {
    match driver {
        Driver::Postgres => {
            let mut conn = Conn::connect_named(driver, connection, &format!("pbps-{own}")).await?;
            let ever = pbps_pg::resolver::recovery::kept(&mut conn).await?;
            between_reads(token).await;
            let alive =
                pbps_pg::resolver::recovery::alive(&mut conn, own, &[token.to_owned()]).await?;
            Ok(alive.map(|alive| Kept {
                alive: alive.contains(token),
                ever,
            }))
        }
        Driver::Mssql => {
            use pbps_mssql::resolver::scratch as sql;
            let mut conn = Conn::connect(driver, connection).await?;
            sql::mark(&mut conn, own).await?;
            if !sql::marker_present(&mut conn, own).await? {
                return Ok(None);
            }
            let ever = sql::kept(&mut conn).await?;
            between_reads(token).await;
            Ok(Some(Kept {
                alive: sql::marker_present(&mut conn, token).await?,
                ever,
            }))
        }
    }
}

/// A reader of `token`'s keeper held between its two reads, for the test that
/// opens the keeper there: its token, how it says it is held, what it waits for.
#[cfg(test)]
type Pause = (
    String,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

#[cfg(test)]
static PAUSE: std::sync::Mutex<Option<Pause>> = std::sync::Mutex::new(None);

/// Arms a pause for the next read of `token`'s keeper: the reader says it is
/// between its reads on the first channel and waits for the second.
#[cfg(test)]
pub(in crate::resolver::server) fn pause_between_reads(
    token: &str,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (paused, between) = tokio::sync::oneshot::channel();
    let (go, wait) = tokio::sync::oneshot::channel();
    *PAUSE.lock().unwrap() = Some((token.to_owned(), paused, wait));
    (between, go)
}

#[cfg(test)]
async fn between_reads(token: &str) {
    let armed = {
        let mut pause = PAUSE.lock().unwrap();
        pause.take_if(|(armed, ..)| armed == token)
    };
    if let Some((_, paused, wait)) = armed {
        let _ = paused.send(());
        let _ = wait.await;
    }
}

#[cfg(not(test))]
async fn between_reads(_: &str) {}
