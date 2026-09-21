//! Local, read-only presentation over CLI subprocesses (ADR-0015).

pub mod client;
pub mod compose;
pub mod contract;

use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use tiny_http::{Header, Request, Response, Server, StatusCode};

use client::{Client, View};

#[cfg(target_os = "linux")]
mod site;

const TOKEN_HEADER: &str = "X-Pbps-Token";
const HTML: &str = include_str!("../assets/index.html");
const JS: &str = include_str!("../assets/app.js");
const CSS: &str = include_str!("../assets/style.css");

/// The CLI supplies randomness and the docs stylesheet hash; this crate needs
/// no access to the model, configuration, documentation renderer or RNG.
pub struct Config {
    pub executable: PathBuf,
    pub project: PathBuf,
    pub token: [u8; 32],
    pub docs_style_hash: String,
}

pub struct Viewer {
    server: Server,
    address: SocketAddr,
    token: String,
    client: Client,
    policy: String,
    /// Where this project sits in a checkout, and everything composing needs.
    /// `None` where the project is not in one, or on a platform whose file
    /// calls ADR-0015 decision 5 has not had measured — either way the page
    /// is told so and shown the commands to run by hand.
    #[cfg(target_os = "linux")]
    site: Option<site::Site>,
}

impl Viewer {
    pub fn bind(config: Config) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let address = listener.local_addr()?;
        let server = Server::from_listener(listener, None).map_err(io::Error::other)?;
        if config.docs_style_hash.len() != 44
            || !config
                .docs_style_hash
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
        {
            return Err(io::Error::other("invalid documentation stylesheet hash"));
        }
        let token = config.token.iter().map(|b| format!("{b:02x}")).collect();
        #[cfg(target_os = "linux")]
        let site = site::Site::at(&config.executable, &config.project, config.token);
        Ok(Self {
            server,
            address,
            token,
            #[cfg(target_os = "linux")]
            site,
            client: Client {
                executable: config.executable,
                project: config.project,
            },
            policy: format!(
                "default-src 'self'; script-src 'self'; style-src 'self' 'sha256-{}'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'",
                config.docs_style_hash
            ),
        })
    }

    pub fn url(&self) -> String {
        format!("http://{}/#{}", self.address, self.token)
    }

    pub fn serve(self) -> io::Result<()> {
        loop {
            let mut request = self.server.recv()?;
            let (code, mime, body) = self.answer(&mut request);
            let response = Response::from_data(body)
                .with_status_code(StatusCode(code))
                .with_header(header("Content-Type", mime))
                .with_header(header("Cache-Control", "no-store"))
                .with_header(header("X-Content-Type-Options", "nosniff"))
                .with_header(header("Referrer-Policy", "no-referrer"))
                .with_header(header("Content-Security-Policy", &self.policy));
            // A tab closing must not stop the next read or expose diagnostics.
            let _ = request.respond(response);
        }
    }

    fn answer(&self, request: &mut Request) -> (u16, &'static str, Vec<u8>) {
        let headers: Vec<_> = request
            .headers()
            .iter()
            .map(|h| (h.field.as_str().as_str(), h.value.as_str()))
            .collect();
        if !authorized(
            request.remote_addr().copied(),
            request.method().as_str(),
            request.url(),
            &headers,
            &self.address.to_string(),
            &self.token,
        ) {
            return plain(403, "Request refused");
        }
        let method = request.method().as_str().to_owned();
        let url = request.url().to_owned();
        if method == "POST" {
            // The only writes this server has, and they are writes to *git*,
            // not to a database: step 5 is still outside this UI (#64).
            return match url.as_str() {
                "/api/compose/preview" | "/api/compose/record" => self.compose(request, &url),
                _ => plain(405, "This viewer has no such write"),
            };
        }
        if !matches!(method.as_str(), "GET" | "HEAD") {
            return plain(405, "This viewer accepts reads only");
        }
        if url == "/api/compose" {
            return self.compose_context();
        }
        match request.url() {
            "/" => (200, "text/html; charset=utf-8", HTML.as_bytes().to_vec()),
            "/app.js" => (
                200,
                "text/javascript; charset=utf-8",
                JS.as_bytes().to_vec(),
            ),
            "/style.css" => (200, "text/css; charset=utf-8", CSS.as_bytes().to_vec()),
            url => match route(url) {
                Ok(view) => match self.client.read(&view) {
                    Ok(bytes) => (
                        200,
                        if matches!(view, View::Docs) {
                            "text/html; charset=utf-8"
                        } else {
                            "application/json"
                        },
                        bytes,
                    ),
                    Err(message) => plain(502, &message),
                },
                Err(message) => plain(400, message),
            },
        }
    }
}

