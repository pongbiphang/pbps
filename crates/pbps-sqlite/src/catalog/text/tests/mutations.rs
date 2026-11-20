//! Local edits of the corpus. A mutated text the engine refuses is never
//! given to the reader (the reader only sees stored text, which is valid). A
//! mutated text the engine takes must read to the same structure as the
//! original, or the tokenizer must refuse it by name, never read it
//! differently; and a feature of the model-less kind added to a column or a
//! table must come out as exactly that marker, in exactly that place.

use super::super::{Kind, Marked, Marker, TableText, Target, table};
use super::corpus::{self, Case};
use super::facts::{agree, facts, stored};
use crate::lexis::{self, Piece};
use crate::tests::Fresh;

/// Where a comment or white space can go without changing what the text says:
/// between code characters at a token boundary, never inside a word or a
/// number, never between the two halves of an operator or a comment opener.
fn boundaries(sql: &str) -> Vec<usize> {
    let mut at = 0;
    let mut out = Vec::new();
    for piece in lexis::pieces(sql) {
        match piece {
            Piece::Code(code) => {
                let chars: Vec<(usize, char)> = code.char_indices().collect();
                for i in 1..chars.len() {
                    let (prev, next) = (chars[i - 1].1, chars[i].1);
                    let wordy = |c: char| {
                        c.is_alphanumeric()
                            || matches!(
                                c,
                                '_' | '$'
                                    | '.'
                                    | '+'
                                    | '-'
                                    | '|'
                                    | '<'
                                    | '>'
                                    | '='
                                    | '!'
                                    | '/'
                                    | '*'
                            )
                    };
                    if !(wordy(prev) && wordy(next)) {
                        out.push(at + chars[i].0);
                    }
                }
            }
            Piece::Quoted(_) | Piece::Comment(_) => {}
        }
        at += match piece {
            Piece::Code(r) | Piece::Quoted(r) | Piece::Comment(r) => r.len(),
        };
    }
    out
}

/// A comparison form of an expression: comments gone, layout collapsed, ASCII
/// case folded outside strings.
fn canon(text: &str) -> String {
    lexis::canonical(text)
}

/// The structure of a table in a form that ignores layout, comments and the
/// case of names, and optionally the markers.
fn structure(text: &TableText, markers: bool) -> String {
    let mut out = format!(
        "{}|{:?}",
        text.name.to_ascii_lowercase(),
        text.columns
            .iter()
            .map(|c| c.to_ascii_lowercase())
            .collect::<Vec<_>>()
    );
    for c in &text.constraints {
        let kind = match &c.kind {
            Kind::Check { expression } => format!("check {}", canon(expression)),
            Kind::Default { text } => format!("default {}", canon(text)),
            other @ (Kind::PrimaryKey { .. }
            | Kind::Unique { .. }
            | Kind::ForeignKey { .. }
            | Kind::NotNull
            | Kind::Null) => format!("{other:?}").to_ascii_lowercase(),
        };
        out.push_str(&format!(
            "\n{:?} {kind} {:?}",
            c.name.as_ref().map(|n| n.to_ascii_lowercase()),
            c.place
        ));
    }
    if markers {
        for m in &text.markers {
            out.push_str(&format!("\n{:?}", m).to_ascii_lowercase());
        }
    }
    out
}

/// The table the engine made of `mutated`, read from its stored text: `None`
/// where the engine refuses the text, `Err` where the tokenizer refuses what
/// the engine took. What the tokenizer reads agrees with the pragmas.
fn tried(case: &Case, mutated: &str) -> Option<Result<(Fresh, TableText), String>> {
    let mut fresh = Fresh::memory();
    fresh.connection().execute_batch(case.setup).expect("setup");
    fresh.connection().execute_batch(mutated).ok()?;
    let (name, sql) = stored(fresh.connection(), "table")
        .into_iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("t"))?;
    let text = match table(&name, &sql) {
        Ok(text) => text,
        Err(refused) => return Some(Err(refused.what().to_owned())),
    };
    agree(&text, &facts(fresh.connection(), &name))
        .unwrap_or_else(|e| panic!("{mutated}\n  stored: {sql}\n  {e}"));
    Some(Ok((fresh, text)))
}

