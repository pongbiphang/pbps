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
    let (name, kind) = match &intent {
        Intent::DropColumn { column, .. } => {
            (column.raw().to_string(), pbps_model::UidKind::Column)
        }
        Intent::DropTable { table, .. } => (table.raw().to_string(), pbps_model::UidKind::Table),
        _ => unreachable!("only the two drop commands record a row requirement"),
    };
    let target = target.resolve(project)?;
    cmd_intent_with(project, intent.into(), |resolved| {
        // The ordinary resolver selects the CLI intent's retired UID. An
        // absorbed repeat may only select an unambiguous audit record; neither
        // spelling is used to find the table on the target.
        let selected = resolved
            .dropped_tables
            .iter()
            .map(|(uid, name)| (uid, name.raw().to_string()))
            .chain(
                resolved
                    .dropped_columns
                    .iter()
                    .map(|(uid, name)| (uid, name.raw().to_string())),
            )
            .find(|(uid, address)| uid.kind() == kind && address == &name)
            .map(|(uid, _)| uid.clone());
        let ids = &mut resolved.ids;
        let candidates: Vec<_> = if let Some(uid) = selected {
            vec![uid]
        } else {
            ids.tombstones
                .iter()
                .filter(|(uid, t)| uid.kind() == kind && t.was == name)
                .map(|(uid, _)| uid.clone())
                .collect()
        };
        let [uid] = candidates.as_slice() else {
            bail!(
                "the drop does not identify exactly one tombstone; historical name reuse requires an unambiguous identity before changing its requirement"
            )
        };
        let table_uid = db::runtime()?.block_on(async {
            let mut conn = db::connect(&target).await?;
            if let Some(predicate) = require {
                let baseline = engine::latest(&mut conn).await?.ok_or_else(|| {
                    anyhow::anyhow!("recording a row requirement needs a target baseline")
                })?;
                let names = pbps_model::plan_names::PlanNames::of(&[], &baseline.snapshot.ids)?;
                let table_uid = if kind == pbps_model::UidKind::Table {
                    uid.clone()
                } else {
                    names
                        .column_at(uid, pbps_model::plan_names::At::Start)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "the requirement's identity is absent from the target baseline"
                            )
                        })?
                        .0
                        .clone()
                };
                let table = names
                    .table_at(&table_uid, pbps_model::plan_names::At::Start)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "the requirement's table identity is absent from the target baseline"
                        )
                    })?;
                engine::vet_drop_requirement(&mut conn, table, predicate).await?;
                return Ok(Some(table_uid));
            }
            Ok::<_, anyhow::Error>(None)
        })?;
        let tombstone = ids
            .tombstones
            .get_mut(uid)
            .expect("candidate came from this map");
        if clear {
            tombstone.require = None;
        } else if let Some(predicate) = require
            && !tombstone.require.as_ref().is_some_and(|r| {
                r.predicate == predicate && Some(&r.table_uid) == table_uid.as_ref()
            })
        {
            let ctx = context(project.root())?;
            tombstone.require = Some(pbps_model::DropRequirement {
                table_uid: table_uid.expect("recording a predicate resolved its target identity"),
                predicate: predicate.into(),
                required_at: ctx.today,
                operator: ctx.operator,
            });
        }
        Ok(())
    })
}
