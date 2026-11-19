//! The tokenizer for what only the stored `CREATE` text holds (#2950, item 4).
//!
//! `sqlite_schema.sql` is the one place SQLite keeps constraint names, CHECK
//! expressions, a default as it was written, and the markers of everything the
//! model cannot hold in a table. This module reads exactly that, from the text
//! of a table, and nothing else: structure that a pragma reports (a column's type and
//! position, a key's columns, an index's keys, a foreign key's rows) is the
//! pragmas', and the reader that uses this module checks the two against each
//! other and refuses a table on which they disagree.
//!
//! # The rule
//!
//! The tokenizer is total over a stated subset and **refuses everything else
//! by name**: [`Unread`] says the object and what was not understood. It never
//! guesses, never re-renders a text and never reads inside an expression. An
//! expression is opaque but balanced: its extent is found by counting
//! parentheses over SQLite's own lexis (`lexis`: a comment ends at the first
//! `*/`, only ASCII white space separates tokens, a `)` in a string, a quoted
//! name or a comment does not count), and its text is returned verbatim, with
//! only the white space at its two ends removed.
//!
//! A feature the model has no field for is **data, not an error**: each is
//! reported as a [`Marker`] that says what it is and exactly where it is (which
//! table, column, constraint or key position). Whether a marker makes a table
//! refused or merely noted is the reader's decision. Text outside both the
//! subset and the markers is [`Unread`].
//!
//! # The subset
//!
//! ```text
//! table      := CREATE TABLE name "(" item ("," item)* ")" [ option ("," option)* ]
//! item       := column | CONSTRAINT-prefixed or bare table constraint
//! column     := name type-words ["(" number ["," number] ")"] constraint*
//! constraint := [CONSTRAINT name] ( PRIMARY KEY [ASC|DESC] [conflict] [AUTOINCREMENT]
//!                                 | NOT NULL [conflict] | NULL [conflict] | UNIQUE [conflict]
//!                                 | CHECK "(" expr ")" | DEFAULT default
//!                                 | REFERENCES fk | COLLATE name
//!                                 | [GENERATED ALWAYS] AS "(" expr ")" [STORED|VIRTUAL] )
//! table cons := [CONSTRAINT name] ( PRIMARY KEY "(" key ("," key)* ")" [conflict]
//!                                 | UNIQUE "(" key ("," key)* ")" [conflict]
//!                                 | CHECK "(" expr ")"
//!                                 | FOREIGN KEY "(" name ("," name)* ")" REFERENCES fk )
//! key        := name [COLLATE name] [ASC|DESC]
//! fk         := name ["(" name ("," name)* ")"]
//!               ( ON (DELETE|UPDATE) action | MATCH name | [NOT] DEFERRABLE
//!               | INITIALLY (DEFERRED|IMMEDIATE) )*
//! default    := ["+"|"-"] number | string | blob | NULL | TRUE | FALSE | CURRENT_TIME
//!               | CURRENT_DATE | CURRENT_TIMESTAMP | "(" expr ")"
//! option     := WITHOUT ROWID | STRICT
//! ```
//!
//! A name is a bare word or one of the four quoted forms (`"a"`, `` `a` ``,
//! `[a]`, `'a'`), and is returned unquoted. A column's type is **skipped, not
//! read**: the pragma has it. It ends at the first word that starts a
//! constraint. That rests on the measurement of the keyword table (item 2):
//! every reserved word, and the eight words the grammar takes as names but
//! never inside a type (`cross full indexed inner left natural outer right`),
//! cannot stand in a declared type, so the first of them after a column's name
//! is where the constraints begin or the text is not valid. A column named like
//! a keyword that SQLite takes bare (`key`, `action`, `generated`, `ignore`)
//! is an ordinary name.
//!
//! The text the engine rewrites is in the subset on purpose: `IF NOT EXISTS`,
//! `TEMP` and a schema prefix are not stored; `ADD COLUMN` inserts a column
//! after the last one, `ADD CONSTRAINT` appends a table constraint,
//! `RENAME` rewrites and quotes names, `DROP COLUMN` removes a definition.
//! Layout and comments are anywhere between tokens.

