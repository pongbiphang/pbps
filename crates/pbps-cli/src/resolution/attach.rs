//! The attach and parent-addition rehearsals' answers, taken before the first
//! diff (DEC-1671.2, DEC-1671.3).
use super::Entry;
use pbps_diff::attach::Questions;
use pbps_model::Hints;
use pbps_model::adoption::{AttachAnswer, Relations, Skipped};
use pbps_pg::Postgres;

/// One answer, or the reason for none, for each table with something to ask or
/// to say. Whatever keeps the scratch from a question leaves the plan as it is
/// without a resolver, and the report says so (DEC-1671.3).
#[allow(clippy::too_many_arguments)]
pub async fn rehearse(
    entry: &mut Entry<'_>,
    conn: &mut pbps_db::Conn,
    target: &crate::db::Target,
    before: pbps_diff::Side<'_>,
    hints: &Hints,
    pg: &Postgres,
    questions: &Questions,
    relations: &Relations,
) -> anyhow::Result<Vec<AttachAnswer>> {
    let asked: Vec<_> = questions
        .attaches
        .iter()
        .map(|q| &q.uid)
        .chain(questions.additions.iter().map(|q| &q.uid))
        .collect();
    // Nothing to run opens no scratch.
    let mut answers = if asked.is_empty() {
        Vec::new()
    } else {
        answered(entry, conn, target, before, hints, pg, questions, relations).await?
    };
    let unanswered: Vec<_> = asked
        .into_iter()
        .filter(|table| !answers.iter().any(|a| a.table() == *table))
        .map(|table| (table.clone(), Skipped::Scratch))
        .collect();
    answers.extend(
        unanswered
            .into_iter()
            .chain(questions.skipped.iter().cloned())
            .map(|(table, why)| AttachAnswer::Skipped { table, why }),
    );
    Ok(answers)
}

/// What the scratch answers. A cleanup it cannot confirm is the one failure
/// that is not an answer.
#[allow(clippy::too_many_arguments)]
async fn answered(
    entry: &mut Entry<'_>,
    conn: &mut pbps_db::Conn,
    target: &crate::db::Target,
    before: pbps_diff::Side<'_>,
    hints: &Hints,
    pg: &Postgres,
    questions: &Questions,
    relations: &Relations,
) -> anyhow::Result<Vec<AttachAnswer>> {
    use pbps_cli::resolver::server::{ProduceError, scratch};
    let Ok(identity) =
        pbps_cli::resolver::server::target_identity(&pbps_pg::Postgres::new(), conn).await
    else {
        return Ok(Vec::new());
    };
    let Ok(open) = entry.open::<pbps_pg::Postgres>(&identity).await else {
        return Ok(Vec::new());
    };
    match scratch::attach::rehearse(
        &open.connection,
        target.connection(),
        &identity,
        before,
        hints,
        pg,
        questions,
        relations,
        &open.standard,
        open.externals.as_deref(),
        &open.provenance,
    )
    .await
    {
        Ok(answers) => Ok(answers),
        Err(scratch::domain::Refused::Unavailable(error @ ProduceError::Cleanup(_))) => {
            Err(error.into())
        }
        Err(_) => Ok(Vec::new()),
    }
}
