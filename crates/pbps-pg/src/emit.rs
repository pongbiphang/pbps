//! What one change looks like on PostgreSQL.
//!
//! The counterpart of `pbps-mssql/src/emit.rs`'s structural half, and the only
//! file in this crate that turns a [`Change`] into SQL a server will run.
//!
//! # The write scope
//!
//! Every statement here is wrapped in a `search_path` of its own — the object's
//! own schema first, then the project's configured extras
//! ([`Postgres::with_write_path_extras`]) — because PostgreSQL binds an
//! unqualified name in a *verbatim expression* when the object is created, and
//! this model holds three of those: a column's default, a check's expression
//! and an index's filter (ADR-0013 §3, and the same section's correction that
//! the rule is scoped to what the model can represent).
//!
//! "First" means first **among the schemas the path names**, and that is not
//! the whole ordering. `pg_catalog` is searched ahead of every listed schema
//! whenever the path does not name it — measured, with a `shad.lower(text)`
//! defined and `search_path = shad`, `lower('X')` is the built-in's `x`, and
//! only `search_path = shad, pg_catalog` reaches the project's function. The
//! path is left that way on purpose (DECISIONS 276): naming `pg_catalog` last
//! would let a project type shadow a built-in one, and the emitter cannot
//! defend against that — `character varying` and `timestamp with time zone`
//! have no schema-qualified spelling to write instead.
//!
//! It is per statement and not per session for the reason ADR-0013 measured: a
//! scope held over a statement is also a scope over everything that statement
//! fires. Nothing here fires a user's trigger — this is DDL — but the rule is
//! the write path's, not this file's, and one exception invites a second.
//!
//! Two things this scope deliberately does **not** carry, both because a
//! setting cannot take effect in the batch it appears in (measured; see
//! [`crate::Postgres::transaction_framing`]): `standard_conforming_strings` and
//! `check_function_bodies`. They are pinned by the transaction framing, which
//! is the earlier batch every connection runs.
//!
//! # What is not here
//!
//! Roles and reference data, each of which is its own Phase 5 step and refuses
//! by name through [`crate::Unbuilt`] until it arrives.

use pbps_dialect::{Created, DialectError, Statement};
use pbps_model::{
    Change, Column, ColumnType, ForeignKey, Index, Module, ModuleId, ModuleKind, PrimaryKey,
    ReferentialAction, RoutineArg, Strategy, Table, TableName, UniqueConstraint,
};

use crate::types::DIALECT;
use crate::{Postgres, Unbuilt, quote, types};

type Sql = Result<Vec<Statement>, DialectError>;

fn invalid(message: String) -> DialectError {
    DialectError::Invalid {
        dialect: DIALECT,
        message,
    }
}

/// `"schema"."name"`, quoted on both halves.
fn qualified(t: &TableName) -> Result<String, DialectError> {
    Ok(format!("{}.{}", quote(&t.schema)?, quote(&t.name)?))
}

fn column_list(columns: &[String]) -> Result<String, DialectError> {
    Ok(columns
        .iter()
        .map(|c| quote(c))
        .collect::<Result<Vec<_>, _>>()?
        .join(", "))
}

/// The types whose *text* is read through a session-sensitive input function
/// (ADR-0013 §3), spelled as [`types::normalize`] leaves them.
///
/// Derived from the rule and not recalled: a type is here when the same
/// characters mean different values under different settings. Measured on 18.6,
/// with the identical declaration created under two `DateStyle`s:
///
/// ```text
/// DEFAULT '01/02/2026' on a date        ->  '2026-01-02'::date  /  '2026-02-01'::date
/// DEFAULT '01/02/2026 03:04' on a timestamptz
///                                       ->  '2026-01-02 03:04:00+00'  /  '2026-02-01 03:04:00+00'
/// ```
///
/// No error, no warning: a different value in the table, decided by whoever ran
/// the DDL. `time` and `time with time zone` are on the list because ADR-0013
/// put them there — `'12:00 CST'::timetz` is `12:00:00-06` under one
/// abbreviation dictionary and `12:00:00+09:30` under another — and narrowing a
/// recorded list because today's probe did not reach one of its rows is how the
/// list stops being the rule it was derived from.
const SETTING_SENSITIVE: &[&str] = &[
    "date",
    "time without time zone",
    "time with time zone",
    "timestamp without time zone",
    "timestamp with time zone",
    "interval",
    "real",
    "double precision",
];

/// Whether `expression` is one string literal and nothing else.
///
/// Not a parser, and it does not have to be one: the question is only whether
/// the whole expression is a single literal, in any of the spellings this
/// engine has for one. Anything with a cast, a call or an operator in it
/// answers `false` and is emitted as written.
///
/// **All four openers, and a first version had only the first.** A rule about
/// `'…'` alone would have let `E'01/02/2026'` and `$$01/02/2026$$` through on a
/// `date` column — the same text, the same session-decided value, and a
/// spelling a person copying from somewhere else would write. `U&'…' UESCAPE
/// '!'` is the one form left over: it is two literals with a keyword between
/// them, it answers `false`, and it is named here so that the gap is a recorded
/// one rather than a spelling nobody thought of.
/// The same expression with any number of grouping parentheses taken off.
///
/// `DEFAULT ('01/02/2026')` is the same declaration as `DEFAULT '01/02/2026'`
/// and the engine reads it the same way — measured, it stores `'2026-01-02'`
/// under `DateStyle` MDY and `'2026-02-01'` under DMY, with the parentheses
/// dropped from what it keeps — so a guard that looked only at the first
/// character let it past.
///
/// The test is a paren *depth* that never returns to zero before the end, and
/// it counts only the parentheses that are **code**: one inside a literal or a
/// comment is data, and the scan steps over it.
///
/// **An earlier version counted every parenthesis and argued that it could
/// afford to.** The argument was that a stray one in a literal can only make
/// the test fail, that failing to unwrap merely costs a refusal, and that this
/// guard is allowed to be wrong in that direction. The first half is true and
/// the second is backwards: this guard refuses when it says *yes*, so an
/// expression it cannot unwrap is one it **permits**. Measured, that is a
/// working way to write the hazard down:
///
/// ```text
/// CREATE TABLE t (d date DEFAULT (/* ) */ '01/02/2026'))
///     -> stores 2026-01-02 under DateStyle MDY, 2026-02-01 under DMY
/// ```
///
/// One `)` inside a comment, and a declaration whose value the applying
/// session decides goes through unrefused. So the scan skips literals and
/// comments — the same constructs [`is_a_bare_literal`] already knows, through
/// the same helpers — and that is still not an expression parser: it never
/// asks what any of it *means*.
///
/// Anything more structural — a cast, a concatenation, a function — stays
/// outside the guard on purpose (DECISIONS 174 and 279: this tool does not
/// parse expressions), covered instead by the settings the framing pins.
fn without_grouping(expression: &str) -> &str {
    let mut e = ascii_trim(expression);
    loop {
        let bytes = e.as_bytes();
        if bytes.first() != Some(&b'(') || bytes.last() != Some(&b')') {
            return e;
        }
        let mut depth = 0usize;
        let mut consumed_to = 0usize;
        let mut paired = true;
        // By character rather than by byte: a name outside a literal may hold
        // any of them, and slicing at a continuation byte would panic.
        for (i, ch) in e.char_indices() {
            if i < consumed_to {
                continue;
            }
            if let Some(len) = skip_datum_at(e, i) {
                consumed_to = i + len;
                continue;
            }
            match ch {
                '(' => depth += 1,
                ')' => {
                    // Back to nothing before the end: the leading `(` was
                    // closed by something other than the last character, so
                    // these two are not a pair. A `)` at depth zero is the
                    // same answer, reached by broken text rather than by two
                    // groups.
                    if depth == 0 || (depth == 1 && i + 1 != e.len()) {
                        paired = false;
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
        if !paired || depth != 0 {
            return e;
        }
        e = ascii_trim(&e[1..e.len() - 1]);
    }
}

/// The length of the literal or comment beginning at the start of `rest`, or
/// `None` when what begins there is code.
///
/// One list of the constructs this engine reads as something other than
/// syntax, for every scan that walks an expression: read any of them as code
/// and a parenthesis, a quote or a comment introducer *inside* one is counted
/// as though the declaration had written it.
///
/// An unterminated literal or comment consumes the rest of the text rather
/// than answering `None`. Nothing after it is code — the engine refuses the
/// whole expression by name — and a scan that resumed there would count
/// parentheses that are inside the run-on literal.
/// [`skip_datum`] for the text at `at`, which knows what came before it.
///
/// A `$` after an identifier byte is a byte of that identifier, not the
/// opener of a dollar-quoted literal: `$` continues a name on this engine
/// (`continues_ident`), and **measured**, `CREATE FUNCTION dq.f(foo$tag$
/// integer)` is accepted with the identity `dq.f(integer)` and the name
/// `"foo$tag$"`. Read from the `$` alone, `$tag$` opened a literal nothing
/// closed, the scan consumed the rest of the definition, and the gate refused
/// a routine the engine creates under exactly the declared key. Every scan
/// that walks per character asks this rather than [`skip_datum`] directly,
/// so that there is one place the rule is spelled.
fn skip_datum_at(text: &str, at: usize) -> Option<usize> {
    if text[at..].starts_with('$')
        && text[..at]
            .chars()
            .next_back()
            .is_some_and(pbps_dialect::continues_ident)
    {
        return None;
    }
    skip_datum(&text[at..])
}

fn skip_datum(rest: &str) -> Option<usize> {
    // Longest opener first: `E'` and `U&'` are openers of their own, not a
    // name followed by a literal.
    for (opener, escapes) in [
        ("E'", true),
        ("e'", true),
        ("U&'", false),
        ("u&'", false),
        ("N'", false),
        ("n'", false),
        ("'", false),
    ] {
        if let Some(after) = rest.strip_prefix(opener) {
            return Some(match end_of_literal(after, escapes) {
                Some(end) => opener.len() + end,
                None => rest.len(),
            });
        }
    }
    if let Some(delim) = dollar_delimiter(rest) {
        let body = &rest[delim.len()..];
        return Some(match body.find(delim) {
            Some(at) => delim.len() + at + delim.len(),
            None => rest.len(),
        });
    }
    if rest.starts_with("--") {
        return Some(rest.find(NEWLINE).map_or(rest.len(), |at| at + 1));
    }
    if let Some(after) = rest.strip_prefix("/*") {
        return Some(match end_of_block_comment(after) {
            Some(tail) => rest.len() - tail.len(),
            None => rest.len(),
        });
    }
    None
}

fn is_a_bare_literal(expression: &str) -> bool {
    // A comment is whitespace to this engine, at either end of an expression
    // as much as between two pieces of a continued one, and either can wrap a
    // grouping that wraps another comment. All three strippers hand back a
    // subslice, so the length settling is the text settling.
    let mut e = expression;
    loop {
        let next = without_grouping(without_trailing_trivia(after_the_gap(e).0));
        if next.len() == e.len() {
            break;
        }
        e = next;
    }
    if e.starts_with('$') {
        return is_one_dollar_quoted_literal(e);
    }
    // `E'…'` is the only one of these in which a backslash escapes; `U&'…'`
    // gives the backslash a meaning of its own (a Unicode escape) that does not
    // change where the literal *ends*, which is the only thing asked here.
    let (escapes, rest) = if let Some(r) = e.strip_prefix("E'").or_else(|| e.strip_prefix("e'")) {
        (true, r)
    } else if let Some(r) = e.strip_prefix("U&'").or_else(|| e.strip_prefix("u&'")) {
        (false, r)
    } else if let Some(r) = e.strip_prefix("N'").or_else(|| e.strip_prefix("n'")) {
        (false, r)
    } else if let Some(r) = e.strip_prefix('\'') {
        (false, r)
    } else {
        return false;
    };
    // Where this piece ends, rather than whether the whole tail is one piece:
    // a string constant may be *continued*, and then the expression is still
    // one literal.
    let Some(end) = end_of_literal(rest, escapes) else {
        return false;
    };
    let mut tail = &rest[end..];
    // **Measured**, the continuation rule is narrower than "another literal":
    //
    // ```text
    // 'a' ⏎ 'b'      -> ab          E'a' ⏎ 'b'   -> ab      U&'a' ⏎ 'b' -> ab
    // 'a' ⏎ E'b'     -> syntax error
    // $$a$$ ⏎ $$b$$  -> syntax error
    // 'a' ⏎ 'b\'c'   -> unterminated: the backslash does not escape here,
    //                   even when the first piece was an `E'…'`
    // ```
    //
    // So a continuation is a plain `'…'`, scanned without escapes whatever the
    // first piece was, and it must be preceded by whitespace containing a
    // newline — on one line the same text is a syntax error, which is why the
    // newline is checked rather than assumed.
    while !tail.is_empty() {
        let (after_gap, continues) = after_the_gap(tail);
        // Nothing but whitespace and comments left: the expression is that one
        // literal, trailing comment and all — measured, `'01/02/2026' -- c` is
        // the same constant as `'01/02/2026'`, and a guard that read the
        // comment as structure would let the hazard through behind one.
        if after_gap.is_empty() {
            return true;
        }
        if !continues {
            return false;
        }
        let Some(next) = after_gap.strip_prefix('\'') else {
            return false;
        };
        let Some(end) = end_of_literal(next, false) else {
            return false;
        };
        tail = &next[end..];
    }
    true
}

/// Past the whitespace and comments that follow one piece of a string
/// constant, and whether what they separate can still be a *continuation* of
/// it.
///
/// **Measured**, and neither half is the obvious one:
///
/// ```text
/// '01/02/' -- c ⏎ '2026'     -> 01/02/2026     a line comment is part of the
/// '01/02/' -- /* x ⏎ '2026'  -> 01/02/2026     gap, and the newline that ends
///                                              it is the newline a
///                                              continuation needs
/// '01/02/' /* c */ ⏎ '2026'  -> syntax error   a block comment ends the
/// '01/02/' ⏎ /* c */ '2026'  -> syntax error   possibility of a continuation,
/// '01/02/' /* -- x ⏎ */ '2026' -> syntax error wherever the newline stands
/// '01/02/2026' -- c          -> 01/02/2026     after the last piece either
/// '01/02/2026' /* c */       -> 01/02/2026     comment is only trailing text
/// ```
///
/// So the two comment forms are not interchangeable here, which is why they
/// are scanned rather than skipped together: the engine's `{whitespace}` rule
/// counts a `--` comment among the things a continuation may be written
/// across, and does not count a `/* … */` one (DECISIONS 278).
fn after_the_gap(tail: &str) -> (&str, bool) {
    let mut rest = tail;
    let mut newline = false;
    let mut blocked = false;
    loop {
        let trimmed = ascii_trim_start(rest);
        newline |= rest[..rest.len() - trimmed.len()].contains(NEWLINE);
        rest = trimmed;
        if let Some(after) = rest.strip_prefix("--") {
            let Some(at) = after.find(NEWLINE) else {
                // Runs to the end of the text: nothing can follow it, so this
                // is the whole gap and no continuation is coming.
                return ("", newline);
            };
            newline = true;
            rest = &after[at + 1..];
        } else if let Some(after) = rest.strip_prefix("/*") {
            let Some(next) = end_of_block_comment(after) else {
                // Nothing closes it. The engine refuses that by name
                // (`unterminated /* comment`), and a declaration it refuses by
                // name is left to it (DECISIONS 266) — so this is not a gap
                // and the expression is not a literal this guard knows.
                return (rest, false);
            };
            blocked = true;
            rest = next;
        } else {
            return (rest, newline && !blocked);
        }
    }
}

/// The expression with the whitespace and comments that *follow* it removed.
///
/// [`after_the_gap`] strips them from the front, and the trailing side cannot
/// be done the same way: a `--` comment is recognisable only from its opening,
/// so finding where the code ends means walking the expression forward and
/// stepping over literals, or a `--` inside one reads as a comment.
///
/// **Measured**, and it is the grouping unwrap that needs it: that test is
/// about the *last character*, and a trailing comment is what the last
/// character then is.
///
/// ```text
/// CREATE TABLE t (d date DEFAULT ('01/02/2026') -- note ⏎ )
///   -> stored as '2026-01-02'::date under DateStyle MDY,
///      '2026-02-01'::date under DMY
/// ```
///
/// Nothing is unwrapped, the literal scan answers `false`, and the guard
/// permits it (DECISIONS 282).
fn without_trailing_trivia(e: &str) -> &str {
    let mut end = 0usize;
    let mut consumed_to = 0usize;
    for (i, ch) in e.char_indices() {
        if i < consumed_to {
            continue;
        }
        let rest = &e[i..];
        if rest.starts_with("--") {
            consumed_to = i + rest.find(NEWLINE).map_or(rest.len(), |at| at + 1);
            continue;
        }
        if let Some(after) = rest.strip_prefix("/*")
            && let Some(tail) = end_of_block_comment(after)
        {
            consumed_to = e.len() - tail.len();
            continue;
        }
        if let Some(len) = skip_datum_at(e, i) {
            // A literal, which is code. An *unterminated* block comment reaches
            // here too, because the branch above wanted a closed one: it stays
            // code, so the expression reaches the engine as written and is
            // refused by name (DECISIONS 266), which is what `after_the_gap`
            // decides about the same text.
            consumed_to = i + len;
            end = consumed_to;
            continue;
        }
        if !ch.is_ascii_whitespace() {
            end = i + ch.len_utf8();
        }
    }
    &e[..end]
}

/// Whitespace is ASCII wherever these scans look for it: to this engine a
/// non-ASCII byte is an identifier byte, a non-breaking space included.
/// Measured, `CREATE FUNCTION r10.f(a r10.x\u{a0}, b int)` has the identity
/// `r10.f(r10."x\u{a0}",integer)` — the byte is the end of the type's name,
/// and a scan that trimmed it read a type that does not exist and refused a
/// valid declaration. `str::trim` is Unicode's answer, and the wrong one here
/// (DECISIONS 313).
fn ascii_trim(text: &str) -> &str {
    ascii_trim_end(ascii_trim_start(text))
}

fn ascii_trim_start(text: &str) -> &str {
    text.trim_start_matches(|c: char| c.is_ascii_whitespace())
}

fn ascii_trim_end(text: &str) -> &str {
    text.trim_end_matches(|c: char| c.is_ascii_whitespace())
}

/// The characters this engine ends a line with, either of them alone.
///
/// **Measured**, and the reason this is a set and not `'\n'`: a bare carriage
/// return is a newline to this lexer, in both places one matters here.
///
/// ```text
/// '01/02/' ⏎(CR) '2026'      -> 01/02/2026   it continues a string constant
/// '01/02/' -- c ⏎(CR) '2026' -> 01/02/2026   and it ends a line comment
/// ```
///
/// The repo has met this before, in the module scanner: a comment ends at a
/// carriage return, and a rule written for `\n` alone reads the rest of the
/// file as commented (PITFALLS, "A comment ends at a carriage return").
const NEWLINE: [char; 2] = ['\n', '\r'];

/// The text after the `*/` closing the block comment whose `/*` was just
/// consumed, or `None` when nothing closes it.
///
/// These nest — measured, `/* /* x */ */` is one comment — so a `/*` inside
/// one opens another, and a `--` inside one is comment text rather than a
/// comment.
fn end_of_block_comment(after: &str) -> Option<&str> {
    let bytes = after.as_bytes();
    let mut depth = 1usize;
    let mut i = 0;
    while i + 1 < bytes.len() {
        match (bytes[i], bytes[i + 1]) {
            (b'/', b'*') => {
                depth += 1;
                i += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return Some(&after[i..]);
                }
            }
            // Every other byte, multi-byte continuation bytes included: none
            // of them can begin either delimiter, both of which are ASCII.
            _ => i += 1,
        }
    }
    None
}

/// The byte index just past the closing quote of the literal that starts at
/// the beginning of `rest`, which is the text *after* its opening quote.
fn end_of_literal(rest: &str, escapes: bool) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if escapes => i += 2,
            b'\'' if bytes.get(i + 1) == Some(&b'\'') => i += 2,
            // The closing quote, and the first byte after it is the answer.
            b'\'' => return Some(i + 1),
            // Every other byte, which includes the continuation bytes of a
            // multi-byte character: none of them can be one of the two above.
            _ => i += 1,
        }
    }
    // Ran off the end: either nothing closed it, or a step consumed the
    // closing quote as an escaped one — `E'a\'` is not a closed literal.
    None
}

