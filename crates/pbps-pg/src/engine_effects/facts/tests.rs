//! The facts of every column-meeting scene, pinned cell for cell. The golden
//! was characterized from the first implementation (`c97cd4ad`), before the
//! lineage index replaced its four name-matching resolvers, and this
//! implementation must reproduce it. Regenerate it only for a reviewed change
//! of meaning: `PBPS_FACTS_GOLDEN=<file>` writes it, `PBPS_FACTS_FULL=<file>`
//! writes the complete text it hashes, for diffing two implementations.
use super::*;
use crate::engine_effects::scenes_test::*;
use pbps_model::operation::StoredColumn;
use pbps_model::{ModuleId, ObjectName, RoutineId};
use sha2::{Digest, Sha256};
use std::fmt::Write;

#[derive(Clone, Copy)]
enum Stored {
    Nothing,
    Defaults,
    Unrepresented,
}
const STORED: [Stored; 3] = [Stored::Nothing, Stored::Defaults, Stored::Unrepresented];

/// Every declared DEFAULT of the baseline, as the catalog would hold it: bound
/// to one routine and to the sequence of the table's `k`.
fn catalog(s: &Scene, stored: Stored) -> MembershipCatalog {
    let routine = ModuleId::Routine(RoutineId::new(
        ObjectName::new("app", "f"),
        vec!["integer".parse().unwrap()],
    ));
    let mut found = MembershipCatalog::default();
    for (table, shape) in &s.before.tables {
        // A member's physical columns are its parent's, with its own values.
        let member = shape
            .partition_of
            .as_ref()
            .map(|of| (&s.before.tables[&of.parent].columns, of));
        for (name, c) in member.map_or(&shape.columns, |(columns, _)| columns) {
            let own = member.and_then(|(_, of)| of.columns.get(name));
            let default = match (own.and_then(|o| o.default.clone()), &c.value_source) {
                (_, _) if matches!(stored, Stored::Nothing) => None,
                (Some(own), _) => Some(own),
                (None, ValueSource::Default(e)) => Some(e.clone()),
                (None, _) => None,
            };
            let bound = default.is_some();
            let local = own.map_or(member.is_none() && !c.nullable, |o| o.not_null);
            found.columns.insert(
                table.column(name),
                StoredColumn {
                    default,
                    routines: bound.then(|| routine.clone()).into_iter().collect(),
                    sequences: bound.then(|| table.column("k")).into_iter().collect(),
                    unrepresented: bound && matches!(stored, Stored::Unrepresented),
                    local_not_null: local && !matches!(stored, Stored::Nothing),
                },
            );
        }
    }
    found
}
fn facts(s: &Scene, pg: &Postgres, stored: Stored) -> Facts {
    let side = |schema, ids| Snapshot { schema, ids };
    of(
        pg,
        &catalog(s, stored),
        side(&s.before, &s.base_ids),
        side(&s.after, &s.ids),
    )
}

/// Every scene of the oracle: the 78 of the class matrix, and three whose
/// member holds a DEFAULT and a NOT NULL of its own.
fn scenes() -> Vec<(String, Scene)> {
    let mut scenes = Vec::new();
    for kind in KINDS {
        for motion in [Motion::Attach, Motion::Detach, Motion::Drop] {
            for cut in [false, true] {
                scenes.push((
                    format!("{kind:?}/{motion:?}/final={cut}"),
                    build(kind, motion, cut),
                ));
            }
        }
    }
    for (kind, motion, column, not_null) in [
        (Kind::Rename, Motion::Attach, "e", true),
        (Kind::SetDefault, Motion::Attach, "d", false),
        (Kind::DropDefault, Motion::Detach, "d", true),
    ] {
        let s = with_own_values(build(kind, motion, false), motion, column, not_null);
        scenes.push((format!("Own/{kind:?}/{motion:?}"), s));
    }
    scenes
}

