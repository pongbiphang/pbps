//! Docker's side of crash recovery (#2410, DEC-2410.3): a scratch container a
//! killed run left running, found by its label and removed when its owner is
//! provably gone. The `Drop` guard that removes a container on a panic does
//! not run on SIGKILL, and nothing else ever removed it.
//!
//! # The owner is the keeper
//!
//! Each run names its container `pbps-scratch-<T>`, labels it with `T`, and
//! opens one session in its server named `pbps-<T>` (PostgreSQL's
//! `application_name`, SQL Server's `##pbps_<T>` marker table: the reads the
//! engine adapters make) that it holds until the container is removed. The
//! container is alive while that session is there. A server also keeps a
//! persistent mark that the keeper once connected, so a container whose
//! keeper has gone is dead at once, not after a wait.
//!
//! # The one window with no sound answer
//!
//! From `docker run` to the keeper's first connection there is no keeper. A
//! container that is running, answers, and has never had one is presumed to be
//! its run's until it is older than [`GRACE`], measured on the container's own
//! clock, which the daemon's host shares (so there is no cross-machine skew).
//! Past it the run's own deadline for the server to come up has passed twice
//! over, and a live run would have removed the container. A run suspended for
//! longer than that during start fails loudly (its container is gone), and
//! corrupts nothing (D1 of #2410).
//!
//! A container whose server this run cannot reach is not judged: the daemon
//! may be shared by processes that see another loopback.

use std::collections::BTreeMap;
use std::time::Duration;

use pbps_db::{Conn, Driver};

use super::super::recovery::{self, Adapter, Failed, Liveness, Resource};
#[cfg(not(test))]
use super::READY;
use super::{docker, remove_named};

/// The label carrying the run's token.
pub(super) const TOKEN: &str = "pbps.resolver.token";

/// The label naming the engine the container runs, to read its sessions.
pub(super) const ENGINE: &str = "pbps.resolver.engine";

/// The persistent mark a keeper leaves in its server once it has connected.
const KEPT: &str = "pbps-keeper";

/// How old a running container with no keeper that never had one must be
/// before it is taken as dead: the run's readiness deadline, and two minutes.
#[cfg(not(test))]
const GRACE: Duration = Duration::from_secs(READY.as_secs() + 120);
#[cfg(test)]
const GRACE: Duration = Duration::from_secs(4);

/// How long one probe of a candidate's server may take.
const PROBE: Duration = Duration::from_secs(15);

/// The run's session in its own container, held until the container is
/// removed (see the module docs).
pub(in crate::resolver::server::scratch) struct Keeper {
    _conn: Conn,
}

impl Keeper {
    pub(in crate::resolver::server::scratch) async fn open(
        driver: Driver,
        connection: &str,
        token: &str,
    ) -> Result<Self, super::ProduceError> {
        let fail = |why: String| {
            super::ProduceError::Acquire(format!("the scratch container's keeper session: {why}"))
        };
        let mut conn = Conn::connect_named(driver, connection, &format!("pbps-{token}"))
            .await
            .map_err(|e| fail(e.to_string()))?;
        match driver {
            Driver::Postgres => {
                pbps_pg::resolver::scratch::keep(&mut conn)
                    .await
                    .map_err(|e| fail(e.to_string()))?;
                conn.execute(&format!("COMMENT ON DATABASE postgres IS '{KEPT}'"))
                    .await
            }
            Driver::Mssql => {
                pbps_mssql::resolver::scratch::mark(&mut conn, token)
                    .await
                    .map_err(|e| fail(e.to_string()))?;
                conn.execute(&format!(
                    "EXEC master.sys.sp_addextendedproperty @name = N'{KEPT}', @value = N'{token}';"
                ))
                .await
            }
        }
        .map_err(|e| fail(e.to_string()))?;
        Ok(Self { _conn: conn })
    }
}

/// What a candidate's server says of its keeper.
struct Seen {
    /// The keeper's session is there.
    alive: bool,
    /// It connected once.
    ever: bool,
}

