//! The **syntactic** representation of a column type.
//!
//! This layer answers only "what shape does this string have?": a type name plus
//! zero or more arguments. It deliberately does not know whether `nvarchar` is a
//! valid type, or whether `int → bigint` is safe — those belong to
//! `pbps-dialect` (SPEC §11.2).
//!
//! The payoff of drawing the line here is that a `Schema` can be parsed,
//! compared and serialized without any dialect being present at all.

use schemars::JsonSchema;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TypeParseError {
    #[error("a type must not be empty")]
    Empty,

    #[error("unclosed parenthesis in type `{0}`")]
    UnclosedParen(String),

    #[error("type `{0}` has trailing content after the closing parenthesis")]
    TrailingContent(String),

    #[error("type `{0}` has an empty argument")]
    EmptyArg(String),

    #[error("the type name in `{0}` is not a valid identifier")]
    BadBaseName(String),

    #[error("the argument position in type `{0}` must be between name words")]
    BadArgumentPosition(String),
    /// An array spelled other than `T[]`: PostgreSQL keeps neither a size nor
    /// a dimension count, and reads every spelling back as `T[]` (#1167).
    #[error(
        "`{0}` is not an array type pbps can hold: write the element type followed by `[]` \
         (`integer[]`), with no size and one pair of brackets, which is how PostgreSQL reads \
         every array back"
    )]
    BadArray(String),
    /// A schema-qualified type name that is not a reference pbps can hold
    /// (#1523): the reason says which rule it broke.
    #[error(
        "`{whole}` is not a user-defined type reference pbps can hold: {reason}. Write \
         `schema.name`, each part optionally in double quotes, optionally followed by `[]`"
    )]
    BadTypeReference { whole: String, reason: String },
}

/// A type argument.
///
/// Rendering requires an explicit choice between canonical data and human text.
///
/// ```compile_fail,E0277
/// let value = pbps_model::TypeArg::Max;
/// let _ = format!("{}", value);
/// ```
///
/// ```compile_fail,E0599
/// let value = pbps_model::TypeArg::Max;
/// let _ = value.to_string();
/// ```
///
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TypeArg {
    /// The 100 of `nvarchar(100)`; the 18 and 2 of `decimal(18, 2)`.
    Int(i64),
    /// The `max` of `nvarchar(max)`. It gets its own variant because in a length
    /// comparison it means "no upper bound", which spares the dialect from
    /// parsing strings when judging narrowing.
    Max,
    /// Any other named argument, kept as written (lowercased).
    Ident(String),
}

impl TypeArg {
    /// The canonical spelling for SQL, serialization and identity comparisons.
    #[must_use]
    pub fn raw(&self) -> impl fmt::Display + Copy + '_ {
        RawTypeArg(self)
    }

    /// The canonical spelling for a person, escaped exactly once.
    #[must_use]
    pub fn shown(&self) -> impl fmt::Display + Copy + '_ {
        ShownTypeArg(self)
    }
}

#[derive(Clone, Copy)]
struct RawTypeArg<'a>(&'a TypeArg);

#[derive(Clone, Copy)]
struct ShownTypeArg<'a>(&'a TypeArg);

impl fmt::Display for ShownTypeArg<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Escape one value's canonical spelling, keeping its quoting and
        // punctuation identical to the data formatter (DEC-2084.1).
        write!(f, "{}", crate::escape::Escaped(&self.0.raw().to_string()))
    }
}

impl fmt::Display for RawTypeArg<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            TypeArg::Int(n) => write!(f, "{n}"),
            TypeArg::Max => f.write_str("max"),
            TypeArg::Ident(s) => f.write_str(s),
        }
    }
}

/// A syntactically normalized type, such as `nvarchar(100)`, `decimal(18,2)` or
/// `bigint`.
///
/// Normalization does exactly two things: lowercase everything and strip
/// whitespace. **Aliases are not expanded here** (`integer` does not become
/// `int`), because which names alias which is dialect knowledge.
///
/// A user-defined type named as `schema.name` is held beside that, as a
/// [`TypeRef`] (#1523). Its canonical qualified text is also `base`, which
/// therefore always contains a `.` and never equals a catalogue name, so a
/// check such as `ty.base == "jsonb"` fails closed for it. The model only
/// holds the reference: whether the dialect admits one is the dialect's call.
///
/// Rendering requires an explicit choice between canonical data and human text.
///
/// ```compile_fail,E0277
/// let value = "integer".parse::<pbps_model::ColumnType>().unwrap();
/// let _ = format!("{}", value);
/// ```
///
/// ```compile_fail,E0599
/// let value = "integer".parse::<pbps_model::ColumnType>().unwrap();
/// let _ = value.to_string();
/// ```
///
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
pub struct ColumnType {
    pub base: String,
    pub args: Vec<TypeArg>,
    // A word boundary, not a byte offset or an engine-specific suffix. Array
    // dimensions can later be represented independently of this modifier.
    args_after_word: Option<usize>,
    // An array of this type (#1167): a marker, not a rank, since the engine
    // enforces neither a size nor a dimension count.
    array: bool,
    // The type this names, when it is a user-defined type referenced by name
    // (#1523). Private so that no caller builds a `base` that disagrees with
    // it; `base` carries the same canonical text. Boxed: nearly every column
    // type is a built-in, and an inline `TypeRef` (two strings) would grow every
    // one of them, and every enum that holds two, by its size.
    user: Option<Box<TypeRef>>,
}

impl ColumnType {
    pub fn new(base: impl Into<String>, args: Vec<TypeArg>) -> Self {
        Self {
            base: base.into().to_ascii_lowercase(),
            args,
            args_after_word: None,
            array: false,
            user: None,
        }
    }

    /// A user-defined type named by reference (#1523). `base` is the
    /// reference's canonical text, which has a `.` in it and so is never a
    /// catalogue name.
    pub fn reference(user: TypeRef) -> Self {
        Self {
            base: user.raw().to_string(),
            args: Vec::new(),
            args_after_word: None,
            array: false,
            user: Some(Box::new(user)),
        }
    }