/// The opening `$tag$` of a dollar-quoted literal at the start of `rest` —
/// the delimiter itself, both dollars included — or `None` when what starts
/// there is not one. `$1` is a parameter placeholder and not an opener, which
/// is the same answer.
///
/// The tag's own rule: empty, or a name that does not start with a digit.
///
/// Asked with the scanner's predicate, not with `char::is_alphanumeric`. The
/// engine's grammar is over **bytes** — `dolq_start [A-Za-z\200-\377_]`,
/// `dolq_cont` the same plus digits (DECISIONS 233) — so every byte of a
/// non-ASCII character is a tag character, including ones Unicode calls marks
/// rather than letters. Measured, `$á$…$á$` with `á` spelled `a` then U+0301
/// is one dollar-quoted literal to the engine; `is_alphanumeric` says the mark
/// is neither letter nor digit, so this read the expression as *not* a bare
/// literal and the default guard never looked at it.
fn dollar_delimiter(rest: &str) -> Option<&str> {
    let after = rest.strip_prefix('$')?;
    let at = after.find('$')?;
    let tag = &after[..at];
    if !tag.is_empty()
        && !(tag.starts_with(|c: char| !c.is_ascii_digit() && pbps_dialect::continues_ident(c))
            && tag.chars().all(pbps_dialect::continues_ident))
    {
        return None;
    }
    Some(&rest[..at + 2])
}

/// Whether `e` is one `$tag$…$tag$` literal and nothing else.
fn is_one_dollar_quoted_literal(e: &str) -> bool {
    let Some(delim) = dollar_delimiter(e) else {
        return false;
    };
    let rest = &e[delim.len()..];
    let Some(close) = rest.find(delim) else {
        return false;
    };
    // Whitespace and comments may follow it and nothing else: a *second* pair
    // of the same tag would be two literals side by side, and — measured —
    // this form does not continue across a newline the way a quoted one does
    // (`$$a$$ ⏎ $$b$$` is a syntax error).
    after_the_gap(&rest[close + delim.len()..]).0.is_empty()
}

/// Refuses a default whose value would be decided by whoever applies it.
///
/// ADR-0013 §3: a plain-literal default on a setting-sensitive column reaches
/// the server as the resolved typed spelling, canonicalized by the engine at
/// plan time. This is the offline half of that decision — nothing here has a
/// connection to ask.
///
/// **What a cast buys, measured, because an earlier version of this comment
/// said it bought the whole thing.** A cast does *not* make the value
/// session-independent: `'01/02/2026'::date`, `DATE '01/02/2026'` and
/// `CAST('01/02/2026' AS date)` all store 2026-01-02 under `DateStyle` MDY and
/// 2026-02-01 under DMY, exactly as the uncast spelling does. What decides the
/// value is whether the *spelling* is ambiguous — a question about a value, and
/// therefore the engine's to answer.
///
/// So the line drawn here is between *provably* unresolved and *possibly*
/// resolved. Measured, this engine reads every string default back with a cast
/// welded on — `'unnamed'` becomes `'unnamed'::text` — so a bare literal is
/// certainly not the engine's own rendering, certainly not canonical, and
/// refusing it costs nothing a declaration could want. A cast form may be that
/// rendering, and usually is: it is what `pull` writes.
///
/// **The typed-but-ambiguous case is the residue, and it is deliberate.** The
/// only offline rule that closes it refuses `'2026-01-02'::date` as well — a
/// correct declaration, the one `pull` writes, with no remedy a message could
/// name. ADR-0013 §3 closes it at plan time, connected, and that resolver
/// arrives with the step that has a caller for it (issue #173).
///
/// None of this is about the plan converging. The state records what each
/// object was declared as beside what it read back (DECISIONS 207–209), so a
/// declaration in any spelling goes quiet after the apply that records it. What
/// does not go away is a column defaulting to February in one environment and
/// January in another.
///
/// Its caller in `validate` is not a second guard, it is the *earlier* one, and
/// it is where the whole rule is actually enforced: [`Change::AlterColumnDefault`]
/// carries a `ColumnRef` and two expressions and **no type**, so the emitter
/// cannot ask this question on the one path that changes a default on a column
/// that already exists. `validate_table` sees the declaration, types and all,
/// and every command that hands statements to a database runs it
/// (DECISIONS 141).
pub(crate) fn refuse_an_unresolved_default(
    column: &str,
    ty: &ColumnType,
    default: &str,
) -> Option<DialectError> {
    if !SETTING_SENSITIVE.contains(&ty.base.as_str()) || !is_a_bare_literal(default) {
        return None;
    }
    Some(invalid(format!(
        "column `{column}` is `{ty}` and its default is the bare literal {default}. What that \
         text means is decided by the session that runs the `CREATE`: measured, `'01/02/2026'` on \
         a `date` stores 2026-01-02 under `DateStyle` MDY and 2026-02-01 under DMY, with no error \
         either way — so this column would default to February in one environment and January in \
         another. Write it the way the engine renders it, with the cast it welds on and a \
         spelling that cannot be read two ways: `'2026-01-02'::date` (ADR-0013 §3, §4)."
    )))
}

/// A declared expression, followed by the newline that closes any comment in
/// it.
///
/// The three expressions this dialect writes verbatim — a default, a check and
/// an index filter (ADR-0013 §3) — are the user's text, and the emitter's own
/// syntax follows them on the same line. A trailing line comment then swallows
/// it. **Measured**, both halves:
///
/// ```text
/// CREATE TABLE t (n int, CONSTRAINT ck CHECK (n > 0 -- reason));
///   -> ERROR: syntax error at end of input
/// CREATE TABLE t (a int DEFAULT 1 -- why, b int);
///   -> ERROR: syntax error at end of input
/// CREATE TABLE t (n int, CONSTRAINT ck CHECK (n > 0 -- reason ⏎ ));
///   -> accepted, and stored as CHECK ((n > 0))
/// ```
///
/// So a valid declaration produced a statement that cannot run, in every one
/// of the five places an expression is interpolated. One newline is the whole
/// fix, and it goes here rather than at each site so that a sixth place has
/// somewhere to reach for (DECISIONS 281).
fn verbatim(expression: &str) -> String {
    format!("{expression}\n")
}

fn null_clause(nullable: bool) -> &'static str {
    if nullable { "NULL" } else { "NOT NULL" }
}

/// One line of a `CREATE TABLE` column list, or the body of an `ALTER TABLE ADD`.
fn column_definition(name: &str, column: &Column) -> Result<String, DialectError> {
    let mut s = format!("{} {}", quote(name)?, types::normalize(&column.ty)?);
    if let Some(id) = column.identity {
        // `GENERATED ALWAYS`, never `BY DEFAULT`: the model holds a seed and an
        // increment and nothing that could tell the two apart, and
        // introspection says so in as many words — read back, a `BY DEFAULT`
        // identity is indistinguishable from the one a plan would emit. The
        // sequence's bounds and its cache are left to the engine for the same
        // reason: those are the values `types::identity_seed_range` and the
        // pull's default-cache check expect to see.
        s.push_str(&format!(
            " GENERATED ALWAYS AS IDENTITY (START WITH {} INCREMENT BY {})",
            id.seed, id.increment
        ));
    }
    s.push(' ');
    s.push_str(null_clause(column.nullable));
    if let Some(expr) = &column.default {
        if let Some(e) = refuse_an_unresolved_default(name, &types::normalize(&column.ty)?, expr) {
            return Err(e);
        }
        s.push_str(&format!(" DEFAULT {}", verbatim(expr)));
    }
    Ok(s)
}

fn primary_key_clause(pk: &PrimaryKey) -> Result<String, DialectError> {
    let cols = column_list(&pk.columns)?;
    Ok(match &pk.name {
        Some(n) => format!("CONSTRAINT {} PRIMARY KEY ({cols})", quote(n)?),
        // Unnamed leaves the server to invent one, which is a choice a user can
        // make and is emitted faithfully rather than named on their behalf.
        None => format!("PRIMARY KEY ({cols})"),
    })
}

fn unique_clause(name: &str, u: &UniqueConstraint) -> Result<String, DialectError> {
    Ok(format!(
        "CONSTRAINT {} UNIQUE ({})",
        quote(name)?,
        column_list(&u.columns)?
    ))
}

const fn referential_action(a: ReferentialAction) -> &'static str {
    match a {
        ReferentialAction::NoAction => "NO ACTION",
        ReferentialAction::Cascade => "CASCADE",
        ReferentialAction::SetNull => "SET NULL",
        ReferentialAction::SetDefault => "SET DEFAULT",
    }
}

fn foreign_key_clause(name: &str, fk: &ForeignKey) -> Result<String, DialectError> {
    let mut s = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        quote(name)?,
        column_list(&fk.columns)?,
        qualified(&fk.references_table)?,
        column_list(&fk.references_columns)?
    );
    // NO ACTION is the default, and spelling out a default adds noise to a plan
    // a human has to read at a deployment gate.
    if fk.on_delete != ReferentialAction::NoAction {
        s.push_str(&format!(" ON DELETE {}", referential_action(fk.on_delete)));
    }
    if fk.on_update != ReferentialAction::NoAction {
        s.push_str(&format!(" ON UPDATE {}", referential_action(fk.on_update)));
    }
    Ok(s)
}

/// Whether this index would be built with `CONCURRENTLY`.
///
/// Two conditions, and the second is not a convenience. **Measured on 18.6**, a
/// concurrent build cannot share a batch with anything:
///
/// ```text
/// SET LOCAL search_path = m2s; CREATE INDEX CONCURRENTLY ix1 ON m2s.t (id);
///   -> ERROR: CREATE INDEX CONCURRENTLY cannot run inside a transaction block
/// ```
///
/// so the statement cannot carry the write `search_path` its filter would be
/// bound under, and a path set by a *preceding* statement is not there after a
/// staged apply resumes on a new connection. An index with a filter is
/// therefore built the ordinary way and the `online` hint is dropped — which
/// the trait allows and requires the reason for: a hint says how to get there,
/// so a dialect that cannot honour one emits the statement without it rather
/// than refusing a plan whose destination is the same either way. An index
/// without a filter has no expression to bind and needs no path at all, so
/// nothing is lost by leaving the scope off it.
const fn built_concurrently(index: &Index, strategy: Strategy) -> bool {
    strategy.online && index.filter.is_none()
}

fn create_index(
    table: &TableName,
    name: &str,
    index: &Index,
    strategy: Strategy,
) -> Result<String, DialectError> {
    let keys = index
        .columns
        .iter()
        .map(|c| {
            Ok(format!(
                "{} {}",
                quote(&c.name)?,
                if c.descending { "DESC" } else { "ASC" }
            ))
        })
        .collect::<Result<Vec<_>, DialectError>>()?
        .join(", ");

    let mut s = format!(
        "CREATE {}INDEX {}{} ON {} ({keys})",
        if index.unique { "UNIQUE " } else { "" },
        if built_concurrently(index, strategy) {
            "CONCURRENTLY "
        } else {
            ""
        },
        quote(name)?,
        qualified(table)?
    );
    if !index.include.is_empty() {
        s.push_str(&format!(" INCLUDE ({})", column_list(&index.include)?));
    }
    if let Some(filter) = &index.filter {
        s.push_str(&format!(" WHERE ({})", verbatim(filter)));
    }
    s.push(';');
    Ok(s)
}