// Read by the catalog reader, the next part of the item; the tests read it now.
#![allow(dead_code)]

use pbps_model::escape::Escaped;

use crate::lexis::{self, Piece};
use crate::{ident, keywords, validate};

/// Text this module does not fully understand. The reader treats it as a
/// refusal of the object, with this sentence as the warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unread {
    what: String,
}

impl Unread {
    pub(crate) fn what(&self) -> &str {
        &self.what
    }
}

impl std::fmt::Display for Unread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.what)
    }
}

/// Where a constraint was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    Table,
    /// The column of this position (`TableText::columns`).
    Column(usize),
}

/// What a marker is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    Table,
    Column(usize),
    /// The constraint of this position (`TableText::constraints`).
    Constraint(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reaction {
    SetNull,
    SetDefault,
    Cascade,
    Restrict,
    NoAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Conflict {
    Rollback,
    Abort,
    Fail,
    Ignore,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Deferral {
    Deferred,
    Immediate,
}

/// What is written for a constraint. Names are unquoted; texts are verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    /// The columns in key order (a column-level key is its own column).
    PrimaryKey {
        columns: Vec<String>,
    },
    Unique {
        columns: Vec<String>,
    },
    /// The text between the parentheses, edges trimmed.
    Check {
        expression: String,
    },
    ForeignKey {
        /// The child columns (a column-level clause is its own column).
        columns: Vec<String>,
        parent: String,
        /// Empty where the clause omits them (the parent's key is meant).
        parent_columns: Vec<String>,
        on_delete: Option<Reaction>,
        on_update: Option<Reaction>,
    },
    NotNull,
    /// An explicit `NULL`.
    Null,
    /// The text after `DEFAULT`, edges trimmed, in one of the listed forms.
    Default {
        text: String,
    },
}

/// One constraint, in the order it is written: the column-level ones column
/// by column, then the table-level ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Constraint {
    /// `None` when no `CONSTRAINT name` stands before it.
    pub name: Option<String>,
    pub kind: Kind,
    pub place: Place,
}

/// A feature the model has no field for, found where it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Marker {
    Strict,
    WithoutRowid,
    /// `AUTOINCREMENT` after a column-level `PRIMARY KEY` (target: that key).
    Autoincrement,
    /// A column's `COLLATE` (target: the column).
    Collate {
        name: String,
    },
    /// A `COLLATE` on the key column at `position` of a key (target: the key).
    KeyCollate {
        position: usize,
        name: String,
    },
    /// `ASC` or `DESC` on the key column at `position` (target: the key).
    KeyDirection {
        position: usize,
        descending: bool,
    },
    /// `ON CONFLICT` on a key or a `NOT NULL` (target: that constraint).
    Conflict(Conflict),
    /// `DEFERRABLE` or `NOT DEFERRABLE` on a foreign key.
    Deferrable {
        not: bool,
    },
    Initially(Deferral),
    /// `MATCH name` on a foreign key.
    Match {
        name: String,
    },
    /// `[GENERATED ALWAYS] AS (..)` (target: the column).
    Generated {
        stored: Option<bool>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Marked {
    pub marker: Marker,
    pub target: Target,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TableText {
    /// The table's name, unquoted.
    pub name: String,
    /// Column names in the order written.
    pub columns: Vec<String>,
    pub constraints: Vec<Constraint>,
    pub markers: Vec<Marked>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Number,
    /// `"a"`, `` `a` `` or `[a]`.
    Name,
    /// `'a'`.
    Str,
    Punct,
}

#[derive(Debug, Clone, Copy)]
struct Tok<'a> {
    class: Class,
    start: usize,
    end: usize,
    text: &'a str,
}

/// The tokens of `sql`, comments and white space left out. An unclosed quote or
/// block comment, which would swallow what follows, and a `;`, which would
/// start another statement, are not text this module reads.
fn lex(sql: &str) -> Result<Vec<Tok<'_>>, &'static str> {
    let mut out = Vec::new();
    let mut at = 0;
    for piece in lexis::pieces(sql) {
        let run = match piece {
            Piece::Code(run) | Piece::Quoted(run) | Piece::Comment(run) => run,
        };
        match piece {
            Piece::Comment(_) => {
                if run.starts_with("/*") && !lexis::closes(piece) {
                    return Err("a block comment that never closes");
                }
            }
            Piece::Quoted(_) => {
                if !lexis::closes(piece) {
                    return Err("a quoted name or string that never closes");
                }
                let class = if run.starts_with('\'') {
                    Class::Str
                } else {
                    Class::Name
                };
                out.push(Tok {
                    class,
                    start: at,
                    end: at + run.len(),
                    text: run,
                });
            }
            Piece::Code(_) => lex_code(run, at, &mut out)?,
        }
        at += run.len();
    }
    Ok(out)
}

fn lex_code<'a>(code: &'a str, base: usize, out: &mut Vec<Tok<'a>>) -> Result<(), &'static str> {
    let chars: Vec<(usize, char)> = code.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (start, c) = chars[i];
        if lexis::is_space(c) {
            i += 1;
            continue;
        }
        let digit_next = |at: usize| chars.get(at + 1).is_some_and(|(_, d)| d.is_ascii_digit());
        let (class, mut j) = if c.is_ascii_digit() || (c == '.' && digit_next(i)) {
            // A number: digits, letters, `_` and `.`, and the sign of an
            // exponent. `0x1F`, `1e+5` and `.5` are one token each.
            let hex = code[start..].starts_with("0x") || code[start..].starts_with("0X");
            let mut j = i + 1;
            while let Some(&(_, d)) = chars.get(j) {
                let exponent_sign = matches!(d, '+' | '-')
                    && !hex
                    && chars
                        .get(j - 1)
                        .is_some_and(|(_, e)| matches!(e, 'e' | 'E'));
                if d.is_ascii_alphanumeric() || d == '_' || d == '.' || exponent_sign {
                    j += 1;
                } else {
                    break;
                }
            }
            (Class::Number, j)
        } else if ident::identifier_continues(c) {
            let mut j = i + 1;
            while chars
                .get(j)
                .is_some_and(|&(_, d)| ident::identifier_continues(d))
            {
                j += 1;
            }
            (Class::Word, j)
        } else {
            if c == ';' {
                return Err("a `;`, which would start another statement");
            }
            (Class::Punct, i + 1)
        };
        if j > chars.len() {
            j = chars.len();
        }
        let end = chars.get(j).map_or(code.len(), |&(at, _)| at);
        out.push(Tok {
            class,
            start: base + start,
            end: base + end,
            text: &code[start..end],
        });
        i = j;
    }
    Ok(())
}

struct Parser<'a> {
    object: &'a str,
    sql: &'a str,
    toks: Vec<Tok<'a>>,
    at: usize,
}

