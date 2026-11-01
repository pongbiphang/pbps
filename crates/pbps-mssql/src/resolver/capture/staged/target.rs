//! Private creation-name questions and complete target surveys (DEC-2541.2).
#[path = "target_visibility.rs"]
mod visibility;
pub(super) use visibility::prove;

use super::super::{
    CandidateClass, CandidateSet, CaptureError, CaptureScope, CapturedInputs, Uncovered, class,
    object,
};
use crate::Mssql;
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_dialect::Dialect;
use pbps_model::{Module, ModuleKind, Schema};
use std::{collections::BTreeSet, ops::ControlFlow};

// Source text stays private and is discarded before the SQL-free blueprint.
struct Token {
    text: String,
    quoted: bool,
}

impl Token {
    fn word(&self, word: &str) -> bool {
        !self.quoted && self.text.eq_ignore_ascii_case(word)
    }
}

/// The shared Lexicon blanks strings/comments. The guard detects an unfinished
/// literal/comment; the token reader checks unfinished delimited identifiers.
fn tokens(text: &str) -> Result<Vec<Token>, &'static str> {
    const GUARD: &str = "pbps_creation_scan2541_guard";
    let lexicon = Mssql.lexicon();
    let code = lexicon.code_only(&format!("{text}\n{GUARD}"));
    let Some(code) = code.strip_suffix(GUARD) else {
        return Err("the creation-order text cannot be lexed; correct the declaration text");
    };
    let mut chars = code.chars().peekable();
    let mut out = Vec::new();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        let close = lexicon
            .quoted_identifiers
            .iter()
            .find(|(open, _)| *open == c);
        let (mut text, quoted) = (String::from(c), close.is_some());
        if let Some((_, close)) = close {
            loop {
                let Some(next) = chars.next() else {
                    return Err(
                        "the creation-order text cannot be lexed; correct the declaration text",
                    );
                };
                text.push(next);
                if next == *close {
                    if chars.next_if_eq(close).is_none() {
                        break;
                    }
                    text.push(*close);
                }
            }
        } else if (lexicon.identifier_continues)(c) {
            while let Some(next) = chars.next_if(|c| (lexicon.identifier_continues)(*c)) {
                text.push(next);
            }
        }
        out.push(Token { text, quoted });
    }
    Ok(out)
}

/// Only the RETURNS clause at header depth zero classifies a function.
/// A parameter/return-type name or a body keyword cannot supply an option.
/// EXECUTE AS belongs to the header; only the later AS starts its body.
fn function_body(tokens: &[Token]) -> Result<Option<(usize, bool)>, &'static str> {
    let mut depth = 0usize;
    let mut returns = None;
    let mut bound = false;
    for (at, token) in tokens.iter().enumerate() {
        let previous = at.checked_sub(1).and_then(|at| tokens.get(at));
        if token.word("(") {
            depth += 1;
        } else if token.word(")") {
            depth = depth.saturating_sub(1);
        } else if depth == 0
            && token.word("SCHEMABINDING")
            && previous.is_some_and(|t| t.word("WITH") || t.word(","))
        {
            bound = true;
        } else if depth == 0 && token.word("RETURNS") {
            returns = Some(at + 1);
        } else if depth == 0
            && ((token.word("AS") && !previous.is_some_and(|t| t.word("EXECUTE")))
                || token.word("RETURN")
                || token.word("BEGIN"))
        {
            let returned = returns.and_then(|at| tokens.get(at));
            let inline = match returned {
                Some(t) if t.word("TABLE") => true,
                Some(t)
                    if t.text.starts_with('@')
                        && returns
                            .and_then(|at| tokens.get(at + 1))
                            .is_some_and(|t| t.word("TABLE")) =>
                {
                    false
                }
                Some(t)
                    if !t.word("AS")
                        && !t.word("BEGIN")
                        && !t.word("RETURN")
                        && (t.quoted || t.text.chars().next().is_some_and(char::is_alphabetic)) =>
                {
                    false
                }
                _ => {
                    return Err(
                        "the function header cannot be classified for creation ordering; correct the declaration text",
                    );
                }
            };
            // Schema binding makes every function's references creation-bound;
            // only an ordinary scalar/MSTVF body can defer those references.
            return Ok((inline || bound).then_some((at + 1, bound)));
        }
    }
    Err(
        "the function header cannot be classified for creation ordering; correct the declaration text",
    )
}

/// An exact two-component run only. In particular db.dbo.v must not be
/// mistaken for dbo.v, and delimiters inside one name are not separators.
fn qualified(tokens: &[Token], schema: &str, name: &str) -> bool {
    tokens.windows(3).enumerate().any(|(at, part)| {
        part[1].word(".")
            && !at.checked_sub(1).is_some_and(|at| tokens[at].word("."))
            && !tokens.get(at + 3).is_some_and(|t| t.word("."))
            && (Mssql.may_name_qualified(
                &format!("{}.{}", part[0].text, part[2].text),
                schema,
                name,
            ) || ((part[0].quoted || part[2].quoted)
                && Mssql.may_name(&part[0].text, schema)
                && Mssql.may_name(&part[2].text, name)))
    })
}

