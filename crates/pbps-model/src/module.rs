//! Modules: views, procedures, functions and triggers
//! ([ADR-0002](../../../docs/ADR-0002-module-model.md)).
//!
//! # Why these are not identity-tracked
//!
//! The identity machinery — uids, tombstones, human intent — exists because
//! columns carry data: a rename mistaken for a drop destroys it irreversibly.
//! A module carries no data. Dropping and recreating one is semantically
//! lossless and its complete definition is in git, so a rename is just drop +
//! add and the audit trail is the commit that did it. **Modules therefore never
//! appear in `schema.ids.json`.**
//!
//! That is a principled line, not an economy: the criterion is "does drop + add
//! destroy state that lives only in the environment?", and for a module the
//! answer is no.
//!
//! # What a definition holds
//!
//! The emitter composes the whole `CREATE OR ALTER` statement, so that SQL still
//! appears exactly once. What the user writes is everything after the part the
//! emitter can derive:
//!
//! | Kind | Emitted prefix | So `definition:` starts at |
//! |---|---|---|
//! | view | `CREATE OR ALTER VIEW <name> AS` | the `SELECT` |
//! | trigger | `CREATE OR ALTER TRIGGER <name> ON <on>` | `AFTER INSERT ...` |
//! | procedure | `CREATE OR ALTER PROCEDURE <name>` | the parameter list, then `AS` |
//! | function | `CREATE OR ALTER FUNCTION <name>` | the parameter list, then `RETURNS` |
//!
//! Procedures and functions keep their parameter list inside `definition`
//! because a parameter is part of the object's contract, and modelling T-SQL
//! parameter syntax would be parsing SQL — which this tool does not do (§8.2).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use crate::name::TableName;
use crate::types::ColumnType;

/// The qualified name of a database object: `schema.object`.
///
/// Deliberately the same type as [`TableName`]: SQL Server keeps tables and
/// modules in **one** `sys.objects` namespace per schema, so "a view may not be
/// named after a table" is not a rule to remember but a consequence of the two
/// names having one type. [`check_names`] is that consequence made checkable.
pub type ObjectName = TableName;

/// The identity of a routine: its qualified name **and** its argument types.
///
/// The types are normalized for *routine identity*, which is not column
/// normalization: PostgreSQL discards type modifiers when it identifies a
/// routine, so `f(varchar(10))` and `f(varchar(20))` are one function
/// (ADR-0009 §1, measured). `Dialect::normalize_routine_arg` is the hook that
/// does it; the model only holds the result.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoutineId {
    pub name: ObjectName,
    /// Empty for a routine declared `f()`. A routine that takes no arguments
    /// is still a routine — the parentheses in the declared name are what
    /// separate it from a view, not the presence of an argument.
    pub args: Vec<ColumnType>,
}

impl RoutineId {
    pub fn new(name: ObjectName, args: Vec<ColumnType>) -> Self {
        Self { name, args }
    }
}

impl fmt::Display for RoutineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}(", self.name)?;
        for (i, a) in self.args.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{a}")?;
        }
        f.write_str(")")
    }
}

/// What identifies one module, which depends on its kind (ADR-0009 §1,
/// DECISIONS 200).
///
/// | Kind | Identified by | Because |
/// |---|---|---|
/// | view | `schema.name` | one `pg_class` namespace with tables |
/// | function, procedure | `schema.name` + argument types | they overload |
/// | trigger | its table + name | `DROP TRIGGER audit` is a syntax error |
///
/// # Why this is the map key and not a field of [`Module`]
///
/// Inviolable constraint 2 — containers hold names, elements do not. A
/// signature is part of the name, and so is the table a trigger is on: two
/// triggers called `audit` on two tables are two objects, and under a
/// name-only key they were one. A key/field pair that has to agree is the
/// class of bug that cannot detect itself.
///
/// # Why not a string
///
/// `app.f(int, text)` and `app.f(integer,text)` are one object. Inviolable
/// constraint 1 says two semantically identical schemas must be `==`; a string
/// key makes them unequal, a key holding normalized types makes them equal.
///
/// The string form — the JSON map key, and what a message shows — is
/// `app.v`, `app.f(integer,text)` and `app.orders.audit`. Each shape is
/// unambiguous: parentheses mean a routine, three dotted parts mean a trigger.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ModuleId {
    /// A view: one name, one namespace with tables.
    Named(ObjectName),
    /// A function or procedure, identified by name and signature.
    Routine(RoutineId),
    /// A trigger: its table, and its own name within that table.
    ///
    /// The trigger's own schema is not held separately because it is not its
    /// own: SQL Server puts a trigger in the schema of the table it is on, and
    /// PostgreSQL gives it no schema at all. `on.schema` is that schema, and
    /// the loader refuses a declaration whose two spellings disagree.
    Trigger { on: ObjectName, name: String },
}

impl ModuleId {
    /// The schema this module lives in.
    pub fn schema(&self) -> &str {
        match self {
            ModuleId::Named(n) => &n.schema,
            ModuleId::Routine(r) => &r.name.schema,
            ModuleId::Trigger { on, .. } => &on.schema,
        }
    }

    /// The object's own name, unqualified.
    pub fn name(&self) -> &str {
        match self {
            ModuleId::Named(n) => &n.name,
            ModuleId::Routine(r) => &r.name.name,
            ModuleId::Trigger { name, .. } => name,
        }
    }

    /// The qualified name, for the one namespace where a module competes with
    /// a table for its spelling.
    ///
    /// A trigger has one on SQL Server, where it is an object in
    /// `sys.objects`; whether that namespace is shared with tables is the
    /// dialect's answer (`shares_namespace_with_tables`), not this type's.
    pub fn object_name(&self) -> ObjectName {
        match self {
            ModuleId::Named(n) => n.clone(),
            ModuleId::Routine(r) => r.name.clone(),
            ModuleId::Trigger { on, name } => ObjectName::new(on.schema.clone(), name.clone()),
        }
    }

