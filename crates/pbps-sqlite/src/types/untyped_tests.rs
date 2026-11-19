//! The reserved word for a column with no declared type (#2950). The engine is
//! the oracle: a column with no type and a column declared `untyped` are
//! observed side by side, and every rule about the word rests on what they do.

use pbps_dialect::TypeChangeRisk;
use pbps_model::ColumnType;

use super::*;
use crate::tests::Fresh;

fn ty(text: &str) -> ColumnType {
    text.parse().expect("a parseable type")
}

/// What a column of `declared` (empty for no type) does with a fixed set of
/// values: the storage class and value of each after insertion.
fn behaviour(declared: &str) -> Vec<(String, String)> {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch(&["CREATE TABLE t(x ", declared, ")"].concat())
        .expect("a table");
    for value in [
        "'1'", "1", "1.5", "'x'", "x'ab'", "NULL", "'0x10'", "'1e2'", "'  12'", "'007'", "'1.0'",
        "'1e400'",
    ] {
        c.execute_batch(&["INSERT INTO t VALUES (", value, ")"].concat())
            .expect("an insert");
    }
    c.query("SELECT typeof(x), quote(x) FROM t ORDER BY rowid", &[])
        .expect("rows")
        .iter()
        .map(|r| {
            (
                r.text(0).expect("class").to_owned(),
                r.text(1).expect("value").to_owned(),
            )
        })
        .collect()
}

#[test]
fn a_column_declared_untyped_is_numeric_and_not_the_column_with_no_type() {
    // The fact the reserved word rests on: the word, written, would store
    // `'1'` as the integer 1 where a column with no type keeps the text.
    let typeless = behaviour("");
    let word = behaviour("untyped");
    assert_ne!(typeless, word);
    assert_eq!(word, behaviour("numeric"));
    assert_eq!(typeless, behaviour("blob"));
    assert_eq!(typeless[0], ("text".to_owned(), "'1'".to_owned()));
    assert_eq!(word[0], ("integer".to_owned(), "1".to_owned()));
    // The rules say the same: the word gets NUMERIC, the empty type BLOB.
    assert_eq!(affinity("untyped"), Affinity::Numeric);
    assert_eq!(affinity(""), Affinity::Blob);
}

#[test]
fn the_reserved_type_has_the_affinity_of_a_column_with_no_type() {
    assert_eq!(affinity_of(&ty("untyped")), Affinity::Blob);
    assert_eq!(
        affinity_of(&ColumnType::new("UnTyped", Vec::new())),
        Affinity::Blob
    );
    // The word as an ordinary name keeps the rules' own answer.
    assert_eq!(affinity_of(&ty("untyped(5)")), Affinity::Numeric);
    assert_eq!(affinity_of(&ty("untyped int")), Affinity::Integer);
    assert_eq!(affinity_of(&ty("untypedx")), Affinity::Numeric);
}

/// A type whose base is set directly, as a type built another way could be:
/// the model lower-cases a base when it parses or constructs one, but the field
/// is open.
fn loud(base: &str) -> ColumnType {
    let mut built = ty("untyped");
    built.base = base.to_owned();
    built
}

#[test]
fn the_word_is_the_word_in_any_case_even_when_the_model_did_not_fold_it() {
    for base in ["UnTyped", "UNTYPED"] {
        assert!(is_untyped(&loud(base)), "{base}");
        assert_eq!(affinity_of(&loud(base)), Affinity::Blob, "{base}");
        assert_eq!(standing(base), Standing::Untyped, "{base}");
        // One canonical spelling comes out, and it is the one the reader gives.
        assert_eq!(normalize(&loud(base)).expect(base), ty("untyped"), "{base}");
    }
    assert!(!is_untyped(&{
        let mut with_argument = loud("UnTyped");
        with_argument.args = ty("untyped(5)").args;
        with_argument
    }));
}

#[test]
fn only_the_bare_word_is_reserved() {
    for (text, reserved) in [
        ("untyped", true),
        ("UNTYPED", true),
        ("UnTyped", true),
        ("untyped(5)", false),
        ("untyped int", false),
        ("int untyped", false),
        ("untypedx", false),
        ("xuntyped", false),
        ("untyped_", false),
        ("blob", false),
    ] {
        assert_eq!(is_untyped(&ty(text)), reserved, "{text}");
    }
    // The policy keys on the base word alone: `untyped(5)` has the base
    // `untyped`, and normalize is what tells it from the reserved type by its
    // arguments.
    assert_eq!(standing(&ty("untyped(5)").base), Standing::Untyped);
    assert_eq!(standing(&ty("untyped int").base), Standing::Unlisted);
}

#[test]
fn normalize_gives_the_word_one_canonical_spelling_and_leaves_the_ordinary_types_alone() {
    for text in ["untyped", "UNTYPED", "UnTyped"] {
        let normalized = normalize(&ColumnType::new(text, Vec::new())).expect(text);
        assert_eq!(normalized, ColumnType::new("untyped", Vec::new()), "{text}");
        assert_eq!(
            normalize(&normalized).expect("again"),
            normalized,
            "idempotent"
        );
    }
    // Ordinary types that hold the word are written as they are.
    for text in ["untyped(5)", "untyped int", "int untyped", "untyped(10, 2)"] {
        let normalized = normalize(&ty(text)).expect(text);
        assert_eq!(normalized, ty(text), "{text}");
        assert!(!is_untyped(&normalized), "{text}");
    }
    // The empty type stays refused, and says what to declare.
    let error = normalize(&ColumnType::new("", Vec::new()))
        .expect_err("empty")
        .to_string();
    assert!(error.contains("declare `untyped`"), "{error}");
}

