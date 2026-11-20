//! The forms of `ALTER TABLE` leave the text in shapes no hand writes. Random
//! sequences of them, in plain SQL, are applied to tables with every kind of
//! constraint, and after every step the text the engine stored must tokenize
//! and agree with the pragmas; at the end of each sequence the names the
//! tokenizer claims are probed with the engine's own `DROP CONSTRAINT`.

use super::super::{Kind, table};
use super::corpus::PARENTS;
use super::facts::{agree, facts, stored};
use crate::tests::Fresh;

/// A small deterministic generator, so a failing sequence can be replayed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

pub(in crate::catalog) const BASES: &[&str] = &[
    "CREATE TABLE t(id integer NOT NULL, owner int NOT NULL, name text DEFAULT 'x', qty int, PRIMARY KEY (id), CONSTRAINT u_name UNIQUE (owner, name), CONSTRAINT c_qty CHECK (qty >= 0), CONSTRAINT f_owner FOREIGN KEY (owner) REFERENCES p(id) ON DELETE CASCADE)",
    "CREATE TABLE t(a int CONSTRAINT nn NOT NULL CONSTRAINT ck CHECK (a > 0), b text DEFAULT 'q' REFERENCES p(id), c int, d int UNIQUE, e int CONSTRAINT dd DEFAULT 5)",
    "CREATE TABLE t(a /* one */, b -- two\n, c)",
    "CREATE TABLE \"t\" (\n    \"a\" integer NOT NULL,\n    \"b\" text,\n    PRIMARY KEY (\"a\"),\n    CONSTRAINT \"c1\" CHECK (\"b\" <> '')\n)",
];

/// A name as the next statement spells it: quoted or bare where bare is legal.
fn spelled(rng: &mut Rng, name: &str) -> String {
    let bare = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if bare && rng.below(2) == 0 {
        name.to_owned()
    } else {
        ["\"", &name.replace('"', "\"\""), "\""].concat()
    }
}

fn one_table(fresh: &mut Fresh) -> (String, String) {
    stored(fresh.connection(), "table")
        .into_iter()
        .find(|(n, _)| !matches!(n.as_str(), "p" | "q"))
        .expect("the table")
}

/// One sequence: returns the statements that took effect.
fn sequence(seed: u64, base: &str, steps: usize) -> Vec<String> {
    sequence_with(seed, base, steps, &mut |_, _| {})
}

