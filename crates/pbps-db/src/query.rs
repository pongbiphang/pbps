//! Bounded diagnostic text, not a SQL value model or a serialized report.

/// Names and server types survive even an empty result.
#[derive(Debug)]
pub struct QueryColumn {
    pub name: String,
    pub type_name: String,
}

/// Types without a client renderer remain visible, without guessing their encoding.
#[derive(Debug, PartialEq, Eq)]
pub enum EvidenceCell {
    Null,
    Text { value: String, truncated: bool },
    Unrenderable,
}

/// At most 20 rows and 64 KiB of rendered cell text. Metadata and cell slots
/// are additionally bounded by PostgreSQL's result column limit. The driver
/// retains wire buffers for individual rows, never the complete result.
#[derive(Debug)]
pub struct QueryEvidence {
    pub columns: Vec<QueryColumn>,
    pub rows: Vec<Vec<EvidenceCell>>,
    /// The session was closed at a bound; more rows may exist. It is unusable.
    pub truncated: bool,
}

pub(crate) const MAX_ROWS: usize = 20;
pub(crate) const MAX_BYTES: usize = 64 * 1024;

/// A description and its exclusive connection, so runner checks precede execution
/// on that same session. No driver statement type crosses the connection seam.
pub struct PreparedQuery<'a> {
    pub(crate) conn: &'a mut crate::postgres::Conn,
    pub(crate) prepared: crate::postgres::PreparedQuery,
}

impl PreparedQuery<'_> {
    pub fn columns(&self) -> &[QueryColumn] {
        &self.prepared.columns
    }

    /// PostgreSQL catalog OIDs, in result column order.
    pub fn type_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.prepared.type_ids()
    }

    /// Runner-owned catalog reads, before executing the described statement.
    pub async fn query(&mut self, sql: &str) -> Result<Vec<crate::Row>, crate::DbError> {
        Ok(self
            .conn
            .query(sql)
            .await?
            .into_iter()
            .map(crate::Row::Postgres)
            .collect())
    }

    /// Streams bounded evidence; truncation or timeout closes the session.
    /// Callers must discard an unfinished connection.
    pub async fn query_bounded(
        self,
        timeout: std::time::Duration,
    ) -> Result<QueryEvidence, crate::DbError> {
        self.conn
            .query_prepared_bounded(self.prepared, timeout)
            .await
    }
}
