//! Substitute only a positively identified run-owned filesystem view.

use super::{API, LocalApi};
use crate::resolver::native::ProcessLease;
use hyper::{Method, StatusCode};
use serde::Deserialize;
use std::process::Command;

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

pub(crate) struct ChangedPseudo {
    socket: String,
    container: String,
    pid: u32,
    start: String,
    kind: &'static str,
    original: Option<Identity>,
}

impl ChangedPseudo {
    pub(crate) async fn capture(api: &mut LocalApi, name: &str, kind: &'static str) -> Self {
        assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
        let (status, body) = api
            .request(Method::GET, &format!("{API}/containers/{name}/json"))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        let record: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let container = record["Id"].as_str().unwrap();
        assert!(container == name || record["Name"] == format!("/{name}"));
        let pid = record["State"]["Pid"].as_u64().unwrap().try_into().unwrap();
        Self::from_pid(api.socket_path.to_str().unwrap(), container, pid, kind)
    }

    pub(crate) fn from_pid(socket: &str, container: &str, pid: u32, kind: &'static str) -> Self {
        assert!(["cgroup", "mqueue"].contains(&kind));
        let process = ProcessLease::capture(pid).unwrap();
        let stat = process.read_proc("stat", 65536).unwrap();
        let start = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .to_owned();
        let mut changed = Self {
            socket: socket.to_owned(),
            container: container.to_owned(),
            pid,
            start,
            kind,
            original: None,
        };
        changed.original = Some(changed.change("read").unwrap());
        changed
    }

    fn change(&self, action: &str) -> Result<Identity, String> {
        let output = Command::new("/usr/bin/python3")
            .arg("-c")
            .arg(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../scripts/resolver-pseudo-fixture.py"
            )))
            .args([
                self.socket.as_str(),
                &self.container,
                &self.pid.to_string(),
                &self.start,
                self.kind,
                action,
            ])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "owned pseudo-filesystem helper: {} {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())
    }

    pub(crate) fn replace(&self) {
        assert_ne!(Some(self.change("replace").unwrap()), self.original);
    }

    pub(crate) fn restore(&self) {
        assert_eq!(Some(self.change("restore").unwrap()), self.original);
    }
}

impl Drop for ChangedPseudo {
    fn drop(&mut self) {
        if self.original.is_some() {
            let _ = self.change("restore");
        }
    }
}
