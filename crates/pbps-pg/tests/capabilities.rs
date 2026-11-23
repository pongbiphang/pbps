//! What PostgreSQL can do or has, as neutral code asks it (#2950). Each answer
//! is the one the engine's behaviour was written against, so a change to one
//! has to say so here.

use pbps_dialect::Dialect;
use pbps_pg::Postgres;

#[test]
fn postgresql_has_each_thing_neutral_code_used_to_find_by_its_name() {
    let d = Postgres::new();
    assert!(d.rebuilds_tables());
    assert!(d.has_extensions());
    assert!(d.manages_row_security());
    assert!(d.names_relations_in_literals());
}

#[test]
fn postgresql_has_no_cross_database_dependency() {
    // Negative: a database is a hard boundary on this engine, so the default
    // stands and a declared dependency is refused.
    assert!(!Postgres::new().has_cross_database_dependencies());
}
