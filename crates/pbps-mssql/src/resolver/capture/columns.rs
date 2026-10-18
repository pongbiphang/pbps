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
    // Each kind's own view adds what only that kind has: a table's temporal
    // and ledger settings, a view's CHECK OPTION, a procedure's startup flag.
    "tables",
    "views",
    "procedures",
    "sql_modules",
    "synonyms",
    "sequences",
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
    // What each object, column or index has beyond those views, from the
    // classification of every catalog view (`catalog.rs`).
    "masked_columns",
    "trigger_events",
    "periods",
    "change_tracking_tables",
    "filetables",
    "fulltext_indexes",
    "fulltext_index_columns",
    "xml_indexes",
    "spatial_indexes",
    "json_indexes",
    "vector_indexes",
    "hash_indexes",
    "selective_xml_index_paths",
    "selective_xml_index_namespaces",
    "spatial_index_tessellations",
    "json_index_paths",
    "partitions",
    "edge_constraints",
    "edge_constraint_clauses",
    "security_policies",
    "security_predicates",
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
    /// A full-text catalog's, stoplist's or search property list's id, 0 or
    /// NULL for none (a stoplist's 0 is the system one), by the kind given.
    FullText(&'static str),
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
        // Every per-index view is filed under its index.
        (
            "xml_indexes"
            | "spatial_indexes"
            | "json_indexes"
            | "vector_indexes"
            | "hash_indexes"
            | "selective_xml_index_paths"
            | "selective_xml_index_namespaces"
            | "spatial_index_tessellations"
            | "json_index_paths"
            | "partitions",
            "object_id" | "index_id",
        ) => KEY,
        (
            "trigger_events"
            | "periods"
            | "change_tracking_tables"
            | "filetables"
            | "fulltext_indexes"
            | "fulltext_index_columns"
            | "edge_constraints"
            | "edge_constraint_clauses"
            | "security_policies"
            | "security_predicates"
            | "masked_columns",
            "object_id",
        ) => KEY,
        ("masked_columns", "column_id") => POSITION,
        ("periods", "start_column_id" | "end_column_id") => Rule::Column("object_id"),
        ("change_tracking_tables", "min_valid_version" | "begin_version" | "cleanup_version") => {
            Rule::Skip("a status: the change-tracking versions reached so far")
        }
        ("filetables", "filename_collation_id") => Rule::AsReported("an engine constant"),
        ("fulltext_indexes", "unique_index_id") => Rule::Index("object_id"),
        ("fulltext_indexes", "fulltext_catalog_id") => Rule::FullText("catalog"),
        ("fulltext_indexes", "stoplist_id") => Rule::FullText("stoplist"),
        ("fulltext_indexes", "property_list_id") => Rule::FullText("property list"),
        (
            "fulltext_indexes",
            "has_crawl_completed"
            | "crawl_type"
            | "crawl_type_desc"
            | "crawl_start_date"
            | "crawl_end_date"
            | "incremental_timestamp"
            | "index_version",
        ) => Rule::Skip("a status: how far the population has got"),
        ("fulltext_index_columns", "column_id" | "type_column_id") => Rule::Column("object_id"),
        ("fulltext_index_columns", "language_id") => Rule::AsReported("an engine constant"),
        ("xml_indexes", "using_xml_index_id") => Rule::Index("object_id"),
        ("xml_indexes" | "selective_xml_index_paths", "path_id") => POSITION,
        // A component's number in its collection: the collection is compared
        // with the schemas it holds.
        ("selective_xml_index_paths", "xml_component_id") => {
            Rule::Skip("a number within its XML schema collection, compared whole")
        }
        ("partitions", "partition_id" | "hobt_id") => Rule::Skip("an internal storage number"),
        ("partitions", "rows") => Rule::Skip("a status: the rows the partition holds"),
        ("partitions", "partition_number") => POSITION,
        ("partitions", "filestream_filegroup_id") => Rule::DataSpace,
        ("edge_constraint_clauses", "clause_number") => POSITION,
        ("edge_constraint_clauses", "from_object_id" | "to_object_id") => Rule::Object,
        ("security_predicates", "security_predicate_id") => POSITION,
        ("security_predicates", "target_object_id") => Rule::Object,
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
            | "tables"
            | "views"
            | "procedures"
            | "sequences"
            | "check_constraints"
            | "default_constraints"
            | "key_constraints"
            | "foreign_keys"
            | "triggers",
            "object_id",
        ) => KEY,
        // A sequence's declaration is compared; what it has issued so far
        // moves with every `NEXT VALUE FOR`.
        ("sequences", "current_value" | "last_used_value" | "is_exhausted") => {
            Rule::Skip("a status: what the sequence has issued so far")
        }
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
        // The highest column number a table has ever used grows with every
        // column added and dropped: the same shape reads differently by its
        // history, not by what it is.
        ("tables", "max_column_id_used") => {
            Rule::Skip("a history: the highest column number ever used")
        }
        ("tables", "history_table_id" | "ledger_view_id") => Rule::Object,
        ("tables", "lob_data_space_id" | "filestream_data_space_id") => Rule::DataSpace,
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
