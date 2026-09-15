//! Operational work, separate from correctness risk (SPEC 14.1).
//!
//! PostgreSQL replaces storage during a rewrite; SQL Server can update rows
//! in place and omit unchanged physical writes. These answers describe the
//! operation's row-processing path, not bytes written or temporary disk space.
//! Locks and catalog context remain each engine's responsibility.

/// Whether the statement sends stored rows through a rewrite operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rewrite {
    /// A table rewrite or in-place row updates. An engine may avoid physically
    /// writing unchanged bytes; this does not make the operation independent
    /// of the number of rows. This answer does not promise a second table copy.
    Yes,
    /// No row rewrite. Validation can still read the table: see [`Reads`].
    No,
    /// The reason identifies the unmeasured or context-dependent case.
    Unknown(String),
}

/// Reading the table is independent of rewriting its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reads {
    EveryRow,
    Nothing,
    Unknown(String),
}