/// Reads the keeper of `token` from the server at `connection`, as this run
/// (`own`) and through the reads the engine adapters make. `None` when the
/// reader cannot see its own session, which proves nothing.
async fn look(
    driver: Driver,
    connection: &str,
    own: &str,
    token: &str,
) -> Result<Option<Seen>, String> {
    let read = async {
        let db = |e: pbps_db::DbError| e.to_string();
        match driver {
            Driver::Postgres => {
                let mut conn = Conn::connect_named(driver, connection, &format!("pbps-{own}"))
                    .await
                    .map_err(db)?;
                let up = pbps_pg::resolver::recovery::alive(&mut conn, own, &[token.to_owned()])
                    .await
                    .map_err(db)?;
                let marks = conn
                    .query(
                        "SELECT pg_catalog.shobj_description(oid, 'pg_database') AS comment \
                           FROM pg_catalog.pg_database WHERE datname = 'postgres'",
                    )
                    .await
                    .map_err(db)?;
                let ever = marks
                    .first()
                    .is_some_and(|row| row.try_get::<&str>("comment").ok().flatten() == Some(KEPT));
                Ok(up.map(|up| Seen {
                    alive: up.contains(token),
                    ever,
                }))
            }
            Driver::Mssql => {
                let mut conn = Conn::connect(driver, connection).await.map_err(db)?;
                pbps_mssql::resolver::scratch::mark(&mut conn, own)
                    .await
                    .map_err(db)?;
                // The marker table `mark` makes, read by name; a reader that
                // cannot see its own proves nothing.
                let up = conn
                    .query(&format!(
                        "SELECT CASE WHEN OBJECT_ID(N'tempdb..##pbps_{own}') IS NULL THEN 0 ELSE 1 \
                         END AS own, CASE WHEN OBJECT_ID(N'tempdb..##pbps_{token}') IS NULL THEN 0 \
                         ELSE 1 END AS keeper;"
                    ))
                    .await
                    .map_err(db)?;
                let up = up.first().and_then(|row| {
                    (row.try_get::<i32>("own").ok().flatten() == Some(1))
                        .then(|| row.try_get::<i32>("keeper").ok().flatten() == Some(1))
                });
                let marks = conn
                    .query(&format!(
                        "SELECT 1 AS there FROM master.sys.extended_properties WHERE class = 0 \
                         AND name = N'{KEPT}'"
                    ))
                    .await
                    .map_err(db)?;
                Ok(up.map(|alive| Seen {
                    alive,
                    ever: !marks.is_empty(),
                }))
            }
        }
    };
    tokio::time::timeout(PROBE, read)
        .await
        .unwrap_or_else(|_| Err("the server did not answer in time".into()))
}

/// Seconds since the epoch of an RFC 3339 time as the daemon prints it
/// (`2026-10-26T02:42:28.675851965Z`, or with an offset).
fn epoch(text: &str) -> Option<i64> {
    let (date, rest) = text.trim().split_once('T')?;
    let mut d = date.split('-').map(|n| n.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let split = rest.find(['Z', '+', '-'])?;
    let (clock, zone) = rest.split_at(split);
    let mut c = clock.split(':');
    let (h, mi) = (
        c.next()?.parse::<i64>().ok()?,
        c.next()?.parse::<i64>().ok()?,
    );
    let s = c.next()?.split('.').next()?.parse::<i64>().ok()?;
    let offset = match zone.as_bytes().first()? {
        b'Z' => 0,
        sign => {
            let (oh, om) = zone[1..].split_once(':')?;
            let seconds = oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60;
            if *sign == b'+' { seconds } else { -seconds }
        }
    };
    // Days from civil (Howard Hinnant), so no date crate is needed.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s - offset)
}

/// The candidates' state as the runtime reports it.
struct State {
    status: String,
    started: String,
    created: String,
    driver: Option<Driver>,
}

/// What the observations say of a candidate's owner. `age` is how old the
/// container is on the daemon's clock, when that could be read.
fn verdict(
    state: &State,
    seen: Option<Result<Option<Seen>, String>>,
    age: Option<i64>,
) -> Liveness {
    let grace = GRACE.as_secs() as i64;
    match state.status.as_str() {
        // A server that is not running serves no run.
        "exited" | "dead" => Liveness::Dead,
        "created" => match age {
            Some(age) if age > grace => Liveness::Dead,
            Some(_) => Liveness::Alive,
            None => Liveness::Unprovable("its age could not be read".into()),
        },
        "running" => match seen {
            None | Some(Err(_)) | Some(Ok(None)) => Liveness::Unprovable(match seen {
                Some(Err(why)) => format!("its server could not be read: {why}"),
                Some(Ok(None)) => {
                    "this session cannot see its own run among the server's sessions".into()
                }
                _ => "its server was not read".into(),
            }),
            Some(Ok(Some(seen))) if seen.alive => Liveness::Alive,
            // It connected once and is gone.
            Some(Ok(Some(seen))) if seen.ever => Liveness::Dead,
            // Never connected: its run may not have yet.
            Some(Ok(Some(_))) => match age {
                Some(age) if age > grace => Liveness::Dead,
                Some(_) => Liveness::Alive,
                None => Liveness::Unprovable("its age could not be read".into()),
            },
        },
        other => Liveness::Unprovable(format!("it is {other}")),
    }
}

/// The scratch containers on the runner, other than this run's.
pub(super) struct Containers<'a> {
    own: &'a str,
}

impl<'a> Containers<'a> {
    pub(super) fn new(own: &'a str) -> Self {
        Self { own }
    }

