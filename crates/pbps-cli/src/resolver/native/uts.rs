//! Kernel UTS values are selected by the reading thread, not a procfs path.

use super::{ProcessLease, UnqualifiedProcess};
use rustix::fd::AsFd as _;
use rustix::thread::{LinkNameSpaceType, move_into_link_name_space};
use std::fs::File;

pub(crate) fn check(process: &ProcessLease) -> Result<(), UnqualifiedProcess> {
    process.check()?;
    let namespace = process
        .namespaces
        .iter()
        .find(|(name, _, _)| *name == "uts")
        .map(|(_, file, _)| file)
        .ok_or(UnqualifiedProcess)?;
    let (hostname, domainname) = read(namespace)?;
    // DECISIONS 544: these complete literal defaults contain no operator-specific input.
    // Empty Docker configuration does not establish any particular kernel
    // value: the measured native host uses localdomain; Linux also uses
    // (none), and an explicitly cleared NIS name is harmless.
    if hostname != super::runtime_files::HOSTNAME.as_bytes()
        || ![
            b"".as_slice(),
            b"(none)".as_slice(),
            b"localdomain".as_slice(),
        ]
        .contains(&domainname.as_slice())
    {
        return Err(UnqualifiedProcess);
    }
    process.check()
}

fn read(namespace: &File) -> Result<(Vec<u8>, Vec<u8>), UnqualifiedProcess> {
    // /proc/PID/root/proc/sys/kernel/hostname still answers for the reader's
    // UTS namespace, even if the file was opened in another one. Enter only
    // the held UTS namespace on a fresh short-lived thread and read uname;
    // never move an async worker or rely on restoring its namespace later.
    // This changes no target state and reads no code from its filesystem.
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("pbps-uts".into())
            .spawn_scoped(scope, || {
                move_into_link_name_space(
                    namespace.as_fd(),
                    Some(LinkNameSpaceType::HostNameAndNISDomainName),
                )
                .map_err(|_| UnqualifiedProcess)?;
                let names = rustix::system::uname();
                Ok((
                    names.nodename().to_bytes().to_vec(),
                    names.domainname().to_bytes().to_vec(),
                ))
            })
            .map_err(|_| UnqualifiedProcess)?
            .join()
            .map_err(|_| UnqualifiedProcess)?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, Write as _};
    use std::process::{Command, Stdio};

    #[test]
    fn an_unreadable_namespace_is_not_a_private_kernel_name() {
        let observer = std::fs::read_link("/proc/thread-self/ns/uts").unwrap();
        assert!(read(&File::open("/dev/null").unwrap()).is_err());
        assert_eq!(
            std::fs::read_link("/proc/thread-self/ns/uts").unwrap(),
            observer
        );
    }

    #[test]
    #[ignore = "requires root on a disposable native Linux fixture"]
    fn a_uts_only_replacement_invalidates_the_lease_without_moving_the_observer() {
        let mut child = Command::new("/usr/bin/python3")
            .args(["-c", r#"
import ctypes, signal, sys
signal.alarm(15)
print('ready', flush=True)
if sys.stdin.readline() != 'change\n': raise SystemExit(1)
libc=ctypes.CDLL(None,use_errno=True)
assert libc.unshare(0x04000000)==0, ctypes.get_errno()
for call, text in [(libc.sethostname,b'pbps-804-observer'),(libc.setdomainname,b'pbps-804-domain.invalid')]:
    assert call(text,len(text))==0, ctypes.get_errno()
print('changed', flush=True)
sys.stdin.readline()
"#])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        assert_eq!(line, "ready\n");
        let process = ProcessLease::capture(child.id()).unwrap();
        let observer = std::fs::read_link("/proc/thread-self/ns/uts").unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"change\n")
            .unwrap();
        line.clear();
        output.read_line(&mut line).unwrap();
        assert_eq!(line, "changed\n");
        let refused = process.check().is_err();
        let namespace = File::open(format!("/proc/{}/ns/uts", child.id())).unwrap();
        let observed = read(&namespace).unwrap();
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        assert!(
            refused,
            "unchanged PID/executable must not hide UTS replacement"
        );
        assert_eq!(
            observed,
            (
                b"pbps-804-observer".to_vec(),
                b"pbps-804-domain.invalid".to_vec()
            )
        );
        assert_eq!(
            std::fs::read_link("/proc/thread-self/ns/uts").unwrap(),
            observer
        );
    }
}
