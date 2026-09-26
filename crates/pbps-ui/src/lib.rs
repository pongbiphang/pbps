//! Local presentation over CLI subprocesses (ADR-0015). Every route reads,
//! except the fixed compose actions of ADR-0017 (#494) and the fixed trigger
//! actions of #64 step 5 (#1025).

pub mod client;
pub mod contract;
mod trigger;

// Qualified on Linux with the files ref backend only (#748); other platforms
// refuse compose until #471 qualifies them.
#[cfg(target_os = "linux")]
pub mod compose;
#[cfg(target_os = "linux")]
mod compose_http;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use tiny_http::{Header, Request, Response, Server, StatusCode};

use client::{Client, View};

const TOKEN_HEADER: &str = "X-Pbps-Token";
const HTML: &str = include_str!("../assets/index.html");
const JS: &str = include_str!("../assets/app.js");
const COMPOSE_JS: &str = include_str!("../assets/compose.js");
const TRIGGER_JS: &str = include_str!("../assets/trigger.js");
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
    #[cfg(target_os = "linux")]
    compose: RefCell<compose_http::Compose>,
    trigger: RefCell<trigger::Trigger>,
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
        Ok(Self {
            server,
            address,
            token,
            #[cfg(target_os = "linux")]
            compose: RefCell::new(compose_http::Compose::new(
                config.executable.clone(),
                config.project.clone(),
            )),
            trigger: RefCell::new(trigger::Trigger::new(
                config.executable.clone(),
                config.project.clone(),
            )),
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
        if let Some(action) = compose_action(request.url()) {
            return self.compose(request, action);
        }
        if let Some(action) = trigger_action(request.url()) {
            return self.trigger(request, action);
        }
        if !matches!(request.method().as_str(), "GET" | "HEAD") {
            return plain(405, "This viewer accepts reads only");
        }
        if let Some((_, mime, body)) = SHELL.iter().find(|(path, ..)| *path == request.url()) {
            return (200, mime, body.as_bytes().to_vec());
        }
        match route(request.url()) {
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
        }
    }
}

impl Viewer {
    fn compose(&self, request: &mut Request, action: &str) -> (u16, &'static str, Vec<u8>) {
        match json_body(request, "Compose", COMPOSE_BODY_LIMIT) {
            Ok(body) => self.compose_answer(action, &body),
            Err(refusal) => refusal,
        }
    }

    fn trigger(&self, request: &mut Request, action: &str) -> (u16, &'static str, Vec<u8>) {
        let body = match json_body(request, "Trigger", trigger::BODY_LIMIT) {
            Ok(body) => body,
            Err(refusal) => return refusal,
        };
        match self.trigger.borrow_mut().answer(action, &body) {
            Ok(bytes) => (200, "application/json", bytes),
            Err((code, message)) => plain(code, &message),
        }
    }