impl<'a> Parser<'a> {
    fn new(object: &'a str, sql: &'a str) -> Result<Self, Unread> {
        let toks = lex(sql).map_err(|why| Unread {
            what: format!("`{}`: {why}", Escaped(object)),
        })?;
        Ok(Self {
            object,
            sql,
            toks,
            at: 0,
        })
    }

    /// An error that names the object, what was expected and where.
    fn fail(&self, what: &str) -> Unread {
        let near: String = self
            .toks
            .get(self.at)
            .map(|t| t.text.chars().take(24).collect())
            .unwrap_or_default();
        let place = if near.is_empty() {
            "at the end of the text".to_owned()
        } else {
            format!("at `{}`", Escaped(&near))
        };
        Unread {
            what: format!("`{}`: {what} ({place})", Escaped(self.object)),
        }
    }

    fn peek(&self) -> Option<Tok<'a>> {
        self.toks.get(self.at).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<Tok<'a>> {
        self.toks.get(self.at + ahead).copied()
    }

    fn word_is(tok: Option<Tok<'_>>, word: &str) -> bool {
        tok.is_some_and(|t| t.class == Class::Word && t.text.eq_ignore_ascii_case(word))
    }

    fn punct_is(tok: Option<Tok<'_>>, c: char) -> bool {
        tok.is_some_and(|t| {
            t.class == Class::Punct && t.text.starts_with(c) && t.text.len() == c.len_utf8()
        })
    }

    fn at_word(&self, word: &str) -> bool {
        Self::word_is(self.peek(), word)
    }

    fn at_punct(&self, c: char) -> bool {
        Self::punct_is(self.peek(), c)
    }

    fn eat_word(&mut self, word: &str) -> bool {
        let found = self.at_word(word);
        if found {
            self.at += 1;
        }
        found
    }

    fn eat_punct(&mut self, c: char) -> bool {
        let found = self.at_punct(c);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_word(&mut self, word: &str) -> Result<(), Unread> {
        if self.eat_word(word) {
            Ok(())
        } else {
            Err(self.fail(&format!("expected `{word}`")))
        }
    }

    fn expect_punct(&mut self, c: char) -> Result<(), Unread> {
        if self.eat_punct(c) {
            Ok(())
        } else {
            Err(self.fail(&format!("expected `{c}`")))
        }
    }

    /// A name: a bare word or one of the four quoted forms, unquoted.
    fn name(&mut self, what: &str) -> Result<String, Unread> {
        let Some(tok) = self.peek() else {
            return Err(self.fail(&format!("expected {what}")));
        };
        let name = match tok.class {
            Class::Word => tok.text.to_owned(),
            Class::Name | Class::Str => match lexis::unquote(tok.text) {
                Some(name) => name,
                None => return Err(self.fail(&format!("expected {what}"))),
            },
            Class::Number | Class::Punct => return Err(self.fail(&format!("expected {what}"))),
        };
        self.at += 1;
        Ok(name)
    }

    /// `( name, name, .. )`.
    fn name_list(&mut self, what: &str) -> Result<Vec<String>, Unread> {
        self.expect_punct('(')?;
        let mut names = vec![self.name(what)?];
        while self.eat_punct(',') {
            names.push(self.name(what)?);
        }
        self.expect_punct(')')?;
        Ok(names)
    }

    /// The balanced group at the cursor, as the byte range between its
    /// parentheses, and the cursor after the closing one. Only a parenthesis
    /// that is code counts: the tokens already leave out strings, quoted names
    /// and comments.
    fn group(&mut self) -> Result<(usize, usize), Unread> {
        let open = match self.peek() {
            Some(t) if Self::punct_is(Some(t), '(') => t,
            _ => return Err(self.fail("expected `(`")),
        };
        let mut depth = 0usize;
        let mut i = self.at;
        while let Some(tok) = self.toks.get(i) {
            if Self::punct_is(Some(*tok), '(') {
                depth += 1;
            } else if Self::punct_is(Some(*tok), ')') {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    self.at = i + 1;
                    return Ok((open.end, tok.start));
                }
            }
            i += 1;
        }
        Err(self.fail("a parenthesis that is never closed"))
    }

    /// The tokens that lie between two byte offsets.
    fn tokens_between(&self, from: usize, to: usize) -> impl Iterator<Item = Tok<'a>> + '_ {
        self.toks
            .iter()
            .copied()
            .filter(move |t| t.start >= from && t.end <= to)
    }

    /// The text of a byte range with the white space at its ends removed.
    fn trimmed(&self, from: usize, to: usize) -> &'a str {
        self.sql
            .get(from..to)
            .unwrap_or("")
            .trim_matches(lexis::is_space)
    }

    /// The text of an expression group, which must hold something.
    fn expression(&mut self, what: &str) -> Result<String, Unread> {
        let (from, to) = self.group()?;
        let text = self.trimmed(from, to);
        if !lexis::has_token(text) {
            return Err(self.fail(&format!("{what} with nothing in it")));
        }
        Ok(text.to_owned())
    }

    fn done(&self) -> Result<(), Unread> {
        if self.at < self.toks.len() {
            Err(self.fail("text after the end of the statement"))
        } else {
            Ok(())
        }
    }
}