#[test]
fn the_policy_lists_the_word_and_changes_nothing_else() {
    assert_eq!(standing("untyped"), Standing::Untyped);
    assert_eq!(standing("UNTYPED"), Standing::Untyped);
    assert_eq!(standing("untyped int"), Standing::Unlisted);
    assert_eq!(standing("untypedx"), Standing::Unlisted);
    assert_eq!(standing("blob"), Standing::Documented);
    assert!(matches!(standing("serial"), Standing::Refused(_)));
    assert!(matches!(standing("untyped serial"), Standing::Refused(_)));
    assert!(matches!(standing("untyped not"), Standing::Refused(_)));
}

#[test]
fn a_type_change_into_or_out_of_the_word_costs_what_it_does_to_a_column_with_no_type() {
    use TypeChangeRisk::{Narrowing, Safe};
    let risk = |from: &str, to: &str| change_risk(&ty(from), &ty(to));
    // The reserved type behaves as `blob` in both directions, against every
    // documented name and the word's own ordinary spellings.
    let mut names: Vec<&str> = DOCUMENTED.to_vec();
    names.extend(["untyped(5)", "untyped int", "decimal(10, 2)"]);
    for name in &names {
        assert_eq!(risk(name, "untyped"), risk(name, "blob"), "{name} into");
        assert_eq!(risk("untyped", name), risk("blob", name), "{name} out of");
    }
    assert_eq!(risk("untyped", "untyped"), Safe);
    assert_eq!(risk("untyped", "blob"), Safe);
    assert_eq!(risk("blob", "untyped"), Safe);
    assert_eq!(
        risk("text", "untyped"),
        Safe,
        "a column with no type converts nothing"
    );
    assert_eq!(
        risk("untyped", "text"),
        Narrowing,
        "out of it the target's affinity converts"
    );
    assert_eq!(
        risk("untyped", "integer"),
        Narrowing,
        "and into the rowid alias"
    );
    // The ordinary type keeps the rules' own answer.
    assert_eq!(risk("untyped(5)", "numeric"), Safe);
    assert_eq!(risk("untyped(5)", "untyped"), risk("numeric", "blob"));
}

#[test]
fn strict_refuses_the_word_and_the_engine_refuses_both_spellings() {
    assert!(!strict_allows(&ty("untyped")));
    assert!(!strict_allows(&ty("untyped(5)")));
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    for sql in [
        "CREATE TABLE s1(x untyped) STRICT",
        "CREATE TABLE s2(x) STRICT",
    ] {
        assert!(c.execute_batch(sql).is_err(), "{sql}");
    }
    c.execute_batch("CREATE TABLE s3(x blob) STRICT")
        .expect("blob is one of the six");
}

#[test]
fn a_type_read_from_the_catalog_is_the_declared_one_and_no_type_is_the_word() {
    // Every declared type survives a round trip through the engine and back;
    // a column with no type is read as the word, and the engine's text for
    // the word is nothing.
    for declared in [
        "int",
        "integer",
        "varchar(10)",
        "decimal(10, 2)",
        "TEXT",
        "VarChar( 10 )",
        "unsigned big int",
        "native character(70)",
        "blob",
        "any",
        "untyped(5)",
        "untyped int",
        "untypedx",
        "uuid",
    ] {
        let mut fresh = Fresh::memory();
        let c = fresh.connection();
        c.execute_batch(&["CREATE TABLE t(a ", declared, ")"].concat())
            .expect(declared);
        let rows = c
            .query("SELECT type FROM pragma_table_xinfo('t')", &[])
            .expect("type");
        let stored = rows[0].text(0).expect("type").to_owned();
        assert_eq!(
            read_back(&stored).expect(declared),
            normalize(&ty(declared)).expect(declared),
            "{declared}"
        );
    }
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch("CREATE TABLE t(a, b int, c \" \")")
        .expect("typeless");
    let rows = c
        .query("SELECT type FROM pragma_table_xinfo('t') ORDER BY cid", &[])
        .expect("types");
    let stored: Vec<&str> = rows.iter().map(|r| r.text(0).expect("type")).collect();
    assert_eq!(stored[0], "");
    assert_eq!(
        read_back(stored[0]).expect("typeless"),
        ColumnType::new("untyped", Vec::new())
    );
    assert_eq!(read_back(stored[1]).expect("int"), ty("int"));
    // A type that is only white space has no type's text either, and the same
    // affinity (measured by the rules' own test).
    assert_eq!(affinity(stored[2]), Affinity::Blob);
    assert_eq!(
        read_back(stored[2]).expect("blank"),
        ColumnType::new("untyped", Vec::new())
    );
}

#[test]
fn a_column_really_declared_with_the_word_is_refused_by_name_and_not_read_as_no_type() {
    for declared in ["untyped", "UNTYPED", "UnTyped"] {
        let mut fresh = Fresh::memory();
        let c = fresh.connection();
        c.execute_batch(&["CREATE TABLE t(a ", declared, ")"].concat())
            .expect(declared);
        let rows = c
            .query("SELECT type FROM pragma_table_xinfo('t')", &[])
            .expect("type");
        let stored = rows[0].text(0).expect("type");
        assert_eq!(stored, declared, "the engine keeps the text as written");
        let error = read_back(stored).expect_err(declared).to_string();
        assert!(
            error.contains("NUMERIC") && error.contains("no type"),
            "{error}"
        );
        assert!(error.contains(declared), "{error}");
    }
    // The ordinary types that hold the word are read as they are.
    assert_eq!(
        read_back("untyped int").expect("ordinary"),
        ty("untyped int")
    );
    assert_eq!(read_back("untyped(5)").expect("ordinary"), ty("untyped(5)"));
    // And what is not a type at all is refused, escaped.
    let error = read_back("not a (type").expect_err("garbage").to_string();
    assert!(error.contains("not one this tool reads"), "{error}");
}
