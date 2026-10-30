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

use pbps_db::resolver::Kept;
use pbps_db::{Conn, Driver};

use super::super::super::engine;
use super::super::recovery::{self, Adapter, Failed, Liveness, Resource};
use super::READY;
use super::{docker, remove_with_volumes, wait_removed};

/// The label carrying the run's token.
pub(super) const TOKEN: &str = "pbps.resolver.token";

/// The label naming the engine the container runs, to read its sessions.
pub(super) const ENGINE: &str = "pbps.resolver.engine";

/// How old a running container with no keeper that never had one must be
/// before it is taken as dead: the run's readiness deadline, and two minutes.
/// Tests sweep under it too: test runs share one daemon with other shards,
/// and a shorter grace removed a loaded SQL Server container that had not yet
/// connected its keeper.
const GRACE: Duration = Duration::from_secs(READY.as_secs() + 120);

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
        let conn = engine::keep_and_mark(driver, connection, token)
            .await
            .map_err(|e| {
                super::ProduceError::Acquire(format!("the scratch container's keeper session: {e}"))
            })?;
        Ok(Self { _conn: conn })
    }
}

/// Reads the keeper of `token` from the server at `connection`, as this run
/// (`own`). `None` when the reader cannot see its own session, which proves
/// nothing.
async fn look(
    driver: Driver,
    connection: &str,
    own: &str,
    token: &str,
) -> Result<Option<Kept>, String> {
    tokio::time::timeout(PROBE, engine::keeper_seen(driver, connection, own, token))
        .await
        .map_err(|_| "the server did not answer in time".to_owned())?
        .map_err(|e| e.to_string())
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
#[derive(Clone)]
struct State {
    id: String,
    volumes: Vec<String>,
    status: String,
    started: String,
    created: String,
    driver: Option<Driver>,
}

/// What the observations say of a candidate's owner. `age` is how old the
/// container is on the daemon's clock, when that could be read.
fn verdict(
    state: &State,
    seen: Option<Result<Option<Kept>, String>>,
    age: Option<i64>,
    grace: Duration,
) -> Liveness {
    let grace = grace.as_secs() as i64;
    match state.status.as_str() {
        // A server that is not running serves no run.
        "exited" | "dead" => Liveness::Dead,
        "removing" => Liveness::Unprovable("its removal has not yet been confirmed".into()),
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

// Validate ownership again at inspection; a listed name can be reused.
fn state_from_json(text: &str, token: &str) -> Result<State, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "unreadable container metadata")?;
    let text = |field: &serde_json::Value| {
        field
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or("unreadable container metadata")
    };
    let labels = &value["Config"]["Labels"];
    // Docker reports the name with a leading slash and Podman without one
    // (SPEC supports both); only that one optional slash is tolerated.
    let name = value["Name"]
        .as_str()
        .map(|n| n.strip_prefix('/').unwrap_or(n));
    if name != Some(format!("pbps-scratch-{token}").as_str())
        || labels["pbps.resolver"] != "scratch"
        || labels[TOKEN] != token
    {
        return Err("its container name and ownership labels do not agree".into());
    }
    let id = text(&value["Id"])?;
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("unreadable container identity".into());
    }
    let mounts = value["Mounts"]
        .as_array()
        .ok_or("its volumes could not be read")?;
    let mut volumes = Vec::new();
    for mount in mounts {
        let kind = text(&mount["Type"])?;
        if kind == "volume" {
            volumes.push(text(&mount["Name"])?);
        }
    }
    Ok(State {
        id,
        volumes,
        status: text(&value["State"]["Status"])?,
        started: text(&value["State"]["StartedAt"])?,
        created: text(&value["Created"])?,
        driver: match labels[ENGINE].as_str() {
            Some("postgres") => Some(Driver::Postgres),
            Some("mssql") => Some(Driver::Mssql),
            _ => None,
        },
    })
}

/// The scratch containers on the runner, other than this run's.
pub(super) struct Containers<'a> {
    own: &'a str,
    grace: Duration,
    /// The one run whose container is judged, when a test confines a sweep
    /// to its own.
    only: Option<&'a str>,
    listed: BTreeMap<String, Result<State, String>>,
}

