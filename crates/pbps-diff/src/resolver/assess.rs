//! Lightweight assessment (ADR-0016 §1, DEC-1515.1): which creation-time
//! binding questions a connected plan raises, answered from the typed plan
//! alone where that is sound. No SQL is read and no catalog is asked.

use super::prepare::{forward, invalidates, surfaces};
use pbps_model::resolver::Surface;
use pbps_model::{Change, ChangeSet, GrantTarget, IdsFile};
use std::collections::BTreeMap;

/// One question's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Answer {
    /// Nothing in the plan can move what this surface binds: the plan changes
    /// no name a lookup could reach.
    Unaffected,
    /// The plan already recreates this surface from its declaration, so its
    /// creation binds afresh whatever the answer would have been.
    Rebuild,
    /// Only an engine can say whether a fresh creation binds differently.
    Resolve,
}

/// Every surface the target holds that the plan keeps, by its declared
/// spelling, with its answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assessment {
    pub questions: BTreeMap<Surface, Answer>,
}

impl Assessment {
    #[must_use]
    pub fn requires_resolution(&self) -> bool {
        self.questions.values().any(|a| *a == Answer::Resolve)
    }

    #[must_use]
    pub fn count(&self, answer: Answer) -> usize {
        self.questions.values().filter(|a| **a == answer).count()
    }

    /// The surfaces only an engine can answer, in order.
    pub fn unresolved(&self) -> impl Iterator<Item = &Surface> {
        self.questions
            .iter()
            .filter(|(_, a)| **a == Answer::Resolve)
            .map(|(s, _)| s)
    }
}

/// Answer each question `ordinary` raises: the plan the differ produced for
/// `base -> desired`, before any connected pass adds to it.
///
/// Deliberately coarse. A surface is unaffected only when the plan changes
/// nothing a name lookup could reach anywhere; any such change sends every
/// surface the plan does not already rebuild to the engine. Narrowing that by
/// path, dependency or name would need the reasoning ADR-0016 leaves to the
/// engine — a qualified call, an overload, a column-notation reference or a
/// relation-namespace arrival each defeats a guess — and a wrong narrowing is
/// a silent wrong answer, where a wide one costs only a resolver run that the
/// project asked for by selecting one.
#[must_use]
pub fn assess(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    ordinary: &ChangeSet,
    dialect: &dyn pbps_dialect::Dialect,
) -> Assessment {
    assessed_with(
        base,
        desired,
        ordinary,
        &Engine {
            indexes_are_relations: dialect.indexes_share_namespace_with_tables(),
            normalize: &|definition| dialect.normalize_definition(definition),
        },
    )
}

/// The engine facts the assessment reads.
struct Engine<'a> {
    /// Whether an index's name is a relation's (DECISIONS 453).
    indexes_are_relations: bool,
    /// The comparison form of a module definition, as the differ compares
    /// one (SPEC §8.2).
    normalize: &'a dyn Fn(&str) -> String,
}

/// [`assess`], with the engine facts it reads.
fn assessed_with(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    ordinary: &ChangeSet,
    engine: &Engine<'_>,
) -> Assessment {
    let after = surfaces(desired.schema);
    let moved = ordinary
        .changes
        .iter()
        .any(|p| moves_bindings(&p.change, base, desired, engine));
    let mut questions = BTreeMap::new();
    for surface in surfaces(base.schema) {
        let kept = forward(&surface, base.ids, desired.ids);
        if !after.contains(&kept) {
            continue;
        }
        let answer = if replaced(&surface, &kept, base.ids, desired.ids)
            || ordinary
                .changes
                .iter()
                .any(|p| recreates(&p.change, &surface, &kept, base, desired, engine))
        {
            Answer::Rebuild
        } else if moved {
            Answer::Resolve
        } else {
            Answer::Unaffected
        };
        questions.insert(kept, answer);
    }
    Assessment { questions }
}

/// Whether the surface found under the same spelling belongs to another table
/// or column: one the plan drops and creates again under a new UID, as when
/// an environment skipped the revision that dropped the old one. Its new
/// surface is created from the declaration, so it is a rebuild, not a kept
/// surface (#1526 review). Only recorded UIDs that differ prove it; a UID
/// either side lacks proves nothing, and the surface stays a question.
fn replaced(surface: &Surface, kept: &Surface, from: &IdsFile, to: &IdsFile) -> bool {
    let differ = |a: Option<&pbps_model::Uid>, b: Option<&pbps_model::Uid>| matches!((a, b), (Some(a), Some(b)) if a != b);
    match (surface, kept) {
        (Surface::Default(before), Surface::Default(after)) => {
            differ(from.column_uid(before), to.column_uid(after))
        }
        (Surface::Check { table: before, .. }, Surface::Check { table: after, .. })
        | (Surface::Index { table: before, .. }, Surface::Index { table: after, .. }) => {
            differ(from.table_uid(before), to.table_uid(after))
        }
        _ => false,
    }
}

