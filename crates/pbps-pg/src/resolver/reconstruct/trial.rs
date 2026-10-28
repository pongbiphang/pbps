//! PostgreSQL creation trials, isolated from committed observations (DEC-2465.1).
use super::{Progress, ReconstructError, Reconstruction, Step, judged_the_statement};
use pbps_db::{DbError, transport::ExecuteConnection};
use pbps_diff::order::scratch::Trials;

pub(super) enum Failure {
    Statement { error: DbError, statement: usize },
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
        let version =
            crate::roles::server_version_num(conn)
                .await
                .map_err(|e| ReconstructError::Read {
                    declaration: "scratch server version".into(),
                    reason: e.to_string(),
                })?;
        let pg16 = version / 10_000 == 16;
        let mut delta = Vec::with_capacity(cursor.pending().count());
        let mut last = vec![None::<super::refusal::Diagnosis>; self.steps.len()];
        let mut recoveries = last.clone();
        let mut overtakes = Vec::new();
        while let Some(attempt) = cursor.next_ready() {
            let at = attempt.position();
            let recovery = attempt.is_recovery();
            let overtaken = attempt.overtaken();
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
                    if !overtaken.is_empty() {
                        overtakes.push((step.declaration.clone(), delta.len(), overtaken));
                    }
                    delta.push((at, step));
                    attempt.succeeded();
                }
                Err(Failure::Statement { error, statement }) => {
                    let code = error.server_error_code();
                    let judged = judged_the_statement(&error);
                    let diagnosis = self.diagnosis(at, statement, error, &delta, overtaken);
                    if recovery {
                        recoveries[at] = Some(diagnosis);
                        return Err(self.pending_refusal(
                            version,
                            &cursor,
                            &last,
                            &recoveries,
                            overtakes,
                        ));
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
                        return Err(if judged {
                            let pending = super::refusal::Pending {
                                declaration: self.steps[at].declaration.clone(),
                                subject: self.steps[at].subject.clone(),
                                owner: self.steps[at].owner.clone(),
                                dependency: None,
                                last: Some(diagnosis),
                                recovery: None,
                                blocked: Vec::new(),
                                why: self.trial.explanation(at),
                            };
                            self.refusal(version, vec![pending], false, overtakes)
                        } else {
                            ReconstructError::Interrupted {
                                declaration: self.steps[at].declaration.clone(),
                                reason: diagnosis.error.to_string(),
                            }
                        });
                    }
                }
                Err(Failure::Terminal(error)) => return Err(error),
            }
        }
        let pending: Vec<_> = cursor.pending().collect();
        if !pending.is_empty() {
            return Err(self.pending_refusal(version, &cursor, &last, &recoveries, overtakes));
        }
        Ok((delta, cursor.finish().expect("complete range")))
    }
    fn pending_refusal(
        &self,
        version: i64,
        cursor: &Trials<'_, crate::Postgres>,
        last: &[Option<super::refusal::Diagnosis>],
        recoveries: &[Option<super::refusal::Diagnosis>],
        overtakes: Vec<(String, usize, Vec<String>)>,
    ) -> ReconstructError {
        let pending = cursor
            .pending()
            .map(|at| super::refusal::Pending {
                declaration: self.steps[at].declaration.clone(),
                subject: self.steps[at].subject.clone(),
                owner: self.steps[at].owner.clone(),
                dependency: None,
                last: last[at].clone(),
                recovery: recoveries[at].clone(),
                blocked: cursor
                    .blocked_by(at)
                    .map(|before| self.steps[before].declaration.clone())
                    .collect(),
                why: self.trial.explanation(at),
            })
            .collect();
        self.refusal(version, pending, true, overtakes)
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
