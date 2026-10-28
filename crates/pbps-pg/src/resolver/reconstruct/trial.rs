//! PostgreSQL creation trials, isolated from committed observations (DEC-2465.1).
use super::{Progress, ReconstructError, Reconstruction, Step, judged_the_statement, with_remedy};
use pbps_db::{DbError, transport::ExecuteConnection};
use pbps_diff::order::scratch::Trials;

pub(super) enum Failure {
    Statement(DbError),
    Terminal(ReconstructError),
}

fn candidate(code: Option<&str>, foreign_key: bool) -> bool {
    matches!(code, Some("42P01" | "42883" | "42704" | "42703" | "3F000"))
        || (code == Some("42830") && foreign_key)
}

impl Reconstruction {
    pub(super) async fn trial_range(
        &self,
        conn: &mut impl ExecuteConnection,
        mut cursor: Trials<'_, crate::Postgres>,
    ) -> Result<(Vec<(usize, Step)>, Progress<crate::Postgres>), ReconstructError> {
        let pg16 =
            crate::roles::server_version_num(conn)
                .await
                .map_err(|e| ReconstructError::Read {
                    declaration: "scratch server version".into(),
                    reason: e.to_string(),
                })?
                / 10_000
                == 16;
        let mut delta = Vec::with_capacity(cursor.pending().count());
        let mut last = vec![None::<String>; self.steps.len()];
        while let Some(attempt) = cursor.next_ready() {
            let at = attempt.position();
            let recovery = attempt.is_recovery();
            conn.execute("SAVEPOINT pbps_creation_trial")
                .await
                .map_err(|_| ReconstructError::Transaction("savepoint"))?;
            match self.attempt_step(conn, at).await {
                Ok(step) => {
                    #[cfg(test)]
                    self.inject(conn, super::trial_tests::Point::Release, at)
                        .await?;
                    conn.execute("RELEASE SAVEPOINT pbps_creation_trial")
                        .await
                        .map_err(|_| ReconstructError::Transaction("release savepoint"))?;
                    delta.push((at, step));
                    attempt.succeeded();
                }
                Err(Failure::Statement(error)) => {
                    let code = error.server_error_code();
                    let diagnosis = format!(
                        "[{}] {}",
                        code.as_deref().unwrap_or("no SQLSTATE"),
                        with_remedy(&error)
                    );
                    if recovery {
                        return Err(ReconstructError::Compile {
                            declaration: self.steps[at].declaration.clone(),
                            reason: format!(
                                "{}; recovery refusal: {diagnosis}",
                                last[at].as_deref().expect("parked error")
                            ),
                        });
                    }
                    if candidate(code.as_deref(), self.steps[at].foreign_key)
                        || (pg16 && code.as_deref() == Some("42601"))
                    {
                        conn.execute("ROLLBACK TO SAVEPOINT pbps_creation_trial")
                            .await
                            .map_err(|_| ReconstructError::Transaction("roll back to savepoint"))?;
                        conn.execute("RELEASE SAVEPOINT pbps_creation_trial")
                            .await
                            .map_err(|_| ReconstructError::Transaction("release savepoint"))?;
                        last[at] = Some(diagnosis);
                        if code.as_deref() == Some("42601") {
                            attempt.park_syntax();
                        }
                    } else {
                        return Err(if judged_the_statement(&error) {
                            ReconstructError::Compile {
                                declaration: self.steps[at].declaration.clone(),
                                reason: diagnosis,
                            }
                        } else {
                            ReconstructError::Interrupted {
                                declaration: self.steps[at].declaration.clone(),
                                reason: diagnosis,
                            }
                        });
                    }
                }
                Err(Failure::Terminal(error)) => return Err(error),
            }
        }
        let pending: Vec<_> = cursor.pending().collect();
        if !pending.is_empty() {
            let declarations = pending
                .iter()
                .map(|&at| self.steps[at].declaration.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let reason = pending
                .iter()
                .map(|&at| {
                    let blocked = cursor
                        .blocked_by(at)
                        .map(|before| self.steps[before].declaration.as_str())
                        .collect::<Vec<_>>();
                    format!(
                        "{}: {}{}",
                        self.steps[at].declaration,
                        last[at]
                            .as_deref()
                            .unwrap_or("[not attempted; no SQLSTATE]"),
                        if blocked.is_empty() {
                            String::new()
                        } else {
                            format!("; hard prerequisites: {}", blocked.join(", "))
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ReconstructError::Compile {
                declaration: declarations,
                reason: format!(
                    "{reason}; no eligible creation made progress in this scratch range; range boundaries are fixed and providers outside it cannot be pulled across"
                ),
            });
        }
        Ok((delta, cursor.finish().expect("complete range")))
    }
}

#[cfg(test)]
mod tests {
    use super::candidate;
    #[test]
    fn only_missing_objects_and_typed_foreign_keys_are_candidates() {
        for code in ["42P01", "42883", "42704", "42703", "3F000"] {
            assert!(candidate(Some(code), false));
        }
        assert!(candidate(Some("42830"), true));
        for code in [
            "42830", "42601", "23505", "42501", "22012", "0A000", "57014", "53200", "08006",
        ] {
            assert!(!candidate(Some(code), false));
        }
        assert!(!candidate(None, true));
    }
}