    /// The user-defined type this names, or `None` for a built-in or opaque
    /// spelling (#1523). Of an array, the element's.
    pub fn type_ref(&self) -> Option<&TypeRef> {
        self.user.as_deref()
    }

    /// An array of this type, `T[]` (#1167).
    #[must_use]
    pub fn into_array(mut self) -> Self {
        self.array = true;
        self
    }

    /// Whether this is an array type, `T[]`.
    pub fn is_array(&self) -> bool {
        self.array
    }

    /// The element type of an array, or the type itself.
    #[must_use]
    pub fn element(&self) -> Self {
        Self {
            array: false,
            ..self.clone()
        }
    }

    /// A type with no arguments, such as `bigint`.
    pub fn simple(base: impl Into<String>) -> Self {
        Self::new(base, Vec::new())
    }

    /// Move the arguments between words of the base name. The default position
    /// remains after the complete name, preserving existing serialized types.
    pub fn with_args_after_word(mut self, words: usize) -> Result<Self, TypeParseError> {
        if self.args.is_empty() || words == 0 || words >= self.base.split_whitespace().count() {
            return Err(TypeParseError::BadArgumentPosition(
                self.shown().to_string(),
            ));
        }
        self.base = self.base.split_whitespace().collect::<Vec<_>>().join(" ");
        self.args_after_word = Some(words);
        Ok(self)
    }

    /// An in-name modifier position, counted in complete base-name words.
    pub fn args_after_word(&self) -> Option<usize> {
        self.args_after_word
    }

    /// The first integer argument, which is what a dialect most often needs when
    /// judging length narrowing. `nvarchar(max)` returns `None` — no upper
    /// bound, not a length of zero.
    pub fn first_int_arg(&self) -> Option<i64> {
        match self.args.first() {
            Some(TypeArg::Int(n)) => Some(*n),
            Some(TypeArg::Max) | Some(TypeArg::Ident(_)) | None => None,
        }
    }

    pub fn is_max(&self) -> bool {
        self.args.first() == Some(&TypeArg::Max)
    }
}

impl ColumnType {
    /// The canonical spelling for SQL, serialization and identity comparisons.
    #[must_use]
    pub fn raw(&self) -> impl fmt::Display + Copy + '_ {
        RawColumnType(self)
    }

    /// The canonical spelling for a person, escaped exactly once.
    #[must_use]
    pub fn shown(&self) -> impl fmt::Display + Copy + '_ {
        ShownColumnType(self)
    }
}

#[derive(Clone, Copy)]
struct RawColumnType<'a>(&'a ColumnType);

#[derive(Clone, Copy)]
struct ShownColumnType<'a>(&'a ColumnType);

impl fmt::Display for ShownColumnType<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Escape one value's canonical spelling, keeping its quoting and
        // punctuation identical to the data formatter (DEC-2084.1).
        write!(f, "{}", crate::escape::Escaped(&self.0.raw().to_string()))
    }
}

impl fmt::Display for RawColumnType<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let boundary = self.0.args_after_word.and_then(|words| {
            self.0
                .base
                .match_indices(' ')
                .nth(words - 1)
                .map(|(i, _)| i)
        });
        let (before, after) =
            boundary.map_or((self.0.base.as_str(), ""), |i| self.0.base.split_at(i));
        f.write_str(before)?;
        if !self.0.args.is_empty() {
            f.write_str("(")?;
            for (i, a) in self.0.args.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{}", a.raw())?;
            }
            f.write_str(")")?;
        }
        f.write_str(after)?;
        if self.0.array {
            f.write_str("[]")?;
        }
        Ok(())
    }
}

impl FromStr for ColumnType {
    type Err = TypeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(TypeParseError::Empty);
        }
        // A schema-qualified or quoted name is a type reference (#1523). Decided
        // on the text before the first `(` or `[`, so a `.` or a quote inside
        // an argument (`numeric(10.5)`) keeps its old reading, and before the
        // array rules, which would refuse the `[` or `]` of a quoted name.
        if names_a_type_reference(s) {
            return parse_reference(s);
        }
        // One trailing `[]` and nothing else of an array's other spellings
        // (#1167): a size, a second pair, `ARRAY` or the engine's `_T`.
        if let Some(element) = s.strip_suffix("[]") {
            let element = element.trim_end();
            if element.contains(['[', ']'])
                || element
                    .split_whitespace()
                    .any(|w| w.eq_ignore_ascii_case("array"))
            {
                return Err(TypeParseError::BadArray(s.to_owned()));
            }
            return Ok(element.parse::<ColumnType>()?.into_array());
        }
        if s.contains(['[', ']'])
            || s.split_whitespace()
                .any(|w| w.eq_ignore_ascii_case("array"))
        {
            return Err(TypeParseError::BadArray(s.to_owned()));
        }

        let Some(open) = s.find('(') else {
            return finish_base(s, s, Vec::new());
        };

        let Some(close) = s.rfind(')') else {
            return Err(TypeParseError::UnclosedParen(s.to_owned()));
        };
        if close < open {
            return Err(TypeParseError::UnclosedParen(s.to_owned()));
        }
        let suffix = s[close + 1..].trim();
        if !suffix.is_empty()
            && (!s[close + 1..].starts_with(char::is_whitespace)
                || !suffix
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' '))
        {
            return Err(TypeParseError::TrailingContent(s.to_owned()));
        }

        let base = s[..open].trim();
        let mut args = Vec::new();
        for raw in s[open + 1..close].split(',') {
            let a = raw.trim();
            if a.is_empty() {
                return Err(TypeParseError::EmptyArg(s.to_owned()));
            }
            if a.contains(['(', ')']) {
                return Err(TypeParseError::TrailingContent(s.to_owned()));
            }
            args.push(match a.parse::<i64>() {
                Ok(n) => TypeArg::Int(n),
                Err(_) if a.eq_ignore_ascii_case("max") => TypeArg::Max,
                Err(_) => TypeArg::Ident(a.to_ascii_lowercase()),
            });
        }
        if suffix.is_empty() {
            finish_base(s, base, args)
        } else {
            let prefix = finish_base(s, base, Vec::new())?;
            let base = format!("{} {suffix}", prefix.base);
            finish_base(s, &base, args)?
                .with_args_after_word(prefix.base.split_whitespace().count())
        }
    }
}

