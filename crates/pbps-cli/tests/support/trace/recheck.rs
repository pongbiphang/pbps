//! Transparent publication-session routing shared by the PG and MSSQL controls.
/// Route a fresh publication recheck to another real engine (DEC-1685.1).
/// Callers choose the cutoff for their backend and scratch kind. Bytes pass
/// unchanged in both directions, including TLS and the engine's own replies.
pub(super) struct RecheckTarget {
    pub(super) address: std::net::SocketAddr,
    pub(super) count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RecheckTarget {
    pub(super) fn new(
        primary: (String, u16),
        alternate: (String, u16),
        primary_sessions: usize,
    ) -> Self {
        use std::net::{Shutdown, TcpListener, TcpStream};
        use std::sync::atomic::Ordering;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = count.clone();
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut copies = Vec::new();
            for client in listener.incoming() {
                let mut client = client.unwrap();
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                let index = seen.fetch_add(1, Ordering::AcqRel);
                let destination = if index < primary_sessions {
                    &primary
                } else {
                    &alternate
                };
                let mut server =
                    TcpStream::connect((destination.0.as_str(), destination.1)).unwrap();
                let mut client_read = client.try_clone().unwrap();
                let mut server_write = server.try_clone().unwrap();
                copies.push(std::thread::spawn(move || {
                    let _ = std::io::copy(&mut client_read, &mut server_write);
                    let _ = server_write.shutdown(Shutdown::Write);
                }));
                copies.push(std::thread::spawn(move || {
                    let _ = std::io::copy(&mut server, &mut client);
                    let _ = client.shutdown(Shutdown::Write);
                }));
            }
            for copy in copies {
                copy.join().unwrap();
            }
        });
        Self {
            address,
            count,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for RecheckTarget {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        let _ = std::net::TcpStream::connect(self.address);
        let result = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::Ordering;

    fn echo(count: usize, reply: u8) -> ((String, u16), std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = ("127.0.0.1".into(), listener.local_addr().unwrap().port());
        let thread = std::thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut received = Vec::new();
                stream.read_to_end(&mut received).unwrap();
                assert_eq!(received, b"\0wire\xff");
                stream.write_all(&[reply, 0, 255]).unwrap();
            }
        });
        (endpoint, thread)
    }

    fn ask(proxy: &RecheckTarget, reply: u8) {
        let mut stream = TcpStream::connect(proxy.address).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        stream.write_all(b"\0wire\xff").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut received = Vec::new();
        stream.read_to_end(&mut received).unwrap();
        assert_eq!(received, [reply, 0, 255]);
    }

    #[test]
    fn publication_route_preserves_all_bytes_and_counts_real_sessions() {
        let (primary, p) = echo(2, 1);
        let (alternate, a) = echo(1, 2);
        let proxy = RecheckTarget::new(primary, alternate, 2);
        for reply in [1, 1, 2] {
            ask(&proxy, reply);
        }
        assert_eq!(proxy.count.load(Ordering::Acquire), 3);
        drop(proxy);
        p.join().unwrap();
        a.join().unwrap();
    }

    #[test]
    fn cutoff_extremes_never_connect_to_the_unused_endpoint() {
        for (cutoff, first) in [(usize::MAX, true), (0, false)] {
            let (endpoint, server) = echo(1, 9);
            let unused = ("127.0.0.1".into(), 0);
            let (primary, alternate) = if first {
                (endpoint, unused)
            } else {
                (unused, endpoint)
            };
            let proxy = RecheckTarget::new(primary, alternate, cutoff);
            ask(&proxy, 9);
            assert_eq!(proxy.count.load(Ordering::Acquire), 1);
            drop(proxy);
            server.join().unwrap();
        }
    }
}
