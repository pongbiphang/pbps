//! The oracles: the engine is asked, and the tokenizer must say the same.
//!
//! - **O1**, pragmas: everything both can say agrees (`facts::agree`), in both
//!   directions, and a table rebuilt from the tokenizer's pieces is the table
//!   the engine made from the original text.
//! - **O2**, `DROP CONSTRAINT`: SQLite 3.53 finds a constraint by name in the
//!   stored text. Every name the tokenizer claims is a name the engine finds,
//!   every name it does not claim is one the engine does not, and the text
//!   after each drop tokenizes to the original minus that constraint.
//! - **O3**, rows: every `CHECK` the engine fires on a row is one the
//!   tokenizer found, with the same name or text.

use pbps_db::sqlite::{Connection, Value};

use super::super::{Kind, Marker, Place, TableText, table};
use super::corpus::{self, Case};
use super::facts::{agree, facts, rebuilt, stored};
use crate::ident;
use crate::tests::Fresh;

/// Runs a case on a fresh engine and returns it, with the stored text and
/// name of its one table `t`.
fn run(case: &Case) -> (Fresh, String) {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch(case.setup).expect("the setup");
    c.execute_batch(&case.sql)
        .unwrap_or_else(|e| panic!("{}: {e}", case.sql));
    let sql = stored(c, "table")
        .into_iter()
        .find(|(name, _)| name == "t")
        .unwrap_or_else(|| panic!("no table t after {}", case.sql))
        .1;
    (fresh, sql)
}

fn read(case: &Case, sql: &str) -> TableText {
    table("t", sql).unwrap_or_else(|e| panic!("{}\n  stored: {sql}\n  {e}", case.sql))
}

#[test]
fn the_corpus_is_what_the_engine_takes_and_is_not_empty() {
    let all = corpus::tables();
    let taken = corpus::accepted(all.clone());
    assert!(taken.len() > 150, "{} of {}", taken.len(), all.len());
    // The engine refuses a second primary key; the corpus keeps it out.
    assert!(!corpus::takes(&Case {
        setup: corpus::PARENTS,
        sql: "CREATE TABLE t(a int primary key, b int primary key)".into(),
    }));
}

#[test]
fn o1_every_table_of_the_corpus_reads_and_agrees_with_the_pragmas() {
    for case in corpus::accepted(corpus::tables()) {
        let (mut fresh, sql) = run(&case);
        let text = read(&case, &sql);
        let found = facts(fresh.connection(), "t");
        agree(&text, &found).unwrap_or_else(|e| panic!("{}\n  stored: {sql}\n  {e}", case.sql));
    }
}

#[test]
fn o1_a_table_rebuilt_from_the_pieces_is_the_table_the_engine_made() {
    let mut rebuilt_count = 0;
    for case in corpus::accepted(corpus::tables()) {
        let (mut fresh, sql) = run(&case);
        let text = read(&case, &sql);
        // A marker is something the rebuild would drop on purpose.
        if !text.markers.is_empty() {
            continue;
        }
        let found = facts(fresh.connection(), "t");
        let again = rebuilt(&text, &found);
        let mut other = Fresh::memory();
        other
            .connection()
            .execute_batch(case.setup)
            .expect("the setup");
        other
            .connection()
            .execute_batch(&again)
            .unwrap_or_else(|e| panic!("{}\n  rebuilt: {again}\n  {e}", case.sql));
        let mut theirs = facts(other.connection(), "t");
        let mut ours = found.clone();
        // Index names are the engine's own numbering.
        for facts in [&mut theirs, &mut ours] {
            for index in &mut facts.indexes {
                index.name.clear();
            }
            facts
                .indexes
                .sort_by(|a, b| format!("{:?}", a.keys).cmp(&format!("{:?}", b.keys)));
        }
        // A duplicate unique constraint is one index however it is written.
        assert_eq!(ours, theirs, "{}\n  rebuilt: {again}", case.sql);
        rebuilt_count += 1;
    }
    assert!(rebuilt_count > 100, "{rebuilt_count}");
}