/// The words that begin a column constraint. Every one is a reserved word, so
/// none can be part of a declared type.
const CONSTRAINT_STARTS: [&str; 10] = [
    "constraint",
    "primary",
    "not",
    "null",
    "unique",
    "check",
    "default",
    "collate",
    "references",
    "as",
];

fn starts_constraint(word: &str) -> bool {
    CONSTRAINT_STARTS
        .iter()
        .any(|w| w.eq_ignore_ascii_case(word))
}

/// The tokenizer's result for the text of a table. `object` is the table's
/// name as `sqlite_schema` has it, for the messages.
pub(crate) fn table(object: &str, sql: &str) -> Result<TableText, Unread> {
    let mut p = Parser::new(object, sql)?;
    p.expect_word("create")?;
    if p.at_word("temp") || p.at_word("temporary") {
        return Err(p.fail("a temporary table, which is not in the file"));
    }
    p.expect_word("table")?;
    let mut out = TableText {
        name: p.name("the table's name")?,
        ..TableText::default()
    };
    if p.at_punct('.') {
        return Err(p.fail("a schema-qualified name"));
    }
    if !p.eat_punct('(') {
        return Err(p.fail("expected the column list (a table made `AS SELECT` is not read)"));
    }
    loop {
        item(&mut p, &mut out)?;
        if p.eat_punct(',') {
            continue;
        }
        if p.eat_punct(')') {
            break;
        }
        return Err(p.fail("expected `,` or `)` after a column or constraint"));
    }
    if out.columns.is_empty() {
        return Err(p.fail("a table with no column"));
    }
    if p.peek().is_some() {
        loop {
            if p.eat_word("strict") {
                out.markers.push(Marked {
                    marker: Marker::Strict,
                    target: Target::Table,
                });
            } else if p.eat_word("without") {
                p.expect_word("rowid")?;
                out.markers.push(Marked {
                    marker: Marker::WithoutRowid,
                    target: Target::Table,
                });
            } else {
                return Err(p.fail("a table option that is not `STRICT` or `WITHOUT ROWID`"));
            }
            if !p.eat_punct(',') {
                break;
            }
        }
    }
    p.done()?;
    Ok(out)
}

