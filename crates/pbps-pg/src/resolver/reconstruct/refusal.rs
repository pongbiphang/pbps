//! Transient, typed refusal facts. Paths and YAML belong to the caller (DEC-2465.2).
use super::{Reconstruction, Step};
use pbps_db::{DbError, ErrorPosition};
use pbps_dialect::Dialect;
use pbps_model::{ObjectName, diagnostic::Declaration, escape::Escaped};
use std::{fmt, sync::Arc};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Candidate {
    Declaration(Declaration),
    Column(pbps_model::ColumnRef),
}
impl Candidate {
    fn owner(&self) -> Declaration {
        match self {
            Self::Declaration(d) => d.clone(),
            Self::Column(c) => Declaration::Table(c.table.clone()),
        }
    }
    fn label(&self) -> String {
        match self {
            Self::Declaration(d) => declaration(d),
            Self::Column(c) => format!("column {}", c.shown()),
        }
    }
}
#[derive(Debug, Clone)]
pub struct Diagnosis {
    pub error: Arc<DbError>,
    pub statement: usize,
    pub generation: usize,
    pub excerpt: Option<(String, usize)>,
    pub reference: Option<ObjectName>,
    pub candidates: Vec<Candidate>,
    pub declared: Vec<Declaration>,
    pub created: Vec<String>,
    pub overtaken: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct Pending {
    pub declaration: String,
    pub subject: pbps_model::Change,
    pub owner: Option<Declaration>,
    pub dependency: Option<pbps_model::ModuleId>,
    pub last: Option<Diagnosis>,
    pub recovery: Option<Diagnosis>,
    pub blocked: Vec<String>,
    pub why: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct Refusal {
    pub version: i64,
    pub pending: Vec<Pending>,
    pub committed: Vec<String>,
    pub stalled: bool,
    pub overtakes: Vec<(String, usize, Vec<String>)>,
}
impl std::error::Error for Refusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.pending
            .iter()
            .find_map(|p| p.last.as_ref())
            .map(|d| d.error.as_ref() as _)
    }
}
impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(|_| None))
    }
}
impl Refusal {
    pub fn render(&self, path: impl Fn(&Declaration) -> Option<String>) -> String {
        let source = |owner: &Option<Declaration>| {
            owner
                .as_ref()
                .and_then(&path)
                .unwrap_or_else(|| "source file unavailable".into())
        };
        let mut out = Vec::new();
        for (name, generation, providers) in &self.overtakes {
            out.push(format!("earlier successful attempt of {name} overtook unmet soft preferences at generation {generation}: {}", providers.join(", ")));
        }
        for pending in &self.pending {
            out.push(format!(
                "scratch could not create {} ({})",
                pending.declaration,
                source(&pending.owner)
            ));
            if let Some(last) = &pending.last {
                out.push(format!(
                    "  PostgreSQL {}.{} said: {} {}",
                    self.version / 10_000,
                    self.version % 10_000,
                    last.error
                        .server_error_code()
                        .as_deref()
                        .unwrap_or("no SQLSTATE"),
                    last.error
                        .diagnostic()
                        .map_or_else(|| last.error.to_string(), |d| d.primary.clone())
                ));
                if let Some((text, width)) = &last.excerpt {
                    out.push(format!("    {text}\n    {}^", " ".repeat(*width)));
                }
                if let Some(reference) = &last.reference {
                    out.push(format!("  positioned name: {}", reference.shown()));
                } else {
                    out.push("  no reliable positioned object identity".into());
                }
                if last.candidates.is_empty()
                    && let Some(name) = &last.reference
                {
                    if last.declared.is_empty() {
                        out.push(format!(
                            "  {} is not declared; declare it or provide it in reviewed externals",
                            name.shown()
                        ));
                    } else {
                        for owner in &last.declared {
                            out.push(format!(
                                "  {} is declared ({}); check its kind and signature for this use",
                                declaration(owner),
                                path(owner).unwrap_or_else(|| "source file unavailable".into())
                            ));
                        }
                    }
                }
                if last.candidates.is_empty() {
                    out.push("  no declared pending candidate identified; declare the missing prerequisite or provide it in reviewed externals".into());
                }
                for candidate in &last.candidates {
                    out.push(format!(
                        "  candidate: {} is declared ({}) but was not created yet",
                        candidate.label(),
                        path(&candidate.owner())
                            .unwrap_or_else(|| "source file unavailable".into())
                    ));
                }
                out.push(format!(
                    "  attempt: statement {}; success generation {}",
                    last.statement + 1,
                    last.generation
                ));
                if !last.overtaken.is_empty() {
                    out.push(format!(
                        "  this attempt overtook unmet soft preferences from: {}",
                        last.overtaken.join(", ")
                    ));
                }
                out.push(format!("  created before this attempt: committed earlier ranges: {}; earlier in this range (rolled back): {}", listed(&self.committed), listed(&last.created)));
            }
            if let Some(recovery) = &pending.recovery {
                out.push(format!(
                    "  recovery refusal: {} {}",
                    recovery
                        .error
                        .server_error_code()
                        .as_deref()
                        .unwrap_or("no SQLSTATE"),
                    recovery
                        .error
                        .diagnostic()
                        .map_or_else(|| recovery.error.to_string(), |d| d.primary.clone())
                ));
            }
            if pending.last.is_none() {
                out.push(format!(
                    "  not attempted: waiting for hard predecessor {}",
                    pending.blocked.join(", ")
                ));
            }
            out.push(format!(
                "  why this step was ordered here: {}",
                pending.why.join("; ")
            ));
            if pending.last.is_none() {
                out.push(format!(
                    "  resolve the refusal shown above for {}; this declaration was never tried",
                    pending.blocked.join(", ")
                ));
            } else if self.stalled {
                out.push("  change the declaration: remove a creation-time dependency or use a genuinely late-bound definition; a hard depends_on: cycle cannot repair mutually creation-bound declarations".into());
            } else {
                out.push("  change the declaration: correct the engine error shown above before retrying".into());
            }
        }
        if self.stalled {
            out.push("no eligible creation made progress in this scratch range; range boundaries are fixed and providers outside it cannot be pulled across".into());
        }
        out.join("\n")
    }
}
fn listed(names: &[String]) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}
pub fn declaration(owner: &Declaration) -> String {
    match owner {
        Declaration::Table(t) => format!("table {}", t.shown()),
        Declaration::Module(m) => format!("module {}", m.shown()),
        Declaration::Extension(e) => format!("extension {}", e.shown()),
        Declaration::Type(t) => format!("type {}", t.shown()),
        Declaration::Role(r) => format!("role {}", r.shown()),
    }
}

