//! A real smart-HTTP endpoint, including CGI advertisements and receive-pack.
use super::*;
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use std::thread;

struct Http {
    address: std::net::SocketAddr,
    mode: Arc<AtomicU8>,
    paths: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    root: PathBuf,
}
impl Http {
    fn new(f: &Fixture) -> Self {
        let root = f.repo.root.with_extension("http");
        fs::create_dir(&root).unwrap();
        for name in ["reviewed.git", "observation.git", "publication.git"] {
            git(
                &f.remote,
                &[
                    "clone",
                    "--bare",
                    "-q",
                    ".",
                    root.join(name).to_str().unwrap(),
                ],
            );
            git(&root.join(name), &["config", "http.receivepack", "true"]);
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mode = Arc::new(AtomicU8::new(0));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (m, p, s, r) = (mode.clone(), paths.clone(), stop.clone(), root.clone());
        let worker = thread::spawn(move || {
            for stream in listener.incoming() {
                if s.load(Ordering::SeqCst) {
                    break;
                }
                Self::serve(stream.unwrap(), &r, address, m.load(Ordering::SeqCst), &p);
            }
        });
        Self {
            address,
            mode,
            paths,
            stop,
            worker: Some(worker),
            root,
        }
    }
    fn endpoint(&self) -> String {
        format!("http://{}/reviewed.git", self.address)
    }
    fn serve(
        mut stream: TcpStream,
        root: &Path,
        address: std::net::SocketAddr,
        mode: u8,
        paths: &Mutex<Vec<String>>,
    ) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            if stream.read_exact(&mut byte).is_err() {
                return;
            }
            header.push(byte[0]);
            assert!(header.len() < 65536);
        }
        let header = String::from_utf8(header).unwrap();
        let first = header
            .lines()
            .next()
            .unwrap()
            .split(' ')
            .collect::<Vec<_>>();
        let (method, target) = (first[0], first[1]);
        paths.lock().unwrap().push(target.into());
        let length = header
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        assert!(
            !header
                .to_ascii_lowercase()
                .contains("transfer-encoding: chunked")
        );
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        if mode == 3 {
            let _ = stream.write_all(b"HTTP/1.0 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"compose\"\r\nContent-Length: 0\r\n\r\n");
            return;
        }
        let redirect = target.starts_with("/reviewed.git/")
            && (mode == 1 || (mode == 2 && target.contains("git-receive-pack")));
        if redirect {
            let name = if mode == 1 {
                "observation.git"
            } else {
                "publication.git"
            };
            let target = target.replacen("reviewed.git", name, 1);
            let response = format!(
                "HTTP/1.0 302 Found\r\nLocation: http://{address}{target}\r\nContent-Length: 0\r\n\r\n"
            );
            let _ = stream.write_all(response.as_bytes());
            return;
        }
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let content_type = header
            .lines()
            .find_map(|l| l.strip_prefix("Content-Type: "))
            .unwrap_or("");
        let mut child = Command::new("git")
            .arg("http-backend")
            .env("GIT_PROJECT_ROOT", root)
            .env("GIT_HTTP_EXPORT_ALL", "1")
            .env("PATH_INFO", path)
            .env("QUERY_STRING", query)
            .env("REQUEST_METHOD", method)
            .env("CONTENT_TYPE", content_type)
            .env("CONTENT_LENGTH", length.to_string())
            .env("REMOTE_USER", "fixture")
            .env("REMOTE_ADDR", "127.0.0.1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&body).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let split = output
            .stdout
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap();
        let headers = &output.stdout[..split];
        let content = &output.stdout[split + 4..];
        let response = format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n", content.len());
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(headers);
        let _ = stream.write_all(b"\r\n\r\n");
        let _ = stream.write_all(content);
    }
    fn untouched_targets(&self, reference: &str) {
        assert!(self.paths.lock().unwrap().iter().all(|p| !p.starts_with("/observation.git/") && !p.starts_with("/publication.git/")), "redirect target was contacted: {:?}", self.paths.lock().unwrap());
        for name in ["observation.git", "publication.git"] {
            let result = Command::new("git")
                .arg("-C")
                .arg(self.root.join(name))
                .args(["show-ref", "--verify", reference])
                .output()
                .unwrap();
            assert!(!result.status.success());
        }
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        self.worker.take().unwrap().join().unwrap();
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn smart_http_redirects_cannot_override_the_reviewed_destination() {
    for mode in [0, 1, 2] {
        let f = Fixture::new(&format!("http-{mode}"));
        let http = Http::new(&f);
        let endpoint = http.endpoint();
        git(&f.repo.root, &["remote", "set-url", "origin", &endpoint]);
        // More specific inherited URL matching must not restore redirects.
        for url in [
            &endpoint,
            &format!("{endpoint}/"),
            &format!("http://{}/", http.address),
        ] {
            git(
                &f.repo.root,
                &["config", &format!("http.{url}.followRedirects"), "true"],
            );
        }
        let (_store, preview, candidate) = f.ready();
        let before = f.repo.preserved();
        http.mode.store(mode, Ordering::SeqCst);
        let mut publisher = f.publisher();
        let result = publisher.confirm(&candidate);
        http.untouched_targets(&preview.output_ref);
        if mode == 0 {
            assert_eq!(result.status, Status::Delivered, "{result:?}");
        } else if mode == 1 {
            assert_eq!(result.status, Status::Refused);
            assert_eq!(result.problem, Some(Problem::RemoteUnavailable));
        } else {
            assert_eq!(result.local, LocalState::Present);
            assert_eq!(result.remote, DeliveryState::Unknown);
            let commit = result.details.unwrap().commit;
            http.mode.store(0, Ordering::SeqCst);
            assert_eq!(
                publisher.retry(&preview.operation_id).remote,
                DeliveryState::Unknown
            );
            let diagnosed = publisher.recover(&preview.operation_id);
            let generation = diagnosed.details.unwrap().delivery_generation.unwrap();
            let retried = publisher.republish(&preview.operation_id, &generation);
            assert_eq!(retried.status, Status::Delivered);
            assert_eq!(retried.details.unwrap().commit, commit);
        }
        f.source_unchanged(&before);
    }
}

#[test]
fn authentication_failure_before_delivery_can_retry_the_same_commit_without_leaking_helpers() {
    let f = Fixture::new("http-auth");
    let http = Http::new(&f);
    git(
        &f.repo.root,
        &["remote", "set-url", "origin", &http.endpoint()],
    );
    git(
        &f.repo.root,
        &[
            "config",
            "credential.helper",
            "!echo FAKE_HELPER_SECRET >&2; exit 1",
        ],
    );
    let (_store, preview, candidate) = f.ready();
    let before = f.repo.preserved();
    let mut publisher = f.publisher();
    let result = publisher.confirm_observed(&candidate, &|boundary| {
        if boundary == Boundary::AfterPersist(DurableStage::LocalPublished) {
            http.mode.store(3, Ordering::SeqCst);
        }
        true
    });
    assert_eq!(result.local, LocalState::Present);
    assert_eq!(result.remote, DeliveryState::NotAttempted);
    assert_eq!(result.problem, Some(Problem::RemoteUnavailable));
    let commit = result.details.as_ref().unwrap().commit.clone();
    let receipt = fs::read(
        f.repo
            .root
            .join(".git/pbps-compose-v2")
            .join(format!("{}.json", preview.operation_id)),
    )
    .unwrap();
    assert!(
        !String::from_utf8(receipt)
            .unwrap()
            .contains("FAKE_HELPER_SECRET")
    );
    assert!(
        !serde_json::to_string(&result)
            .unwrap()
            .contains("FAKE_HELPER_SECRET")
    );
    drop(publisher);
    http.mode.store(0, Ordering::SeqCst);
    let result = f.publisher().retry(&preview.operation_id);
    assert_eq!(result.status, Status::Delivered);
    assert_eq!(result.details.unwrap().commit, commit);
    f.source_unchanged(&before);
}
