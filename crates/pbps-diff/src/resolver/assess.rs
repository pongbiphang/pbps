//! Lightweight assessment (ADR-0016 §1, DEC-1515.1): which creation-time
//! binding questions a connected plan raises, answered from the typed plan
//! alone where that is sound. No SQL is read and no catalog is asked.

use super::prepare::{forward, invalidates, surfaces};
use pbps_model::resolver::Surface;
use pbps_model::{Change, ChangeSet, GrantTarget};
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
pub fn assess(base: crate::Side<'_>, desired: crate::Side<'_>, ordinary: &ChangeSet) -> Assessment {
    let after = surfaces(desired.schema);
    let moved = ordinary
        .changes
        .iter()
        .any(|p| moves_bindings(&p.change, base, desired));
    let mut questions = BTreeMap::new();
    for surface in surfaces(base.schema) {
        let kept = forward(&surface, base.ids, desired.ids);
        if !after.contains(&kept) {
            continue;
        }
        let answer = if ordinary
            .changes
            .iter()
            .any(|p| recreates(&p.change, &surface, &kept, base, desired))
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

/// Whether this change recreates the surface because its declaration says
/// so. A module rebuilt with an unchanged declaration is ADR-0013's candidate
/// rebuild, which is the question itself, not its answer.
fn recreates(
    change: &Change,
    surface: &Surface,
    kept: &Surface,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
) -> bool {
    if let (Change::AlterModule { id, .. }, Surface::Module(m)) = (change, kept) {
        return id == m && declared_differently(id, base, desired);
    }
    // A change names a surface by whichever spelling it acts under; a kept
    // surface has one of each only across a rename.
    invalidates(change, surface) || invalidates(change, kept)
}

fn declared_differently(
    id: &pbps_model::ModuleId,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
) -> bool {
    match (base.schema.modules.get(id), desired.schema.modules.get(id)) {
        (Some(was), Some(now)) => was.kind != now.kind || was.definition != now.definition,
        _ => true,
    }
}

/// Whether a change can alter what some name lookup finds: it brings a name
/// into a namespace, takes one out, changes a type an overload is chosen by,
/// or changes who may look in a schema. Listed in full, so a new kind of
/// change is classified here rather than defaulting either way.
fn moves_bindings(change: &Change, base: crate::Side<'_>, desired: crate::Side<'_>) -> bool {
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
        // Each creates an index, and an index's name is a relation's name: an
        // unqualified relation lookup on the path finds it first.
        | Change::SetPrimaryKey { .. }
        | Change::AddUnique { .. }
        | Change::AddIndex { .. }
        // Renames the clones of its parent's indexes and keys to the
        // declaration's names, and its table leaves the parent's inheritance.
        | Change::DetachPartition { .. }
        // A routine's identity, a view's columns, a type's existence.
        | Change::CreateModule { .. }
        | Change::DropModule { .. } => true,
        Change::AlterModule { id, .. } => declared_differently(id, base, desired),
        // A schema the creating role may not use is skipped by the lookup.
        Change::Grant { target, .. } | Change::Revoke { target, .. } => {
            matches!(target, GrantTarget::Schema(_))
        }
        // An expression's own definition is its own question; changing it
        // brings no name anywhere.
        Change::AlterColumnDefault { .. }
        | Change::AlterColumnExpression { .. }
        | Change::AddCheck { .. }
        | Change::DropCheck { .. }
        | Change::DropIndex { .. }
        | Change::DropUnique { .. }
        | Change::AddForeignKey { .. }
        | Change::DropForeignKey { .. }
        | Change::AlterColumnNullability { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetReplicaIdentity { .. }
        | Change::SetStorageParameters { .. }
        | Change::SetTablePersistence { .. }
        | Change::SetIndexStorageParameters { .. }
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
    use pbps_dialect::MinimalDialect;
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
        assess(base, desired, &ordinary)
    }

    fn with(base: &Schema, changes: Vec<Change>) -> Assessment {
        let base_ids = ids(base, &IdsFile::default());
        let side = crate::Side {
            schema: base,
            ids: &base_ids,
        };
        let ordinary = ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        };
        assess(side, side, &ordinary)
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
