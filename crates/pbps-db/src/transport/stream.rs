//! Database protocol over an already opened stream. Provisioning and channel
//! admission stay outside this crate; accepting bytes does not certify them.

use super::{ConnectionId, QueryConnection, query_sealed};
use crate::{Conn, DbError, Driver, Row};

pub trait ByteStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> ByteStream for T {}
pub(crate) type BoxedStream = Box<dyn ByteStream>;

/// Fresh run credentials, supplied independently from the destination stream.
/// No URL, host lookup, reconnect, logging or Debug rendering is available.
pub struct StreamLogin {
    pub user: String,
    pub password: String,
    pub database: String,
}

/// An authenticated database protocol session, **not** a peer-authenticated
/// transport. Only a separately qualified private runtime channel may use
/// this for resolver traffic. It is never a fallback for failed remote TLS.
pub struct StreamConn {
    conn: Conn,
    id: ConnectionId,
}

impl StreamConn {
    pub async fn connect(
        driver: Driver,
        stream: impl ByteStream + 'static,
        login: StreamLogin,
    ) -> Result<Self, DbError> {
        let stream: BoxedStream = Box::new(stream);
        let conn = match driver {
            Driver::Postgres => Conn::Postgres(Box::new(
                crate::postgres::Conn::connect_stream(stream, login).await?,
            )),
            Driver::Mssql => Conn::Mssql(Box::new(
                crate::mssql::Conn::connect_stream(stream, login).await?,
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
    pub async fn query(&mut self, sql: &str) -> Result<Vec<Row>, DbError> {
        self.conn.query(sql).await
    }
    pub async fn execute(&mut self, sql: &str) -> Result<(), DbError> {
        self.conn.execute(sql).await
    }
}

impl query_sealed::Sealed for StreamConn {}
impl QueryConnection for StreamConn {
    async fn query<'a>(&'a mut self, sql: &'a str) -> Result<Vec<Row>, DbError> {
        StreamConn::query(self, sql).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt as _;

    #[tokio::test]
    async fn both_protocols_use_the_supplied_stream_and_refuse_a_truncated_handshake() {
        for driver in [Driver::Postgres, Driver::Mssql] {
            let (stream, mut server) = tokio::io::duplex(16384);
            let observe = tokio::spawn(async move {
                let mut first = [0; 8];
                server.read_exact(&mut first).await.unwrap();
                match driver {
                    Driver::Postgres => assert_eq!(&first[4..8], &[0, 3, 0, 0]),
                    Driver::Mssql => assert_eq!(first[0], 0x12),
                }
                // Closing during the protocol handshake cannot yield a session.
            });
            let login = StreamLogin {
                user: "fixture".into(),
                password: "synthetic".into(),
                database: "fixture".into(),
            };
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    StreamConn::connect(driver, stream, login)
                )
                .await
                .unwrap()
                .is_err()
            );
            observe.await.unwrap();
        }
    }
}
