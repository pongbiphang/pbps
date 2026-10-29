//! Admission and execution of an already identified artifact.
use super::Attempt;
use crate::db::{self, Target};
use crate::deploy::{
    ApplyRequest, Deployment, apply_staged_under_lock, apply_under_lock, record_failed_apply,
    reject_non_transactional,
};
use anyhow::{Context as _, bail};
use pbps_config::Project;
use pbps_dialect::Dialect as _;

/// Everything `apply` does once it knows which artifact it is holding.
///
/// Returns through [`Attempt`] or through `Err`, and nothing else: the caller
/// turns both into the attempt event, so a refusal added here later cannot
/// bypass the hook by construction.
pub(in crate::deploy) fn apply_identified(
    project: &Project,
    target: &Target,
    request: &ApplyRequest<'_>,
    plan: &pbps_model::SavedPlan,
    plan_checksum: &str,
    selected: &crate::emission::SelectedDialect,
    operator: &str,
) -> anyhow::Result<Attempt> {
    pbps_cli::with_plan_dialect!(selected, |dialect| {
        let ApplyRequest {
            plan_path,
            approved_checksum,
            allow,
            staged,
            resume,
        } = *request;

        let admitted =
            pbps_diff::order::admit(plan.clone(), dialect, plan_path, approved_checksum)?;
        let plan = &admitted;

        // The mode lives in the file because that is what the gate approved; the
        // flag exists so that the CI configuration says out loud which kind of
        // deployment this is. A disagreement between them is somebody's mistake,
        // and running whichever was typed would be the tool choosing the loser.
        if plan.mode.is_staged() != staged {
            bail!(
                "`{}` is a {} plan and `--staged` was {}.\n\
             A staged plan runs outside a transaction and is applied with `--staged`; a \
             transactional one is not.",
                plan_path.display(),
                plan.mode,
                if staged { "given" } else { "not given" }
            );
        }
        if resume && !staged {
            bail!("--resume continues a staged apply; pass --staged as well");
        }

        // A cluster role's rename has no SQL, but its approved identity mapping
        // still has to be checked and recorded. Additions and removal of the last
        // role need the same check (421). SQL Server keeps its connection-free
        // empty path; PostgreSQL decides whether the role map moved under the lock.
        if plan.changes.is_empty() && dialect.manages_roles() {
            return Ok(Attempt::Empty);
        }

        // The gate. It can be this coarse precisely because the checksum pins the
        // plan: what `--allow destructive` approves is this plan's destruction and
        // no other (SPEC §7.3).
        let unapproved = plan.changes.unapproved_risks(allow);
        if !unapproved.is_empty() {
            let names: Vec<&str> = unapproved.iter().map(|r| r.as_str()).collect();
            bail!(
                "this plan carries risks that were not approved: {}.\n\
             Read {} and, if it is what you intend, re-run with --allow {}",
                names.join(", "),
                plan_path.display(),
                plan.changes
                    .gated_risks()
                    .iter()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }

        let statements = crate::statements(plan, dialect)?;
        crate::engine::require_transactional_rebuilds(target.driver(), &plan.changes, staged)?;
        // Only for a transactional plan. A staged one exists *because* its
        // statement cannot run inside a transaction (ADR-0003): `plan --db
        // --staged` accepts it deliberately, and rejecting it here would leave
        // staged execution refusing the only kind of change it is for.
        //
        // Nothing the T-SQL emitter writes is marked non-transactional yet, so this
        // is latent — which is the reason to fix it now rather than when the first
        // such statement arrives and the guard swallows every staged apply.
        if !staged {
            reject_non_transactional(&statements)?;
        }
        let names = pbps_model::plan_names::PlanNames::of(&plan.changes.changes, &plan.ids)?;
        let targets = crate::engine::rename_targets(target.driver(), &names);
        // A resolved plan's key, before anything connects: evidence sealed under
        // another key, or none, is no evidence here (#616, DEC-952.1).
        let keyed = pbps_cli::resolver::apply::Keyed::check(
            plan,
            pbps_cli::resolver::sealing::environment_key(project, target.environment()),
            &target.label,
        )?;
        let deployment = Deployment {
            project,
            target,
            plan,
            plan_checksum,
            statements: &statements,
            rename_targets: &targets,
            dialect,
            operator,
        };

        let (result, released) = db::runtime()?.block_on(async {
            let mut conn = db::connect(target).await?;

            // This capability is constant for the connection and needs no ledger
            // lock. Check before any writes, including staged resumes.
            crate::engine::permission_support(&mut conn, &plan.changes, &plan.ids).await?;
            crate::engine::generation_support(&mut conn, &plan.changes, &plan.ids).await?;
            // A resolved plan's cluster, constant for the connection like the
            // checks above: the evidence describes one cluster (#616, DEC-1685.1).
            let resolved = match &keyed {
                Some(keyed) => Some(keyed.open(&mut conn).await?),
                None => None,
            };

            // The lock comes first, before the checks and not after them: a
            // pre-flight that passed while another pipeline was mid-apply would
            // have been answered about a database that is already moving.
            crate::engine::lock(&mut conn, operator).await?;
            let result = if staged {
                apply_staged_under_lock(&mut conn, &deployment, resume)
                    .await
                    .map(Some)
            } else {
                apply_under_lock(&mut conn, &deployment, resolved.as_ref()).await
            };
            if let Err(error) = &result {
                record_failed_apply(&mut conn, &deployment, error).await;
            }
            // Released whatever happened. A lock left behind by a failed apply
            // blocks the very pipeline that would fix it.
            let released = crate::engine::unlock(&mut conn).await;
            Ok::<_, anyhow::Error>((result, released))
        })?;

        Ok(match result {
            Ok(Some(entry)) => Attempt::Applied { entry, released },
            Ok(None) => {
                released.context("the empty plan's deployment lock could not be released")?;
                Attempt::Empty
            }
            Err(error) => Attempt::Failed { error, released },
        })
    })
}