fn item(p: &mut Parser<'_>, out: &mut TableText) -> Result<(), Unread> {
    let table_level = p.peek().is_some_and(|t| {
        t.class == Class::Word
            && ["constraint", "primary", "unique", "check", "foreign"]
                .iter()
                .any(|w| w.eq_ignore_ascii_case(t.text))
    });
    if table_level {
        if out.columns.is_empty() {
            return Err(p.fail("a table constraint before any column"));
        }
        table_constraint(p, out)
    } else {
        if out
            .constraints
            .iter()
            .any(|c| matches!(c.place, Place::Table))
        {
            return Err(p.fail("a column after a table constraint"));
        }
        column(p, out)
    }
}

fn push(out: &mut TableText, name: Option<String>, kind: Kind, place: Place) -> usize {
    out.constraints.push(Constraint { name, kind, place });
    out.constraints.len() - 1
}

fn mark(out: &mut TableText, marker: Marker, target: Target) {
    out.markers.push(Marked { marker, target });
}

/// A marker on the constraint at `key`.
fn mark_key(out: &mut TableText, key: usize, marker: Marker) {
    mark(out, marker, Target::Constraint(key));
}

/// `ON CONFLICT action`, if it is next.
fn conflict(p: &mut Parser<'_>, out: &mut TableText, key: usize) -> Result<(), Unread> {
    if !(p.at_word("on") && Parser::word_is(p.peek_at(1), "conflict")) {
        return Ok(());
    }
    p.at += 2;
    let action = [
        ("rollback", Conflict::Rollback),
        ("abort", Conflict::Abort),
        ("fail", Conflict::Fail),
        ("ignore", Conflict::Ignore),
        ("replace", Conflict::Replace),
    ]
    .into_iter()
    .find(|(word, _)| p.at_word(word));
    let Some((_, action)) = action else {
        return Err(p.fail("a conflict action that is not one of the five"));
    };
    p.at += 1;
    mark_key(out, key, Marker::Conflict(action));
    Ok(())
}

