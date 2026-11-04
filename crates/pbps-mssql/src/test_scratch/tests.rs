use super::*;

fn split(sql: &str) -> Vec<String> {
    batches(sql).expect("framing")
}

#[test]
fn a_go_line_separates_batches_and_is_not_sent() {
    assert_eq!(
        split("CREATE TABLE #t(a int)\nGO\nINSERT #t VALUES (1)\ngo\n"),
        ["CREATE TABLE #t(a int)\n", "INSERT #t VALUES (1)\n"]
    );
    // Case, surrounding blanks and a trailing comment do not stop it being one.
    assert_eq!(split("SELECT 1\n  Go  \nSELECT 2\nGO -- done\n").len(), 2);
}

#[test]
fn go_inside_a_literal_identifier_or_comment_is_not_a_separator() {
    for sql in [
        "SELECT 'a\nGO\nb'",
        "SELECT [a\nGO\nb] FROM t",
        "SELECT \"a\nGO\nb\" FROM t",
        "/* one\nGO\n*/ SELECT 1",
        "/* outer /* inner */\nGO\n*/ SELECT 1",
        "SELECT 'it''s\nGO\nstill'",
        "SELECT [a]]b\nGO\n] FROM t",
    ] {
        assert_eq!(split(sql), [sql.to_owned()], "{sql:?}");
    }
    // A line comment ends with its line, so the next line's GO is real.
    assert_eq!(split("SELECT 1 -- 'x\nGO\nSELECT 2").len(), 2);
}

#[test]
fn a_word_that_only_starts_with_go_is_ordinary_sql() {
    for sql in ["GOTO label", "GO_ON", "GOODBYE", "SELECT 1 GO"] {
        assert_eq!(split(sql), [sql.to_owned()], "{sql:?}");
    }
}

#[test]
fn empty_batches_are_dropped_and_text_is_kept_byte_for_byte() {
    assert!(split("").is_empty());
    assert!(split("GO\n\nGO\n  \n").is_empty());
    // CRLF inside a batch is data (it may sit in a literal), never rewritten.
    assert_eq!(split("SELECT 'a\r\nb'\r\nGO\r\nSELECT 2\r\n").len(), 2);
    assert_eq!(split("SELECT 'a\r\nb'\r\nGO\r\n")[0], "SELECT 'a\r\nb'\r\n");
}

#[test]
fn client_commands_are_refused_before_anything_runs() {
    for (sql, what) in [
        ("SELECT 1\nGO 5\n", "GO <count>"),
        ("SELECT 1\n  go   2\n", "GO <count>"),
        (":r other.sql\nSELECT 1", "SQLCMD"),
        ("SELECT 1\n:setvar a b\n", "SQLCMD"),
        ("SELECT 1\n!! echo hi\n", "SQLCMD"),
    ] {
        match batches(sql) {
            Err(FixtureError::Unsupported(message)) => {
                assert!(message.contains(what), "{sql:?}: {message}");
            }
            other => panic!("{sql:?} was framed: {other:?}"),
        }
    }
}

#[test]
fn legacy_double_colon_calls_and_client_text_in_literals_are_not_commands() {
    for sql in [
        "SELECT\n::fn_helpcollations()",
        "SELECT '\n:r file\n'",
        "SELECT 1 /*\n!! x\nGO 3\n*/",
    ] {
        assert!(batches(sql).is_ok(), "{sql:?}");
    }
}

#[test]
fn identities_embed_the_run_and_differ_by_role() {
    let ids = Identities::for_run("0123");
    assert_eq!(ids.apply, "pbps_test_apply_0123");
    let all = [&ids.apply, &ids.fixture, &ids.assertion];
    assert_eq!(all.iter().collect::<BTreeSet<_>>().len(), 3);
}

#[test]
fn a_difference_in_either_direction_is_a_boundary_violation() {
    let set = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    let expected = BTreeMap::from([("t".to_owned(), set(&["SELECT"]))]);
    assert!(exceeds("u", &expected.clone(), &expected).is_ok());
    let wider = BTreeMap::from([("t".to_owned(), set(&["SELECT", "INSERT"]))]);
    let message = exceeds("u", &wider, &expected).unwrap_err().to_string();
    assert!(message.contains("extra: [INSERT]"), "{message}");
    // A securable only one side has is a difference, and so is a lost right.
    let other = BTreeMap::from([("s".to_owned(), set(&["ALTER"]))]);
    assert!(exceeds("u", &other, &expected).is_err());
    assert!(exceeds("u", &BTreeMap::new(), &expected).is_err());
}
