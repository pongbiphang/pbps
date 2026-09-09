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

/// The qualified name of a database object: `schema.object`.
///
/// Deliberately the same type as [`TableName`]: SQL Server keeps tables and
/// modules in **one** `sys.objects` namespace per schema, so "a view may not be
/// named after a table" is not a rule to remember but a consequence of the two
/// names having one type. [`check_names`] is that consequence made checkable.
pub type ObjectName = TableName;

/// One argument type of a routine's identity, in the engine's own spelling.
///
/// **Not a [`ColumnType`], and the difference is not a nicety.** Measured on
/// PostgreSQL 18.6, a routine's identity is `proargtypes` rendered by
/// `format_type` — the same list `oid::regprocedure` prints — and it holds
/// spellings a column type cannot:
///
/// ```text
/// CREATE FUNCTION id.a(varchar(10), "char", int[], id.pos) ...
///   -> id.a(character varying,"char",integer[],id.pos)
/// ```
///
/// An array, a quoted name whose case the engine keeps, and a schema-qualified
/// user type. [`ColumnType`]'s base name admits ASCII alphanumerics, underscore
/// and space and lowercases what it holds, so of those four one is refused for
/// its brackets, one for its quotes, one for its dot, and the quoted one would
/// have its case taken away as well. And a dialect's column normalizer is a
/// closed catalogue of the engine's own type names, which refuses a domain the
/// user declared — while a routine may take one.
///
/// So this holds text and compares as text, for the reason
/// [`Module::definition`] does: **the engine is the normalizer.**
/// `Dialect::normalize_routine_arg` turns a declared spelling into the one the
/// catalog will show, and this type carries the result.
///
/// Modifiers are gone by then, not stripped here: the engine discards them
/// when it identifies a routine (`f(varchar(10))` and `f(varchar(20))` are one
/// function, ADR-0009 §1), which is the dialect's normalization to perform and
/// this type's to record.
///
/// # The one opinion it does have
///
/// Inviolable constraint 1 says two semantically identical schemas must be
/// `==`, and `app.f(int, text)` and `app.f(INT,text)` are one key, before any
/// dialect is present to say so — the loader builds the map, and an offline
/// `plan` compares two maps without a connection. So the text is canonicalized
/// on the way in, and **only where every SQL engine agrees**: outside double
/// quotes, ASCII case does not matter and whitespace around punctuation does
/// not either. Inside them nothing is touched, which is the whole reason this
/// is not a [`ColumnType`]: `"char"` is a type whose case the engine keeps,
/// and lowercasing it would name a different type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoutineArg(String);

impl RoutineArg {
    /// The spelling, unchanged.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a routine argument's text may not be.
///
/// Every rule here is about the *shape* the identity string needs, and none is
/// about which types exist: a type this model has never heard of is a type the
/// engine may still have, and refusing it here would refuse a valid routine
/// because the model's catalogue is younger than the database.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RoutineArgError {
    #[error("a routine argument type is empty")]
    Empty,

    /// Text that is not one type name: an unbalanced bracket, quote or
    /// parenthesis; a comma outside all of them, which is the character that
    /// separates one argument from the next; or, outside quotes, a character a
    /// type name is not written with.
    #[error("`{0}` is not one routine argument type")]
    Shape(String),
}

impl FromStr for RoutineArg {
    type Err = RoutineArgError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // ASCII whitespace, here and below: to this engine every non-ASCII
        // byte is an identifier character, a non-breaking space included.
        // Measured, `CREATE FUNCTION r8.f(v r8.a\u{a0}b)` is accepted and its
        // identity reads `r8.f(r8."a\u{a0}b")` — the byte kept and the name
        // quoted — while `r8.a b` with a plain space names no type at all. A
        // fold that took Unicode's word for what whitespace is turned the
        // first spelling into the second, and the key pointed at nothing.
        let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
        if t.is_empty() {
            return Err(RoutineArgError::Empty);
        }
        // A Unicode-escaped identifier first, so that the pass below sees the
        // plain quoted name it spells and nothing else has to know the form.
        let t = &*without_unicode_escapes(t)?;
        // Structure and canonical form in one pass, because the second needs
        // the first: whether a character is punctuation to fold around, or a
        // byte of a quoted name to leave alone, is what the quote state says.
        let mut out = String::with_capacity(t.len());
        let mut parens = 0usize;
        let mut brackets = 0usize;
        let mut quoted = false;
        let mut pending_space = false;
        let mut chars = t.char_indices();
        while let Some((i, c)) = chars.next() {
            if quoted {
                out.push(c);
                if c == '"' {
                    // A doubled quote is a quote inside the name, which is how
                    // both engines spell one, so it does not close the region.
                    if t[i + 1..].starts_with('"') {
                        out.push('"');
                        chars.next();
                    } else {
                        quoted = false;
                    }
                }
                continue;
            }
            if c.is_ascii_whitespace() {
                pending_space = !out.is_empty();
                continue;
            }
            match c {
                '(' => parens += 1,
                ')' => parens = parens.checked_sub(1).ok_or_else(|| shape(t))?,
                '[' => brackets += 1,
                ']' => brackets = brackets.checked_sub(1).ok_or_else(|| shape(t))?,
                ',' if parens == 0 && brackets == 0 => return Err(shape(t)),
                ',' | '"' | '.' | '_' => {}
                // Any non-ASCII byte is a name byte, which is the engine's own
                // rule (`continues_ident`): a letter, a symbol, a space that is
                // not the ASCII one.
                c if c.is_alphanumeric() || !c.is_ascii() => {}
                // Everything else. A type name is written with letters,
                // digits, `_`, `.`, and the punctuation above; a semicolon, an
                // apostrophe or the start of a comment is not one, and this
                // text is interpolated verbatim into `DROP FUNCTION` and
                // `GRANT`. A whitelist makes that statement safe by
                // construction instead of by review — and refusing an
                // unwritable name costs nothing, because no catalog returns
                // one.
                _ => return Err(shape(t)),
            }
            // A space between two words is part of the name — `timestamp with
            // time zone` — and a space beside punctuation is layout.
            //
            // `.` is in that punctuation, because the engine accepts a space
            // around a qualified type's dot and never writes one back:
            // **measured**, `CREATE FUNCTION md.spaced(a md . my_type)` is
            // accepted and its identity reads `md.spaced(md.my_type)`. Left
            // unfolded, the declared key and the catalog key differ, and every
            // plan drops and recreates a routine that never changed.
            if pending_space
                && !matches!(c, '(' | ')' | '[' | ']' | ',' | '.')
                && !out.ends_with(['(', '[', ',', '.'])
            {
                out.push(' ');
            }
            pending_space = false;
            if c == '"' {
                quoted = true;
                out.push(c);
            } else {
                // **ASCII only**, which is the engine's rule and not a
                // simplification of it: measured, `CREATE FUNCTION
                // mn.f(a mn.Ätype)` reads back as `mn.f(mn."Ätype")` — the
                // engine left the byte alone and quoted the name rather than
                // folding it. A Unicode fold would turn the declared spelling
                // into `ätype`, which is a *different* type name, and the key
                // would then point at nothing. The dialect's own folding is
                // ASCII (`unquoted` in the emitters), and this is the same
                // rule in the model.
                out.push(c.to_ascii_lowercase());
            }
        }
        if quoted || parens != 0 || brackets != 0 {
            return Err(shape(t));
        }
        Ok(RoutineArg(out))
    }
}