/// Whether the text can only be a user-defined type reference: it starts with
/// a quote, or its name (up to the first `(` or `[`) holds a `.` or a quote.
/// Every such text was `BadBaseName` before (#1523).
fn names_a_type_reference(s: &str) -> bool {
    s.starts_with('"')
        || s.split(['(', '['])
            .next()
            .is_some_and(|name| name.contains(['.', '"']))
}

/// A user-defined type named by schema and name (#1523): `app.mood`, or
/// `"App"."Mood"` where the case is kept.
///
/// The model stays syntactic. It checks the shape that makes a reference
/// unambiguous and leaves what the type is, and whether it may be used, to the
/// dialect: PostgreSQL decides the kind, SQL Server refuses it. **It holds a
/// part of any length**, as [`crate::RoutineArg`] does, because the identifier
/// limit is an engine's (PostgreSQL keeps 63 bytes, SQL Server 128 UTF-16 units)
/// and a shared parser that enforced one would refuse the other's references
/// with the wrong engine's words before its dialect could speak (#2021).
/// PostgreSQL's `normalize` refuses a part over its limit.
///
/// A reference is **always qualified**, because the plan runs under an empty
/// `search_path` (DEC-1564.1) where a bare name resolves to nothing, and it
/// takes no arguments. Equality is structural over the two parts as the
/// engine would keep them: unquoted parts are folded to lowercase (ASCII only,
/// as the engine does), quoted parts are kept as written, so `APP.Mood` equals
/// `app.mood` and `"App"."Mood"` does not.
///
/// Refused, each with its reason:
/// - a name starting with `_`, which is how the engine spells the array type of
///   the name that follows, so `app._mood` could not be told from `app.mood[]`;
/// - a temporary schema (`pg_temp`, `pg_temp_3`), whose types belong to one
///   session;
/// - `U&"…"` escapes, three-part names and a part that is empty.
///
/// Rendering requires an explicit choice between canonical data and human text.
///
/// ```compile_fail,E0277
/// let value = pbps_model::TypeRef::new("app", "mood").unwrap();
/// let _ = format!("{}", value);
/// ```
///
/// ```compile_fail,E0599
/// let value = pbps_model::TypeRef::new("app", "mood").unwrap();
/// let _ = value.to_string();
/// ```
///
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeRef {
    schema: String,
    name: String,
}

impl FromStr for TypeRef {
    type Err = TypeParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let ty = parse_reference(text.trim())?;
        if ty.is_array() {
            return Err(bad_reference(
                text,
                "an object name cannot have an array suffix",
            ));
        }
        ty.type_ref()
            .cloned()
            .ok_or_else(|| bad_reference(text, "expected a qualified type name"))
    }
}

impl serde::Serialize for TypeRef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw().to_string())
    }
}

impl<'de> serde::Deserialize<'de> for TypeRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A domain's collation uses qualified SQL identifier syntax, without the
/// type-only array-name restriction. Its identity is case-sensitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollationName(TypeRef);

impl CollationName {
    pub fn schema(&self) -> &str {
        self.0.schema()
    }
    pub fn name(&self) -> &str {
        self.0.name()
    }
    pub fn raw(&self) -> impl fmt::Display + Copy + '_ {
        self.0.raw()
    }
}

impl FromStr for CollationName {
    type Err = TypeParseError;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (name, array) = qualified_parts(text.trim())?;
        if array
            || name.schema.is_empty()
            || name.name.is_empty()
            || name.schema.contains('\0')
            || name.name.contains('\0')
        {
            return Err(bad_reference(text, "expected a qualified collation name"));
        }
        Ok(Self(name))
    }
}

impl serde::Serialize for CollationName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw().to_string())
    }
}

impl<'de> serde::Deserialize<'de> for CollationName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

string_json_schema!(TypeRef, CollationName);

impl TypeRef {
    /// A reference from two parts exactly as the engine keeps them, with no
    /// folding: the catalog's own `nspname` and `typname`.
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Result<Self, TypeParseError> {
        let user = Self::unchecked(schema.into(), name.into());
        match user.problem() {
            Some(reason) => Err(bad_reference(&user.shown().to_string(), reason)),
            None => Ok(user),
        }
    }

    fn unchecked(schema: String, name: String) -> Self {
        Self { schema, name }
    }

    /// Why these two parts cannot name one type, or `None`.
    fn problem(&self) -> Option<String> {
        check_part(&self.schema, true)
            .and(check_part(&self.name, false))
            .err()
    }

    /// The schema, as the engine keeps it.
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// The type's name, as the engine keeps it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// The canonical text: a part is bare when it would read back as itself
/// unquoted, and double-quoted (with `"` doubled) otherwise, so one reference
/// has one spelling.
impl TypeRef {
    /// The canonical spelling for SQL, serialization and identity comparisons.
    #[must_use]
    pub fn raw(&self) -> impl fmt::Display + Copy + '_ {
        RawTypeRef(self)
    }

    /// The canonical spelling for a person, escaped exactly once.
    #[must_use]
    pub fn shown(&self) -> impl fmt::Display + Copy + '_ {
        ShownTypeRef(self)
    }
}

#[derive(Clone, Copy)]
struct RawTypeRef<'a>(&'a TypeRef);

#[derive(Clone, Copy)]
struct ShownTypeRef<'a>(&'a TypeRef);

