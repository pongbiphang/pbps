//! The connection seam engine-owned resolver SQL runs through, and the
//! socket type both drivers share.
//!
//! The peer-verified connection type (#607) was removed with the measured
//! resolver profiles that were its only users (#1636, DEC-1636.1). The
//! resolver reaches its scratch over the ordinary connection, with whatever
//! TLS its connection string asks for.

use crate::{Conn, DbError, Row};

/// A driver's socket, boxed so one client type serves every transport.
pub(crate) trait ByteStream:
    tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send
{
}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> ByteStream for T {}
pub(crate) type BoxedStream = Box<dyn ByteStream>;

mod query_sealed {
    pub trait Sealed {}
}

/// Engine-owned queries can use either connection type. This trait does not
/// classify SQL as read-only.
pub trait QueryConnection: query_sealed::Sealed {
    fn query<'a>(
        &'a mut self,
        sql: &'a str,
    ) -> impl std::future::Future<Output = Result<Vec<Row>, DbError>> + Send + 'a;
}

/// Engine-owned statements whose results are discarded, such as the DDL a
/// resolver runs on its scratch database. Like [`QueryConnection`], it
/// classifies no SQL.
pub trait ExecuteConnection: QueryConnection {
    /// Empty-parameter execution: PostgreSQL uses the single-command extended
    /// protocol. Callers retain framing commands at their typed boundaries.
    fn execute_extended<'a>(
        &'a mut self,
        sql: &'a str,
    ) -> impl std::future::Future<Output = Result<(), DbError>> + Send + 'a;

    fn execute<'a>(
        &'a mut self,
        sql: &'a str,
    ) -> impl std::future::Future<Output = Result<(), DbError>> + Send + 'a;
}

impl query_sealed::Sealed for Conn {}
/// The ordinary CLI connection, the only one these traits are implemented
/// for; they stay sealed so engine SQL cannot be pointed at another type.
impl QueryConnection for Conn {
    async fn query<'a>(&'a mut self, sql: &'a str) -> Result<Vec<Row>, DbError> {
        Conn::query(self, sql).await
    }
}

impl ExecuteConnection for Conn {
    async fn execute_extended<'a>(&'a mut self, sql: &'a str) -> Result<(), DbError> {
        Conn::execute_with(self, sql, &[]).await.map(|_| ())
    }

    async fn execute<'a>(&'a mut self, sql: &'a str) -> Result<(), DbError> {
        Conn::execute(self, sql).await
    }
}
