//! A scratch container's keeper, per engine (#2410, DEC-2410.3): the session a
//! run holds in its own server, the persistent mark that it connected, and the
//! reads that tell whether it is still there. The SQL is the engine crates'.

use super::container::ContainerSupport;
use pbps_db::resolver::Kept;
use pbps_db::{Conn, DbError};

/// The keeper's session in its own server, named by the run's token, and the
/// mark that it connected. The mark is written once the session exists. The
/// caller holds the session as long as the container lives.
pub(in crate::resolver::server) async fn keep_and_mark(
    support: &ContainerSupport,
    connection: &str,
    token: &str,
) -> Result<Conn, DbError> {
    let mut conn =
        Conn::connect_named(support.driver, connection, &format!("pbps-{token}")).await?;
    (support.hold_keeper)(&mut conn, token).await?;
    Ok(conn)
}

/// What the server at `connection` shows of the keeper of `token`, read by a
/// session of its own named for `own`; `None` when that session cannot see
/// itself, which proves nothing about the keeper. Each read takes the mark
/// first and the session second: a keeper that appears between them is then
/// alive and not yet kept, never gone and kept. The order is the engine's own
/// read, written beside its SQL in [`ContainerSupport`].
pub(in crate::resolver::server) async fn keeper_seen(
    support: &ContainerSupport,
    connection: &str,
    own: &str,
    token: &str,
) -> Result<Option<Kept>, DbError> {
    (support.read_keeper)(connection, own, token).await
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
pub(super) async fn between_reads(token: &str) {
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
pub(super) async fn between_reads(_: &str) {}