impl<'a> Containers<'a> {
    pub(super) fn new(own: &'a str) -> Self {
        Self {
            own,
            grace: GRACE,
            only: None,
            listed: BTreeMap::new(),
        }
    }

    async fn state(&self, reference: &str, token: &str) -> Result<State, String> {
        let text = docker(&["container", "inspect", "--format", "{{json .}}", reference]).await?;
        state_from_json(&text, token)
    }

    async fn classify(&self, token: &str, observed: Result<State, String>) -> Liveness {
        let listed = match self.listed.get(token) {
            Some(Ok(state)) => state,
            Some(Err(why)) => {
                return Liveness::Unprovable(format!(
                    "its container identity and volumes could not be read before removal: {why}"
                ));
            }
            None => {
                return Liveness::Unprovable(
                    "its container identity and volumes were not listed".into(),
                );
            }
        };
        let state = match observed {
            Err(why) if super::no_such_container(&why) => return Liveness::Dead,
            Err(why) => return Liveness::Unprovable(why),
            Ok(state) if state.id == listed.id => state,
            Ok(_) => return Liveness::Unprovable("its container identity changed".into()),
        };
        if state.status == "removing" {
            return match wait_removed(&listed.id).await {
                Ok(()) => Liveness::Dead,
                Err(why) => Liveness::Unprovable(why),
            };
        }
        let seen = match (state.status.as_str(), state.driver) {
            ("running", Some(driver)) => Some(match self.connection(&listed.id, driver).await {
                Ok(connection) => look(driver, &connection, self.own, token).await,
                Err(why) => Err(why),
            }),
            ("running", None) => Some(Err("its engine is not labelled".into())),
            _ => None,
        };
        let needs_age = state.status == "created"
            || matches!(&seen, Some(Ok(Some(seen))) if !seen.alive && !seen.ever);
        let age = if needs_age {
            self.age(&listed.id, &state).await
        } else {
            None
        };
        let live = verdict(&state, seen, age, self.grace);
        // A keeper probe can race the same removal. A live keeper always wins;
        // only an unreadable running server needs another runtime observation.
        // A remover can stop it before deletion enters `removing` (DEC-2513.1).
        if state.status == "running" && matches!(live, Liveness::Unprovable(_)) {
            match self.state(&listed.id, token).await {
                Err(why) if super::no_such_container(&why) => return Liveness::Dead,
                Err(why) => return Liveness::Unprovable(why),
                Ok(now) if now.id != listed.id => {
                    return Liveness::Unprovable("its container identity changed".into());
                }
                Ok(now) if matches!(now.status.as_str(), "exited" | "dead") => {
                    return Liveness::Dead;
                }
                Ok(now) if now.status == "removing" => {
                    return match wait_removed(&listed.id).await {
                        Ok(()) => Liveness::Dead,
                        Err(why) => Liveness::Unprovable(why),
                    };
                }
                _ => {}
            }
        }
        live
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
        let mut found = parse_listing(&listing, self.own);
        if let Some(only) = self.only {
            found.retain(|token, _| token == only);
        }
        // Keep immutable identity and volume evidence while the record exists.
        // A later No such container cannot reconstruct the volumes it owned.
        self.listed.clear();
        for token in found.keys() {
            self.listed.insert(
                token.clone(),
                self.state(&format!("pbps-scratch-{token}"), token).await,
            );
        }
        Ok(found)
    }

    async fn liveness(&mut self, tokens: &[String]) -> Result<BTreeMap<String, Liveness>, String> {
        let mut lives = BTreeMap::new();
        for token in tokens {
            let observed = match self.listed.get(token) {
                Some(Ok(state)) => self.state(&state.id, token).await,
                Some(Err(why)) => Err(why.clone()),
                None => Err("its container was not listed".into()),
            };
            let live = self.classify(token, observed).await;
            lives.insert(token.clone(), live);
        }
        Ok(lives)
    }