    #[cfg(target_os = "linux")]
    fn compose_answer(&self, action: &str, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
        match self.compose.borrow_mut().answer(action, body) {
            Ok(bytes) => (200, "application/json", bytes),
            Err((code, message)) => plain(code, &message),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn compose_answer(&self, _action: &str, _body: &[u8]) -> (u16, &'static str, Vec<u8>) {
        plain(501, COMPOSE_UNQUALIFIED)
    }
}

/// Compose is qualified on Linux only (ADR-0017). On Windows the qualified
/// build is one WSL away, with the checkout on the WSL filesystem (#1070);
/// native Windows is #471.
#[cfg(windows)]
pub const COMPOSE_UNQUALIFIED: &str = "Compose is qualified on Linux only. On Windows, run pbps ui inside WSL with the project in the WSL filesystem (for example ~/project, not /mnt/c), and open the printed URL in your Windows browser; or use the CLI (native Windows: #471)";
#[cfg(not(windows))]
pub const COMPOSE_UNQUALIFIED: &str =
    "Compose is qualified on Linux only; use the CLI on this platform (#471)";

#[cfg(target_os = "linux")]
const COMPOSE_BODY_LIMIT: u64 = compose_http::BODY_LIMIT;
#[cfg(not(target_os = "linux"))]
const COMPOSE_BODY_LIMIT: u64 = 64 * 1024;
const COMPOSE_ACTIONS: [&str; 11] = [
    "preview",
    "confirm",
    "list",
    "recover",
    "retry",
    "republish",
    "alternative",
    "cleanup",
    "resources",
    "recover-resources",
    "forget",
];

/// The immutable shell: served without the token (ADR-0015 decision 3), and
/// the only routes that are. Serving and authorizing both read this table,
/// so the two cannot disagree about which files are public.
const SHELL: [(&str, &str, &str); 5] = [
    ("/", "text/html; charset=utf-8", HTML),
    ("/app.js", "text/javascript; charset=utf-8", JS),
    ("/compose.js", "text/javascript; charset=utf-8", COMPOSE_JS),
    ("/trigger.js", "text/javascript; charset=utf-8", TRIGGER_JS),
    ("/style.css", "text/css; charset=utf-8", CSS),
];

/// Every read view: its path, the one parameter it takes, and the view it
/// becomes. The router is this table, so a read cannot exist without being
/// in it, and the guardrail tests enumerate it (#1050).
type ReadView = fn(String) -> View;
const READS: [(&str, Option<&str>, ReadView); 5] = [
    ("/api/status", None, |_| View::Status),
    ("/api/drift", Some("env"), View::Drift),
    ("/api/plan", Some("path"), View::Plan),
    ("/api/timeline", Some("env"), View::Timeline),
    ("/api/docs", None, |_| View::Docs),
];

/// Every route the viewer answers besides the shell, as the router reads
/// them: each read path with the one parameter it takes, then each write
/// path. For the guardrail tests (#1050), which must cover a route the day it
/// is added; not an API.
#[doc(hidden)]
pub fn routes() -> (Vec<(&'static str, Option<&'static str>)>, Vec<String>) {
    let reads = READS
        .iter()
        .map(|(path, parameter, _)| (*path, *parameter))
        .collect();
    let writes = COMPOSE_ACTIONS
        .iter()
        .map(|action| format!("/api/compose/{action}"))
        .chain(
            trigger::ACTIONS
                .iter()
                .map(|action| format!("/api/trigger/{action}")),
        )
        .collect();
    (reads, writes)
}

/// The complete write vocabulary: a fixed action name, no query, no path
/// parameters. Anything else is an ordinary (read) route or refused.
fn compose_action(url: &str) -> Option<&'static str> {
    let action = url.strip_prefix("/api/compose/")?;
    COMPOSE_ACTIONS.into_iter().find(|known| *known == action)
}

/// The trigger's vocabulary, fixed the same way (DEC-1025.1).
fn trigger_action(url: &str) -> Option<&'static str> {
    let action = url.strip_prefix("/api/trigger/")?;
    trigger::ACTIONS.into_iter().find(|known| *known == action)
}

