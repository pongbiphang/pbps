//! Plan-time retirement evidence, saved as ordinary informational findings.
//! Git stays here; the policy owns identity and the counterfactual (DEC-2398.2).
use crate::baseline::{self, ancestry::Evidence};
use anyhow::Context as _;
use pbps_policy::consumers::{Bindings, BoundContract};
use std::collections::BTreeMap;

pub(crate) fn findings(
    cs: &pbps_model::ChangeSet,
    project: &pbps_config::Project,
    dialect: &dyn pbps_dialect::Dialect,
    ctx: &pbps_policy::Context,
    sides: [pbps_diff::Side<'_>; 2],
    revision: Option<&pbps_model::Text>,
) -> Vec<(usize, pbps_model::Finding)> {
    // An empty baseline has no previous consumer interfaces to retire.
    if cs.changes.is_empty()
        || (sides[0].schema.tables.is_empty() && sides[0].schema.modules.is_empty())
    {
        return Vec::new();
    }
    assess(cs, project, dialect, ctx, sides, revision).unwrap_or_else(|error| {
        vec![(0, pbps_model::Finding::new(
            "consumer.retirement-unavailable", pbps_model::Severity::Note,
            format!("Consumer retirement evidence is unavailable: {}. This does not establish that no retirements were relied on.", pbps_model::escape::Escaped(&format!("{error:#}"))),
            None,
        ))]
    })
}

fn assess(
    cs: &pbps_model::ChangeSet,
    project: &pbps_config::Project,
    dialect: &dyn pbps_dialect::Dialect,
    ctx: &pbps_policy::Context,
    sides: [pbps_diff::Side<'_>; 2],
    revision: Option<&pbps_model::Text>,
) -> anyhow::Result<Vec<(usize, pbps_model::Finding)>> {
    let revision = revision
        .context("the baseline has no recorded Git commit")?
        .raw();
    let evidence = baseline::ancestry::commit(&project.root, revision);
    anyhow::ensure!(
        matches!(evidence, Evidence::Contains),
        "baseline Git commit `{revision}` is unavailable: {evidence:?}; fetch readable complete history"
    );
    let mut history = History {
        project,
        dialect,
        bindings: BTreeMap::new(),
    };
    let old: Vec<_> = history.bindings(revision)?.contracts().cloned().collect();
    if old.is_empty() {
        return Ok(Vec::new());
    }
    let policy_sides = sides.map(|s| pbps_policy::consumers::Side {
        schema: s.schema,
        ids: s.ids,
    });
    let current = Bindings::resolve(&project.config.consumers, &policy_sides, &|id| {
        crate::policy_module_id(id, dialect)
    })
    .map_err(|errors| anyhow::anyhow!(errors.join("; ")))?;
    let policies = project.config.policies();
    let unchanged = pbps_diff::schema_diff::restoration_comparison(sides[0], sides[1], dialect);
    let mut out = Vec::new();
    // Ask the existing rule, including its severity, suppression and restoration
    // decisions. A removal unrelated to an error in this plan is not evidence.
    for contract in old.iter().filter(|entry| !current.contains(entry)) {
        let errors: Vec<_> = pbps_policy::consumers::plan(
            cs,
            &contract.bindings(),
            &policy_sides,
            &policies,
            ctx,
            &unchanged,
        )
        .into_iter()
        .filter(|(_, f)| f.severity == pbps_model::Severity::Error)
        .collect();
        if errors.is_empty() {
            continue;
        }
        let candidate = baseline::git(&project.root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
        let candidate = candidate.trim();
        anyhow::ensure!(
            matches!(
                baseline::ancestry::check_between(&project.root, revision, candidate),
                Evidence::Contains
            ),
            "Git history does not establish the baseline as an ancestor of the candidate; fetch the complete history"
        );
        let source = if history.bindings(candidate)?.contains(contract) {
            "working tree, not committed".to_owned()
        } else {
            format!(
                "commit `{}`",
                history.retiring_commit(revision, candidate, contract)?
            )
        };
        for (index, finding) in errors {
            out.push((
                index,
                pbps_model::Finding::new(
                    "consumer.retired",
                    pbps_model::Severity::Note,
                    format!(
                        "Retirement relied on: `{}` ({source}). Without retirement [{}]: {}",
                        pbps_model::escape::Escaped(&contract.declaration()),
                        finding.id,
                        finding.message
                    ),
                    finding.subject,
                ),
            ));
        }
    }
    Ok(out)
}

struct History<'a> {
    project: &'a pbps_config::Project,
    dialect: &'a dyn pbps_dialect::Dialect,
    bindings: BTreeMap<String, Bindings>,
}
impl History<'_> {
    fn bindings(&mut self, revision: &str) -> anyhow::Result<&Bindings> {
        if !self.bindings.contains_key(revision) {
            let mut bound = Bindings::default();
            if let Some(config) = baseline::config_at(self.project, revision)?
                && !config.consumers.is_empty()
            {
                let base = baseline::load(
                    self.project,
                    &baseline::Source::Git {
                        rev: revision.into(),
                    },
                    self.dialect,
                )?;
                let side = pbps_policy::consumers::Side {
                    schema: &base.schema,
                    ids: &base.ids,
                };
                bound = Bindings::resolve(&config.consumers, &[side; 2], &|id| {
                    crate::policy_module_id(id, self.dialect)
                })
                .map_err(|errors| {
                    anyhow::anyhow!("consumer contracts at `{revision}`: {}", errors.join("; "))
                })?;
            }
            self.bindings.insert(revision.into(), bound);
        }
        Ok(&self.bindings[revision])
    }

    fn retiring_commit(
        &mut self,
        baseline: &str,
        candidate: &str,
        contract: &BoundContract,
    ) -> anyhow::Result<String> {
        // One rule for linear and merged history, including reintroduction:
        // Git's most recent topological commit absent here, present in a parent.
        let revisions = baseline::git(
            &self.project.root,
            &[
                "rev-list",
                "--topo-order",
                "--parents",
                &format!("{baseline}..{candidate}"),
            ],
        )?;
        for row in revisions.lines() {
            let mut ids = row.split_whitespace();
            let commit = ids.next().context("Git returned an empty revision")?;
            if self.bindings(commit)?.contains(contract) {
                continue;
            }
            for parent in ids {
                if self.bindings(parent)?.contains(contract) {
                    return Ok(commit.into());
                }
            }
        }
        anyhow::bail!("no retiring commit was found in the available baseline..candidate history")
    }
}