/// The same, calling `after` with the database and the table's name after
/// every step that took effect (the reader's own closure test uses it).
pub(in crate::catalog) fn sequence_with(
    seed: u64,
    base: &str,
    steps: usize,
    after: &mut dyn FnMut(&mut Fresh, &str),
) -> Vec<String> {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut fresh = Fresh::memory();
    fresh.connection().execute_batch(PARENTS).expect("parents");
    fresh.connection().execute_batch(base).expect("base");
    let mut history = vec![base.to_owned()];
    let (mut name, _) = one_table(&mut fresh);
    let mut counter = 0;
    for step in 0..steps {
        counter += 1;
        let (_, sql) = one_table(&mut fresh);
        let text =
            table(&name, &sql).unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}\n  {sql}"));
        let at = spelled(&mut rng, &name);
        let statements: Vec<String> = match rng.below(9) {
            0 => {
                let ty = ["integer", "text", "varchar(10)", "numeric", ""][rng.below(5)];
                let extra =
                    ["", " NOT NULL DEFAULT 0", " DEFAULT 'd'", " DEFAULT 1.5"][rng.below(4)];
                vec![format!(
                    "ALTER TABLE main.{at} ADD COLUMN \"n{counter}\" {ty}{extra}"
                )]
            }
            1 => {
                let column = &text.columns[rng.below(text.columns.len())];
                let to = spelled(&mut rng, &format!("r{counter}"));
                vec![format!(
                    "ALTER TABLE main.{at} RENAME COLUMN {} TO {to}",
                    spelled(&mut rng, column)
                )]
            }
            2 => {
                let to = format!("t{counter}");
                let target = spelled(&mut rng, &to);
                let stmt = format!("ALTER TABLE main.{at} RENAME TO {target}");
                vec![stmt]
            }
            3 => {
                let column = &text.columns[rng.below(text.columns.len())];
                vec![format!(
                    "ALTER TABLE main.{at} DROP COLUMN {}",
                    spelled(&mut rng, column)
                )]
            }
            4 => {
                let column = &text.columns[rng.below(text.columns.len())];
                let use_column = spelled(&mut rng, column);
                vec![format!(
                    "ALTER TABLE main.{at} ADD CONSTRAINT {} CHECK ({use_column} IS NOT NULL OR {use_column} IS NULL)",
                    spelled(&mut rng, &format!("k{counter}"))
                )]
            }
            5 => {
                let named: Vec<&str> = text
                    .constraints
                    .iter()
                    .filter_map(|c| c.name.as_deref())
                    .collect();
                if named.is_empty() {
                    continue;
                }
                let pick = rng.below(named.len());
                vec![format!(
                    "ALTER TABLE main.{at} DROP CONSTRAINT {}",
                    spelled(&mut rng, named[pick])
                )]
            }
            6 => {
                let column = &text.columns[rng.below(text.columns.len())];
                vec![format!(
                    "ALTER TABLE main.{at} ALTER COLUMN {} SET NOT NULL",
                    spelled(&mut rng, column)
                )]
            }
            7 => {
                let column = &text.columns[rng.below(text.columns.len())];
                vec![format!(
                    "ALTER TABLE main.{at} ALTER COLUMN {} DROP NOT NULL",
                    spelled(&mut rng, column)
                )]
            }
            _ => {
                // A table renamed to its own name in another case, in two steps.
                let flipped: String = name
                    .chars()
                    .map(|c| {
                        if c.is_ascii_lowercase() {
                            c.to_ascii_uppercase()
                        } else {
                            c.to_ascii_lowercase()
                        }
                    })
                    .collect();
                let via = format!("__pbps_rename_x{counter}");
                let stmts = vec![
                    format!("ALTER TABLE main.{at} RENAME TO \"{via}\""),
                    format!("ALTER TABLE main.\"{via}\" RENAME TO \"{flipped}\""),
                ];
                stmts
            }
        };
        let mut took = true;
        fresh
            .connection()
            .execute_batch("SAVEPOINT s")
            .expect("savepoint");
        for statement in &statements {
            if fresh.connection().execute_batch(statement).is_err() {
                took = false;
                break;
            }
        }
        if !took {
            fresh
                .connection()
                .execute_batch("ROLLBACK TO s; RELEASE s")
                .expect("rollback");
            // A rename that was not made leaves the name where it was.
            name = one_table(&mut fresh).0;
            continue;
        }
        fresh
            .connection()
            .execute_batch("RELEASE s")
            .expect("release");
        history.extend(statements);
        let (now, sql) = one_table(&mut fresh);
        name = now;
        let read = table(&name, &sql).unwrap_or_else(|e| {
            panic!("seed {seed} step {step}: {e}\n  {sql}\n  after {history:#?}")
        });
        agree(&read, &facts(fresh.connection(), &name)).unwrap_or_else(|e| {
            panic!("seed {seed} step {step}: {e}\n  {sql}\n  after {history:#?}")
        });
        after(&mut fresh, &name);
    }
    // The names claimed at the end are the names the engine finds.
    let (name, sql) = one_table(&mut fresh);
    let end = table(&name, &sql).expect("reads");
    for c in &end.constraints {
        let Some(claimed) = &c.name else { continue };
        let mut copy = Fresh::memory();
        copy.connection().execute_batch(PARENTS).expect("parents");
        for statement in &history {
            copy.connection().execute_batch(statement).expect("replay");
        }
        let outcome = copy.connection().execute_batch(
            &[
                "ALTER TABLE main.\"",
                &name.replace('"', "\"\""),
                "\" DROP CONSTRAINT \"",
                &claimed.replace('"', "\"\""),
                "\"",
            ]
            .concat(),
        );
        match (&c.kind, outcome) {
            (Kind::PrimaryKey { .. } | Kind::Unique { .. } | Kind::Null, Err(e)) => {
                assert!(
                    e.to_string().contains("may not be dropped"),
                    "seed {seed}: {claimed}: {e}"
                );
            }
            (_, Ok(())) => {}
            (Kind::Default { .. }, Err(e)) if e.to_string().contains("no such constraint") => {}
            (_, Err(e)) => panic!(
                "seed {seed}: the tokenizer claims `{claimed}`, the engine says: {e}\n  {sql}"
            ),
        }
    }
    history
}

#[test]
fn after_every_alter_the_stored_text_tokenizes_and_agrees_with_the_pragmas() {
    let mut steps = 0;
    for (b, base) in BASES.iter().enumerate() {
        for seed in 0..60u64 {
            steps += sequence(seed * 7 + b as u64, base, 14).len() - 1;
        }
    }
    assert!(steps > 1_000, "{steps}");
}

#[test]
fn the_sequence_generator_takes_every_form_and_is_deterministic() {
    let a = sequence(3, BASES[0], 14);
    let b = sequence(3, BASES[0], 14);
    assert_eq!(a, b);
    let all: Vec<String> = (0..60u64).flat_map(|s| sequence(s, BASES[0], 14)).collect();
    for form in [
        "ADD COLUMN",
        "RENAME COLUMN",
        "RENAME TO",
        "DROP COLUMN",
        "ADD CONSTRAINT",
        "DROP CONSTRAINT",
        "SET NOT NULL",
        "DROP NOT NULL",
        "__pbps_rename_",
    ] {
        assert!(
            all.iter().any(|s| s.contains(form)),
            "no sequence took {form}"
        );
    }
}
