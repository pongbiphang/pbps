use super::*;
use std::os::fd::OwnedFd;
use std::process::{Command, Stdio};
use tokio::io::AsyncReadExt as _;

#[test]
#[ignore = "inherited-descriptor helper inside the owned daemon fixture"]
fn fixture_acceptor_holds_only_its_accepted_sockets() {
    use std::os::fd::AsFd as _;
    assert_eq!(std::env::var("PBPS_DAEMON_FIXTURE").as_deref(), Ok("1"));
    let descriptor = std::io::stdin().as_fd().try_clone_to_owned().unwrap();
    let listener = std::os::unix::net::UnixListener::from(descriptor);
    let mut sockets = Vec::new();
    while let Ok((socket, _)) = listener.accept() {
        sockets.push(socket);
        assert!(sockets.len() <= 8);
    }
}

#[tokio::test]
#[ignore = "PID 1 in a disposable private Docker fixture; never run on the host"]
async fn socket_activation_requires_the_candidate_daemon_to_own_the_actual_peer() {
    use crate::resolver::docker::{Error, LocalApi};
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(std::env::var("PBPS_DAEMON_FIXTURE").as_deref(), Ok("1"));
    assert_eq!(std::process::id(), 1);
    assert_eq!(std::env::current_exe().unwrap(), Path::new("/systemd"));
    let path = Path::new("/run/pbps-activation.sock");
    let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let descriptor: OwnedFd = listener.into();
    let mut daemon = Command::new("/dockerd")
        .args([
            "--ignored",
            "--exact",
            "resolver::native::daemon::tests::fixture_acceptor_holds_only_its_accepted_sockets",
        ])
        .stdin(Stdio::from(descriptor))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::fs::write("/run/docker.pid", daemon.id().to_string()).unwrap();
    let accepted = LocalApi::connect_native(path).await;
    let second = LocalApi::connect_native(path).await;
    // A protected PID file must not vouch for another accepted socket.
    let proxy_path = Path::new("/run/pbps-proxy.sock");
    let listener = tokio::net::UnixListener::bind(proxy_path).unwrap();
    std::fs::set_permissions(proxy_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (refused, bytes) = tokio::join!(
        async { LocalApi::connect_native(proxy_path).await.map(drop) },
        async {
            let (mut peer, _) = listener.accept().await.unwrap();
            let mut byte = [0];
            tokio::time::timeout(Duration::from_secs(10), peer.read(&mut byte))
                .await
                .unwrap()
                .unwrap()
        }
    );
    std::fs::write("/run/docker.pid", "1").unwrap();
    let substituted = LocalApi::connect_native(path).await;
    let second_ok = second.is_ok();
    drop(second);
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(
        accepted.is_ok(),
        "socket activation must authenticate the actual acceptor"
    );
    assert!(
        second_ok,
        "each additional socket must authenticate independently"
    );
    assert!(
        matches!(refused, Err(Error::NativeDaemon)),
        "a protected candidate cannot qualify a proxy socket"
    );
    assert_eq!(bytes, 0, "unqualified peers receive no Docker requests");
    assert!(
        matches!(substituted, Err(Error::NativeDaemon)),
        "a substituted candidate is refused"
    );
}

#[tokio::test]
async fn an_inherited_listener_is_bound_to_the_acceptor_and_lost_ownership_is_terminal() {
    let path =
        std::env::temp_dir().join(format!("pbps-activation-{:032x}", rand::random::<u128>()));
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let descriptor: OwnedFd = listener.into();
    // Hand only this owned listener to the child. No root or external daemon
    // is needed to exercise the kernel's socket-activation identity rules.
    let mut child = Command::new("/usr/bin/python3")
        .args(["-c", "import socket; s=socket.socket(fileno=0); c,_=s.accept(); s.close(); c.send(b'R'); c.recv(1)"])
        .stdin(Stdio::from(descriptor)).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().unwrap();
    let mut stream = UnixStream::connect(&path).await.unwrap();
    let mut ready = [0];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut ready))
        .await
        .unwrap()
        .unwrap();
    let process = ProcessLease::capture(child.id()).unwrap();
    let creator = stream.peer_cred().unwrap().pid().unwrap() as u32;
    let observed = UnixPeer::capture(&stream, &process).await;
    let mut unrelated = Command::new("/usr/bin/sleep").arg("30").spawn().unwrap();
    let other = ProcessLease::capture(unrelated.id()).unwrap();
    let outcome = observed.as_ref().map(|peer| {
        (
            peer.check(&process).is_ok(),
            peer.check(&other).is_err(),
            unix_peer(peer.inode, peer.cookie.wrapping_add(1)).is_err(),
        )
    });
    drop(stream);
    child.wait().unwrap();
    let lost = observed.as_ref().map(|peer| peer.check(&process).is_err());
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(creator, std::process::id());
    assert_ne!(
        creator,
        child.id(),
        "peer credentials name the listener creator"
    );
    assert!(
        matches!(outcome, Ok((true, true, true))),
        "acceptor owns the connected socket; unrelated owners and cookie substitution refuse"
    );
    assert!(
        matches!(lost, Ok(true)),
        "losing the socket or acceptor invalidates its lease"
    );
}

fn reply() -> Vec<u8> {
    let mut reply = Vec::new();
    reply.extend(40_u32.to_ne_bytes());
    reply.extend(20_u16.to_ne_bytes());
    reply.extend(0_u16.to_ne_bytes());
    reply.extend(77_u32.to_ne_bytes());
    reply.extend(0_u32.to_ne_bytes());
    reply.extend([1, 1, 1, 0]);
    reply.extend(81_u32.to_ne_bytes());
    reply.extend(91_u32.to_ne_bytes());
    reply.extend(0_u32.to_ne_bytes());
    reply.extend(8_u16.to_ne_bytes());
    reply.extend(2_u16.to_ne_bytes());
    reply.extend(101_u32.to_ne_bytes());
    reply
}

#[test]
fn kernel_diagnostics_reject_truncation_wrong_identity_and_duplicate_peers() {
    let valid = reply();
    assert_eq!(parse_peer(&valid, 77, 81, 91).unwrap(), 101);
    for length in 0..valid.len() {
        assert!(parse_peer(&valid[..length], 77, 81, 91).is_err());
    }
    for index in [0, 4, 6, 8, 16, 17, 18, 20, 24, 28, 32, 33, 34] {
        let mut changed = valid.clone();
        changed[index] ^= 1;
        assert!(
            parse_peer(&changed, 77, 81, 91).is_err(),
            "invalid byte {index}"
        );
    }
    let mut duplicate = valid.clone();
    duplicate.extend(&valid[32..]);
    duplicate[..4].copy_from_slice(&(48_u32.to_ne_bytes()));
    assert!(parse_peer(&duplicate, 77, 81, 91).is_err());
}
