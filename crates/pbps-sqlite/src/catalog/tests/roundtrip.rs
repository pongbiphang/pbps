//! What is read, emitted into a fresh database and read again is the same
//! schema; and a declared schema, emitted and read, is the declared one.

use pbps_db::catalog::Pulled;
use pbps_model::Schema;

use super::{declarable, read, read_sql, table};
use crate::catalog::text::tests::closure::{BASES, sequence_with};
use crate::catalog::text::tests::corpus::{self, Case};
use crate::dialect::Sqlite;
use crate::emit::tests::{ids, plan};
use crate::tests::Fresh;

/// The statements that make `schema` in an empty database.
fn make(schema: &Schema) -> Result<Vec<String>, String> {
    let empty = Schema::default();
    plan(
        (&empty, &ids(&empty, &[])),
        (schema, &ids(schema, &[])),
        &Sqlite::default(),
    )
    .map(|s| s.into_iter().map(|s| s.sql).collect())
}

/// Runs `statements` on a fresh database and reads it.
fn made(statements: &[String]) -> Pulled {
    let mut fresh = Fresh::memory();
    for s in statements {
        fresh
            .connection()
            .execute_batch(s)
            .unwrap_or_else(|e| panic!("{s}: {e}"));
    }
    read(&mut fresh)
}

#[test]
fn every_table_index_and_view_of_the_corpus_is_read_emitted_and_read_again_as_itself() {
    let cases: Vec<Case> = corpus::accepted(corpus::tables())
        .into_iter()
        .chain(corpus::accepted(corpus::indexes()))
        .chain(corpus::accepted(corpus::views()))
        .collect();
    assert!(cases.len() > 250, "{}", cases.len());
    let (mut read_whole, mut read_around) = (0, 0);
    for case in &cases {
        let sql = format!("{}; {}", case.setup, case.sql);
        let (_, pulled) = read_sql(&sql);
        // Nothing the model holds is lost on the way through the engine: what
        // was read is declarable, whatever the reader left out of the file.
        let declared = declarable(&pulled.schema);
        let statements = make(&declared).unwrap_or_else(|e| panic!("{sql}\n{e}"));
        let again = made(&statements);
        assert_eq!(again.schema, declared, "{sql}\n{statements:#?}");
        // The names the second read gives are the ones declared: nothing is
        // unnamed any more, so nothing is synthetic.
        assert!(
            again
                .warnings
                .iter()
                .all(|w| !w.contains("no name in the file")),
            "{sql}: {:#?}",
            again.warnings
        );
        if pulled.limitations.is_empty() {
            read_whole += 1;
        } else {
            read_around += 1;
        }
    }
    // Both kinds of case are in the corpus, so neither half of the property is
    // vacuous: files read whole, and files read around what the model lacks.
    assert!(
        read_whole > 150 && read_around > 20,
        "{read_whole} {read_around}"
    );
}

#[test]
fn a_declared_schema_emitted_and_read_is_the_declared_one_and_the_diff_is_empty() {
    let (_, from_file) = read_sql(super::reader::FILE);
    let declared = from_file.schema;
    let statements = make(&declared).expect("plans");
    let pulled = made(&statements);
    assert_eq!(pulled.schema, declared);
    assert!(pulled.warnings.is_empty() && pulled.limitations.is_empty());
    let diff = plan(
        (&pulled.schema, &ids(&pulled.schema, &[])),
        (&declared, &ids(&declared, &[])),
        &Sqlite::default(),
    )
    .expect("plans");
    assert!(diff.is_empty(), "{diff:#?}");
    // Negative: a declared change is a plan, so the empty one above says something.
    let mut changed = declared.clone();
    changed
        .tables
        .values_mut()
        .next()
        .expect("a table")
        .checks
        .insert(
            "extra".into(),
            pbps_model::CheckConstraint {
                expression: "1 = 1".into(),
            },
        );
    let diff = plan(
        (&pulled.schema, &ids(&pulled.schema, &[])),
        (&changed, &ids(&changed, &[])),
        &Sqlite::default(),
    )
    .expect("plans");
    assert!(!diff.is_empty());
}

#[test]
fn a_synthetic_name_cannot_be_declared_or_changed_by_a_statement_so_the_declaration_is_refused() {
    let (_, pulled) = read_sql("CREATE TABLE t(a integer, CHECK (a > 0))");
    // What was read is not declarable as it stands: the reserved prefix is
    // refused, which is the rule that makes a synthetic name unambiguous.
    let error = make(&pulled.schema).expect_err("refused");
    assert!(error.contains("__pbps_unnamed_"), "{error}");
    // Named, it is: the same constraint, declared.
    assert!(make(&declarable(&pulled.schema)).is_ok());
}

#[test]
fn the_alter_forms_leave_a_table_the_reader_reads_whole_after_every_step() {
    let mut steps = 0;
    for (b, base) in BASES.iter().enumerate() {
        for seed in 0..25u64 {
            sequence_with(seed * 7 + b as u64, base, 14, &mut |fresh, name| {
                steps += 1;
                let pulled = read(fresh);
                let t = table(&pulled, name);
                let columns: Vec<String> = fresh
                    .connection()
                    .query(
                        "SELECT name FROM pragma_table_info(?1) ORDER BY cid",
                        &[pbps_db::sqlite::Value::Text(name.to_owned())],
                    )
                    .expect("columns")
                    .iter()
                    .map(|r| r.text(0).expect("name").to_owned())
                    .collect();
                let read: Vec<&str> = t.columns.keys().map(pbps_model::ColumnName::raw).collect();
                assert_eq!(read, columns, "seed {seed} base {b}");
                assert!(
                    pulled.limitations.is_empty(),
                    "seed {seed} base {b}: {:#?}",
                    pulled.limitations
                );
                // And what was read is itself again through the engine.
                let declared = declarable(&pulled.schema);
                let statements =
                    make(&declared).unwrap_or_else(|e| panic!("seed {seed} base {b}: {e}"));
                assert_eq!(made(&statements).schema, declared, "seed {seed} base {b}");
            });
        }
    }
    assert!(steps > 800, "{steps}");
}