fn column(p: &mut Parser<'_>, out: &mut TableText) -> Result<(), Unread> {
    let name = p.name("a column's name")?;
    let at = out.columns.len();
    out.columns.push(name.clone());
    // The type is skipped: the pragma has it. Its words end at the first one
    // that starts a constraint, and a word that cannot be in a type and does
    // not start a constraint is text this module does not understand.
    loop {
        let Some(tok) = p.peek() else {
            return Err(p.fail("a column definition that is never closed"));
        };
        match tok.class {
            Class::Punct
                if Parser::punct_is(Some(tok), ',') || Parser::punct_is(Some(tok), ')') =>
            {
                break;
            }
            Class::Punct if Parser::punct_is(Some(tok), '(') => {
                // The arguments of a type: numbers, signs and a comma, with
                // comments and layout between them.
                let (from, to) = p.group()?;
                let plain = p.tokens_between(from, to).all(|t| match t.class {
                    Class::Number => true,
                    Class::Punct => t.text.len() == 1 && matches!(t.text, "," | "+" | "-"),
                    Class::Word | Class::Name | Class::Str => false,
                });
                if !plain || p.tokens_between(from, to).next().is_none() {
                    return Err(p.fail("a type argument that is not a number"));
                }
            }
            Class::Word if starts_constraint(tok.text) => break,
            Class::Word if keywords::ends_type(tok.text) => {
                return Err(p.fail(&format!(
                    "`{}` where a type word or a constraint was expected",
                    Escaped(tok.text)
                )));
            }
            Class::Word | Class::Name | Class::Str => p.at += 1,
            Class::Number | Class::Punct => {
                return Err(p.fail("a character that is not part of a type"));
            }
        }
    }
    let mut pending: Option<String> = None;
    loop {
        let Some(tok) = p.peek() else {
            return Err(p.fail("a column definition that is never closed"));
        };
        if Parser::punct_is(Some(tok), ',') || Parser::punct_is(Some(tok), ')') {
            if pending.is_some() {
                return Err(p.fail("a constraint name with no constraint after it"));
            }
            return Ok(());
        }
        if tok.class != Class::Word {
            return Err(p.fail("expected a column constraint"));
        }
        let word = tok.text.to_ascii_lowercase();
        let place = Place::Column(at);
        match word.as_str() {
            "constraint" => {
                if pending.is_some() {
                    return Err(p.fail("two constraint names in a row"));
                }
                p.at += 1;
                pending = Some(p.name("a constraint's name")?);
            }
            "primary" => {
                p.at += 1;
                p.expect_word("key")?;
                let key = push(
                    out,
                    pending.take(),
                    Kind::PrimaryKey {
                        columns: vec![name.clone()],
                    },
                    place,
                );
                if let Some(descending) = direction(p) {
                    mark_key(
                        out,
                        key,
                        Marker::KeyDirection {
                            position: 0,
                            descending,
                        },
                    );
                }
                conflict(p, out, key)?;
                if p.eat_word("autoincrement") {
                    mark_key(out, key, Marker::Autoincrement);
                }
            }
            "not" => {
                p.at += 1;
                p.expect_word("null")?;
                let key = push(out, pending.take(), Kind::NotNull, place);
                conflict(p, out, key)?;
            }
            "null" => {
                p.at += 1;
                let key = push(out, pending.take(), Kind::Null, place);
                conflict(p, out, key)?;
            }
            "unique" => {
                p.at += 1;
                let key = push(
                    out,
                    pending.take(),
                    Kind::Unique {
                        columns: vec![name.clone()],
                    },
                    place,
                );
                conflict(p, out, key)?;
            }
            "check" => {
                p.at += 1;
                let expression = p.expression("a `CHECK`")?;
                push(out, pending.take(), Kind::Check { expression }, place);
            }
            "default" => {
                p.at += 1;
                let text = default(p)?;
                push(out, pending.take(), Kind::Default { text }, place);
            }
            "references" => {
                p.at += 1;
                foreign_key(p, out, vec![name.clone()], pending.take(), place)?;
            }
            "collate" => {
                if pending.is_some() {
                    return Err(p.fail("a constraint name before `COLLATE`"));
                }
                p.at += 1;
                let collation = p.name("a collation's name")?;
                mark(out, Marker::Collate { name: collation }, Target::Column(at));
            }
            "generated" | "as" => {
                if pending.is_some() {
                    return Err(p.fail("a constraint name before a generated column"));
                }
                if p.eat_word("generated") {
                    p.expect_word("always")?;
                }
                p.expect_word("as")?;
                p.expression("a generated column's expression")?;
                let stored = if p.eat_word("stored") {
                    Some(true)
                } else if p.eat_word("virtual") {
                    Some(false)
                } else {
                    None
                };
                mark(out, Marker::Generated { stored }, Target::Column(at));
            }
            _ => return Err(p.fail("a word that is not a column constraint")),
        }
    }
}

