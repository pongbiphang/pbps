//! Pending source lookups expose templates, never a tentative execution
//! position. The selected boundary is checked before certification.
use super::{Dependent, Holds, Part, Sources};
use pbps_model::operation::{End, Lineage, Snapshot};
use pbps_model::physical_sources::{ColumnAt, Located, Owner, PhysicalSources};
use pbps_model::plan_names::{At, PlanNames};
use pbps_model::{Change, ColumnRef, IdsFile, Partitions, PlannedChange, Schema, TableName};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Intent {
    Release,
    Restore,
    Either,
}

pub(super) enum Lookup {
    Known(Located),
    Pending { before: ColumnAt, after: Located },
}
impl Lookup {
    fn template(self, intent: Intent) -> Located {
        match self {
            Self::Known(location) => location,
            Self::Pending { before, .. } if intent == Intent::Release => Located::Present(before),
            Self::Pending { after, .. } => after,
        }
    }
}
pub(super) trait View {
    fn names(&self) -> &PlanNames<'_>;
    fn lookup(&self, column: &ColumnRef, at: At) -> Lookup;
    fn table_template(&self, table: &TableName, at: At, intent: Intent) -> Located<TableName>;
    fn partitions(&self, given: &Partitions, at: At, intent: Intent, holds: &Holds) -> Partitions {
        let _ = (intent, holds);
        given.at(self.names(), At::End, at)
    }
    fn pending_for(&self, _holds: &Holds) -> bool {
        false
    }
    fn column_template(&self, column: &ColumnRef, at: At, intent: Intent) -> Located {
        self.lookup(column, at).template(intent)
    }
}
impl View for PhysicalSources<'_> {
    fn names(&self) -> &PlanNames<'_> {
        self.names()
    }
    fn lookup(&self, column: &ColumnRef, at: At) -> Lookup {
        Lookup::Known(self.locate(column, at))
    }
    fn table_template(&self, table: &TableName, at: At, _: Intent) -> Located<TableName> {
        self.locate_table(table, at)
    }
}
pub(super) enum Projection<'p> {
    Known(Box<PhysicalSources<'p>>),
    Pending(Box<Pending<'p>>),
}
pub(super) struct Pending<'p> {
    known: PhysicalSources<'p>,
    family: BTreeSet<pbps_model::Uid>,
    before: crate::Side<'p>,
    after: &'p IdsFile,
    templates: BTreeMap<ColumnRef, (ColumnAt, Located)>,
    released: Partitions,
    restored: Partitions,
}
impl<'p> Projection<'p> {
    /// Bind the already selected physical address to its recorded endpoint.
    /// It must not inherit an added teardown's Base spelling merely because
    /// that is its logical role. Active families keep their pending templates.
    #[allow(dead_code)] // PR 5 applies this qualification through the address chain.
    pub(super) fn spelling(
        &self,
        table: Option<&TableName>,
        at: At,
        intrinsic: pbps_model::operation::Spelling,
    ) -> Result<Option<pbps_model::operation::Spelling>, String> {
        use pbps_model::operation::Spelling;
        use pbps_model::physical_sources::Source;
        let (Self::Pending(v), Some(table)) = (self, table) else {
            return Ok(None);
        };
        let Source::Stored(source) = v.known.table_source(table, at) else {
            return Ok(None);
        };
        let uid = v.before.ids.table_uid(&source).ok_or_else(|| {
            format!(
                "the projected table has no recorded UID; {}",
                crate::order::membership::REMEDY
            )
        })?;
        if v.family.contains(uid) {
            return Ok(None);
        }
        if (intrinsic == Spelling::Base && v.before.ids.tables.get(uid) == Some(table))
            || (intrinsic == Spelling::Final && v.after.tables.get(uid) == Some(table))
        {
            return Ok(None);
        }
        if v.before.ids.tables.get(uid) == Some(table) {
            return Ok(Some(Spelling::Base));
        }
        if v.after.tables.get(uid) == Some(table) {
            return Ok(Some(Spelling::Final));
        }
        Err(format!(
            "the projected table address is not a recorded endpoint; {}",
            crate::order::membership::REMEDY
        ))
    }