    /// The name another module's definition would use to reference this one,
    /// where such a reference is possible at all.
    ///
    /// `None` for a trigger: nothing names a trigger in its body, and the one
    /// ordering edge a trigger has is [`ModuleId::attached_to`].
    pub fn referenced_name(&self) -> Option<ObjectName> {
        match self {
            ModuleId::Named(n) => Some(n.clone()),
            ModuleId::Routine(r) => Some(r.name.clone()),
            ModuleId::Trigger { .. } => None,
        }
    }

    /// The table or view a trigger is on; `None` for every other kind.
    pub fn attached_to(&self) -> Option<&ObjectName> {
        match self {
            ModuleId::Trigger { on, .. } => Some(on),
            ModuleId::Named(_) | ModuleId::Routine(_) => None,
        }
    }

    /// The declared argument types, where the kind has them.
    pub fn args(&self) -> Option<&[ColumnType]> {
        match self {
            ModuleId::Routine(r) => Some(&r.args),
            ModuleId::Named(_) | ModuleId::Trigger { .. } => None,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModuleIdError {
    #[error(
        "a module id is `schema.view`, `schema.function(argument types)` or \
         `schema.table.trigger`, got `{0}`"
    )]
    Shape(String),

    #[error("`{0}` contains an empty identifier")]
    EmptySegment(String),

    #[error("`{argument}` in `{whole}` is not a type: {source}")]
    Argument {
        whole: String,
        argument: String,
        #[source]
        source: crate::types::TypeParseError,
    },
}

impl fmt::Display for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleId::Named(n) => n.fmt(f),
            ModuleId::Routine(r) => r.fmt(f),
            ModuleId::Trigger { on, name } => write!(f, "{on}.{name}"),
        }
    }
}

/// Splits an argument list at the commas that separate arguments, and not at
/// the ones inside a type's own modifier.
///
/// The two are the same character: `decimal(10,2)` is one argument with a
/// comma in it, and `integer,text` is two without. Only nesting depth tells
/// them apart, so a plain `split(',')` turned `app.f(decimal(10,2))` into
/// `decimal(10` and `2)` — a valid declaration that would not parse back out
/// of the JSON map key its own `Display` had written (PITFALLS: a round trip
/// tested only on the simple case).
///
/// `None` if the parentheses do not balance, which the caller reports as a
/// malformed identity rather than guessing where the argument ended.
fn split_top_level(args: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in args.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.checked_sub(1)?,
            ',' if depth == 0 => {
                parts.push(&args[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    parts.push(&args[start..]);
    Some(parts)
}

impl FromStr for ModuleId {
    type Err = ModuleIdError;

    /// Shape decides the variant, and only shape: parentheses mean a routine
    /// (`app.f()` is one that takes nothing), three dotted parts mean a
    /// trigger. Nothing here knows which kinds a dialect lets overload — that
    /// is `Dialect::overloads`, checked where the dialect is present.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(open) = s.find('(') {
            let Some(args) = s.strip_suffix(')').map(|t| &t[open + 1..]) else {
                return Err(ModuleIdError::Shape(s.to_owned()));
            };
            let name: ObjectName = s[..open]
                .parse()
                .map_err(|_| ModuleIdError::Shape(s.to_owned()))?;
            let mut types = Vec::new();
            if !args.trim().is_empty() {
                for arg in
                    split_top_level(args).ok_or_else(|| ModuleIdError::Shape(s.to_owned()))?
                {
                    types.push(arg.parse().map_err(|e| ModuleIdError::Argument {
                        whole: s.to_owned(),
                        argument: arg.trim().to_owned(),
                        source: e,
                    })?);
                }
            }
            return Ok(ModuleId::Routine(RoutineId::new(name, types)));
        }
        let parts: Vec<&str> = s.split('.').collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err(ModuleIdError::EmptySegment(s.to_owned()));
        }
        match parts.as_slice() {
            [schema, name] => Ok(ModuleId::Named(ObjectName::new(*schema, *name))),
            [schema, table, name] => Ok(ModuleId::Trigger {
                on: ObjectName::new(*schema, *table),
                name: (*name).to_owned(),
            }),
            _ => Err(ModuleIdError::Shape(s.to_owned())),
        }
    }
}

impl TryFrom<String> for ModuleId {
    type Error = ModuleIdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<ModuleId> for String {
    fn from(id: ModuleId) -> String {
        id.to_string()
    }
}

// The map key of `Schema::modules` and of `ModuleDeps`, so it crosses JSON as a
// string like every other name in this model.
impl serde::Serialize for ModuleId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for ModuleId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ModuleKind {
    View,
    Procedure,
    Function,
    Trigger,
}

impl ModuleKind {
    /// The word the user writes as the file's leading key, and the one an error
    /// message uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            ModuleKind::View => "view",
            ModuleKind::Procedure => "procedure",
            ModuleKind::Function => "function",
            ModuleKind::Trigger => "trigger",
        }
    }

    pub const ALL: [ModuleKind; 4] = [
        ModuleKind::View,
        ModuleKind::Procedure,
        ModuleKind::Function,
        ModuleKind::Trigger,
    ];
}

impl std::fmt::Display for ModuleKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One view, procedure, function or trigger.
///
/// As everywhere else in the model, the container holds the name: a module's
/// identity is the key in [`crate::Schema::modules`], a [`ModuleId`].
///
/// That is where the table a trigger is on lives, and where a routine's
/// argument types live. Both were fields once; both are part of what the
/// object *is* (ADR-0009 §1), and a field beside a key that has to agree with
/// it is the bug that cannot detect itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Module {
    pub kind: ModuleKind,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// The body, kept verbatim and **never parsed**.
    ///
    /// The same rule as check and default expressions (SPEC §8.2): the database
    /// is the normalizer. After an apply the stored text is read back, so both
    /// sides of the drift check live in the engine's own space.
    pub definition: String,
}

