//! Closed row-predicate syntax, independent of recording and execution.
//! DEC-2400.20: no statement, subquery, qualified reference or raw SQL node.

use pbps_model::{ColumnName, TypeArg};

mod lex;
mod parse;
mod render;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "row precondition: {0}; use a plan-time assertion (#2401, planned), or a CHECK constraint for the transition"
)]
pub struct Error(String);
impl Error {
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    Ordinary,
    Cast,
    Convert,
    Datepart,
    Trim,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Function {
    pub name: &'static str,
    pub form: Form,
    pub arity: (usize, usize),
    pub major: u16,
}
/// A trusted dialect's validated type; no type name comes from input text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastType {
    pub base: &'static str,
    pub args: Vec<TypeArg>,
}
#[derive(Clone, Copy, Debug)]
pub struct CallInfo {
    pub arity: usize,
    pub directional: bool,
    pub literal_path: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct Capabilities {
    pub major: u16,
    pub compatibility: u16,
}
/// Static engine knowledge only; this is not an execution or plugin interface.
pub trait Syntax: crate::Dialect {
    fn string_literal(&self, value: &str, national: bool) -> String;
    fn function(&self, name: &str) -> Option<&'static Function>;
    fn cast_type(&self, name: &str, args: Vec<TypeArg>) -> Result<CastType>;
    fn datepart(&self, function: &Function, name: &str) -> Option<&'static str>;
    fn requirement(&self, function: &Function, call: CallInfo) -> (u16, u16);
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node<T> {
    kind: T,
    depth: usize,
}
type Scalar = Node<Value>;
type Predicate = Node<Condition>;
#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Number(String),
    String(String, bool),
    Null,
    Column(ColumnName),
    Unary(&'static str, Box<Scalar>),
    Binary(&'static str, Box<Scalar>, Box<Scalar>),
    Call(&'static Function, Call),
    SimpleCase(Box<Scalar>, Vec<(Scalar, Scalar)>, Option<Box<Scalar>>),
    SearchedCase(Vec<(Predicate, Scalar)>, Option<Box<Scalar>>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Ordinary(Vec<Scalar>),
    Cast(Box<Scalar>, CastType),
    Convert(CastType, Box<Scalar>, Option<i32>),
    Datepart(&'static str, Vec<Scalar>),
    Trim(Option<&'static str>, Option<Box<Scalar>>, Box<Scalar>),
}
impl Call {
    fn values(&self) -> Vec<&Scalar> {
        match self {
            Self::Ordinary(v) | Self::Datepart(_, v) => v.iter().collect(),
            Self::Cast(v, _) | Self::Convert(_, v, _) => vec![v],
            Self::Trim(_, chars, v) => chars.iter().map(Box::as_ref).chain([v.as_ref()]).collect(),
        }
    }
    fn info(&self) -> CallInfo {
        let arity = match self {
            Self::Cast(..) => 1,
            Self::Convert(_, _, style) => 2 + usize::from(style.is_some()),
            Self::Datepart(_, v) => v.len() + 1,
            Self::Ordinary(_) | Self::Trim(..) => self.values().len(),
        };
        CallInfo {
            arity,
            directional: matches!(self, Self::Trim(Some(_), ..)),
            literal_path: matches!(self, Self::Ordinary(v) if matches!(v.get(1).map(|s| &s.kind), Some(Value::String(..)))),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Condition {
    Compare(&'static str, Scalar, Scalar),
    Null(Scalar, bool),
    In(Scalar, Vec<Scalar>, bool),
    Between(Scalar, Scalar, Scalar, bool),
    Like(Scalar, Scalar, Option<Scalar>, bool),
    Not(Box<Predicate>),
    Logic(&'static str, Box<Predicate>, Box<Predicate>),
}
fn node<T>(kind: T, child_depth: usize) -> Result<Node<T>> {
    if child_depth >= 128 {
        return Err(Error::new("expression depth exceeds 128"));
    }
    Ok(Node {
        kind,
        depth: child_depth + 1,
    })
}
fn maximum(values: impl Iterator<Item = usize>) -> usize {
    values.max().unwrap_or(0)
}
fn scalar(kind: Value) -> Result<Scalar> {
    let depth = match &kind {
        Value::Unary(_, v) => v.depth,
        Value::Binary(_, a, b) => a.depth.max(b.depth),
        Value::Call(_, args) => maximum(args.values().iter().map(|v| v.depth)),
        Value::SimpleCase(v, arms, otherwise) => maximum(
            [v.depth]
                .into_iter()
                .chain(arms.iter().flat_map(|(a, b)| [a.depth, b.depth]))
                .chain(otherwise.iter().map(|v| v.depth)),
        ),
        Value::SearchedCase(arms, otherwise) => maximum(
            arms.iter()
                .flat_map(|(a, b)| [a.depth, b.depth])
                .chain(otherwise.iter().map(|v| v.depth)),
        ),
        Value::Number(_) | Value::String(..) | Value::Null | Value::Column(_) => 0,
    };
    node(kind, depth)
}
fn predicate(kind: Condition) -> Result<Predicate> {
    let depth = match &kind {
        Condition::Compare(_, a, b) => a.depth.max(b.depth),
        Condition::Null(v, _) => v.depth,
        Condition::In(v, list, _) => {
            maximum([v.depth].into_iter().chain(list.iter().map(|v| v.depth)))
        }
        Condition::Between(a, b, c, _) => a.depth.max(b.depth).max(c.depth),
        Condition::Like(a, b, c, _) => a.depth.max(b.depth).max(c.as_ref().map_or(0, |v| v.depth)),
        Condition::Not(v) => v.depth,
        Condition::Logic(_, a, b) => a.depth.max(b.depth),
    };
    node(kind, depth)
}

/// Syntax alone has no SQL rendering method: scope and version vetting come first.
#[derive(Debug)]
pub struct Parsed<S> {
    root: Predicate,
    syntax: S,
}
impl<S> PartialEq for Parsed<S> {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
    }
}
impl<S> Eq for Parsed<S> {}
/// The scope resolver must resolve only within the one target table, using
/// engine-owned binding facts. The parser never guesses catalogue collation.
impl<S: Syntax> Parsed<S> {
    /// Collect candidates without granting SQL rendering authority. The caller
    /// still resolves each against the target's catalog before `vet` succeeds.
    pub fn column_names(&mut self) -> Result<std::collections::BTreeSet<ColumnName>> {
        let mut names = std::collections::BTreeSet::new();
        self.root.visit(&mut |value| {
            if let Value::Column(name) = &value.kind {
                names.insert(name.clone());
            }
            Ok(())
        })?;
        Ok(names)
    }

    pub fn vet(
        mut self,
        mut column: impl FnMut(&ColumnName) -> Option<ColumnName>,
        capabilities: Capabilities,
    ) -> Result<Vetted<S>> {
        if capabilities.major < 10 || capabilities.compatibility == 0 {
            return Err(Error::new(
                "known SQL Server version and compatibility are required before planning",
            ));
        }
        self.root.visit(&mut |value| {
            match &mut value.kind {
                Value::Column(name) => {
                    let message = format!("column {} is not in the target table", name.shown());
                    *name = column(name).ok_or_else(|| Error::new(message))?;
                }
                Value::Call(function, call) => {
                    let (major, compatibility) = self.syntax.requirement(function, call.info());
                    if capabilities.major < major || capabilities.compatibility < compatibility {
                        let message = format!(
                            "{} requires SQL Server major {major} and compatibility \
                             {compatibility} or later",
                            function.name
                        );
                        return Err(Error::new(message));
                    }
                }
                Value::Number(_)
                | Value::String(..)
                | Value::Null
                | Value::Unary(..)
                | Value::Binary(..)
                | Value::SimpleCase(..)
                | Value::SearchedCase(..) => (),
            }
            Ok(())
        })?;
        Ok(Vetted(self))
    }
}
pub struct Vetted<S>(Parsed<S>);
impl<S: Syntax> Vetted<S> {
    pub fn sql(&self) -> Result<String> {
        render::condition(&self.0.root, &self.0.syntax)
    }
    pub fn parsed(&self) -> &Parsed<S> {
        &self.0
    }
}
pub fn parse<S: Syntax>(text: &str, syntax: S) -> Result<Parsed<S>> {
    if text.len() > 65536 {
        return Err(Error::new("expression exceeds 64 KiB"));
    }
    let root = parse::Parser::read(text, &syntax)?;
    Ok(Parsed { root, syntax })
}
impl Predicate {
    fn visit(&mut self, f: &mut impl FnMut(&mut Scalar) -> Result<()>) -> Result<()> {
        match &mut self.kind {
            Condition::Compare(_, a, b) => {
                for v in [a, b] {
                    v.visit(f)?;
                }
            }
            Condition::Null(v, _) => v.visit(f)?,
            Condition::In(v, list, _) => {
                v.visit(f)?;
                for v in list {
                    v.visit(f)?;
                }
            }
            Condition::Between(a, b, c, _) => {
                for v in [a, b, c] {
                    v.visit(f)?;
                }
            }
            Condition::Like(a, b, c, _) => {
                for v in [a, b] {
                    v.visit(f)?;
                }
                if let Some(v) = c {
                    v.visit(f)?;
                }
            }
            Condition::Not(v) => v.visit(f)?,
            Condition::Logic(_, a, b) => {
                for v in [a, b] {
                    v.visit(f)?;
                }
            }
        }
        Ok(())
    }
}
impl Scalar {
    fn visit(&mut self, f: &mut impl FnMut(&mut Scalar) -> Result<()>) -> Result<()> {
        f(self)?;
        match &mut self.kind {
            Value::Unary(_, v) => v.visit(f)?,
            Value::Binary(_, a, b) => {
                for v in [a, b] {
                    v.visit(f)?;
                }
            }
            Value::Call(_, call) => match call {
                Call::Ordinary(v) | Call::Datepart(_, v) => {
                    for v in v {
                        v.visit(f)?;
                    }
                }
                Call::Cast(v, _) | Call::Convert(_, v, _) => v.visit(f)?,
                Call::Trim(_, chars, v) => {
                    if let Some(c) = chars {
                        c.visit(f)?;
                    }
                    v.visit(f)?;
                }
            },
            Value::SimpleCase(v, arms, otherwise) => {
                v.visit(f)?;
                for (a, b) in arms {
                    a.visit(f)?;
                    b.visit(f)?;
                }
                if let Some(v) = otherwise {
                    v.visit(f)?;
                }
            }
            Value::SearchedCase(arms, otherwise) => {
                for (a, b) in arms {
                    a.visit(f)?;
                    b.visit(f)?;
                }
                if let Some(v) = otherwise {
                    v.visit(f)?;
                }
            }
            Value::Number(_) | Value::String(..) | Value::Null | Value::Column(_) => (),
        }
        Ok(())
    }
}