    pub(super) fn of<'d>(
        sources: &'p Sources<'_>,
        changes: &'p [PlannedChange],
        declared: &'p Schema,
        found: impl IntoIterator<Item = &'d Dependent>,
        parents: &BTreeSet<pbps_model::Uid>,
    ) -> Result<Self, String> {
        if parents.is_empty() {
            return sources.project(changes).map(|v| Self::Known(Box::new(v)));
        }
        let lineage = Lineage::of(
            Snapshot {
                schema: sources.before.schema,
                ids: sources.before.ids,
            },
            Snapshot {
                schema: declared,
                ids: sources.after,
            },
        );
        let family: BTreeSet<_> = lineage
            .families()
            .filter(|(p, _)| parents.contains(p.uid()))
            .flat_map(|(p, f)| std::iter::once(p).chain(f.children()))
            .map(|r| r.uid().clone())
            .collect();
        let known = PhysicalSources::of_excluding(
            template_names(changes, sources, &family).map_err(|e| {
                format!(
                    "pending source UID evidence is unavailable: {e}; {}",
                    crate::order::membership::REMEDY
                )
            })?,
            sources.before.schema,
            sources.before.ids,
            family.clone(),
        )
        .map_err(|e| {
            format!(
                "pending source UID evidence is unavailable: {e}; {}",
                crate::order::membership::REMEDY
            )
        })?;
        let baseline = PhysicalSources::of(
            PlanNames::of(&[], sources.before.ids).map_err(|e| {
                format!(
                    "pending source UID evidence is unavailable: {e}; {}",
                    crate::order::membership::REMEDY
                )
            })?,
            sources.before.schema,
            sources.before.ids,
        )
        .map_err(|e| {
            format!(
                "pending source UID evidence is unavailable: {e}; {}",
                crate::order::membership::REMEDY
            )
        })?;
        let mut templates = BTreeMap::new();
        for dependent in found {
            let Holds::TablePart {
                table,
                part: Part::Default(column) | Part::Generated(column),
            } = &dependent.holds
            else {
                continue;
            };
            let source = table.column(column);
            if templates.contains_key(&source) {
                continue;
            }
            let Located::Present(before) = baseline.locate(&source, At::Start) else {
                return Err(format!(
                    "the physical source of {} has no recorded UID; {}",
                    source.shown(),
                    crate::order::membership::REMEDY
                ));
            };
            if before.owner == Owner::Unavailable {
                return Err(format!(
                    "the physical source of {} has no recorded column UID; {}",
                    source.shown(),
                    crate::order::membership::REMEDY
                ));
            }
            if !family.contains(&before.table_uid) {
                continue;
            }
            let after = goal(sources, declared, &lineage, &before)?;
            if matches!(
                &after,
                Located::Present(ColumnAt {
                    owner: Owner::Unavailable,
                    ..
                }) | Located::Unavailable
            ) {
                return Err(format!(
                    "the target physical owner of {} is unavailable; {}",
                    source.shown(),
                    crate::order::membership::REMEDY
                ));
            }
            templates.insert(source, (before, after));
        }
        // Membership events alone change the relation represented by this view.
        #[allow(clippy::wildcard_enum_match_arm)]
        let moving: BTreeSet<_> = changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::AttachPartition { uid, .. }
                | Change::DetachPartition { uid, .. }
                | Change::DropTable {
                    uid,
                    detach_from: Some(_),
                    ..
                } => Some(uid),
                _ => None,
            })
            .collect();
        let before_names = moving
            .iter()
            .filter_map(|uid| sources.before.ids.tables.get(*uid));
        let after_names = moving
            .iter()
            .filter_map(|uid| sources.after.tables.get(*uid));
        Ok(Self::Pending(Box::new(Pending {
            known,
            family,
            before: sources.before,
            after: sources.after,
            templates,
            released: Partitions::of(sources.before.schema).without(before_names),
            restored: Partitions::of(declared).without(after_names),
        })))
    }
}
impl View for Projection<'_> {
    fn names(&self) -> &PlanNames<'_> {
        match self {
            Self::Known(v) => v.names(),
            Self::Pending(v) => v.known.names(),
        }
    }
    fn lookup(&self, column: &ColumnRef, at: At) -> Lookup {
        match self {
            Self::Known(v) => Lookup::Known(v.locate(column, at)),
            Self::Pending(v) => v.templates.get(column).map_or(
                Lookup::Known(v.known.locate(column, at)),
                |(before, after)| Lookup::Pending {
                    before: before.clone(),
                    after: after.clone(),
                },
            ),
        }
    }
    fn table_template(&self, table: &TableName, at: At, intent: Intent) -> Located<TableName> {
        match self {
            Self::Known(v) => v.locate_table(table, at),
            Self::Pending(v) => {
                let Some(uid) = v.before.ids.table_uid(table) else {
                    return Located::Unavailable;
                };
                if !v.family.contains(uid) {
                    return v.known.locate_table(table, at);
                }
                if intent == Intent::Release {
                    Located::Present(table.clone())
                } else {
                    v.after
                        .tables
                        .get(uid)
                        .cloned()
                        .map_or(Located::Retired, Located::Present)
                }
            }
        }
    }
    fn partitions(&self, given: &Partitions, at: At, intent: Intent, holds: &Holds) -> Partitions {
        match self {
            Self::Known(v) => given.at(v.names(), At::End, at),
            Self::Pending(v) => {
                if !self.pending_for(holds) {
                    return given.at(v.known.names(), At::End, at);
                }
                if intent == Intent::Release {
                    v.released.clone()
                } else {
                    v.restored.clone()
                }
            }
        }
    }
    fn pending_for(&self, holds: &Holds) -> bool {
        let (Self::Pending(v), Holds::TablePart { table, .. }) = (self, holds) else {
            return false;
        };
        v.before
            .ids
            .table_uid(table)
            .is_some_and(|uid| v.family.contains(uid))
    }
}
fn goal(
    sources: &Sources<'_>,
    declared: &Schema,
    lineage: &Lineage,
    source: &ColumnAt,
) -> Result<Located, String> {
    let Some(table) = sources.after.tables.get(&source.table_uid) else {
        return Ok(Located::Retired);
    };
    let Some(target) = declared.tables.get(table) else {
        return Ok(Located::Unavailable);
    };
    if let Owner::Column(uid) = &source.owner {
        if let Some(at) = sources.after.columns.get(uid) {
            if &at.table != table {
                return Err(format!(
                    "a physical column cannot change its table identity; {}",
                    crate::order::membership::REMEDY
                ));
            }
            return Ok(Located::Present(ColumnAt {
                table_uid: source.table_uid.clone(),
                address: at.clone(),
                owner: Owner::Column(uid.clone()),
            }));
        }
        // A retired ordinary identity never follows the next holder of its name.
        if target.partition_of.is_none() {
            return Ok(Located::Retired);
        }
    }
    let before = Snapshot {
        schema: sources.before.schema,
        ids: sources.before.ids,
    };
    let Some(child) = before.table_identity(&source.address.table) else {
        return Ok(Located::Unavailable);
    };
    let Some(parent) = lineage
        .parent_of(&child, End::Goal)
        .or_else(|| lineage.parent_of(&child, End::Base))
    else {
        return Ok(Located::Unavailable);
    };
    let family = lineage
        .family(parent)
        .expect("a lineage parent has a family");
    let mut matches = family.columns().filter(|(uid, _)| {
        family
            .physical(uid, &child, End::Base)
            .is_some_and(|p| p.name == source.address.name)
    });
    let Some((uid, _)) = matches.next() else {
        return Ok(Located::Unavailable);
    };
    if matches.next().is_some() {
        return Err(format!(
            "the physical column has no unique lineage; {}",
            crate::order::membership::REMEDY
        ));
    }
    let Some(physical) = family.physical(uid, &child, End::Goal) else {
        let unread = family
            .slot(uid, &child, End::Goal)
            .is_some_and(|p| target.columns.contains_key(&p.name));
        return Ok(if unread {
            Located::Unavailable
        } else {
            Located::Retired
        });
    };
    let owner = match physical.key {
        pbps_model::operation::ColumnKey::Recorded(uid) => Owner::Column(uid.uid().clone()),
        pbps_model::operation::ColumnKey::Partition { .. } => {
            let Some(parent) = sources.after.tables.get(parent.uid()) else {
                return Ok(Located::Unavailable);
            };
            Owner::Partition {
                parent: parent.clone(),
            }
        }
    };
    Ok(Located::Present(ColumnAt {
        table_uid: source.table_uid.clone(),
        address: table.column(&physical.name),
        owner,
    }))
}