    async fn state(&self, name: &str) -> Result<State, String> {
        let text = docker(&[
            "container",
            "inspect",
            "--format",
            &format!(
                "{{{{.State.Status}}}}|{{{{.State.StartedAt}}}}|{{{{.Created}}}}|{{{{index .Config.Labels \"{ENGINE}\"}}}}"
            ),
            name,
        ])
        .await?;
        let mut parts = text.trim().splitn(4, '|');
        let mut next = || parts.next().unwrap_or_default().to_owned();
        Ok(State {
            status: next(),
            started: next(),
            created: next(),
            driver: match next().as_str() {
                "postgres" => Some(Driver::Postgres),
                "mssql" => Some(Driver::Mssql),
                _ => None,
            },
        })
    }

    /// The connection to a candidate's server, from what the runtime holds of
    /// it: the published loopback port and the password its run generated.
    async fn connection(&self, name: &str, driver: Driver) -> Result<String, String> {
        let env = docker(&[
            "container",
            "inspect",
            "--format",
            "{{range .Config.Env}}{{println .}}{{end}}",
            name,
        ])
        .await?;
        let variable = match driver {
            Driver::Postgres => "POSTGRES_PASSWORD=",
            Driver::Mssql => "MSSQL_SA_PASSWORD=",
        };
        let password = env
            .lines()
            .find_map(|line| line.strip_prefix(variable))
            .ok_or("its password is not in its environment")?;
        let port = match driver {
            Driver::Postgres => 5432,
            Driver::Mssql => 1433,
        };
        let published = docker(&["port", name, &format!("{port}/tcp")]).await?;
        let port = super::published_port(&published, port)?;
        Ok(match driver {
            Driver::Postgres => format!(
                "host=127.0.0.1 port={port} user=postgres password={password} dbname=postgres \
                 sslmode=disable"
            ),
            Driver::Mssql => format!(
                "Server=127.0.0.1,{port};Database=master;User Id=sa;Password={password};\
                 TrustServerCertificate=true"
            ),
        })
    }

    /// How old a candidate is: the daemon's clock for a container that was
    /// never started, and the container's own for one that runs, which is the
    /// daemon host's.
    async fn age(&self, name: &str, state: &State) -> Option<i64> {
        if state.status == "created" {
            let now = docker(&["info", "--format", "{{.SystemTime}}"])
                .await
                .ok()?;
            return Some(epoch(&now)? - epoch(&state.created)?);
        }
        let now = docker(&["exec", name, "date", "+%s"]).await.ok()?;
        Some(now.trim().parse::<i64>().ok()? - epoch(&state.started)?)
    }
}

impl Adapter for Containers<'_> {
    async fn leftovers(&mut self) -> Result<BTreeMap<String, Vec<Resource>>, String> {
        let listing = docker(&[
            "ps",
            "--all",
            "--no-trunc",
            "--filter",
            "label=pbps.resolver=scratch",
            "--format",
            &format!("{{{{.Names}}}} {{{{.Label \"{TOKEN}\"}}}}"),
        ])
        .await?;
        Ok(parse_listing(&listing, self.own))
    }

    async fn liveness(&mut self, tokens: &[String]) -> Result<BTreeMap<String, Liveness>, String> {
        let mut lives = BTreeMap::new();
        for token in tokens {
            let name = format!("pbps-scratch-{token}");
            let live = match self.state(&name).await {
                // Removed since it was listed: nothing left to decide.
                Err(why) if super::no_such_container(&why) => Liveness::Dead,
                Err(why) => Liveness::Unprovable(why),
                Ok(state) => {
                    let seen = match (state.status.as_str(), state.driver) {
                        ("running", Some(driver)) => {
                            Some(match self.connection(&name, driver).await {
                                Ok(connection) => look(driver, &connection, self.own, token).await,
                                Err(why) => Err(why),
                            })
                        }
                        ("running", None) => Some(Err("its engine is not labelled".into())),
                        _ => None,
                    };
                    let needs_age = state.status == "created"
                        || matches!(&seen, Some(Ok(Some(seen))) if !seen.alive && !seen.ever);
                    let age = if needs_age {
                        self.age(&name, &state).await
                    } else {
                        None
                    };
                    verdict(&state, seen, age)
                }
            };
            lives.insert(token.clone(), live);
        }
        Ok(lives)
    }

    async fn remove(&mut self, resource: &Resource) -> Result<(), Failed> {
        remove_named(&resource.names[0])
            .await
            .map_err(|left| left.join("; ").into())
    }

    fn statement(&self, resource: &Resource) -> String {
        format!("docker rm --force --volumes {}", resource.names[0])
    }
}

