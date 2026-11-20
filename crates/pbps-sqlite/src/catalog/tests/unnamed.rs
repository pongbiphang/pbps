//! A constraint the file leaves unnamed is read under a stable name made of
//! what it is: `__pbps_unnamed_<kind>_<digest>`, with `_2`, `_3` for identical
//! ones. The model keys a check, a unique constraint and a foreign key by name,
//! and SQLite has none to give.

use std::collections::BTreeSet;

use pbps_db::catalog::Pulled;

use super::{limits, read_sql, table, warned};
use crate::names::UNNAMED_PREFIX;
use crate::tests::Fresh;

const P: &str = "CREATE TABLE p(id integer PRIMARY KEY, k integer UNIQUE); ";

/// The names the reader gave table `t`'s unnamed constraints, by kind.
fn synthetic(pulled: &Pulled) -> (Vec<String>, Vec<String>, Vec<String>) {
    let t = table(pulled, "t");
    let only = |keys: Vec<&pbps_model::ObjectLocalName>| {
        keys.into_iter()
            .map(|k| k.raw().to_owned())
            .filter(|k| k.starts_with(UNNAMED_PREFIX))
            .collect::<Vec<_>>()
    };
    (
        only(t.checks.keys().collect()),
        only(t.unique.keys().collect()),
        only(t.foreign_keys.keys().collect()),
    )
}

fn names_of(sql: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    synthetic(&read_sql(&[P, sql].concat()).1)
}

/// Whether `name` is `__pbps_unnamed_<kind>_<16 hex digits>` and then `suffix`.
fn shaped(name: &str, kind: &str, suffix: &str) -> bool {
    let Some(rest) = name.strip_prefix(&format!("{UNNAMED_PREFIX}{kind}_")) else {
        return false;
    };
    let (digest, tail) = rest.split_at(rest.len().min(16));
    digest.len() == 16
        && digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        && tail == suffix
}

#[test]
fn an_unnamed_check_unique_and_foreign_key_are_read_under_a_stable_name_and_one_warning() {
    let (_, pulled) = read_sql(&format!(
        "{P}CREATE TABLE t(a integer CHECK (a > 0) UNIQUE REFERENCES p(id), b integer, \
         UNIQUE (a, b), CHECK (b IS NOT NULL), FOREIGN KEY (b) REFERENCES p(k))"
    ));
    let (checks, uniques, foreign) = synthetic(&pulled);
    assert_eq!((checks.len(), uniques.len(), foreign.len()), (2, 2, 2));
    assert!(checks.iter().all(|n| shaped(n, "check", "")), "{checks:?}");
    assert!(
        uniques.iter().all(|n| shaped(n, "unique", "")),
        "{uniques:?}"
    );
    assert!(
        foreign.iter().all(|n| shaped(n, "foreign", "")),
        "{foreign:?}"
    );
    let all: BTreeSet<_> = checks.iter().chain(&uniques).chain(&foreign).collect();
    assert_eq!(all.len(), 6, "six constraints, six names");
    // One warning for the table, naming them all. Nothing is left out, so there
    // is no limitation.
    let said: Vec<_> = pulled
        .warnings
        .iter()
        .filter(|w| w.contains("table `t` has"))
        .collect();
    assert_eq!(said.len(), 1, "{:#?}", pulled.warnings);
    assert!(
        said[0].contains("table `t` has 6 constraint(s)"),
        "{}",
        said[0]
    );
    for name in &all {
        assert!(said[0].contains(name.as_str()), "{}", said[0]);
    }
    assert!(limits(&pulled, "t").is_empty());
    // Negative: a named constraint and a primary key never get one.
    let (_, named) = read_sql(&format!(
        "{P}CREATE TABLE t(a integer, CONSTRAINT c CHECK (a > 0), CONSTRAINT u UNIQUE (a), PRIMARY KEY (a))"
    ));
    assert_eq!(synthetic(&named), (vec![], vec![], vec![]));
    assert!(!warned(&named, "table `t` has"));
}