impl fmt::Display for ShownTypeRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Escape one value's canonical spelling, keeping its quoting and
        // punctuation identical to the data formatter (DEC-2084.1).
        write!(f, "{}", crate::escape::Escaped(&self.0.raw().to_string()))
    }
}

impl fmt::Display for RawTypeRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_part(f, &self.0.schema)?;
        f.write_str(".")?;
        write_part(f, &self.0.name)
    }
}

fn write_part(f: &mut fmt::Formatter<'_>, part: &str) -> fmt::Result {
    let bare = part
        .chars()
        .next()
        .is_some_and(|c| matches!(c, 'a'..='z' | '_'))
        && part
            .chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '$'));
    if bare {
        return f.write_str(part);
    }
    f.write_str("\"")?;
    for c in part.chars() {
        if c == '"' {
            f.write_str("\"")?;
        }
        write!(f, "{c}")?;
    }
    f.write_str("\"")
}

/// Why a part cannot be half of a reference, or `Ok`.
fn check_part(part: &str, is_schema: bool) -> Result<(), String> {
    if part.is_empty() {
        return Err("a name part is empty".to_owned());
    }
    if part.contains('\0') {
        return Err("a name part holds a NUL byte".to_owned());
    }
    if is_schema {
        let numbered = |prefix: &str| {
            part.strip_prefix(prefix)
                .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
        };
        if part == "pg_temp" || numbered("pg_temp_") || numbered("pg_toast_temp_") {
            return Err(format!(
                "`{part}` is a temporary schema, and its types belong to one session"
            ));
        }
    } else if let Some(element) = part.strip_prefix('_') {
        return Err(format!(
            "`{part}` starts with `_`, which is how PostgreSQL spells the array type of \
             `{element}`, so it could not be told from `{element}[]`"
        ));
    }
    Ok(())
}

fn bad_reference(whole: &str, reason: impl Into<String>) -> TypeParseError {
    TypeParseError::BadTypeReference {
        whole: whole.to_owned(),
        reason: reason.into(),
    }
}

/// Reads `schema.name` or `schema.name[]`, each part bare or double-quoted.
fn parse_reference(whole: &str) -> Result<ColumnType, TypeParseError> {
    let (user, array) = qualified_parts(whole)?;
    if let Some(reason) = user.problem() {
        return Err(bad_reference(whole, reason));
    }
    let ty = ColumnType::reference(user);
    Ok(if array { ty.into_array() } else { ty })
}

fn qualified_parts(whole: &str) -> Result<(TypeRef, bool), TypeParseError> {
    let fail = |reason: &str| bad_reference(whole, reason);
    let (schema, rest) = take_part(whole, whole)?;
    let Some(rest) = rest.strip_prefix('.') else {
        return Err(fail(if rest.is_empty() {
            "the name is not qualified, and a type is always named with its schema"
        } else if rest.starts_with('(') {
            "a user-defined type takes no arguments"
        } else {
            "expected `.` between the schema and the name, with no space around it"
        }));
    };
    let (name, rest) = take_part(whole, rest)?;
    let rest = rest.trim_start();
    let array = match rest {
        "" => false,
        "[]" => true,
        _ if rest.starts_with('.') => {
            return Err(fail("a name of three parts is not a type reference"));
        }
        _ if rest.starts_with('(') => return Err(fail("a user-defined type takes no arguments")),
        _ if rest.starts_with('[') => return Err(TypeParseError::BadArray(whole.to_owned())),
        _ => return Err(fail("unexpected text after the name")),
    };
    Ok((TypeRef::unchecked(schema, name), array))
}

/// One name part from the front of `text`, and what follows it. A bare part is
/// folded to lowercase (ASCII only, which is the engine's rule); a quoted part
/// is kept, and `""` inside it is one quote.
fn take_part<'a>(whole: &str, text: &'a str) -> Result<(String, &'a str), TypeParseError> {
    if text
        .get(..2)
        .is_some_and(|head| head.eq_ignore_ascii_case("u&"))
    {
        return Err(bad_reference(
            whole,
            "a `U&\"…\"` escaped name is not supported; write the name itself in double quotes",
        ));
    }
    if let Some(quoted) = text.strip_prefix('"') {
        let mut part = String::new();
        let mut chars = quoted.char_indices();
        while let Some((i, c)) = chars.next() {
            if c != '"' {
                part.push(c);
            } else if quoted[i + 1..].starts_with('"') {
                part.push('"');
                chars.next();
            } else {
                if part.is_empty() {
                    return Err(bad_reference(whole, "a quoted name is empty (`\"\"`)"));
                }
                return Ok((part, &quoted[i + 1..]));
            }
        }
        return Err(bad_reference(whole, "a quoted name is not closed"));
    }
    // The engine's `ident_start` and `ident_cont`: a digit and `$` continue a
    // name and cannot begin one, and every non-ASCII character is a letter.
    let starts = |c: char| c.is_ascii_alphabetic() || c == '_' || !c.is_ascii();
    let continues = |c: char| starts(c) || c.is_ascii_digit() || c == '$';
    if !text.chars().next().is_some_and(starts) {
        return Err(bad_reference(
            whole,
            if text.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                "a name starting with a digit must be in double quotes"
            } else {
                "a name part is empty or does not start like an identifier"
            },
        ));
    }
    let end = text.find(|c: char| !continues(c)).unwrap_or(text.len());
    Ok((text[..end].to_ascii_lowercase(), &text[end..]))
}

fn finish_base(whole: &str, base: &str, args: Vec<TypeArg>) -> Result<ColumnType, TypeParseError> {
    let base = base.trim();
    // Type names may contain spaces (`double precision`, `timestamp with time
    // zone`), but structural characters such as parentheses and commas must not
    // sneak in.
    let ok = !base.is_empty()
        && base
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ');
    if !ok {
        return Err(TypeParseError::BadBaseName(whole.to_owned()));
    }
    Ok(ColumnType::new(base, args))
}