/// Explicit creation-order edges, `module -> the modules it needs first`.
///
/// Keyed by [`ModuleId`] on **both** sides: under a name-only key one entry
/// for `app.f` was shared by `app.f(integer)` and `app.f(text)`, which are two
/// objects with two different bodies and two different orders.
///
/// # Why this is not a field of [`Module`]
///
/// Order of creation is invisible in the database, so a `depends_on:` inside
/// the model would make a declared module compare unequal to the identical
/// module read back from the catalog — inviolable constraint 1, and drift
/// crying wolf on every run. It travels beside the model the way `strategy:`
/// does, and for the same reason: it says how to get there, not where to go.
pub type ModuleDeps = BTreeMap<ModuleId, BTreeSet<ModuleId>>;

/// The annotations that travel beside the model.
///
/// `pbps-load` returns them separately from the [`crate::Schema`], the differ
/// attaches or applies them, and neither can ever take part in a comparison of
/// two states.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hints {
    /// Per-table execution strategy (ADR-0003).
    pub strategies: crate::strategy::Strategies,
    /// Per-module creation-order edges (ADR-0002).
    pub module_deps: ModuleDeps,
}

/// Whether `definition` mentions `name`, by a best-effort identifier scan.
///
/// # Why scanning text does not break "expressions are never parsed"
///
/// §8.2's rule is about **comparison**: pbps must never decide that two
/// definitions differ by understanding what they say. This asks a much smaller
/// question — does this text contain this identifier — and its answer only
/// picks a creation order. The failure mode is safe by construction: a wrong
/// order fails the CREATE inside the plan's transaction, everything rolls back,
/// and the environment is unchanged. The escape hatch for the cases it gets
/// wrong is [`ModuleDeps`].
pub fn references(definition: &str, name: &ObjectName) -> bool {
    let haystack = scannable(definition);
    let schema = name.schema.to_ascii_lowercase();
    let object = name.name.to_ascii_lowercase();

    // The qualified form, and the bare one — a definition written inside its
    // own schema very often omits the qualifier.
    contains_word(&haystack, &format!("{schema}.{object}")) || contains_word(&haystack, &object)
}

/// The definition with everything that is not code blanked out.
///
/// String literals and both comment forms are replaced by spaces, character for
/// character, so line structure and offsets survive. A **quoted identifier**
/// (`[name]`, or `"name"` under QUOTED_IDENTIFIER ON, which is the only setting
/// pbps manages) is passed through untouched: it is a name, which is exactly
/// what the callers are looking for.
///
/// # Why this is still not parsing SQL
///
/// It answers only "is this position inside a literal or a comment", which
/// every SQL lexer agrees on and no dialect argues about. Nothing here
/// understands what the code *says* — that rule (§8.2) is about comparison, and
/// this feeds two questions that are not comparisons: which modules a
/// definition names, and whether it contains a `GO`. Both were asked of the raw
/// text before, where a name inside a comment invented a dependency edge and a
/// `GO` inside a literal refused a valid procedure.
pub fn code_only(definition: &str) -> String {
    lexical_code(definition, true)
}

/// The definition with literals, comments, and quoted identifiers blanked.
///
/// [`code_only`] retains quoted identifiers because dependency and batch scans
/// need their names. Keyword detection needs the opposite: `[null]` and
/// `"try_cast"` are identifiers, not the SQL constructs their contents happen
/// to spell.
pub(crate) fn code_without_quoted_identifiers(definition: &str) -> String {
    lexical_code(definition, false)
}

/// The characters SQL Server's lexer takes as the end of a `--` comment.
///
/// Exactly these two. The others that look like candidates — NEL (U+0085),
/// LINE SEPARATOR (U+2028), form feed and vertical tab — were tried against
/// the engine and left the comment open, so treating them as line endings here
/// would blank code the engine runs.
fn is_line_ending(ch: char) -> bool {
    ch == '\n' || ch == '\r'
}

fn lexical_code(definition: &str, keep_quoted_identifiers: bool) -> String {
    enum At {
        Code,
        /// Inside `'...'`: blanked, because its contents are data.
        Literal,
        /// Inside `[...]` or `"..."`: its contents are a name. The flag marks
        /// the second delimiter in an escaped pair (`]]` or `""`) so it cannot
        /// also close the identifier.
        Ident(char, bool),
        Line,
        /// SQL Server block comments nest. `depth` tracks the unmatched
        /// openers; `seen` prevents an opener's `*` from also closing it in
        /// the overlapping spelling `/*/`.
        Block {
            depth: usize,
            seen: usize,
        },
    }
    let mut out = String::with_capacity(definition.len());
    let mut at = At::Code;
    let bytes = definition.as_bytes();
    // A line ending always survives: it ends a line comment, and the `GO`
    // check reads lines. Both `\n` and `\r` count, because the engine ends a
    // `--` comment at either — measured, not assumed: a bare CR terminated it,
    // while NEL, U+2028, form feed and vertical tab did not. Blanking the CR
    // to a space would make `-- note\rNULL` read as one long comment, and a
    // required column with that default would skip the gate and fail at apply.
    fn blank(out: &mut String, ch: char) {
        if is_line_ending(ch) {
            out.push(ch);
        } else {
            for _ in 0..ch.len_utf8() {
                out.push(' ');
            }
        }
    }
    for (i, ch) in definition.char_indices() {
        let next = bytes.get(i + ch.len_utf8()).copied();
        match at {
            At::Literal => {
                // A doubled `''` needs no special case: the first closes and the
                // second opens again, and everything between is blanked either
                // way.
                if ch == '\'' {
                    at = At::Code;
                }
                blank(&mut out, ch);
            }
            At::Ident(closing, escaped_closer) => {
                if escaped_closer {
                    at = At::Ident(closing, false);
                } else if ch == closing {
                    at = if next == Some(closing as u8) {
                        At::Ident(closing, true)
                    } else {
                        At::Code
                    };
                } else {
                    at = At::Ident(closing, false);
                }
                if keep_quoted_identifiers {
                    out.push(ch);
                } else {
                    blank(&mut out, ch);
                }
            }
            At::Line => {
                if is_line_ending(ch) {
                    at = At::Code;
                }
                blank(&mut out, ch);
            }
            At::Block { depth, seen } => {
                at = if ch == '/' && seen >= 2 && bytes[i - 1] == b'*' {
                    if depth == 1 {
                        At::Code
                    } else {
                        At::Block {
                            depth: depth - 1,
                            seen: 2,
                        }
                    }
                } else if ch == '/' && next == Some(b'*') {
                    At::Block {
                        depth: depth + 1,
                        seen: 0,
                    }
                } else {
                    At::Block {
                        depth,
                        seen: seen + 1,
                    }
                };
                blank(&mut out, ch);
            }
            At::Code => match (ch, next) {
                ('-', Some(b'-')) => {
                    at = At::Line;
                    blank(&mut out, ch);
                }
                ('/', Some(b'*')) => {
                    at = At::Block { depth: 1, seen: 0 };
                    blank(&mut out, ch);
                }
                ('\'', _) => {
                    at = At::Literal;
                    blank(&mut out, ch);
                }
                ('[' | '"', _) => {
                    at = At::Ident(if ch == '[' { ']' } else { ch }, false);
                    if keep_quoted_identifiers {
                        out.push(ch);
                    } else {
                        blank(&mut out, ch);
                    }
                }
                _ => out.push(ch),
            },
        }
    }
    out
}