fn shape(s: &str) -> RoutineArgError {
    RoutineArgError::Shape(s.to_owned())
}

/// Every `U&"…"` outside a quoted region, with or without its `UESCAPE 'x'`,
/// rewritten as the plain quoted identifier it spells.
///
/// PostgreSQL accepts the form wherever an identifier goes, a routine's
/// argument type included: **measured**, `CREATE FUNCTION r12.a(v
/// r12.U&"\006doney")` has the identity `r12.a(r12.money)`, and with
/// `UESCAPE '!'` the escape character is the one given. Canonicalized here to
/// the quoted form rather than carried, so that one spelling of a name is one
/// key. A form that does not decode is refused as text that is not one
/// argument, which is what the engine says of it too.
fn without_unicode_escapes(t: &str) -> Result<std::borrow::Cow<'_, str>, RoutineArgError> {
    if !t.contains("&\"") {
        return Ok(std::borrow::Cow::Borrowed(t));
    }
    let mut out = String::with_capacity(t.len());
    let mut rest = t;
    while !rest.is_empty() {
        if rest.starts_with('"') {
            let end = quoted_end(rest).ok_or_else(|| shape(t))?;
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        let opens = rest.len() > 2
            && rest.is_char_boundary(2)
            && rest[..2].eq_ignore_ascii_case("u&")
            && rest[2..].starts_with('"')
            && !out
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if !opens {
            let c = rest.chars().next().expect("not empty");
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let end = quoted_end(&rest[2..]).ok_or_else(|| shape(t))? + 2;
        let inner = rest[3..end - 1].replace("\"\"", "\"");
        rest = &rest[end..];
        let mut escape = '\\';
        // `UESCAPE 'x'`, after ASCII whitespace: the engine's whitespace.
        let after = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if after.len() > 7
            && after.is_char_boundary(7)
            && after[..7].eq_ignore_ascii_case("uescape")
        {
            let clause = after[7..].trim_start_matches(|c: char| c.is_ascii_whitespace());
            let mut chars = clause.chars();
            match (chars.next(), chars.next(), chars.next()) {
                (Some('\''), Some(e), Some('\''))
                    if e != '\'' && e != '+' && !e.is_ascii_hexdigit() && !e.is_whitespace() =>
                {
                    escape = e;
                    rest = &clause[2 + e.len_utf8()..];
                }
                _ => return Err(shape(t)),
            }
        }
        let decoded = decode_unicode_escapes(&inner, escape).ok_or_else(|| shape(t))?;
        out.push('"');
        out.push_str(&decoded.replace('"', "\"\""));
        out.push('"');
    }
    Ok(std::borrow::Cow::Owned(out))
}

/// The end of the `"…"` at the front of `text`, a doubled quote being a quote
/// inside the name.
fn quoted_end(text: &str) -> Option<usize> {
    let mut at = 1;
    loop {
        let close = at + text[at..].find('"')?;
        at = close + 1;
        if text[at..].starts_with('"') {
            at += 1;
        } else {
            return Some(at);
        }
    }
}

/// `\XXXX` and `\+XXXXXX` to the code point they name, a doubled escape to
/// itself, and a surrogate pair to the one character it encodes — the rules
/// PostgreSQL reads `U&"…"` by. `None` where the text does not decode: a
/// digit that is not hex, a lone surrogate.
pub fn decode_unicode_escapes(inner: &str, escape: char) -> Option<String> {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    let mut high: Option<u32> = None;
    while let Some(c) = chars.next() {
        if c != escape {
            if high.is_some() {
                return None;
            }
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&escape) {
            chars.next();
            if high.is_some() {
                return None;
            }
            out.push(escape);
            continue;
        }
        let digits = if chars.peek() == Some(&'+') {
            chars.next();
            6
        } else {
            4
        };
        let mut code = 0u32;
        for _ in 0..digits {
            code = code * 16 + chars.next()?.to_digit(16)?;
        }
        match (high.take(), code) {
            (None, 0xD800..=0xDBFF) => high = Some(code),
            (Some(h), 0xDC00..=0xDFFF) => {
                out.push(char::from_u32(
                    0x10000 + ((h - 0xD800) << 10) + (code - 0xDC00),
                )?);
            }
            (None, code) => out.push(char::from_u32(code)?),
            (Some(_), _) => return None,
        }
    }
    if high.is_some() {
        return None;
    }
    Some(out)
}

impl fmt::Display for RoutineArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

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
    pub args: Vec<RoutineArg>,
}