#[test]
fn a_name_is_a_function_of_what_the_constraint_is_and_not_of_where_it_stands() {
    let base = "CREATE TABLE t(a integer, b integer, CHECK (a > 0), UNIQUE (a, b), FOREIGN KEY (b) REFERENCES p(id) ON DELETE CASCADE)";
    let (checks, uniques, foreign) = names_of(base);
    assert_eq!((checks.len(), uniques.len(), foreign.len()), (1, 1, 1));
    let same = |sql: &str, what: &str| {
        let (c, u, f) = names_of(sql);
        assert!(
            c.contains(&checks[0]) && u.contains(&uniques[0]) && f.contains(&foreign[0]),
            "{what}: {c:?} {u:?} {f:?}"
        );
        (c, u, f)
    };
    // Another constraint added, before and after, and one removed.
    same(
        "CREATE TABLE t(a integer, b integer, CHECK (b < 9), CHECK (a > 0), UNIQUE (b), UNIQUE (a, b), FOREIGN KEY (a) REFERENCES p(k), FOREIGN KEY (b) REFERENCES p(id) ON DELETE CASCADE)",
        "others added",
    );
    same(
        "CREATE TABLE t(a integer, b integer, FOREIGN KEY (b) REFERENCES p(id) ON DELETE CASCADE, UNIQUE (a, b), CHECK (a > 0))",
        "reordered",
    );
    let (c, ..) = names_of("CREATE TABLE t(a integer, b integer, CHECK (a > 0))");
    assert_eq!(c, checks, "the others removed");
    // Spelled another way: case and layout and a comment in the expression, the
    // case of a column in a clause, `NO ACTION` written out.
    same(
        "CREATE TABLE t(a integer, b integer, CHECK (  A>0 /* why */ ), UNIQUE (A, B), FOREIGN KEY (B) REFERENCES P(ID) ON DELETE CASCADE ON UPDATE NO ACTION)",
        "spelled another way",
    );
    // A column constraint is the same constraint as a table's of the same text.
    let (_, column_form, _) = names_of("CREATE TABLE t(a integer UNIQUE)");
    let (_, table_form, _) = names_of("CREATE TABLE t(a integer, UNIQUE (a))");
    assert_eq!(column_form, table_form);
    // And a different constraint has a different name: each part of what a
    // constraint is changes its name.
    for (sql, kind) in [
        ("CREATE TABLE t(a integer, b integer, CHECK (a > 1))", 0),
        (
            "CREATE TABLE t(a integer, b integer, CHECK (a > 0 AND b > 0))",
            0,
        ),
        ("CREATE TABLE t(a integer, b integer, CHECK (a = 'A'))", 0),
        ("CREATE TABLE t(a integer, b integer, UNIQUE (b, a))", 1),
        ("CREATE TABLE t(a integer, b integer, UNIQUE (a))", 1),
        (
            "CREATE TABLE t(a integer, b integer, FOREIGN KEY (b) REFERENCES p(id) ON DELETE SET NULL)",
            2,
        ),
        (
            "CREATE TABLE t(a integer, b integer, FOREIGN KEY (b) REFERENCES p(id) ON UPDATE CASCADE)",
            2,
        ),
        (
            "CREATE TABLE t(a integer, b integer, FOREIGN KEY (a) REFERENCES p(id) ON DELETE CASCADE)",
            2,
        ),
        (
            "CREATE TABLE t(a integer, b integer, FOREIGN KEY (b) REFERENCES p(k) ON DELETE CASCADE)",
            2,
        ),
    ] {
        let (c, u, f) = names_of(sql);
        let (now, then) = match kind {
            0 => (&c, &checks),
            1 => (&u, &uniques),
            _ => (&f, &foreign),
        };
        assert_eq!(now.len(), 1, "{sql}");
        assert_ne!(now, then, "{sql} is another constraint");
    }
}