/// Lower-cases, drops the quoting characters and closes the gaps around dots,
/// so that `[Dbo] . [V]` and `dbo.v` become one string to search.
fn scannable(definition: &str) -> String {
    let lowered = code_only(definition).to_ascii_lowercase();
    let unquoted: String = lowered.chars().filter(|c| !"[]\"`".contains(*c)).collect();
    let mut out = String::with_capacity(unquoted.len());
    for (i, ch) in unquoted.char_indices() {
        if ch.is_whitespace() {
            let before = unquoted[..i].chars().next_back();
            let after = unquoted[i + ch.len_utf8()..]
                .chars()
                .find(|c| !c.is_whitespace());
            // Whitespace that only separates a qualifier from its dot is
            // noise; everywhere else it is a boundary and must be kept.
            if before == Some('.') || after == Some('.') {
                continue;
            }
        }
        out.push(ch);
    }
    out
}

/// Whether `needle` occurs with no identifier character on either side.
fn contains_word(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        let start = from + at;
        let end = start + needle.len();
        if !is_ident_char(haystack[..start].chars().next_back())
            && !is_ident_char(haystack[end..].chars().next())
        {
            return true;
        }
        from = end;
    }
    false
}

/// A dot counts: `dbo.active_customer` must not match inside
/// `sales.dbo.active_customer`, and `active_customer` must not match inside
/// `dbo.active_customer` — the qualified needle is tried first and answers that
/// case properly.
fn is_ident_char(c: Option<char>) -> bool {
    c.is_some_and(|c| is_regular_identifier_continue(c) || c == '.')
}

/// Whether a character can continue an unquoted SQL Server identifier.
///
/// Unicode letters and decimal digits are covered conservatively by
/// `is_alphanumeric`; SQL Server additionally admits these four symbols after
/// the first character. Keyword and dependency scans share this boundary so
/// `seq$null` cannot mean one token to one and two tokens to the other.
pub(crate) fn is_regular_identifier_continue(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '@' | '#' | '$')
}

/// The order in which modules must be created: a module comes after everything
/// it references.
///
/// Cycles cannot be ordered, and the tool does not pretend otherwise: the
/// members of one are emitted in name order, which is deterministic, and the
/// engine has the last word inside the plan's transaction. (A genuine cycle
/// between views is not creatable by any order.)
///
/// One edge the scan cannot supply: between the overloads of one routine name,
/// which the scan cannot tell apart, only `depends_on:` orders (DECISIONS 212).
pub fn creation_order(modules: &BTreeMap<ModuleId, Module>, deps: &ModuleDeps) -> Vec<ModuleId> {
    let names: Vec<ModuleId> = modules.keys().cloned().collect();

    // needs[a] = the modules `a` must follow.
    let mut needs: BTreeMap<&ModuleId, BTreeSet<&ModuleId>> = BTreeMap::new();
    for name in &names {
        let module = &modules[name];
        let mut set: BTreeSet<&ModuleId> = BTreeSet::new();
        for other in &names {
            if other == name {
                continue;
            }
            let declared = deps.get(name).is_some_and(|d| d.contains(other));
            // A trigger's target is not named in its definition — the emitter
            // writes it into the `ON` clause — so the identity's own table has
            // to be read directly. It matters only when the target is itself a
            // module: a trigger on a view has to be created after that view.
            let attached = name
                .attached_to()
                .is_some_and(|t| other.referenced_name().as_ref() == Some(t));
            // The scan matches a *name*, and where a kind overloads a name is
            // not an identity (ADR-0009 §1). So `app.f` in a routine's body
            // matches every `app.f(...)`: between the overloads of one name an
            // automatic edge would order each against all its siblings, and a
            // body that mentions its own name — a recursive overload, or two
            // that call each other one way — would make a cycle out of them.
            // `depends_on:` could not repair that, because it adds an edge and
            // cannot remove one. Among the modules that share a routine's name,
            // therefore, only `depends_on:` orders (DECISIONS 212).
            //
            // A trigger is named by nothing, so nothing can reference it.
            let sibling = matches!(name, ModuleId::Routine(_))
                && other.referenced_name() == name.referenced_name();
            let referenced = !sibling
                && other
                    .referenced_name()
                    .is_some_and(|n| references(&module.definition, &n));
            if declared || attached || referenced {
                set.insert(other);
            }
        }
        // A trigger on a *table* plays no part here: tables are created by an
        // earlier ordering class in any case.
        needs.insert(name, set);
    }

    let mut done: BTreeSet<&ModuleId> = BTreeSet::new();
    let mut out: Vec<ModuleId> = Vec::new();
    // Kahn's algorithm, taking the name-least ready module each round so that
    // two runs over the same declarations produce the same plan.
    loop {
        let ready: Vec<&ModuleId> = names
            .iter()
            .filter(|n| !done.contains(n))
            .filter(|n| needs[*n].iter().all(|d| done.contains(d)))
            .collect();
        if ready.is_empty() {
            break;
        }
        for n in ready {
            done.insert(n);
            out.push(n.clone());
        }
    }
    // Whatever is left is in a cycle: deterministic order, and the engine
    // decides.
    for n in &names {
        if !done.contains(n) {
            out.push(n.clone());
        }
    }
    out
}