impl RoutineId {
    pub fn new(name: ObjectName, args: Vec<RoutineArg>) -> Self {
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
    pub fn args(&self) -> Option<&[RoutineArg]> {
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
        source: RoutineArgError,
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
/// `None` if the brackets do not balance, which the caller reports as a
/// malformed identity rather than guessing where the argument ended.
///
/// # It has to know every place a comma can hide, not one
///
/// A first version tracked parentheses only, which was the whole story while
/// an argument was a [`crate::ColumnType`]. [`RoutineArg`] admits two more:
/// `"a,b"` is one quoted type name PostgreSQL will hand back for a type
/// created with that name, and a comma inside `[…]` is inside the argument
/// too. Splitting on either produced two halves that each fail
/// `RoutineArg`'s own balance check, so `app.f(s."a,b")` was a valid
/// declaration its own `Display` could write and nothing could read back.
///
/// The states are exactly the ones [`RoutineArg::from_str`] tracks, and they
/// are here rather than shared with it because that one is folding text as it
/// goes and this one only has to find a boundary — two readers of one rule,
/// which is a shape this project has been wrong about before
/// (PITFALLS: one rule, spelled in three places). They are pinned together by
/// `an_identity_holding_an_array_and_a_quoted_name_round_trips`.
fn split_top_level(args: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut parens = 0usize;
    let mut brackets = 0usize;
    let mut quoted = false;
    let mut start = 0usize;
    let mut chars = args.char_indices();
    while let Some((i, c)) = chars.next() {
        if quoted {
            if c == '"' {
                // A doubled quote is a quote inside the name, and does not
                // close the region.
                if args[i + 1..].starts_with('"') {
                    chars.next();
                } else {
                    quoted = false;
                }
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            '(' => parens += 1,
            ')' => parens = parens.checked_sub(1)?,
            '[' => brackets += 1,
            ']' => brackets = brackets.checked_sub(1)?,
            ',' if parens == 0 && brackets == 0 => {
                parts.push(&args[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if quoted || parens != 0 || brackets != 0 {
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
    references_as(definition, name, Case::Folded)
}

/// How the scan compares letters, narrowest last.
///
/// The scan asks with the widest, and `creation_order` re-asks with a narrower
/// one where the answer made a cycle. Which one a database would use cannot be
/// decided here — the scan runs in the loader, with nothing to ask — but a
/// cycle the fold invented is evidence the loader does have (DECISIONS 245).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    /// The usual answer: a database collation is case-insensitive far more
    /// often than not, and it folds the whole alphabet when it is.
    Folded,
    /// The fold every case-insensitive collation performs, and no more. It is
    /// what separates two declarations that only this scan reads as one — the
    /// three measured characters it folds and the engine does not.
    Ascii,
    /// Two declarations that even an ASCII fold reads as one can only be held
    /// by a case-sensitive database, which folds nothing.
    Exact,
}

fn cased(text: &str, case: Case) -> String {
    match case {
        Case::Folded => folded(text),
        Case::Ascii => text.to_ascii_lowercase(),
        Case::Exact => text.to_owned(),
    }
}

/// The needle for the qualified form of `name`.
fn qualified(name: &ObjectName, case: Case) -> String {
    format!("{}.{}", cased(&name.schema, case), cased(&name.name, case))
}

fn references_as(definition: &str, name: &ObjectName, case: Case) -> bool {
    let haystack = scannable(definition, case);

    // The qualified form, and the bare one — a definition written inside its
    // own schema very often omits the qualifier.
    contains_word(&haystack, &qualified(name, case))
        || contains_word(&haystack, &cased(&name.name, case))
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

/// Case-folded for the dependency scan: simple lower-case, one character in
/// and one character out.
///
/// # Why not `to_ascii_lowercase`
///
/// SQL Server folds the whole alphabet, not the ASCII part of it. Measured on
/// SQL Server 2022 under `SQL_Latin1_General_CP1_CI_AS`, `Latin1_General_CI_AS`
/// and `Latin1_General_100_CI_AS_SC` alike -- and by creating each object and
/// selecting from the other spelling, which resolved exactly where the
/// comparison said it would -- `CAFÉ` and `café` are one table, and so are `Σ`
/// and `σ`. An ASCII fold left the accented halves untouched, so the scan found
/// no edge and `creation_order` was free to put a view before the table it
/// reads (DECISIONS 245).
///
/// It is an approximation, not the collation. Every single-character
/// lower-case mapping in the BMP was put to the engine: this fold agrees with
/// all three collations on 964 of the 1180, and of the 216 it does not, 149
/// are pairs the three collations answer differently *from each other*. No
/// offline rule can be right about those, which is why `creation_order` does
/// not trust it alone: what a fold gets wrong it gets wrong by matching too
/// much, and matching too much is what turns an ordering into a cycle.
///
/// # Why not `str::to_lowercase`
///
/// Full lower-casing is allowed to return more characters than it was given,
/// and the two that matter here are the two that do. `İ` (U+0130) becomes `i`
/// plus a combining dot, which the engine does *not* read as `i` -- measured
/// `ne`, and the object did not resolve. Worse than the wrong answer is where
/// it lands: the combining mark is not an identifier character, so
/// `contains_word` sees a word boundary inside what was one letter and reports
/// a match for the bare needle `i`. Folding character by character keeps `İ`
/// whole and keeps the scan's boundaries where the text put them.
fn folded(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut lower = c.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some(one), None) => one,
                // Two or more: this is one of the expanding mappings, and the
                // character stands as it is rather than become two.
                _ => c,
            }
        })
        .collect()
}

/// Lower-cases, drops the quoting characters and closes the gaps around dots,
/// so that `[Dbo] . [V]` and `dbo.v` become one string to search.
fn scannable(definition: &str, case: Case) -> String {
    let lowered = cased(&code_only(definition), case);
    let unquoted: String = lowered.chars().filter(|c| !"[]\"`".contains(*c)).collect();
    let mut out = String::with_capacity(unquoted.len());
    for (i, ch) in unquoted.char_indices() {
        if ch.is_whitespace() {
            // From `out`, not from `unquoted`: what precedes this character in
            // the *result* is the dot itself when the whitespace between them
            // has already been dropped. Read from the input it was the first
            // character of the run, so only that one went — a formatter's
            // `dbo.\n    customer` kept its indentation and the qualified
            // needle then matched nothing (DECISIONS 239). The trailing side
            // never had the bug: `after` skips the whole run to find the dot.
            let before = out.chars().next_back();
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

    // The edges for one comparison. Only the scanned ones move with it:
    // `depends_on:` and a trigger's target are identities, not text.
    let needs_among = |pending: &[ModuleId], case: Case| {
        let mut needs: BTreeMap<ModuleId, BTreeSet<ModuleId>> = BTreeMap::new();
        for name in pending {
            let module = &modules[name];
            let mut set: BTreeSet<ModuleId> = BTreeSet::new();
            for other in pending {
                if other == name {
                    continue;
                }
                let declared = deps.get(name).is_some_and(|d| d.contains(other));
                // A trigger's target is not named in its definition — the
                // emitter writes it into the `ON` clause — so the identity's
                // own table has to be read directly. It matters only when the
                // target is itself a module: a trigger on a view has to be
                // created after that view.
                let attached = name
                    .attached_to()
                    .is_some_and(|t| other.referenced_name().as_ref() == Some(t));
                // The scan matches a *name*, and where a kind overloads a name
                // is not an identity (ADR-0009 §1). So `app.f` in a routine's
                // body matches every `app.f(...)`: between the overloads of one
                // name an automatic edge would order each against all its
                // siblings, and a body that mentions its own name — a recursive
                // overload, or two that call each other one way — would make a
                // cycle out of them. `depends_on:` could not repair that,
                // because it adds an edge and cannot remove one. Among the
                // modules that share a routine's name, therefore, only
                // `depends_on:` orders (DECISIONS 212).
                //
                // A trigger is named by nothing, so nothing can reference it.
                let sibling = matches!(name, ModuleId::Routine(_))
                    && other.referenced_name() == name.referenced_name();
                let referenced = !sibling
                    && other
                        .referenced_name()
                        .is_some_and(|n| references_as(&module.definition, &n, case));
                if declared || attached || referenced {
                    set.insert(other.clone());
                }
            }
            // A trigger on a *table* plays no part here: tables are created by
            // an earlier ordering class in any case.
            needs.insert(name.clone(), set);
        }
        needs
    };

    // Kahn's algorithm, emitting every ready module each round in name order so
    // that two runs over the same declarations produce the same plan.
    //
    // Run up to three times, and only ever on what is left. The fold is wider
    // than some collations and narrower than none, so what it gets wrong it
    // gets wrong by saying *yes* too often: asked whether a definition names
    // `dbo.CAFÉ` it answers yes to `dbo.café`, and two such over-answers make
    // an ordering cycle out of modules that have none. A missing edge is
    // invisible here and a false one is not — it is exactly what stops Kahn —
    // so the place to narrow is the wreckage, not the whole schema
    // (DECISIONS 245). Whatever is still unplaced is re-scanned under the
    // ASCII fold, which every case-insensitive collation performs, and then
    // under no fold at all, which only a case-sensitive database needs.
    //
    // Narrowing where a cycle appeared and nowhere else is what keeps the price
    // proportionate: a module ordered by the wide fold keeps that ordering, and
    // a pair the fold merged pays for it alone. There is nothing to ask the
    // database — the scan runs in the loader, which is why `fold_ident` folds
    // nothing there either — but a cycle is evidence the loader has in hand.
    let mut out: Vec<ModuleId> = Vec::new();
    let mut pending: Vec<ModuleId> = names.clone();
    for case in [Case::Folded, Case::Ascii, Case::Exact] {
        if pending.is_empty() {
            break;
        }
        let needs = needs_among(&pending, case);
        let mut done: BTreeSet<ModuleId> = BTreeSet::new();
        loop {
            let ready: Vec<ModuleId> = pending
                .iter()
                .filter(|n| !done.contains(*n))
                .filter(|n| needs[*n].iter().all(|d| done.contains(d)))
                .cloned()
                .collect();
            if ready.is_empty() {
                break;
            }
            for n in ready {
                out.push(n.clone());
                done.insert(n);
            }
        }
        pending.retain(|n| !done.contains(n));
    }
    // Whatever is left is a cycle no fold separates — a genuine one, or one
    // `depends_on:` declared. Deterministic order, and the engine has the last
    // word inside the plan's transaction.
    out.extend(pending);
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

    /// The gap around a dot is closed however long it is, and on both sides.
    /// A formatter that breaks a qualified name across lines leaves a run of
    /// whitespace after the dot; reading the character before it from the
    /// input dropped only the first of that run, so `dbo.active_customer`
    /// matched nothing, no edge was recorded, and `creation_order` was free to
    /// put the view before what it selects from — a valid plan whose
    /// `CREATE VIEW` fails inside its own transaction (DECISIONS 239).
    #[test]
    fn a_run_of_whitespace_around_a_dot_is_closed_on_both_sides() {
        // Asked of the scan itself: `references` also tries the bare name,
        // which matches a half-closed gap and would hide the difference.
        for definition in [
            "SELECT * FROM dbo.\n    active_customer",
            "SELECT * FROM dbo.\r\n\tactive_customer",
            "SELECT * FROM dbo  . active_customer",
            "SELECT * FROM [dbo] . [active_customer]",
            "SELECT * FROM dbo\n  .\n  active_customer",
        ] {
            assert_eq!(
                scannable(definition, Case::Folded),
                "select * from dbo.active_customer",
                "{definition:?}"
            );
            assert!(
                references(definition, &n("dbo.active_customer")),
                "{definition:?}"
            );
        }
        // Whitespace that is not beside a dot is a boundary and stays.
        assert_eq!(
            scannable("select a\n  from dbo.t", Case::Folded),
            "select a\n  from dbo.t"
        );
    }

    /// Closing the gap must not fuse what the dot does not join. The three-part
    /// name is another object, and the longer identifier is still a longer
    /// identifier once the whitespace is gone — both would be invented edges.
    #[test]
    fn closing_the_gap_does_not_invent_a_reference() {
        for definition in [
            "SELECT * FROM sales.dbo.\n    active_customer",
            "SELECT * FROM dbo.\n    active_customer_archive",
        ] {
            assert!(
                !references(definition, &n("dbo.active_customer")),
                "{definition:?}"
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

    /// A case-sensitive database holds `dbo.CAF\u{c9}` beside `dbo.caf\u{e9}`, and
    /// the fold that reads `DBO.CUSTOMER` as `dbo.customer` cannot tell those
    /// two apart. Asked with the fold, the scan answers *both* — so a
    /// definition naming one of them gets an edge to the other as well, and a
    /// second such definition closes a cycle between modules that have none.
    /// `creation_order` breaks a cycle by emitting its members in name order,
    /// which is how a valid plan ends up with a `CREATE VIEW` that fails.
    ///
    /// The cycle is visible without a connection, so the modules left in it
    /// are re-scanned with a narrower comparison — here the ASCII fold, which
    /// every case-insensitive collation performs (DECISIONS 245).
    #[test]
    fn two_declarations_that_fold_to_one_name_are_told_apart() {
        let upper = "dbo.CAF\u{c9}";
        let lower = "dbo.caf\u{e9}";
        // A chain, not a cycle: `lower` names nothing, `dbo.z` names `lower`,
        // and `upper` names `dbo.z`. Folded, `dbo.z` reads as naming `upper`
        // too, which closes a loop with it.
        let m = modules(&[
            (upper, "SELECT * FROM dbo.z"),
            (lower, "SELECT 1"),
            ("dbo.z", format!("SELECT * FROM {lower}").as_str()),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id(lower), id("dbo.z"), id(upper)],
            "each module comes after the one it names, and there is no cycle"
        );

        // And the fold is still the answer for a name nothing collides with:
        // the same three definitions with the accented pair spelled apart.
        let m = modules(&[
            ("dbo.caf\u{e9}", "SELECT 1"),
            ("dbo.tea", "SELECT * FROM DBO.CAF\u{c9}"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("dbo.caf\u{e9}"), id("dbo.tea")]
        );
    }

    /// A fold collision is not evidence of a case-sensitive database. The
    /// Kelvin sign is one of the three characters this scan folds and the
    /// engine does not, so a *case-insensitive* database can be holding
    /// `dbo.ktbl` and `dbo.\u{212a}tbl` at once — and there `SELECT * FROM
    /// DBO.KTBL` still means `dbo.ktbl`. Nothing here makes a cycle, so
    /// nothing narrows: both edges stand, and the fold's over-answer costs an
    /// order stricter than it needed to be rather than an edge.
    #[test]
    fn a_collision_this_scan_invents_keeps_the_case_insensitivity_the_engine_has() {
        let kelvin = "dbo.\u{212a}tbl";
        // `dbo.a` sorts first, so it comes out first if nothing orders it and
        // last if something does: the assertion is about the edge, not about
        // the tie-break.
        let m = modules(&[
            ("dbo.ktbl", "SELECT 1"),
            (kelvin, "SELECT 2"),
            ("dbo.a", "SELECT * FROM DBO.KTBL"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("dbo.ktbl"), id(kelvin), id("dbo.a")],
            "`dbo.a` names `dbo.ktbl`, in the case a definition is free to use"
        );

        // And the Kelvin-signed one is still told apart, which is the whole
        // reason the pair is not compared with the wide fold.
        let m = modules(&[
            ("dbo.ktbl", "SELECT 1"),
            (kelvin, "SELECT 2"),
            ("dbo.a", "SELECT * FROM dbo.\u{212a}TBL"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("dbo.ktbl"), id(kelvin), id("dbo.a")]
        );
        assert!(!references_as(
            "SELECT * FROM dbo.\u{212a}TBL",
            &n("dbo.ktbl"),
            Case::Ascii
        ));
    }

    /// The scan looks for the bare object name as well as the qualified one, so
    /// the bare form can collide on its own: `s9.ktbl` and `s2.\u{212a}tbl` are
    /// two names with one folded bare spelling, and a definition in `s9`
    /// writing plain `ktbl` would be attached to both. One false edge and one
    /// real one the other way is a cycle, and a cycle is emitted in name order.
    /// The narrower scan the cycle asks for separates them again.
    ///
    /// Two schemas holding the same bare name is a different thing entirely —
    /// `dbo.t` beside `sales.t` differ in no case at all — and the second half
    /// pins that no narrowing is spent on it.
    #[test]
    fn a_bare_name_that_two_declarations_fold_onto_is_told_apart_too() {
        let kelvin = "s2.\u{212a}tbl";
        let m = modules(&[
            (kelvin, "SELECT * FROM s9.a"),
            ("s9.a", "SELECT * FROM ktbl"),
            ("s9.ktbl", "SELECT 3"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("s9.ktbl"), id("s9.a"), id(kelvin)],
            "the bare `ktbl` names `s9.ktbl` alone, so there is no cycle"
        );

        // Two schemas holding the same bare name is not this collision, and
        // must keep the fold: `dbo.t` is what `SALES.V` means by `DBO.T`.
        let m = modules(&[
            ("dbo.t", "SELECT 1"),
            ("sales.t", "SELECT 2"),
            ("sales.v", "SELECT * FROM DBO.T"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("dbo.t"), id("sales.t"), id("sales.v")]
        );
    }

    /// A bare name two schemas hold in two cases still costs no qualified edge.
    ///
    /// `warehouse.t` beside `app.T` is one bare name in two spellings and two
    /// qualified names in one spelling each — a pair a case-insensitive
    /// database holds without complaint, because the qualifiers differ. There
    /// is no cycle here for a narrower fold to break, so `report.v` keeps the
    /// edge it gets from the perfectly ordinary `WAREHOUSE.T`. An answer that
    /// narrowed on the collision itself would drop that edge and emit the view
    /// before the table it reads — the failure of 239, reached through a guard
    /// meant to prevent it.
    #[test]
    fn a_bare_name_shared_in_two_cases_costs_no_qualified_edge() {
        let m = modules(&[
            ("warehouse.t", "SELECT 1"),
            ("app.T", "SELECT 2"),
            ("report.v", "SELECT * FROM WAREHOUSE.T"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("app.T"), id("warehouse.t"), id("report.v")],
            "`report.v` follows the table it names, and sorts before it without \
             that edge"
        );

        // And where the same pair *does* make a cycle, the narrowing that
        // breaks it is the narrowing the cycle asked for: `report.w` writes
        // plain `t`, which the fold offers to both, and only `warehouse.t`
        // can have meant it.
        let m = modules(&[
            ("warehouse.t", "SELECT 1"),
            ("app.T", "SELECT * FROM report.w"),
            ("report.w", "SELECT * FROM t"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("warehouse.t"), id("report.w"), id("app.T")],
            "one bare edge survives, and there is no cycle to emit in name order"
        );
    }

    /// The fold's over-answers are narrowed where a cycle shows them, which is
    /// the only place the loader can see one — including against a name it is
    /// not ordering at all.
    ///
    /// `creation_order` is given the modules; the tables are ordered by an
    /// earlier class and are not in it. So a view `dbo.ktbl` beside a *table*
    /// `dbo.\u{212a}tbl` is a collision no map of the declarations passed here
    /// could hold. The cycle is visible all the same: `dbo.z` reads the table,
    /// the fold offers the view, and the view really does read `dbo.z`.
    #[test]
    fn a_fold_collision_with_something_unordered_is_narrowed_by_its_cycle() {
        let m = modules(&[
            ("dbo.ktbl", "SELECT * FROM dbo.z"),
            ("dbo.z", "SELECT * FROM dbo.\u{212a}tbl"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("dbo.z"), id("dbo.ktbl")],
            "the view follows the module it reads; the table is another class"
        );
    }

    /// A bare name is offered to every declaration that can hold it, and
    /// narrowing it on a collision would pick one by its spelling — which is
    /// not how the engine picks.
    ///
    /// `sales.A` writing plain `t` means `sales.T` on a case-insensitive
    /// database, and `dbo.t` is the one an exact comparison would find. There
    /// is no cycle here, so both edges stand: over-answering costs an order
    /// that is merely stricter than it needs to be, and under-answering costs
    /// the apply.
    #[test]
    fn a_bare_name_is_offered_to_both_schemas_that_could_hold_it() {
        let m = modules(&[
            ("dbo.t", "SELECT 1"),
            ("sales.T", "SELECT * FROM sales.z"),
            ("sales.A", "SELECT * FROM t"),
            ("sales.z", "SELECT 1"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("dbo.t"), id("sales.z"), id("sales.T"), id("sales.A")],
            "`sales.A` follows both declarations its bare `t` could name"
        );
    }

    /// A spelling nothing declares does not hold a cycle shut.
    ///
    /// The fold attaches `\u{e9}` — here a column alias, not a reference — to
    /// the declared `dbo.\u{c9}`, which is the scan's ordinary over-reach. It
    /// costs an edge back to a module that really does read `dbo.\u{3a9}`, and
    /// that is a cycle: the narrower scan drops the invented half and keeps the
    /// real one, so the table is still created before the view.
    #[test]
    fn a_spelling_nothing_declares_does_not_hold_a_cycle_shut() {
        let m = modules(&[
            ("dbo.\u{c9}", "SELECT * FROM dbo.\u{3a9}"),
            ("dbo.\u{3a9}", "SELECT 1 AS \u{e9}"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("dbo.\u{3a9}"), id("dbo.\u{c9}")],
            "the alias is not a reference, and the view follows what it reads"
        );
    }

    /// A name beside a fold collision keeps the edges no cycle disputes.
    ///
    /// `caf\u{e9}.ktbl` and `caf\u{e9}.\u{212a}tbl` are two declarations this
    /// scan folds onto one name and the engine does not. A definition writing
    /// `CAF\u{c9}.KTBL` means the first of them, and differs from it in a part
    /// of the name the collision has nothing to do with — the schema. Nothing
    /// here is a cycle, so the wide fold stands and the edge is found.
    #[test]
    fn a_name_beside_a_fold_collision_keeps_the_edges_no_cycle_disputes() {
        let m = modules(&[
            ("caf\u{e9}.ktbl", "SELECT 1"),
            ("caf\u{e9}.\u{212a}tbl", "SELECT 2"),
            ("aa.v", "SELECT * FROM CAF\u{c9}.KTBL"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![
                id("caf\u{e9}.ktbl"),
                id("caf\u{e9}.\u{212a}tbl"),
                id("aa.v")
            ],
            "`aa.v` follows what it names, and sorts first without that edge"
        );
    }

    /// Two declarations that even the ASCII fold reads as one can only be held
    /// by a case-sensitive database, which folds nothing — so nothing is lost
    /// by comparing them exactly, and a cycle between modules that have none
    /// is avoided.
    #[test]
    fn declarations_that_differ_only_in_ascii_case_are_compared_exactly() {
        let m = modules(&[
            ("dbo.Z", "SELECT * FROM dbo.y"),
            ("dbo.y", "SELECT * FROM dbo.z"),
            ("dbo.z", "SELECT 1"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![id("dbo.z"), id("dbo.y"), id("dbo.Z")],
            "each module comes after the one it names, and there is no cycle"
        );
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
            // one inside `decimal(10,2)` are the same character, and only
            // nesting tells them apart. Spelled without the space, because
            // `RoutineArg` folds whitespace beside punctuation — the same
            // canonical form whichever way the declaration wrote it, which is
            // the property `one_routine_written_two_ways_is_one_key` asserts.
            "app.f(decimal(10,2))",
            "app.f(decimal(10,2),text,numeric(38,10))",
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

    /// Every spelling PostgreSQL 18.6 puts in a routine's identity, measured
    /// from `proargtypes` through `format_type` — the same list
    /// `oid::regprocedure` prints, and the reason a routine argument is not a
    /// `ColumnType`: of these, only three would parse as one.
    #[test]
    fn every_identity_spelling_the_engine_writes_is_one_argument() {
        for spelling in [
            "character varying",
            "\"char\"",
            "integer",
            "numeric",
            "timestamp with time zone",
            "integer[]",
            "text[]",
            "id.pos",
            "time without time zone",
            "interval",
            "bit varying",
            "character",
            "double precision[]",
            "\"My Type\"",
            "s.\"Odd Name\"[]",
        ] {
            let arg: RoutineArg = spelling.parse().expect(spelling);
            assert_eq!(arg.to_string(), spelling, "{spelling} did not survive");
        }
    }

    /// The canonicalization is only what every SQL engine agrees on: outside
    /// quotes, case and the whitespace beside punctuation do not matter.
    /// Inside them nothing is touched — `"char"` is a real type on PostgreSQL
    /// and `"CHAR"` is not the same one.
    #[test]
    fn an_argument_is_folded_outside_quotes_and_kept_inside_them() {
        for (written, canonical) in [
            ("INT", "int"),
            ("  Integer  ", "integer"),
            ("decimal(10, 2)", "decimal(10,2)"),
            ("integer []", "integer[]"),
            ("TIMESTAMP  WITH   TIME ZONE", "timestamp with time zone"),
            ("Character Varying ( 10 )", "character varying(10)"),
            ("ID.Pos", "id.pos"),
            // The engine accepts a space around a qualified type's dot and
            // never writes one back, so the two spellings have to be one key.
            ("md . my_type", "md.my_type"),
            ("md .my_type", "md.my_type"),
            ("md. my_type", "md.my_type"),
            ("s . \"Odd Name\" []", "s.\"Odd Name\"[]"),
            // ASCII only, which is the engine's rule: measured,
            // `CREATE FUNCTION mn.f(a mn.Ätype)` reads back as
            // `mn.f(mn.\"Ätype\")` — the byte is left alone and the name is
            // quoted rather than folded. A Unicode fold would write `ätype`,
            // which names a type that does not exist.
            ("MN.Ätype", "mn.Ätype"),
            ("ÄÖÜ", "ÄÖÜ"),
            // And ASCII whitespace only: a non-breaking space is a name byte
            // to the engine — measured, `r8.a\u{a0}b` is a type and `r8.a b`
            // is not — so it is neither folded to a space nor trimmed away.
            ("r8.a\u{a0}b", "r8.a\u{a0}b"),
            // A Unicode-escaped identifier is the plain quoted name it spells,
            // decoded with its own escape or the default. Measured:
            // `r12.U&"\006doney"` is identified as `r12.money`.
            ("app.U&\"\\006doney\"", "app.\"money\""),
            ("app.u&\"M!00f6ney\" UESCAPE '!'", "app.\"Möney\""),
            ("U&\"\\0061pp\".t", "\"app\".t"),
            ("U&\"d\\0061t\\+000061\"[]", "\"data\"[]"),
            ("U&\"\\D83D\\DE00\"", "\"😀\""),
            ("U&\"a\"\"b\"", "\"a\"\"b\""),
            // Inside a quoted name it is text, and after a name byte `U&` is
            // not the prefix.
            ("\"U&\"\"x\"\"\"", "\"U&\"\"x\"\"\""),
            ("  r8.a\u{2003}b  ", "r8.a\u{2003}b"),
            ("\u{a0}r8.x\u{a0}", "\u{a0}r8.x\u{a0}"),
            ("r8.→", "r8.→"),
            ("\"char\"", "\"char\""),
            ("\"CHAR\"", "\"CHAR\""),
            ("s.\"Odd Name\"", "s.\"Odd Name\""),
            ("\"a\"\"b\"", "\"a\"\"b\""),
        ] {
            assert_eq!(
                written.parse::<RoutineArg>().expect(written).to_string(),
                canonical,
                "{written}"
            );
        }
        assert_ne!(
            "\"char\"".parse::<RoutineArg>().unwrap(),
            "\"CHAR\"".parse::<RoutineArg>().unwrap(),
            "a quoted type name keeps its case, and these are two types"
        );
        assert_ne!(
            "Ätype".parse::<RoutineArg>().unwrap().as_str(),
            "ätype",
            "a fold that changes a byte the engine leaves alone names another type"
        );
    }

    /// The negative half, which is where a structural rule earns its place: an
    /// argument that could not be written into an identity string and read
    /// back out of one is refused rather than stored.
    #[test]
    fn text_that_is_not_one_argument_is_refused() {
        // A Unicode-escaped identifier that does not decode, or a `UESCAPE`
        // the engine would refuse: not one argument either.
        for malformed in [
            "U&\"\\00G1\"",
            "U&\"\\D83D\"",
            "U&\"\\DE00\"",
            "U&\"!0074\" UESCAPE '+'",
            "U&\"!0074\" UESCAPE ''",
            "U&\"\\0074",
        ] {
            assert!(malformed.parse::<RoutineArg>().is_err(), "{malformed}");
        }
        for bad in [
            "",
            "   ",
            // The character that separates one argument from the next.
            "integer,text",
            // Nothing closes them.
            "integer[",
            "\"unclosed",
            "numeric(10",
            "numeric)",
            "integer]",
            // Characters no type name is written with — and this text is
            // interpolated into `DROP FUNCTION`.
            "integer; DROP TABLE t",
            "integer'",
            "integer -- note",
            "integer/*note*/",
        ] {
            assert!(
                bad.parse::<RoutineArg>().is_err(),
                "`{bad}` is not one argument"
            );
        }
        // And a comma that is *inside* something is not a separator.
        assert!("numeric(10,2)".parse::<RoutineArg>().is_ok());
    }

    /// The identity string carries them the same way, which is what makes the
    /// map key and the JSON key the same text.
    #[test]
    fn an_identity_holding_an_array_and_a_quoted_name_round_trips() {
        for spelling in [
            "app.f(\"char\",integer[])",
            "app.f(character varying,timestamp with time zone)",
            "app.f(id.pos)",
            // The comma that is not a separator, in each of the three places
            // it can hide: a modifier, a quoted name, and a bracket.
            "app.f(numeric(10,2),text)",
            "app.f(s.\"a,b\",integer)",
            "app.f(\"a,b\"[],\"c\"\"d\")",
        ] {
            let parsed: ModuleId = spelling.parse().unwrap();
            assert_eq!(parsed.to_string(), spelling);
            assert_eq!(
                serde_json::from_str::<ModuleId>(&serde_json::to_string(&parsed).unwrap()).unwrap(),
                parsed,
                "{spelling} does not survive JSON"
            );
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

    /// Measured on SQL Server 2022: under the server's own
    /// `SQL_Latin1_General_CP1_CI_AS`, and under `Latin1_General_CI_AS` and
    /// `Latin1_General_100_CI_AS_SC`, each of these pairs compares equal — and
    /// creating the object under one spelling and selecting it under the other
    /// resolved. The scan folded ASCII only, so it saw two names where the
    /// engine sees one, found no edge, and let `creation_order` put a view
    /// before the table it reads.
    #[test]
    fn a_case_the_engine_folds_beyond_ascii_is_one_name_to_the_scan() {
        for (what, declared, written) in [
            ("E acute", "dbo.caf\u{e9}", "dbo.CAF\u{c9}"),
            ("sigma", "dbo.\u{3c3}um", "dbo.\u{3a3}UM"),
            ("Cyrillic de", "dbo.\u{434}om", "dbo.\u{414}OM"),
            ("fullwidth a", "dbo.\u{ff41}b", "dbo.\u{ff21}B"),
            ("angstrom", "dbo.\u{e5}r", "dbo.\u{212b}R"),
        ] {
            let target: ObjectName = declared.parse().unwrap();
            assert!(
                references(&format!("SELECT * FROM {written}"), &target),
                "{what}: the engine reads {written} and {declared} as one name"
            );
            // And the other way round, because either side may be the one
            // written in upper case.
            let target: ObjectName = written.parse().unwrap();
            assert!(
                references(&format!("SELECT * FROM {declared}"), &target),
                "{what}: the fold has to be symmetric"
            );
        }
    }

    /// The other direction of the same measurement, and the reason the fold is
    /// character for character rather than `str::to_lowercase`.
    ///
    /// `İ` (U+0130) is not `i` to the engine — measured unequal under all three
    /// collations, and the object did not resolve. Full lower-casing returns
    /// `i` *plus a combining dot* for it, and the combining dot is not an
    /// identifier character: `contains_word` would find its boundary in the
    /// middle of what was one letter and report a reference to `dbo.i` that
    /// the engine does not have. A fold that returns one character per
    /// character cannot do that, whatever the character folds to.
    #[test]
    fn an_expanding_fold_does_not_invent_a_reference() {
        assert_eq!(folded("\u{130}"), "\u{130}", "one character in, one out");
        assert_eq!(folded("CAF\u{c9}"), "caf\u{e9}");
        assert_eq!(folded("Ab_1"), "ab_1", "ASCII is unchanged");

        let target: ObjectName = "dbo.i".parse().unwrap();
        assert!(
            !references("SELECT * FROM dbo.\u{130}", &target),
            "U+0130 is not `i` on the engine"
        );
        assert!(references("SELECT * FROM dbo.I", &target), "but `I` is");

        // The dotless `ı` is the same measurement mirrored: `I` folds to `i`,
        // and U+0131 is a third letter that neither of them reaches.
        let target: ObjectName = "dbo.\u{131}".parse().unwrap();
        assert!(!references("SELECT * FROM dbo.I", &target));
        assert!(references("SELECT * FROM dbo.\u{131}", &target));
    }

    /// Where the fold is knowingly wider than the engine's, pinned so the next
    /// reader sees the price rather than rediscovers it.
    ///
    /// Measured unequal on the engine, and equal after a simple lower-case:
    /// the Kelvin sign, the Ohm sign, and capital sharp s — three of the 216
    /// such pairs in the BMP, and three the sweep found all three collations
    /// agreeing on. No fold short of the collation itself gets this right: the
    /// engine folds U+212B to `å` and does *not* fold U+212A to `k`, and for
    /// 149 of the 216 the three collations do not even agree with each other
    /// (DECISIONS 245).
    ///
    /// `creation_order` narrows what this costs: an over-answer is free until
    /// it makes an ordering cycle, and the modules in that cycle are re-scanned
    /// with the ASCII fold, which separates the pair and keeps the
    /// case-insensitivity the engine does have. An over-answer that orders
    /// something more strictly than the engine would have is left alone, and
    /// `depends_on:` is the escape hatch where a definition needs the edge the
    /// narrowing removed.
    #[test]
    fn the_fold_is_wider_than_the_engines_in_measured_places() {
        for (what, declared, written) in [
            ("Kelvin sign", "dbo.ktbl", "dbo.\u{212a}tbl"),
            ("Ohm sign", "dbo.\u{3c9}tbl", "dbo.\u{2126}tbl"),
            ("capital sharp s", "dbo.\u{df}tbl", "dbo.\u{1e9e}tbl"),
        ] {
            let target: ObjectName = declared.parse().unwrap();
            assert!(
                references(&format!("SELECT * FROM {written}"), &target),
                "{what}: the scan folds this and the engine does not"
            );
        }
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
