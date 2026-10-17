//! What each column of a catalog view means to the capture (#2153).
//!
//! Every column a view reports is a property, unless a rule here says
//! otherwise: `docs/RESOLVER-MSSQL.md` §8's rule, applied to the capture,
//! because a hand-written list of what to read missed one column after
//! another (#2150's rounds b to f). A column no rule names is compared as the
//! engine reports it, so a column a later build adds is compared before
//! anyone has read about it.
//!
//! The one exception is an id. A number names a row of this database only,
//! and the same object has another on scratch or once rebuilt, so an id is
//! compared as the logical identity it names. An id column no rule names
//! cannot be translated and refuses by name rather than be compared raw: the
//! coverage test (`mssql_every_catalog_column_has_a_rule_or_is_compared`)
//! fails on such a column before a capture ever meets one.

/// The views read whole, each with the columns that key its rows to what
/// they describe.
pub(super) const VIEWS: &[&str] = &[
    "objects",
    "sql_modules",
    "synonyms",
    "check_constraints",
    "default_constraints",
    "key_constraints",
    "foreign_keys",
    "foreign_key_columns",
    "triggers",
    "columns",
    "computed_columns",
    "identity_columns",
    "indexes",
    "index_columns",
    "parameters",
    "types",
    "table_types",
];

/// What one column of one view is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Rule {
    /// Not compared, for the reason given.
    Skip(&'static str),
    /// Compared as reported though its name ends in `_id`, for the reason
    /// given: the same on every database that holds the same shape.
    AsReported(&'static str),
    /// A principal's id; NULL or absent is none.
    Principal,
    /// A schema's id.
    Schema,
    /// An object's id, 0 or NULL for none: a table type's own table object
    /// is named by its type.
    Object,
    /// A type's `user_type_id`.
    Type,
    /// A column's id in the object the named column of the same row holds.
    Column(&'static str),
    /// An index's id in the object the named column of the same row holds.
    Index(&'static str),
    /// A filegroup's or partition scheme's id.
    DataSpace,
    /// An XML schema collection's id, 0 for none.
    XmlCollection,
    /// A column encryption key's id, NULL for none.
    EncryptionKey,
    /// `sys.sql_modules.execute_as_principal_id`: NULL is the caller, -2 the
    /// owner, any other a principal (`SELF` is stored as its creator's).
    ExecuteAs,
}

const KEY: Rule = Rule::Skip("the row's key: the identity it is filed under");
const POSITION: Rule = Rule::AsReported("a position in its table, routine or key");
const DATES: Rule = Rule::Skip("a creation or change date differs by construction");

/// The rule for `column` of `sys.<view>`, or `None` when it is compared as
/// the engine reports it.
pub(super) fn rule(view: &str, column: &str) -> Option<Rule> {
    Some(match (view, column) {
        (_, "create_date" | "modify_date") => DATES,
        // Each row is filed under the identity its key names.
        ("foreign_key_columns", "constraint_object_id") => KEY,
        ("foreign_key_columns", "parent_object_id" | "referenced_object_id") => Rule::Object,
        ("foreign_key_columns", "parent_column_id") => Rule::Column("parent_object_id"),
        ("foreign_key_columns", "referenced_column_id") => Rule::Column("referenced_object_id"),
        ("index_columns", "object_id" | "index_id") => KEY,
        ("index_columns", "column_id") => Rule::Column("object_id"),
        ("indexes", "object_id") => KEY,
        // An index is filed under its name; its number is the engine's.
        ("indexes", "index_id") => KEY,
        ("parameters", "object_id") => KEY,
        ("columns" | "computed_columns" | "identity_columns", "object_id") => KEY,
        (
            "objects"
            | "sql_modules"
            | "synonyms"
            | "check_constraints"
            | "default_constraints"
            | "key_constraints"
            | "foreign_keys"
            | "triggers",
            "object_id",
        ) => KEY,
        ("types" | "table_types", "user_type_id") => KEY,
        ("columns" | "computed_columns" | "identity_columns", "column_id") => POSITION,
        ("parameters", "parameter_id") => POSITION,
        ("index_columns", "index_column_id") => POSITION,
        ("foreign_key_columns", "constraint_column_id") => POSITION,
        // A table type's own table object is the engine's, read through its
        // columns (`is_ms_shipped = 1`, measured on #2141).
        ("table_types", "type_table_object_id") => {
            Rule::Skip("the type's own table object is read through its columns")
        }
        // The last value an identity issued moves with every insert.
        ("identity_columns", "last_value") => {
            Rule::Skip("a status: the last value the identity issued")
        }
        ("sql_modules", "execute_as_principal_id") => Rule::ExecuteAs,
        (_, "principal_id") => Rule::Principal,
        (_, "schema_id") => Rule::Schema,
        (
            _,
            "parent_object_id" | "referenced_object_id" | "default_object_id" | "rule_object_id",
        ) => Rule::Object,
        ("triggers", "parent_id") => Rule::Object,
        (_, "user_type_id") => Rule::Type,
        // The engine's own type number, fixed for every database: a table
        // type's (243) has no `sys.types` row of its own.
        (_, "system_type_id") => Rule::AsReported("an engine constant"),
        (_, "parent_column_id") => Rule::Column("parent_object_id"),
        ("foreign_keys", "key_index_id") => Rule::Index("referenced_object_id"),
        ("key_constraints", "unique_index_id") => Rule::Index("parent_object_id"),
        (_, "data_space_id") => Rule::DataSpace,
        (_, "xml_collection_id") => Rule::XmlCollection,
        (_, "column_encryption_key_id") => Rule::EncryptionKey,
        _ => return None,
    })
}

/// Whether a column no rule names may be compared as reported. An id names
/// a row of this database only, so it may not.
pub(super) fn comparable_raw(column: &str) -> bool {
    !column.ends_with("_id")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_column_needs_a_rule_and_any_other_is_compared() {
        assert_eq!(rule("objects", "schema_id"), Some(Rule::Schema));
        assert_eq!(rule("indexes", "filter_definition"), None);
        assert!(comparable_raw("filter_definition"));
        assert!(!comparable_raw("some_future_id"));
        assert_eq!(rule("columns", "some_future_id"), None);
    }

    #[test]
    fn dates_and_the_identitys_last_value_are_skipped_and_its_seed_is_not() {
        assert!(matches!(
            rule("objects", "modify_date"),
            Some(Rule::Skip(_))
        ));
        assert!(matches!(
            rule("identity_columns", "last_value"),
            Some(Rule::Skip(_))
        ));
        assert_eq!(rule("identity_columns", "seed_value"), None);
    }
}
