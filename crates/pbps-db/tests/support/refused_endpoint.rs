/// Reserve a loopback endpoint that refuses connections for as long as the
/// returned socket is held. Dropping a listener first would let another test
/// or process take its port before the dial (#2759).
pub(crate) fn refused_endpoint() -> tokio::net::TcpSocket {
    let socket = tokio::net::TcpSocket::new_v4().expect("a socket");
    socket
        .bind("127.0.0.1:0".parse().expect("a loopback address"))
        .expect("a reserved endpoint");
    socket
}

#[test]
fn a_reserved_endpoint_refuses_connections_without_listening() {
    let socket = refused_endpoint();
    let addr = socket.local_addr().expect("the reserved address");
    // Windows retries refused SYNs, so its refusal can take several seconds.
    let error = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(30))
        .expect_err("an endpoint that never listens must refuse the dial");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
}

#[test]
fn another_listener_cannot_take_a_reserved_endpoint() {
    let socket = refused_endpoint();
    let addr = socket.local_addr().expect("the reserved address");
    let error = std::net::TcpListener::bind(addr)
        .expect_err("the endpoint must stay reserved until the socket is dropped");
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
}