/// `ASC` or `DESC`, if next: whether it is descending.
fn direction(p: &mut Parser<'_>) -> Option<bool> {
    if p.eat_word("asc") {
        Some(false)
    } else if p.eat_word("desc") {
        Some(true)
    } else {
        None
    }
}

/// The text after `DEFAULT`: a signed number, a string, a blob, `NULL`,
/// `TRUE`, `FALSE`, `CURRENT_TIME`, `CURRENT_DATE`, `CURRENT_TIMESTAMP` or a
/// parenthesised expression, and nothing else. The form is held to the same
/// rule the dialect applies to a declared default, so the two cannot differ.
fn default(p: &mut Parser<'_>) -> Result<String, Unread> {
    let Some(first) = p.peek() else {
        return Err(p.fail("expected a default"));
    };
    let start = first.start;
    let mut end = first.end;
    match first.class {
        Class::Punct
            if Parser::punct_is(Some(first), '+') || Parser::punct_is(Some(first), '-') =>
        {
            p.at += 1;
            match p.peek() {
                Some(number) if number.class == Class::Number => {
                    end = number.end;
                    p.at += 1;
                }
                _ => return Err(p.fail("a sign that is not followed by a number")),
            }
        }
        Class::Punct if Parser::punct_is(Some(first), '(') => {
            let (_, to) = p.group()?;
            end = to + 1;
        }
        Class::Number | Class::Str => {
            p.at += 1;
        }
        Class::Word => {
            p.at += 1;
            // A blob is `x` and its string; that nothing stands between them
            // is the dialect's rule for a default, applied below.
            if first.text.eq_ignore_ascii_case("x")
                && let Some(blob) = p.peek().filter(|t| t.class == Class::Str)
            {
                end = blob.end;
                p.at += 1;
            }
        }
        Class::Name | Class::Punct => {
            return Err(p.fail("a default that is not one of the forms this reader takes"));
        }
    }
    let text = p.trimmed(start, end);
    if !validate::default_value(text) {
        return Err(p.fail("a default that is not one of the forms this reader takes"));
    }
    Ok(text.to_owned())
}

fn action(p: &mut Parser<'_>) -> Result<Reaction, Unread> {
    let found = if p.eat_word("cascade") {
        Some(Reaction::Cascade)
    } else if p.eat_word("restrict") {
        Some(Reaction::Restrict)
    } else if p.at_word("set") {
        p.at += 1;
        if p.eat_word("null") {
            Some(Reaction::SetNull)
        } else if p.eat_word("default") {
            Some(Reaction::SetDefault)
        } else {
            None
        }
    } else if p.at_word("no") {
        p.at += 1;
        p.eat_word("action").then_some(Reaction::NoAction)
    } else {
        None
    };
    found.ok_or_else(|| p.fail("a foreign key action that is not one of the five"))
}

