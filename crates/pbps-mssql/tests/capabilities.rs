//! What SQL Server can do or has, as neutral code asks it (#2950). Each answer
//! is the one the engine's behaviour was written against, so a change to one
//! has to say so here.

use pbps_dialect::Dialect;
use pbps_mssql::Mssql;

#[test]
fn sql_server_has_cross_database_dependencies() {
    assert!(Mssql.has_cross_database_dependencies());
}

#[test]
fn sql_server_answers_no_to_what_pbps_does_only_on_postgresql() {
    // Negative: the defaults stand, so neutral code refuses a table rebuild,
    // an extension, a row-security policy and a relation named in a literal.
    // Two of these are about pbps, not the engine: SQL Server has security
    // policies and could rebuild a table, and nothing here does either.
    assert!(!Mssql.rebuilds_tables());
    assert!(!Mssql.has_extensions());
    assert!(!Mssql.manages_row_security());
    assert!(!Mssql.names_relations_in_literals());
}
