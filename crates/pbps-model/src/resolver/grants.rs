//! Source-free object-grant questions, separate from binding inputs.

use crate::{Change, ChangeSet, GrantTarget, Permission, RoleName};
use std::collections::{BTreeMap, BTreeSet};

/// An address absent at an endpoint is different from a present target
/// with no requested grants. Unreadable observations never make this value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum GrantTargetState {
    Absent,
    Present { target: GrantTarget },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum GrantPrincipal {
    Absent,
    Public,
    Role { name: RoleName },
}

/// A keyed answer to one typed question, not an exported ACL or an
/// authorization recipe. The target and principal are included in its seal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantPhase {
    pub target: GrantTargetState,
    pub grantee: GrantPrincipal,
    pub fingerprint: String,
}

/// One approved change's grant postcondition. The change supplies the
/// permissions and direction; repeating or omitting a change is invalid.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationGrant {
    pub change: usize,
    pub before: GrantPhase,
    pub after: GrantPhase,
}

/// The question selected by an approved grant change. This is descriptive
/// data only and grants no emission authority.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GrantWrite {
    pub target: GrantTarget,
    pub grantee: GrantPrincipal,
    pub permissions: BTreeSet<Permission>,
    pub grant: bool,
}

impl AuthorizationGrant {
    /// Schema grants already have their qualified authorization projection.
    /// All other grants, including derived owned-sequence grants, use these
    /// separately sealed postconditions (DEC-1454.1).
    pub fn writes(changes: &ChangeSet) -> BTreeMap<usize, GrantWrite> {
        changes
            .changes
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                if let Change::Grant {
                    role,
                    target,
                    permissions,
                }
                | Change::Revoke {
                    role,
                    target,
                    permissions,
                } = &p.change
                {
                    return (!matches!(target, GrantTarget::Schema(_))).then(|| {
                        (
                            i,
                            GrantWrite {
                                target: target.clone(),
                                grantee: GrantPrincipal::Role { name: role.clone() },
                                permissions: permissions.clone(),
                                grant: matches!(p.change, Change::Grant { .. }),
                            },
                        )
                    });
                }
                let Change::PublicExecution {
                    routine, access, ..
                } = &p.change
                else {
                    return None;
                };
                Some((
                    i,
                    GrantWrite {
                        target: GrantTarget::Routine(routine.clone()),
                        grantee: GrantPrincipal::Public,
                        permissions: BTreeSet::from([Permission::Execute]),
                        grant: *access == crate::PublicAccess::Kept,
                    },
                ))
            })
            .collect()
    }

    pub(super) fn complete(changes: &ChangeSet, grants: &[Self]) -> bool {
        let writes = Self::writes(changes);
        if writes.len() != grants.len()
            || grants
                .windows(2)
                .any(|pair| pair[0].change >= pair[1].change)
        {
            return false;
        }
        grants.iter().all(|grant| {
            let Some(write) = writes.get(&grant.change) else {
                return false;
            };
            !write.permissions.is_empty()
                && [&grant.before, &grant.after].into_iter().all(|phase| {
                    let target = !matches!(
                        phase.target,
                        GrantTargetState::Present {
                            target: GrantTarget::Schema(_)
                        }
                    );
                    let principal = matches!(
                        (&write.grantee, &phase.grantee),
                        (GrantPrincipal::Public, GrantPrincipal::Public)
                            | (
                                GrantPrincipal::Role { .. },
                                GrantPrincipal::Role { .. } | GrantPrincipal::Absent
                            )
                    );
                    target
                        && principal
                        && phase.fingerprint.len() == 64
                        && phase.fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
                })
        })
    }

    #[cfg(test)]
    pub(crate) fn fixtures(changes: &ChangeSet) -> Vec<Self> {
        Self::writes(changes)
            .into_iter()
            .map(|(change, write)| {
                let phase = GrantPhase {
                    target: GrantTargetState::Present {
                        target: write.target,
                    },
                    grantee: write.grantee,
                    fingerprint: "0".repeat(64),
                };
                Self {
                    change,
                    before: phase.clone(),
                    after: phase,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes() -> ChangeSet {
        ChangeSet {
            changes: vec![crate::PlannedChange::new(Change::Grant {
                role: "reader".into(),
                target: GrantTarget::Object(crate::ObjectName::new("app", "t")),
                permissions: BTreeSet::from([Permission::Select]),
            })],
        }
    }

    #[test]
    fn every_object_grant_requires_its_own_keyed_question() {
        let changes = changes();
        let complete = AuthorizationGrant::fixtures(&changes);
        assert!(AuthorizationGrant::complete(&changes, &complete));
        assert!(!AuthorizationGrant::complete(&changes, &[]));
        let mut repeated = complete.clone();
        repeated.push(complete[0].clone());
        assert!(!AuthorizationGrant::complete(&changes, &repeated));
        let mut wrong = complete.clone();
        wrong[0].change = 1;
        assert!(!AuthorizationGrant::complete(&changes, &wrong));
        let mut empty = complete.clone();
        empty[0].after.fingerprint.clear();
        assert!(!AuthorizationGrant::complete(&changes, &empty));
    }

    #[test]
    fn explicit_absence_survives_the_wire_but_omission_does_not() {
        let phase = GrantPhase {
            target: GrantTargetState::Absent,
            grantee: GrantPrincipal::Absent,
            fingerprint: "0".repeat(64),
        };
        let encoded = serde_json::to_value(&phase).unwrap();
        assert_eq!(
            serde_json::from_value::<GrantPhase>(encoded.clone()).unwrap(),
            phase
        );
        let mut omitted = encoded;
        omitted.as_object_mut().unwrap().remove("target");
        assert!(serde_json::from_value::<GrantPhase>(omitted).is_err());
    }
}