impl TryFrom<String> for ColumnType {
    type Error = TypeParseError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<ColumnType> for String {
    fn from(v: ColumnType) -> String {
        v.raw().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    #[test]
    fn parses_simple_and_parameterised() {
        assert_eq!(p("bigint"), ColumnType::simple("bigint"));
        assert_eq!(
            p("nvarchar(100)"),
            ColumnType::new("nvarchar", vec![TypeArg::Int(100)])
        );
        assert_eq!(
            p("decimal(18, 2)"),
            ColumnType::new("decimal", vec![TypeArg::Int(18), TypeArg::Int(2)])
        );
    }

    #[test]
    fn normalises_case_and_whitespace() {
        assert_eq!(p("  NVARCHAR ( 100 ) "), p("nvarchar(100)"));
        assert_eq!(p("BigInt"), p("bigint"));
    }

    /// The same type in different cases must compare equal, or diff will invent
    /// changes that are not there.
    #[test]
    fn case_differences_do_not_create_changes() {
        assert_eq!(p("NVARCHAR(255)"), p("nvarchar(255)"));
        assert_eq!(p("DATETIME2(3)"), p("datetime2(3)"));
    }

    #[test]
    fn max_is_its_own_variant() {
        let t = p("nvarchar(MAX)");
        assert!(t.is_max());
        assert_eq!(t.first_int_arg(), None, "max is not a length of zero");
        assert_eq!(t.raw().to_string(), "nvarchar(max)");
    }

    #[test]
    fn multi_word_base_names_are_allowed() {
        assert_eq!(p("double precision").base, "double precision");
        assert_eq!(
            p("timestamp with time zone").base,
            "timestamp with time zone"
        );
    }

    #[test]
    fn display_round_trips() {
        for s in ["bigint", "nvarchar(100)", "decimal(18, 2)", "nvarchar(max)"] {
            assert_eq!(p(s).raw().to_string(), s, "{s} did not round-trip");
        }
    }

    #[test]
    fn in_name_modifiers_preserve_position_and_string_serialization() {
        for name in ["time", "timestamp"] {
            for zone in ["with", "without"] {
                let spelling = format!("{name}(3) {zone} time zone");
                let ty = p(&spelling);
                assert_eq!(ty.base, format!("{name} {zone} time zone"));
                assert_eq!(ty.args_after_word(), Some(1));
                assert_eq!(ty.raw().to_string(), spelling);
                assert_eq!(p(&ty.raw().to_string()), ty);
                let json = serde_json::to_string(&ty).unwrap();
                assert_eq!(json, format!("\"{spelling}\""));
                assert_eq!(serde_json::from_str::<ColumnType>(&json).unwrap(), ty);
                assert_ne!(ty, ColumnType::new(ty.base.clone(), ty.args.clone()));
            }
        }
        assert_eq!(p("custom name(4) qualifier").args_after_word(), Some(2));
        assert_eq!(
            p("timestamp (3)  with  time zone"),
            p("timestamp(3) with time zone")
        );
        for spelling in ["nvarchar(100)", "decimal(18, 2)", "datetime2(7)"] {
            assert_eq!(
                serde_json::to_string(&p(spelling)).unwrap(),
                format!("\"{spelling}\"")
            );
        }
    }

    /// An array is `T[]` and only that, written back the way PostgreSQL
    /// reads every array spelling (#1167): the element, with any modifier in
    /// its place, then one `[]`. Equal elements make equal arrays, and an
    /// array never equals its element.
    #[test]
    fn an_array_is_its_element_and_one_pair_of_brackets() {
        for (spelling, written) in [
            ("text[]", "text[]"),
            ("INTEGER[]", "integer[]"),
            ("varchar(10)[]", "varchar(10)[]"),
            ("numeric(10, 2) []", "numeric(10, 2)[]"),
            (
                "timestamp(3) with time zone[]",
                "timestamp(3) with time zone[]",
            ),
        ] {
            let ty: ColumnType = spelling
                .parse()
                .unwrap_or_else(|e| panic!("{spelling}: {e}"));
            assert!(ty.is_array(), "{spelling}");
            assert_eq!(ty.raw().to_string(), written, "{spelling}");
            assert_eq!(written.parse::<ColumnType>().unwrap(), ty, "{spelling}");
            assert!(!ty.element().is_array());
            assert_ne!(ty.element(), ty, "{spelling}");
        }
        assert_eq!(
            "timestamp(3) with time zone[]"
                .parse::<ColumnType>()
                .unwrap()
                .element(),
            "timestamp(3) with time zone".parse::<ColumnType>().unwrap()
        );
        // Every other spelling of an array is refused, never read as a scalar.
        for spelling in [
            "int[3]",
            "int[][]",
            "int [ ]",
            "integer ARRAY",
            "integer array[4]",
            "int]",
            "[]",
        ] {
            assert!(spelling.parse::<ColumnType>().is_err(), "{spelling}");
        }
    }

    #[test]
    fn modifiers_need_name_boundaries() {
        for spelling in [
            "timestamp(3)with time zone",
            "timestamp(3) with time zone;",
            "timestamp(3)(4)",
            "timestamp(3) with (4)",
        ] {
            assert!(spelling.parse::<ColumnType>().is_err(), "{spelling}");
        }
        for position in [0, 4, usize::MAX] {
            assert!(
                ColumnType::new("timestamp with time zone", vec![TypeArg::Int(3)])
                    .with_args_after_word(position)
                    .is_err()
            );
        }
        assert!(
            ColumnType::simple("timestamp with time zone")
                .with_args_after_word(1)
                .is_err()
        );
    }

    #[test]
    fn malformed_types_are_rejected() {
        assert!("".parse::<ColumnType>().is_err());
        assert!("nvarchar(100".parse::<ColumnType>().is_err());
        assert!("nvarchar(100) junk;".parse::<ColumnType>().is_err());
        assert!("nvarchar(,)".parse::<ColumnType>().is_err());
        assert!("nvarchar()".parse::<ColumnType>().is_err());
    }

    // -- type references (#1523) --

    fn reference(spelling: &str) -> ColumnType {
        spelling
            .parse()
            .unwrap_or_else(|e| panic!("{spelling}: {e}"))
    }

    /// The canonical text is the whole identity: it is what `Display` writes,
    /// what `base` carries, and what reads back as the same value.
    #[test]
    fn a_type_reference_round_trips_in_its_canonical_text() {
        for (spelling, canonical) in [
            ("app.mood", "app.mood"),
            ("APP.Mood", "app.mood"),
            ("app.mood[]", "app.mood[]"),
            ("app.mood []", "app.mood[]"),
            ("\"app\".\"mood\"", "app.mood"),
            ("\"App\".\"Mood\"", "\"App\".\"Mood\""),
            ("\"App\".\"Mood\"[]", "\"App\".\"Mood\"[]"),
            ("app.\"Mood\"", "app.\"Mood\""),
            ("\"a\"\"b\".c", "\"a\"\"b\".c"),
            ("\"a.b\".\"c d\"", "\"a.b\".\"c d\""),
            ("_app.mood$1", "_app.mood$1"),
            ("app.\"1st\"", "app.\"1st\""),
            ("app.\u{c4}type", "app.\"\u{c4}type\""),
        ] {
            let ty = reference(spelling);
            assert_eq!(ty.raw().to_string(), canonical, "{spelling}");
            assert_eq!(reference(canonical), ty, "{spelling}");
            assert!(ty.type_ref().is_some(), "{spelling}");
            assert!(ty.args.is_empty() && ty.args_after_word().is_none());
            assert_eq!(ty.is_array(), canonical.ends_with("[]"), "{spelling}");
            // `base` is the canonical text without the marker, and it always
            // has a `.`, so no catalogue name can equal it.
            assert_eq!(ty.base, canonical.trim_end_matches("[]"), "{spelling}");
            assert!(ty.base.contains('.'), "{spelling}");
            let json = serde_json::to_string(&ty).unwrap();
            assert_eq!(
                json,
                serde_json::to_string(canonical).unwrap(),
                "{spelling}"
            );
            assert_eq!(serde_json::from_str::<ColumnType>(&json).unwrap(), ty);
        }
    }

    /// An unquoted part folds as the engine folds it, a quoted one is kept, and
    /// equality is over the two parts.
    #[test]
    fn a_type_reference_is_equal_by_its_parts() {
        assert_eq!(reference("APP.Mood"), reference("app.mood"));
        assert_eq!(reference("\"app\".\"mood\""), reference("app.mood"));
        assert_ne!(reference("\"App\".\"Mood\""), reference("app.mood"));
        assert_ne!(reference("app.\"Mood\""), reference("app.mood"));
        assert_ne!(reference("app.mood"), reference("app.moods"));
        assert_ne!(reference("app.mood"), reference("other.mood"));
        // An array is not its element, and a reference is not a built-in.
        assert_ne!(reference("app.mood[]"), reference("app.mood"));
        assert_ne!(reference("app.mood"), ColumnType::simple("app.mood"));
        assert_eq!(reference("app.mood[]").element(), reference("app.mood"));
        let kept = reference("\"App\".\"Mood\"");
        let user = kept.type_ref().unwrap();
        assert_eq!((user.schema(), user.name()), ("App", "Mood"));
        let folded = reference("APP.Mood");
        let user = folded.type_ref().unwrap();
        assert_eq!((user.schema(), user.name()), ("app", "mood"));
        // A part is folded in ASCII only: the engine leaves other letters alone.
        assert_eq!(
            reference("app.\u{c4}x").type_ref().unwrap().name(),
            "\u{c4}x"
        );
    }

    /// A reference built from the catalog's own parts holds them as given, and
    /// reads back from its text.
    #[test]
    fn a_type_reference_built_from_parts_keeps_them_unfolded() {
        let user = TypeRef::new("App", "Mood").unwrap();
        let ty = ColumnType::reference(user.clone());
        assert_eq!(ty.raw().to_string(), "\"App\".\"Mood\"");
        assert_eq!(ty.type_ref(), Some(&user));
        assert_eq!(reference(&ty.raw().to_string()), ty);
        assert!(TypeRef::new("app", "").is_err());
        assert!(TypeRef::new("", "mood").is_err());
        assert!(TypeRef::new("pg_temp", "mood").is_err());
        assert!(TypeRef::new("app", "_mood").is_err());
    }

    #[test]
    fn a_type_reference_is_refused_where_it_cannot_name_one_type() {
        for (spelling, why) in [
            ("a.b.c", "three parts"),
            ("\"a\".\"b\".\"c\"", "three quoted parts"),
            (".mood", "no schema"),
            ("app.", "no name"),
            ("\"\"", "an empty quoted name"),
            ("app.\"\"", "an empty quoted name"),
            ("\"\".mood", "an empty quoted schema"),
            ("app._mood", "the array type's own name"),
            ("app.\"_mood\"", "the array type's own name, quoted"),
            ("app.mood(3)", "an argument"),
            ("app.mood (3)", "an argument after a space"),
            ("app.mood(3)[]", "an argument on an array"),
            ("app.mood[3]", "an array with a size"),
            ("app.mood[][]", "two pairs of brackets"),
            ("app.mood[", "an open bracket"),
            ("app.mood ARRAY", "the SQL array spelling"),
            ("app.mood[]x", "trailing text"),
            ("app.mood x", "trailing words"),
            ("app . mood", "a space around the dot"),
            ("app. mood", "a space after the dot"),
            ("pg_temp.mood", "the temporary schema"),
            ("PG_TEMP.mood", "the temporary schema, upper case"),
            ("\"pg_temp\".mood", "the temporary schema, quoted"),
            ("pg_temp_3.mood", "a numbered temporary schema"),
            ("pg_toast_temp_3.mood", "a numbered toast temporary schema"),
            ("U&\"mo\\0064\".x", "an escaped schema"),
            ("app.U&\"mo\\0064\"", "an escaped name"),
            ("u&\"x\".y", "an escaped schema, lower case"),
            ("\"app.mood", "an open quote"),
            ("app.\"mood", "an open quote in the name"),
            ("app.\"mo\"od", "text after a closing quote"),
            ("1app.mood", "a schema starting with a digit"),
            ("app.1mood", "a name starting with a digit"),
            ("app.mo-od", "a character no bare name holds"),
            ("app.mo;od", "a semicolon"),
            ("app.mo od", "a space in a bare name"),
            ("app.mood;", "a trailing semicolon"),
            ("app.\"a\0b\"", "a NUL byte"),
        ] {
            let error = spelling
                .parse::<ColumnType>()
                .expect_err(&format!("{why}: `{spelling}`"));
            // The refusal is the model's own, not the old base-name one, and it
            // names what the user wrote.
            let message = error.to_string();
            assert!(
                !matches!(error, TypeParseError::BadBaseName(_)),
                "{why}: {message}"
            );
            assert!(
                message.contains(spelling.trim()) || spelling.is_empty(),
                "{why}: {message}"
            );
        }
        // A schema may start with an underscore; only a type name may not.
        assert!("_app.mood".parse::<ColumnType>().is_ok());
        assert!("pg_temporary.mood".parse::<ColumnType>().is_ok());
        assert!("pg_temp_x.mood".parse::<ColumnType>().is_ok());
    }

    /// The model holds a part of any length (#2021): the identifier limit is an
    /// engine's, and each dialect's `normalize` applies its own, so a part PostgreSQL
    /// would truncate (64 bytes) or SQL Server would refuse (129 units) still
    /// parses, keeps its whole text, and reads back as the same reference.
    #[test]
    fn a_type_reference_holds_a_part_of_any_length() {
        let multibyte = "\u{e9}".repeat(32);
        for part in [
            "a".repeat(63),
            "a".repeat(64),
            "a".repeat(128),
            "a".repeat(129),
            "a".repeat(1000),
            multibyte,
        ] {
            for spelling in [
                format!("app.{part}"),
                format!("{part}.mood"),
                format!("\"{part}\".\"{part}\""),
                format!("app.{part}[]"),
            ] {
                let ty = reference(&spelling);
                let user = ty.type_ref().expect("a reference");
                assert!(user.schema() == part || user.name() == part, "{spelling}");
                // The whole part survives, uncut, and the canonical text reads
                // back as the same value.
                assert!(ty.raw().to_string().contains(&part), "{spelling}");
                assert_eq!(reference(&ty.raw().to_string()), ty, "{spelling}");
                assert_eq!(
                    serde_json::from_str::<ColumnType>(&serde_json::to_string(&ty).unwrap())
                        .unwrap(),
                    ty,
                    "{spelling}"
                );
            }
            // The same parts built from a catalogue's own, as a connected read
            // will.
            let user = TypeRef::new(part.clone(), part.clone()).unwrap();
            assert_eq!((user.schema(), user.name()), (part.as_str(), part.as_str()));
            assert_eq!(reference(&user.raw().to_string()).type_ref(), Some(&user));
        }
        // The other rules do not depend on length: a long temporary schema and a
        // long `_` name are still refused, and so is an empty part.
        assert!(TypeRef::new(format!("pg_temp_{}", "1".repeat(100)), "mood").is_err());
        assert!(TypeRef::new("app", format!("_{}", "a".repeat(100))).is_err());
        assert!(TypeRef::new("a".repeat(100), "").is_err());
        // An unterminated or malformed long name is still the shape's refusal.
        assert!(
            format!("app.\"{}", "a".repeat(100))
                .parse::<ColumnType>()
                .is_err()
        );
        assert!(
            format!("{0}.{0}.{0}", "a".repeat(100))
                .parse::<ColumnType>()
                .is_err()
        );
    }

    /// A reference is told from a built-in by the name before any argument, so a
    /// dot or a quote inside an argument keeps the reading it always had.
    #[test]
    fn a_dot_or_quote_inside_an_argument_is_not_a_reference() {
        for spelling in ["numeric(1.5)", "foo(\"x\")", "foo(a.b)", "foo(1) bar"] {
            let ty = reference(spelling);
            assert!(ty.type_ref().is_none(), "{spelling}");
            assert!(!ty.base.contains('.'), "{spelling}");
        }
        assert!("foo(a.b".parse::<ColumnType>().is_err());
    }

    /// Nothing that loaded before changes: every existing scalar and array
    /// spelling holds no reference and serializes as it did.
    #[test]
    fn existing_spellings_hold_no_reference_and_serialize_as_before() {
        for (spelling, written) in [
            ("bigint", "bigint"),
            ("INTEGER", "integer"),
            ("nvarchar(max)", "nvarchar(max)"),
            ("decimal(18, 2)", "decimal(18, 2)"),
            ("double precision", "double precision"),
            ("character varying(10)", "character varying(10)"),
            ("timestamp(3) with time zone", "timestamp(3) with time zone"),
            ("time(3) without time zone", "time(3) without time zone"),
            ("text[]", "text[]"),
            ("varchar(10)[]", "varchar(10)[]"),
            ("numeric(10, 2) []", "numeric(10, 2)[]"),
            (
                "timestamp(3) with time zone[]",
                "timestamp(3) with time zone[]",
            ),
            ("jsonb", "jsonb"),
            ("bit varying(3)", "bit varying(3)"),
        ] {
            let ty = reference(spelling);
            assert!(ty.type_ref().is_none(), "{spelling}");
            assert_eq!(ty.raw().to_string(), written, "{spelling}");
            assert_eq!(
                serde_json::to_string(&ty).unwrap(),
                format!("\"{written}\""),
                "{spelling}"
            );
        }
        // And what was refused as a base name for its punctuation still is.
        for spelling in ["a-b", "foo;", "a b;c", "text]", "[]", "int[3]"] {
            assert!(spelling.parse::<ColumnType>().is_err(), "{spelling}");
        }
    }

    /// Expanding aliases is the dialect's job; this layer must not decide on its
    /// own.
    #[test]
    fn aliases_are_left_alone() {
        assert_ne!(p("integer"), p("int"));
    }

    const HOSTILE: &str = "type\nline\r\t\u{1b}\u{9b}\\\u{202e}\"名前";
    const VISIBLE: &str = r#"type\nline\r\t\u{1b}\u{9b}\\\u{202e}"名前"#;

    #[test]
    fn type_adapters_preserve_canonical_syntax_and_escape_only_the_human_spelling() {
        let user = TypeRef::new("app", HOSTILE).unwrap();
        let canonical = "app.\"type\nline\r\t\u{1b}\u{9b}\\\u{202e}\"\"名前\"";
        let visible = r#"app."type\nline\r\t\u{1b}\u{9b}\\\u{202e}""名前""#;
        assert_eq!(user.raw().to_string(), canonical);
        assert_eq!(user.shown().to_string(), visible);
        assert_ne!(user.raw().to_string(), user.shown().to_string());
        let scalar = ColumnType::reference(user.clone());
        let array = scalar.clone().into_array();
        assert_eq!(scalar.raw().to_string(), canonical);
        assert_eq!(scalar.shown().to_string(), visible);
        assert_eq!(array.raw().to_string(), format!("{canonical}[]"));
        assert_eq!(array.shown().to_string(), format!("{visible}[]"));
        for (ty, spelling) in [
            (TypeArg::Int(i64::MIN), i64::MIN.to_string()),
            (TypeArg::Max, "max".into()),
        ] {
            assert_eq!(ty.raw().to_string(), spelling);
            assert_eq!(ty.shown().to_string(), spelling);
        }
        let arg = TypeArg::Ident(HOSTILE.into());
        assert_eq!(arg.raw().to_string(), HOSTILE);
        assert_eq!(arg.shown().to_string(), VISIBLE);
        for text in [
            "integer",
            "decimal(18, 4)",
            "nvarchar(max)",
            "timestamp(3) with time zone[]",
        ] {
            let ty: ColumnType = text.parse().unwrap();
            assert_eq!(ty.raw().to_string(), text);
            assert_eq!(ty.shown().to_string(), text);
        }
    }

    #[test]
    fn canonical_type_keys_keep_raw_controls_distinct_from_their_shown_lookalikes() {
        let user = TypeRef::new("app", HOSTILE).unwrap();
        let lookalike = TypeRef::new("app", VISIBLE).unwrap();
        let ty = ColumnType::reference(user.clone());
        assert_eq!(
            ty.base,
            "app.\"type\nline\r\t\u{1b}\u{9b}\\\u{202e}\"\"名前\""
        );
        assert_ne!(ty.base, user.shown().to_string());
        let other = ColumnType::reference(lookalike.clone());
        assert_ne!(ty, other);
        assert_eq!(std::collections::BTreeSet::from([user, lookalike]).len(), 2);
        for ty in [ty, other] {
            let raw = String::from(ty.clone());
            assert_eq!(raw, ty.raw().to_string());
            assert_eq!(raw.parse::<ColumnType>().unwrap(), ty);
            let json = serde_json::to_string(&ty).unwrap();
            assert_eq!(json, serde_json::to_string(&raw).unwrap());
            assert_eq!(serde_json::from_str::<ColumnType>(&json).unwrap(), ty);
        }
    }

    #[test]
    fn named_argument_keys_serialize_canonical_text_without_showing_it() {
        let raw = "\"arg\n\\\u{1b}\u{9b}\u{200b}名前\"";
        let ty = ColumnType::new("timestamp with time zone", vec![TypeArg::Ident(raw.into())])
            .with_args_after_word(1)
            .unwrap()
            .into_array();
        let canonical = format!("timestamp({raw}) with time zone[]");
        let shown = r#"timestamp("arg\n\\\u{1b}\u{9b}\u{200b}名前") with time zone[]"#;
        assert_eq!(ty.raw().to_string(), canonical);
        assert_eq!(String::from(ty.clone()), canonical);
        assert_eq!(ty.shown().to_string(), shown);
        assert_ne!(canonical, shown);
        assert_eq!(canonical.parse::<ColumnType>().unwrap(), ty);
        assert_ne!(shown.parse::<ColumnType>().unwrap(), ty);
        assert_eq!(
            serde_json::to_string(&ty).unwrap(),
            serde_json::to_string(&canonical).unwrap()
        );
    }

    #[test]
    fn typed_parse_error_operands_are_shown_once() {
        let error = ColumnType::new("integer", vec![TypeArg::Ident("line\n\\x".into())])
            .with_args_after_word(0)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            r"the argument position in type `integer(line\n\\x)` must be between name words"
        );
        let error = TypeRef::new("", "line\n\\x").unwrap_err();
        assert_eq!(
            error.to_string(),
            r#"`""."line\n\\x"` is not a user-defined type reference pbps can hold: a name part is empty. Write `schema.name`, each part optionally in double quotes, optionally followed by `[]`"#
        );
    }
}

#[cfg(test)]
mod collation_name_tests {
    use super::*;

    #[test]
    fn collation_names_keep_sql_identity_without_type_only_restrictions() {
        for text in [
            "app._collation",
            "pg_catalog.\"C\"",
            "\"App\".\"collation.with.dot\"",
        ] {
            let name: CollationName = text.parse().unwrap();
            assert_eq!(
                name.raw().to_string().parse::<CollationName>().unwrap(),
                name
            );
            assert_eq!(
                serde_json::from_str::<CollationName>(&serde_json::to_string(&name).unwrap())
                    .unwrap(),
                name
            );
        }
        assert!("app._collation".parse::<TypeRef>().is_err());
        for text in ["unqualified", "app.c[]", "app.c.extra", "app.\"\""] {
            assert!(text.parse::<CollationName>().is_err(), "{text}");
        }
    }
}
