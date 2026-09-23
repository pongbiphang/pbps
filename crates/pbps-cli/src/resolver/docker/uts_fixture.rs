//! Change only the UTS namespace of an explicitly owned test container.

use super::{API, LocalApi};
use crate::resolver::native::ProcessLease;
use hyper::{Method, StatusCode};
use serde::Deserialize;
use std::process::Command;

#[derive(Deserialize)]
struct Names {
    hostname: String,
    domainname: String,
}

pub(crate) struct ChangedUts {
    socket: String,
    container: String,
    pid: u32,
    start: String,
    original: Option<Names>,
}

impl ChangedUts {
    pub(crate) async fn capture(api: &mut LocalApi, name: &str) -> Self {
        assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
        let (status, body) = api
            .request(Method::GET, &format!("{API}/containers/{name}/json"))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        let record: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let container = record["Id"].as_str().unwrap().to_owned();
        assert!(container == name || record["Name"] == format!("/{name}"));
        let pid: u32 = record["State"]["Pid"].as_u64().unwrap().try_into().unwrap();
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
            socket: api.socket_path.to_str().unwrap().to_owned(),
            container,
            pid,
            start,
            original: None,
        };
        let original = changed.change("read", "").unwrap();
        assert_eq!(original.hostname, "pbps-resolver");
        assert!(["", "(none)", "localdomain"].contains(&original.domainname.as_str()));
        changed.original = Some(original);
        changed
    }

    fn change(&self, action: &str, value: &str) -> Result<Names, String> {
        let output = Command::new("/usr/bin/python3")
            .arg("-c")
            .arg(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../scripts/resolver-uts-fixture.py"
            )))
            .args([
                self.socket.as_str(),
                &self.container,
                &self.pid.to_string(),
                &self.start,
                action,
                value,
            ])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "owned UTS helper: {} {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())
    }

    pub(crate) fn replace(&self, field: &str, value: &str) {
        let names = self.change(field, value).unwrap();
        match field {
            "hostname" => assert_eq!(names.hostname, value),
            "domainname" => assert_eq!(names.domainname, value),
            _ => panic!("only UTS names may be changed"),
        }
    }

    pub(crate) async fn restore_or_confirm_removed(&self, api: &mut LocalApi) {
        let original = self.original.as_ref().unwrap();
        if self.change("hostname", &original.hostname).is_ok()
            && self.change("domainname", &original.domainname).is_ok()
        {
            return;
        }
        // Discard can already have stopped this owned forwarder. An exited
        // view needs no restoration; a live unreadable one is still a failure.
        let (status, body) = api
            .request(
                Method::GET,
                &format!("{API}/containers/{}/json", self.container),
            )
            .await
            .unwrap();
        if status == StatusCode::NOT_FOUND {
            return;
        }
        assert_eq!(status, StatusCode::OK);
        let record: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(record["Id"], self.container);
        assert_eq!(record["State"]["Running"], false);
    }

    pub(crate) fn restore(&self) {
        let original = self.original.as_ref().unwrap();
        self.replace("hostname", &original.hostname);
        self.replace("domainname", &original.domainname);
    }
}

impl Drop for ChangedUts {
    fn drop(&mut self) {
        if let Some(original) = &self.original {
            let _ = self.change("hostname", &original.hostname);
            let _ = self.change("domainname", &original.domainname);
        }
    }
}