fn bare(
    tokens: &[Token],
    from: &str,
    external: &ObjectIdentity,
    candidates: &[&ObjectIdentity],
) -> bool {
    let [schema, name] = external.name.as_slice() else {
        return false;
    };
    let Some(rank) = Mssql.bare_name_rank(from, schema) else {
        return false;
    };
    tokens.windows(2).enumerate().any(|(at, part)| {
        (part[0].word("FROM") || part[0].word("JOIN"))
            && !tokens.get(at + 2).is_some_and(|t| t.word("."))
            && Mssql.may_name(&part[1].text, name)
            // DECISIONS 317: prefix externals and managed objects also mask
            // a farther candidate. Looking only among chained roots is wrong.
            && candidates.iter().filter_map(|candidate| {
                let [schema, name] = candidate.name.as_slice() else { return None; };
                (candidate.class == class::OBJECT && Mssql.may_name(&part[1].text, name))
                    .then(|| Mssql.bare_name_rank(from, schema)).flatten()
            }).min() == Some(rank)
    })
}

// Opaque even to reconstruction: source is neither serializable nor printable.
pub(in crate::resolver) struct CreationNames {
    tokens: Vec<Token>,
    bare: bool,
}

impl CreationNames {
    pub(in crate::resolver) fn module(module: &Module) -> Result<Self, &'static str> {
        let source = tokens(&module.definition)?;
        let (start, bound) = match module.kind {
            ModuleKind::Procedure | ModuleKind::Trigger => {
                return Ok(Self {
                    tokens: vec![],
                    bare: false,
                });
            }
            ModuleKind::View => (0, module.options.schemabinding),
            ModuleKind::Function => match function_body(&source)? {
                None => {
                    return Ok(Self {
                        tokens: vec![],
                        bare: false,
                    });
                }
                Some(answer) => answer,
            },
        };
        Ok(Self {
            tokens: source.into_iter().skip(start).collect(),
            bare: !bound,
        })
    }

    pub(in crate::resolver) fn statement(text: &str) -> Result<Self, &'static str> {
        Ok(Self {
            tokens: tokens(text)?,
            bare: false,
        })
    }

    pub(in crate::resolver) fn needs(
        &self,
        home: &str,
        external: &ObjectIdentity,
        candidates: &[&ObjectIdentity],
    ) -> bool {
        let [schema, name] = external.name.as_slice() else {
            return false;
        };
        qualified(&self.tokens, schema, name)
            || (self.bare
                && external.class == class::OBJECT
                && bare(&self.tokens, home, external, candidates))
    }

    fn questions(&self, home: &str) -> impl Iterator<Item = CandidateSet> + '_ {
        let home = home.to_owned();
        self.tokens
            .windows(2)
            .enumerate()
            .filter(move |(at, part)| {
                self.bare
                    && (part[0].word("FROM") || part[0].word("JOIN"))
                    && !self.tokens.get(at + 2).is_some_and(|t| t.word("."))
                    && (part[1].quoted
                        || part[1]
                            .text
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_alphabetic() || matches!(c, '_' | '#' | '@')))
            })
            .flat_map(move |(_, part)| {
                let text = &part[1].text;
                let name = if part[1].quoted {
                    let close = text.chars().last().expect("a complete quoted token");
                    text[1..text.len() - close.len_utf8()]
                        .replace(&format!("{close}{close}"), &close.to_string())
                } else {
                    text.clone()
                };
                [home.clone(), "dbo".into()].map(|schema| CandidateSet {
                    class: CandidateClass::Object,
                    schema,
                    name: name.clone(),
                })
            })
    }
}

fn seed(scope: &mut CaptureScope, schemas: &[Schema]) -> Result<(), CaptureError> {
    for schema in schemas {
        for (id, module) in &schema.modules {
            let names = CreationNames::module(module).map_err(|reason| {
                CaptureError::Coverage(Uncovered::object(&object(id.schema(), id.name()), reason))
            })?;
            scope.candidates.extend(names.questions(id.schema()));
        }
    }
    Ok(())
}

// Only the enlarged request survives a partial survey. In particular no old
// object properties or memberships can be joined into the final observation.
fn advance(
    captured: CapturedInputs,
    managed: &BTreeSet<ObjectIdentity>,
) -> ControlFlow<CapturedInputs, CaptureScope> {
    // Target bodies of managed objects can be replaced by the desired text.
    // Only desired scans seed their questions; old runtime bodies must not
    // drag unrelated candidates into this preparation (including deferred procs).
    let runtime = captured
        .runtime
        .iter()
        .filter(|(id, _)| !managed.contains(id))
        .map(|(id, names)| (id.clone(), names.clone()))
        .collect();
    let mut next = captured.scope().clone();
    next.candidates
        .extend(super::super::select::runtime_candidates(
            &captured.inputs,
            &runtime,
        ));
    if next == *captured.scope() {
        ControlFlow::Break(captured)
    } else {
        ControlFlow::Continue(next)
    }
}

pub(super) async fn prepare(
    target: &mut impl pbps_db::transport::QueryConnection,
    mut scope: CaptureScope,
    schemas: &[Schema],
    managed: &BTreeSet<ObjectIdentity>,
    declarations: &pbps_model::dependency::Dependencies,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<CapturedInputs, CaptureError> {
    seed(&mut scope, schemas)?;
    loop {
        // Candidate sets already materialize their visible members through
        // select's ordinary catalog closure; no runtime name becomes a binding.
        visibility::prove(target, schemas).await?;
        let captured = super::super::capture_declared(target, &scope, declarations, key).await?;
        visibility::prove(target, schemas).await?;
        match advance(captured, managed) {
            ControlFlow::Break(complete) => return Ok(complete),
            ControlFlow::Continue(expanded) => scope = expanded,
        }
    }
}

#[cfg(test)]
#[path = "target_tests.rs"]
mod tests;