#[test]
fn facts_of_every_scene_match_the_characterized_golden() {
    let (mut lines, mut full) = (String::new(), String::new());
    for (label, s) in &scenes() {
        write!(lines, "{label}").unwrap();
        for version in [160000, 180000] {
            let pg = Postgres::new().with_server_version(version);
            let text: String = STORED.iter().fold(String::new(), |t, &k| {
                t + &format!("{:#?}\n", facts(s, &pg, k))
            });
            write!(lines, "\t{:x}", Sha256::digest(text.as_bytes())).unwrap();
            writeln!(full, "== {label}/PG{version}\n{text}").unwrap();
        }
        lines.push('\n');
    }
    if let Ok(path) = std::env::var("PBPS_FACTS_FULL") {
        std::fs::write(path, &full).unwrap();
    }
    if let Ok(path) = std::env::var("PBPS_FACTS_GOLDEN") {
        return std::fs::write(path, lines).unwrap();
    }
    let golden = include_str!("facts.golden");
    let differing: Vec<_> = lines
        .lines()
        .zip(golden.lines())
        .filter(|(now, then)| now != then)
        .map(|(now, _)| now.split('\t').next().unwrap())
        .collect();
    assert_eq!(lines.lines().count(), golden.lines().count());
    assert!(differing.is_empty(), "{differing:?}");
}

#[test]
fn a_plan_that_moves_no_partition_has_no_facts() {
    let s = build(Kind::Widen, Motion::Attach, false);
    let side = Snapshot {
        schema: &s.before,
        ids: &s.base_ids,
    };
    let pg = Postgres::new().with_server_version(180000);
    assert_eq!(
        of(&pg, &catalog(&s, Stored::Defaults), side, side),
        Facts::default()
    );
}

#[test]
fn an_absent_owned_sequence_has_no_source_and_is_never_inherited() {
    let s = build(Kind::Widen, Motion::Attach, false);
    let pg = Postgres::new().with_server_version(180000);
    let facts = facts(&s, &pg, Stored::Defaults);
    for state in [&facts.base, &facts.goal] {
        let numbering = state
            .cells
            .iter()
            .filter(|((_, p), _)| matches!(p, Property::Column(_, Attribute::Numbering)));
        assert!(numbering.clone().count() > 0);
        for (_, cell) in numbering {
            let value = cell.value.known().unwrap();
            assert_eq!(
                (&value.datum, &value.ownership),
                (&Datum::Absent, &Ownership::Own)
            );
            assert!(cell.origin.is_none());
        }
    }
}

#[test]
fn a_stored_default_binds_what_the_catalog_says_and_never_less() {
    let s = build(Kind::DropDefault, Motion::Detach, false);
    let pg = Postgres::new().with_server_version(180000);
    let before = Snapshot {
        schema: &s.before,
        ids: &s.base_ids,
    };
    let (parent, child) = (name("app.p"), name("app.a"));
    let default = |facts: &Facts, owner: &TableName, key: ColumnKey| {
        let owner = before.table_identity(owner).unwrap();
        facts.base.cells[&(owner, Property::Column(key, Attribute::Default))]
            .value
            .clone()
    };
    let own = || before.column_identity(&parent.column("d")).unwrap();
    let inherited = || ColumnKey::Partition {
        table: before.table_identity(&child).unwrap(),
        column: "d".into(),
    };
    let Fact::Known(bound) = default(&facts(&s, &pg, Stored::Defaults), &parent, own()) else {
        panic!("a read default is known")
    };
    // The routine and the sequence's owning column, as baseline bindings.
    assert_eq!(bound.bindings.len(), 2);
    assert!(matches!(
        default(&facts(&s, &pg, Stored::Unrepresented), &parent, own()),
        Fact::Unsupported { .. }
    ));
    // Without the catalog's text there is only the declared one: no bindings.
    let Fact::Known(declared) = default(&facts(&s, &pg, Stored::Nothing), &parent, own()) else {
        panic!()
    };
    assert!(declared.bindings.is_empty());
    // A member's default is bound by a sequence whose owner has no UID of its
    // own: that binding is unread, never a binding set without it.
    let member = default(&facts(&s, &pg, Stored::Defaults), &child, inherited());
    assert_eq!(member, Fact::Unread);
}