impl Viewer {
    #[cfg(target_os = "linux")]
    fn compose_context(&self) -> (u16, &'static str, Vec<u8>) {
        match &self.site {
            Some(site) => json(200, &site.context()),
            None => json(200, &site::Site::unavailable()),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn compose_context(&self) -> (u16, &'static str, Vec<u8>) {
        json(200, &unsupported_platform())
    }

    #[cfg(target_os = "linux")]
    fn compose(&self, request: &mut Request, url: &str) -> (u16, &'static str, Vec<u8>) {
        let mut body = Vec::new();
        if std::io::Read::read_to_end(request.as_reader(), &mut body).is_err() {
            return site::refused(400, "The request body could not be read");
        }
        // The shape of the request is decided before anything about this
        // checkout is: `deny_unknown_fields` on the way in, for the same
        // reason decision 6 puts it on the way out, and a field the page
        // invents fails here whether or not a compose could have run.
        let asked: crate::compose::run::Request = match serde_json::from_slice(&body) {
            Ok(asked) => asked,
            Err(e) => {
                return site::refused(400, &format!("That is not a compose this UI accepts: {e}"));
            }
        };
        match &self.site {
            Some(site) => site.answer(url, &asked),
            None => site::refused(
                409,
                "This project is not inside a git checkout, so there is nothing to record                  the intent in.",
            ),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn compose(&self, _request: &mut Request, _url: &str) -> (u16, &'static str, Vec<u8>) {
        json(409, &unsupported_platform())
    }
}

/// Decision 5's own rule for a platform whose calls it has not had measured:
/// refuse to compose and give the commands to run by hand, which is what a
/// machine without `git` gets too.
#[cfg(not(target_os = "linux"))]
fn unsupported_platform() -> serde_json::Value {
    serde_json::json!({
        "available": false,
        "why": "Composing has only been measured on Linux. Run the intent in a shell instead.",
        "commands": [
            "pbps rename <from> <to>",
            "git add <the declaration> <the ids file>",
            "git commit -m \"<the intent>\"",
            "git push",
        ],
    })
}

fn json(code: u16, value: &impl serde::Serialize) -> (u16, &'static str, Vec<u8>) {
    match serde_json::to_vec(value) {
        Ok(bytes) => (code, "application/json", bytes),
        Err(_) => plain(500, "The answer could not be rendered"),
    }
}

fn plain(code: u16, message: &str) -> (u16, &'static str, Vec<u8>) {
    (
        code,
        "text/plain; charset=utf-8",
        message.as_bytes().to_vec(),
    )
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name, value).expect("fixed header or validated stylesheet hash")
}

fn authorized(
    peer: Option<SocketAddr>,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    address: &str,
    token: &str,
) -> bool {
    if !peer.is_some_and(|p| p.ip().is_loopback()) {
        return false;
    }
    let one = |name: &str| -> Result<Option<&str>, ()> {
        let mut values = headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| *value);
        let value = values.next();
        if values.next().is_some() {
            Err(())
        } else {
            Ok(value)
        }
    };
    if one("Host") != Ok(Some(address)) {
        return false;
    }
    match one("Origin") {
        Ok(Some(origin)) if origin == format!("http://{address}") => {}
        Ok(None) if matches!(method, "GET" | "HEAD") => {}
        _ => return false,
    }
    if matches!(url, "/" | "/app.js" | "/style.css") {
        return true;
    }
    one(TOKEN_HEADER) == Ok(Some(token))
}

fn route(url: &str) -> Result<View, &'static str> {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let mut parameters = BTreeMap::new();
    if !query.is_empty() {
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').ok_or("Malformed query")?;
            if parameters.insert(decode(key)?, decode(value)?).is_some() {
                return Err("Duplicate parameter");
            }
        }
    }
    let mut value = |key| {
        parameters
            .remove(key)
            .filter(|v| !v.is_empty())
            .ok_or("A named environment or saved plan path is required")
    };
    let view = match path {
        "/api/status" => View::Status,
        "/api/drift" => View::Drift(value("env")?),
        "/api/plan" => View::Plan(value("path")?),
        "/api/timeline" => View::Timeline(value("env")?),
        "/api/docs" => View::Docs,
        _ => return Err("Unknown read view"),
    };
    if parameters.is_empty() {
        Ok(view)
    } else {
        Err("Unknown parameter")
    }
}