#[test]
fn o1_a_disagreement_is_seen_in_each_direction() {
    let case = Case { setup: corpus::PARENTS, sql: "CREATE TABLE t(a int NOT NULL DEFAULT (5), b int, PRIMARY KEY (a, b), UNIQUE (b), FOREIGN KEY (a) REFERENCES p(id))".into() };
    let (mut fresh, sql) = run(&case);
    let found = facts(fresh.connection(), "t");
    let good = read(&case, &sql);
    agree(&good, &found).expect("agrees");
    let mut bad = good.clone();
    bad.columns.swap(0, 1);
    assert!(agree(&bad, &found).unwrap_err().contains("columns"));
    let mut bad = good.clone();
    bad.constraints.retain(|c| !matches!(c.kind, Kind::NotNull));
    assert!(agree(&bad, &found).unwrap_err().contains("NOT NULL"));
    let mut bad = good.clone();
    bad.constraints
        .retain(|c| !matches!(c.kind, Kind::Unique { .. }));
    assert!(agree(&bad, &found).unwrap_err().contains("unique index"));
    let mut bad = good.clone();
    bad.constraints
        .retain(|c| !matches!(c.kind, Kind::ForeignKey { .. }));
    assert!(agree(&bad, &found).unwrap_err().contains("foreign keys"));
    let mut bad = good.clone();
    bad.constraints
        .retain(|c| !matches!(c.kind, Kind::PrimaryKey { .. }));
    assert!(agree(&bad, &found).unwrap_err().contains("primary key"));
    let mut bad = good.clone();
    for c in &mut bad.constraints {
        if let Kind::Default { text } = &mut c.kind {
            *text = "6".into();
        }
    }
    assert!(agree(&bad, &found).unwrap_err().contains("default"));
    let mut bad = good;
    bad.constraints.push(super::super::Constraint {
        name: None,
        kind: Kind::Unique {
            columns: vec!["a".into()],
        },
        place: Place::Table,
    });
    assert!(agree(&bad, &found).unwrap_err().contains("no index"));
}

#[test]
fn the_eight_words_that_are_not_in_a_type_are_where_the_engine_says() {
    // Whatever position the engine takes one in, the tokenizer reads the text
    // and finds the constraints after it; where the engine refuses it, the
    // tokenizer is never asked. Quoted, any of them is an ordinary word.
    let mut engine_takes = 0;
    for word in [
        "cross", "full", "indexed", "inner", "left", "natural", "outer", "right",
    ] {
        for sql in [
            format!("CREATE TABLE t(a {word} NOT NULL)"),
            format!("CREATE TABLE t(a int {word} NOT NULL)"),
            format!("CREATE TABLE t(a {word} int NOT NULL)"),
            format!("CREATE TABLE t(a unsigned {word} int NOT NULL)"),
            format!("CREATE TABLE t(a \"{word}\" NOT NULL)"),
            format!("CREATE TABLE t(a int \"{word}\" NOT NULL)"),
            format!("CREATE TABLE t({word} int NOT NULL)"),
        ] {
            let case = Case {
                setup: corpus::PARENTS,
                sql,
            };
            let taken = corpus::takes(&case);
            if taken {
                engine_takes += 1;
                let (mut fresh, stored_sql) = run(&case);
                let text = read(&case, &stored_sql);
                agree(&text, &facts(fresh.connection(), "t"))
                    .unwrap_or_else(|e| panic!("{}: {e}", case.sql));
                assert_eq!(text.constraints.len(), 1, "{}", case.sql);
            } else {
                // The tokenizer refuses the same texts by name or reads them;
                // it never panics and never invents a constraint.
                if let Ok(text) = table("t", &case.sql) {
                    assert!(text.constraints.len() <= 1, "{}", case.sql);
                }
            }
        }
    }
    assert!(engine_takes > 0);
}

/// The names the tokenizer claims, ASCII case folded, with the kind of each.
fn claimed(text: &TableText) -> Vec<(String, &'static str)> {
    text.constraints
        .iter()
        .filter_map(|c| {
            let kind = match &c.kind {
                Kind::PrimaryKey { .. } => "primary",
                Kind::Unique { .. } => "unique",
                Kind::Check { .. } => "check",
                Kind::ForeignKey { .. } => "foreign",
                Kind::NotNull => "not null",
                Kind::Null => "null",
                Kind::Default { .. } => "default",
            };
            c.name.as_ref().map(|n| (n.to_ascii_lowercase(), kind))
        })
        .collect()
}

fn drop_constraint(c: &Connection, name: &str) -> Result<(), String> {
    let sql = [
        "ALTER TABLE main.t DROP CONSTRAINT \"",
        &name.replace('"', "\"\""),
        "\"",
    ]
    .concat();
    c.execute_batch(&sql).map_err(|e| e.to_string())
}

