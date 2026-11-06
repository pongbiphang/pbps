//! SQL Server creation trials on scratch (DEC-2143.9, mirroring PostgreSQL's
//! DEC-2465.1).
//!
//! Order issues the cursor; this module only runs the group it selects and
//! reports whether the engine took it. A group that fails because an object it
//! names does not exist yet waits for another group's success, so a name the
//! text only looked like a reference to never forces an order.
use super::{ReconstructError, Reconstruction, judged_the_statement};
use pbps_db::transport::ExecuteConnection;
use pbps_diff::order::scratch::Trials;

/// The server errors that say an object the statement names is not there
/// (yet): 208 an invalid object name, 4121 a function or column that cannot be
/// found, 4513 a schema-bound caller of something that is not bound (which the
/// callee's own creation can fix). Every other code is a verdict on the
/// declaration and is never retried. Messages are never read.
pub(super) fn candidate(code: Option<&str>) -> bool {
    matches!(code, Some("208" | "4121" | "4513"))
}

impl Reconstruction {
    /// Runs the parts of one range of `trials` until each has been created or
    /// none can make progress. A failed statement leaves nothing behind in
    /// autocommit (DEC-2374.2), but only the first statement of a group is a
    /// clean refusal: a later one failing leaves its earlier statements in
    /// place, so such a group is never retried.
    pub(super) async fn trial_range(
        &self,
        conn: &mut impl ExecuteConnection,
        mut trials: Trials<'_, crate::Mssql>,
    ) -> Result<pbps_diff::order::scratch::Progress<crate::Mssql>, ReconstructError> {
        let mut last: Vec<Option<String>> = vec![None; self.step_of_part.len()];
        while let Some(attempt) = trials.next_ready() {
            let part = attempt.position();
            let declaration = self.part_declaration(part);
            let mut failure = None;
            for (ordinal, statement) in self.lowered.groups()[part].statements().iter().enumerate()
            {
                if let Err(error) = conn.execute(&statement.sql).await {
                    failure = Some((ordinal, error));
                    break;
                }
            }
            let Some((ordinal, error)) = failure else {
                attempt.succeeded();
                continue;
            };
            let reason = error.to_string();
            if !judged_the_statement(&error) {
                return Err(ReconstructError::Interrupted {
                    declaration: declaration.to_owned(),
                    reason,
                });
            }
            if ordinal == 0 && candidate(error.server_error_code().as_deref()) {
                last[part] = Some(reason);
            } else {
                return Err(ReconstructError::Compile {
                    declaration: declaration.to_owned(),
                    reason,
                });
            }
        }
        let stalled: Vec<_> = trials
            .pending()
            .map(|part| {
                let mut line = self.part_declaration(part).to_owned();
                if let Some(reason) = &last[part] {
                    line.push_str(&format!(": {reason}"));
                }
                let waits: Vec<_> = trials
                    .blocked_by(part)
                    .map(|before| self.part_declaration(before))
                    .collect();
                if !waits.is_empty() {
                    line.push_str(&format!(" (waits for {})", waits.join(", ")));
                }
                line
            })
            .collect();
        if stalled.is_empty() {
            Ok(trials.finish().expect("a complete range finishes"))
        } else {
            Err(ReconstructError::Stalled(stalled.join("; ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::candidate;

    #[test]
    fn only_a_missing_object_class_is_retried() {
        for code in ["208", "4121", "4513"] {
            assert!(candidate(Some(code)), "{code}");
        }
        // A syntax error, a name that is no object at all, a permission, a
        // duplicate and a resource failure say nothing about order.
        for code in [
            "102", "111", "156", "207", "229", "262", "2714", "2760", "2788", "1205", "701",
        ] {
            assert!(!candidate(Some(code)), "{code}");
        }
        assert!(!candidate(None));
    }
}