#[test]
fn the_digest_is_pinned_because_a_declaration_will_carry_the_name() {
    // The 64-bit FNV-1a, in 16 hex digits, of `<kind> NUL <content>`, worked out
    // here without the reader: a check's content is its canonical expression
    // (`a>0`), a unique constraint's its columns (`a`), a foreign key's its
    // columns, parent table, parent columns and actions, each joined by U+0002
    // (`a`, `p`, `id`, `no action/no action`).
    fn fnv(text: &str) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in text.bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
        format!("{hash:016x}")
    }
    assert_eq!(fnv("check\0a>0"), "0ff7a70dbf4f12d2");
    assert_eq!(fnv("unique\0a"), "4380e5d4517d1a89");
    assert_eq!(
        fnv("foreign\0a\u{2}p\u{2}id\u{2}no action/no action"),
        "781a5b7d6dc5e1b0"
    );
    let (checks, uniques, foreign) = names_of(
        "CREATE TABLE t(a integer, CHECK (a > 0), UNIQUE (a), FOREIGN KEY (a) REFERENCES p(id))",
    );
    assert_eq!(checks, ["__pbps_unnamed_check_0ff7a70dbf4f12d2"]);
    assert_eq!(uniques, ["__pbps_unnamed_unique_4380e5d4517d1a89"]);
    assert_eq!(foreign, ["__pbps_unnamed_foreign_781a5b7d6dc5e1b0"]);
}

#[test]
fn identical_unnamed_constraints_each_get_a_name_in_the_order_written() {
    // The engine's side, measured: a repeated CHECK is stored twice and
    // enforced; a repeated UNIQUE shares one index; a repeated foreign key is
    // two rows of the pragma.
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch(&format!(
        "{P}CREATE TABLE t(a integer, CHECK (a > 0), CHECK (a > 0), UNIQUE (a), UNIQUE (a), \
         FOREIGN KEY (a) REFERENCES p(id), FOREIGN KEY (a) REFERENCES p(id))"
    ))
    .expect("table");
    let count = |sql: &str| {
        c.query(sql, &[]).expect("query")[0]
            .integer(0)
            .expect("count")
    };
    assert_eq!(
        count("SELECT count(*) FROM sqlite_schema WHERE sql LIKE '%CHECK (a > 0), CHECK (a > 0)%'"),
        1
    );
    assert_eq!(
        count("SELECT count(*) FROM pragma_index_list('t') WHERE origin = 'u'"),
        1
    );
    assert_eq!(
        count("SELECT count(DISTINCT id) FROM pragma_foreign_key_list('t')"),
        2
    );
    c.execute_batch("PRAGMA foreign_keys = OFF")
        .expect("pragma");
    assert!(
        c.execute_batch("INSERT INTO t VALUES (-1)").is_err(),
        "both checks are enforced"
    );
    let pulled = super::read(&mut fresh);
    let (checks, uniques, foreign) = synthetic(&pulled);
    for (names, kind) in [
        (&checks, "check"),
        (&uniques, "unique"),
        (&foreign, "foreign"),
    ] {
        assert_eq!(names.len(), 2, "{kind}: {names:?}");
        assert!(
            shaped(&names[0], kind, "") || shaped(&names[1], kind, ""),
            "{names:?}"
        );
        let second = names
            .iter()
            .find(|n| n.ends_with("_2"))
            .unwrap_or_else(|| panic!("{names:?}"));
        assert!(shaped(second, kind, "_2"), "{second}");
        let first = names.iter().find(|n| !n.ends_with("_2")).expect("a first");
        assert_eq!(
            second.strip_suffix("_2"),
            Some(first.as_str()),
            "the second is the first's name and `_2`"
        );
    }
    // Three, and then one again: the first keeps its name throughout, and the
    // others are numbered in the order written.
    let one = names_of("CREATE TABLE t(a integer, CHECK (a > 0))").0;
    let three =
        names_of("CREATE TABLE t(a integer, CHECK (a > 0), CHECK (a > 0), CHECK (a > 0))").0;
    assert_eq!(three.len(), 3);
    assert!(three.contains(&one[0]));
    assert!(three.contains(&format!("{}_2", one[0])) && three.contains(&format!("{}_3", one[0])));
    // Negative: constraints that differ are not numbered.
    let (c, ..) = names_of("CREATE TABLE t(a integer, CHECK (a > 0), CHECK (a > 1))");
    assert!(c.iter().all(|n| shaped(n, "check", "")), "{c:?}");
}

