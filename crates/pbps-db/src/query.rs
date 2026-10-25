//! Bounded diagnostic text, not a SQL value model or a serialized report.

/// Names and server types survive even an empty result.
#[derive(Debug)]
pub struct QueryColumn {
    pub name: String,
    pub type_name: String,
}

/// Unsupported binary types remain visible violations, without guessing their encoding.
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