/// Drops whatever primary key the table currently has.
///
/// A `DO` block when the declaration did not name it, for the reason SQL
/// Server's counterpart uses dynamic SQL: only a key pbps created carries a
/// predictable name, and adopting a database pbps did not create is the whole
/// point of `pull`. `quote_ident` and `quote_literal` do the quoting inside the
/// block, so a name the server invented cannot break out of it.
fn drop_primary_key(table: &TableName, pk: &PrimaryKey) -> Result<String, DialectError> {
    let q = qualified(table)?;
    Ok(match &pk.name {
        Some(n) => format!("ALTER TABLE {q} DROP CONSTRAINT {};", quote(n)?),
        // Both interpolations are in **literal** position and neither is in
        // code position, which is the trap: the name is inside a `DO` body and
        // then again inside the format string that body executes, so quoting it
        // as an identifier there would close the literal at the first
        // apostrophe. It is escaped for `format` first — a `%` in a name is a
        // placeholder to that function — and quoted as a literal after.
        None => {
            let body = format!(
                "DECLARE pk name := (SELECT conname FROM pg_catalog.pg_constraint\n\
                 \x20                    WHERE conrelid = {}::pg_catalog.regclass AND contype = 'p');\n\
                 BEGIN\n\
                 \x20   IF pk IS NOT NULL THEN\n\
                 \x20       EXECUTE pg_catalog.format({}, pk);\n\
                 \x20   END IF;\n\
                 END",
                literal(&q),
                literal(&format!(
                    "ALTER TABLE {} DROP CONSTRAINT %I",
                    q.replace('%', "%%")
                ))
            );
            let tag = dollar_tag(&body);
            format!("DO {tag}\n{body}\n{tag};")
        }
    })
}

/// A `$…$` tag that the body cannot close.
///
/// **The scan is the point.** PostgreSQL's lexer looks for a dollar-quote's
/// closing tag *literally*, without regard for quotes inside it, so a body
/// containing the tag ends the block there — and a table named `x$pbps$y` is a
/// legal identifier that would do exactly that, with the rest of the block
/// arriving as top-level SQL. Choosing a tag the body does not contain makes
/// that unrepresentable rather than checked for.
fn dollar_tag(body: &str) -> String {
    (0..)
        .map(|n| {
            if n == 0 {
                "$pbps$".to_owned()
            } else {
                format!("$pbps{n}$")
            }
        })
        .find(|tag| !body.contains(tag.as_str()))
        .expect("a body is finite and the tags are not")
}

/// A string literal, quoted the way the engine's own `quote_literal` does.
///
/// Doubling is the whole rule only while `standard_conforming_strings` is `on`,
/// which the transaction framing pins and which this crate's scanner already
/// assumes (ADR-0011 Amendment 2).
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The write `search_path` for an object in `schema`.
/// The one schema name a `search_path` cannot carry, quoted or not.
///
/// **Measured on 18.6**, with a schema literally named `$user` and a function
/// in it, under role `postgres` with a `postgres` schema beside it:
///
/// ```text
/// SET search_path = "$user";
/// SELECT current_setting('search_path'), which();
///   -> "$user" | the role schema
/// ```
///
/// The quotes survive into the setting and change nothing: the engine
/// substitutes the entry for the current role's own schema. So a path built
/// for a table in a schema of that name silently scopes the statement
/// somewhere else, and an unqualified name in a check, a filter or a default
/// binds to whatever the deployment role happens to own — a different object,
/// or none.
///
/// The comparison is exact and case-sensitive because the engine's is
/// (`namespace.c` compares against `"$user"`): refusing `$USER` as well would
/// refuse a schema name this engine treats as ordinary.
pub(crate) const NOT_A_SCHEMA_A_PATH_CAN_NAME: &str = "$user";

/// The schema a path changes the meaning of by *listing* it.
///
/// `pg_catalog` is searched first while a path leaves it out, and sits where
/// it is written when a path names it. Measured, in a schema `shad` holding a
/// function `lower(text)` of its own:
///
/// ```text
/// SET search_path = "shad";                  CHECK (lower(c) = c) -> pg_catalog.lower
/// SET search_path = "shad", "pg_catalog";    CHECK (lower(c) = c) -> shad.lower
/// ```
///
/// Both statements are accepted and neither says anything; the constraint the
/// second one stores calls a different function from the one the declaration
/// reads as. So an extra of this name is refused rather than dropped from the
/// path: an emitted definition whose meaning depends on where a caller put
/// `pg_catalog` is exactly what the write path exists to prevent
/// (ADR-0013 §3, DECISIONS 276 and 277).
pub(crate) const NOT_A_SCHEMA_A_PATH_MAY_LIST: &str = "pg_catalog";

fn write_path(pg: &Postgres, schema: &str) -> Result<String, DialectError> {
    let mut parts = Vec::new();
    for part in std::iter::once(schema).chain(pg.write_path_extras().iter().map(String::as_str)) {
        if part == NOT_A_SCHEMA_A_PATH_MAY_LIST {
            return Err(invalid(format!(
                "`{part}` cannot be part of a write `search_path`: this engine searches it first \
                 only while a path leaves it out, and a path that names it puts it where it is \
                 written. Measured, a schema holding a `lower(text)` of its own binds \
                 `CHECK (lower(c) = c)` to the built-in under `SET search_path = \"shad\"` and to \
                 its own function under `SET search_path = \"shad\", \"pg_catalog\"`, with no \
                 error either way — so listing it would let a project object shadow a built-in \
                 in a check, a filter or a default (ADR-0013 §3). Leave it out; it is already \
                 first."
            )));
        }
        if part == NOT_A_SCHEMA_A_PATH_CAN_NAME {
            return Err(invalid(format!(
                "`{part}` cannot be part of a write `search_path`: this engine reads that entry \
                 as the current role's own schema rather than as a schema of that name, and \
                 quoting it does not help — measured, `SET search_path = \"$user\"` binds an \
                 unqualified name through the deployment role's schema even where a schema \
                 called `$user` exists. A statement scoped that way would resolve a name in a \
                 check, a filter or a default against whatever that role owns (ADR-0013 §3)."
            )));
        }
        parts.push(quote(part)?);
    }
    Ok(parts.join(", "))
}

/// One statement, under the write path of `schema`.
///
/// `SET` and not `SET LOCAL`: a rendered script is run statement by statement
/// outside any transaction, where `SET LOCAL` is a warning and a no-op, and a
/// scope that quietly does nothing is worse than none. The `RESET` afterwards
/// is in the same batch, so inside a transaction a failure rolls it back with
/// everything else, and outside one it returns the connection to the settings
/// the operator's environment gives it.
fn scoped(pg: &Postgres, schema: &str, body: &str) -> Result<Statement, DialectError> {
    Ok(Statement::new(format!(
        "SET search_path = {};\n{body}\nRESET search_path;",
        write_path(pg, schema)?
    )))
}

/// The same, for a table's own schema.
fn on(pg: &Postgres, table: &TableName, body: &str) -> Result<Statement, DialectError> {
    scoped(pg, &table.schema, body)
}

fn one(pg: &Postgres, table: &TableName, body: String) -> Sql {
    Ok(vec![on(pg, table, &body)?])
}

/// The engine's keyword for a module kind.
const fn keyword(kind: ModuleKind) -> &'static str {
    match kind {
        ModuleKind::View => "VIEW",
        ModuleKind::Procedure => "PROCEDURE",
        ModuleKind::Function => "FUNCTION",
        ModuleKind::Trigger => "TRIGGER",
    }
}

/// The whole `CREATE` statement for a module, under its schema's write path.
///
/// The emitter composes the prefix and the declaration holds the body, so the
/// SQL still appears exactly once (ADR-0002). Where the prefix ends is the
/// engine's grammar, not a convention:
///
/// | Kind | Emitted prefix | So `definition:` starts at |
/// |---|---|---|
/// | view | `CREATE VIEW <name> AS` | the `SELECT` |
/// | function, procedure | `CREATE FUNCTION <name>` | the parameter list |
/// | trigger | `CREATE TRIGGER <name>` | `AFTER INSERT ON <table> …` |
///
/// **A trigger is the one that differs from the SQL Server side, and it is the
/// grammar that decides it.** T-SQL writes `CREATE TRIGGER x ON t AFTER
/// INSERT`, so the emitter can supply the table; PostgreSQL writes
/// `CREATE TRIGGER x AFTER INSERT ON t`, where the table comes *after* text
/// only the declaration holds. Splitting the prefix there would mean finding
/// the end of the event list, which is parsing SQL (§8.2). So the table is in
/// the identity **and** in the body, and [`validate_module`] refuses a
/// declaration where the body does not name the table the identity does —
/// because nothing else would catch it: **measured**, `CREATE TRIGGER audit
/// AFTER INSERT ON app.other` under the key `app.t.audit` is accepted by the
/// engine, and the mismatch only surfaces a plan later when
/// `DROP TRIGGER audit ON app.t` cannot find it.
///
/// And **measured**, a trigger's own name is never schema-qualified:
///
/// ```text
/// CREATE TRIGGER m1.audit AFTER INSERT ON m1.t …   syntax error at or near "."
/// CREATE TRIGGER audit    AFTER INSERT ON m1.t …   accepted
/// ```
///
/// which is the same fact ADR-0009 §1 records from the other side: the schema
/// in `ModuleId::Trigger` is the table's, and there is nowhere else for it to
/// come from (DECISIONS 302).
fn create_module(pg: &Postgres, id: &ModuleId, module: &Module) -> Result<Statement, DialectError> {
    // ASCII, not Unicode: a non-breaking space is an identifier byte to this
    // engine (DECISIONS 313), and **measured**, `CREATE VIEW v AS SELECT 1 AS
    // x\u{a0}` names the column `x\u{a0}` — two characters. `str::trim` took
    // the byte off the end of the body, and the view the plan created had a
    // column the declaration does not name.
    let body = ascii_trim(&module.definition);
    if body.is_empty() {
        return Err(empty_definition(id));
    }
    let sql = match module.kind {
        // The `AS` is the emitter's, so a view's definition is just its query —
        // which is what a reader of the declarations wants to see.
        ModuleKind::View => format!("CREATE VIEW {} AS\n{body}", qualified(&id.object_name())?),
        // A parameter list is part of the object's contract and modelling
        // PostgreSQL's parameter syntax — modes, defaults, `VARIADIC` — would
        // be parsing SQL. So everything after the name is the user's
        // (ADR-0009 §1).
        ModuleKind::Function | ModuleKind::Procedure => format!(
            "CREATE {} {}\n{body}",
            keyword(module.kind),
            qualified(&id.object_name())?
        ),
        ModuleKind::Trigger => format!("CREATE TRIGGER {}\n{body}", quote(id.name())?),
    };
    // The terminator on a line of its own, because the line before it is the
    // user's: a definition ending in `-- note` would otherwise swallow it, and
    // the statement would run on into the `RESET search_path` the scope adds.
    // The same rule as every other verbatim expression here (DECISIONS 281),
    // and the whole body is verbatim.
    scoped(pg, id.schema(), &format!("{sql}\n;"))
}

/// The `DROP` for a module, under its schema's write path.
///
/// **Never `CASCADE`.** It is the shortest path out of every dependency
/// refusal in ADR-0009 §4 and it destroys objects nobody reviewed; SPEC 14.3's
/// guardrail is that the plan names every object it drops, or it does not drop.
/// The dependents are the connected plan's to enumerate and to put in front of
/// the approver.
fn drop_module(pg: &Postgres, id: &ModuleId, kind: ModuleKind) -> Result<Statement, DialectError> {
    let sql = match kind {
        ModuleKind::View => format!("DROP VIEW {};", qualified(&id.object_name())?),
        // With the signature, because the name alone is not the object: two
        // overloads share it, and **measured**, `DROP FUNCTION app.f` is
        // refused by name where more than one exists.
        ModuleKind::Function | ModuleKind::Procedure => format!(
            "DROP {} {}({});",
            keyword(kind),
            qualified(&id.object_name())?,
            signature(id)?
        ),
        // `DROP TRIGGER audit` is a syntax error: the name is scoped to the
        // table (ADR-0009 §1), so the table is not decoration here.
        ModuleKind::Trigger => format!(
            "DROP TRIGGER {} ON {};",
            quote(id.name())?,
            qualified(attached_to(id)?)?
        ),
    };
    scoped(pg, id.schema(), &sql)
}

/// The argument types of a routine's identity, as the engine spells them.
///
/// Interpolated rather than quoted because they are type names, not
/// identifiers — and safe to interpolate because [`RoutineArg`] admits only
/// the characters a type name is written with.
fn signature(id: &ModuleId) -> Result<String, DialectError> {
    let args = id.args().ok_or_else(|| {
        invalid(format!(
            "`{id}` is a routine without an argument list, and this engine identifies a routine by \
             its arguments: two overloads share the name, so `DROP FUNCTION` needs the signature \
             to say which one (ADR-0009 §1)"
        ))
    })?;
    Ok(args
        .iter()
        .map(RoutineArg::as_str)
        .collect::<Vec<_>>()
        .join(", "))
}

fn attached_to(id: &ModuleId) -> Result<&TableName, DialectError> {
    id.attached_to().ok_or_else(|| {
        invalid(format!(
            "trigger `{id}` does not say which table it is on, and on this engine a trigger's \
             name is scoped to its table rather than to a schema (ADR-0009 §1)"
        ))
    })
}

/// The table a `CREATE TRIGGER` body says it is on.
///
/// **Not `references`.** A scan for the name anywhere in the text answers yes
/// to `AFTER UPDATE OF t ON app.other` under the identity `app.t.audit`,
/// because the column list mentions `t` — so the check passed and the engine
/// created the trigger on the wrong table, which is the exact silent mismatch
/// the check exists to prevent. What decides the outcome is the name after
/// `ON`, so that is what is read.
///
/// This is a lexical scan and not a parse: it steps over literals and comments
/// with [`skip_datum`], counts parentheses so that an `ON` inside a `WHEN (…)`
/// is not the clause, and stops at the first bare `on` at depth zero. The
/// grammar puts nothing else there —
/// `CREATE TRIGGER name { BEFORE | AFTER | INSTEAD OF } event … ON table` —
/// and `INSTEAD OF` is `OF`, not `ON`. Where it finds none, the caller refuses
/// rather than guessing, which is the direction a scan may be wrong in.
///
/// **A bare `on` cannot be anything but the keyword, and that is the engine's
/// rule rather than an assumption.** `ON` is reserved, so a column or schema
/// of that name must be quoted — measured:
///
/// ```text
/// CREATE TABLE ma.t1 (on int);   syntax error at or near "on"
/// CREATE SCHEMA on;              syntax error at or near "on"
/// CREATE TABLE ma.t2 (id int, "on" int);   accepted
/// ```
///
/// and a quoted identifier is stepped over whole here, so
/// `AFTER UPDATE OF "on" ON app.t` finds the clause and not the column.
fn the_table_the_body_is_on(definition: &str) -> Option<&str> {
    let mut depth = 0usize;
    let mut at = 0usize;
    while at < definition.len() {
        if let Some(skip) = skip_datum_at(definition, at) {
            at += skip;
            continue;
        }
        let c = definition[at..].chars().next()?;
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if pbps_dialect::continues_ident(c) && c != '"' => {
                let end = at
                    + definition[at..]
                        .find(|c: char| !pbps_dialect::continues_ident(c) || c == '"')
                        .unwrap_or(definition.len() - at);
                if depth == 0 && definition[at..end].eq_ignore_ascii_case("on") {
                    // Through the gap, not past the spaces: measured, `ON /*
                    // c */ app.t` creates the trigger on `app.t`.
                    return qualified_name_at(after_the_gap(&definition[end..]).0);
                }
                at = end;
                continue;
            }
            // A quoted identifier is never the keyword, and stepping over it
            // whole is what keeps a `"on"` inside a name from ending the scan.
            '"' => {
                let rest = qualified_name_at(&definition[at..])?;
                at += rest.len();
                continue;
            }
            _ => {}
        }
        at += c.len_utf8();
    }
    None
}

/// The qualified name at the front of `text`: `t`, `app.t`, `"App"."T"` — and
/// `app . t`, because the dot is a token of its own to this engine.
/// **Measured**, `ON app . orders`, and a comment or a line break on either
/// side of the dot, all create the trigger on `app.orders`; a scan that wanted
/// the dot glued to the name read `app` as the table and refused a valid
/// declaration.
///
/// Returns the slice the name occupies, the trivia inside it included, so a
/// caller can both read it and skip it. `None` where the text does not start
/// with one.
fn qualified_name_at(text: &str) -> Option<&str> {
    let mut at = one_ident_len(text)?;
    loop {
        // A Unicode-escaped part may carry its `UESCAPE 'x'` right after it,
        // and that clause is part of the name.
        if let Some(n) = uescape_len(after_the_gap(&text[at..]).0) {
            at = text.len() - after_the_gap(&text[at..]).0.len() + n;
        }
        let Some(after_dot) = after_the_gap(&text[at..]).0.strip_prefix('.') else {
            break;
        };
        let rest = after_the_gap(after_dot).0;
        let next = one_ident_len(rest)?;
        at = text.len() - rest.len() + next;
    }
    Some(&text[..at])
}