/// What follows `REFERENCES`, for a column or a table constraint.
fn foreign_key(
    p: &mut Parser<'_>,
    out: &mut TableText,
    columns: Vec<String>,
    name: Option<String>,
    place: Place,
) -> Result<(), Unread> {
    let parent = p.name("the referenced table")?;
    if p.at_punct('.') {
        return Err(p.fail("a schema-qualified parent"));
    }
    let parent_columns = if p.at_punct('(') {
        p.name_list("a referenced column")?
    } else {
        Vec::new()
    };
    let (mut on_delete, mut on_update) = (None, None);
    let mut found = Vec::new();
    loop {
        if p.at_word("on") {
            let delete = Parser::word_is(p.peek_at(1), "delete");
            if !delete && !Parser::word_is(p.peek_at(1), "update") {
                return Err(p.fail("`ON` that is not followed by `DELETE` or `UPDATE`"));
            }
            p.at += 2;
            let chosen = action(p)?;
            let slot = if delete {
                &mut on_delete
            } else {
                &mut on_update
            };
            if slot.replace(chosen).is_some() {
                return Err(p.fail("an action given twice"));
            }
        } else if p.at_word("match") {
            p.at += 1;
            found.push(Marker::Match {
                name: p.name("a match name")?,
            });
        } else if p.at_word("not") && Parser::word_is(p.peek_at(1), "deferrable") {
            p.at += 2;
            found.push(Marker::Deferrable { not: true });
        } else if p.eat_word("deferrable") {
            found.push(Marker::Deferrable { not: false });
        } else if p.eat_word("initially") {
            found.push(Marker::Initially(if p.eat_word("deferred") {
                Deferral::Deferred
            } else if p.eat_word("immediate") {
                Deferral::Immediate
            } else {
                return Err(p.fail("`INITIALLY` that is not `DEFERRED` or `IMMEDIATE`"));
            }));
        } else {
            break;
        }
    }
    let kind = Kind::ForeignKey {
        columns,
        parent,
        parent_columns,
        on_delete,
        on_update,
    };
    let key = push(out, name, kind, place);
    for marker in found {
        mark_key(out, key, marker);
    }
    Ok(())
}

fn table_constraint(p: &mut Parser<'_>, out: &mut TableText) -> Result<(), Unread> {
    let name = if p.eat_word("constraint") {
        Some(p.name("a constraint's name")?)
    } else {
        None
    };
    if p.eat_word("primary") {
        p.expect_word("key")?;
        let key = key_constraint(p, out, name, true)?;
        conflict(p, out, key)
    } else if p.eat_word("unique") {
        let key = key_constraint(p, out, name, false)?;
        conflict(p, out, key)
    } else if p.eat_word("check") {
        let expression = p.expression("a `CHECK`")?;
        push(out, name, Kind::Check { expression }, Place::Table);
        Ok(())
    } else if p.eat_word("foreign") {
        p.expect_word("key")?;
        let columns = p.name_list("a column's name")?;
        p.expect_word("references")?;
        foreign_key(p, out, columns, name, Place::Table)
    } else {
        Err(p.fail("a constraint name that is not followed by a constraint"))
    }
}

/// `( key, key, .. )` of a primary key or a unique constraint.
fn key_constraint(
    p: &mut Parser<'_>,
    out: &mut TableText,
    name: Option<String>,
    primary: bool,
) -> Result<usize, Unread> {
    p.expect_punct('(')?;
    let mut columns = Vec::new();
    let mut pending = Vec::new();
    loop {
        let column = p.name("a key column")?;
        if p.at_punct('(') {
            return Err(p.fail("a key column that is an expression, which is not read"));
        }
        let position = columns.len();
        columns.push(column);
        if p.eat_word("collate") {
            let collation = p.name("a collation's name")?;
            pending.push(Marker::KeyCollate {
                position,
                name: collation,
            });
        }
        if let Some(descending) = direction(p) {
            pending.push(Marker::KeyDirection {
                position,
                descending,
            });
        }
        if !p.eat_punct(',') {
            break;
        }
    }
    p.expect_punct(')')?;
    let kind = if primary {
        Kind::PrimaryKey { columns }
    } else {
        Kind::Unique { columns }
    };
    let key = push(out, name, kind, Place::Table);
    for marker in pending {
        mark_key(out, key, marker);
    }
    Ok(key)
}

#[cfg(test)]
mod tests;
