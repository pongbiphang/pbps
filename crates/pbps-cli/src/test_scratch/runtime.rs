//! The container runtime, as far as reconciling a run and finishing one needs
//! it: list by label, inspect by id, remove and confirm. The resolver's own
//! Docker commands do the work (`container::docker`, `remove_with_volumes`), so the
//! removal and its confirmation are the shared scratch path.

use std::collections::BTreeMap;

use pbps_cli::resolver::server::scratch::container::{
    docker, no_such_container, no_such_volume, remove_with_volumes,
};

use super::record::Daemon;

/// Why the runtime could not answer. Absence is never one of these: it is
/// `Ok(None)` or an empty list, said by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Fail {
    Unreadable(String),
    TimedOut(String),
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Unreadable(why) => write!(f, "unreadable: {why}"),
            Fail::TimedOut(why) => write!(f, "timed out: {why}"),
        }
    }
}

fn fail(why: String) -> Fail {
    if why.contains("did not finish") {
        Fail::TimedOut(why)
    } else {
        Fail::Unreadable(why)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Inspected {
    pub id: String,
    pub name: String,
    pub labels: BTreeMap<String, String>,
    /// Named volumes mounted in it.
    pub volumes: Vec<String>,
    /// Mounts that are not named volumes (a bind mount is never ours).
    pub other_mounts: Vec<String>,
}

#[allow(
    async_fn_in_trait,
    reason = "every caller uses the runtime by reference in one function; no Send bound is needed"
)]
pub(super) trait Runtime {
    /// The containers carrying `label` (`key=value`), all states.
    async fn list(&self, label: &str) -> Result<Vec<String>, Fail>;
    /// The container with this id; `None` only when the runtime says there is none.
    async fn inspect(&self, id: &str) -> Result<Option<Inspected>, Fail>;
    /// The ids of the containers that mount `volume`.
    async fn users(&self, volume: &str) -> Result<Vec<String>, Fail>;
    /// Removes a volume the run recorded, if the runtime still has it; the
    /// names still there when it cannot confirm it gone.
    async fn remove_volume(&self, volume: &str) -> Result<(), Vec<String>>;
    /// Removes the container with the volumes already inspected and recorded,
    /// and confirms both gone; the names still there when it cannot.
    async fn remove(&self, id: &str, volumes: &[String]) -> Result<(), Vec<String>>;
}

pub(super) struct Docker;

impl Runtime for Docker {
    async fn list(&self, label: &str) -> Result<Vec<String>, Fail> {
        let listing = docker(&[
            "ps",
            "--all",
            "--no-trunc",
            "--filter",
            &format!("label={label}"),
            "--format",
            "{{.ID}}",
        ])
        .await
        .map_err(fail)?;
        Ok(listing.split_whitespace().map(str::to_owned).collect())
    }

    async fn inspect(&self, id: &str) -> Result<Option<Inspected>, Fail> {
        let json = match docker(&["container", "inspect", "--format", "{{json .}}", id]).await {
            Ok(json) => json,
            Err(why) if no_such_container(&why) => return Ok(None),
            Err(why) => return Err(fail(why)),
        };
        let value: serde_json::Value = serde_json::from_str(json.trim())
            .map_err(|e| Fail::Unreadable(format!("its inspection did not parse: {e}")))?;
        let text = |v: &serde_json::Value| v.as_str().map(str::to_owned);
        let (Some(found), Some(name)) = (text(&value["Id"]), text(&value["Name"])) else {
            return Err(Fail::Unreadable("its inspection has no id or name".into()));
        };
        let labels = value["Config"]["Labels"]
            .as_object()
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|(k, v)| Some((k.clone(), text(v)?)))
                    .collect()
            })
            .unwrap_or_default();
        let (mut volumes, mut other_mounts) = (Vec::new(), Vec::new());
        for mount in value["Mounts"].as_array().into_iter().flatten() {
            match (mount["Type"].as_str(), mount["Name"].as_str()) {
                (Some("volume"), Some(name)) => volumes.push(name.to_owned()),
                (kind, _) => other_mounts.push(kind.unwrap_or("unknown").to_owned()),
            }
        }
        Ok(Some(Inspected {
            id: found,
            name: name.trim_start_matches('/').to_owned(),
            labels,
            volumes,
            other_mounts,
        }))
    }

    async fn users(&self, volume: &str) -> Result<Vec<String>, Fail> {
        let listing = docker(&[
            "ps",
            "--all",
            "--no-trunc",
            "--filter",
            &format!("volume={volume}"),
            "--format",
            "{{.ID}}",
        ])
        .await
        .map_err(fail)?;
        Ok(listing.split_whitespace().map(str::to_owned).collect())
    }

    async fn remove(&self, id: &str, volumes: &[String]) -> Result<(), Vec<String>> {
        remove_with_volumes(id, Ok(volumes.to_vec())).await
    }

    async fn remove_volume(&self, volume: &str) -> Result<(), Vec<String>> {
        match docker(&["volume", "rm", volume]).await {
            Err(why) if !no_such_volume(&why) => {
                Err(vec![format!("the docker volume {volume}: {why}")])
            }
            _ => Ok(()),
        }
    }
}

/// Where the runtime is and which daemon answers there. Podman's reported
/// socket follows its effective connection, including named and default
/// connections that do not change `DOCKER_HOST` (DEC-2459.1).
pub(super) async fn current() -> Result<Daemon, Fail> {
    current_with(
        docker,
        std::env::var("DOCKER_HOST").unwrap_or_else(|_| "default".into()),
    )
    .await
}

pub(super) async fn current_with(
    info: impl AsyncFn(&[&str]) -> Result<String, String>,
    endpoint: String,
) -> Result<Daemon, Fail> {
    let identity = match info(&["info", "--format", "{{.ID}}|{{.DockerRootDir}}"]).await {
        Ok(text) => text,
        // Podman 5.7 rejects `.ID`. Its graph root alone is a path that
        // another host can share, so all three identity parts are required.
        Err(_) => {
            let text = info(&["info", "--format", "{{json .}}"])
                .await
                .map_err(fail)?;
            let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                Fail::Unreadable(format!("the runtime identity did not parse: {e}"))
            })?;
            let part = |path: &str| {
                value
                    .pointer(path)
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .ok_or_else(|| {
                        Fail::Unreadable(format!(
                            "the runtime identity has no nonempty string {path}"
                        ))
                    })
            };
            let hostname = part("/host/hostname")?;
            let socket = part("/host/remoteSocket/path")?;
            let graph_root = part("/store/graphRoot")?;
            return Ok(Daemon {
                endpoint: socket.to_owned(),
                // JSON keeps delimiters in any reported value unambiguous;
                // the tag cannot match an older graph-root-only identity.
                identity: serde_json::to_string(&["podman", hostname, graph_root])
                    .expect("strings serialize"),
            });
        }
    };
    let identity = identity.trim();
    if identity.is_empty() || identity == "|" {
        return Err(Fail::Unreadable("the runtime reports no identity".into()));
    }
    Ok(Daemon {
        endpoint,
        identity: identity.to_owned(),
    })
}
