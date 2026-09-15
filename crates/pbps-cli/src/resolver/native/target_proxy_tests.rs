//! The proxy intentionally terminates fixture TLS and alters a managed-only
//! query on its plaintext backend link. Native admission must refuse it even
//! though the real driver accepts TLS and reads a well-formed engine result.

use super::*;
use rustls::pki_types::pem::PemObject as _;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn invalid() -> std::io::Error {
    std::io::Error::other("unsupported fixture protocol frame")
}

fn packet(socket: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut header = [0; 8];
    socket.read_exact(&mut header)?;
    let length = u16::from_be_bytes([header[2], header[3]]) as usize;
    if length <= 8 {
        return Err(invalid());
    }
    let mut data = vec![0; length];
    data[..8].copy_from_slice(&header);
    socket.read_exact(&mut data[8..])?;
    Ok(data)
}

fn encryption(packet: &mut [u8], value: u8) -> std::io::Result<u8> {
    let payload = &mut packet[8..];
    for index in (0..payload.len()).step_by(5) {
        if payload[index] == 0xff {
            break;
        }
        if index + 5 > payload.len() {
            return Err(invalid());
        }
        if payload[index] == 1 {
            let offset = u16::from_be_bytes([payload[index + 1], payload[index + 2]]) as usize;
            if u16::from_be_bytes([payload[index + 3], payload[index + 4]]) != 1 {
                return Err(invalid());
            }
            let entry = payload.get_mut(offset).ok_or_else(invalid)?;
            return Ok(std::mem::replace(entry, value));
        }
    }
    Err(invalid())
}

struct Handshake<'a> {
    socket: &'a mut TcpStream,
    pending: std::io::Cursor<Vec<u8>>,
}

impl Read for Handshake<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.pending.position() == self.pending.get_ref().len() as u64 {
            let frame = packet(self.socket)?;
            if frame[0] != 0x12 {
                return Err(invalid());
            }
            self.pending = std::io::Cursor::new(frame[8..].to_vec());
        }
        self.pending.read(output)
    }
}

impl Write for Handshake<'_> {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        let count = input.len().min(65527);
        let length = ((count + 8) as u16).to_be_bytes();
        self.socket
            .write_all(&[0x12, 1, length[0], length[1], 0, 0, 1, 0])?;
        self.socket.write_all(&input[..count])?;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.socket.flush()
    }
}

