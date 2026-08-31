//! The managed set: which of a database's tables pbps is answerable for
//! (SPEC §8.2).
//!
//! # Why the scope is drawn at all
//!
//! pbps has to be able to share a database with tooling that was there first —
//! that is the precondition for gradual adoption. So a table nobody declared is
//! not drift; it is somebody else's table. Comparing the whole database instead
//! would make the drift check fire on the first run in every real estate, and a
//! check that always fires is a check nobody reads.
//!
//! # Why the identity file draws it, not the declarations
//!
//! [`crate::diff`] matches by uid and never looks at a table the identity file
//! does not name, so scoping by the ids file is scoping by exactly what the
//! comparison can see. Using the declarations instead would let the two
//! disagree, and the difference would show up as a plan that creates a table
//! that already exists.

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{ColumnRef, IdsFile, ObjectName, Schema, TableName, Uid, UidKind};

/// A live schema cut down to the managed set, with what fell outside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scoped {
    /// Only the tables pbps manages. This is what drift and `plan --db` compare.
    pub schema: Schema,

    /// Tables the database has that the identity file does not name. Left
    /// untouched — pbps will neither change nor drop them.
    pub unmanaged: Vec<TableName>,

    /// Tables the identity file names that the database does not have.
    ///
    /// Reported rather than quietly treated as "nothing to compare": against a
    /// recorded state this is a table someone dropped by hand, and against a
    /// fresh baseline it means the ids file and this environment describe
    /// different databases.
    pub missing: Vec<TableName>,

    /// Modules the database has that nobody manages. Left alone, exactly like
    /// an unmanaged table.
    pub unmanaged_modules: Vec<ObjectName>,
}

/// Cuts a live schema down to the managed set.
///
/// # Why modules are scoped by a set instead of by the ids file
///
/// Modules carry no identity — they carry no data, so they never enter the ids
/// file (ADR-0002). The set of managed modules is therefore supplied by the
/// caller, and what each caller passes says what question it is asking: `verify`
/// passes the modules of the **recorded** state ("has this environment moved
/// since pbps last recorded it"), while `plan --db` passes those plus the
/// declared ones ("what would it take to get there"). A module in neither is
/// somebody else's, and pbps neither changes nor drops it.
pub fn scope(schema: &Schema, ids: &IdsFile, managed_modules: &BTreeSet<ObjectName>) -> Scoped {
    let managed: BTreeSet<&TableName> = ids.tables.values().collect();

    let mut scoped = Schema::default();
    let mut unmanaged = Vec::new();
    for (name, table) in &schema.tables {
        if managed.contains(name) {
            scoped.tables.insert(name.clone(), table.clone());
        } else {
            unmanaged.push(name.clone());
        }
    }

    let missing = managed
        .into_iter()
        .filter(|n| !schema.tables.contains_key(*n))
        .cloned()
        .collect();

    let mut unmanaged_modules = Vec::new();
    for (name, module) in &schema.modules {
        if managed_modules.contains(name) {
            scoped.modules.insert(name.clone(), module.clone());
        } else {
            unmanaged_modules.push(name.clone());
        }
    }

    Scoped {
        schema: scoped,
        unmanaged,
        missing,
        unmanaged_modules,
    }
}

/// The identity of a database **as observed**, for comparing a live schema
/// against a recorded one.
///
/// # The problem this solves
///
/// [`crate::diff`] matches by uid, so a comparison where both sides carry the
/// same identity file can only ever see attribute changes on objects that exist
/// on both sides. A column somebody added by hand has no uid, and one they
/// dropped still has its old one — so the two changes a drift check most needs
/// to report are exactly the two it would miss.
///
/// So the live side gets an identity file built from what is actually there:
/// objects that match a recorded name keep their recorded uid, objects that do
/// not get a derived one, and recorded objects with nothing to match simply do
/// not appear. The differ then sees additions and removals the way it sees them
/// in a plan.
///
/// # Matching by name, and what that costs
///
/// A hand-rename comes out as a drop plus an add. That is not a shortcoming to
/// be apologized for: nothing in the database records that the two were the same
/// column, and a tool that guessed would eventually guess wrong about a
/// column holding real data. Rename intent is human input everywhere else in
/// pbps (constraint 6), and drift is no exception.
pub fn observed_ids(live: &Schema, recorded: &IdsFile) -> IdsFile {
    let tables_by_name: BTreeMap<&TableName, &Uid> =
        recorded.tables.iter().map(|(u, n)| (n, u)).collect();
    let columns_by_ref: BTreeMap<&ColumnRef, &Uid> =
        recorded.columns.iter().map(|(u, c)| (c, u)).collect();

    // Every recorded uid is off-limits for derivation: reusing one would make
    // the differ match a brand-new column to an unrelated recorded one and
    // report a rename that never happened.
    let mut taken: BTreeSet<Uid> = recorded
        .tables
        .keys()
        .chain(recorded.columns.keys())
        .chain(recorded.tombstones.keys())
        .cloned()
        .collect();

    let mut observed = IdsFile::default();
    for (name, table) in &live.tables {
        let uid = match tables_by_name.get(name) {
            Some(u) => (*u).clone(),
            None => free_uid(UidKind::Table, &name.to_string(), &mut taken),
        };
        observed.tables.insert(uid, name.clone());

        for column in table.columns.keys() {
            let cref = name.column(column);
            let uid = match columns_by_ref.get(&cref) {
                Some(u) => (*u).clone(),
                None => free_uid(UidKind::Column, &cref.to_string(), &mut taken),
            };
            observed.columns.insert(uid, cref);
        }
    }
    observed
}

