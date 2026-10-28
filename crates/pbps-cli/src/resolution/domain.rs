//! Advisory declared-domain code vetting, independent of binding resolution.
use super::{Entry, Refused};
use crate::output::Finding;
use pbps_model::{Change, ChangeSet, ManagedType};

/// No scratch is acquired for enums, renames, drops or code-free domains.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn needed(changes: &ChangeSet) -> bool {
    changes.changes.iter().any(|p| {
        let definition = match &p.change {
            Change::CreateType { definition, .. } => definition,
            Change::AlterDomain { to, .. } => to,
            _ => return false,
        };
        matches!(definition.as_ref(), ManagedType::Domain { default, checks, .. }
            if default.is_some() || !checks.is_empty())
    })
}

pub enum Verdict {
    Passed,
    Refused(Finding),
    Unavailable(Option<pbps_model::Finding>),
}

/// A failed advisory read is a policy-controlled warning, never resolver
/// evidence. Apply repeats its own binding and owner checks (DEC-2309.1).
pub async fn vet(
    entry: &mut Entry<'_>,
    conn: &mut pbps_db::Conn,
    target: &crate::db::Target,
    project: &pbps_config::Project,
    desired: pbps_diff::Side<'_>,
) -> Verdict {
    use pbps_cli::resolver::server::scratch::domain;
    let result = async {
        let identity = pbps_cli::resolver::server::target_identity(conn)
            .await
            .map_err(|error| error.to_string())?;
        let open = entry
            .open(conn.driver(), &identity)
            .await
            .map_err(|refused| match refused {
                Refused::Finding(finding) => finding.message,
                Refused::Unanswerable(error) => error.to_string(),
            })?;
        Ok(domain::vet(
            &open.connection,
            target.connection(),
            &identity,
            desired,
            &open.standard,
            open.externals.as_deref(),
            &open.provenance,
        )
        .await)
    }
    .await;
    let why = match result {
        Ok(Ok(())) => return Verdict::Passed,
        Ok(Err(domain::Refused::Code(why))) => {
            return Verdict::Refused(crate::policy_finding(&pbps_model::Finding::new(
                "plan.domain-code",
                pbps_model::Severity::Error,
                why,
                None,
            )));
        }
        Ok(Err(domain::Refused::Unavailable(error))) => error.to_string(),
        Err(why) => why,
    };
    let id = pbps_policy::rules::DOMAIN_VET_UNAVAILABLE;
    let policies = project.config.policies();
    let today = pbps_policy::civil::at(crate::unix_seconds(), 0).date();
    let finding = policies.effective(id).severity
        .filter(|_| !policies.suppressed(id, None, &today))
        .map(|severity| pbps_model::Finding::new(
            id, severity,
            format!("The declared-domain code vet could not answer: {why}. CHECK and DEFAULT code will still be vetted inside apply."),
            None,
        ));
    Verdict::Unavailable(finding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{DomainHalf, PlannedChange};

    #[test]
    fn only_created_or_altered_domains_with_code_need_the_advisory_goal() {
        let uid: pbps_model::Uid = "y_000001".parse().unwrap();
        let name: pbps_model::TypeRef = "app.d".parse().unwrap();
        let domain = |code: bool| ManagedType::Domain {
            base: "text".parse().unwrap(),
            not_null: false,
            default: code.then(|| "'x'".into()),
            checks: Default::default(),
            collation: None,
        };
        for (definition, expected) in [
            (
                ManagedType::Enum {
                    values: vec!["ok".into()],
                },
                false,
            ),
            (domain(false), false),
            (domain(true), true),
        ] {
            let changes = ChangeSet {
                changes: vec![PlannedChange::new(Change::CreateType {
                    uid: uid.clone(),
                    name: name.clone(),
                    definition: Box::new(definition),
                })],
            };
            assert_eq!(needed(&changes), expected);
        }
        for code in [false, true] {
            let changes = ChangeSet {
                changes: vec![PlannedChange::new(Change::AlterDomain {
                    uid: uid.clone(),
                    name: name.clone(),
                    from: Box::new(domain(false)),
                    to: Box::new(domain(code)),
                    half: DomainHalf::Tighten,
                })],
            };
            assert_eq!(needed(&changes), code);
        }
        assert!(!needed(&ChangeSet::default()));
        assert!(!needed(&ChangeSet {
            changes: vec![PlannedChange::new(Change::DropType {
                uid,
                name,
                definition: Box::new(domain(true)),
            })]
        }));
    }
}