#[test]
fn a_column_named_like_a_missing_one_without_a_uid_is_unread_not_absent() {
    let mut s = build(Kind::Add, Motion::Attach, false);
    // The baseline parent already holds an `x` the identity file never recorded.
    let x = Column::new("integer".parse().unwrap());
    s.before
        .tables
        .get_mut(&name("app.p"))
        .unwrap()
        .columns
        .insert("x".into(), x);
    let pg = Postgres::new().with_server_version(180000);
    let facts = facts(&s, &pg, Stored::Nothing);
    let parent = Snapshot {
        schema: &s.before,
        ids: &s.base_ids,
    }
    .table_identity(&name("app.p"))
    .unwrap();
    let key = ColumnKey::Recorded(
        Snapshot {
            schema: &s.after,
            ids: &s.ids,
        }
        .column_identity(&name("app.p").column("x"))
        .map(|k| match k {
            ColumnKey::Recorded(r) => r,
            ColumnKey::Partition { .. } => unreachable!(),
        })
        .unwrap(),
    );
    let cell = &facts.base.cells[&(parent, Property::Column(key, Attribute::Presence))];
    assert_eq!(cell.value, Fact::Unread);
    assert!(cell.origin.is_none());
}

#[test]
fn a_renamed_column_never_reads_the_old_holder_of_its_goal_name() {
    // The candidate is cut at the baseline: its column is `d`, the parent's
    // goal name for it is `e`, and the catalog also stores an unrelated `e`.
    let s = build(Kind::Rename, Motion::Attach, false);
    let (before, after) = (
        Snapshot {
            schema: &s.before,
            ids: &s.base_ids,
        },
        Snapshot {
            schema: &s.after,
            ids: &s.ids,
        },
    );
    let pg = Postgres::new().with_server_version(180000);
    let lineage = Lineage::of(before, after);
    let stored = |names: &[&str]| MembershipCatalog {
        columns: names
            .iter()
            .map(|n| (name("app.a").column(*n), StoredColumn::default()))
            .collect(),
    };
    let child = before.table_identity(&name("app.a")).unwrap();
    let key = ColumnKey::Partition {
        table: child.clone(),
        column: "e".into(),
    };
    let at = |catalog: &MembershipCatalog| {
        Read {
            pg: &pg,
            catalog,
            lineage: &lineage,
            ends: [before, after],
        }
        .source(&child, &key, End::Goal)
    };
    assert_eq!(at(&stored(&["d", "e"])), Some(name("app.a").column("d")));
    assert_eq!(at(&stored(&["e"])), None);
}

#[test]
fn a_generated_expression_that_may_bind_is_unread_until_the_catalog_says_what_it_binds() {
    let mut s = build(Kind::Widen, Motion::Detach, false);
    s.before
        .tables
        .get_mut(&name("app.p"))
        .unwrap()
        .columns
        .get_mut("d")
        .unwrap()
        .value_source = ValueSource::Generated(pbps_model::Generated {
        expression: "app.f(1)".into(),
        stored: true,
    });
    let pg = Postgres::new().with_server_version(180000);
    let (before, after) = (
        Snapshot {
            schema: &s.before,
            ids: &s.base_ids,
        },
        Snapshot {
            schema: &s.after,
            ids: &s.ids,
        },
    );
    let column = before.column_identity(&name("app.p").column("d")).unwrap();
    let parent = before.table_identity(&name("app.p")).unwrap();
    let ty = |stored: &MembershipCatalog| {
        let facts = of(&pg, stored, before, after);
        facts.base.cells[&(
            parent.clone(),
            Property::Column(column.clone(), Attribute::Type),
        )]
            .value
            .clone()
    };
    assert_eq!(ty(&MembershipCatalog::default()), Fact::Unread);
    let routine = ModuleId::Routine(RoutineId::new(
        ObjectName::new("app", "f"),
        vec!["integer".parse().unwrap()],
    ));
    let read = MembershipCatalog {
        columns: [(
            name("app.p").column("d"),
            StoredColumn {
                default: Some("app.f(1)".into()),
                routines: [routine].into(),
                ..Default::default()
            },
        )]
        .into(),
    };
    let Fact::Known(known) = ty(&read) else {
        panic!("the catalog read what the expression binds")
    };
    assert_eq!(known.bindings.len(), 1);
}

