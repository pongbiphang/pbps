use super::*;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![kind, 0, 0, 0];
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

#[tokio::test]
async fn aggregate_traffic_limits_bound_small_frames_and_outbound_commands() {
    let (client, mut server) = tokio::io::duplex(128);
    let mut client = FramedIo::new(client);
    client.read_left = 10;
    server.write_all(&frame(1, b"ab")).await.unwrap();
    server.write_all(&frame(1, b"cd")).await.unwrap();
    let mut first = [0; 2];
    client.read_exact(&mut first).await.unwrap();
    assert_eq!(&first, b"ab");
    assert!(client.read_u8().await.is_err());
    assert!(client.write_all(b"later").await.is_err());

    let (client, mut server) = tokio::io::duplex(128);
    let mut client = FramedIo::new(client);
    client.write_left = 3;
    assert!(client.write_all(b"longer").await.is_err());
    let mut sent = [0; 3];
    server.read_exact(&mut sent).await.unwrap();
    assert_eq!(&sent, b"lon");
    assert!(client.read_u8().await.is_err());
}

#[tokio::test]
async fn fragmented_stdout_frames_preserve_bytes_and_stdin_is_unframed() {
    let (client, mut server) = tokio::io::duplex(32);
    let mut client = FramedIo::new(client);
    let task = tokio::spawn(async move {
        let mut request = [0; 5];
        server.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"login");
        for byte in frame(1, b"first").into_iter().chain(frame(1, b"second")) {
            server.write_all(&[byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
    });
    client.write_all(b"login").await.unwrap();
    let mut output = [0; 11];
    client.read_exact(&mut output).await.unwrap();
    assert_eq!(&output, b"firstsecond");
    task.await.unwrap();
    assert_eq!(
        client.read_u8().await.unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert!(client.write_all(b"later").await.is_err());
}

#[tokio::test]
async fn stderr_malformed_oversized_and_truncated_frames_poison_both_directions() {
    let mut bad_reserved = frame(1, b"payload");
    bad_reserved[2] = 1;
    for bytes in [
        frame(2, b"private source must never escape"),
        frame(0, b"wrong direction"),
        bad_reserved,
        vec![1, 0, 0, 0, 0, 32, 0, 0],
        vec![1, 0],
        vec![1, 0, 0, 0, 0, 0, 0, 4, b'x'],
    ] {
        let (client, mut server) = tokio::io::duplex(128);
        server.write_all(&bytes).await.unwrap();
        // Half-close output but keep accepting input: the wrapper itself must
        // reject writes after invalid framing, not rely on a dead peer.
        server.shutdown().await.unwrap();
        let mut client = FramedIo::new(client);
        let mut output = Vec::new();
        let error = client.read_to_end(&mut output).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "resolver private control stream is invalid"
        );
        assert!(client.write_all(b"later").await.is_err());
        assert!(client.read_u8().await.is_err());
        assert!(!output.windows(7).any(|w| w == b"private"));
    }
}

#[tokio::test]
async fn http_upgrade_preserves_early_frames_and_refuses_non_upgraded_replies() {
    for valid in [true, false] {
        let fixture = super::super::tests::Fixture::new();
        let api = fixture.client().await;
        let server = tokio::spawn(async move {
            let (mut socket, _) = fixture.listener.accept().await.unwrap();
            let request = super::super::tests::request_line(&mut socket).await;
            assert!(request.contains("/attach?stream=1&stdin=1&stdout=1&stderr=1"));
            let header = if valid {
                "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n"
            } else {
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
            };
            let mut reply = header.as_bytes().to_vec();
            if valid {
                reply.extend(frame(1, b"early"));
            }
            socket.write_all(&reply).await.unwrap();
            if valid {
                let mut request = [0; 5];
                socket.read_exact(&mut request).await.unwrap();
                assert_eq!(&request, b"login");
            }
        });
        let result = api.attach_inner(&"a".repeat(64)).await;
        if valid {
            let mut stream = result.unwrap();
            let mut output = [0; 5];
            stream.read_exact(&mut output).await.unwrap();
            assert_eq!(&output, b"early");
            stream.write_all(b"login").await.unwrap();
        } else {
            assert!(matches!(result, Err(Error::Response)));
        }
        server.await.unwrap();
    }
}

#[tokio::test]
async fn an_immediate_peer_without_native_daemon_qualification_cannot_attach() {
    let fixture = super::super::tests::Fixture::new();
    let api = fixture.client().await;
    assert!(matches!(
        api.attach(&"a".repeat(64)).await,
        Err(Error::NativeDaemon)
    ));
}