/// Whether this change recreates the surface because its declaration says
/// so. A module rebuilt with an unchanged declaration is ADR-0013's candidate
/// rebuild, which is the question itself, not its answer.
fn recreates(
    change: &Change,
    surface: &Surface,
    kept: &Surface,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    engine: &Engine<'_>,
) -> bool {
    if let (Change::AlterModule { id, .. }, Surface::Module(m)) = (change, kept) {
        return id == m && declared_differently(id, base, desired, engine);
    }
    // A change names a surface by whichever spelling it acts under; a kept
    // surface has one of each only across a rename.
    invalidates(change, surface) || invalidates(change, kept)
}

/// Whether the declaration of a module differs, as the differ decides it:
/// after the dialect's normalization. A layout-only edit is no change there,
/// so the rebuild an arrival makes of that module is still the candidate
/// one, a question for the engine (#1526 review).
fn declared_differently(
    id: &pbps_model::ModuleId,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    engine: &Engine<'_>,
) -> bool {
    match (base.schema.modules.get(id), desired.schema.modules.get(id)) {
        (Some(was), Some(now)) => {
            was.kind != now.kind
                || (engine.normalize)(&was.definition) != (engine.normalize)(&now.definition)
        }
        _ => true,
    }
}

/// Whether a change can alter what some name lookup finds: it brings a name
/// into a namespace, takes one out, changes a type an overload is chosen by,
/// or changes who may look in a schema. Listed in full, so a new kind of
/// change is classified here rather than defaulting either way.
fn moves_bindings(
    change: &Change,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    engine: &Engine<'_>,
) -> bool {
    match change {
        // Relations, their row types and their columns.
        Change::CreateTable { .. }
        | Change::DropTable { .. }
        | Change::RenameTable { .. }
        | Change::AddColumn { .. }
        | Change::DropColumn { .. }
        | Change::RenameColumn { .. }
        | Change::AlterColumnType { .. }
        | Change::AddComputedColumn { .. }
        | Change::DropComputedColumn { .. }
        // Renames the clones of its parent's indexes and keys to the
        // declaration's names, and its table leaves the parent's inheritance;
        // or, attached, builds whichever of them the table lacks under names
        // the engine chooses, and its table joins it (#1545). Partitions are
        // PostgreSQL's alone, where an index's name is a relation's.
        | Change::DetachPartition { .. }
        | Change::AttachPartition { .. }
        // A routine's identity, a view's columns, a type's existence.
        | Change::CreateModule { .. }
        | Change::DropModule { .. } => true,
        // Each creates or drops an index. Where an index's name is a
        // relation's (PostgreSQL), an unqualified relation lookup on the path
        // finds it first, and one that bound it, such as a `regclass`
        // constant, finds another relation once it is gone. Where an index
        // is named per table (SQL Server), no lookup reaches it.
        Change::SetPrimaryKey { .. }
        | Change::AddUnique { .. }
        | Change::DropUnique { .. }
        | Change::AddIndex { .. }
        | Change::DropIndex { .. } => engine.indexes_are_relations,
        Change::AlterModule { id, .. } => declared_differently(id, base, desired, engine),
        // A schema the creating role may not use is skipped by the lookup.
        Change::Grant { target, .. } | Change::Revoke { target, .. } => {
            matches!(target, GrantTarget::Schema(_))
        }
        // An expression's own definition is its own question; changing it
        // brings no name anywhere. A check's or foreign key's name is a
        // constraint's, which no lookup in an expression reaches.
        Change::AlterColumnDefault { .. }
        | Change::SetPartitionDefault { .. }
        | Change::AlterColumnExpression { .. }
        | Change::AddCheck { .. }
        | Change::DropCheck { .. }
        | Change::AddForeignKey { .. }
        | Change::DropForeignKey { .. }
        | Change::AlterColumnNullability { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetReplicaIdentity { .. }
        | Change::SetStorageParameters { .. }
        | Change::SetTablePersistence { .. }
        | Change::SetIndexStorageParameters { .. }
        | Change::SetPartitionNotNull { .. }
        | Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::DeleteRow { .. }
        | Change::SetDataMode { .. }
        | Change::CreateRole { .. }
        | Change::DropRole { .. }
        | Change::RenameRole { .. }
        | Change::PublicExecution { .. } => false,
    }
}

#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use pbps_dialect::{Dialect as _, MinimalDialect};
    use pbps_model::{
        CheckConstraint, Column, Hints, IdsFile, Module, ModuleKind, Permission, PlannedChange,
        Schema, Table,
    };
    use std::collections::BTreeSet;

    fn ids(schema: &Schema, old: &IdsFile) -> IdsFile {
        crate::resolve(
            schema,
            old,
            &[],
            &crate::Context {
                operator: "1515".into(),
                today: "2026-10-05".into(),
            },
        )
        .unwrap()
        .ids
    }

    fn view(definition: &str) -> Module {
        Module {
            kind: ModuleKind::View,
            description: None,
            definition: definition.into(),
        }
    }

    /// A table with a default and a CHECK, and a view over it: one surface
    /// of each kind the table and the module carry.
    fn bound() -> Schema {
        let mut schema = Schema::default();
        let mut table = Table::default();
        table
            .columns
            .insert("id".into(), Column::new("int".parse().unwrap()));
        let mut n = Column::new("int".parse().unwrap());
        n.default = Some("0".into());
        table.columns.insert("n".into(), n);
        table.checks.insert(
            "ck_n".into(),
            CheckConstraint {
                expression: "n >= 0".into(),
            },
        );
        schema.tables.insert("app.t".parse().unwrap(), table);
        schema
            .modules
            .insert("app.v".parse().unwrap(), view("SELECT id FROM app.t"));
        schema
    }

    fn assessed(base: &Schema, desired: &Schema) -> Assessment {
        let base_ids = ids(base, &IdsFile::default());
        let desired_ids = ids(desired, &base_ids);
        let base = crate::Side {
            schema: base,
            ids: &base_ids,
        };
        let desired = crate::Side {
            schema: desired,
            ids: &desired_ids,
        };
        let ordinary = crate::diff(base, desired, &MinimalDialect, &Hints::default()).unwrap();
        assess(base, desired, &ordinary, &MinimalDialect)
    }

    fn minimal(definition: &str) -> String {
        MinimalDialect.normalize_definition(definition)
    }

    fn engine(indexes_are_relations: bool) -> Engine<'static> {
        Engine {
            indexes_are_relations,
            normalize: &minimal,
        }
    }

    /// The changes on an engine whose index names are relation names.
    fn with(base: &Schema, changes: Vec<Change>) -> Assessment {
        with_indexes(base, changes, true)
    }

    fn with_indexes(
        base: &Schema,
        changes: Vec<Change>,
        indexes_are_relations: bool,
    ) -> Assessment {
        let base_ids = ids(base, &IdsFile::default());
        let side = crate::Side {
            schema: base,
            ids: &base_ids,
        };
        let ordinary = ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        };
        assessed_with(side, side, &ordinary, &engine(indexes_are_relations))
    }

    fn surface(name: &str) -> Surface {
        match name {
            "default" => Surface::Default(
                "app.t"
                    .parse::<pbps_model::TableName>()
                    .unwrap()
                    .column("n"),
            ),
            "check" => Surface::Check {
                table: "app.t".parse().unwrap(),
                name: "ck_n".into(),
            },
            "view" => Surface::Module("app.v".parse().unwrap()),
            other => panic!("no fixture surface {other}"),
        }
    }

    #[test]
    fn a_plan_that_moves_no_name_answers_every_question_without_an_engine() {
        let base = bound();
        let grant = |target| Change::Grant {
            role: "reader".into(),
            target,
            permissions: BTreeSet::from([Permission::Select]),
        };
        let found = with(
            &base,
            vec![
                grant(GrantTarget::Object("app.t".parse().unwrap())),
                Change::CreateRole {
                    uid: "r_aaaaaa".parse().unwrap(),
                    name: "reader".into(),
                },
            ],
        );
        assert!(!found.requires_resolution(), "{found:?}");
        assert_eq!(found.count(Answer::Unaffected), 3, "{found:?}");
    }

    /// Ordinary table changes are not exempt (ADR-0016 §1): a column that
    /// arrives can capture an unqualified reference or a column-notation
    /// call, and nothing short of the engine says which.
    #[test]
    fn an_added_column_sends_every_kept_surface_to_the_engine() {
        let base = bound();
        let mut desired = base.clone();
        desired
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns
            .insert("label".into(), Column::new("text".parse().unwrap()));
        let found = assessed(&base, &desired);
        assert!(found.requires_resolution());
        for name in ["default", "check", "view"] {
            assert_eq!(found.questions[&surface(name)], Answer::Resolve, "{name}");
        }
    }

    /// ADR-0013's rebuild of an unchanged module is the candidate test's
    /// guess. Read as proof, it would answer the very question the resolver
    /// exists to ask, and the irrelevant arrival of #230 would rebuild again.
    #[test]
    fn a_candidate_rebuild_is_a_question_not_an_answer() {
        let base = bound();
        let found = with(
            &base,
            vec![
                Change::CreateModule {
                    id: "app.f()".parse().unwrap(),
                    module: Box::new(Module {
                        kind: ModuleKind::Function,
                        description: None,
                        definition: "RETURNS int LANGUAGE sql RETURN 1".into(),
                    }),
                },
                Change::AlterModule {
                    id: "app.v".parse().unwrap(),
                    module: Box::new(view("SELECT id FROM app.t")),
                },
            ],
        );
        assert_eq!(found.questions[&surface("view")], Answer::Resolve);
    }

    #[test]
    fn a_declared_rebuild_is_answered_and_the_rest_still_need_the_engine() {
        let base = bound();
        let mut desired = base.clone();
        desired
            .modules
            .insert("app.v".parse().unwrap(), view("SELECT id, n FROM app.t"));
        desired.modules.insert(
            "app.f()".parse().unwrap(),
            Module {
                kind: ModuleKind::Function,
                description: None,
                definition: "RETURNS int LANGUAGE sql RETURN 1".into(),
            },
        );
        let found = assessed(&base, &desired);
        assert_eq!(found.questions[&surface("view")], Answer::Rebuild);
        assert_eq!(found.questions[&surface("check")], Answer::Resolve);
        assert_eq!(found.questions[&surface("default")], Answer::Resolve);
        // The routine the plan creates binds at creation like any new object;
        // only what the target already holds is a question.
        assert!(
            !found
                .questions
                .contains_key(&Surface::Module("app.f()".parse().unwrap()))
        );
    }

    /// A layout-only edit is no declared change to the differ, which compares
    /// definitions normalized: the rebuild an arrival makes of that module
    /// is still the candidate one, and a question. Read bytewise, it would
    /// answer the question and skip the selected resolver (#1526 review). A
    /// real edit is still a declared rebuild.
    #[test]
    fn a_layout_only_edit_leaves_a_candidate_rebuild_a_question() {
        let base = bound();
        let answer = |definition: &str| {
            let mut desired = base.clone();
            desired
                .modules
                .insert("app.v".parse().unwrap(), view(definition));
            let base_ids = ids(&base, &IdsFile::default());
            let desired_ids = ids(&desired, &base_ids);
            let ordinary = ChangeSet {
                changes: [
                    Change::CreateModule {
                        id: "app.f()".parse().unwrap(),
                        module: Box::new(Module {
                            kind: ModuleKind::Function,
                            description: None,
                            definition: "RETURNS int LANGUAGE sql RETURN 1".into(),
                        }),
                    },
                    Change::AlterModule {
                        id: "app.v".parse().unwrap(),
                        module: Box::new(view(definition)),
                    },
                ]
                .into_iter()
                .map(PlannedChange::new)
                .collect(),
            };
            assessed_with(
                crate::Side {
                    schema: &base,
                    ids: &base_ids,
                },
                crate::Side {
                    schema: &desired,
                    ids: &desired_ids,
                },
                &ordinary,
                &engine(true),
            )
            .questions[&surface("view")]
        };
        assert_eq!(answer("SELECT  id\n    FROM app.t"), Answer::Resolve);
        assert_eq!(answer("SELECT id, n FROM app.t"), Answer::Rebuild);
    }

    #[test]
    fn a_rewritten_default_is_a_rebuild_not_a_question() {
        let base = bound();
        let mut desired = base.clone();
        let table = desired.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
        table.columns.get_mut("n").unwrap().default = Some("1".into());
        table
            .columns
            .insert("label".into(), Column::new("text".parse().unwrap()));
        let found = assessed(&base, &desired);
        assert_eq!(found.questions[&surface("default")], Answer::Rebuild);
        assert_eq!(found.questions[&surface("check")], Answer::Resolve);
    }

    #[test]
    fn a_target_without_bound_surfaces_raises_no_question() {
        let mut base = Schema::default();
        let mut table = Table::default();
        table
            .columns
            .insert("id".into(), Column::new("int".parse().unwrap()));
        base.tables.insert("app.t".parse().unwrap(), table);
        let mut desired = base.clone();
        desired
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns
            .insert("label".into(), Column::new("text".parse().unwrap()));
        let found = assessed(&base, &desired);
        assert!(found.questions.is_empty(), "{found:?}");
        assert!(!found.requires_resolution());
    }

    /// A partition's own default is a binding surface like a column's
    /// (#1578 review): a module arriving makes it a question for the engine,
    /// where reading only the partition's columns, which it has none of,
    /// found nothing to ask. A partition with no default of its own raises
    /// none, and one the plan rewrites is a rebuild.
    #[test]
    fn a_partitions_own_default_is_a_question_when_bindings_move() {
        let tree = |own: Option<&str>| {
            let mut schema = Schema::default();
            let mut parent = Table::default();
            parent
                .columns
                .insert("id".into(), Column::new("int".parse().unwrap()));
            parent.partition_by = Some(pbps_model::PartitionBy {
                columns: vec!["id".into()],
            });
            schema.tables.insert("app.ev".parse().unwrap(), parent);
            let partition = Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: "app.ev".parse().unwrap(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: own
                        .map(|default| {
                            (
                                "id".to_owned(),
                                pbps_model::PartitionColumn {
                                    default: Some(default.to_owned()),
                                    not_null: false,
                                },
                            )
                        })
                        .into_iter()
                        .collect(),
                }),
                ..Table::default()
            };
            schema.tables.insert("app.ev_1".parse().unwrap(), partition);
            schema
        };
        let arrives = |base: &Schema| {
            let mut desired = base.clone();
            desired
                .modules
                .insert("app.v".parse().unwrap(), view("SELECT 1 AS n"));
            assessed(base, &desired)
        };
        let base = tree(Some("app.f(1)"));
        let found = arrives(&base);
        assert_eq!(
            found.questions[&Surface::Default("app.ev_1.id".parse().unwrap())],
            Answer::Resolve,
            "{found:?}"
        );
        assert!(found.requires_resolution());
        let found = arrives(&tree(None));
        assert!(found.questions.is_empty(), "{found:?}");
        // Its default rewritten by the same plan is rebuilt, not asked
        // (#1607 review), while bindings still move.
        let mut desired = tree(Some("app.f(2)"));
        desired
            .modules
            .insert("app.v".parse().unwrap(), view("SELECT 1 AS n"));
        let found = assessed(&base, &desired);
        assert_eq!(
            found.questions[&Surface::Default("app.ev_1.id".parse().unwrap())],
            Answer::Rebuild,
            "{found:?}"
        );
    }

    /// An index's name leaves the relation namespace with it: a surface
    /// that bound it, a `regclass` constant for one, finds another relation
    /// afterwards, and only the engine can say which (#1526 review). A
    /// constraint's name is no relation's.
    #[test]
    fn a_dropped_index_name_moves_lookups_and_a_dropped_check_does_not() {
        let base = bound();
        let table: pbps_model::TableName = "app.t".parse().unwrap();
        for change in [
            Change::DropIndex {
                table: table.clone(),
                name: "ix_gone".into(),
            },
            Change::DropUnique {
                table: table.clone(),
                name: "uq_gone".into(),
            },
        ] {
            assert!(
                with(&base, vec![change.clone()]).requires_resolution(),
                "{change:?}"
            );
        }
        for change in [
            Change::DropCheck {
                table: table.clone(),
                name: "ck_gone".into(),
            },
            Change::DropForeignKey {
                table,
                name: "fk_gone".into(),
            },
        ] {
            assert!(
                !with(&base, vec![change.clone()]).requires_resolution(),
                "{change:?}"
            );
        }
    }

    /// A table dropped and created again under its old name, with a new UID,
    /// takes its default and check with it: the new ones are created from the
    /// declarations, so they are rebuilds and ask the engine nothing, even
    /// though the drop and create move lookups elsewhere (#1526 review).
    #[test]
    fn a_surface_on_a_replaced_table_is_a_rebuild_not_a_kept_question() {
        let base = bound();
        let mut base_only = base.clone();
        base_only.modules.clear();
        let base_ids = ids(&base_only, &IdsFile::default());
        let mut desired_ids = base_ids.clone();
        let table: pbps_model::TableName = "app.t".parse().unwrap();
        let old = base_ids.table_uid(&table).unwrap().clone();
        desired_ids.tables.remove(&old);
        desired_ids.tables.insert(
            pbps_model::Uid::generate(pbps_model::UidKind::Table),
            table.clone(),
        );
        let columns: Vec<_> = desired_ids.columns.keys().cloned().collect();
        for uid in columns {
            let column = desired_ids.columns.remove(&uid).unwrap();
            desired_ids.columns.insert(
                pbps_model::Uid::generate(pbps_model::UidKind::Column),
                column,
            );
        }
        let ordinary = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::DropTable {
                    name: table.clone(),
                    uid: old,
                    detach_from: None,
                }),
                PlannedChange::new(Change::CreateTable {
                    uid: desired_ids.table_uid(&table).unwrap().clone(),
                    name: table.clone(),
                    table: Box::new(base_only.tables[&table].clone()),
                }),
            ],
        };
        let assessment = assessed_with(
            crate::Side {
                schema: &base_only,
                ids: &base_ids,
            },
            crate::Side {
                schema: &base_only,
                ids: &desired_ids,
            },
            &ordinary,
            &engine(true),
        );
        assert!(!assessment.requires_resolution(), "{assessment:?}");
        // The same surfaces kept under their recorded UIDs are questions.
        let kept = assessed_with(
            crate::Side {
                schema: &base_only,
                ids: &base_ids,
            },
            crate::Side {
                schema: &base_only,
                ids: &base_ids,
            },
            &ordinary,
            &engine(true),
        );
        assert!(kept.requires_resolution(), "{kept:?}");
    }

    /// Where an index is named per table, as on SQL Server, adding or dropping
    /// one moves no lookup, so a selected resolver is not asked about it
    /// (DECISIONS 453; #1526 review).
    #[test]
    fn an_index_named_per_table_moves_no_lookup() {
        let base = bound();
        let table: pbps_model::TableName = "app.t".parse().unwrap();
        for change in [
            Change::DropIndex {
                table: table.clone(),
                name: "ix_gone".into(),
            },
            Change::DropUnique {
                table: table.clone(),
                name: "uq_gone".into(),
            },
        ] {
            assert!(
                !with_indexes(&base, vec![change.clone()], false).requires_resolution(),
                "{change:?}"
            );
            assert!(
                with_indexes(&base, vec![change.clone()], true).requires_resolution(),
                "{change:?}"
            );
        }
    }

    /// A detach renames its partition's index clones to the declaration's
    /// names, which are relation names: the engine is asked, as for any
    /// index that arrives or leaves. A grant on the same table moves nothing.
    #[test]
    fn a_detached_partition_moves_lookups() {
        let base = bound();
        let table: pbps_model::TableName = "app.t_2026".parse().unwrap();
        let detach = Change::DetachPartition {
            uid: pbps_model::Uid::generate(pbps_model::UidKind::Table),
            table: table.clone(),
            parent: "app.t".parse().unwrap(),
            names: vec![pbps_model::DetachedName {
                kind: pbps_model::DetachedKind::Index,
                parent: "ix_t".into(),
                name: Some("ix_t_2026".into()),
            }],
            shape: Box::new(base.tables[&"app.t".parse().unwrap()].clone()),
        };
        let found = with(&base, vec![detach]);
        assert!(found.requires_resolution(), "{found:?}");
        let grant = Change::Grant {
            role: "reader".into(),
            target: GrantTarget::Object(table),
            permissions: BTreeSet::from([Permission::Select]),
        };
        assert!(!with(&base, vec![grant]).requires_resolution());
    }

    /// The lookup skips a schema its role may not use, so who may use one is
    /// part of what it finds. Who may read a table is not.
    #[test]
    fn a_schema_grant_moves_lookups_and_an_object_grant_does_not() {
        let base = bound();
        let grant = |target| Change::Grant {
            role: "owner".into(),
            target,
            permissions: BTreeSet::from([Permission::Usage]),
        };
        let revoke = |target| Change::Revoke {
            role: "owner".into(),
            target,
            permissions: BTreeSet::from([Permission::Usage]),
        };
        for change in [
            grant(GrantTarget::Schema("ext".into())),
            revoke(GrantTarget::Schema("ext".into())),
        ] {
            assert!(
                with(&base, vec![change.clone()]).requires_resolution(),
                "{change:?}"
            );
        }
        assert!(
            !with(
                &base,
                vec![grant(GrantTarget::Object("app.t".parse().unwrap()))]
            )
            .requires_resolution()
        );
    }
}