fn template_names<'p>(
    changes: &'p [PlannedChange],
    sources: &Sources<'p>,
    family: &BTreeSet<pbps_model::Uid>,
) -> Result<PlanNames<'p>, pbps_model::plan_names::PlanNamesError> {
    let mut pending_names = family.clone();
    for ids in [sources.before.ids, sources.after] {
        let index = ids.index();
        pending_names.extend(
            ids.columns
                .iter()
                .filter(|(_, at)| {
                    index
                        .table_uid(&at.table)
                        .is_some_and(|uid| family.contains(uid))
                })
                .map(|(uid, _)| uid.clone()),
        );
    }
    PlanNames::of_templates(changes, sources.after, &pending_names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, ModuleId, PartitionBound, PartitionBy, PartitionOf, Table};
    fn uid(value: &str) -> pbps_model::Uid {
        value.parse().unwrap()
    }
    type Fixture = (
        Schema,
        IdsFile,
        Schema,
        IdsFile,
        Vec<PlannedChange>,
        BTreeMap<ModuleId, Vec<Dependent>>,
    );
    fn fixture() -> Fixture {
        let p: TableName = "app.p".parse().unwrap();
        let a: TableName = "app.a".parse().unwrap();
        let mut parent = Table {
            partition_by: Some(PartitionBy {
                strategy: Default::default(),
                columns: vec!["k".into()],
                external: false,
            }),
            ..Default::default()
        };
        for name in ["k", "d"] {
            parent
                .columns
                .insert(name.into(), Column::new("integer".parse().unwrap()));
        }
        let mut child = Table::default();
        for name in ["k", "e"] {
            child
                .columns
                .insert(name.into(), Column::new("integer".parse().unwrap()));
        }
        let mut before = Schema::default();
        before.tables.insert(p.clone(), parent);
        before.tables.insert(a.clone(), child);
        let mut base = IdsFile::default();
        base.tables.insert(uid("t_aaaaaa"), p.clone());
        base.tables.insert(uid("t_bbbbbb"), a.clone());
        for (id, at) in [
            ("c_aaaaaa", p.column("k")),
            ("c_bbbbbb", p.column("d")),
            ("c_cccccc", a.column("k")),
            ("c_dddddd", a.column("e")),
        ] {
            base.columns.insert(uid(id), at);
        }
        let mut after = before.clone();
        let parent = after.tables.get_mut(&p).unwrap();
        let c = parent.columns.shift_remove("d").unwrap();
        parent.columns.insert("e".into(), c);
        after.tables.insert(
            a.clone(),
            Table {
                partition_of: Some(PartitionOf {
                    parent: p.clone(),
                    bound: PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..Default::default()
            },
        );
        let mut ids = base.clone();
        ids.columns.remove(&uid("c_cccccc"));
        ids.columns.remove(&uid("c_dddddd"));
        ids.columns.insert(uid("c_bbbbbb"), p.column("e"));
        let changes = vec![
            PlannedChange::new(Change::AttachPartition {
                uid: uid("t_bbbbbb"),
                table: a.clone(),
                parent: p.clone(),
                bound: PartitionBound::Default,
                key: vec![],
                shape: Box::new(after.tables[&a].clone()),
            }),
            PlannedChange::new(Change::RenameColumn {
                uid: uid("c_bbbbbb"),
                table: p.clone(),
                from: "d".into(),
                to: "e".into(),
                table_was: None,
            }),
        ];
        let deps = [
            (p, Part::Default("d".into())),
            (a, Part::Default("e".into())),
        ]
        .into_iter()
        .map(|(table, part)| Dependent {
            described: "default".into(),
            holds: Holds::TablePart { table, part },
        })
        .collect();
        (
            before,
            base,
            after,
            ids,
            changes,
            [("app.f()".parse().unwrap(), deps)].into(),
        )
    }
    #[test]
    fn a_meeting_in_the_payload_cannot_register_pending_authority() {
        let (before, base, after, ids, steps, found) = fixture();
        let sources = Sources {
            before: crate::Side {
                schema: &before,
                ids: &base,
            },
            after: &ids,
        };
        assert!(
            Projection::of(
                &sources,
                &steps,
                &after,
                found.values().flatten(),
                &BTreeSet::new()
            )
            .is_err()
        );
        assert!(
            Projection::of(
                &sources,
                &steps,
                &after,
                found.values().flatten(),
                &[uid("t_aaaaaa")].into()
            )
            .is_ok()
        );
    }

    #[test]
    fn a_release_cannot_match_the_restoration_spelling() {
        use super::super::{REMOVES, find, projected_parts};
        let (before, base, after, ids, mut steps, found) = fixture();
        let sources = Sources {
            before: crate::Side {
                schema: &before,
                ids: &base,
            },
            after: &ids,
        };
        let removal = |column: &str| {
            PlannedChange::new(Change::AlterColumnDefault {
                uid: uid("c_bbbbbb"),
                column: format!("app.p.{column}").parse().unwrap(),
                from: Some("app.f()".into()),
                to: None,
            })
        };
        steps.push(removal("e"));
        let holds = &found.values().next().unwrap()[0].holds;
        let parents = [uid("t_aaaaaa")].into();
        let view =
            Projection::of(&sources, &steps, &after, found.values().flatten(), &parents).unwrap();
        assert_eq!(
            find(
                &view,
                &projected_parts(&steps),
                holds,
                &Partitions::default(),
                REMOVES
            ),
            None
        );
        steps.push(removal("d"));
        let view =
            Projection::of(&sources, &steps, &after, found.values().flatten(), &parents).unwrap();
        assert_eq!(
            find(
                &view,
                &projected_parts(&steps),
                holds,
                &Partitions::default(),
                REMOVES
            ),
            Some(3)
        );
    }

    #[test]
    fn pending_sources_keep_both_templates_before_the_shape_order_exists() {
        let (before, base, after, ids, steps, found) = fixture();
        let sources = Sources {
            before: crate::Side {
                schema: &before,
                ids: &base,
            },
            after: &ids,
        };
        assert!(sources.project(&steps).is_err());
        let view = Projection::of(
            &sources,
            &steps,
            &after,
            found.values().flatten(),
            &["t_aaaaaa".parse().unwrap()].into(),
        )
        .unwrap();
        let stored: ColumnRef = "app.a.e".parse().unwrap();
        assert!(matches!(
            view.lookup(&stored, At::Before(0)),
            Lookup::Pending { .. }
        ));
        let Located::Present(release) =
            view.column_template(&stored, At::Before(0), Intent::Release)
        else {
            panic!()
        };
        assert_eq!(release.owner, Owner::Column(uid("c_dddddd")));
        let Located::Present(restore) =
            view.column_template(&stored, At::Before(0), Intent::Restore)
        else {
            panic!()
        };
        assert_eq!(
            restore.owner,
            Owner::Partition {
                parent: "app.p".parse().unwrap()
            }
        );
        let stored: ColumnRef = "app.p.d".parse().unwrap();
        assert_eq!(
            view.column_template(&stored, At::End, Intent::Release),
            Located::Present(ColumnAt {
                table_uid: uid("t_aaaaaa"),
                address: stored,
                owner: Owner::Column(uid("c_bbbbbb"))
            })
        );
        let Located::Present(restore) =
            view.column_template(&"app.p.d".parse().unwrap(), At::Start, Intent::Restore)
        else {
            panic!()
        };
        assert_eq!(restore.address, "app.p.e".parse().unwrap());
    }
    #[test]
    fn unrelated_sources_keep_their_current_boundary_during_a_pending_meeting() {
        let (mut before, mut base, mut after, mut ids, mut steps, mut found) = fixture();
        let old: TableName = "app.unrelated".parse().unwrap();
        let new: TableName = "app.renamed".parse().unwrap();
        let mut shape = Table::default();
        shape
            .columns
            .insert("n".into(), Column::new("integer".parse().unwrap()));
        before.tables.insert(old.clone(), shape.clone());
        after.tables.insert(new.clone(), shape);
        base.tables.insert(uid("t_cccccc"), old.clone());
        ids.tables.insert(uid("t_cccccc"), new.clone());
        base.columns.insert(uid("c_eeeeee"), old.column("n"));
        ids.columns.insert(uid("c_eeeeee"), new.column("n"));
        steps.push(PlannedChange::new(Change::RenameTable {
            uid: uid("t_cccccc"),
            from: old.clone(),
            to: new.clone(),
            defaults: vec![],
        }));
        found.values_mut().next().unwrap().push(Dependent {
            described: "unrelated default".into(),
            holds: Holds::TablePart {
                table: old.clone(),
                part: Part::Default("n".into()),
            },
        });
        let sources = Sources {
            before: crate::Side {
                schema: &before,
                ids: &base,
            },
            after: &ids,
        };
        let view = Projection::of(
            &sources,
            &steps,
            &after,
            found.values().flatten(),
            &["t_aaaaaa".parse().unwrap()].into(),
        )
        .unwrap();
        assert_eq!(
            view.spelling(
                Some(&new),
                At::After(2),
                pbps_model::operation::Spelling::Base
            )
            .unwrap(),
            Some(pbps_model::operation::Spelling::Final)
        );
        assert_eq!(
            view.spelling(
                Some(&new),
                At::After(2),
                pbps_model::operation::Spelling::Final
            )
            .unwrap(),
            None
        );
        assert_eq!(
            view.spelling(
                Some(&old),
                At::Before(2),
                pbps_model::operation::Spelling::Base
            )
            .unwrap(),
            None
        );
        for intent in [Intent::Release, Intent::Restore] {
            let Located::Present(at) =
                view.column_template(&old.column("n"), At::Before(2), intent)
            else {
                panic!()
            };
            assert_eq!(at.address, old.column("n"));
            let Located::Present(at) = view.column_template(&old.column("n"), At::After(2), intent)
            else {
                panic!()
            };
            assert_eq!(at.address, new.column("n"));
        }
    }

    #[test]
    fn a_pending_source_missing_its_uid_cannot_borrow_the_goal_holder_by_name() {
        let (before, mut base, after, ids, steps, found) = fixture();
        base.tables.remove(&uid("t_bbbbbb"));
        let sources = Sources {
            before: crate::Side {
                schema: &before,
                ids: &base,
            },
            after: &ids,
        };
        let error = match Projection::of(
            &sources,
            &steps,
            &after,
            found.values().flatten(),
            &["t_aaaaaa".parse().unwrap()].into(),
        ) {
            Ok(_) => panic!("missing UID accepted"),
            Err(e) => e,
        };
        assert!(
            error.contains("UID") && error.contains(crate::order::membership::REMEDY),
            "{error}"
        );
    }
}

#[cfg(test)]
mod missing_column_test {
    use super::*;
    #[test]
    fn an_unrecorded_ordinary_column_cannot_become_a_partition_template() {
        let p: TableName = "app.p".parse().unwrap();
        let a: TableName = "app.a".parse().unwrap();
        let mut before = Schema::default();
        for table in [&p, &a] {
            let mut t = pbps_model::Table::default();
            t.columns.insert(
                "d".into(),
                pbps_model::Column::new("integer".parse().unwrap()),
            );
            before.tables.insert(table.clone(), t);
        }
        let mut base = IdsFile::default();
        base.tables.insert("t_aaaaaa".parse().unwrap(), p.clone());
        base.tables.insert("t_bbbbbb".parse().unwrap(), a.clone());
        base.columns
            .insert("c_aaaaaa".parse().unwrap(), p.column("d"));
        let mut after = before.clone();
        after.tables.get_mut(&a).unwrap().columns.clear();
        after.tables.get_mut(&a).unwrap().partition_of = Some(pbps_model::PartitionOf {
            parent: p,
            bound: pbps_model::PartitionBound::Default,
            columns: Default::default(),
        });
        let sources = Sources {
            before: crate::Side {
                schema: &before,
                ids: &base,
            },
            after: &base,
        };
        let found: BTreeMap<pbps_model::ModuleId, Vec<Dependent>> = [(
            "app.f()".parse().unwrap(),
            vec![Dependent {
                described: "default".into(),
                holds: Holds::TablePart {
                    table: a,
                    part: Part::Default("d".into()),
                },
            }],
        )]
        .into();
        let error = match Projection::of(
            &sources,
            &[],
            &after,
            found.values().flatten(),
            &["t_aaaaaa".parse().unwrap()].into(),
        ) {
            Ok(_) => panic!("untracked source accepted"),
            Err(e) => e,
        };
        assert!(
            error.contains("recorded column UID")
                && error.contains(crate::order::membership::REMEDY),
            "{error}"
        );
    }
}