/// Derives a uid from a name, stepping past anything already claimed.
///
/// The salt makes the walk deterministic, so the same database observed twice
/// produces the same identity file and therefore a byte-identical drift report.
fn free_uid(kind: UidKind, seed: &str, taken: &mut BTreeSet<Uid>) -> Uid {
    for salt in 0.. {
        let uid = Uid::derived(kind, seed, salt);
        if taken.insert(uid.clone()) {
            return uid;
        }
    }
    unreachable!("u32 salts exhausted against a 32^6 space")
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use pbps_model::{Column, ColumnType, Table};

    fn t(s: &str) -> TableName {
        s.parse().unwrap()
    }

    fn schema(names: &[&str]) -> Schema {
        let mut s = Schema::default();
        for n in names {
            let mut columns = IndexMap::new();
            columns.insert(
                "id".to_string(),
                Column::new("int".parse::<ColumnType>().unwrap()),
            );
            s.tables.insert(
                t(n),
                Table {
                    columns,
                    ..Default::default()
                },
            );
        }
        s
    }

    fn ids(entries: &[(&str, &str)]) -> IdsFile {
        let mut ids = IdsFile::default();
        for (uid, name) in entries {
            ids.tables.insert(uid.parse::<Uid>().unwrap(), t(name));
        }
        ids
    }

    #[test]
    fn only_the_declared_tables_are_kept() {
        let scoped = scope(
            &schema(&["dbo.customer", "dbo.order", "dbo.legacy_audit"]),
            &ids(&[("t_aaaaaa", "dbo.customer"), ("t_bbbbbb", "dbo.order")]),
            &BTreeSet::new(),
        );
        assert_eq!(
            scoped.schema.tables.keys().collect::<Vec<_>>(),
            vec![&t("dbo.customer"), &t("dbo.order")]
        );
        assert_eq!(scoped.unmanaged, vec![t("dbo.legacy_audit")]);
        assert!(scoped.missing.is_empty());
    }

    /// Somebody else's table is not drift. Without this the check would fire on
    /// the first run in every database that has any history.
    #[test]
    fn an_unmanaged_table_does_not_change_the_managed_state() {
        let ids = ids(&[("t_aaaaaa", "dbo.customer")]);
        assert_eq!(
            scope(&schema(&["dbo.customer"]), &ids, &BTreeSet::new()).schema,
            scope(
                &schema(&["dbo.customer", "dbo.other_tool"]),
                &ids,
                &BTreeSet::new()
            )
            .schema,
        );
    }

    /// A declared table the database does not have must be named, not silently
    /// skipped: it is either a hand-dropped table or an ids file describing a
    /// different database.
    #[test]
    fn a_declared_table_missing_from_the_database_is_reported() {
        let scoped = scope(
            &schema(&["dbo.customer"]),
            &ids(&[("t_aaaaaa", "dbo.customer"), ("t_bbbbbb", "dbo.order")]),
            &BTreeSet::new(),
        );
        assert_eq!(scoped.missing, vec![t("dbo.order")]);
        assert!(scoped.unmanaged.is_empty());
    }

    #[test]
    fn an_empty_identity_file_manages_nothing() {
        let scoped = scope(
            &schema(&["dbo.customer"]),
            &IdsFile::default(),
            &BTreeSet::new(),
        );
        assert!(scoped.schema.tables.is_empty());
        assert_eq!(scoped.unmanaged, vec![t("dbo.customer")]);
    }

    /// Columns need their own fixture here: `observed_ids` is entirely about
    /// which columns exist, which the table-level helper above ignores.
    fn schema_with(spec: &[(&str, &[&str])]) -> Schema {
        let mut s = Schema::default();
        for (name, cols) in spec {
            let mut columns = IndexMap::new();
            for c in *cols {
                columns.insert(
                    (*c).to_string(),
                    Column::new("int".parse::<ColumnType>().unwrap()),
                );
            }
            s.tables.insert(
                t(name),
                Table {
                    columns,
                    ..Default::default()
                },
            );
        }
        s
    }

    fn recorded(tables: &[(&str, &str)], columns: &[(&str, &str)]) -> IdsFile {
        let mut ids = IdsFile::default();
        for (uid, name) in tables {
            ids.tables.insert(uid.parse::<Uid>().unwrap(), t(name));
        }
        for (uid, cref) in columns {
            ids.columns
                .insert(uid.parse::<Uid>().unwrap(), cref.parse().unwrap());
        }
        ids
    }

    #[test]
    fn an_object_that_still_exists_keeps_its_recorded_identity() {
        let ids = recorded(
            &[("t_aaaaaa", "dbo.customer")],
            &[("c_bbbbbb", "dbo.customer.email")],
        );
        let observed = observed_ids(&schema_with(&[("dbo.customer", &["email"])]), &ids);
        assert_eq!(observed.tables, ids.tables);
        assert_eq!(observed.columns, ids.columns);
    }

    /// The two cases a shared identity file makes invisible, and the whole
    /// reason this function exists.
    #[test]
    fn a_hand_added_column_gets_an_identity_and_a_dropped_one_loses_its_place() {
        let ids = recorded(
            &[("t_aaaaaa", "dbo.customer")],
            &[
                ("c_bbbbbb", "dbo.customer.email"),
                ("c_cccccc", "dbo.customer.gone"),
            ],
        );
        let observed = observed_ids(
            &schema_with(&[("dbo.customer", &["email", "nickname"])]),
            &ids,
        );

        // Sorted by name, because the map is keyed by uid and uid order is not
        // name order.
        let mut names: Vec<String> = observed.columns.values().map(ToString::to_string).collect();
        names.sort();
        assert_eq!(names, ["dbo.customer.email", "dbo.customer.nickname"]);
        assert!(
            !observed.columns.values().any(|c| c.name == "gone"),
            "a dropped column must not appear, or the differ cannot see the drop"
        );
        assert!(
            observed.columns.keys().all(|u| u.as_str() != "c_cccccc"),
            "the dropped column's uid must not be handed to another column"
        );
    }

    /// The report is fed to a hook. A payload that differs between two runs
    /// over an unchanged database cannot be deduplicated by whatever receives
    /// it.
    #[test]
    fn observing_the_same_database_twice_gives_the_same_identities() {
        let live = schema_with(&[("dbo.customer", &["email", "nickname"])]);
        let ids = recorded(&[("t_aaaaaa", "dbo.customer")], &[]);
        assert_eq!(observed_ids(&live, &ids), observed_ids(&live, &ids));
    }

    /// A derived uid colliding with a recorded one would make the differ match
    /// a new column to an unrelated old one and report a rename that never
    /// happened.
    #[test]
    fn a_derived_identity_never_collides_with_a_recorded_one() {
        let live = schema_with(&[("dbo.customer", &["nickname"])]);
        // Claim the uid derivation would otherwise hand out.
        let clash = Uid::derived(UidKind::Column, "dbo.customer.nickname", 0);
        let mut ids = recorded(&[("t_aaaaaa", "dbo.customer")], &[]);
        ids.columns
            .insert(clash.clone(), "dbo.other.x".parse().unwrap());

        let observed = observed_ids(&live, &ids);
        let assigned = observed.columns.keys().next().unwrap();
        assert_ne!(assigned, &clash);
        assert_eq!(
            assigned,
            &Uid::derived(UidKind::Column, "dbo.customer.nickname", 1),
            "the walk past a collision must be deterministic too"
        );
    }

    /// A table nobody recorded is still observable — that is how a hand-created
    /// table inside the managed set becomes visible at all.
    #[test]
    fn an_unrecorded_table_is_given_an_identity() {
        let observed = observed_ids(
            &schema_with(&[("dbo.brand_new", &["id"])]),
            &IdsFile::default(),
        );
        assert_eq!(observed.tables.len(), 1);
        assert_eq!(observed.columns.len(), 1);
    }

    // ---- modules (ADR-0002) ----

    fn with_module(mut schema: Schema, name: &str) -> Schema {
        schema.modules.insert(
            name.parse().unwrap(),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: None,
                on: None,
                definition: "SELECT 1".into(),
            },
        );
        schema
    }

    /// A module nobody manages is somebody else's object, exactly like an
    /// undeclared table: pbps neither changes nor drops it, and drift must not
    /// fire on it.
    #[test]
    fn only_the_named_modules_are_in_scope() {
        let live = with_module(
            with_module(schema(&["dbo.customer"]), "dbo.v_mine"),
            "dbo.v_theirs",
        );
        let managed = BTreeSet::from(["dbo.v_mine".parse::<TableName>().unwrap()]);
        let scoped = scope(&live, &ids(&[("t_aaaaaa", "dbo.customer")]), &managed);

        assert_eq!(scoped.schema.modules.len(), 1);
        assert!(
            scoped
                .schema
                .modules
                .contains_key(&"dbo.v_mine".parse().unwrap())
        );
        assert_eq!(
            scoped.unmanaged_modules,
            vec!["dbo.v_theirs".parse::<TableName>().unwrap()]
        );
    }
}
