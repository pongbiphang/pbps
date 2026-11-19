//! The tokenizer is total: it answers `Ok` or `Err` for any text and never
//! panics. Every prefix and every single-character deletion of every corpus
//! text is fed in. Where the engine also takes the mutated text, what the
//! tokenizer reads from the text the engine stored must agree with the pragmas.
//! (A character, not a byte: deleting one byte of a multi-byte character would
//! not be text.)

use pbps_db::sqlite::Connection;

use super::super::table;
use super::corpus::{self, Case};
use super::facts::{agree, facts, stored};
use crate::tests::Fresh;

fn mutations(sql: &str) -> Vec<String> {
    let bounds: Vec<usize> = sql
        .char_indices()
        .map(|(at, _)| at)
        .chain([sql.len()])
        .collect();
    let mut out: Vec<String> = bounds.iter().map(|&at| sql[..at].to_owned()).collect();
    for window in bounds.windows(2) {
        out.push([&sql[..window[0]], &sql[window[1]..]].concat());
    }
    out
}

/// What the engine made of `mutated`, if it took it, read back and compared.
fn check(c: &Connection, known: &[String], mutated: &str) {
    for (name, sql) in stored(c, "table")
        .into_iter()
        .filter(|(n, _)| !known.contains(n))
    {
        if let Ok(text) = table(&name, &sql) {
            agree(&text, &facts(c, &name))
                .unwrap_or_else(|e| panic!("{mutated}\n  stored: {sql}\n  {e}"));
        }
    }
}

fn run(cases: Vec<Case>) -> (usize, usize) {
    let (mut fed, mut taken) = (0, 0);
    for case in corpus::accepted(cases) {
        let mut fresh = Fresh::memory();
        let c = fresh.connection();
        c.execute_batch(case.setup).expect("setup");
        let known: Vec<String> = stored(c, "table").into_iter().map(|(n, _)| n).collect();
        for mutated in mutations(&case.sql) {
            // 1. Never a panic, whatever the text.
            let _ = table("o", &mutated);
            fed += 1;
            // 2. Where the engine takes it, the stored text reads in agreement.
            c.execute_batch("SAVEPOINT m").expect("savepoint");
            if c.execute_batch(&mutated).is_ok() {
                taken += 1;
                check(c, &known, &mutated);
            }
            c.execute_batch("ROLLBACK TO m; RELEASE m")
                .expect("rollback");
        }
    }
    (fed, taken)
}

#[test]
fn every_prefix_and_every_deletion_of_a_table_text_is_answered_and_never_contradicted() {
    let (fed, taken) = run(corpus::tables());
    // The corpus is large, and the engine takes the text of many of its
    // mutations (a deleted character of a comment, of a default, of a name).
    assert!(fed > 20_000, "{fed}");
    assert!(taken > 500, "{taken}");
}

#[test]
fn a_deletion_that_is_not_text_is_not_made() {
    // Whole characters only: every mutation is a string.
    for mutated in mutations("CREATE TABLE \"é\"(ß int)") {
        assert!(mutated.chars().count() <= 23);
    }
    assert_eq!(mutations("ab").len(), 3 + 2);
}
