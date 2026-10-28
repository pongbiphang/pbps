//! Recording is connected: unsafe predicates are refused before the ids write.
use super::*;

pub(super) fn record(
    project: &Project,
    intent: Intent,
    target: &TargetArgs,
    require: Option<&str>,
    clear: bool,
) -> anyhow::Result<()> {
    if require.is_none() && !clear {
        return cmd_intent(project, intent.into());
    }
    #[allow(clippy::wildcard_enum_match_arm)]
    let (name, table, kind) = match &intent {
        Intent::DropColumn { column, .. } => (
            column.raw().to_string(),
            column.table.clone(),
            pbps_model::UidKind::Column,
        ),
        Intent::DropTable { table, .. } => (
            table.raw().to_string(),
            table.clone(),
            pbps_model::UidKind::Table,
        ),
        _ => unreachable!("only the two drop commands record a row requirement"),
    };
    let target = target.resolve(project)?;
    cmd_intent_with(project, intent.into(), |ids| {
        let candidates: Vec<_> = ids
            .tombstones
            .iter()
            .filter(|(uid, t)| uid.kind() == kind && t.was == name)
            .map(|(uid, _)| uid.clone())
            .collect();
        let [uid] = candidates.as_slice() else {
            bail!(
                "the drop does not identify exactly one tombstone; historical name reuse requires an unambiguous identity before changing its requirement"
            )
        };
        db::runtime()?.block_on(async {
            let mut conn = db::connect(&target).await?;
            if let Some(predicate) = require {
                engine::vet_drop_requirement(&mut conn, &table, predicate).await?;
            }
            Ok::<_, anyhow::Error>(())
        })?;
        let tombstone = ids
            .tombstones
            .get_mut(uid)
            .expect("candidate came from this map");
        if clear {
            tombstone.require = None;
        } else if let Some(predicate) = require
            && !tombstone
                .require
                .as_ref()
                .is_some_and(|r| r.predicate == predicate)
        {
            let ctx = context(project.root())?;
            tombstone.require = Some(pbps_model::DropRequirement {
                predicate: predicate.into(),
                required_at: ctx.today,
                operator: ctx.operator,
            });
        }
        Ok(())
    })
}