/// The containers listed, `{name} {token}` a line, that are a run's: labelled
/// as scratch, named for their own token, which is not `own`. A container
/// without a token, or named otherwise, is not this tool's to judge.
fn parse_listing(listing: &str, own: &str) -> BTreeMap<String, Vec<Resource>> {
    listing
        .lines()
        .filter_map(|line| {
            let (name, token) = line.split_once(' ')?;
            let token = token.trim();
            (token.len() == 32
                && token
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && token != own
                && name == format!("pbps-scratch-{token}"))
            .then(|| {
                (
                    token.to_owned(),
                    vec![Resource::one("docker container", name)],
                )
            })
        })
        .collect()
}

/// Removes what runs killed earlier left, before this run starts a container.
/// Never fails the run for what it cannot remove (D4 of #2410).
pub(super) async fn sweep(own: &str) {
    recovery::emit(&recovery::sweep(&mut Containers::new(own)).await);
}

/// What a server shows of a keeper, as `(its session is there, it connected
/// once)`, for the tests that read each engine's marks.
#[cfg(test)]
pub(in crate::resolver::server::scratch) async fn seen(
    driver: Driver,
    connection: &str,
    own: &str,
    token: &str,
) -> Option<(bool, bool)> {
    look(driver, connection, own, token)
        .await
        .unwrap()
        .map(|seen| (seen.alive, seen.ever))
}

/// The sweep, with its report returned instead of printed.
#[cfg(test)]
pub(in crate::resolver::server::scratch) async fn swept(own: &str) -> recovery::Report {
    recovery::sweep(&mut Containers::new(own)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn the_daemons_times_are_read_with_or_without_an_offset() {
        assert_eq!(epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch("2026-10-26T02:42:28.675851965Z"), Some(1_792_982_548));
        assert_eq!(
            epoch("2026-10-26T10:42:28.675851965+08:00"),
            epoch("2026-10-26T02:42:28Z")
        );
        assert_eq!(
            epoch("2026-10-26T02:42:28-02:30"),
            Some(1_792_982_548 + 9000)
        );
        assert!(epoch("0001-01-01T00:00:00Z").is_some());
        for bad in ["", "not a time", "2026-10-26", "2026-10-26T02:42"] {
            assert_eq!(epoch(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_a_container_named_for_its_own_token_is_a_runs() {
        let listing = format!(
            "pbps-scratch-{T} {T}\n\
             pbps-scratch-{T} \n\
             pbps-scratch-0000000000000000 {T}\n\
             foreign {T}\n\
             pbps-scratch-{o} {o}\n\
             pbps-scratch-{u} {u}\n",
            o = "f".repeat(32),
            u = T.to_uppercase()
        );
        let found = parse_listing(&listing, &"f".repeat(32));
        assert_eq!(found.keys().collect::<Vec<_>>(), [T], "{found:?}");
        assert!(
            !parse_listing(&listing, T).contains_key(T),
            "its own is never a leftover"
        );
    }

    fn state(status: &str) -> State {
        State {
            status: status.into(),
            started: String::new(),
            created: String::new(),
            driver: Some(Driver::Postgres),
        }
    }

    fn seen(alive: bool, ever: bool) -> Option<Result<Option<Seen>, String>> {
        Some(Ok(Some(Seen { alive, ever })))
    }

    #[test]
    fn what_the_observations_prove_and_what_they_do_not() {
        let old = Some(GRACE.as_secs() as i64 + 1);
        let young = Some(1);
        // A server that is not running serves no run.
        assert_eq!(verdict(&state("exited"), None, None), Liveness::Dead);
        assert_eq!(verdict(&state("dead"), None, None), Liveness::Dead);
        // Its keeper is there.
        assert_eq!(
            verdict(&state("running"), seen(true, true), old),
            Liveness::Alive
        );
        // It connected once and is gone: dead at once, however young.
        assert_eq!(
            verdict(&state("running"), seen(false, true), young),
            Liveness::Dead
        );
        // It never connected: presumed its run's until the grace has passed.
        assert_eq!(
            verdict(&state("running"), seen(false, false), young),
            Liveness::Alive
        );
        assert_eq!(
            verdict(&state("running"), seen(false, false), old),
            Liveness::Dead
        );
        // Its age cannot be read: not judged, and named.
        assert!(matches!(
            verdict(&state("running"), seen(false, false), None),
            Liveness::Unprovable(_)
        ));
        // A server this run cannot read, or reads blind, is not judged.
        for unread in [Some(Err("refused".to_owned())), Some(Ok(None)), None] {
            assert!(
                matches!(
                    verdict(&state("running"), unread, old),
                    Liveness::Unprovable(_)
                ),
                "unread servers are kept"
            );
        }
        // Never started, past the grace.
        assert_eq!(verdict(&state("created"), None, old), Liveness::Dead);
        assert_eq!(verdict(&state("created"), None, young), Liveness::Alive);
        // Anything else is not judged.
        assert!(matches!(
            verdict(&state("paused"), seen(false, true), old),
            Liveness::Unprovable(_)
        ));
    }
}
