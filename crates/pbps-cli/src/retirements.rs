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
    let policy_sides = sides.map(|s| pbps_policy::consumers::Side {
        schema: s.schema,
        ids: s.ids,
    });
    let mut history = History {
        project,
        dialect,
        anchor: policy_sides[0],
        revisions: BTreeMap::new(),
    };
    let old: Vec<_> = history.bindings(revision)?.0.contracts().cloned().collect();
    if old.is_empty() {
        return Ok(Vec::new());
    }
    let current = Bindings::resolve(&project.config.consumers, &policy_sides, &|id| {
        crate::policy_module_id(id, dialect)
    })
    .map_err(|errors| anyhow::anyhow!(errors.join("; ")))?;
    let policies = project.config.policies();
    let unchanged = pbps_diff::schema_diff::restoration_comparison(sides[0], sides[1], dialect);
    let mut out = Vec::new();
    // Ask the existing rule, including its severity, suppression and restoration
    // decisions. A removal unrelated to an error in this plan is not evidence.
    for contract in &old {
        let contract = contract
            .resolve(&policy_sides, &|id| crate::policy_module_id(id, dialect))
            .map_err(|errors| anyhow::anyhow!(errors.join("; ")))?;
        if current.contains(&contract) {
            continue;
        }
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
        let source = if history.contains(candidate, &contract)? {
            "working tree, not committed".to_owned()
        } else {
            format!(
                "commit `{}`",
                history.retiring_commit(revision, candidate, &contract)?
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

struct Historical {
    consumers: pbps_policy::consumers::Consumers,
    baseline: baseline::Baseline,
}
struct History<'a> {
    project: &'a pbps_config::Project,
    dialect: &'a dyn pbps_dialect::Dialect,
    anchor: pbps_policy::consumers::Side<'a>,
    revisions: BTreeMap<String, Option<Historical>>,
}
impl History<'_> {
    fn bindings(
        &mut self,
        revision: &str,
    ) -> anyhow::Result<(Bindings, [pbps_policy::consumers::Side<'_>; 2])> {
        if !self.revisions.contains_key(revision) {
            let historical = match baseline::config_at(self.project, revision)? {
                Some(config) if !config.consumers.is_empty() => Some(Historical {
                    consumers: config.consumers,
                    baseline: baseline::load(
                        self.project,
                        &baseline::Source::Git {
                            rev: revision.into(),
                        },
                        self.dialect,
                    )?,
                }),
                _ => None,
            };
            self.revisions.insert(revision.into(), historical);
        }
        let Some(historical) = &self.revisions[revision] else {
            return Ok((Bindings::default(), [self.anchor; 2]));
        };
        let sides = [
            self.anchor,
            pbps_policy::consumers::Side {
                schema: &historical.baseline.schema,
                ids: &historical.baseline.ids,
            },
        ];
        let bound = Bindings::resolve(&historical.consumers, &sides, &|id| {
            crate::policy_module_id(id, self.dialect)
        })
        .map_err(|errors| {
            anyhow::anyhow!("consumer contracts at `{revision}`: {}", errors.join("; "))
        })?;
        Ok((bound, sides))
    }

    fn contains(&mut self, revision: &str, contract: &BoundContract) -> anyhow::Result<bool> {
        let dialect = self.dialect;
        let (bound, sides) = self.bindings(revision)?;
        if bound.contracts().next().is_none() {
            return Ok(false);
        }
        let restored = contract
            .resolve(&sides, &|id| crate::policy_module_id(id, dialect))
            .map_err(|errors| anyhow::anyhow!(errors.join("; ")))?;
        Ok(bound.contains(&restored))
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
        // A visible baseline path does not prove a merged branch is complete.
        // A shallow root inside the range can hide a more recent removal.
        // Roots outside it (including the baseline) hide no part of this range.
        let shallow_path =
            baseline::git(&self.project.root, &["rev-parse", "--git-path", "shallow"])?;
        let shallow_path = self.project.root.join(shallow_path.trim());
        let shallow = match std::fs::read_to_string(&shallow_path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error).context("the shallow history boundary is unreadable"),
        };
        anyhow::ensure!(
            !revisions.lines().any(|row| shallow
                .lines()
                .any(|root| Some(root) == row.split_whitespace().next())),
            "a shallow boundary could hide the retiring commit; fetch complete history (fetch-depth: 0)"
        );
        for row in revisions.lines() {
            let mut ids = row.split_whitespace();
            let commit = ids.next().context("Git returned an empty revision")?;
            if self.contains(commit, contract)? {
                continue;
            }
            for parent in ids {
                if self.contains(parent, contract)? {
                    return Ok(commit.into());
                }
            }
        }
        anyhow::bail!("no retiring commit was found in the available baseline..candidate history")
    }
}