/// `tried`, for a text the tokenizer must read.
fn table_of(case: &Case, mutated: &str) -> Option<(Fresh, TableText)> {
    tried(case, mutated).map(|read| read.unwrap_or_else(|e| panic!("{mutated}\n  {e}")))
}

#[test]
fn layout_and_comments_anywhere_between_tokens_do_not_change_what_is_read() {
    let (mut checked, mut refused) = (0, 0);
    for case in corpus::accepted(corpus::tables()) {
        let Some((_, original)) = table_of(&case, &case.sql) else {
            continue;
        };
        let expected = structure(&original, true);
        for at in boundaries(&case.sql) {
            for filler in [" /*c*/ ", "\n", "\t", " -- c\n"] {
                let mutated = [&case.sql[..at], filler, &case.sql[at..]].concat();
                match tried(&case, &mutated) {
                    None => {}
                    Some(Ok((_, text))) => {
                        assert_eq!(structure(&text, true), expected, "{mutated}");
                        checked += 1;
                    }
                    // The one text the engine takes that the subset does not:
                    // a comment between the sign of a default and its number,
                    // where the dialect's own rule for a default allows none.
                    Some(Err(said)) => {
                        assert!(
                            said.contains("default that is not one of the forms"),
                            "{mutated}: {said}"
                        );
                        assert!(
                            mutated.contains("DEFAULT -")
                                || mutated.contains("DEFAULT +")
                                || mutated.contains("DEFAULT - ")
                                || mutated.contains("DEFAULT +"),
                            "{mutated}"
                        );
                        refused += 1;
                    }
                }
            }
        }
    }
    assert!(checked > 5_000, "{checked}");
    assert!(refused < 100, "{refused}");
}

#[test]
fn the_case_of_every_word_and_name_does_not_change_what_is_read() {
    let mut checked = 0;
    for case in corpus::accepted(corpus::tables()) {
        let Some((_, original)) = table_of(&case, &case.sql) else {
            continue;
        };
        let expected = structure(&original, true);
        for upper in [true, false] {
            let mutated: String = lexis::pieces(&case.sql)
                .into_iter()
                .map(|piece| match piece {
                    Piece::Code(code) if upper => code.to_ascii_uppercase(),
                    Piece::Code(code) => code.to_ascii_lowercase(),
                    Piece::Quoted(run) | Piece::Comment(run) => run.to_owned(),
                })
                .collect();
            if let Some((_, text)) = table_of(&case, &mutated) {
                // Names are read as written; their case is not a difference.
                assert_eq!(structure(&text, true), expected, "{mutated}");
                checked += 1;
            }
        }
    }
    assert!(checked > 100, "{checked}");
}

/// The end of the first top-level item of a table's column list: the byte of
/// its comma or of the closing parenthesis.
fn first_item_end(sql: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut at = 0;
    for piece in lexis::pieces(sql) {
        if let Piece::Code(code) = piece {
            for (i, c) in code.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(at + i);
                        }
                    }
                    ',' if depth == 1 => return Some(at + i),
                    _ => {}
                }
            }
        }
        at += match piece {
            Piece::Code(r) | Piece::Quoted(r) | Piece::Comment(r) => r.len(),
        };
    }
    None
}