#[test]
fn o2_the_names_the_tokenizer_claims_are_the_names_the_engine_finds() {
    let mut probed = 0;
    let mut dropped = 0;
    for case in corpus::accepted(corpus::tables()) {
        let (_fresh, sql) = run(&case);
        let text = read(&case, &sql);
        let names = claimed(&text);
        // Distinct names only: with a repeated name the engine takes the first.
        let mut seen = std::collections::BTreeSet::new();
        if !names.iter().all(|(n, _)| seen.insert(n.clone())) {
            continue;
        }
        for (name, kind) in &names {
            // A copy of the database for each drop.
            let mut copy = Fresh::memory();
            copy.connection().execute_batch(case.setup).expect("setup");
            copy.connection().execute_batch(&case.sql).expect("sql");
            let original = text
                .constraints
                .iter()
                .find(|c| c.name.as_ref().map(|n| n.to_ascii_lowercase()).as_deref() == Some(name))
                .expect("claimed");
            let outcome =
                drop_constraint(copy.connection(), original.name.as_deref().expect("named"));
            probed += 1;
            match (outcome, *kind) {
                (Ok(()), "primary" | "unique" | "null") => {
                    panic!("{}: the engine dropped {name}", case.sql)
                }
                (Err(e), "primary" | "unique" | "null") => {
                    assert!(
                        e.contains("may not be dropped"),
                        "{}: {name}: {e}",
                        case.sql
                    );
                }
                (Err(e), _) if e.contains("no such constraint") => {
                    // A name the tokenizer claims and the engine does not find.
                    // The one kind the engine does not take is a named default.
                    assert_eq!(*kind, "default", "{}: {name}: {e}", case.sql);
                }
                (Err(e), _) => panic!("{}: {name}: {e}", case.sql),
                (Ok(()), _) => {
                    dropped += 1;
                    let after = stored(copy.connection(), "table")
                        .into_iter()
                        .find(|(n, _)| n == "t")
                        .expect("table")
                        .1;
                    let left = read(&case, &after);
                    let mut expect = text.clone();
                    let at = expect
                        .constraints
                        .iter()
                        .position(|c| {
                            c.name.as_ref().map(|n| n.to_ascii_lowercase()).as_deref() == Some(name)
                        })
                        .expect("claimed");
                    // Dropping a named default (measured) removes the name
                    // and keeps the default; every other drop removes the
                    // constraint.
                    if *kind == "default" {
                        expect.constraints[at].name = None;
                    } else {
                        expect.constraints.remove(at);
                    }
                    // Compare what the drop leaves by kind, name and place.
                    let shape = |t: &TableText| -> Vec<(Option<String>, String)> {
                        t.constraints
                            .iter()
                            .map(|c| {
                                (
                                    c.name.clone(),
                                    format!("{:?}", std::mem::discriminant(&c.kind)),
                                )
                            })
                            .collect()
                    };
                    assert_eq!(
                        shape(&left),
                        shape(&expect),
                        "{}: after dropping {name}\n  {after}",
                        case.sql
                    );
                    agree(&left, &facts(copy.connection(), "t")).unwrap_or_else(|e| {
                        panic!("{}: after dropping {name}: {e}\n  {after}", case.sql)
                    });
                }
            }
        }
        // And the names it does not claim: every other word the engine is asked
        // for is `no such constraint`.
        let folded: Vec<String> = names.iter().map(|(n, _)| n.clone()).collect();
        let mut others: Vec<String> = text.columns.clone();
        others.extend(["zz".into(), "k_nope".into(), "t".into()]);
        for other in others {
            if folded.iter().any(|n| ident::same(n, &other)) {
                continue;
            }
            let mut copy = Fresh::memory();
            copy.connection().execute_batch(case.setup).expect("setup");
            copy.connection().execute_batch(&case.sql).expect("sql");
            let e = drop_constraint(copy.connection(), &other).expect_err("not a constraint");
            assert!(
                e.contains("no such constraint"),
                "{}: `{other}` is not claimed but the engine says: {e}",
                case.sql
            );
        }
    }
    assert!(
        probed > 40 && dropped > 20,
        "{probed} probes, {dropped} drops"
    );
}