    async fn remove(&mut self, token: &str, _: &Resource) -> Result<(), Failed> {
        let state = self
            .listed
            .get(token)
            .and_then(|s| s.as_ref().ok())
            .ok_or_else(|| {
                Failed::from("its container identity and volumes were not read".to_owned())
            })?;
        remove_with_volumes(&state.id, Ok(state.volumes.clone()))
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
        .map(|kept| (kept.alive, kept.ever))
}

/// The sweep, with its report returned instead of printed.
#[cfg(test)]
pub(in crate::resolver::server::scratch) async fn swept(own: &str) -> recovery::Report {
    recovery::sweep(&mut Containers::new(own)).await
}

/// The sweep of `token`'s container alone, under `grace`, so that a test can
/// cross a short grace without judging the containers of runs it shares the
/// daemon with.
#[cfg(test)]
pub(in crate::resolver::server::scratch) async fn swept_alone(
    own: &str,
    token: &str,
    grace: Duration,
) -> recovery::Report {
    let mut containers = Containers::new(own);
    containers.grace = grace;
    containers.only = Some(token);
    recovery::sweep(&mut containers).await
}

// Feed a real runtime observation into the normal classifier after listing.
// Keeping it across the scheduling boundary makes runtime transitions repeatable
// even when they finish before classification is polled.
#[cfg(test)]
pub(in crate::resolver::server::scratch) async fn swept_with_observation(
    own: &str,
    token: &str,
    expected_status: &str,
    between: impl AsyncFnOnce(&str) -> String,
) -> recovery::Report {
    struct Listed<'a> {
        inner: Containers<'a>,
        found: BTreeMap<String, Vec<Resource>>,
        observed: Option<State>,
    }
    impl Adapter for Listed<'_> {
        async fn leftovers(&mut self) -> Result<BTreeMap<String, Vec<Resource>>, String> {
            Ok(std::mem::take(&mut self.found))
        }
        async fn liveness(
            &mut self,
            tokens: &[String],
        ) -> Result<BTreeMap<String, Liveness>, String> {
            assert_eq!(tokens.len(), 1);
            Ok([(
                tokens[0].clone(),
                self.inner
                    .classify(&tokens[0], Ok(self.observed.take().unwrap()))
                    .await,
            )]
            .into())
        }
        async fn remove(&mut self, token: &str, resource: &Resource) -> Result<(), Failed> {
            self.inner.remove(token, resource).await
        }
        fn statement(&self, resource: &Resource) -> String {
            self.inner.statement(resource)
        }
    }
    let mut inner = Containers::new(own);
    inner.only = Some(token);
    let found = inner.leftovers().await.unwrap();
    let state = inner.listed[token].as_ref().unwrap();
    let observed = state_from_json(&between(&state.id).await, token).unwrap();
    assert_eq!(
        observed.status, expected_status,
        "the retained observation precedes classification"
    );
    recovery::sweep(&mut Listed {
        inner,
        found,
        observed: Some(observed),
    })
    .await
}

#[cfg(test)]
mod tests {

    #[test]
    fn recovery_metadata_pins_identity_ownership_and_a_readable_volume_list() {
        let valid = serde_json::json!({"Id":"a".repeat(64),"Name":format!("/pbps-scratch-{T}"),"Config":{"Labels":{"pbps.resolver":"scratch",TOKEN:T,ENGINE:"postgres"}},"State":{"Status":"removing","StartedAt":"2026-10-29T00:00:00Z"},"Created":"2026-10-29T00:00:00Z","Mounts":[{"Type":"volume","Name":"owned-volume"}]});
        let state = state_from_json(&valid.to_string(), T).unwrap();
        assert_eq!(state.volumes, ["owned-volume"]);
        let mut podman = valid.clone();
        podman["Name"] = format!("pbps-scratch-{T}").into();
        assert_eq!(
            state_from_json(&podman.to_string(), T).unwrap().volumes,
            ["owned-volume"]
        );
        assert_eq!(
            verdict(&state, None, None, GRACE),
            Liveness::Unprovable("its removal has not yet been confirmed".into())
        );
        for (field, replacement) in [
            ("Name", serde_json::json!("/foreign")),
            (
                "Name",
                serde_json::json!("//pbps-scratch-0123456789abcdef0123456789abcdef"),
            ),
            ("Name", serde_json::json!(format!("/pbps-scratch-{T}/"))),
            ("Name", serde_json::Value::Null),
            ("Id", serde_json::json!("")),
            ("Mounts", serde_json::Value::Null),
            ("Mounts", serde_json::json!([{"Type":"volume"}])),
        ] {
            let mut bad = valid.clone();
            bad[field] = replacement;
            assert!(state_from_json(&bad.to_string(), T).is_err(), "{bad}");
        }
        let mut bad = valid.clone();
        bad["Config"]["Labels"][TOKEN] = "another".into();
        assert!(state_from_json(&bad.to_string(), T).is_err());
        assert!(state_from_json("permission denied", T).is_err());
    }

