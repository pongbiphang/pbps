//! One routine-prerequisite policy for table and domain creation parts.
use super::*;
pub(super) fn add(
    out: &mut Schedule,
    at: usize,
    p: &PlannedChange,
    modules: &[ModuleStep<'_>],
    dialect: &(impl Dialect + ?Sized),
    scratch: bool,
    guessed: bool,
) {
    let edge = |out: &mut Schedule, a, b| {
        if a != b {
            out.edge(a, b, Source::Lexical, OrderReason::Structural);
        }
    };
    let guess = |out: &mut Schedule, a, b| {
        if a != b {
            if guessed {
                out.prefer(a, b, OrderReason::Structural);
            } else {
                out.edge(a, b, Source::Inferred, OrderReason::Structural);
            }
        }
    };
    // The engine needs a routine before the expression that calls it.
    // A name matched anywhere else (a type spelling `text` beside a
    // view `app.text`) is a guess, and so is a name that is no
    // function (DEC-2389.4).
    let read = part_texts(p, dialect, scratch);
    let expressions = if matches!(p.change, Change::AlterDomain { .. }) && is_part(p) {
        domains::texts(&p.change)
    } else {
        expressions(&p.change)
    };
    // Call-position hints read code only; literals remain reference
    // guesses. PG trials resolve both hints (DEC-2465.1).
    let called: Vec<_> = expressions
        .iter()
        .map(|text| {
            if guessed {
                std::borrow::Cow::Owned(dialect.lexicon().code_only(text))
            } else {
                std::borrow::Cow::Borrowed(*text)
            }
        })
        .collect();
    for (function, id, module) in modules {
        if module.kind == ModuleKind::Function
            && called.iter().any(|t| dialect.generation_calls(t, id))
        {
            // A call spelling can also be a parameter or a column.
            // PG scratch resolves the guess by trials (DEC-2465.1).
            if guessed {
                guess(out, *function, at);
            } else {
                edge(out, *function, at);
            }
        } else if module.kind == ModuleKind::Function
            && guessed
            && expressions.iter().any(|t| dialect.generation_calls(t, id))
        {
            guess(out, *function, at);
        } else if (module.kind != ModuleKind::Function || !guessed)
            && read.iter().any(|t| dialect.generation_calls(t, id))
        {
            // A view or table can be named as a type, a function
            // cannot: where guesses are read exactly, a function in
            // a type spelling is no reference at all.
            guess(out, *function, at);
        }
    }
}
