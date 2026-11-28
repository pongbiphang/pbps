//! The attach rehearsal's answers, taken before the first diff (DEC-1671.2).
use super::Entry;
use pbps_diff::attach::Question;
use pbps_model::Hints;
use pbps_model::adoption::{AttachAnswer, AttachRehearsal, Relations};

/// One answer for each question. Whatever keeps the scratch from one leaves it
/// keeping nothing, and so the plan as it is without a resolver; the report
/// then offers no resolver for what one was asked about (DEC-1671.3).
#[allow(clippy::too_many_arguments)]
pub async fn rehearse(
    entry: &mut Entry<'_>,
    conn: &mut pbps_db::Conn,
    target: &crate::db::Target,
    before: pbps_diff::Side<'_>,
    hints: &Hints,
    questions: &[Question],
    relations: &Relations,
) -> anyhow::Result<Vec<AttachAnswer>> {
    let mut answers = answered(entry, conn, target, before, hints, questions, relations).await?;
    for question in questions {
        if !answers.iter().any(|a| a.uid == question.uid) {
            answers.push(AttachAnswer {
                uid: question.uid.clone(),
                parent: question.parent.clone(),
                rehearsal: AttachRehearsal {
                    parent: Vec::new(),
                    table: Vec::new(),
                    kept: Default::default(),
                },
            });
        }
    }
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
    questions: &[Question],
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