/// A scene where the baseline parent's `d` becomes `e` and a different column
/// is added under the name `d`: at the baseline the member's slot `d` belongs
/// to the renamed column, not to the added one.
fn rename_and_reuse_the_name(motion: Motion) -> Scene {
    let mut s = build(Kind::Rename, motion, false);
    let mut added = Column::new("integer".parse().unwrap());
    added.nullable = true;
    s.after
        .tables
        .get_mut(&name("app.p"))
        .unwrap()
        .columns
        .insert("d".into(), added);
    s.ids
        .columns
        .insert(uid("c_gggggg"), name("app.p").column("d"));
    s
}

#[test]
fn a_column_added_under_a_renamed_columns_old_name_leaves_its_occupied_slot_alone() {
    let pg = Postgres::new().with_server_version(180000);
    let slot_cells = |s: &Scene| {
        let base = Snapshot {
            schema: &s.before,
            ids: &s.base_ids,
        };
        let child = base.table_identity(&name("app.a")).unwrap();
        let slot = ColumnKey::Partition {
            table: child.clone(),
            column: "d".into(),
        };
        let cells: Vec<_> = facts(s, &pg, Stored::Defaults)
            .base
            .cells
            .into_iter()
            .filter(|((owner, p), _)| {
                owner == &child && matches!(p, Property::Column(k, _) if k == &slot)
            })
            .collect();
        cells
    };
    let alone = slot_cells(&build(Kind::Rename, Motion::Drop, false));
    let reused = slot_cells(&rename_and_reuse_the_name(Motion::Drop));
    assert!(!alone.is_empty());
    assert_eq!(reused, alone);
    let presence = reused
        .iter()
        .find(|((_, p), _)| matches!(p, Property::Column(_, Attribute::Presence)))
        .unwrap();
    assert!(presence.1.origin.is_some());
    // The added column itself is still known to be absent in the parent.
    let s = rename_and_reuse_the_name(Motion::Drop);
    let parent = Snapshot {
        schema: &s.before,
        ids: &s.base_ids,
    }
    .table_identity(&name("app.p"))
    .unwrap();
    let key = Snapshot {
        schema: &s.after,
        ids: &s.ids,
    }
    .column_identity(&name("app.p").column("d"))
    .unwrap();
    let cell = &facts(&s, &pg, Stored::Defaults).base.cells
        [&(parent, Property::Column(key, Attribute::Presence))];
    assert!(cell.origin.is_none());
    assert_ne!(cell.value, Fact::Unread);
}

#[test]
fn a_goal_expression_is_matched_before_the_baseline_bindings_apply() {
    let pg = Postgres::new().with_server_version(180000);
    let s = build(Kind::DropDefault, Motion::Detach, false);
    let before = Snapshot {
        schema: &s.before,
        ids: &s.base_ids,
    };
    let (owner, column) = (
        before.table_identity(&name("app.p")).unwrap(),
        before.column_identity(&name("app.p").column("d")).unwrap(),
    );
    let goal_default = |s: &Scene, expression: &str| {
        let mut s = Scene {
            before: s.before.clone(),
            after: s.after.clone(),
            base_ids: s.base_ids.clone(),
            ids: s.ids.clone(),
        };
        s.after
            .tables
            .get_mut(&name("app.p"))
            .unwrap()
            .columns
            .get_mut("d")
            .unwrap()
            .value_source = ValueSource::Default(expression.into());
        facts(&s, &pg, Stored::Unrepresented).goal.cells[&(
            owner.clone(),
            Property::Column(column.clone(), Attribute::Default),
        )]
            .value
            .clone()
    };
    // The baseline default is stored as `1`, unrepresented. A goal constant
    // that replaces it is not that default and keeps its own known value.
    assert!(matches!(goal_default(&s, "7"), Fact::Known(_)));
    // The same text is the stored default, so the baseline status applies.
    assert!(matches!(goal_default(&s, "1"), Fact::Unsupported { .. }));
}
