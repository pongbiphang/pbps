//! Peer-verified TLS connections for the resolver's transport foundation.
//!
//! This authenticates the TLS peer, not any hop behind it. Runtime admission
//! must separately bind the backend, every proxy hop, instance separation and
//! analysis run before accepting evidence or sending declarations (ADR-0016
//! decisions 4–5). This type cannot authorize a resolver or certify bindings.

use crate::{Conn, DbError, Driver, Param, Row};

/// An opaque, process-local identity minted after a successful verified
/// handshake. It is neither a server identity nor a saved-plan field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionId([u8; 32]);

/// Owns exactly one TLS connection with verified chain and expected peer name.
///
/// There is no reconnect, conversion from an ordinary connection, or mutable
/// access to the inner connection: replacing it must mint a new identity.
/// Ordinary `Conn::connect` defaults do not establish this property.
pub struct PeerVerifiedConn {
    conn: Conn,
    id: ConnectionId,
}

impl PeerVerifiedConn {
    /// PostgreSQL requires explicit `sslmode=require` and uses the native trust
    /// store. SQL Server requires encryption (also its default) and rejects
    /// `TrustServerCertificate=true`; native roots or its configured
    /// `TrustServerCertificateCA` verify the expected peer. A failed handshake
    /// never yields this type. Local/private channel qualification is separate.
    pub async fn connect(driver: Driver, connection_string: &str) -> Result<Self, DbError> {
        let conn = match driver {
            Driver::Mssql => Conn::Mssql(Box::new(
                crate::mssql::Conn::connect_verified(connection_string).await?,
            )),
            Driver::Postgres => Conn::Postgres(Box::new(
                crate::postgres::Conn::connect_verified(connection_string).await?,
            )),
        };
        Ok(Self {
            conn,
            id: ConnectionId(rand::random()),
        })
    }

    pub fn id(&self) -> ConnectionId {
        self.id
    }

    pub fn driver(&self) -> Driver {
        self.conn.driver()
    }

    /// Qualification reads use this same connection. Their SQL and meaning
    /// belong to engine adapters; returned rows are not resolver evidence.
    pub async fn query(&mut self, sql: &str) -> Result<Vec<Row>, DbError> {
        self.conn.query(sql).await
    }

    pub async fn query_with(
        &mut self,
        sql: &str,
        params: &[Param<'_>],
    ) -> Result<Vec<Row>, DbError> {
        self.conn.query_with(sql, params).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn insecure_options_are_refused_without_opening_a_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        for options in ["", " sslmode=disable", " sslmode=prefer"] {
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                PeerVerifiedConn::connect(
                    Driver::Postgres,
                    &format!("host=127.0.0.1 port={port} user=synthetic password=marker{options}"),
                ),
            )
            .await
            .expect("insecure options must refuse before a handshake");
            assert!(matches!(result, Err(DbError::Refused(_))));
        }
        for options in [
            "Encrypt=false",
            "Encrypt=no",
            "Encrypt=DANGER_PLAINTEXT",
            "TrustServerCertificate=true",
            "TrustServerCertificate=YES",
            "Encrypt=true;Encrypt=false",
            "TrustServerCertificate=false;TrustServerCertificate=true",
            "TrustServerCertificate=true;TrustServerCertificateCA=missing.pem",
        ] {
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                PeerVerifiedConn::connect(
                    Driver::Mssql,
                    &format!("Server=127.0.0.1,{port};User Id=synthetic;Password=marker;{options}"),
                ),
            )
            .await
            .expect("insecure options must refuse before a handshake");
            assert!(matches!(result, Err(DbError::Refused(_))), "{options}");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn ssl_refusal_cannot_downgrade_before_authentication() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 8];
            socket.read_exact(&mut request).await.unwrap();
            assert_eq!(request, [0, 0, 0, 8, 4, 210, 22, 47]);
            socket.write_all(b"N").await.unwrap();
            let mut after_refusal = Vec::new();
            socket.read_to_end(&mut after_refusal).await.unwrap();
            assert!(
                after_refusal.is_empty(),
                "authentication followed TLS refusal"
            );
        });
        assert!(
            PeerVerifiedConn::connect(
                Driver::Postgres,
                &format!(
                    "host=localhost port={port} user=synthetic password=marker sslmode=require"
                ),
            )
            .await
            .is_err()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tds_encryption_refusal_cannot_elicit_a_plaintext_login() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = [0; 8];
            socket.read_exact(&mut header).await.unwrap();
            assert_eq!(header[0], 0x12); // PRELOGIN, never LOGIN7.
            let mut payload = vec![0; usize::from(u16::from_be_bytes([header[2], header[3]])) - 8];
            socket.read_exact(&mut payload).await.unwrap();
            // A PRELOGIN reply with ENCRYPT_NOT_SUP. The client may reject
            // immediately or insist on TLS, but must never send LOGIN7.
            socket
                .write_all(&[0x04, 1, 0, 15, 0, 0, 1, 0, 1, 0, 6, 0, 1, 0xff, 2])
                .await
                .unwrap();
            let mut next = [0];
            if socket.read(&mut next).await.unwrap() != 0 {
                assert_eq!(
                    next[0], 0x12,
                    "plaintext authentication followed TLS refusal"
                );
            }
        });
        assert!(
            PeerVerifiedConn::connect(
                Driver::Mssql,
                &format!("Server=localhost,{port};User Id=synthetic;Password=marker;Encrypt=true"),
            )
            .await
            .is_err()
        );
        server.await.unwrap();
    }
}
