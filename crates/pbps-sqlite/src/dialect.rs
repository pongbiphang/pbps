//! The SQLite dialect (#2950, item 2): names, quoting, the type table and what
//! is refused, answered for the pure side of the engine.
//!
//! [`Sqlite`] implements [`Dialect`]. Everything a declaration, a differ or a
//! scan asks of the engine without a connection is answered here or in
//! `ident`, `types`, `validate`, `lexis` and `names`; the SQL a change becomes
//! is the emitter's (item 3), and `PlanDialect`, which needs the differ's
//! sealed plan type, waits for it. Until then `intermediate_tables` answers
//! `NotBuilt`, which the trait documents for a dialect arriving in steps, and
//! never an empty answer.

use std::borrow::Cow;

use pbps_dialect::{Dialect, DialectError, Lexicon, TransactionFraming, TypeChangeRisk};
use pbps_model::{
    Change, ColumnType, Module, ModuleId, ModuleKind, Role, SessionZone, Strategy, Table, TableName,
};

use crate::{DEFAULT_VERSION_FLOOR, TRANSACTION, Version, ident, lexis, types, validate};

/// The SQLite dialect. Holds the version floor as a value: the
/// oldest SQLite the project targets, which later items gate what they write
/// on. The project field that sets it belongs to the wiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sqlite {
    floor: Version,
}

impl Sqlite {
    /// A dialect for a project that targets `floor` and later.
    #[must_use]
    pub const fn with_floor(floor: Version) -> Self {
        Self { floor }
    }

    /// The oldest SQLite this dialect writes for.
    #[must_use]
    pub const fn floor(&self) -> Version {
        self.floor
    }

    /// Whether a feature that arrived in `version` may be written.
    #[must_use]
    pub fn at_least(&self, version: Version) -> bool {
        self.floor >= version
    }
}

impl Default for Sqlite {
    fn default() -> Self {
        Self::with_floor(DEFAULT_VERSION_FLOOR)
    }
}

