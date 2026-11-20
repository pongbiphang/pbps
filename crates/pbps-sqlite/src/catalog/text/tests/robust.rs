//! The tokenizer is total: it answers `Ok` or `Err` for any text and never
//! panics. Every prefix and every single-character deletion of every corpus
//! text is fed in. Where the engine also takes the mutated text, what the
//! tokenizer reads from the text the engine stored must agree with the pragmas.
//! (A character, not a byte: deleting one byte of a multi-byte character would
//! not be text.)

use pbps_db::sqlite::Connection;

use super::super::{index, table, view};
use super::corpus::{self, Case};
use super::facts::{agree, agree_index, agree_view, facts, stored};
use crate::tests::Fresh;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Table,
    Index,
    View,
}

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

/// What the engine stored for objects that were not there before.
fn created(c: &Connection, kind: Kind, before: usize) -> Vec<(String, String)> {
    let found = stored(
        c,
        match kind {
            Kind::Table => "table",
            Kind::Index => "index",
            Kind::View => "view",
        },
    );
    found.into_iter().skip(before).collect()
}

fn feed(kind: Kind, name: &str, sql: &str) -> bool {
    match kind {
        Kind::Table => table(name, sql).is_ok(),
        Kind::Index => index(name, sql).is_ok(),
        Kind::View => view(name, sql).is_ok(),
    }
}

/// What the engine made of `mutated`, if it took it, read back and compared.
fn check(c: &Connection, kind: Kind, known: &[String], mutated: &str) {
    for (name, sql) in created(c, kind, 0)
        .into_iter()
        .filter(|(n, _)| !known.contains(n))
    {
        let fail = |e: String| -> ! { panic!("{mutated}\n  stored: {sql}\n  {e}") };
        match kind {
            Kind::Table => {
                if let Ok(text) = table(&name, &sql) {
                    // A quoted empty type name (`a ""`) is the one place the
                    // text and the pragma are known to differ: the pragma
                    // reports no type. The reader refuses it; the tests of
                    // the reader pin that.
                    agree(&text, &facts(c, &name)).unwrap_or_else(|e| {
                        if !e.ends_with("the engine reports the type ``") {
                            fail(e);
                        }
                    });
                }
            }
            Kind::Index => {
                if let Ok(text) = index(&name, &sql) {
                    let owner = text.table.clone();
                    let found = facts(c, &owner);
                    if let Some(row) = found.indexes.iter().find(|i| i.name == name) {
                        agree_index(&text, row, &owner).unwrap_or_else(|e| fail(e));
                    }
                }
            }
            Kind::View => {
                if let Ok(text) = view(&name, &sql) {
                    agree_view(&text, c).unwrap_or_else(|e| fail(e));
                }
            }
        }
    }
}

fn run(kind: Kind, cases: Vec<Case>) -> (usize, usize) {
    let (mut fed, mut taken) = (0, 0);
    for case in corpus::accepted(cases) {
        let mut shared = Fresh::memory();
        shared
            .connection()
            .execute_batch(case.setup)
            .expect("setup");
        let known: Vec<String> = created(shared.connection(), kind, 0)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        for mutated in mutations(&case.sql) {
            // 1. Never a panic, whatever the text.
            feed(kind, "o", &mutated);
            fed += 1;
            // 2. Where the engine takes it, the stored text reads in agreement.
            // A view the engine could not resolve leaves its schema cache
            // unreliable after a rollback, so a view gets an engine of its own.
            let mut own;
            let c = if kind == Kind::View {
                own = Fresh::memory();
                own.connection().execute_batch(case.setup).expect("setup");
                own.connection()
            } else {
                shared.connection()
            };
            c.execute_batch("SAVEPOINT m").expect("savepoint");
            if c.execute_batch(&mutated).is_ok() {
                taken += 1;
                check(c, kind, &known, &mutated);
            }
            if kind != Kind::View {
                c.execute_batch("ROLLBACK TO m; RELEASE m")
                    .expect("rollback");
            }
        }
    }
    (fed, taken)
}

#[test]
fn every_prefix_and_every_deletion_of_a_table_text_is_answered_and_never_contradicted() {
    let (fed, taken) = run(Kind::Table, corpus::tables());
    // The corpus is large, and the engine takes the text of many of its
    // mutations (a deleted character of a comment, of a default, of a name).
    assert!(fed > 20_000, "{fed}");
    assert!(taken > 500, "{taken}");
}

#[test]
fn every_prefix_and_every_deletion_of_an_index_text_is_answered_and_never_contradicted() {
    let (fed, taken) = run(Kind::Index, corpus::indexes());
    assert!(fed > 1_000, "{fed}");
    assert!(taken > 20, "{taken}");
}

#[test]
fn every_prefix_and_every_deletion_of_a_view_text_is_answered_and_never_contradicted() {
    let (fed, taken) = run(Kind::View, corpus::views());
    assert!(fed > 500, "{fed}");
    assert!(taken > 10, "{taken}");
}

#[test]
fn a_deletion_that_is_not_text_is_not_made() {
    // Whole characters only: every mutation is a string.
    for mutated in mutations("CREATE TABLE \"é\"(ß int)") {
        assert!(mutated.chars().count() <= 23);
    }
    assert_eq!(mutations("ab").len(), 3 + 2);
}