/// The length of a `UESCAPE 'x'` clause at the front of `text`, or `None`.
///
/// The engine takes any single character but a quote, a hex digit, `+` or
/// whitespace; a clause spelled otherwise is left to the engine to refuse.
fn uescape_len(text: &str) -> Option<usize> {
    let word = one_ident_len(text)?;
    if !text[..word].eq_ignore_ascii_case("uescape") {
        return None;
    }
    let rest = after_the_gap(&text[word..]).0;
    let mut chars = rest.chars();
    let (Some('\''), Some(escape), Some('\'')) = (chars.next(), chars.next(), chars.next()) else {
        return None;
    };
    if escape == '\'' || escape == '+' || escape.is_ascii_hexdigit() || escape.is_whitespace() {
        return None;
    }
    Some(text.len() - rest.len() + 2 + escape.len_utf8())
}

/// The `"…"` a Unicode-escaped identifier `U&"…"` wraps, or `None` where
/// `text` does not start with one. The prefix is glued to the quote: measured,
/// `U & "r11"` is a syntax error, `U&"r11".U&"\0074"`, `u&"r11"."t"` and
/// `U&"r11".U&"!0074" UESCAPE '!'` all create the trigger on `r11.t`.
fn unicode_quoted(text: &str) -> Option<&str> {
    (text.len() > 2
        && text.is_char_boundary(2)
        && text[..2].eq_ignore_ascii_case("u&")
        && text[2..].starts_with('"'))
    .then(|| &text[2..])
}

fn one_ident_len(text: &str) -> Option<usize> {
    if let Some(quoted) = unicode_quoted(text) {
        return one_ident_len(quoted).map(|n| n + 2);
    }
    if !text.starts_with('"') {
        let end = text
            .find(|c: char| !pbps_dialect::continues_ident(c) || c == '"')
            .unwrap_or(text.len());
        return (end > 0).then_some(end);
    }
    // Every index here is into `text`, which is the whole reason this is not
    // written against the tail after the opening quote: a first version mixed
    // the two and read `"app"."t"` as one nine-character name.
    let mut at = 1;
    loop {
        let close = at + text[at..].find('"')?;
        at = close + 1;
        // A doubled quote is a quote inside the name.
        if text[at..].starts_with('"') {
            at += 1;
        } else {
            return Some(at);
        }
    }
}

/// Whether the name a trigger's body puts after `ON` is the table its identity
/// names.
///
/// A bare name is the module's own schema, because that is what the write
/// scope puts first on the `search_path` — and the object it would find there
/// is the very table the identity names. An unquoted part folds to lower case
/// the way this engine folds one; a quoted part keeps what it holds.
fn names_the_same_table(named: &str, on: &TableName) -> bool {
    let mut parts = Vec::new();
    let mut rest = named;
    loop {
        let len = match one_ident_len(rest) {
            Some(len) => len,
            None => return false,
        };
        let part = &rest[..len];
        rest = after_the_gap(&rest[len..]).0;
        // A Unicode-escaped part reads with its own `UESCAPE`, or the default.
        let escape = match uescape_len(rest) {
            Some(n) => {
                // The clause is `UESCAPE 'x'`: the character before the
                // closing quote.
                let escape = rest[..n].chars().rev().nth(1).unwrap_or('\\');
                rest = after_the_gap(&rest[n..]).0;
                escape
            }
            None => '\\',
        };
        // A part this reader cannot decode is one it cannot be certain about,
        // and the gate refuses only what it is certain about: the catalog
        // assertion after the `CREATE` stands behind the rest.
        let Some(part) = unquoted(part, escape) else {
            return true;
        };
        parts.push(part);
        // The same gap `qualified_name_at` stepped through: it is inside the
        // slice that scan returned, so this reader of it steps through it too.
        match rest.strip_prefix('.') {
            Some(after) => rest = after_the_gap(after).0,
            None => break,
        }
    }
    match parts.as_slice() {
        [name] => *name == on.name,
        [schema, name] => *schema == on.schema && *name == on.name,
        _ => false,
    }
}

/// The name an identifier spells: an unquoted one folded the way this engine
/// folds it, a quoted one as written, a Unicode-escaped one decoded with its
/// escape character. `None` where an escape does not decode — a lone
/// surrogate, a digit that is not hex — which the engine refuses by name.
fn unquoted(ident: &str, escape: char) -> Option<String> {
    if let Some(quoted) = unicode_quoted(ident) {
        let inner = quoted
            .strip_prefix('"')?
            .strip_suffix('"')?
            .replace("\"\"", "\"");
        // The model's decoder: the one `RoutineArg` canonicalizes a key by (313).
        return pbps_model::module::decode_unicode_escapes(&inner, escape);
    }
    Some(
        ident
            .strip_prefix('"')
            .and_then(|i| i.strip_suffix('"'))
            .map_or_else(
                || ident.to_ascii_lowercase(),
                |inner| inner.replace("\"\"", "\""),
            ),
    )
}

/// The parameter list at the front of a routine's definition, without its
/// parentheses, or `None` where the text does not begin with one.
///
/// **Measured**, the list is not optional: `CREATE FUNCTION me.noparens
/// RETURNS int …` is `syntax error at or near "RETURNS"`. So a definition that
/// does not start with `(` is one the engine would refuse anyway, and this
/// answers `None` rather than reading the whole body as a parameter.
fn parameter_list(definition: &str) -> Option<&str> {
    // Through the gap, not merely the whitespace: a comment is whitespace to
    // this engine, so a definition opening with one still begins with its
    // parameter list.
    let body = after_the_gap(definition).0;
    if !body.starts_with('(') {
        return None;
    }
    let mut depth = 0usize;
    let mut at = 0usize;
    while at < body.len() {
        if let Some(skip) = skip_datum_at(body, at) {
            at += skip;
            continue;
        }
        let c = body[at..].chars().next()?;
        match c {
            '"' => {
                at += one_ident_len(&body[at..])?;
                continue;
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&body[1..at]);
                }
            }
            _ => {}
        }
        at += c.len_utf8();
    }
    None
}

/// One slice per parameter, split at the commas the list's own depth zero
/// puts between them — not at every comma, because `numeric(10, 2)` has one
/// inside it and `DEFAULT '=,)'` has one inside a literal.
fn parameters(list: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut at = 0usize;
    while at < list.len() {
        if let Some(skip) = skip_datum_at(list, at) {
            at += skip;
            continue;
        }
        let Some(c) = list[at..].chars().next() else {
            break;
        };
        match c {
            '"' => {
                if let Some(len) = one_ident_len(&list[at..]) {
                    at += len;
                    continue;
                }
            }
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(ascii_trim(&list[start..at]));
                start = at + 1;
            }
            _ => {}
        }
        at += c.len_utf8();
    }
    parts.push(ascii_trim(&list[start..]));
    if parts.len() == 1 && parts[0].is_empty() {
        return Vec::new();
    }
    parts
}

/// A parameter mode this engine writes, folded.
fn is_a_mode(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "in" | "out" | "inout" | "variadic"
    )
}

/// Whether the identity carries this parameter, and the text after its mode.
///
/// The mode may come **before or after the name**, and both are the engine's,
/// not a guess — measured:
///
/// ```text
/// CREATE FUNCTION mf.a(a out int) …   identity  mf.a()
/// CREATE FUNCTION me.o(out int) …     identity  me.o()
/// CREATE FUNCTION mf.c(c inout int) … identity  mf.c(integer)
/// ```
///
/// so `OUT` is the one mode a parameter can carry and stay out of
/// `proargtypes`, and `VARIADIC text[]` is carried as `text[]` (ADR-0009 §1).
fn after_the_mode(parameter: &str) -> (bool, &str) {
    // Through the gap first — and through every gap after it. A comment is
    // whitespace to this engine, and measured, `CREATE FUNCTION mo.c(/* note
    // */ OUT value integer)`, `(value /* note */ OUT integer)`, `(OUT /* note
    // */ value integer)` and a line comment between the name and the mode all
    // have the identity `mo.c()`. Left in, the mode is invisible, the
    // parameter counts as one the identity carries, and a correctly keyed
    // routine is refused for a count that is only wrong to this scan.
    let parameter = after_the_gap(parameter).0;
    let Some(first) = one_ident_len(parameter) else {
        return (true, parameter);
    };
    if is_a_mode(&parameter[..first]) {
        return (
            !parameter[..first].eq_ignore_ascii_case("out"),
            after_the_gap(&parameter[first..]).0,
        );
    }
    // `name mode type`: the name is read and thrown away, because what is left
    // is the type either way.
    let after_name = after_the_gap(&parameter[first..]).0;
    if let Some(second) = one_ident_len(after_name)
        && is_a_mode(&after_name[..second])
    {
        return (
            !after_name[..second].eq_ignore_ascii_case("out"),
            after_the_gap(&after_name[second..]).0,
        );
    }
    (true, parameter)
}

/// The text before a parameter's default, which is the type and maybe a name.
///
/// `DEFAULT` and `=` are the two spellings, and the scan is the lexical one
/// again: a `=` inside `DEFAULT '=,)'` is a byte of a literal, not the start
/// of one.
fn before_the_default(text: &str) -> &str {
    let mut depth = 0usize;
    let mut at = 0usize;
    while at < text.len() {
        if let Some(skip) = skip_datum_at(text, at) {
            at += skip;
            continue;
        }
        let Some(c) = text[at..].chars().next() else {
            break;
        };
        match c {
            '"' => {
                if let Some(len) = one_ident_len(&text[at..]) {
                    at += len;
                    continue;
                }
            }
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            '=' if depth == 0 => return ascii_trim_end(&text[..at]),
            _ if depth == 0 && pbps_dialect::continues_ident(c) && c != '"' => {
                let end = at
                    + text[at..]
                        .find(|c: char| !pbps_dialect::continues_ident(c) || c == '"')
                        .unwrap_or(text.len() - at);
                if text[at..end].eq_ignore_ascii_case("default") {
                    return ascii_trim_end(&text[..at]);
                }
                at = end;
                continue;
            }
            _ => {}
        }
        at += c.len_utf8();
    }
    ascii_trim_end(text)
}

/// Whether a type the body declares is **certainly not** the one the identity
/// carries.
///
/// Certainly, and not merely apparently, because this decides a refusal. Two
/// readings are tried, since a parameter with its mode off is `type` or
/// `name type` and the engine's own grammar offers nothing else; a reading
/// this dialect cannot parse as a type counts as agreement, because a scan
/// that cannot read a spelling has not learned it is wrong.
fn certainly_not(parameter: &str, identity: &RoutineArg) -> bool {
    let rest = without_trailing_trivia(before_the_default(after_the_mode(parameter).1));
    let whole = rest.parse::<RoutineArg>().ok();
    // A spelling this catalogue knows is the whole type, and the second
    // reading is not offered for it. Otherwise `(double precision)` is also
    // read as a parameter named `double` of type `precision` — and with a user
    // type of that name the gate accepts a body that creates
    // `f(double precision)` under the key `f(app.precision)`. Measured, the
    // engine does not offer that reading either: with `mq.precision` in the
    // database, `CREATE FUNCTION mq.b(double precision)` still creates
    // `mq.b(double precision)`.
    let split = if whole.as_ref().is_some_and(types::catalogued) {
        None
    } else {
        one_ident_len(rest).map(|n| after_the_gap(&rest[n..]).0)
    };
    let readings = [Some(rest), split];
    !readings.into_iter().flatten().any(|reading| {
        reading
            .parse::<RoutineArg>()
            .ok()
            .is_none_or(|declared| agrees(&declared, identity))
    })
}

/// Whether two spellings are the same type, **or could be**.
fn agrees(declared: &RoutineArg, identity: &RoutineArg) -> bool {
    let declared = types::routine_arg(declared);
    let identity = types::routine_arg(identity);
    if declared == identity {
        return true;
    }
    // A bare spelling in the body and a qualified one in the identity are one
    // type whenever the write path puts that schema in front of the name, and
    // this dialect cannot know whether it does: `format_type` under the empty
    // read path always qualifies a user type (ADR-0013 §3), so the identity is
    // written `md.my_type` while a hand-written body may say `my_type` and the
    // engine resolve it to the same thing. Refusing that would refuse a valid
    // plan, which is the one direction this gate may not be wrong in — and the
    // catalog assertion after the `CREATE` (ADR-0009 §3) is what covers the
    // case this cannot decide.
    let (declared, declared_array) = types::peel_array(declared.as_str());
    let (identity, identity_array) = types::peel_array(identity.as_str());
    declared_array == identity_array
        && !declared.is_empty()
        && !identity.is_empty()
        && (identity
            .strip_suffix(declared)
            .is_some_and(|schema| schema.ends_with('.'))
            || declared
                .strip_suffix(identity)
                .is_some_and(|schema| schema.ends_with('.')))
}

/// What the body's parameter list disagrees with the identity about.
///
/// The same shape as the trigger's `ON` check above and for the same reason:
/// the identity holds the argument types **and** the body holds the parameter
/// list, so the two can disagree and the engine accepts the disagreement
/// without a word. `CREATE FUNCTION app.f\n(x text) …` under the key
/// `app.f(integer)` creates `app.f(text)`; the key names an object that does
/// not exist, and every later plan creates it again and drops nothing.
///
/// Only a **certain** disagreement is refused. A count is always certain: an
/// `OUT` parameter is not in `proargtypes` and every other mode is, so the
/// number the body carries into the identity is a count this scan can take.
/// A type is certain only where both spellings are read; where one is a bare
/// name the write path may qualify, [`agrees`] says so and this says nothing,
/// and the catalog assertion after the `CREATE` (ADR-0009 §3) is what stands
/// behind it.
fn the_body_declares_the_identity(
    id: &ModuleId,
    definition: &str,
    args: &[RoutineArg],
) -> Vec<DialectError> {
    let Some(list) = parameter_list(definition) else {
        return vec![invalid(format!(
            "routine `{id}` has a definition that does not begin with a parameter list. On this \
             engine the emitter writes `CREATE FUNCTION {}` and the declaration writes everything \
             after the name, so the list is the body's — and **measured**, the engine requires \
             one: `CREATE FUNCTION f RETURNS int …` is a syntax error",
            id.object_name()
        ))];
    };
    let carried: Vec<&str> = parameters(list)
        .into_iter()
        .filter(|p| after_the_mode(p).0)
        .collect();
    if carried.len() != args.len() {
        return vec![invalid(format!(
            "routine `{id}` is declared with {} argument type(s), and its definition's parameter \
             list carries {} into the identity. On this engine a routine is its name and its \
             argument types (ADR-0009 §1), so the engine would create an object this key does not \
             name — and accept it without a word",
            args.len(),
            carried.len()
        ))];
    }
    carried
        .iter()
        .zip(args)
        .enumerate()
        .filter(|(_, (parameter, arg))| certainly_not(parameter, arg))
        .map(|(at, (parameter, arg))| {
            invalid(format!(
                "routine `{id}` names `{arg}` as argument {}, and its definition declares that \
                 parameter as `{parameter}`. The identity comes from the parameter list the \
                 definition holds, so the engine would create a different routine under this key \
                 and the next plan would not find the one named here",
                at + 1
            ))
        })
        .collect()
}

fn empty_definition(id: &ModuleId) -> DialectError {
    invalid(format!("module `{id}` has an empty definition"))
}