/// Problems that need the whole schema to see, and no dialect.
///
/// Each would otherwise surface only at apply time, as an engine error on a
/// database that is half-changed:
///
/// - a module whose kind and whose identity disagree: a trigger that does not
///   say which table it is on, or a view under a trigger's identity;
/// - a trigger on a table nobody declares: the tool would be managing a
///   trigger on an object it does not manage, and a `pull` of that environment
///   would not reproduce it.
///
/// Two rules that were here are not, because they are the engine's and not the
/// model's: whether a module competes with a table for its name, and which
/// kinds overload. Both moved to `pbps_dialect::check_module_names`, where the
/// dialect can answer them (ADR-0009 §1: views share `pg_class` with tables and
/// routines do not).
pub fn check_names(schema: &crate::schema::Schema) -> Vec<String> {
    let mut problems = Vec::new();
    for (id, module) in &schema.modules {
        match (module.kind, id) {
            (ModuleKind::Trigger, ModuleId::Trigger { on, .. }) => {
                // A view is as valid a target as a table: SQL Server supports
                // `INSTEAD OF` triggers on views, and `pull` reconstructs the
                // target from what it finds — so refusing one here would make
                // a database that has one impossible to round-trip.
                let on_a_view = schema.modules.iter().any(|(other, m)| {
                    m.kind == ModuleKind::View && other.referenced_name().as_ref() == Some(on)
                });
                if !schema.tables.contains_key(on) && !on_a_view {
                    problems.push(format!(
                        "trigger `{id}` is on `{on}`, which is not declared here as a table or a \
                         view"
                    ));
                }
            }
            (ModuleKind::Trigger, _) => problems.push(format!(
                "trigger `{id}` does not say which table it is on (`on:`)"
            )),
            (kind, ModuleId::Trigger { on, .. }) => problems.push(format!(
                "`{id}` is a {kind} and cannot be `on: {on}`; only a trigger names a table"
            )),
            (_, ModuleId::Named(_) | ModuleId::Routine(_)) => {}
        }
        if module.definition.trim().is_empty() {
            problems.push(format!("`{id}` has an empty definition"));
        }
    }
    problems
}