    #[tokio::test]
    async fn recovery_missing_or_denied_observations_do_not_invent_volume_evidence() {
        let mut containers = Containers::new("reader");
        assert!(matches!(
            containers
                .classify(T, Err("No such container".into()))
                .await,
            Liveness::Unprovable(_)
        ));
        containers.listed.insert(T.into(), Ok(state("removing")));
        assert_eq!(
            containers
                .classify(T, Err("permission denied".into()))
                .await,
            Liveness::Unprovable("permission denied".into())
        );
        containers
            .listed
            .insert(T.into(), Err("permission denied".into()));
        assert!(
            matches!(containers.classify(T, Err("No such container".into())).await, Liveness::Unprovable(why) if why.contains("permission denied"))
        );
    }

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

    #[test]
    fn the_container_module_leaves_engine_sql_to_the_engine_seam() {
        for (file, source) in [
            ("recovery.rs", include_str!("recovery.rs")),
            ("startup.rs", include_str!("startup.rs")),
        ] {
            for krate in ["pbps_pg", "pbps_mssql"] {
                let call = format!("{krate}::");
                assert!(!source.contains(&call), "{file} calls {krate}, not engine");
            }
        }
    }

    fn state(status: &str) -> State {
        State {
            id: "a".repeat(64),
            volumes: Vec::new(),
            status: status.into(),
            started: String::new(),
            created: String::new(),
            driver: Some(Driver::Postgres),
        }
    }

    fn seen(alive: bool, ever: bool) -> Option<Result<Option<Kept>, String>> {
        Some(Ok(Some(Kept { alive, ever })))
    }

    #[test]
    fn what_the_observations_prove_and_what_they_do_not() {
        let old = Some(GRACE.as_secs() as i64 + 1);
        let young = Some(1);
        // A server that is not running serves no run.
        assert_eq!(verdict(&state("exited"), None, None, GRACE), Liveness::Dead);
        assert_eq!(verdict(&state("dead"), None, None, GRACE), Liveness::Dead);
        // Its keeper is there.
        assert_eq!(
            verdict(&state("running"), seen(true, true), old, GRACE),
            Liveness::Alive
        );
        // It connected once and is gone: dead at once, however young.
        assert_eq!(
            verdict(&state("running"), seen(false, true), young, GRACE),
            Liveness::Dead
        );
        // It never connected: presumed its run's until the grace has passed.
        assert_eq!(
            verdict(&state("running"), seen(false, false), young, GRACE),
            Liveness::Alive
        );
        assert_eq!(
            verdict(&state("running"), seen(false, false), old, GRACE),
            Liveness::Dead
        );
        // Its age cannot be read: not judged, and named.
        assert!(matches!(
            verdict(&state("running"), seen(false, false), None, GRACE),
            Liveness::Unprovable(_)
        ));
        // A server this run cannot read, or reads blind, is not judged.
        for unread in [Some(Err("refused".to_owned())), Some(Ok(None)), None] {
            assert!(
                matches!(
                    verdict(&state("running"), unread, old, GRACE),
                    Liveness::Unprovable(_)
                ),
                "unread servers are kept"
            );
        }
        // Never started, past the grace.
        assert_eq!(verdict(&state("created"), None, old, GRACE), Liveness::Dead);
        assert_eq!(
            verdict(&state("created"), None, young, GRACE),
            Liveness::Alive
        );
        // Anything else is not judged.
        assert!(matches!(
            verdict(&state("paused"), seen(false, true), old, GRACE),
            Liveness::Unprovable(_)
        ));
    }
}
