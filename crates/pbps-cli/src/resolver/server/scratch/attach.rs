//! Attach rehearsals share the code-goal run's lifecycle (DEC-1671.2).
use super::{Provenance, Standard, domain};
use pbps_db::Conn;
use pbps_diff::attach::Question;
use pbps_model::Hints;
use pbps_model::adoption::{AttachAnswer, Relations};

pub(super) struct Rehearsal<'a> {
    pub questions: &'a [Question],
    pub relations: &'a Relations,
    pub hints: &'a Hints,
}

impl Rehearsal<'_> {
    pub async fn run(&self, conn: &mut Conn) -> Vec<AttachAnswer> {
        let mut answers = Vec::new();
        for question in self.questions {
            let held = |name| self.relations.get(name).and_then(Option::as_ref);
            let (Some(parent), Some(table)) = (held(&question.parent), held(&question.table))
            else {
                continue;
            };
            // A question the scratch cannot answer keeps the plan's own drops.
            if let Ok(Some(rehearsal)) =
                pbps_pg::adoption::rehearse(conn, question, parent, table).await
            {
                answers.push(AttachAnswer {
                    uid: question.uid.clone(),
                    parent: question.parent.clone(),
                    rehearsal,
                });
            }
        }
        answers
    }
}

/// Answers `questions` on the scratch, which first compiles what `before`
/// declares of types and functions.
#[allow(clippy::too_many_arguments)]
pub async fn rehearse(
    scratch: &str,
    target: &str,
    identity: &str,
    before: pbps_diff::Side<'_>,
    hints: &Hints,
    questions: &[Question],
    relations: &Relations,
    standard: &Standard<pbps_pg::Postgres>,
    externals: Option<&str>,
    provenance: &Provenance,
) -> Result<Vec<AttachAnswer>, domain::Refused> {
    let rehearsal = Rehearsal {
        questions,
        relations,
        hints,
    };
    domain::run(
        scratch,
        target,
        identity,
        before,
        standard,
        externals,
        provenance,
        domain::Goal::Attach(&rehearsal),
    )
    .await
    .map(|answers| answers.attach)
}