/// What a module declaration must satisfy before anything connects.
///
/// Each of these is a refusal the engine would otherwise make at `CREATE`
/// time — or, for the trigger's table, one it would **not** make at all.
pub(crate) fn validate_module(id: &ModuleId, module: &Module) -> Vec<DialectError> {
    let mut found = Vec::new();
    if ascii_trim(&module.definition).is_empty() {
        found.push(empty_definition(id));
    }
    // The names first: `quote` refuses an identifier over the engine's byte
    // limit, so a module this call passed would be one the emitter cannot
    // spell. `validate` is the command that exists to say so offline.
    if let Err(e) = quote(id.schema()).and_then(|_| quote(id.name())) {
        found.push(e);
    }
    match module.kind {
        ModuleKind::Function | ModuleKind::Procedure => match id.args() {
            // `signature` is the one that says what a routine without an
            // argument list costs, and it errs exactly here.
            None => found.extend(signature(id).err()),
            Some(args) => {
                // A name the engine would truncate: the routine is created
                // under the truncated identity and never found under this key
                // (see `overlong_name`).
                found.extend(args.iter().filter_map(types::overlong_name).map(|name| {
                    invalid(format!(
                        "module `{id}` names `{name}` in an argument type, and that name is                          over {} bytes. The engine truncates a longer identifier with a NOTICE                          nothing reads and creates the routine under the truncated identity,                          which is not this key: the next plan cannot find it, and the `CREATE`                          it emits again is refused as already existing",
                        crate::MAX_IDENT_BYTES
                    ))
                }));
                // An empty definition is already refused above; running the
                // parameter scan on it would say the same thing twice, in
                // worse words.
                if !ascii_trim(&module.definition).is_empty() {
                    found.extend(the_body_declares_the_identity(id, &module.definition, args));
                }
            }
        },
        ModuleKind::Trigger => match attached_to(id) {
            Err(e) => found.push(e),
            Ok(on) => {
                if let Err(e) = qualified(on) {
                    found.push(e);
                }
                // The body carries the `ON <table>` this engine's grammar puts
                // after the event list, so the identity and the text can
                // disagree — and the engine accepts the disagreement without a
                // word.
                match the_table_the_body_is_on(&module.definition) {
                    Some(named) if names_the_same_table(named, on) => {}
                    Some(named) => found.push(invalid(format!(
                        "trigger `{id}` is declared on `{on}`, and its definition puts it on \
                         `{named}`. On this engine the table is part of the statement the \
                         declaration holds — `CREATE TRIGGER {} AFTER INSERT ON {on} …` — so a \
                         definition naming another table creates the trigger there, under this \
                         key, and the next plan cannot find it",
                        id.name()
                    ))),
                    None => found.push(invalid(format!(
                        "trigger `{id}` has a definition this dialect cannot find an `ON \
                         <table>` in. That clause is what decides which table the trigger is \
                         created on, and it has to be the `{on}` this identity names — so a \
                         definition whose target cannot be read is refused rather than created \
                         somewhere this key does not point"
                    ))),
                }
            }
        },
        ModuleKind::View => {}
    }
    found
}

pub(crate) fn emit(pg: &Postgres, change: &Change, strategy: Strategy) -> Sql {
    match change {
        // The first statement is the one that brings the table into being; it
        // says so, and a staged checkpoint adopts the table from there.
        Change::CreateTable { name, table, .. } => {
            let mut out = create_table(pg, name, table)?;
            if let Some(first) = out.first_mut() {
                first.creates.push(Created::Table(name.clone()));
            }
            Ok(out)
        }

        Change::DropTable { name, .. } => {
            one(pg, name, format!("DROP TABLE {};", qualified(name)?))
        }

        // Two statements when both halves move, and neither engine has one that
        // does both: `RENAME TO` cannot cross a schema and `SET SCHEMA` cannot
        // rename. The transfer goes first, so the name in between is the old
        // one in the new schema — and each statement says what it does to the
        // name (`Statement::renaming`), because between them the table is
        // findable under neither the baseline's name nor the plan's.
        Change::RenameTable { from, to, .. } => rename_table(pg, from, to),

        // No `own_batch`, and that is the difference from T-SQL rather than an
        // omission: measured, `ALTER TABLE t ADD COLUMN c int; ALTER TABLE t
        // ADD CONSTRAINT ck CHECK (c > 0); CREATE INDEX ix ON t (c);` is
        // accepted as one batch here, because PostgreSQL analyses each
        // statement of a simple query when it reaches it. What it *does* read
        // up front is the whole batch's *lexis*, which is why
        // `standard_conforming_strings` is pinned by the framing and not here.
        Change::AddColumn {
            table,
            name,
            column,
            ..
        } => Ok(vec![
            on(
                pg,
                table,
                &format!(
                    "ALTER TABLE {} ADD COLUMN {};",
                    qualified(table)?,
                    column_definition(name, column)?
                ),
            )?
            .creating(Created::Column(table.clone(), name.clone())),
        ]),

        Change::DropColumn { column, .. } => one(
            pg,
            &column.table,
            format!(
                "ALTER TABLE {} DROP COLUMN {};",
                qualified(&column.table)?,
                quote(&column.name)?
            ),
        ),

        Change::RenameColumn {
            table, from, to, ..
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {};",
                qualified(table)?,
                quote(from)?,
                quote(to)?
            ),
        ),

        Change::AlterColumnType {
            column,
            from,
            to,
            from_nullable,
            to_nullable,
            ..
        } => {
            let normalized = types::normalize(to)?;
            let was = types::normalize(from)?;
            // No `USING`, ever (ADR-0012 §5). Where the engine would need one,
            // the change is refused here with the clause named, rather than
            // carried to the server to fail there — or, worse, performed under
            // a cast pbps chose, which is a data transformation nobody
            // declared, nobody reviewed and nobody can find in git.
            //
            // Both ends normalized first, and that is not a formality: the
            // catalogue's families are keyed on the spelling the engine gives
            // back, so `varchar(10)` unnormalized is a type it does not know
            // and every change from one reads as `Incompatible` — a widening
            // refused for needing a clause it does not need.
            if types::change_risk(&was, &normalized) == pbps_dialect::TypeChangeRisk::Incompatible {
                return Err(invalid(format!(
                    "column `{}` cannot be changed from `{}` to `{normalized}`: this engine \
                     refuses the conversion outright — `column \"{}\" cannot be cast \
                     automatically` — and its remedy is a `USING` clause, which pbps does not \
                     emit. A `USING` expression says what the data becomes, and that is a \
                     transformation to declare and review, not one for a tool to choose \
                     (ADR-0012 §5). Add the new column, fill it in a declared step, and drop the \
                     old one.",
                    column.name,
                    types::normalize(from)?,
                    column.name
                )));
            }
            // The same refusal, for the same reason, on a change the risk
            // classes cannot say no to. Gaining or losing the time zone is a
            // conversion the applying session's `TimeZone` decides — measured,
            // `12:00` becomes `12:00:00+00` from a `UTC` session and
            // `17:00:00+00` from `America/New_York` — so two operators running
            // one approved plan store two different instants. `change_risk`
            // knows the shape and answers `Narrowing`, but `Narrowing` is a
            // risk a human clears at the gate, and what the human cleared was
            // the loss, not a zone that was never in the plan. The framing
            // pins `TimeZone = UTC` for everything pbps applies, which makes
            // the result *reproducible* — it does not make it *declared*, and
            // a silent UTC reinterpretation of every stored value is exactly
            // the undeclared transformation ADR-0012 §5 refuses.
            if types::depends_on_the_session_time_zone(&was, &normalized) {
                return Err(invalid(format!(
                    "column `{}` cannot be changed from `{was}` to `{normalized}`: the \
                     conversion gains or loses the time zone, and what each stored value \
                     becomes is then read from a session setting rather than from anything \
                     declared — the same value converts to a different instant depending on \
                     the zone the applying session happens to hold. Say what the values mean \
                     instead: add the new column, fill it in a declared step with the zone \
                     written out (`AT TIME ZONE \'…\'`), and drop the old one.",
                    column.name
                )));
            }
            // One `ALTER TABLE` takes both subcommands (ADR-0011, Amendment 1),
            // and the nullability is restated only when it moves: unlike SQL
            // Server, a `TYPE` subcommand here leaves `NOT NULL` where it was,
            // so restating it always would put a line in plan.sql that changes
            // nothing.
            let mut parts = vec![format!(
                "ALTER COLUMN {} TYPE {normalized}",
                quote(&column.name)?
            )];
            if from_nullable != to_nullable {
                parts.push(format!(
                    "ALTER COLUMN {} {}",
                    quote(&column.name)?,
                    if *to_nullable {
                        "DROP NOT NULL"
                    } else {
                        "SET NOT NULL"
                    }
                ));
            }
            one(
                pg,
                &column.table,
                format!(
                    "ALTER TABLE {} {};",
                    qualified(&column.table)?,
                    parts.join(", ")
                ),
            )
        }

        // `ty` is carried for SQL Server, which restates the whole column
        // definition and reads an omitted `NULL` as nullable. This engine has a
        // subcommand for exactly this and needs no type, so the field is unused
        // here — deliberately, and not because it was missed.
        Change::AlterColumnNullability {
            column,
            to_nullable,
            ..
        } => one(
            pg,
            &column.table,
            format!(
                "ALTER TABLE {} ALTER COLUMN {} {};",
                qualified(&column.table)?,
                quote(&column.name)?,
                if *to_nullable {
                    "DROP NOT NULL"
                } else {
                    "SET NOT NULL"
                }
            ),
        ),

        // A default is not a named object here — it is a property of the column
        // — so there is nothing to drop by name and no generated constraint
        // name to guess. `SET DEFAULT` replaces whatever was there.
        Change::AlterColumnDefault { column, to, .. } => one(
            pg,
            &column.table,
            match to {
                Some(expr) => format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {};",
                    qualified(&column.table)?,
                    quote(&column.name)?,
                    verbatim(expr)
                ),
                None => format!(
                    "ALTER TABLE {} ALTER COLUMN {} DROP DEFAULT;",
                    qualified(&column.table)?,
                    quote(&column.name)?
                ),
            },
        ),

        // Deprecation is a fact about the declarations, not about the database.
        // `COMMENT ON` would make it one, and that is a decision with its own
        // round trip to design; until then the honest output is nothing at all
        // rather than a statement that pretends to do something.
        Change::SetColumnDeprecated { .. } => Ok(Vec::new()),

        Change::SetPrimaryKey { table, from, to } => {
            let mut out = Vec::new();
            if let Some(pk) = from {
                out.push(on(pg, table, &drop_primary_key(table, pk)?)?);
            }
            if let Some(pk) = to {
                out.push(on(
                    pg,
                    table,
                    &format!(
                        "ALTER TABLE {} ADD {};",
                        qualified(table)?,
                        primary_key_clause(pk)?
                    ),
                )?);
            }
            Ok(out)
        }

        // No concurrent path for a unique constraint, and that is measured
        // rather than assumed: this engine builds the backing index under an
        // `ACCESS EXCLUSIVE` lock, and the online spelling is a two-step —
        // `CREATE UNIQUE INDEX CONCURRENTLY` then `ADD CONSTRAINT … USING
        // INDEX` — whose halves commit separately. A plan that half-applied
        // would be a change the gate never approved, so the hint is dropped
        // (see `built_concurrently`) rather than honoured by splitting one
        // declared constraint into two committed steps.
        Change::AddUnique {
            table,
            name,
            constraint,
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} ADD {};",
                qualified(table)?,
                unique_clause(name, constraint)?
            ),
        ),

        Change::AddForeignKey {
            table,
            name,
            constraint,
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} ADD {};",
                qualified(table)?,
                foreign_key_clause(name, constraint)?
            ),
        ),

        Change::AddCheck {
            table,
            name,
            constraint,
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({});",
                qualified(table)?,
                quote(name)?,
                verbatim(&constraint.expression)
            ),
        ),

        Change::DropUnique { table, name }
        | Change::DropForeignKey { table, name }
        | Change::DropCheck { table, name } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} DROP CONSTRAINT {};",
                qualified(table)?,
                quote(name)?
            ),
        ),

        Change::AddIndex { table, name, index } => {
            let sql = create_index(table, name, index, strategy)?;
            if built_concurrently(index, strategy) {
                // Alone in its batch and outside the transaction, because the
                // engine says so in as many words: `CREATE INDEX CONCURRENTLY
                // cannot run inside a transaction block`. Saying it here rather
                // than in the runner is what lets a plan carrying one be
                // refused at plan time, with the whole plan intact, instead of
                // halfway through an apply.
                Ok(vec![Statement::new(sql).own_batch().non_transactional()])
            } else {
                Ok(vec![on(pg, table, &sql)?])
            }
        }

        // No `CONCURRENTLY` on the drop, deliberately. It would make the
        // statement non-transactional — and so the whole plan — to spare an
        // `ACCESS EXCLUSIVE` lock held for a catalog update, which is the one
        // part of an index's life that is not proportional to the table.
        Change::DropIndex { table, name } => one(
            pg,
            table,
            format!("DROP INDEX {}.{};", quote(&table.schema)?, quote(name)?),
        ),

        // The mode is a property of the declaration, not of the database: it
        // decides what future plans do about undeclared rows. The row changes
        // it implies are separate entries in this same plan.
        Change::SetDataMode { .. } => Ok(Vec::new()),

        // A module is created, never replaced. `CREATE OR REPLACE` exists on
        // this engine and buys nothing: measured, it refuses a changed return
        // type and a reordered view column, and it drops `reloptions` just as a
        // rebuild does — so the cheap path is neither cheap nor complete, and
        // *which* edits it can express is decided by text §8.2 forbids parsing.
        // One shape, always (ADR-0009 §3).
        Change::CreateModule { id, module } => Ok(vec![create_module(pg, id, module)?]),

        // Two statements, not one, so that the plan a human approves says
        // `DROP` where a `DROP` will run. Everything the catalog attached to
        // the old object goes with it, and carrying that across is the
        // connected plan's obligation, not the emitter's: only a connection can
        // see an ACL, an owner or a `reloptions` (ADR-0009 §3).
        Change::AlterModule { id, module } => Ok(vec![
            drop_module(pg, id, module.kind)?,
            create_module(pg, id, module)?,
        ]),

        Change::DropModule { id, kind } => Ok(vec![drop_module(pg, id, *kind)?]),
        Change::CreateRole { .. }
        | Change::DropRole { .. }
        | Change::RenameRole { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. } => Err(Unbuilt::Roles.refuse()),
        Change::InsertRow { .. } | Change::UpdateRow { .. } | Change::DeleteRow { .. } => {
            Err(Unbuilt::ReferenceData.refuse())
        }
    }
}

fn rename_table(pg: &Postgres, from: &TableName, to: &TableName) -> Sql {
    let mut out = Vec::new();
    let mut at = from.clone();
    if from.schema != to.schema {
        let moved = TableName::new(&to.schema, &from.name);
        out.push(
            on(
                pg,
                &at,
                &format!(
                    "ALTER TABLE {} SET SCHEMA {};",
                    qualified(&at)?,
                    quote(&to.schema)?
                ),
            )?
            .renaming(at.clone(), moved.clone()),
        );
        at = moved;
    }
    if at.name != to.name {
        out.push(
            on(
                pg,
                &at,
                &format!(
                    "ALTER TABLE {} RENAME TO {};",
                    qualified(&at)?,
                    quote(&to.name)?
                ),
            )?
            .renaming(at.clone(), to.clone()),
        );
    }
    Ok(out)
}

