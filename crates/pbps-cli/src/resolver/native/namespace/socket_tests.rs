use super::*;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, Stdio};

struct Fixture {
    child: Child,
    output: BufReader<ChildStdout>,
    values: serde_json::Value,
}

impl Fixture {
    fn start(case: &str) -> Self {
        let script = std::env::var_os("PBPS_SOCKET_FIXTURE").expect("owned fixture script");
        let mut child = Command::new("/usr/bin/python3")
            .arg(script)
            .arg(case)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let values = serde_json::from_str(&line).unwrap();
        Self {
            child,
            output,
            values,
        }
    }

    fn pid(&self, name: &str) -> u32 {
        self.values[name].as_u64().unwrap().try_into().unwrap()
    }

    fn inode(&self) -> u64 {
        self.values["inode"].as_u64().unwrap()
    }

    fn command(&mut self, command: &str) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{command}").unwrap();
        let mut reply = String::new();
        self.output.read_line(&mut reply).unwrap();
        assert_eq!(reply.trim(), "ok");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = writeln!(self.child.stdin.as_mut().unwrap(), "stop");
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "requires the disposable PID/mount namespace socket fixture"]
fn socket_observation_preserves_groups_reparented_holders_and_service_identity() {
    use crate::resolver::native::{last_reading, socket_owner};
    for case in ["orphan", "two", "one", "threads", "worker-only"] {
        eprintln!("socket fixture case={case}");
        let fixture = Fixture::start(case);
        // A is deliberately not PID 1: the view must include other holders
        // even when they are siblings or have reparented outside A's tree.
        let service = ProcessLease::capture(fixture.pid("a")).unwrap();
        let holders = observed_socket_holders(&service, fixture.inode()).unwrap();
        let expected = if matches!(case, "two" | "orphan") {
            2
        } else {
            1
        };
        assert_eq!(holders.len(), expected, "{case}");
        let local = fixture.values["local"].as_str().unwrap().parse().unwrap();
        let peer = fixture.values["peer"].as_str().unwrap().parse().unwrap();
        if expected == 2 {
            assert!(socket_owner(&service, local, peer).is_err(), "{case}");
            assert_eq!(last_reading(), Some(Reading::OwnerCount(2)), "{case}");
        } else {
            let (owner, _) = socket_owner(&service, local, peer).unwrap();
            assert!(owner.same_process(&service).unwrap(), "{case}");
            let parent = ProcessLease::capture(fixture.pid("anchor")).unwrap();
            assert!(belongs_to_service(&owner, &parent).unwrap());
            let other = ProcessLease::capture(fixture.pid("b")).unwrap();
            assert!(socket_owner(&other, local, peer).is_err());
        }
    }
}

#[test]
#[ignore = "requires the disposable PID/mount namespace socket fixture"]
fn an_empty_observation_gets_one_fresh_backend_observation() {
    let mut fixture = Fixture::start("one");
    let service = ProcessLease::capture(fixture.pid("anchor")).unwrap();
    let inode = fixture.inode();
    let a = fixture.pid("a");
    let b = fixture.pid("b");
    let mut visits = 0;
    let holders = observe_with(&service, inode, |pid| {
        if pid == a {
            visits += 1;
            if visits == 1 {
                fixture.command("move");
            }
        }
    })
    .unwrap();
    assert_eq!(visits, 2, "only the empty first observation is retried");
    assert_eq!(holders.len(), 1);
    assert_eq!(holders[0].namespace_pid(), b);
    assert!(belongs_to_service(&holders[0], &service).unwrap());
}

#[test]
#[ignore = "requires the disposable PID/mount namespace socket fixture"]
fn a_singleton_observation_does_not_claim_exhaustive_socket_ownership() {
    let mut fixture = Fixture::start("handoff");
    let service = ProcessLease::capture(fixture.pid("anchor")).unwrap();
    let inode = fixture.inode();
    let a = fixture.pid("a");
    let c = fixture.pid("c");
    for pass in 0..4 {
        if pass != 0 {
            fixture.command("reset");
        }
        let holders = observe_with(&service, inode, |pid| {
            if pid == c {
                // B has already been inspected. Receipt is acknowledged
                // before C closes; A also holds the socket throughout.
                fixture.command("move");
            }
        })
        .unwrap();
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].namespace_pid(), a);
        assert_eq!(observed_socket_holders(&service, inode).unwrap().len(), 2);
    }
}

#[test]
#[ignore = "requires the disposable PID/mount namespace socket fixture"]
fn an_intermediate_exit_cannot_preserve_a_stale_service_relation() {
    let mut fixture = Fixture::start("ancestry");
    let service = ProcessLease::capture(fixture.pid("a")).unwrap();
    let holders = observed_socket_holders(&service, fixture.inode()).unwrap();
    assert_eq!(holders.len(), 1);
    let owner = &holders[0];
    assert_eq!(owner.namespace_pid(), fixture.pid("orphan"));
    assert!(belongs_to_service(owner, &service).unwrap());
    let unrelated = ProcessLease::capture(fixture.pid("b")).unwrap();
    assert!(!matches!(belongs_to_service(owner, &unrelated), Ok(true)));

    let mut acknowledged = false;
    let result = relation_with(owner, &service, |parent| {
        if parent == service.namespace_pid() {
            // The intermediate-to-service link has been checked. Its exit
            // reparents the still-live holder outside the selected service.
            fixture.command("exit-parent");
            acknowledged = true;
        }
    });
    assert!(acknowledged, "the parent exit must precede acceptance");
    assert!(
        result.is_err(),
        "an exited intermediate invalidates the chain"
    );
    owner
        .check()
        .expect("the socket holder itself remains alive");
    service.check().expect("the selected service remains alive");
    let reaper = ProcessLease::capture(fixture.pid("anchor")).unwrap();
    assert!(belongs_to_service(owner, &reaper).unwrap());
    assert!(!matches!(belongs_to_service(owner, &service), Ok(true)));
}