/// A write action's body: `POST`, exactly one `application/json` content
/// type, and no more than `limit` bytes.
fn json_body(
    request: &mut Request,
    kind: &str,
    limit: u64,
) -> Result<Vec<u8>, (u16, &'static str, Vec<u8>)> {
    if request.method().as_str() != "POST" {
        return Err(plain(405, &format!("{kind} actions accept POST only")));
    }
    let json = request
        .headers()
        .iter()
        .filter(|h| h.field.equiv("Content-Type"));
    if json.map(|h| h.value.as_str()).collect::<Vec<_>>() != ["application/json"] {
        return Err(plain(415, &format!("{kind} actions accept JSON only")));
    }
    let mut body = Vec::new();
    if request
        .as_reader()
        .take(limit + 1)
        .read_to_end(&mut body)
        .is_err()
    {
        return Err(plain(
            400,
            &format!("The {} request could not be read", kind.to_lowercase()),
        ));
    }
    if body.len() as u64 > limit {
        return Err(plain(
            413,
            &format!("The {} request is too large", kind.to_lowercase()),
        ));
    }
    Ok(body)
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
    if SHELL.iter().any(|(path, ..)| *path == url) {
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
    let (_, parameter, view) = READS
        .iter()
        .find(|(known, ..)| *known == path)
        .ok_or("Unknown read view")?;
    let view = view(match parameter {
        Some(key) => value(*key)?,
        None => String::new(),
    });
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
mod guardrails;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_immutable_shell_can_be_read_without_the_token() {
        let peer = Some("127.0.0.1:1234".parse().unwrap());
        for path in ["/", "/app.js", "/compose.js", "/trigger.js", "/style.css"] {
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

    /// Where compose is refused, the answer names the qualified route: WSL on
    /// Windows (#1070), and the native-Windows issue everywhere (#471).
    #[test]
    fn the_unqualified_compose_answer_names_wsl_only_on_windows() {
        assert_eq!(COMPOSE_UNQUALIFIED.contains("inside WSL"), cfg!(windows));
        assert!(COMPOSE_UNQUALIFIED.contains("#471"));
        assert!(COMPOSE_UNQUALIFIED.contains("CLI"));
    }

    #[test]
    fn the_only_writes_are_the_fixed_compose_and_trigger_actions() {
        for action in COMPOSE_ACTIONS {
            assert_eq!(
                compose_action(&format!("/api/compose/{action}")),
                Some(action)
            );
            // A write is never a read view, so it cannot bypass the POST gate.
            assert!(route(&format!("/api/compose/{action}")).is_err());
        }
        for url in [
            "/api/compose",
            "/api/compose/",
            "/api/compose/apply",
            "/api/compose/push",
            "/api/compose/preview?remote=origin",
            "/api/compose/preview/",
            "/api/compose/PREVIEW",
            "/api/compose/confirm%00",
            "/api/apply",
            "/api/plan?apply=1",
        ] {
            assert_eq!(compose_action(url), None, "{url}");
        }
        for action in trigger::ACTIONS {
            assert_eq!(
                trigger_action(&format!("/api/trigger/{action}")),
                Some(action)
            );
            assert!(route(&format!("/api/trigger/{action}")).is_err());
            assert_eq!(compose_action(&format!("/api/trigger/{action}")), None);
        }
        for url in [
            "/api/trigger",
            "/api/trigger/",
            "/api/trigger/sql",
            "/api/trigger/push",
            "/api/trigger/apply?checksum=x",
            "/api/trigger/apply/",
            "/api/trigger/APPLY",
            "/api/trigger/plan%00",
            "/api/compose/apply",
        ] {
            assert_eq!(trigger_action(url), None, "{url}");
        }
    }

    #[test]
    fn the_ui_crate_has_no_workspace_dependency_and_embeds_no_remote_assets() {
        // Parsed, and each dependency resolved to the package it names, so a
        // dependency cannot hide behind a spelling, a section or a rename
        // (`serde = { package = "pbps-model", .. }`), including through the
        // workspace table (#1065). Shipped dependencies are `[dependencies]`
        // and every `[target.*.dependencies]`; the one dev-dependency never
        // reaches the binary.
        fn names(table: Option<&toml::Value>) -> Vec<&str> {
            table
                .and_then(toml::Value::as_table)
                .map(|t| t.keys().map(String::as_str).collect())
                .unwrap_or_default()
        }
        let manifest: toml::Table = include_str!("../Cargo.toml").parse().unwrap();
        let root: toml::Table = include_str!("../../../Cargo.toml").parse().unwrap();
        let workspace = &root["workspace"]["dependencies"];
        let mut sections = vec![manifest.get("dependencies")];
        for target in manifest
            .get("target")
            .and_then(toml::Value::as_table)
            .into_iter()
            .flat_map(|targets| targets.values())
        {
            sections.push(target.get("dependencies"));
        }
        let mut shipped = Vec::new();
        for section in sections.into_iter().flatten() {
            for (name, spec) in section.as_table().unwrap() {
                // `workspace = true` takes its source from the root table.
                let spec = if spec.get("workspace") == Some(&toml::Value::Boolean(true)) {
                    &workspace[name.as_str()]
                } else {
                    spec
                };
                for local in ["path", "git"] {
                    assert!(
                        spec.get(local).is_none(),
                        "{name} comes from a {local} source"
                    );
                }
                let package = spec
                    .get("package")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(name);
                shipped.push(package.to_owned());
            }
        }
        // A build script and its dependencies also run as part of this crate,
        // and could generate code for it; the UI has neither.
        assert!(manifest.get("build-dependencies").is_none());
        for target in manifest
            .get("target")
            .and_then(toml::Value::as_table)
            .into_iter()
            .flat_map(|targets| targets.values())
        {
            assert!(target.get("build-dependencies").is_none());
        }
        assert!(manifest["package"].get("build").is_none());
        // Tests run in the package root, where Cargo finds a `build.rs`.
        assert!(!std::path::Path::new("build.rs").exists());
        shipped.sort_unstable();
        assert_eq!(
            shipped,
            ["rustix", "serde", "serde_json", "sha2", "tiny_http"],
            "no workspace crate, and nothing new, ships in the UI"
        );
        assert_eq!(names(manifest.get("dev-dependencies")), ["toml"]);
        assert!(!HTML.contains("<script>"));
        assert!(!JS.contains("innerHTML"));
        assert!(!COMPOSE_JS.contains("innerHTML"));
        assert!(!TRIGGER_JS.contains("innerHTML"));
        assert!(!TRIGGER_JS.contains("localStorage") && !TRIGGER_JS.contains("sessionStorage"));
        assert!(!TRIGGER_JS.contains("https://"));
        assert!(!COMPOSE_JS.contains("localStorage") && !COMPOSE_JS.contains("sessionStorage"));
        assert!(!JS.contains("localStorage") && !JS.contains("sessionStorage"));
        assert!(
            !HTML.contains("https://") && !CSS.contains("https://") && !JS.contains("https://")
        );
        assert!(JS.contains("sandbox"));
    }
}