fn create_table(pg: &Postgres, name: &TableName, table: &Table) -> Sql {
    if table.columns.is_empty() {
        return Err(invalid(format!("table `{name}` has no columns")));
    }
    let q = qualified(name)?;

    let mut body: Vec<String> = Vec::new();
    for (col_name, column) in &table.columns {
        body.push(column_definition(col_name, column)?);
    }
    // The primary key goes inline; every other constraint is added afterwards,
    // so that creating a table and altering one take the same code path and
    // cannot drift apart.
    if let Some(pk) = &table.primary_key {
        body.push(primary_key_clause(pk)?);
    }

    // `USING heap` is written out, not left to `default_table_access_method`.
    // The reader accepts a table only when `relam` is heap (`catalog.rs`), so a
    // table created under a role whose setting names another installed method
    // is created *successfully* and then read back as an unsupported object:
    // absent from the pulled schema, planned as a `CREATE` that the engine
    // refuses for already existing, and the deployment cannot converge. The
    // apply reports success and the recording says the table is as declared.
    //
    // In the statement rather than in the transaction framing beside the other
    // session pins (DECISIONS 267), because this one *can* be said in the
    // statement: a clause cannot be defeated by any session, on any path,
    // including the rendered `--sql` script an operator runs through `psql`
    // outside the framing (issue #174). A pin would leave that path open.
    //
    // Not measured end to end, and the reason is worth the line: the pinned
    // image ships exactly one table access method, so there is no second one
    // to create a divergent table with. What is measured is each half — the
    // setting exists and is validated against the installed methods, and
    // `USING heap` fixes `relam` — and the reader's rule is code, not a guess.
    let mut out = vec![on(
        pg,
        name,
        &format!(
            "CREATE TABLE {q} (\n    {}\n) USING heap;",
            body.join(",\n    ")
        ),
    )?];

    for (n, u) in &table.unique {
        out.push(on(
            pg,
            name,
            &format!("ALTER TABLE {q} ADD {};", unique_clause(n, u)?),
        )?);
    }
    for (n, c) in &table.checks {
        out.push(on(
            pg,
            name,
            &format!(
                "ALTER TABLE {q} ADD CONSTRAINT {} CHECK ({});",
                quote(n)?,
                verbatim(&c.expression)
            ),
        )?);
    }
    for (n, fk) in &table.foreign_keys {
        out.push(on(
            pg,
            name,
            &format!("ALTER TABLE {q} ADD {};", foreign_key_clause(n, fk)?),
        )?);
    }
    for (n, idx) in &table.indexes {
        // No online build here: the table was created by the statement above it
        // and holds no rows, so there is nothing for a concurrent build to
        // spare — and a concurrent one could not share this transaction.
        out.push(on(
            pg,
            name,
            &create_index(name, n, idx, Strategy::default())?,
        )?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_dialect::Dialect;
    use pbps_model::{CheckConstraint, ColumnRef, ColumnType, Identity, IndexColumn, Uid, UidKind};

    fn ty(s: &str) -> ColumnType {
        s.parse().expect("a type parses")
    }

    fn name(schema: &str, table: &str) -> TableName {
        TableName::new(schema, table)
    }

    fn sql_of(pg: &Postgres, change: &Change) -> Vec<String> {
        pg.emit(change, Strategy::default())
            .expect("emit")
            .into_iter()
            .map(|s| s.sql)
            .collect()
    }

    fn module(kind: ModuleKind, definition: &str) -> Module {
        Module {
            kind,
            description: None,
            definition: definition.to_owned(),
        }
    }

    fn id(s: &str) -> ModuleId {
        s.parse().expect("a module id parses")
    }

    /// The routine half of the same rule the trigger's `ON` check enforces:
    /// the identity and the body both carry the argument types, and the engine
    /// creates whatever the body says under whatever key the declarations use.
    #[test]
    fn a_parameter_list_that_creates_another_identity_is_refused_offline() {
        for (key, definition) in [
            // The shape the review found: same count, different type.
            (
                "app.f(integer)",
                "(x text) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // A second parameter the identity does not carry.
            (
                "app.f(integer)",
                "(a integer, b text) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // None at all where the identity carries one.
            (
                "app.f(integer)",
                "() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // `OUT` is the one mode that keeps a parameter out of the
            // identity, so this list carries nothing into it.
            (
                "app.f(integer)",
                "(a out integer) LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // The modifier goes, and `numeric(10,2)` is still not `integer`.
            (
                "app.f(integer)",
                "(a numeric(10, 2)) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // Two qualified spellings that are both certain and differ.
            (
                "app.f(md.my_type)",
                "(a other.my_type) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // A spelling the catalogue knows is the whole type. Split again it
            // reads as a parameter named `double` of type `precision`, and
            // with a user type of that name the qualification rule would let
            // `app.precision` through — while the engine creates
            // `f(double precision)`. Measured: it does not offer that reading.
            (
                "app.f(app.precision)",
                "(double precision) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // The default is cut at the `=` outside the literal, and what is
            // left is still the wrong type.
            (
                "app.f(integer)",
                "(a text = ')') RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // Measured: the engine requires the list, so a body without one is
            // not a routine this dialect can create under any key.
            (
                "app.f(integer)",
                "RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
        ] {
            let found = Postgres::new()
                .validate_module(&id(key), &module(ModuleKind::Function, definition));
            assert!(
                !found.is_empty(),
                "`{key}` with `{definition}` was accepted, and the engine would create another \
                 object under this key"
            );
        }
    }

    /// A type name over the engine's byte limit in the argument list is
    /// refused offline: measured, the engine truncates it, creates the routine
    /// under the truncated identity, and refuses the same `CREATE` the next
    /// time — so a plan that was applied once is refused ever after. A name
    /// at the limit passes.
    #[test]
    fn an_argument_type_the_engine_would_truncate_is_refused_before_it_is_created() {
        let at = "t".repeat(crate::MAX_IDENT_BYTES);
        let over = format!("{at}x");
        for (key, definition) in [
            (
                format!("app.f(app.{over})"),
                format!("(a app.{over}) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"),
            ),
            (
                format!("app.p(app.\"{over}\"[])"),
                format!("(a app.\"{over}\"[]) LANGUAGE sql AS $$ SELECT 1 $$"),
            ),
        ] {
            let kind = if key.starts_with("app.p") {
                ModuleKind::Procedure
            } else {
                ModuleKind::Function
            };
            let found = Postgres::new().validate_module(&id(&key), &module(kind, &definition));
            assert!(
                found
                    .iter()
                    .any(|e| e.to_string().contains("over 63 bytes")),
                "`{key}` was accepted, and the engine would truncate it: {found:?}"
            );
        }
        let key = format!("app.f(app.{at})");
        let found = Postgres::new().validate_module(
            &id(&key),
            &module(
                ModuleKind::Function,
                &format!("(a app.{at}) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"),
            ),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    /// The other half, which is the one that decides whether the gate is
    /// usable: every spelling the engine accepts for the declared identity
    /// passes, including the ones a scan could mistake for a disagreement.
    #[test]
    fn a_parameter_list_that_creates_the_declared_identity_is_accepted() {
        for (key, definition) in [
            (
                "app.f(integer)",
                "(x integer) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // No name at all, which is a parameter list of one type.
            (
                "app.f(integer)",
                "(integer) RETURNS int LANGUAGE sql AS $$ SELECT $1 $$",
            ),
            // `$` continues a name: measured, `foo$tag$` is one parameter
            // name and not `foo` followed by a literal that never closes.
            (
                "app.f(integer)",
                "(foo$tag$ integer) RETURNS int LANGUAGE sql AS $$ SELECT foo$tag$ $$",
            ),
            (
                "app.f(integer, text)",
                "(a$ integer, b$c$ text DEFAULT $x$a$x$) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // A spelling the engine folds to the identity's.
            (
                "app.f(integer)",
                "(a int DEFAULT 3) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // The modifier is discarded from every routine argument.
            (
                "app.f(numeric)",
                "(a numeric(10, 2)) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            (
                "app.f(character varying)",
                "(a character varying(5)) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // A two-word type with a name in front of it, which the two
            // readings are there for.
            (
                "app.f(double precision)",
                "(a double precision) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // And without one.
            (
                "app.f(double precision)",
                "(double precision) RETURNS int LANGUAGE sql AS $$ SELECT $1 $$",
            ),
            // `OUT` is not in the identity; every other mode is.
            ("app.f()", "(out x text) LANGUAGE sql AS $$ SELECT 'q' $$"),
            (
                "app.f(integer)",
                "(a integer, out b text) LANGUAGE sql AS $$ SELECT 'q' $$",
            ),
            (
                "app.f(integer)",
                "(a inout integer) LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            (
                "app.f(text[])",
                "(variadic a text[]) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // Measured: the mode may follow the name as well as precede it.
            ("app.f()", "(a out integer) LANGUAGE sql AS $$ SELECT 1 $$"),
            // A comment is whitespace to this engine, inside the list as much
            // as before it: measured, `mo.c(/* note */ OUT value integer)` has
            // the identity `mo.c()`. Read as code it hides the mode, and a
            // correctly keyed routine is refused for a count only this scan
            // gets wrong.
            (
                "app.f()",
                "(/* note */ out value integer) LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            (
                "app.f(integer)",
                "(-- which one\n a integer) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // And in every gap after the first one: measured, each of these
            // has the identity `()` — or `(integer)` for the `IN`.
            (
                "app.f()",
                "(value /* note */ out integer) LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            (
                "app.f()",
                "(out /* note */ value integer) LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            (
                "app.f()",
                "(value out /* note */ integer) LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            (
                "app.f()",
                "(value\n-- line comment\nout integer) LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            (
                "app.f(integer)",
                "(in /* note */ x /* note */ int) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // A non-breaking space is the last byte of the type's name, not
            // whitespace before the comma: measured, the identity is
            // `f(md."x\u{a0}",integer)`.
            (
                "app.f(md.x\u{a0}, integer)",
                "(a md.x\u{a0}, b integer) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            (
                "app.f(md.x\u{a0})",
                "(a md.x\u{a0} DEFAULT NULL) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            (
                "app.f(integer)",
                "(a integer /* trailing */) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // A bare name the write path may qualify to the identity's — this
            // dialect cannot know whether it does, so it does not refuse.
            (
                "app.f(md.my_type)",
                "(a my_type) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            (
                "app.f(md.my_type[])",
                "(a my_type[]) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // A quoted name with a comma in it is one parameter, not two.
            (
                "app.f(integer)",
                "(\"a,b\" integer) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // The comma inside a literal default is not a separator either.
            (
                "app.f(integer,text)",
                "(a integer, b text DEFAULT ',)') RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // Nothing on either side.
            ("app.f()", "() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"),
            // A leading comment is not a missing parameter list.
            (
                "app.f(integer)",
                "/* the id */ (a integer) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
        ] {
            let found = Postgres::new()
                .validate_module(&id(key), &module(ModuleKind::Function, definition));
            assert!(
                found.is_empty(),
                "`{key}` with `{definition}` was refused: {:?}",
                found.iter().map(ToString::to_string).collect::<Vec<_>>()
            );
        }
    }

    /// A scan that cannot read a spelling has not learned that it is wrong,
    /// and a refusal it makes anyway refuses a plan the engine would accept.
    #[test]
    fn a_parameter_this_scan_cannot_read_is_not_read_as_a_disagreement() {
        for (key, definition) in [
            // A type spelling `RoutineArg` does not admit, in a body the
            // engine is perfectly happy with.
            (
                "app.f(integer)",
                "(a t%rowtype) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            // A quoted type name the catalogue has never heard of.
            (
                "app.f(\"odd type\")",
                "(a \"odd type\") RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
            ),
        ] {
            let found = Postgres::new()
                .validate_module(&id(key), &module(ModuleKind::Function, definition));
            assert!(
                found.is_empty(),
                "`{key}` with `{definition}` was refused on a spelling this dialect cannot read: \
                 {:?}",
                found.iter().map(ToString::to_string).collect::<Vec<_>>()
            );
        }
    }

    /// Each prefix ends where this engine's grammar puts the declaration's
    /// first word, and a trigger's is the one that is not the SQL Server
    /// shape — measured, `CREATE TRIGGER m1.audit` is a syntax error and the
    /// table comes after the event list.
    #[test]
    fn each_kind_is_created_with_the_prefix_its_grammar_allows() {
        let pg = Postgres::new();
        let cases = [
            (
                id("app.v"),
                module(ModuleKind::View, "SELECT id FROM app.t"),
                "CREATE VIEW \"app\".\"v\" AS\nSELECT id FROM app.t",
            ),
            // ASCII whitespace is trimmed off the body; a non-breaking space
            // is an identifier byte and stays — measured, it names the column.
            (
                id("app.v"),
                module(ModuleKind::View, " \n SELECT 1 AS x\u{a0}\n "),
                "CREATE VIEW \"app\".\"v\" AS\nSELECT 1 AS x\u{a0}",
            ),
            (
                id("app.f(integer)"),
                module(
                    ModuleKind::Function,
                    "(a integer) RETURNS integer AS $$ SELECT a $$",
                ),
                "CREATE FUNCTION \"app\".\"f\"\n(a integer) RETURNS integer AS $$ SELECT a $$",
            ),
            (
                id("app.p(integer)"),
                module(
                    ModuleKind::Procedure,
                    "(a integer) LANGUAGE sql AS $$ SELECT 1 $$",
                ),
                "CREATE PROCEDURE \"app\".\"p\"\n(a integer) LANGUAGE sql AS $$ SELECT 1 $$",
            ),
            (
                id("app.t.audit"),
                module(
                    ModuleKind::Trigger,
                    "AFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.trf()",
                ),
                "CREATE TRIGGER \"audit\"\nAFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION \
                 app.trf()",
            ),
        ];
        for (id, m, expected) in cases {
            let sql = sql_of(
                &pg,
                &Change::CreateModule {
                    id: id.clone(),
                    module: Box::new(m),
                },
            );
            assert_eq!(
                sql,
                vec![format!(
                    "SET search_path = \"app\";\n{expected}\n;\nRESET search_path;"
                )],
                "{id}"
            );
        }
    }

    /// `CREATE OR REPLACE` is never written, so a change is two statements and
    /// the plan a human approves says `DROP` where a `DROP` will run
    /// (ADR-0009 §3).
    #[test]
    fn a_module_change_is_a_drop_and_a_create_and_never_a_replace() {
        let pg = Postgres::new();
        let sql = sql_of(
            &pg,
            &Change::AlterModule {
                id: id("app.f(integer, text)"),
                module: Box::new(module(
                    ModuleKind::Function,
                    "(a integer, b text) RETURNS int",
                )),
            },
        );
        assert_eq!(sql.len(), 2, "{sql:?}");
        assert!(
            sql[0].contains("DROP FUNCTION \"app\".\"f\"(integer, text);"),
            "{}",
            sql[0]
        );
        assert!(
            sql[1].contains("CREATE FUNCTION \"app\".\"f\""),
            "{}",
            sql[1]
        );
        assert!(
            !sql.iter().any(|s| s.to_uppercase().contains("OR REPLACE")),
            "{sql:?}"
        );
    }

    /// The name alone is not the object where routines overload — measured,
    /// `DROP FUNCTION m3.f` is refused as not unique — and a trigger's name is
    /// not an object at all without its table.
    #[test]
    fn a_drop_names_exactly_one_object_on_an_engine_that_overloads() {
        let pg = Postgres::new();
        let cases = [
            (id("app.v"), ModuleKind::View, "DROP VIEW \"app\".\"v\";"),
            (
                id("app.f(character varying, integer[])"),
                ModuleKind::Function,
                "DROP FUNCTION \"app\".\"f\"(character varying, integer[]);",
            ),
            (
                id("app.f()"),
                ModuleKind::Procedure,
                "DROP PROCEDURE \"app\".\"f\"();",
            ),
            (
                id("app.t.audit"),
                ModuleKind::Trigger,
                "DROP TRIGGER \"audit\" ON \"app\".\"t\";",
            ),
        ];
        for (id, kind, expected) in cases {
            let sql = sql_of(
                &pg,
                &Change::DropModule {
                    id: id.clone(),
                    kind,
                },
            );
            assert_eq!(sql.len(), 1, "{id}");
            assert!(sql[0].contains(expected), "{id}: {}", sql[0]);
        }
    }

    /// SPEC 14.3: the plan names every object it drops, or it does not drop.
    /// `CASCADE` is the shortest way out of every ADR-0009 §4 refusal and it
    /// destroys objects nobody reviewed, so no path here writes it.
    #[test]
    fn no_module_statement_offers_cascade() {
        let pg = Postgres::new();
        let changes = [
            Change::DropModule {
                id: id("app.v"),
                kind: ModuleKind::View,
            },
            Change::DropModule {
                id: id("app.f(integer)"),
                kind: ModuleKind::Function,
            },
            Change::DropModule {
                id: id("app.t.audit"),
                kind: ModuleKind::Trigger,
            },
            Change::AlterModule {
                id: id("app.v"),
                module: Box::new(module(ModuleKind::View, "SELECT 1")),
            },
        ];
        for change in changes {
            for sql in sql_of(&pg, &change) {
                assert!(!sql.to_uppercase().contains("CASCADE"), "{sql}");
            }
        }
    }

    /// The table is in the identity *and* in the text this engine's grammar
    /// requires, and the engine accepts a disagreement between them: a trigger
    /// created on another table sits under this key until a `DROP` a plan
    /// later cannot find it.
    /// The escapes the engine reads a `U&"…"` identifier by, and the ones it
    /// refuses — which this reader does not decide, so that a name it cannot
    /// read is left to the engine rather than compared wrongly.
    #[test]
    fn a_unicode_escaped_identifier_decodes_the_way_the_engine_reads_it() {
        for (written, name) in [
            ("U&\"d\\0061t\\+000061\"", "data"),
            ("U&\"a\\\\b\"", "a\\b"),
            ("U&\"\\D83D\\DE00\"", "😀"),
            ("u&\"a\"\"b\"", "a\"b"),
            ("U&\"Ätype\"", "Ätype"),
        ] {
            assert_eq!(unquoted(written, '\\').as_deref(), Some(name), "{written}");
        }
        assert_eq!(unquoted("U&\"!0074\"", '!').as_deref(), Some("t"));
        for malformed in [
            "U&\"\\00G1\"",
            "U&\"\\D83D\"",
            "U&\"\\DE00\"",
            "U&\"\\D83Dx\"",
        ] {
            assert_eq!(unquoted(malformed, '\\'), None, "{malformed}");
        }
        assert_eq!(uescape_len("UESCAPE '!' FOR"), Some(11));
        assert_eq!(uescape_len("uescape  '!'"), Some(12));
        for not_one in [
            "UESCAPE '+'",
            "UESCAPE 'a'",
            "UESCAPE ''",
            "UESCAPED '!'",
            "ON app.t",
        ] {
            assert_eq!(uescape_len(not_one), None, "{not_one}");
        }
    }

    #[test]
    fn a_trigger_whose_body_names_another_table_is_refused_offline() {
        // What decides the outcome is the name after `ON`, and nothing else.
        // A scan for the identity's table *anywhere* in the text says yes to
        // the second of these — the column list mentions `t` — and the engine
        // then creates the trigger on `app.other` under this key.
        for body in [
            "AFTER INSERT ON app.other FOR EACH ROW EXECUTE FUNCTION app.trf()",
            "AFTER UPDATE OF t ON app.other FOR EACH ROW EXECUTE FUNCTION app.trf()",
            "AFTER INSERT ON other FOR EACH ROW EXECUTE FUNCTION app.trf()",
            "AFTER INSERT ON elsewhere.t FOR EACH ROW EXECUTE FUNCTION app.trf()",
            // Trivia around the dot changes nothing about which table it is.
            "AFTER INSERT ON app . other FOR EACH ROW EXECUTE FUNCTION app.trf()",
            // Nor does spelling the other table with Unicode escapes.
            "AFTER INSERT ON U&\"app\".U&\"\\006Fther\" FOR EACH ROW EXECUTE FUNCTION app.trf()",
        ] {
            let found = Postgres::new()
                .validate_module(&id("app.t.audit"), &module(ModuleKind::Trigger, body));
            assert_eq!(found.len(), 1, "{body}: {found:?}");
            assert!(
                found[0].to_string().contains("app.t"),
                "{body}: {}",
                found[0]
            );
        }

        // And a definition with no readable `ON` at all is refused rather than
        // guessed at: the clause decides where the object is created.
        for body in [
            "FOR EACH ROW EXECUTE FUNCTION app.trf()",
            // Every `on` here is inside something: a literal, a comment, and
            // the parenthesised `WHEN`.
            "AFTER INSERT /* on app.t */ WHEN (new.a = 'on app.t') FOR EACH ROW EXECUTE \
             FUNCTION app.trf()",
        ] {
            let found = Postgres::new()
                .validate_module(&id("app.t.audit"), &module(ModuleKind::Trigger, body));
            assert_eq!(found.len(), 1, "{body}: {found:?}");
            assert!(
                found[0].to_string().contains("cannot find an `ON"),
                "{body}: {}",
                found[0]
            );
        }

        // Qualified or bare, a definition that does name its table passes: a
        // declaration written inside its own schema very often omits the
        // qualifier, and refusing that would refuse valid work. Case folds the
        // way this engine folds an unquoted name, and a quoted one is taken as
        // it is written.
        for body in [
            "AFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.trf()",
            "AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION trf()",
            "AFTER INSERT ON APP.T FOR EACH ROW EXECUTE FUNCTION app.trf()",
            "AFTER INSERT ON \"app\".\"t\" FOR EACH ROW EXECUTE FUNCTION app.trf()",
            "AFTER UPDATE OF other ON app.t FOR EACH ROW EXECUTE FUNCTION app.trf()",
            // A column called `on` has to be quoted — `ON` is reserved, and
            // the engine refuses the unquoted spelling — so a quoted one is
            // stepped over whole and the clause after it is the one found.
            "AFTER UPDATE OF \"on\" ON app.t FOR EACH ROW EXECUTE FUNCTION app.trf()",
            "AFTER UPDATE OF \"a on b\" ON app.t FOR EACH ROW EXECUTE FUNCTION app.trf()",
            // The dot is a token of its own: measured, the engine takes trivia
            // on either side of it and creates the trigger on `app.t`.
            "AFTER INSERT ON app . t FOR EACH ROW EXECUTE FUNCTION app.trf()",
            "AFTER INSERT ON app /* schema */ .\n  t FOR EACH ROW EXECUTE FUNCTION app.trf()",
            "AFTER INSERT ON \"app\" . \"t\" FOR EACH ROW EXECUTE FUNCTION app.trf()",
            // And the gap after `ON` itself is a gap.
            "AFTER INSERT ON /* c */ app.t FOR EACH ROW EXECUTE FUNCTION app.trf()",
            "AFTER INSERT ON\n-- c\napp.t FOR EACH ROW EXECUTE FUNCTION app.trf()",
            // A Unicode-escaped identifier is one identifier, read with its
            // escape: measured, each of these lands on `app.t`.
            "AFTER INSERT ON U&\"app\".U&\"\\0074\" FOR EACH ROW EXECUTE FUNCTION app.trf()",
            "AFTER INSERT ON u&\"app\".\"t\" FOR EACH ROW EXECUTE FUNCTION app.trf()",
            "AFTER INSERT ON U&\"app\".U&\"!0074\" UESCAPE '!' FOR EACH ROW EXECUTE FUNCTION \
             app.trf()",
            "AFTER INSERT ON U&\"\\0061pp\" UESCAPE '\\'.U&\"\\+000074\" FOR EACH ROW EXECUTE \
             FUNCTION app.trf()",
            // The keyword is found past a literal and a comment that both
            // contain something that looks like one.
            "AFTER INSERT -- on app.other\nON app.t FOR EACH ROW EXECUTE FUNCTION app.trf()",
            "AFTER INSERT ON app.t FOR EACH ROW WHEN (new.a = 'on app.other') EXECUTE \
             FUNCTION app.trf()",
        ] {
            assert!(
                Postgres::new()
                    .validate_module(&id("app.t.audit"), &module(ModuleKind::Trigger, body))
                    .is_empty(),
                "{body}"
            );
        }
    }

    /// An empty definition is a module that would emit `CREATE VIEW app.v AS`
    /// and nothing else. Refused where a user is looking at the declaration,
    /// and again at emit time, because the emitter is handed a change and not
    /// a schema.
    #[test]
    fn a_module_with_no_definition_is_refused_by_both_gates() {
        let empty = module(ModuleKind::View, "  \n ");
        let found = Postgres::new().validate_module(&id("app.v"), &empty);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].to_string().contains("empty definition"),
            "{}",
            found[0]
        );
        let refused = Postgres::new().emit(
            &Change::CreateModule {
                id: id("app.v"),
                module: Box::new(empty),
            },
            Strategy::default(),
        );
        assert!(refused.is_err(), "an empty definition emitted a statement");
    }

    /// The scope is the object's own schema first and the extras after it, in
    /// the order they were configured — because the order is what a name
    /// resolves through, and ADR-0013 measured a view keeping the binding its
    /// creation order gave it. First among the *listed* schemas: `pg_catalog`
    /// is searched ahead of all of them because the path does not name it, and
    /// DECISIONS 276 says why it is left out.
    #[test]
    fn every_statement_sets_the_write_path_and_gives_it_back() {
        let pg = Postgres::with_write_path_extras(vec!["shared".into(), "public".into()]);
        let sql = sql_of(
            &pg,
            &Change::DropTable {
                uid: Uid::generate(UidKind::Table),
                name: name("app", "t"),
            },
        );
        assert_eq!(
            sql,
            vec![
                "SET search_path = \"app\", \"shared\", \"public\";\n\
                 DROP TABLE \"app\".\"t\";\n\
                 RESET search_path;"
            ]
        );
    }

    /// With nothing configured the path is the object's own schema alone, which
    /// is the value every project has until there is a key to set.
    #[test]
    fn the_path_of_an_unconfigured_project_is_the_objects_own_schema() {
        let sql = sql_of(
            &Postgres::new(),
            &Change::DropIndex {
                table: name("app", "t"),
                name: "ix".into(),
            },
        );
        assert_eq!(
            sql,
            vec!["SET search_path = \"app\";\nDROP INDEX \"app\".\"ix\";\nRESET search_path;"]
        );
    }

    /// `$user` is refused wherever it would enter the path — as the table's own
    /// schema and as a configured extra — because the engine substitutes it
    /// rather than reading it as a name.
    #[test]
    fn the_one_schema_a_path_cannot_name_is_refused_from_either_end() {
        let drop_it = |pg: &Postgres, schema: &str| {
            pg.emit(
                &Change::DropTable {
                    uid: Uid::generate(UidKind::Table),
                    name: name(schema, "t"),
                },
                Strategy::default(),
            )
        };
        for (pg, schema) in [
            (Postgres::new(), "$user"),
            (
                Postgres::with_write_path_extras(vec!["$user".into()]),
                "app",
            ),
        ] {
            let refusal = drop_it(&pg, schema).expect_err("the path would not mean this");
            assert!(refusal.to_string().contains("$user"), "{refusal}");
        }
        // A name that merely contains it, or differs in case, is an ordinary
        // schema: the engine compares the whole entry, exactly.
        for (pg, schema) in [
            (Postgres::new(), "$users"),
            (Postgres::new(), "$USER"),
            (Postgres::with_write_path_extras(vec!["user".into()]), "app"),
        ] {
            assert!(
                drop_it(&pg, schema).is_ok(),
                "`{schema}` is an ordinary name"
            );
        }
    }

    /// Listing `pg_catalog` is not the same statement as leaving it out, so an
    /// extra of that name is refused rather than dropped: dropping it would
    /// leave the caller believing a path they never got.
    #[test]
    fn the_one_schema_a_path_must_leave_out_is_refused_wherever_it_is_written() {
        let drop_it = |pg: &Postgres, schema: &str| {
            pg.emit(
                &Change::DropTable {
                    uid: Uid::generate(UidKind::Table),
                    name: name(schema, "t"),
                },
                Strategy::default(),
            )
        };
        for (pg, schema) in [
            (Postgres::new(), "pg_catalog"),
            (
                Postgres::with_write_path_extras(vec!["pg_catalog".into()]),
                "app",
            ),
            (
                Postgres::with_write_path_extras(vec!["shared".into(), "pg_catalog".into()]),
                "app",
            ),
        ] {
            let refusal = drop_it(&pg, schema).expect_err("the path would not mean this");
            assert!(refusal.to_string().contains("pg_catalog"), "{refusal}");
        }
        // A name that merely contains it is an ordinary schema, and a path
        // that leaves it out is the ordinary case this refusal protects.
        for (pg, schema) in [
            (Postgres::new(), "pg_catalogue"),
            (
                Postgres::with_write_path_extras(vec!["shared".into()]),
                "app",
            ),
        ] {
            assert!(
                drop_it(&pg, schema).is_ok(),
                "`{schema}` is an ordinary path"
            );
        }
    }

    /// An extra that cannot be an identifier is refused where it is used, not
    /// silently dropped from the path: a path one schema short binds a name
    /// somewhere else and says nothing.
    #[test]
    fn an_extra_that_cannot_be_an_identifier_refuses_the_statement() {
        let pg = Postgres::with_write_path_extras(vec![String::new()]);
        let refusal = pg
            .emit(
                &Change::DropTable {
                    uid: Uid::generate(UidKind::Table),
                    name: name("app", "t"),
                },
                Strategy::default(),
            )
            .expect_err("an empty schema name is not an identifier");
        assert!(
            matches!(refusal, DialectError::UnquotableIdent(_)),
            "{refusal}"
        );
    }

    fn retype(from: &str, to: &str) -> Change {
        Change::AlterColumnType {
            uid: Uid::generate(UidKind::Column),
            column: ColumnRef {
                table: name("app", "t"),
                name: "at".into(),
            },
            from: ty(from),
            to: ty(to),
            from_nullable: false,
            to_nullable: false,
        }
    }

    /// Both refusals in the type arm, and the pair that must still pass
    /// between them. The zone one is not reachable through the risk classes:
    /// every row of the first list is `Narrowing`, which a human is allowed to
    /// clear at the gate, and clearing a loss is not approving a
    /// reinterpretation of every stored value.
    #[test]
    fn a_type_change_the_engine_or_the_session_would_decide_is_refused_by_name() {
        let pg = Postgres::new();
        for (from, to, named) in [
            ("timestamp", "timestamptz", "time zone"),
            ("timestamptz", "timestamp", "time zone"),
            ("date", "timestamptz", "time zone"),
            ("timestamptz", "timetz", "time zone"),
            ("character varying(10)", "integer", "USING"),
        ] {
            let refusal = pg
                .emit(&retype(from, to), Strategy::default())
                .expect_err("neither conversion is pbps's to choose");
            let DialectError::Invalid { message, .. } = &refusal else {
                panic!("`{from}` -> `{to}`: {refusal}");
            };
            assert!(message.contains(named), "`{from}` -> `{to}`: {message}");
            assert!(message.contains("at"), "`{from}` -> `{to}`: {message}");
        }
        // The widening beside them still goes through, in one statement: a
        // guard that cannot say yes is a guard nobody can use.
        assert_eq!(
            sql_of(
                &pg,
                &retype("character varying(10)", "character varying(20)")
            ),
            vec![
                "SET search_path = \"app\";\n\
                 ALTER TABLE \"app\".\"t\" ALTER COLUMN \"at\" TYPE character varying(20);\n\
                 RESET search_path;"
            ]
        );
    }

    /// The access method is in the statement, not left to the session.
    ///
    /// The reader accepts `relam = heap` and nothing else, so a table created
    /// under a role whose `default_table_access_method` names another installed
    /// method succeeds and then reads back as an unsupported object — absent
    /// from the pulled schema, planned again as a `CREATE` the engine refuses
    /// for already existing. A clause cannot be answered differently by a
    /// session, which a framing pin could be on the one path that has no
    /// framing: the rendered `--sql` script.
    #[test]
    fn a_created_table_names_the_access_method_it_will_be_read_back_under() {
        let mut table = Table::default();
        table
            .columns
            .insert("id".into(), Column::new(ty("integer")).not_null());
        let sql = sql_of(
            &Postgres::new(),
            &Change::CreateTable {
                uid: Uid::generate(UidKind::Table),
                name: name("app", "t"),
                table: Box::new(table),
            },
        );
        assert_eq!(
            sql,
            vec![
                "SET search_path = \"app\";\n\
                 CREATE TABLE \"app\".\"t\" (\n\
                 \x20   \"id\" integer NOT NULL\n\
                 ) USING heap;\n\
                 RESET search_path;"
            ]
        );
    }

    /// The bare-literal test is about the *whole* expression being one literal.
    /// Everything else is emitted as written, because everything else is an
    /// expression the engine resolves rather than a text a setting reads.
    #[test]
    fn only_a_whole_bare_literal_is_one() {
        for yes in [
            "'2026-01-02'",
            "  '2026-01-02'  ",
            "''",
            "'it''s'",
            // The other spellings of one literal, each of which a rule
            // about `'…'` alone would have let through on a `date`.
            r"E'2026-01-02'",
            r"e'it\'s'",
            "N'2026-01-02'",
            "n'2026-01-02'",
            "(N') ')",
            "$$2026-01-02$$",
            "$d$2026-01-02$d$",
            // A tag the engine's byte grammar accepts and Unicode's letter
            // classes do not: `a` followed by U+0301, whose second character is
            // a mark. Measured, this is one dollar-quoted literal on 18.6, so
            // reading it as anything else leaves an ambiguous date unguarded.
            "$a\u{301}$01/02/2026$a\u{301}$",
            // Grouping is not resolution: the engine drops the parentheses and
            // reads the same text through the same setting.
            "('01/02/2026')",
            // Continued across a newline, which this engine reads as one
            // string constant — measured, and measured again for the forms
            // that do *not* continue, below.
            "'01/02/'\n'2026'",
            "E'01/02/'\n'2026'",
            "U&'01/02/'\n'2026'",
            "'01/02/'\n  \n  '2026'",
            "'01/'\n'02/'\n'2026'",
            "  ( ( '01/02/2026' ) )  ",
            "($$01/02/2026$$)",
            "U&'2026-01-02'",
            // A comment is whitespace, and these are the forms in which it is
            // — measured, each of these is the same one constant to the engine
            // as the same text without the comment.
            "'01/02/' -- split here\n'2026'",
            "'01/02/' --\n'2026'",
            // A `/*` inside a line comment is comment text: the line comment
            // still ends at the newline and the continuation still stands.
            "'01/02/' -- /* x\n'2026'",
            "'2026-01-02' -- trailing",
            "'2026-01-02' /* trailing */",
            "/* leading */ '2026-01-02'",
            "(/* inside the grouping */ '2026-01-02')",
            "$$2026-01-02$$ -- trailing",
            // A bare carriage return is a newline to this lexer, both as the
            // gap a continuation needs and as the end of a line comment —
            // measured, all three of these are the one constant `01/02/2026`.
            "'01/02/'\r'2026'",
            "'01/02/'\r\n'2026'",
            "'01/02/' -- c\r'2026'",
            // The same character class, at the other scanner: a comment that
            // a carriage return closes cannot swallow the parenthesis that
            // closes the grouping. Measured, this is 2026-01-02 under MDY and
            // 2026-02-01 under DMY, which is the whole hazard behind one CR.
            "( -- )\r '01/02/2026')",
            // Trailing trivia is not only whitespace, and the grouping unwrap
            // is a test about the last character — measured, this stores
            // 2026-01-02 under MDY and 2026-02-01 under DMY as a `date`
            // default, with the parentheses and the comment both dropped from
            // what the engine keeps.
            "('01/02/2026') -- note",
            "('01/02/2026') /* note */",
            "(('01/02/2026') -- inner\n) -- outer",
            "$$01/02/2026$$ /* note */",
            // A parenthesis that is data cannot be the one that closes the
            // grouping. Measured, each of these is the same session-decided
            // value as the same declaration without the comment — the first
            // stores 2026-01-02 under MDY and 2026-02-01 under DMY as a column
            // default, which is the whole hazard written behind one `)`.
            "(/* ) */ '01/02/2026')",
            "('01/02/2026' /* ( */)",
            "('01/02/2026' -- (\n)",
            "($$01/02/2026$$ /* ) */)",
        ] {
            assert!(is_a_bare_literal(yes), "{yes}");
        }
        for no in [
            "'2026-01-02'::date",
            "DATE '2026-01-02'",
            "'a' || 'b'",
            "now()",
            "'a', 'b'",
            "",
            "'",
            "current_date",
            // Two literals, not one, in each spelling.
            r"E'a' || E'b'",
            "$$a$$ || $$b$$",
            // The closing quote is escaped, so nothing closed the literal.
            r"E'a\'",
            // A tag that is not a tag, and a body that is not closed.
            "$1$a$1$",
            "$d$a$e$",
            // A tag may not *start* with a digit even when every later
            // character is fine, and the byte rule does not change that.
            "$1a$b$1a$",
            // Parenthesised, but not one literal: the first group closes
            // before the end, so nothing is unwrapped and nothing is refused.
            "('a') || ('b')",
            // Two literals on one line are a syntax error to the engine, not
            // one constant, so this is not a declaration to refuse.
            "'01/02/' '2026'",
            // A continuation may not be an escape string, and dollar quoting
            // does not continue at all: both are syntax errors, measured.
            "'01/02/'\nE'2026'",
            "$$01/02/$$\n$$2026$$",
            // Still two literals with an operator between them, newline or
            // not.
            "'a'\n|| 'b'",
            "('a')::date",
            "('a'",
            // A literal's own parentheses cannot make a pair out of two
            // groups: the `(` inside the string is data, so the depth returns
            // to zero at the first group's own `)` and the two are not a pair.
            "('(') || (b)",
            // The same, with the parenthesis hidden in a comment instead.
            "(/* ( */ 'a') || ('b')",
            // The recorded gap: two literals with a keyword between them.
            "U&'a' UESCAPE '!'",
            // A block comment is whitespace too, but not the kind a
            // continuation may be written across: measured, each of these is a
            // syntax error whether the newline stands before the comment or
            // after it, so neither is a declaration to refuse.
            "'01/02/' /* c */\n'2026'",
            // The other newline does not change what a block comment does to
            // a continuation either.
            "'01/02/' /* c */\r'2026'",
            // Trailing trivia is stripped, and what is left still has to be
            // one literal: these are two groups and an operator either way.
            "('a') || ('b') -- note",
            // Nothing closes the comment, so it is not trivia — the engine
            // refuses the whole expression by name and this leaves it to say
            // so (DECISIONS 266).
            "('01/02/2026') /* unterminated",
            "'01/02/'\n/* c */ '2026'",
            // Block comments nest, so this is one comment in the gap and not
            // two — and the `--` inside one is comment text, which leaves the
            // gap a block comment either way.
            "'01/02/' /* /* x */ */\n'2026'",
            "'01/02/' /* -- x\n*/ '2026'",
            // Nothing closes the comment. The engine refuses that by name, so
            // this guard leaves it alone rather than refuse it as something
            // else (DECISIONS 266).
            "'2026-01-02' /* unterminated",
            // A comment gives a continuation nothing a newline would not
            // have given it: the two forms that do not continue across a
            // newline do not continue across a comment either, and an
            // operator between two literals is still an operator.
            "$$01/02/$$ -- c\n$$2026$$",
            "'01/02/' -- c\nE'2026'",
            "'a' -- c\n|| 'b'",
        ] {
            assert!(!is_a_bare_literal(no), "{no}");
        }
    }

    /// `GENERATED ALWAYS`, and no bounds or cache: those are the values
    /// introspection expects to see, and a plan that wrote others would read
    /// back as an identity this model cannot tell from the one it declared.
    #[test]
    fn an_identity_is_generated_always_and_leaves_the_sequence_at_its_defaults() {
        let mut table = Table::default();
        let mut id = Column::new(ty("bigint")).not_null();
        id.identity = Some(Identity {
            seed: 7,
            increment: 3,
        });
        table.columns.insert("id".into(), id);
        let sql = sql_of(
            &Postgres::new(),
            &Change::CreateTable {
                uid: Uid::generate(UidKind::Table),
                name: name("app", "t"),
                table: Box::new(table),
            },
        );
        assert!(
            sql[0].contains("GENERATED ALWAYS AS IDENTITY (START WITH 7 INCREMENT BY 3)"),
            "{}",
            sql[0]
        );
        assert!(
            !sql[0].contains("MINVALUE") && !sql[0].contains("CACHE"),
            "{}",
            sql[0]
        );
    }

    /// A change that is a fact about the declarations and not about the
    /// database emits nothing — and that is not the same as an unbuilt part,
    /// which is an error.
    #[test]
    fn a_change_the_database_cannot_hold_emits_nothing_rather_than_pretending() {
        let deprecated = Change::SetColumnDeprecated {
            uid: Uid::generate(UidKind::Column),
            column: name("app", "t").column("c"),
            reason: Some("gone in March".into()),
        };
        let mode = Change::SetDataMode {
            table: name("app", "t"),
            from: Some(pbps_model::DataMode::Ensure),
            to: Some(pbps_model::DataMode::Exact),
        };
        for change in [deprecated, mode] {
            assert!(
                Postgres::new()
                    .emit(&change, Strategy::default())
                    .expect("emit")
                    .is_empty()
            );
        }
    }

    /// `NO ACTION` is this engine's default too, and spelling out a default
    /// adds a line to a plan a human reads at a deployment gate.
    #[test]
    fn a_referential_action_is_spelled_only_when_it_is_not_the_default() {
        let fk = |on_delete, on_update| ForeignKey {
            columns: vec!["a".into()],
            references_table: name("app", "parent"),
            references_columns: vec!["b".into()],
            on_delete,
            on_update,
        };
        let emit = |fk| {
            sql_of(
                &Postgres::new(),
                &Change::AddForeignKey {
                    table: name("app", "t"),
                    name: "fk".into(),
                    constraint: Box::new(fk),
                },
            )
            .remove(0)
        };
        let quiet = emit(fk(ReferentialAction::NoAction, ReferentialAction::NoAction));
        assert!(
            !quiet.contains("ON DELETE") && !quiet.contains("ON UPDATE"),
            "{quiet}"
        );
        let loud = emit(fk(
            ReferentialAction::SetDefault,
            ReferentialAction::Cascade,
        ));
        assert!(loud.contains("ON DELETE SET DEFAULT"), "{loud}");
        assert!(loud.contains("ON UPDATE CASCADE"), "{loud}");
    }

    /// A name is quoted wherever it goes, including inside the `DO` block that
    /// drops an unnamed key: there the table is in *literal* position, and the
    /// identifier quoting that is right in code position would be wrong.
    #[test]
    fn a_name_that_needs_quoting_is_quoted_in_both_positions() {
        let sql = sql_of(
            &Postgres::new(),
            &Change::SetPrimaryKey {
                table: name("odd schema", "it's"),
                from: Some(PrimaryKey {
                    name: None,
                    columns: vec!["id".into()],
                }),
                to: None,
            },
        );
        let block = &sql[0];
        assert!(
            block.contains("SET search_path = \"odd schema\";"),
            "{block}"
        );
        // Code position: doubled quotes. Literal position: doubled apostrophe —
        // and the statement `format` builds is a literal *inside* a literal, so
        // its own apostrophes are doubled a second time. Quoting the name as an
        // identifier there would have closed the format string at the `'`.
        assert!(
            block.contains("'\"odd schema\".\"it''s\"'::pg_catalog.regclass"),
            "{block}"
        );
        assert!(
            block.contains(
                "pg_catalog.format('ALTER TABLE \"odd schema\".\"it''s\" DROP CONSTRAINT %I', pk)"
            ),
            "{block}"
        );
        // The `DO` body is dollar-quoted, so that literal needs one level of
        // doubling and not two — and the tag is chosen so the body cannot end
        // it early.
        assert!(
            block.starts_with("SET search_path = \"odd schema\";\nDO $pbps$\n"),
            "{block}"
        );
    }

    /// A name carrying the tag would close the block and put the rest of it on
    /// the server as top-level SQL. The tag is chosen against the body, so
    /// there is no name that can do it.
    #[test]
    fn a_name_that_spells_the_dollar_tag_does_not_end_the_block() {
        let sql = sql_of(
            &Postgres::new(),
            &Change::SetPrimaryKey {
                table: name("app", "x$pbps$y"),
                from: Some(PrimaryKey {
                    name: None,
                    columns: vec!["id".into()],
                }),
                to: None,
            },
        );
        let block = &sql[0];
        assert!(block.contains("DO $pbps1$"), "{block}");
        assert_eq!(block.matches("$pbps1$").count(), 2, "{block}");
    }

    /// An index without a filter and without the hint is the ordinary case, and
    /// it is scoped like everything else; the concurrent one is the only
    /// statement here that is not, because it cannot be.
    #[test]
    fn only_the_concurrent_index_leaves_the_scope_off() {
        let index = Index {
            columns: vec![IndexColumn {
                name: "n".into(),
                descending: true,
            }],
            include: vec!["m".into()],
            unique: true,
            filter: None,
        };
        let change = Change::AddIndex {
            table: name("app", "t"),
            name: "ix".into(),
            index: Box::new(index),
        };
        let offline = sql_of(&Postgres::new(), &change).remove(0);
        assert!(
            offline.starts_with("SET search_path = \"app\";"),
            "{offline}"
        );
        assert!(
            offline.contains(
                "CREATE UNIQUE INDEX \"ix\" ON \"app\".\"t\" (\"n\" DESC) INCLUDE (\"m\");"
            ),
            "{offline}"
        );
        let online = Postgres::new()
            .emit(&change, Strategy { online: true })
            .expect("emit")
            .remove(0);
        assert_eq!(
            online.sql,
            "CREATE UNIQUE INDEX CONCURRENTLY \"ix\" ON \"app\".\"t\" (\"n\" DESC) INCLUDE (\"m\");"
        );
    }

    /// A check's expression reaches the server as written. The parentheses
    /// around it are the emitter's, so a declaration that is already
    /// parenthesised does not need to guess whether to add its own.
    #[test]
    fn a_declared_expression_is_emitted_verbatim() {
        let sql = sql_of(
            &Postgres::new(),
            &Change::AddCheck {
                table: name("app", "t"),
                name: "ck".into(),
                constraint: CheckConstraint {
                    expression: "(n > 0)".into(),
                },
            },
        )
        .remove(0);
        assert!(
            sql.contains("ADD CONSTRAINT \"ck\" CHECK ((n > 0)\n);"),
            "{sql}"
        );

        // And the newline is not decoration: a comment in the declared text
        // ends at one, so the syntax that closes the statement has to be past
        // it. Measured, `CHECK (n > 0 -- reason));` is a syntax error and the
        // same text with the closer on the next line is accepted.
        for (expression, tail) in [
            ("n > 0 -- reason", "-- reason\n);"),
            ("n > 0 /* reason */", "/* reason */\n);"),
        ] {
            let sql = sql_of(
                &Postgres::new(),
                &Change::AddCheck {
                    table: name("app", "t"),
                    name: "ck".into(),
                    constraint: CheckConstraint {
                        expression: expression.into(),
                    },
                },
            )
            .remove(0);
            assert!(sql.contains(tail), "{expression}: {sql}");
        }

        // The same for the other two expressions this dialect writes
        // verbatim, at every site each of them reaches.
        let mut column = Column::new(ty("integer"));
        column.default = Some("1 -- why".into());
        let mut table = Table::default();
        table.columns.insert("n".into(), column.clone());
        let created = sql_of(
            &Postgres::new(),
            &Change::CreateTable {
                uid: Uid::generate(UidKind::Table),
                name: name("app", "t"),
                table: Box::new(table),
            },
        )
        .remove(0);
        assert!(created.contains("DEFAULT 1 -- why\n"), "{created}");
        let added = sql_of(
            &Postgres::new(),
            &Change::AddColumn {
                uid: Uid::generate(UidKind::Column),
                table: name("app", "t"),
                name: "n".into(),
                column: Box::new(column),
            },
        )
        .remove(0);
        assert!(added.contains("DEFAULT 1 -- why\n"), "{added}");
        let set = sql_of(
            &Postgres::new(),
            &Change::AlterColumnDefault {
                uid: Uid::generate(UidKind::Column),
                column: name("app", "t").column("n"),
                from: None,
                to: Some("1 -- why".into()),
            },
        )
        .remove(0);
        assert!(set.contains("SET DEFAULT 1 -- why\n;"), "{set}");
        let indexed = sql_of(
            &Postgres::new(),
            &Change::AddIndex {
                table: name("app", "t"),
                name: "ix".into(),
                index: Box::new(Index {
                    columns: vec![IndexColumn {
                        name: "n".into(),
                        descending: false,
                    }],
                    include: vec![],
                    unique: false,
                    filter: Some("n > 0 -- why".into()),
                }),
            },
        )
        .remove(0);
        assert!(indexed.contains("WHERE (n > 0 -- why\n);"), "{indexed}");
    }

    /// A table with no columns is not a table this engine will make, and the
    /// refusal names it rather than letting the server answer with a syntax
    /// error about a bracket.
    #[test]
    fn a_table_with_no_columns_is_refused_by_name() {
        let refusal = Postgres::new()
            .emit(
                &Change::CreateTable {
                    uid: Uid::generate(UidKind::Table),
                    name: name("app", "empty"),
                    table: Box::new(Table::default()),
                },
                Strategy::default(),
            )
            .expect_err("no columns");
        assert!(refusal.to_string().contains("app.empty"), "{refusal}");
    }
}