/// Tables whose every named or unnamed check a row from the domain violates.
fn check_cases() -> Vec<Case> {
    [
        "CREATE TABLE t(a int, CHECK (a > 0))",
        "CREATE TABLE t(a int, CONSTRAINT k CHECK (a > 0), CONSTRAINT \"j j\" CHECK (a < 1))",
        "CREATE TABLE t(a int CHECK (a <> 1), b int CONSTRAINT bk CHECK (b <> 0))",
        "CREATE TABLE t(a int, b int, CHECK (a <> b AND (a IS NOT NULL OR b IS NOT NULL)))",
        "CREATE TABLE t(a text, CHECK (a <> ')' AND a <> 'x'))",
        "CREATE TABLE t(a int, CHECK (a /* /* */ > 0 AND a /* ) */ < 1000))",
        "CREATE TABLE \"t\" (\n    \"a\" integer,\n    CONSTRAINT \"c_a\" CHECK (\"a\" >= 0)\n, CONSTRAINT \"c_z\" CHECK (a <> 1))",
    ]
    .iter()
    .map(|sql| Case { setup: "", sql: (*sql).to_owned() })
    .collect()
}

#[test]
fn o3_every_check_the_engine_fires_is_one_the_tokenizer_found_and_each_one_it_found_fires() {
    let domain = [
        Value::Null,
        Value::Integer(-1),
        Value::Integer(0),
        Value::Integer(1),
        Value::Text("x".into()),
        Value::Text(")".into()),
    ];
    for case in check_cases() {
        let (mut fresh, sql) = run(&case);
        let text = read(&case, &sql);
        let c = fresh.connection();
        let columns = text.columns.len().min(2);
        let mut fired: std::collections::BTreeSet<String> = Default::default();
        let combos = domain.len().pow(columns as u32);
        for n in 0..combos {
            let mut values = Vec::new();
            let mut rest = n;
            for _ in 0..columns {
                values.push(domain[rest % domain.len()].clone());
                rest /= domain.len();
            }
            let names: Vec<String> = text
                .columns
                .iter()
                .take(columns)
                .map(|n| ["\"", n, "\""].concat())
                .collect();
            let marks: Vec<String> = (1..=columns).map(|i| format!("?{i}")).collect();
            let sql = [
                "INSERT INTO t(",
                &names.join(", "),
                ") VALUES (",
                &marks.join(", "),
                ")",
            ]
            .concat();
            c.execute_batch("SAVEPOINT r").expect("savepoint");
            if let Err(e) = c.execute(&sql, &values)
                && let Some(which) = e.to_string().split("CHECK constraint failed: ").nth(1)
            {
                fired.insert(which.trim().to_owned());
            }
            c.execute_batch("ROLLBACK TO r; RELEASE r")
                .expect("release");
        }
        let claims: Vec<String> = text
            .constraints
            .iter()
            .filter_map(|k| match &k.kind {
                Kind::Check { expression } => {
                    Some(k.name.clone().unwrap_or_else(|| expression.clone()))
                }
                Kind::PrimaryKey { .. }
                | Kind::Unique { .. }
                | Kind::ForeignKey { .. }
                | Kind::NotNull
                | Kind::Null
                | Kind::Default { .. } => None,
            })
            .collect();
        for which in &fired {
            assert!(
                claims.contains(which),
                "{}: the engine fired `{which}`, the tokenizer found {claims:?}",
                case.sql
            );
        }
        for claim in &claims {
            assert!(
                fired.contains(claim),
                "{}: the tokenizer found `{claim}`, which no row of the domain fired ({fired:?})",
                case.sql
            );
        }
    }
}

#[test]
fn markers_found_are_exactly_the_features_the_engine_was_given() {
    // No marker where the text has none, and the strict and rowid flags the
    // engine reports for the table agree with the markers.
    for case in corpus::accepted(corpus::tables()) {
        let (mut fresh, sql) = run(&case);
        let text = read(&case, &sql);
        let strict = fresh
            .connection()
            .query(
                "SELECT strict, wr FROM pragma_table_list WHERE schema='main' AND name='t'",
                &[],
            )
            .expect("table_list");
        let (engine_strict, engine_wr) = (
            strict[0].integer(0).expect("strict") == 1,
            strict[0].integer(1).expect("wr") == 1,
        );
        let has = |m: &Marker| text.markers.iter().any(|x| &x.marker == m);
        assert_eq!(has(&Marker::Strict), engine_strict, "{}", case.sql);
        assert_eq!(has(&Marker::WithoutRowid), engine_wr, "{}", case.sql);
    }
}
