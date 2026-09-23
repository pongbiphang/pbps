//! Change only an explicitly selected owned fixture's runtime file.

use super::{API, LocalApi};
use crate::resolver::native::ProcessLease;
use hyper::{Method, StatusCode};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

pub(crate) struct ChangedFile {
    source: Source,
    original: String,
}

enum Source {
    Generated(PathBuf),
    Image {
        socket: String,
        container: String,
        pid: u32,
        start: String,
        token: String,
        path: String,
    },
}

impl ChangedFile {
    pub(crate) async fn capture(api: &mut LocalApi, container: &str, path: &str) -> Self {
        assert_eq!(container.len(), 64);
        assert!(container.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let key = match path {
            "/etc/resolv.conf" => "ResolvConfPath",
            "/etc/hosts" => "HostsPath",
            "/etc/hostname" => "HostnamePath",
            _ => panic!("only the three fixture runtime files may be changed"),
        };
        let (status, body) = api
            .request(Method::GET, &format!("{API}/containers/{container}/json"))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        let record: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(record["Id"], container);
        let pid: u32 = record["State"]["Pid"].as_u64().unwrap().try_into().unwrap();
        let process = ProcessLease::capture(pid).unwrap();
        let original = process
            .read_root_file(path.trim_start_matches('/'), 4096)
            .unwrap();
        let generated = record[key].as_str().unwrap();
        let source = if generated.is_empty() {
            // NetworkDisabled=true leaves image files instead of daemon binds.
            // A private overmount must never edit a shared image layer.
            let stat = process.read_proc("stat", 65536).unwrap();
            let start = stat
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .nth(19)
                .unwrap()
                .to_owned();
            Source::Image {
                socket: api.socket_path.to_str().unwrap().to_owned(),
                container: container.to_owned(),
                pid,
                start,
                token: format!("{:032x}", rand::random::<u128>()),
                path: path.to_owned(),
            }
        } else {
            let source = PathBuf::from(generated);
            let owner = record["HostConfig"]["NetworkMode"]
                .as_str()
                .unwrap()
                .strip_prefix("container:")
                .unwrap_or(container);
            assert!(source.is_absolute());
            assert_eq!(source.parent().unwrap().file_name().unwrap(), owner);
            assert_eq!(
                source.file_name().unwrap(),
                path.rsplit('/').next().unwrap()
            );
            assert!(std::fs::symlink_metadata(&source).unwrap().is_file());
            assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
            Source::Generated(source)
        };
        Self { source, original }
    }

    pub(crate) fn original(&self) -> &str {
        &self.original
    }

    fn change(&self, text: &str, restore: bool) -> Result<(), String> {
        match &self.source {
            Source::Generated(path) => {
                std::fs::write(path, text).map_err(|error| error.to_string())
            }
            Source::Image {
                socket,
                container,
                pid,
                start,
                token,
                path,
            } => {
                let status = Command::new("/usr/bin/python3")
                    .arg("-c")
                    .arg(include_str!(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/../../scripts/resolver-host-file-fixture.py"
                    )))
                    .args([
                        socket.as_str(),
                        container.as_str(),
                        &pid.to_string(),
                        start,
                        token,
                        path,
                        if restore { "restore" } else { "apply" },
                        text,
                    ])
                    .status()
                    .map_err(|error| error.to_string())?;
                if status.success() {
                    Ok(())
                } else {
                    Err(format!("owned file helper: {status}"))
                }
            }
        }
    }

    pub(crate) fn replace(&self, text: &str) {
        self.change(text, false).unwrap();
    }
    pub(crate) fn restore(&self) {
        self.change(&self.original, true).unwrap();
    }
}

impl Drop for ChangedFile {
    fn drop(&mut self) {
        let _ = self.change(&self.original, true);
    }
}

pub(crate) fn mutations<'a>(path: &'a str, original: &str) -> Vec<(&'a str, String)> {
    match path {
        "/etc/resolv.conf" => vec![
            ("dns-address", "nameserver 192.0.2.53\n".to_owned()),
            (
                "search-domain",
                format!("{original}search pbps-host-search.invalid\n"),
            ),
            (
                "trailing-comment",
                format!("{original}# pbps-host-comment.invalid\n"),
            ),
            (
                "alternate-layout",
                "# pbps-host-comment.invalid\nnameserver 127.0.0.1\noptions ndots:0\n".to_owned(),
            ),
        ],
        "/etc/hosts" => vec![
            (
                "host-entry",
                format!("{original}192.0.2.54 pbps-host-entry.invalid\n"),
            ),
            (
                "host-comment",
                format!("{original}# pbps-host-comment.invalid\n"),
            ),
        ],
        "/etc/hostname" => vec![("host-name", "pbps-host-sentinel\n".into())],
        _ => panic!("unknown runtime file"),
    }
}
