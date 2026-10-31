//! Reordered creation facets retain complete per-statement definition evidence.
//! SQL still derives solely from the existing DomainDelta and DomainHalf.
//! An earlier typed CREATE and its UID establish context even after wire
//! round-trip; diagnostic provenance is deliberately irrelevant.
use pbps_model::{Change, ChangeSet, DomainDelta};
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct DomainParts(BTreeMap<usize, Change>);
impl DomainParts {
    #[allow(clippy::wildcard_enum_match_arm)]
    pub fn new(changes: &ChangeSet) -> Self {
        let mut current = BTreeMap::new();
        let mut out = Self::default();
        for (at, planned) in changes.changes.iter().enumerate() {
            match &planned.change {
                Change::CreateType {
                    uid, definition, ..
                } => {
                    current.insert(uid.clone(), definition.as_ref().clone());
                }
                Change::AlterDomain {
                    uid,
                    name,
                    from,
                    to,
                    half,
                } => {
                    let Some(now) = current.get_mut(uid) else {
                        continue;
                    };
                    let before = now.clone();
                    let Some(delta) = DomainDelta::of(from, to) else {
                        continue;
                    };
                    let Some(next) = delta.apply_to(now, *half) else {
                        continue;
                    };
                    *now = next;
                    out.0.insert(
                        at,
                        Change::AlterDomain {
                            uid: uid.clone(),
                            name: name.clone(),
                            from: Box::new(before),
                            to: Box::new(now.clone()),
                            half: *half,
                        },
                    );
                }
                Change::DropType { uid, .. } => {
                    current.remove(uid);
                }
                _ => {}
            }
        }
        out
    }
    pub fn at<'a>(&'a self, at: usize, original: &'a Change) -> &'a Change {
        self.0.get(&at).unwrap_or(original)
    }
}