fn decode(value: &str) -> Result<String, &'static str> {
    let mut result = Vec::new();
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        result.push(match byte {
            b'+' => b' ',
            b'%' => {
                let hi = bytes
                    .next()
                    .and_then(|b| (b as char).to_digit(16))
                    .ok_or("Invalid escape")?;
                let lo = bytes
                    .next()
                    .and_then(|b| (b as char).to_digit(16))
                    .ok_or("Invalid escape")?;
                (hi * 16 + lo) as u8
            }
            b => b,
        });
    }
    let text = String::from_utf8(result).map_err(|_| "Query must be UTF-8")?;
    if text.chars().any(char::is_control) {
        Err("Control characters are not accepted")
    } else {
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_immutable_shell_can_be_read_without_the_token() {
        let peer = Some("127.0.0.1:1234".parse().unwrap());
        for path in ["/", "/app.js", "/style.css"] {
            assert!(authorized(
                peer,
                "GET",
                path,
                &[("Host", "127.0.0.1:8080")],
                "127.0.0.1:8080",
                "secret"
            ));
        }
        for path in [
            "/api/status",
            "/api/drift?env=dev",
            "/api/plan?path=x",
            "/api/timeline?env=dev",
            "/api/docs",
            "/unknown",
            "/?token=secret",
        ] {
            assert!(!authorized(
                peer,
                "GET",
                path,
                &[("Host", "127.0.0.1:8080")],
                "127.0.0.1:8080",
                "secret"
            ));
            assert!(authorized(
                peer,
                "GET",
                path,
                &[("Host", "127.0.0.1:8080"), (TOKEN_HEADER, "secret")],
                "127.0.0.1:8080",
                "secret"
            ));
        }
    }

    #[test]
    fn foreign_origins_peers_hosts_and_ambiguous_credentials_are_refused() {
        let peer = Some("127.0.0.1:1234".parse().unwrap());
        let good = [("Host", "127.0.0.1:8080"), (TOKEN_HEADER, "secret")];
        let ask = |peer, method, headers: &[(&str, &str)]| {
            authorized(
                peer,
                method,
                "/api/status",
                headers,
                "127.0.0.1:8080",
                "secret",
            )
        };
        assert!(ask(peer, "GET", &good));
        assert!(ask(peer, "HEAD", &good));
        assert!(!ask(peer, "POST", &good));
        for origin in ["https://evil.example", "null", "http://127.0.0.1:8081"] {
            let mut headers = good.to_vec();
            headers.push(("Origin", origin));
            assert!(!ask(peer, "GET", &headers));
            assert!(!ask(peer, "POST", &headers));
        }
        assert!(!ask(Some("192.0.2.1:80".parse().unwrap()), "GET", &good));
        assert!(!ask(None, "GET", &good));
        for host in ["localhost:8080", "rebind.example:8080", "127.0.0.1:8081"] {
            assert!(!ask(
                peer,
                "GET",
                &[("Host", host), (TOKEN_HEADER, "secret")]
            ));
        }
        assert!(!ask(
            peer,
            "GET",
            &[("Host", "127.0.0.1:8080"), (TOKEN_HEADER, "wrong")]
        ));
        for duplicated in [("Host", "127.0.0.1:8080"), (TOKEN_HEADER, "secret")] {
            let mut headers = good.to_vec();
            headers.push(duplicated);
            assert!(!ask(peer, "GET", &headers));
        }
        let mut headers = good.to_vec();
        headers.push(("Origin", "http://127.0.0.1:8080"));
        assert!(ask(peer, "POST", &headers));
        headers.push(("origin", "http://127.0.0.1:8080"));
        assert!(!ask(peer, "GET", &headers));
    }

    #[test]
    fn the_route_vocabulary_cannot_express_writes_or_connection_strings() {
        assert_eq!(
            route("/api/plan?path=my%20plan%2B%E5%9C%96.json"),
            Ok(View::Plan("my plan+圖.json".into()))
        );
        assert_eq!(route("/api/drift?env=dev"), Ok(View::Drift("dev".into())));
        for path in [
            "/api/apply",
            "/api/plan?sql=DROP+TABLE",
            "/api/status?db=secret",
            "/api/drift?env=",
            "/api/drift?env=dev&env=prod",
            "/api/docs?out=x",
            "/api/plan?path=%",
            "/api/plan?path=%FF",
            "/api/plan?path=%00",
            "/../pbps.yml",
        ] {
            assert!(route(path).is_err(), "{path}");
        }
    }

    #[test]
    fn the_ui_crate_has_no_workspace_dependency_and_embeds_no_remote_assets() {
        let manifest = include_str!("../Cargo.toml");
        let dependencies = manifest
            .split("[dependencies]")
            .nth(1)
            .unwrap()
            .split("[lints]")
            .next()
            .unwrap();
        assert!(!dependencies.contains("pbps-"));
        for line in dependencies
            .lines()
            .filter(|line| line.contains('=') && !line.trim_start().starts_with('#'))
        {
            // The list is the decision, not a record of what happens to be
            // there: decision 6 allows serde, one small HTTP crate and nothing
            // of this workspace, and compose adds `rustix` for the syscalls a
            // tree that forbids `unsafe` cannot otherwise reach.
            assert!(
                matches!(
                    line.split('=').next().unwrap().trim(),
                    "serde.workspace" | "serde_json.workspace" | "tiny_http.workspace" | "rustix"
                ),
                "{line}"
            );
        }
        assert!(!HTML.contains("<script>"));
        assert!(!JS.contains("innerHTML"));
        assert!(!JS.contains("localStorage") && !JS.contains("sessionStorage"));
        assert!(
            !HTML.contains("https://") && !CSS.contains("https://") && !JS.contains("https://")
        );
        assert!(JS.contains("sandbox"));
        // The confirmation sends the body the preview was made from, never one
        // rebuilt from the form: a field edited while the preview was in
        // flight would otherwise commit a different intent than the diff that
        // was read. Both halves of that are shape assertions on the page,
        // which is as much as this crate can check without a browser.
        assert!(JS.contains("composeGeneration"));
        assert!(JS.contains("previewed.body"));
        assert!(
            !JS.contains("composeRequest(\"/api/compose/record\")"),
            "the record request must be given a body, not build one"
        );
    }
}
