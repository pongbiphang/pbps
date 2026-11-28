//! Attach and parent-addition rehearsals share the code-goal run's lifecycle
//! (DEC-1671.2, DEC-1671.3).
use super::{Provenance, Standard, domain};
use pbps_db::Conn;
use pbps_diff::attach::Questions;
use pbps_model::adoption::{AttachAnswer, AttachedAnswer, Relations, Skipped};
use pbps_model::{Hints, Uid};
use pbps_pg::Postgres;

pub(super) struct Rehearsal<'a> {
    pub questions: &'a Questions,
    pub relations: &'a Relations,
    pub hints: &'a Hints,
    /// The dialect the plan is written with, whose statements for a parent's
    /// addition are the apply's.
    pub pg: &'a Postgres,
}

impl Rehearsal<'_> {
    /// What the engine says of each question. One the scratch cannot answer
    /// has nothing here, and keeps the plan's own drops.
    pub async fn run(&self, conn: &mut Conn) -> Vec<AttachAnswer> {
        let held = |name| self.relations.get(name).and_then(Option::as_ref);
        let mut answers = Vec::new();
        for question in &self.questions.attaches {
            let (Some(parent), Some(table)) = (held(&question.parent), held(&question.table))
            else {
                continue;
            };
            match pbps_pg::adoption::rehearse(conn, question, parent, table).await {
                Ok(Ok(rehearsal)) => answers.push(AttachAnswer::Attach(AttachedAnswer {
                    uid: question.uid.clone(),
                    parent: question.parent.clone(),
                    rehearsal,
                })),
                Ok(Err(why)) => answers.push(skipped(&question.uid, why)),
                Err(_) => {}
            }
        }
        for question in &self.questions.additions {
            let (Some(parent), Some(table)) = (held(&question.parent), held(&question.table))
            else {
                continue;
            };
            match pbps_pg::adoption::rehearse_addition(conn, self.pg, question, parent, table).await
            {
                Ok(Ok(found)) => answers.push(AttachAnswer::Addition(Box::new(found))),
                Ok(Err(why)) => answers.push(skipped(&question.uid, why)),
                Err(_) => {}
            }
        }
        answers
    }
}

fn skipped(table: &Uid, why: Skipped) -> AttachAnswer {
    AttachAnswer::Skipped {
        table: table.clone(),
        why,
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
    pg: &Postgres,
    questions: &Questions,
    relations: &Relations,
    standard: &Standard<pbps_pg::Postgres>,
    externals: Option<&str>,
    provenance: &Provenance,
) -> Result<Vec<AttachAnswer>, domain::Refused> {
    let rehearsal = Rehearsal {
        questions,
        relations,
        hints,
        pg,
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