#[test]
fn a_feature_added_to_the_first_column_or_the_table_is_exactly_that_marker_in_that_place() {
    // (the text appended after the first column's definition, the marker it
    // must come out as, whether a new constraint is expected with it)
    let features: Vec<(&str, Marker, bool)> = vec![
        (
            " COLLATE nocase",
            Marker::Collate {
                name: "nocase".into(),
            },
            false,
        ),
        (
            " COLLATE rtrim",
            Marker::Collate {
                name: "rtrim".into(),
            },
            false,
        ),
        (
            " NOT NULL ON CONFLICT IGNORE",
            Marker::Conflict(super::super::Conflict::Ignore),
            true,
        ),
        (
            " UNIQUE ON CONFLICT REPLACE",
            Marker::Conflict(super::super::Conflict::Replace),
            true,
        ),
    ];
    let mut added = 0;
    for case in corpus::accepted(corpus::tables()) {
        let Some((_, original)) = table_of(&case, &case.sql) else {
            continue;
        };
        if !original.markers.is_empty() || original.columns.is_empty() {
            continue;
        }
        let Some(end) = first_item_end(&case.sql) else {
            continue;
        };
        for (suffix, marker, constraint) in &features {
            let mutated = [&case.sql[..end], suffix, &case.sql[end..]].concat();
            let Some((_, text)) = table_of(&case, &mutated) else {
                continue;
            };
            let want: Vec<&Marked> = text
                .markers
                .iter()
                .filter(|m| &m.marker == marker)
                .collect();
            assert_eq!(want.len(), 1, "{mutated}: {:?}", text.markers);
            match (&want[0].target, constraint) {
                (Target::Column(0), false) => {}
                (Target::Constraint(i), true) => {
                    assert_eq!(
                        text.constraints[*i].place,
                        super::super::Place::Column(0),
                        "{mutated}"
                    );
                }
                (other, _) => panic!("{mutated}: the marker is on {other:?}"),
            }
            // Nothing else changed: the rest of the structure is the original's
            // (the added constraint is the only new one).
            assert_eq!(text.markers.len(), 1, "{mutated}");
            assert_eq!(
                text.constraints.len(),
                original.constraints.len() + usize::from(*constraint),
                "{mutated}"
            );
            added += 1;
        }
    }
    assert!(added > 100, "{added}");
}

#[test]
fn the_table_options_added_to_a_corpus_table_are_the_markers_the_engine_reports() {
    let mut added = 0;
    for case in corpus::accepted(corpus::tables()) {
        let Some((_, original)) = table_of(&case, &case.sql) else {
            continue;
        };
        if !original.markers.is_empty() {
            continue;
        }
        for (option, marker) in [
            (" STRICT", Marker::Strict),
            (" WITHOUT ROWID", Marker::WithoutRowid),
        ] {
            let mutated = [case.sql.as_str(), option].concat();
            let Some((mut fresh, text)) = table_of(&case, &mutated) else {
                continue;
            };
            assert_eq!(text.markers.len(), 1, "{mutated}");
            assert_eq!(text.markers[0].marker, marker, "{mutated}");
            assert_eq!(text.markers[0].target, Target::Table);
            let flags = fresh
                .connection()
                .query(
                    "SELECT strict, wr FROM pragma_table_list WHERE name = 't' AND schema = 'main'",
                    &[],
                )
                .expect("list");
            let (strict, wr) = (
                flags[0].integer(0).expect("strict"),
                flags[0].integer(1).expect("wr"),
            );
            assert_eq!(
                (strict == 1, wr == 1),
                (marker == Marker::Strict, marker == Marker::WithoutRowid),
                "{mutated}"
            );
            added += 1;
        }
    }
    assert!(added > 20, "{added}");
}

#[test]
fn the_helpers_that_make_the_mutations_do_what_they_say() {
    // A boundary is never inside a word, a number or an operator.
    let sql = "CREATE TABLE t(a int DEFAULT 1.5e3, b text CHECK (b || 'x' <> \"y\"))";
    let wordy = |c: Option<char>| {
        c.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '.' | '|' | '<' | '>'))
    };
    assert!(!boundaries(sql).is_empty());
    for at in boundaries(sql) {
        let (left, right) = (&sql[..at], &sql[at..]);
        assert!(
            !(wordy(left.chars().last()) && wordy(right.chars().next())),
            "{at}: {left:?} | {right:?}"
        );
    }
    assert_eq!(canon("A /* x */  b\n'C d'"), "a b 'C d'");
    assert_eq!(canon("( /*c*/ a ,b )"), "(a,b)");
    assert_eq!(canon("f ('X  y' , 1)"), "f('X  y',1)");
    assert_eq!(first_item_end("CREATE TABLE t(a int(5), b)"), Some(23));
}