fn relay(
    mut client: TcpStream,
    upstream: SocketAddr,
    driver: Driver,
    config: Arc<rustls::ServerConfig>,
    changed: Arc<AtomicBool>,
) -> std::io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(5)))?;
    client.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut backend = TcpStream::connect_timeout(&upstream, Duration::from_secs(5))?;
    backend.set_read_timeout(Some(Duration::from_secs(5)))?;
    backend.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut tls = rustls::ServerConnection::new(config).map_err(|_| invalid())?;
    if driver == Driver::Postgres {
        let mut request = [0; 8];
        client.read_exact(&mut request)?;
        if request != [0, 0, 0, 8, 4, 210, 22, 47] {
            return Err(invalid());
        }
        client.write_all(b"S")?;
        while tls.is_handshaking() {
            tls.complete_io(&mut client)?;
        }
    } else {
        let mut request = packet(&mut client)?;
        encryption(&mut request, 2)?; // Backend explicitly negotiates no TLS.
        backend.write_all(&request)?;
        let mut response = packet(&mut backend)?;
        if encryption(&mut response, 1)? != 2 {
            return Err(invalid());
        }
        client.write_all(&response)?; // Frontend explicitly requires TLS.
        let mut framed = Handshake {
            socket: &mut client,
            pending: std::io::Cursor::new(Vec::new()),
        };
        while tls.is_handshaking() {
            tls.complete_io(&mut framed)?;
        }
        if framed.pending.position() != framed.pending.get_ref().len() as u64 {
            return Err(invalid());
        }
    }
    client.set_nonblocking(true)?;
    backend.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut plaintext = Vec::new();
    let mut startup = driver == Driver::Postgres;
    while Instant::now() < deadline {
        match tls.read_tls(&mut client) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                tls.process_new_packets().map_err(|_| invalid())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => (),
            Err(error) => return Err(error),
        }
        let mut bytes = [0; 8192];
        loop {
            match tls.reader().read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => plaintext.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
            if plaintext.len() > 65536 {
                return Err(invalid());
            }
        }
        loop {
            let length = if driver == Driver::Mssql {
                if plaintext.len() < 8 {
                    break;
                }
                u16::from_be_bytes([plaintext[2], plaintext[3]]) as usize
            } else if startup {
                if plaintext.len() < 4 {
                    break;
                }
                u32::from_be_bytes(plaintext[..4].try_into().unwrap()) as usize
            } else {
                if plaintext.len() < 5 {
                    break;
                }
                1 + u32::from_be_bytes(plaintext[1..5].try_into().unwrap()) as usize
            };
            if length == 0 || length > 65536 {
                return Err(invalid());
            }
            if plaintext.len() < length {
                break;
            }
            let mut frame: Vec<_> = plaintext.drain(..length).collect();
            startup = false;
            let pattern: Vec<u8> = if driver == Driver::Postgres {
                b"CAST(608 AS INT)".to_vec()
            } else {
                "CAST(608 AS INT)"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect()
            };
            if let Some(index) = frame
                .windows(pattern.len())
                .position(|part| part == pattern)
            {
                frame[index + if driver == Driver::Postgres { 7 } else { 14 }] = b'9';
                changed.store(true, Ordering::SeqCst);
            }
            backend.write_all(&frame)?;
        }
        match backend.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(count) => tls.writer().write_all(&bytes[..count])?,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => (),
            Err(error) => return Err(error),
        }
        while tls.wants_write() {
            match tls.write_tls(&mut client) {
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "fixture relay deadline",
    ))
}

pub(super) async fn unprotected_backend_cannot_inherit_frontend_tls(
    driver: Driver,
    primary: &str,
    upstream: SocketAddr,
    service: u32,
) {
    let certificate =
        std::env::var("PBPS_NATIVE_PROXY_CERT").expect("owned fixture TLS certificate");
    let key = std::env::var("PBPS_NATIVE_PROXY_KEY").expect("owned fixture TLS key");
    let certificate = rustls::pki_types::CertificateDer::from_pem_file(certificate).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::from_pem_file(key).unwrap();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let address = match driver {
        Driver::Postgres => primary.replace(
            &format!("port={}", upstream.port()),
            &format!("port={port}"),
        ),
        Driver::Mssql => primary.replace(&format!(",{};", upstream.port()), &format!(",{port};")),
    };
    assert_ne!(address, primary);
    let changed = Arc::new(AtomicBool::new(false));
    let report = changed.clone();
    let proxy = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        relay(socket, upstream, driver, Arc::new(config), report)
    });
    let mut connection = PeerVerifiedConn::connect(driver, &address).await.unwrap();
    let rows = connection
        .query("SELECT CAST(608 AS INT) AS value")
        .await
        .unwrap();
    assert_eq!(
        rows[0].try_get::<i32>("value").unwrap(),
        Some(609),
        "the plaintext backend link must actually permit managed-only query tampering"
    );
    assert!(changed.load(Ordering::SeqCst));
    assert!(
        matches!(
            NativeTarget::establish(connection, service).await,
            Err(NativeTargetError::SocketOwner)
        ),
        "a valid frontend TLS hop cannot qualify a modified plaintext backend response"
    );
    // The PostgreSQL driver's drop aborts an async task. Keep the runtime
    // polling while joining, so that task can close its actual TCP stream.
    tokio::task::spawn_blocking(move || proxy.join().unwrap())
        .await
        .unwrap()
        .unwrap();
}