/// Every `depends_on:` target has to be a declared module.
///
/// [`creation_order`] considers dependencies only between modules it is
/// iterating, so an unknown name is silently a no-op. The declaration then
/// passes every check while the ordering edge its author asked for does not
/// exist — and the failure surfaces much later, as an apply that emits the
/// dependent module first (the same argument as ADR-0003's rejection of unknown
/// `strategy:` keys).
pub fn check_dependencies(schema: &crate::schema::Schema, deps: &ModuleDeps) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, on) in deps {
        for target in on {
            if target == name {
                problems.push(format!("`{name}` lists itself in `depends_on`"));
            } else if !schema.modules.contains_key(target) {
                problems.push(format!(
                    "`{name}` depends on `{target}`, which is not a declared module; the ordering \
                     it asks for would silently not happen"
                ));
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Schema, Table};

    fn n(s: &str) -> ObjectName {
        s.parse().unwrap()
    }

    /// A module identity, by the same shape rules the file uses.
    fn id(s: &str) -> ModuleId {
        s.parse().unwrap()
    }

    fn module(kind: ModuleKind, definition: &str) -> Module {
        Module {
            kind,
            description: None,
            definition: definition.to_owned(),
        }
    }

    fn view(definition: &str) -> Module {
        module(ModuleKind::View, definition)
    }

    #[test]
    fn a_qualified_reference_is_found_in_every_spelling() {
        for definition in [
            "SELECT * FROM dbo.active_customer",
            "SELECT * FROM [dbo].[active_customer]",
            "select a from DBO.ACTIVE_CUSTOMER c",
            "SELECT * FROM active_customer",
        ] {
            assert!(
                references(definition, &n("dbo.active_customer")),
                "{definition}"
            );
        }
    }

    /// A name that merely appears inside a longer identifier is not a
    /// reference; treating it as one would invent an edge and, with enough of
    /// them, a cycle.
    #[test]
    fn a_name_inside_a_longer_identifier_is_not_a_reference() {
        for definition in [
            "SELECT * FROM dbo.active_customer_archive",
            "SELECT * FROM dbo.old_active_customer",
            "SELECT @active_customer",
            "SELECT seq$active_customer",
            "SELECT seq#active_customer",
            "SELECT 序列active_customer",
        ] {
            assert!(
                !references(definition, &n("dbo.active_customer")),
                "{definition}"
            );
        }
        assert!(!references("SELECT 1", &n("dbo.active_customer")));
    }

    fn modules(specs: &[(&str, &str)]) -> BTreeMap<ModuleId, Module> {
        specs
            .iter()
            .map(|(name, def)| (id(name), view(def)))
            .collect()
    }

    /// A view over a view has to be created second, or the CREATE fails.
    #[test]
    fn a_referenced_module_is_created_first() {
        let m = modules(&[
            ("dbo.top", "SELECT * FROM dbo.middle"),
            ("dbo.middle", "SELECT * FROM dbo.base"),
            ("dbo.base", "SELECT * FROM dbo.customer"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("dbo.base"), id("dbo.middle"), id("dbo.top")]
        );
    }

    /// The escape hatch has to work where the scan sees nothing — a view
    /// reached only through a synonym, say.
    #[test]
    fn an_explicit_dependency_orders_what_the_scan_cannot_see() {
        let m = modules(&[("dbo.a", "SELECT 1"), ("dbo.b", "SELECT 2")]);
        let mut deps = ModuleDeps::default();
        deps.insert(id("dbo.a"), BTreeSet::from([id("dbo.b")]));
        assert_eq!(creation_order(&m, &deps), vec![id("dbo.b"), id("dbo.a")]);
    }

    /// Unrelated modules must come out in one fixed order, or two runs over the
    /// same declarations would produce plans that diff against each other.
    #[test]
    fn independent_modules_come_out_in_a_stable_order() {
        let m = modules(&[
            ("dbo.z", "SELECT 1"),
            ("dbo.a", "SELECT 2"),
            ("dbo.m", "SELECT 3"),
        ]);
        let first = creation_order(&m, &ModuleDeps::default());
        assert_eq!(first, vec![id("dbo.a"), id("dbo.m"), id("dbo.z")]);
        for _ in 0..5 {
            assert_eq!(creation_order(&m, &ModuleDeps::default()), first);
        }
    }

    /// The scan matches a name, and a name is not an identity where routines
    /// overload: `app.f` in a body matches every `app.f(...)`. So the overloads
    /// of one name must not order each other automatically — a body mentioning
    /// its own name would otherwise put its siblings in a cycle with it, where
    /// `depends_on:` can add an edge but not take one away. Only `depends_on:`
    /// orders them, and it must be obeyed.
    #[test]
    fn a_reference_to_its_own_name_does_not_order_an_overload_against_its_siblings() {
        let mut m = BTreeMap::new();
        m.insert(
            id("app.f(integer)"),
            module(ModuleKind::Function, "SELECT app.f(1::text)"),
        );
        m.insert(
            id("app.f(text)"),
            module(ModuleKind::Function, "SELECT app.f(1)"),
        );
        // A third module naming `app.f` cannot tell the overloads apart either,
        // so it follows all of them — the conservative reading (DECISIONS 212).
        m.insert(id("app.v"), view("SELECT * FROM app.f(1)"));

        let mut deps = ModuleDeps::default();
        deps.insert(id("app.f(integer)"), BTreeSet::from([id("app.f(text)")]));

        assert_eq!(
            creation_order(&m, &deps),
            vec![id("app.f(text)"), id("app.f(integer)"), id("app.v")],
            "the explicit hint orders the siblings, and the view follows both"
        );
    }

    /// The negative half: two overloads that mention the name and declare
    /// nothing have no edge either way, so neither is held back. A caller shows
    /// the difference that name order alone would hide — with an automatic edge
    /// the two are a cycle, nothing is ever ready, and the fallback emits every
    /// module in name order, which puts the caller *before* what it calls.
    #[test]
    fn overloads_that_declare_nothing_are_not_a_cycle_that_reorders_their_caller() {
        let mut m = BTreeMap::new();
        for args in ["integer", "text"] {
            m.insert(
                id(&format!("app.f({args})")),
                module(ModuleKind::Function, "SELECT app.f(1)"),
            );
        }
        m.insert(id("app.caller"), view("SELECT * FROM app.f(1)"));
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("app.f(integer)"), id("app.f(text)"), id("app.caller")],
            "the overloads are ready together, and the caller follows both"
        );
    }

    /// And the edge that is real stays: a routine referencing a *differently*
    /// named module is ordered after it, overloading or not.
    #[test]
    fn a_routine_still_follows_a_module_it_references_under_another_name() {
        let mut m = BTreeMap::new();
        m.insert(
            id("app.f(integer)"),
            module(ModuleKind::Function, "SELECT * FROM app.base"),
        );
        m.insert(id("app.base"), view("SELECT 1"));
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("app.base"), id("app.f(integer)")]
        );
    }

    /// A cycle cannot be created in any order. It must not hang or drop a
    /// module either: everything is emitted, and the engine gives the verdict.
    #[test]
    fn a_cycle_still_yields_every_module_once() {
        let m = modules(&[
            ("dbo.a", "SELECT * FROM dbo.b"),
            ("dbo.b", "SELECT * FROM dbo.a"),
        ]);
        let order = creation_order(&m, &ModuleDeps::default());
        assert_eq!(order.len(), 2);
        assert!(order.contains(&id("dbo.a")) && order.contains(&id("dbo.b")));
    }

    fn trigger_id(on: &str, name: &str) -> ModuleId {
        ModuleId::Trigger {
            on: n(on),
            name: name.to_owned(),
        }
    }

    #[test]
    fn a_trigger_must_name_a_declared_table() {
        let mut schema = Schema::default();
        let trigger = module(ModuleKind::Trigger, "AFTER INSERT AS SELECT 1");
        // An identity with no table at all: the loader refuses one, and a
        // state or plan read from JSON can still carry it.
        schema.modules.insert(id("dbo.trg"), trigger.clone());
        assert!(check_names(&schema)[0].contains("does not say which table"));

        schema.modules.clear();
        schema
            .modules
            .insert(trigger_id("dbo.absent", "trg"), trigger.clone());
        assert!(check_names(&schema)[0].contains("not declared here"));

        schema.tables.insert(n("dbo.absent"), Table::default());
        assert!(check_names(&schema).is_empty());
    }

    /// Two triggers of one name on two tables are two objects, not one
    /// declaration overwriting the other (ADR-0009 §1, measured on
    /// PostgreSQL 18: `DROP TRIGGER audit` is a syntax error).
    #[test]
    fn one_trigger_name_on_two_tables_is_two_modules() {
        let mut schema = Schema::default();
        schema.tables.insert(n("tg.orders"), Table::default());
        schema.tables.insert(n("tg.customers"), Table::default());
        let trigger = module(ModuleKind::Trigger, "AFTER INSERT AS SELECT 1");
        schema
            .modules
            .insert(trigger_id("tg.orders", "audit"), trigger.clone());
        schema
            .modules
            .insert(trigger_id("tg.customers", "audit"), trigger);
        assert_eq!(schema.modules.len(), 2);
        assert!(
            check_names(&schema).is_empty(),
            "{:?}",
            check_names(&schema)
        );
    }

    /// SQL Server allows `INSTEAD OF` triggers on views, and `pull` rebuilds
    /// the `on:` from whatever it finds — so a target that is a declared view
    /// has to load, or such a database could never be round-tripped.
    #[test]
    fn a_trigger_may_be_attached_to_a_declared_view() {
        let mut schema = Schema::default();
        schema.modules.insert(id("dbo.v"), view("SELECT 1 AS one"));
        schema.modules.insert(
            trigger_id("dbo.v", "trg"),
            module(ModuleKind::Trigger, "INSTEAD OF INSERT AS SELECT 1"),
        );
        assert!(
            check_names(&schema).is_empty(),
            "{:?}",
            check_names(&schema)
        );

        // And the view has to exist before the trigger can be attached to it.
        // The definition never names it — the emitter writes it into the `ON`
        // clause — so only the identity can supply that edge.
        let order = creation_order(&schema.modules, &ModuleDeps::default());
        assert_eq!(order, vec![id("dbo.v"), trigger_id("dbo.v", "trg")]);
    }

    /// A target that is neither a declared table nor a declared view is still
    /// refused: the trigger would be created on an object pbps does not manage.
    #[test]
    fn a_trigger_on_a_procedure_is_still_refused() {
        let mut schema = Schema::default();
        schema
            .modules
            .insert(id("dbo.p"), module(ModuleKind::Procedure, "AS SELECT 1"));
        schema.modules.insert(
            trigger_id("dbo.p", "trg"),
            module(ModuleKind::Trigger, "AFTER INSERT AS SELECT 1"),
        );
        assert!(
            check_names(&schema)[0].contains("not declared here as a table or a view"),
            "{:?}",
            check_names(&schema)
        );
    }

    /// A comma inside a type's modifier is not an argument separator, and the
    /// two are the same character. `app.f(decimal(10, 2))` is one argument;
    /// splitting it flat made `decimal(10` and ` 2)`, so a routine the model
    /// can hold serialized into a key that would not parse back.
    #[test]
    fn a_comma_inside_a_modifier_does_not_separate_arguments() {
        let one: ModuleId = "app.f(decimal(10, 2))".parse().unwrap();
        assert_eq!(one.args().unwrap().len(), 1, "{one}");
        let three: ModuleId = "app.f(decimal(10, 2),text,numeric(38, 10))"
            .parse()
            .unwrap();
        assert_eq!(three.args().unwrap().len(), 3, "{three}");
        // Spacing is layout, not identity: the compact spelling a human might
        // type is the same routine as the one `Display` writes.
        assert_eq!("app.f(decimal(10,2))".parse::<ModuleId>().unwrap(), one);

        // Unbalanced is refused rather than guessed at. A silent split here
        // would invent an argument list nobody declared.
        for bad in [
            "app.f(decimal(10, 2)",
            "app.f(decimal10, 2))",
            "app.f(decimal(10, 2)))",
        ] {
            assert!(
                bad.parse::<ModuleId>().is_err(),
                "`{bad}` parsed as an identity"
            );
        }
    }

    /// The three shapes, through the string form the JSON map key uses.
    /// Parentheses mean a routine — `app.f()` is one that takes nothing —
    /// and three dotted parts mean a trigger (ADR-0009 §1).
    #[test]
    fn every_identity_round_trips_through_its_string_form() {
        for spelling in [
            "app.v",
            "app.f(integer,text)",
            // A modifier with its own comma: the argument separator and the
            // one inside `decimal(10, 2)` are the same character, and only
            // nesting tells them apart. Spelled as `ColumnType` spells it,
            // because that is what wrote the key.
            "app.f(decimal(10, 2))",
            "app.f(decimal(10, 2),text,numeric(38, 10))",
            "app.f()",
            "app.orders.audit",
        ] {
            let parsed: ModuleId = spelling.parse().unwrap();
            assert_eq!(parsed.to_string(), spelling);
            assert_eq!(
                serde_json::from_str::<ModuleId>(&serde_json::to_string(&parsed).unwrap()).unwrap(),
                parsed,
                "{spelling} does not survive JSON"
            );
        }
        assert_eq!(id("app.v"), ModuleId::Named(n("app.v")));
        assert!(matches!(id("app.f(int)"), ModuleId::Routine(_)));
        assert_eq!(
            id("app.orders.audit"),
            ModuleId::Trigger {
                on: n("app.orders"),
                name: "audit".to_owned()
            }
        );
        for bad in [
            "app",
            "a.b.c.d",
            "app.f(",
            "app.f(int",
            "app..f",
            "app.f(,)",
        ] {
            assert!(bad.parse::<ModuleId>().is_err(), "`{bad}` must not parse");
        }
    }

    /// Constraint 1: two semantically identical schemas are `==`. The
    /// declaration's spelling of a type is not part of what a routine *is*,
    /// so `app.f(int, text)` and `app.f(int,text)` are one key — the type
    /// parser folds whitespace and case, and the dialect folds the rest
    /// (`normalize_routine_arg`), which is why this is not a string key.
    #[test]
    fn one_routine_written_two_ways_is_one_key() {
        let mut modules = BTreeMap::new();
        modules.insert(id("app.f(int, text)"), view("SELECT 1"));
        modules.insert(id("app.f(INT,text)"), view("SELECT 2"));
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[&id("app.f(int,text)")].definition, "SELECT 2");
    }

    /// Two overloads are two objects with two bodies, and each carries its
    /// own ordering edge: under a name-only key one `depends_on` entry was
    /// shared by both.
    #[test]
    fn two_overloads_are_two_modules_with_their_own_dependencies() {
        let mut schema = Schema::default();
        schema
            .modules
            .insert(id("app.f(integer)"), module(ModuleKind::Function, "AS 1"));
        schema
            .modules
            .insert(id("app.f(text)"), module(ModuleKind::Function, "AS 2"));
        schema.modules.insert(id("app.v"), view("SELECT 1"));
        assert_eq!(schema.modules.len(), 3);

        let mut deps = ModuleDeps::default();
        deps.insert(id("app.f(integer)"), BTreeSet::from([id("app.v")]));
        assert!(check_dependencies(&schema, &deps).is_empty());

        // And one that names an overload nobody declared is still caught.
        let mut wrong = ModuleDeps::default();
        wrong.insert(id("app.f(integer)"), BTreeSet::from([id("app.f(bigint)")]));
        assert!(
            check_dependencies(&schema, &wrong)[0].contains("not a declared module"),
            "{:?}",
            check_dependencies(&schema, &wrong)
        );
    }

    /// A name inside a comment or a string is not a reference. Inventing the
    /// edge is worse than missing one: it can close a cycle, and a cycle falls
    /// back to name order — while `depends_on:` can only *add* edges, so the
    /// user has no way to take the invented one back.
    #[test]
    fn a_name_in_a_comment_or_a_literal_is_not_a_dependency() {
        let target: ObjectName = "dbo.active_customer".parse().unwrap();
        for definition in [
            "SELECT 1 -- superseded by dbo.active_customer",
            "/* see dbo.active_customer */ SELECT 1",
            "SELECT 1 /* outer /* nested */ dbo.active_customer */",
            "SELECT 'dbo.active_customer' AS note",
        ] {
            assert!(
                !references(definition, &target),
                "{definition} must not count as a reference"
            );
        }
        // A quoted identifier still does: it is a name, not text.
        assert!(references("SELECT * FROM [dbo].[active_customer]", &target));
        assert!(references(
            "SELECT * FROM \"dbo\".\"active_customer\"",
            &target
        ));
    }

    /// The engine ends a `--` comment at a bare carriage return, so a scan
    /// that waited for `\n` blanked real code: the name after the CR is a
    /// reference the engine resolves, and a `NULL` after it is the keyword.
    /// The other vertical-whitespace characters were measured *not* to end
    /// the comment, and the scan must agree in that direction too — or it
    /// would invent a reference from text the engine never reads.
    #[test]
    fn a_carriage_return_ends_a_line_comment_and_other_vertical_whitespace_does_not() {
        let target: ObjectName = "dbo.active_customer".parse().unwrap();
        assert!(references(
            "SELECT 1 -- note\rUNION ALL SELECT id FROM dbo.active_customer",
            &target
        ));
        assert!(references(
            "SELECT 1 -- note\r\nUNION ALL SELECT id FROM dbo.active_customer",
            &target
        ));
        assert_eq!(
            code_only("a -- b\rc\r\nd"),
            "a     \rc\r\nd",
            "line endings survive so line structure does"
        );
        for (name, separator) in [
            ("NEL", '\u{85}'),
            ("LINE SEPARATOR", '\u{2028}'),
            ("form feed", '\u{0c}'),
            ("vertical tab", '\u{0b}'),
        ] {
            let definition =
                format!("SELECT 1 -- note{separator}UNION ALL SELECT id FROM dbo.active_customer");
            assert!(
                !references(&definition, &target),
                "{name} does not end a line comment on the engine"
            );
        }
    }

    /// An ordering edge that silently does not exist is the failure this check
    /// is for: the declaration looks right and the apply emits in the wrong
    /// order much later.
    #[test]
    fn a_depends_on_target_that_is_not_declared_is_refused() {
        let mut schema = Schema::default();
        schema.modules.insert(id("dbo.a"), view("SELECT 1"));
        schema.modules.insert(id("dbo.b"), view("SELECT 2"));

        let mut deps = ModuleDeps::default();
        deps.insert(id("dbo.b"), [id("dbo.a")].into_iter().collect());
        assert!(check_dependencies(&schema, &deps).is_empty());

        deps.insert(id("dbo.b"), [id("dbo.typo")].into_iter().collect());
        assert!(
            check_dependencies(&schema, &deps)[0].contains("not a declared module"),
            "{:?}",
            check_dependencies(&schema, &deps)
        );

        deps.insert(id("dbo.b"), [id("dbo.b")].into_iter().collect());
        assert!(check_dependencies(&schema, &deps)[0].contains("lists itself"));
    }

    /// A view under a trigger's identity would read as if the table meant
    /// something; it does not, and a declaration that quietly means nothing is
    /// the failure mode this tool is built to avoid. The loader refuses the
    /// `on:` that would produce it; this is the same check one layer in, for a
    /// state or plan that arrived as JSON.
    #[test]
    fn only_a_trigger_may_name_a_table() {
        let mut schema = Schema::default();
        schema
            .modules
            .insert(trigger_id("dbo.customer", "v"), view("SELECT 1"));
        assert!(check_names(&schema)[0].contains("only a trigger"));
    }

    #[test]
    fn an_empty_definition_is_reported() {
        let mut schema = Schema::default();
        schema.modules.insert(id("dbo.v"), view("   \n"));
        assert!(check_names(&schema)[0].contains("empty definition"));
    }
}