#[test]
fn a_file_constraint_named_with_the_reserved_prefix_leaves_its_table_unread() {
    let sql =
        "CREATE TABLE t(a integer, CONSTRAINT __pbps_unnamed_check_0123456789abcdef CHECK (a > 0))";
    assert!(crate::tests::accepts(sql), "the engine takes the name");
    let (_, pulled) = read_sql(&format!("CREATE TABLE ok(a); {sql}"));
    assert!(pulled.schema.tables.keys().all(|n| n.name == "ok"));
    let found = limits(&pulled, "t");
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("keeps for the names it gives"),
        "{found:?}"
    );
}

#[test]
fn a_quoted_run_keeps_its_case_in_a_check_s_name_because_it_may_be_a_string() {
    // A double-quoted run that names no column is a string to the engine, so
    // these two checks say different things and are two names.
    assert!(crate::tests::accepts(
        "CREATE TABLE t(x, CHECK (\"A\" = 'A')); INSERT INTO t VALUES (1)"
    ));
    assert!(!crate::tests::accepts(
        "CREATE TABLE t(x, CHECK (\"a\" = 'A')); INSERT INTO t VALUES (1)"
    ));
    let upper = names_of("CREATE TABLE t(x, CHECK (\"A\" = 'A'))").0;
    let lower = names_of("CREATE TABLE t(x, CHECK (\"a\" = 'A'))").0;
    assert_ne!(upper, lower);
    // Nothing inside a quoted name is layout: `"a  b"` and `"a b"` are two
    // columns, so two checks.
    assert!(crate::tests::accepts("CREATE TABLE t(\"a  b\", \"a b\")"));
    let wide = names_of("CREATE TABLE t(\"a  b\", \"a b\", CHECK (\"a  b\" = 1))").0;
    let narrow = names_of("CREATE TABLE t(\"a  b\", \"a b\", CHECK (\"a b\" = 1))").0;
    assert_ne!(wide, narrow);
    // Negative: the layout around a quoted name is not part of it.
    assert_eq!(
        names_of("CREATE TABLE t(\"a  b\", CHECK (\"a  b\"   =\n1))").0,
        names_of("CREATE TABLE t(\"a  b\", CHECK (\"a  b\" = 1))").0
    );
    // Negative: the case of code outside quotes is not part of it.
    assert_eq!(
        names_of("CREATE TABLE t(x, CHECK (X = 'A' AND Length(x) > 0))").0,
        names_of("CREATE TABLE t(x, CHECK (x = 'A' and length(X) > 0))").0
    );
}

/// A constraint that is left out is not read under any name, so the warning
/// that lists the names given does not list one for it.
#[test]
fn a_constraint_that_is_left_out_is_given_no_name_in_the_warning() {
    let (_, pulled) =
        read_sql("CREATE TABLE t(a int, b int, UNIQUE (a) ON CONFLICT REPLACE, CHECK (b > 0))");
    let t = table(&pulled, "t");
    assert!(t.unique.is_empty());
    assert_eq!(t.checks.len(), 1);
    assert!(
        warned(&pulled, "has 1 constraint(s) with no name"),
        "{:#?}",
        pulled.warnings
    );
    assert!(
        !warned(&pulled, "__pbps_unnamed_unique_"),
        "{:#?}",
        pulled.warnings
    );
    // The same for a foreign key that cannot be read (its parent is not there).
    let (_, pulled) = read_sql("CREATE TABLE t(a int, FOREIGN KEY (a) REFERENCES missing (x))");
    assert!(table(&pulled, "t").foreign_keys.is_empty());
    assert!(!warned(&pulled, "with no name"), "{:#?}", pulled.warnings);
    // And when every unnamed one is left out, there is no such warning.
    let (_, pulled) = read_sql("CREATE TABLE t(a int, UNIQUE (a) ON CONFLICT REPLACE)");
    assert!(!warned(&pulled, "with no name"), "{:#?}", pulled.warnings);
}