/// P counts characters in the submitted statement. Internal p applies only to q,
/// and q is printable only when it is text this run already owns.
pub(super) fn location<'a>(
    error: &DbError,
    sql: &'a str,
    owned: &'a [String],
) -> Option<(&'a str, usize)> {
    let (text, position) = match error.diagnostic()?.position.as_ref()? {
        ErrorPosition::Original(p) => (sql, *p),
        ErrorPosition::Internal { position, query } => {
            let body = owned
                .iter()
                .find_map(|body| body.find(query).map(|at| &body[at..at + query.len()]))?;
            (body, *position)
        }
    };
    let byte = text
        .char_indices()
        .nth(usize::try_from(position.checked_sub(1)?).ok()?)?
        .0;
    Some((text, byte))
}
pub(super) fn excerpt(text: &str, byte: usize) -> (String, usize) {
    let start = text[..byte].rfind('\n').map_or(0, |at| at + 1);
    let end = text[byte..].find('\n').map_or(text.len(), |at| byte + at);
    let prefix = Escaped(&text[start..byte]).to_string();
    (
        Escaped(&text[start..end]).to_string(),
        UnicodeWidthStr::width(prefix.as_str()),
    )
}
/// Deliberately narrow: unsupported escapes/punctuation remain unknown. A
/// positioned literal can spell a regclass name; a bare name needs search_path.
pub(super) fn positioned_name(text: &str, byte: usize) -> Option<ObjectName> {
    if text[..byte]
        .chars()
        .next_back()
        .is_some_and(|c| (crate::LEXICON.identifier_continues)(c) || matches!(c, '.' | '\'' | '"'))
    {
        return None;
    }
    let tail = &text[byte..];
    let token = if let Some(literal) = tail.strip_prefix('\'') {
        let end = literal.find('\'')?;
        if literal[end + 1..].starts_with('\'') {
            return None;
        }
        let token = &literal[..end];
        if token.contains('\\') {
            return None;
        }
        token
    } else {
        let mut end = 0;
        let mut quoted = false;
        for (at, c) in tail.char_indices() {
            if c == '"' {
                quoted = !quoted;
            } else if !quoted && c != '.' && !(crate::LEXICON.identifier_continues)(c) {
                break;
            }
            end = at + c.len_utf8();
        }
        if quoted {
            return None;
        }
        &tail[..end]
    };
    // The model's parser decodes delimited names and folds unquoted names.
    let name: pbps_model::TypeRef = token.parse().ok()?;
    Some(ObjectName::new(name.schema(), name.name()))
}
impl Reconstruction {
    pub(super) fn diagnosis(
        &self,
        at: usize,
        statement: usize,
        error: DbError,
        delta: &[(usize, Step)],
        overtaken: Vec<String>,
    ) -> Diagnosis {
        let sql = &self.lowered.groups()[at].statements()[statement].sql;
        let owned: Vec<_> = self.steps.iter().filter_map(|s| s.body.clone()).collect();
        let location = location(&error, sql, &owned);
        let code = error.server_error_code();
        let reference = matches!(code.as_deref(), Some("42P01" | "42883" | "42704"))
            .then(|| location.and_then(|(text, byte)| positioned_name(text, byte)))
            .flatten();
        let regions: Vec<_> = std::iter::once(sql.clone())
            .chain(crate::LEXICON.string_literals(sql))
            .collect();
        let column = (code.as_deref() == Some("42703"))
            .then(|| {
                location
                    .and_then(|(text, byte)| positioned_name(&format!("app.{}", &text[byte..]), 0))
                    .map(|n| pbps_model::ColumnName::from(n.name))
            })
            .flatten();
        let mut candidates: Vec<Candidate> = self
            .steps
            .iter()
            .enumerate()
            .filter(|(i, step)| {
                *i != at
                    && step.owner != self.steps[at].owner
                    && !self.committed.contains(&super::FillWork::Declared(*i))
                    && !delta.iter().any(|(done, _)| done == i)
            })
            .filter(|(_, s)| {
                reference.as_ref().map_or_else(
                    || {
                        s.names.iter().any(|(_, schema, name)| {
                            regions.iter().any(|region| {
                                crate::Postgres::new().generation_may_call(region, name)
                                    || pbps_model::module::references_with(
                                        region,
                                        &ObjectName::new(schema, name),
                                        &pbps_model::module::Lexis {
                                            code_only: &|s| crate::LEXICON.code_only(s),
                                            continues_ident: crate::LEXICON.identifier_continues,
                                            reserved: crate::LEXICON.reserved,
                                            bare_rank: &pbps_model::module::every_schema,
                                        },
                                    )
                            })
                        })
                    },
                    |name| {
                        s.names
                            .iter()
                            .any(|(_, schema, leaf)| schema == &name.schema && leaf == &name.name)
                    },
                )
            })
            .filter(|(_, s)| {
                s.names.iter().any(|(kind, _, _)| match code.as_deref() {
                    Some("42P01") => {
                        matches!(kind, super::Nameable::Relation | super::Nameable::Index)
                    }
                    Some("42704") => {
                        matches!(kind, super::Nameable::Relation | super::Nameable::Type)
                    }
                    Some("42703") => false,
                    _ => true,
                })
            })
            .filter_map(|(_, s)| s.owner.clone().map(Candidate::Declaration))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if code.as_deref() == Some("42703") {
            for (i, step) in self.steps.iter().enumerate() {
                if i != at
                    && !self.committed.contains(&super::FillWork::Declared(i))
                    && !delta.iter().any(|(at, _)| *at == i)
                    && let pbps_model::Change::AddColumn { table, name, .. } = &step.subject
                    && column.as_ref().map_or_else(
                        || {
                            regions.iter().any(|region| {
                                crate::Postgres::new().generation_may_call(region, name.raw())
                            })
                        },
                        |column| name == column,
                    )
                {
                    candidates.push(Candidate::Column(table.column(name.clone())));
                }
            }
        }
        Diagnosis {
            excerpt: location.map(|(text, byte)| excerpt(text, byte)),
            reference: reference.clone(),
            candidates,
            declared: reference
                .as_ref()
                .map(|name| {
                    self.steps
                        .iter()
                        .filter(|s| {
                            s.names.iter().any(|(_, schema, leaf)| {
                                schema == &name.schema && leaf == &name.name
                            })
                        })
                        .filter_map(|s| s.owner.clone())
                        .collect()
                })
                .unwrap_or_default(),
            error: Arc::new(error),
            statement,
            generation: delta.len(),
            created: delta.iter().map(|(_, s)| s.declaration.clone()).collect(),
            overtaken,
        }
    }
    // Initial module order does not retain every text dependency as an edge.
    // A possible return path must suppress advice, even without a hard cycle.
    fn may_need(&self, provider: usize, owner: usize) -> bool {
        let mut pending = vec![provider];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(at) = pending.pop() {
            if at == owner {
                return true;
            }
            if seen.insert(at)
                && let Some(body) = &self.steps[at].body
            {
                let regions: Vec<_> = std::iter::once(body.clone())
                    .chain(crate::LEXICON.string_literals(body))
                    .collect();
                pending.extend(
                    self.steps
                        .iter()
                        .enumerate()
                        .filter(|(_, step)| {
                            step.module.as_ref().is_some_and(|id| {
                                regions.iter().any(|region| {
                                    crate::Postgres::new().generation_may_call(region, id.name())
                                })
                            })
                        })
                        .map(|(at, _)| at),
                );
            }
        }
        false
    }
    pub(super) fn refusal(
        &self,
        version: i64,
        mut pending: Vec<Pending>,
        stalled: bool,
        overtakes: Vec<(String, usize, Vec<String>)>,
    ) -> super::ReconstructError {
        let owners: Vec<_> = pending.iter().filter_map(|p| p.owner.clone()).collect();
        for item in &mut pending {
            if let (Some(Declaration::Module(owner)), Some(last)) = (&item.owner, &item.last)
                && !self.has_fill()
                && last.reference.is_some()
                && let [Candidate::Declaration(Declaration::Module(provider))] =
                    last.candidates.as_slice()
                && matches!(provider, pbps_model::ModuleId::Named(_))
                && !owners.contains(&Declaration::Module(provider.clone()))
                && let (Some(before), Some(after)) = (
                    self.steps
                        .iter()
                        .position(|s| s.module.as_ref() == Some(provider)),
                    self.steps
                        .iter()
                        .position(|s| s.module.as_ref() == Some(owner)),
                )
                && self.trial.can_precede(before, after)
                && !self.may_need(before, after)
            {
                item.dependency = Some(provider.clone());
            }
        }
        super::ReconstructError::Refused(Arc::new(Refusal {
            version,
            pending,
            committed: self
                .committed
                .iter()
                .map(|&work| self.work_name(work))
                .collect(),
            stalled,
            overtakes,
        }))
    }
}