impl Dialect for Sqlite {
    fn name(&self) -> &'static str {
        types::DIALECT
    }

    fn lexicon(&self) -> Lexicon {
        ident::LEXICON
    }

    /// The code of a definition, read with SQLite's own lexis: the shared
    /// scanner nests block comments, SQLite does not.
    fn code_only(&self, definition: &str) -> String {
        lexis::code_only(definition)
    }

    fn fold_ident<'a>(&self, ident: &'a str) -> Cow<'a, str> {
        ident::fold(ident)
    }

    fn quote_ident(&self, ident: &str) -> Result<String, DialectError> {
        ident::quote(ident)
    }

    fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
        types::normalize(ty)
    }

    fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> TypeChangeRisk {
        types::change_risk(from, to)
    }

    fn validate_table(&self, name: &TableName, table: &Table) -> Vec<DialectError> {
        validate::table(name, table)
    }

    fn validate_module(&self, id: &ModuleId, module: &Module) -> Vec<DialectError> {
        validate::module(id, module)
    }

    fn validate_role(
        &self,
        name: &pbps_model::RoleName,
        role: &Role,
        _schema: &pbps_model::Schema,
    ) -> Vec<DialectError> {
        validate::role(name, role)
    }

    /// Views are in the tables' namespace (measured: a view and a table of one
    /// name collide). A trigger has a namespace of its own, and a procedure or
    /// a function does not exist.
    fn shares_namespace_with_tables(&self, kind: ModuleKind) -> bool {
        match kind {
            ModuleKind::View => true,
            ModuleKind::Procedure | ModuleKind::Function | ModuleKind::Trigger => false,
        }
    }

    /// Measured: an index may not be named like a table, a view or any other
    /// index in the database, whichever table it is on.
    fn indexes_share_namespace_with_tables(&self) -> bool {
        true
    }

    /// Measured: constraint names are unique nowhere. Two checks, or a primary
    /// key and a unique constraint, may share one, even in another case.
    fn constraints_share_namespace_with_tables(&self) -> bool {
        false
    }

    /// A partial index's `WHERE` takes the table's own columns and
    /// deterministic built-ins; it names no module.
    fn filter_binds_expression(&self) -> bool {
        false
    }

    /// SQLite has no roles at all, so none is the tool's to create, rename or
    /// drop. The trait's default says `true`, which would claim they are.
    fn manages_roles(&self) -> bool {
        false
    }

    /// `BEGIN IMMEDIATE`; SQLite has no session time zone.
    fn transaction_framing(&self, _zone: &SessionZone) -> TransactionFraming {
        TRANSACTION
    }

    /// None: a built-in SQLite expression cannot write, so a probe needs no
    /// transaction of its own to undo one.
    fn probe_framing(&self) -> Option<TransactionFraming> {
        None
    }

    /// Not built: SQL generation is item 3. Said, never answered empty.
    fn intermediate_tables(
        &self,
        _change: &Change,
        _strategy: Strategy,
    ) -> Result<Vec<TableName>, DialectError> {
        Err(DialectError::NotBuilt {
            dialect: types::DIALECT,
            part: "SQL generation (item 3 of #2950)".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::accepts;
    use pbps_dialect::{check_constraint_names, check_index_names, check_module_names};
    use pbps_model::{Column, IndexColumn, IndexKey, ModuleOptions, ObjectName, RoleName, Schema};

    fn ty(text: &str) -> ColumnType {
        text.parse().expect("a type")
    }

    /// Every answer the plan gives for a method this implementation answers.
    #[test]
    fn each_answered_method_gives_the_answer_the_engine_supports() {
        let d = Sqlite::default();
        assert_eq!(d.name(), "sqlite");
        assert_eq!(d.fold_ident("Ab\u{c9}"), "ab\u{c9}");
        assert_eq!(d.quote_ident("a\"b").expect("quote"), "\"a\"\"b\"");
        assert!(matches!(
            d.quote_ident(""),
            Err(DialectError::UnquotableIdent(_))
        ));
        assert_eq!(
            d.normalize_type(&ty("character varying(10)"))
                .expect("normalize"),
            ty("varchar(10)")
        );
        assert!(d.normalize_type(&ty("serial")).is_err());
        assert_eq!(
            d.type_change_risk(&ty("int"), &ty("text")),
            TypeChangeRisk::Narrowing
        );
        assert_eq!(
            d.type_change_risk(&ty("int"), &ty("bigint")),
            TypeChangeRisk::Safe
        );
        assert_eq!(
            d.type_change_risk(&ty("serial"), &ty("int")),
            TypeChangeRisk::Incompatible
        );
        assert_eq!(d.transaction_framing(&SessionZone::utc()), TRANSACTION);
        assert_eq!(d.probe_framing(), None);
        assert_eq!(d.batch_separator(), None);
        assert!(
            d.lexicon().quoted_identifiers == ident::LEXICON.quoted_identifiers
                && !d.lexicon().escape_strings
        );
        // The namespaces, each measured: a view shares the tables', a trigger does not,
        // an index shares them across tables, constraint names are unique nowhere.
        assert!(d.shares_namespace_with_tables(ModuleKind::View));
        for kind in [
            ModuleKind::Trigger,
            ModuleKind::Procedure,
            ModuleKind::Function,
        ] {
            assert!(!d.shares_namespace_with_tables(kind), "{kind}");
        }
        assert!(
            d.indexes_share_namespace_with_tables() && !d.constraints_share_namespace_with_tables()
        );
        assert!(
            accepts("CREATE TABLE t(c); CREATE VIEW u AS SELECT 1; CREATE TABLE u2(c)")
                && !accepts("CREATE TABLE t(c); CREATE VIEW t AS SELECT 1")
        );
        assert!(!d.filter_binds_expression());
    }

    /// Nothing the engine builds yet is answered with an empty success: SQL
    /// generation is said to be unbuilt, by name.
    #[test]
    fn sql_generation_is_said_to_be_unbuilt_and_never_answered_empty() {
        let change = Change::Table(pbps_model::TableChange::DropTable {
            uid: "t_aaaaaa".parse().expect("uid"),
            name: TableName::new("main", "t"),
            detach_from: None,
        });
        match Sqlite::default().intermediate_tables(&change, Strategy::default()) {
            Err(DialectError::NotBuilt {
                dialect: "sqlite",
                part,
            }) => assert!(part.contains("item 3"), "{part}"),
            other => panic!("expected NotBuilt, got {other:?}"),
        }
    }

    /// The methods this implementation leaves at the trait's default, which is
    /// SQLite's answer for each (nothing here to override), pinned so a later
    /// edit that changes one has to say so.
    #[test]
    fn the_methods_left_at_their_defaults_are_sqlite_s_answers() {
        let d = Sqlite::default();
        assert!(
            !d.holds_descriptions()
                && !d.records_conversions()
                && !d.manages_roles()
                && !d.changes_numbering()
        );
        assert!(
            !d.rebuilds_modules(),
            "a view edit is a drop and a create, which takes no grant with it: SQLite has none"
        );
        assert!(
            !d.compares_names_exactly(),
            "default; SQLite folds ASCII case (see `names::check_collisions`)"
        );
        assert!(!d.creates_public_executable_routines() && !d.relations_have_row_types());
        for kind in [
            ModuleKind::View,
            ModuleKind::Trigger,
            ModuleKind::Procedure,
            ModuleKind::Function,
        ] {
            assert!(!d.overloads(kind), "{kind}");
        }
        assert!(
            d.deprecated_types().is_empty() && d.declaration_notes(&Schema::default()).is_empty()
        );
        assert!(!d.type_change_needs_using(&ty("int"), &ty("text")));
        // Types, search objects and sequences keep the trait's refusal by name.
        let sequence = pbps_model::Sequence {
            ty: ty("int"),
            start: 1,
            increment: 1,
            options: Default::default(),
        };
        let refused =
            d.validate_sequence(&ObjectName::new("main", "s"), &sequence, &Schema::default());
        assert!(matches!(
            refused[0],
            DialectError::Unsupported {
                dialect: "sqlite",
                ..
            }
        ));
        let enumeration = pbps_model::ManagedType::Enum {
            values: vec!["a".into()],
        };
        let refused = d.validate_type(&"main.e".parse().expect("type ref"), &enumeration);
        assert!(matches!(
            refused[0],
            DialectError::Unsupported {
                dialect: "sqlite",
                ..
            }
        ));
        assert!(!accepts("CREATE TYPE e AS ENUM ('a')") && !accepts("CREATE SEQUENCE s"));
    }

    #[test]
    fn the_floor_is_a_value_that_gates_what_may_be_written() {
        assert_eq!(Sqlite::default().floor(), DEFAULT_VERSION_FLOOR);
        assert_eq!(
            Sqlite::default(),
            Sqlite::with_floor(Version::new(3, 35, 0))
        );
        let old = Sqlite::with_floor(Version::new(3, 24, 0));
        assert!(old.at_least(Version::new(3, 8, 0)) && old.at_least(Version::new(3, 24, 0)));
        assert!(
            !old.at_least(Version::new(3, 35, 0)),
            "DROP COLUMN needs 3.35"
        );
        assert!(
            Sqlite::default().at_least(Version::new(3, 35, 0))
                && !Sqlite::default().at_least(Version::new(3, 37, 0))
        );
    }

    /// The overridden `code_only` is the one that reads SQLite's comments.
    #[test]
    fn code_only_reads_a_nested_comment_the_way_the_engine_does() {
        let text = "SELECT 1 /* /* */ ; DROP TABLE t /* */";
        assert!(Sqlite::default().code_only(text).contains("DROP TABLE t"));
        assert!(
            !ident::LEXICON.code_only(text).contains("DROP TABLE t"),
            "the shared scanner would not see it"
        );
    }

    fn table_with(columns: &[&str]) -> pbps_model::Table {
        let mut table = pbps_model::Table::default();
        for c in columns {
            table.columns.insert((*c).into(), Column::new(ty("int")));
        }
        table
    }

    fn key(name: &str) -> IndexColumn {
        IndexColumn {
            key: IndexKey::Column(name.into()),
            descending: false,
            opclass: None,
            collation: None,
            opclass_parameters: Default::default(),
        }
    }

    fn with_index(mut table: pbps_model::Table, name: &str) -> pbps_model::Table {
        table.indexes.insert(
            name.into(),
            pbps_model::Index {
                columns: vec![key("c")],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: pbps_model::IndexMethod::Btree,
                storage_parameters: Default::default(),
                filegroup: None,
            },
        );
        table
    }

    fn a_view(schema: &mut Schema, name: &str) {
        schema.modules.insert(
            ModuleId::Named(ObjectName::new("main", name)),
            Module {
                kind: ModuleKind::View,
                description: None,
                options: ModuleOptions::default(),
                definition: "SELECT 1".into(),
            },
        );
    }

    /// The shared name checks run with this dialect: silent on a valid schema,
    /// and each reports what the engine refuses (a view named like a table, an
    /// index named like another table's index), as the namespace answers say.
    #[test]
    fn the_shared_name_checks_follow_the_namespace_answers() {
        let d = Sqlite::default();
        let mut valid = Schema::default();
        valid.tables.insert(
            TableName::new("main", "a"),
            with_index(table_with(&["c"]), "ia"),
        );
        valid.tables.insert(
            TableName::new("main", "b"),
            with_index(table_with(&["c"]), "ib"),
        );
        a_view(&mut valid, "v");
        for found in [
            check_module_names(&valid, &d),
            check_index_names(&valid, &d),
            check_constraint_names(&valid, &d),
        ] {
            assert!(found.is_empty(), "{found:?}");
        }
        assert!(accepts(
            "CREATE TABLE a(c); CREATE TABLE b(c); CREATE INDEX ia ON a(c); CREATE INDEX ib ON b(c); CREATE VIEW v AS SELECT 1"
        ));

        let mut clash = valid.clone();
        a_view(&mut clash, "a");
        assert!(!check_module_names(&clash, &d).is_empty());
        assert!(!accepts("CREATE TABLE a(c); CREATE VIEW a AS SELECT 1"));

        let mut shared = Schema::default();
        shared.tables.insert(
            TableName::new("main", "a"),
            with_index(table_with(&["c"]), "i"),
        );
        shared.tables.insert(
            TableName::new("main", "b"),
            with_index(table_with(&["c"]), "i"),
        );
        assert!(!check_index_names(&shared, &d).is_empty());
        assert!(!accepts(
            "CREATE TABLE a(c); CREATE TABLE b(c); CREATE INDEX i ON a(c); CREATE INDEX i ON b(c)"
        ));

        // Case-only differences are not these checks' (they compare exactly): the free function is.
        let mut cased = Schema::default();
        cased
            .tables
            .insert(TableName::new("main", "Parent"), table_with(&["c"]));
        cased
            .tables
            .insert(TableName::new("main", "parent"), table_with(&["c"]));
        assert!(
            check_module_names(&cased, &d).is_empty() && check_index_names(&cased, &d).is_empty()
        );
        assert_eq!(crate::names::check_collisions(&cased).len(), 1);
    }

    /// The trait's two refusals the dialect crate must answer for itself: a
    /// role (the default would accept it) and every module kind but a view.
    #[test]
    fn the_defaults_that_would_accept_are_overridden() {
        let d = Sqlite::default();
        let refused = d.validate_role(
            &RoleName::new("r"),
            &pbps_model::Role::default(),
            &Schema::default(),
        );
        assert!(matches!(
            refused[0],
            DialectError::Unsupported {
                dialect: "sqlite",
                ..
            }
        ));
        let procedure = Module {
            kind: ModuleKind::Procedure,
            description: None,
            options: ModuleOptions::default(),
            definition: "x".into(),
        };
        let id = ModuleId::Named(ObjectName::new("main", "p"));
        assert!(!d.validate_module(&id, &procedure).is_empty());
        let table_errors = d.validate_table(&TableName::new("main", "t"), &table_with(&["c"]));
        assert!(table_errors.is_empty(), "{table_errors:?}");
        assert!(
            !d.validate_table(&TableName::new("s", "t"), &table_with(&["c"]))
                .is_empty()
        );
    }
}
