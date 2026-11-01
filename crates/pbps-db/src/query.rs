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
/// are additionally bounded by the engine's result column limit. The driver
/// retains wire buffers for individual rows, never the complete result.
#[derive(Debug)]
pub struct QueryEvidence {
    pub columns: Vec<QueryColumn>,
    pub rows: Vec<Vec<EvidenceCell>>,
    /// The session was closed at a bound; more rows may exist. It is unusable.
    pub truncated: bool,
}

/// Shared execution facts, not a saved-plan or report format.
#[derive(Debug)]
pub enum AssertionResult {
    Pass(QueryEvidence),
    Violation(QueryEvidence),
    InvalidResult { message: String },
    SqlError(crate::DbError),
    BoundaryViolation,
    Unanswerable(crate::DbError),
}

impl AssertionResult {
    /// Only a complete, tabular empty result establishes success (ADR-0019 §7).
    pub fn from_evidence(evidence: QueryEvidence) -> Self {
        // DEC-1165.1: zero columns cannot establish a tabular assertion result.
        if evidence.columns.is_empty() {
            Self::InvalidResult {
                message: "an assertion must return exactly one result set with at least one output column; use SELECT 1 ...".into(),
            }
        } else if evidence.rows.is_empty() && !evidence.truncated {
            Self::Pass(evidence)
        } else {
            Self::Violation(evidence)
        }
    }
}

pub(crate) fn text_cell(text: &str, remaining: &mut usize) -> EvidenceCell {
    let mut end = text.len().min(*remaining);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    *remaining -= end;
    EvidenceCell::Text {
        value: text[..end].to_owned(),
        truncated: end != text.len(),
    }
}

impl QueryEvidence {
    pub(crate) fn at_limit(&self, remaining: usize) -> bool {
        self.rows.len() == MAX_ROWS
            || remaining == 0
            || self.rows.last().is_some_and(|row| {
                row.iter().any(|cell| {
                    matches!(
                        cell,
                        EvidenceCell::Text {
                            truncated: true,
                            ..
                        }
                    )
                })
            })
    }
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

    /// PostgreSQL catalog OIDs inferred by Parse/Describe, in parameter order.
    /// Reading these does not bind values or execute the described statement.
    pub fn parameter_type_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.prepared.parameter_type_ids()
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

#[cfg(test)]
mod tests {
    use super::*;
    fn evidence(rows: Vec<Vec<EvidenceCell>>, truncated: bool) -> QueryEvidence {
        QueryEvidence {
            columns: vec![QueryColumn {
                name: "value".into(),
                type_name: "text".into(),
            }],
            rows,
            truncated,
        }
    }
    #[test]
    fn only_a_complete_tabular_empty_result_passes() {
        assert!(matches!(
            AssertionResult::from_evidence(evidence(vec![], false)),
            AssertionResult::Pass(_)
        ));
        for cell in [
            EvidenceCell::Null,
            EvidenceCell::Unrenderable,
            EvidenceCell::Text {
                value: String::new(),
                truncated: false,
            },
            EvidenceCell::Text {
                value: "false".into(),
                truncated: false,
            },
        ] {
            assert!(matches!(
                AssertionResult::from_evidence(evidence(vec![vec![cell]], false)),
                AssertionResult::Violation(_)
            ));
        }
        assert!(matches!(
            AssertionResult::from_evidence(evidence(vec![], true)),
            AssertionResult::Violation(_)
        ));
        let mut invalid = evidence(vec![], false);
        invalid.columns.clear();
        assert!(matches!(
            AssertionResult::from_evidence(invalid),
            AssertionResult::InvalidResult { .. }
        ));
    }
    #[test]
    fn evidence_budget_preserves_utf8_and_reveals_truncation() {
        let mut remaining = 4;
        assert_eq!(
            text_cell("€€", &mut remaining),
            EvidenceCell::Text {
                value: "€".into(),
                truncated: true
            }
        );
        assert_eq!(remaining, 1);
        assert_eq!(
            text_cell("x", &mut remaining),
            EvidenceCell::Text {
                value: "x".into(),
                truncated: false
            }
        );
        assert_eq!(remaining, 0);
    }
}
