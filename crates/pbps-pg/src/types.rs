//! The PostgreSQL type catalogue: what the engine spells back, and how safe it
//! is to change one type into another (ADR-0012).
//!
//! # Why normalization has to be aggressive
//!
//! Diff compares normalized types for equality, and introspection reads types
//! back out of the catalog, which reports the *stored* form and never the
//! spelling the user wrote. `int`, `varchar(50)` and `decimal(10,2)` come back
//! as `integer`, `character varying(50)` and `numeric(10,2)`. Unless the
//! declared side is folded into the same form first, every run reports a type
//! change that does not exist.
//!
//! # The contract this file is held to
//!
//! ADR-0011 Amendment 3: normalization is idempotent, **and its output is what
//! introspection reads back for a column declared that way**. A spelling for
//! which that is impossible is an error, not something to normalize. Three
//! families of spelling are refused for exactly that reason, each measured on
//! PostgreSQL 18.6:
//!
//! - the `serial` family, which is a macro and not a type — see
//!   [`refuse_serial`];
//! - **arrays**, which the model has nowhere to put (ADR-0012 §1);
//! - **a precision on the four `time`/`timestamp` spellings**, which the engine
//!   spells *inside* the name — see [`ArgShape::NoModifier`].
//!
//! # Why the catalogue is closed
//!
//! Every name is one the engine returns, and a name not in the table is
//! refused rather than passed through. A pass-through would let `text ARRAY`
//! load as the base name `text array`, which no catalog will ever return, so
//! the column would be reported as changed on every run and no plan could ever
//! fix it — ADR-0012 §1 names that trap, and it is the reason this table has no
//! default branch.

use std::ops::RangeInclusive;

use pbps_dialect::{DialectError, TypeChangeRisk};
use pbps_model::{ColumnType, RoutineArg, TypeArg};

pub const DIALECT: &str = "postgres";

/// The longest `character` or `character varying` the engine will declare.
/// Measured: `varchar(10485761)` is `length for type varchar cannot exceed
/// 10485760`, and `varchar(0)` is `length for type varchar must be at least 1`.
const MAX_STRING_LEN: i64 = 10_485_760;

/// `numeric`'s bounds, measured: precision 1..=1000, and a scale that may be
/// **negative** — `numeric(10,-5)` is legal and reads back as itself, which is
/// why a scale is not checked against the precision here as it is on SQL
/// Server.
const MAX_NUMERIC_PRECISION: i64 = 1000;
const MAX_NUMERIC_SCALE: i64 = 1000;

/// `float(n)` counts bits of mantissa, and the engine accepts 1..=53.
const MAX_FLOAT_BITS: i64 = 53;

/// `interval`'s seconds precision. Measured, and the reason it is checked here:
/// the engine does **not** refuse `interval(7)`. It silently stores
/// `interval(6)`, which is the identifier-truncation shape again — a
/// declaration that records itself at one value and reads back at another.
const MAX_INTERVAL_PRECISION: i64 = 6;

/// What kind of argument list a type takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgShape {
    /// No arguments at all: `integer`, `text`, `date`, `uuid`.
    None,
    /// One length in `1..=MAX_STRING_LEN`. `fills_in` is what the engine
    /// supplies when the declaration writes none, and the two string types
    /// disagree about it — measured, `character` becomes `character(1)` and
    /// `character varying` stays unbounded.
    Length { fills_in: Option<i64> },
    /// `numeric[(p[,s])]`. Measured: bare `numeric` reads back as `numeric`
    /// with no arguments at all — unbounded, and *not* the `(18,0)` SQL Server
    /// fills in — while `numeric(10)` reads back as `numeric(10,0)`.
    Numeric,
    /// `interval[(p)]`, `p` in `0..=MAX_INTERVAL_PRECISION`.
    IntervalPrecision,
    /// A type whose modifier the engine spells **inside** the name.
    ///
    /// Measured: `timestamptz(3)` reads back as `timestamp(3) with time zone`,
    /// and `timestamp with time zone(3)` is a *syntax error*. A `ColumnType` is
    /// a name followed by its arguments and has nowhere to put a modifier in
    /// the middle, so there is no value this normalizer could return that both
    /// introspection would read back and the emitter could spell. The
    /// declaration is refused, and the refusal says what to write instead
    /// (DECISIONS 242; lifting it is the model change in issue #130).
    NoModifier,
}

/// Every type name this dialect admits, spelled as the engine spells it back,
/// with the shape of its argument list. Aliases are resolved before this table
/// is consulted.
///
/// The list is closed (DECISIONS 240). It is ADR-0012 §1's catalogue, and the
/// rule that derives it is worth more than the rows: **the catalogue must admit every type the other ADRs'
/// rules name.** ADR-0013's offline refusal list names `date`, `time`,
/// `timetz`, `timestamp`, `timestamptz`, `interval`, `real` and `double
/// precision`; its rendering rules name `text` and `bytea`. A rule about
/// `bytea` columns in a dialect that cannot declare one is not implementable,
/// and that is checkable by reading.
const CATALOGUE: &[(&str, ArgShape)] = &[
    // Exact numerics.
    ("smallint", ArgShape::None),
    ("integer", ArgShape::None),
    ("bigint", ArgShape::None),
    ("numeric", ArgShape::Numeric),
    // Approximate numerics. `float(n)` is an alias that resolves into one of
    // these two, so the spelling `float` never survives normalization.
    ("real", ArgShape::None),
    ("double precision", ArgShape::None),
    ("boolean", ArgShape::None),
    // Character strings. Only `character varying` may be unbounded.
    ("character", ArgShape::Length { fills_in: Some(1) }),
    ("character varying", ArgShape::Length { fills_in: None }),
    ("text", ArgShape::None),
    ("bytea", ArgShape::None),
    // Date and time. The four with a `NoModifier` shape are the ones whose
    // precision the engine spells inside the name.
    ("date", ArgShape::None),
    ("time without time zone", ArgShape::NoModifier),
    ("time with time zone", ArgShape::NoModifier),
    ("timestamp without time zone", ArgShape::NoModifier),
    ("timestamp with time zone", ArgShape::NoModifier),
    ("interval", ArgShape::IntervalPrecision),
    // Everything else.
    ("uuid", ArgShape::None),
    ("json", ArgShape::None),
    ("jsonb", ArgShape::None),
];

/// Alias to canonical name. The right-hand side is what `format_type` returns,
/// so that a declaration and an introspected column converge on one spelling.
///
/// Every row was measured by declaring a column and reading it back.
const ALIASES: &[(&str, &str)] = &[
    ("int", "integer"),
    ("int4", "integer"),
    ("int2", "smallint"),
    ("int8", "bigint"),
    ("decimal", "numeric"),
    ("dec", "numeric"),
    ("float4", "real"),
    ("float8", "double precision"),
    ("bool", "boolean"),
    ("char", "character"),
    ("varchar", "character varying"),
    ("time", "time without time zone"),
    ("timetz", "time with time zone"),
    ("timestamp", "timestamp without time zone"),
    ("timestamptz", "timestamp with time zone"),
];

/// The names among this dialect's that are rows of `pg_type` and not words
/// of the grammar: the ones a declaration may qualify, `pg_catalog.text`,
/// or quote, `"int4"`, and still name the type. **Measured** on 18.6 with
/// `to_regtype('pg_catalog."<name>"')`: `integer`, `boolean`, `character
/// varying` and `double precision` are grammar and resolve to nothing once
/// quoted or qualified, while these do. `bpchar`, the catalog's name for
/// `character`, is left out: `NULL::bpchar` on a `character` column is a
/// default the engine keeps, its modifier not the column's. `char` is left
/// out too: `"char"` in quotes is the engine's one-byte internal type, not
/// `character` (DECISIONS 364).
const CATALOG_NAMES: &[&str] = &[
    "numeric",
    "text",
    "bytea",
    "date",
    "interval",
    "uuid",
    "json",
    "jsonb",
    "int2",
    "int4",
    "int8",
    "float4",
    "float8",
    "bool",
    "varchar",
    "time",
    "timetz",
    "timestamp",
    "timestamptz",
];

/// `ty` as the grammar spells it: a `pg_catalog.` qualification taken off,
/// quotes taken off a quoted name, a bare name folded to lower case as the
/// lexer folds it. `None` where the qualified or quoted name is not one the
/// catalog has under that spelling — another schema's type is another type,
/// a domain over `text` among them, and `"integer"` or `pg_catalog.integer`
/// is no type at all — or where the text is not a name. A plain unquoted,
/// unqualified `ty` comes back as it is (DECISIONS 364).
pub(crate) fn as_the_grammar_spells(ty: &str) -> Option<String> {
    let ty = ty.trim();
    let (head, tail) = match ty.find(['(', '[']) {
        Some(at) => ty.split_at(at),
        None => (ty, ""),
    };
    if !head.contains(['"', '.']) {
        return Some(ty.to_owned());
    }
    let (first, rest) = identifier(head)?;
    let (name, rest) = match rest.trim_start().strip_prefix('.') {
        Some(after) => {
            if !(first.starts_with('"') && first == "\"pg_catalog\""
                || !first.starts_with('"') && first.eq_ignore_ascii_case("pg_catalog"))
            {
                return None;
            }
            identifier(after.trim_start())?
        }
        None => (first, rest),
    };
    if !rest.trim().is_empty() {
        return None;
    }
    let name = match name.strip_prefix('"') {
        Some(quoted) => quoted.strip_suffix('"')?.to_owned(),
        None => name.to_ascii_lowercase(),
    };
    CATALOG_NAMES
        .contains(&name.as_str())
        .then(|| format!("{name}{tail}"))
}

/// One identifier at the start of `s` — `"quoted"`, its quotes kept, with
/// no `""` inside (a name with a quote in it is no type of this dialect's),
/// or a bare word of letters, digits and `_` — and what follows it.
fn identifier(s: &str) -> Option<(&str, &str)> {
    if let Some(inner) = s.strip_prefix('"') {
        let close = inner.find('"')?;
        if inner[close + 1..].starts_with('"') {
            return None;
        }
        return Some(s.split_at(close + 2));
    }
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    (end > 0).then(|| s.split_at(end))
}

fn shape_of(base: &str) -> Option<ArgShape> {
    CATALOGUE.iter().find(|(n, _)| *n == base).map(|(_, s)| *s)
}

fn canonical_base(base: &str) -> &str {
    ALIASES
        .iter()
        .find(|(a, _)| *a == base)
        .map_or(base, |(_, c)| *c)
}

fn unknown(ty: &ColumnType) -> DialectError {
    DialectError::UnknownType {
        dialect: DIALECT,
        ty: ty.to_string(),
    }
}

fn arity(ty: &ColumnType, detail: impl Into<String>) -> DialectError {
    DialectError::BadTypeArity {
        dialect: DIALECT,
        ty: ty.to_string(),
        detail: detail.into(),
    }
}

/// A spelling the *model* cannot hold, as distinct from one the engine lacks.
///
/// [`DialectError::NotBuilt`] rather than `Unsupported`, and the distinction is
/// the one that variant exists for: PostgreSQL has arrays and it has
/// `timestamp(3)`. What is missing is a `pbps-model` representation, and a
/// reader sent to the engine's documentation for a limitation of this tool
/// would look in the wrong place.
fn needs_a_model_change(part: impl Into<String>) -> DialectError {
    DialectError::NotBuilt {
        dialect: DIALECT,
        part: part.into(),
    }
}

/// The `serial` family: spellings that are not types (ADR-0011 Amendment 3).
///
/// Refused rather than normalized, and the reason is the contract on
/// `Dialect::normalize_type` — its output is what introspection reads back for
/// a column declared that way. PostgreSQL expands each of these into an integer
/// column plus an owned sequence and reads the column back as that integer
/// type, so no normalization can make the declared spelling equal the one that
/// comes back. Left alone it is a schema that differs from itself on every run:
/// the permanent phantom change.
///
/// The replacement is named in the refusal, because ADR-0010 §7 measured that
/// it also disposes of a second problem: an identity column needs no sequence
/// privilege, and a `serial` one does (`ERROR: permission denied for sequence
/// ser_id_seq`). Measured on PostgreSQL 18.6 by the live suite in this crate,
/// which is where the read-back spellings below come from (DECISIONS 227).
pub fn refuse_serial(ty: &ColumnType, column: Option<&str>) -> Option<DialectError> {
    let base = ty.base.to_ascii_lowercase();
    let reads_back = match base.as_str() {
        "smallserial" | "serial2" => "smallint",
        "bigserial" | "serial8" => "bigint",
        "serial" | "serial4" => "integer",
        // Not one of them, and that is the answer: this function refuses a
        // closed list and normalizes nothing.
        _ => return None,
    };
    let at = column.map_or_else(String::new, |name| format!("column `{name}`: "));
    Some(DialectError::Invalid {
        dialect: DIALECT,
        message: format!(
            "{at}`{declared}` is a macro, not a type. The column is created as `{reads_back}` \
             with an owned sequence, and `{reads_back}` is what introspection reads back, so a \
             declared `{declared}` would differ from itself on every run. Declare \
             `{reads_back}` with an `identity:` instead, which this engine emits as \
             GENERATED ... AS IDENTITY and which needs no sequence privilege.",
            declared = ty.base,
        ),
    })
}

/// Whether this base name is an array spelling, in the one form that reaches
/// here.
///
/// `text[]` never does: `ColumnType::from_str` allows only `[A-Za-z0-9_ ]` in a
/// base name, so the square brackets are refused by the loader. **The
/// SQL-standard spelling is not**, because spaces are legal in a base name
/// (`double precision`, `timestamp with time zone`) — so `text ARRAY` loads
/// happily as the base name `text array`, which no catalog will ever return.
/// That is the dangerous one, and ADR-0012 §1 is explicit that both spellings
/// have to fail rather than one of them being silently accepted into a
/// comparison it can never win (DECISIONS 240; the model change is issue #130).
fn is_array(base: &str) -> bool {
    base.split_whitespace().next_back() == Some("array")
}

/// Expands aliases, fills in the arguments PostgreSQL fills in itself, and
/// resolves the spellings the engine resolves.
pub fn normalize(ty: &ColumnType) -> Result<ColumnType, DialectError> {
    if let Some(refusal) = refuse_serial(ty, None) {
        return Err(refusal);
    }
    // `ColumnType` already lowercases and squeezes whitespace inside arguments,
    // but a multi-word base name such as `double  precision` can still carry
    // runs of spaces.
    let base_words: Vec<&str> = ty.base.split_whitespace().collect();
    let base = base_words.join(" ");
    if is_array(&base) {
        return Err(needs_a_model_change(
            "an array column. `ColumnType` holds a base name and its arguments, with nowhere \
             to put a dimension, so `text[]` and `text ARRAY` are both refused rather than one \
             of them loading as the base name `text array` — which no catalog returns, so the \
             column would be reported as changed on every run and no plan could fix it \
             (ADR-0012 §1)",
        ));
    }
    let base = canonical_base(&base).to_owned();
    // `float(n)` is resolved before the catalogue is consulted, and is not in
    // it: the spelling is not a type this dialect ever returns, and which type
    // it becomes depends on its argument, which no alias table can express.
    if base == "float" {
        return resolve_float(ty);
    }
    let Some(shape) = shape_of(&base) else {
        return Err(unknown(ty));
    };

    let args = match shape {
        ArgShape::None => {
            if !ty.args.is_empty() {
                return Err(arity(ty, format!("`{base}` takes no arguments")));
            }
            Vec::new()
        }

        ArgShape::Length { fills_in } => match ty.args.as_slice() {
            [] => fills_in.map(TypeArg::Int).into_iter().collect(),
            [TypeArg::Int(n)] if (1..=MAX_STRING_LEN).contains(n) => vec![TypeArg::Int(*n)],
            [TypeArg::Int(n)] => {
                return Err(arity(
                    ty,
                    format!("the length must be between 1 and {MAX_STRING_LEN}, got {n}"),
                ));
            }
            // `max` is SQL Server's word, and this engine has no equivalent:
            // the unbounded form of `character varying` is the one with no
            // argument at all.
            _ => {
                return Err(arity(
                    ty,
                    format!("`{base}` takes one length, or none for the unbounded form"),
                ));
            }
        },

        ArgShape::Numeric => match ty.args.as_slice() {
            // Bare `numeric` is arbitrary precision, and stays that way: the
            // engine reads it back with no arguments, so filling any in would
            // declare a different type from the one that comes back.
            [] => Vec::new(),
            [TypeArg::Int(p)] => {
                check_numeric(ty, *p, 0)?;
                vec![TypeArg::Int(*p), TypeArg::Int(0)]
            }
            [TypeArg::Int(p), TypeArg::Int(s)] => {
                check_numeric(ty, *p, *s)?;
                vec![TypeArg::Int(*p), TypeArg::Int(*s)]
            }
            _ => {
                return Err(arity(
                    ty,
                    "`numeric` takes a precision and an optional scale, or nothing at all",
                ));
            }
        },

        ArgShape::IntervalPrecision => match ty.args.as_slice() {
            [] => Vec::new(),
            [TypeArg::Int(n)] if (0..=MAX_INTERVAL_PRECISION).contains(n) => {
                vec![TypeArg::Int(*n)]
            }
            [TypeArg::Int(n)] => {
                return Err(arity(
                    ty,
                    format!(
                        "the precision of `interval` must be between 0 and \
                         {MAX_INTERVAL_PRECISION}, got {n}. The engine does not refuse a larger \
                         one — it stores `interval({MAX_INTERVAL_PRECISION})` and says nothing, \
                         so a declaration that kept it would read back as something else"
                    ),
                ));
            }
            _ => return Err(arity(ty, "`interval` takes one precision")),
        },

        ArgShape::NoModifier => {
            if !ty.args.is_empty() {
                return Err(needs_a_model_change(format!(
                    "a precision on `{base}`. The engine spells the modifier **inside** the \
                     name — `timestamptz(3)` reads back as `timestamp(3) with time zone`, and \
                     `timestamp with time zone(3)` is a syntax error — and a `ColumnType` is a \
                     name followed by its arguments, with nowhere to put one in the middle. \
                     Declare `{base}` without a precision, which is the engine's own default \
                     of microseconds"
                )));
            }
            Vec::new()
        }
    };

    Ok(ColumnType::new(base, args))
}

/// `float(n)` is not stored as written: 1..=24 becomes `real` and 25..=53
/// becomes `double precision`. Normalizing to what is stored is what stops
/// `float(30)` from looking like a change on every run.
fn resolve_float(ty: &ColumnType) -> Result<ColumnType, DialectError> {
    match ty.args.as_slice() {
        // Bare `float` is `double precision`, measured — not `real`, which is
        // the SQL Server-shaped guess.
        [] => Ok(ColumnType::simple("double precision")),
        [TypeArg::Int(n)] if (1..=24).contains(n) => Ok(ColumnType::simple("real")),
        [TypeArg::Int(n)] if (25..=MAX_FLOAT_BITS).contains(n) => {
            Ok(ColumnType::simple("double precision"))
        }
        [TypeArg::Int(n)] => Err(arity(
            ty,
            format!("the precision of `float` must be between 1 and {MAX_FLOAT_BITS}, got {n}"),
        )),
        _ => Err(arity(ty, "`float` takes one precision")),
    }
}

fn check_numeric(ty: &ColumnType, p: i64, s: i64) -> Result<(), DialectError> {
    if !(1..=MAX_NUMERIC_PRECISION).contains(&p) {
        return Err(arity(
            ty,
            format!(
                "the precision of `numeric` must be between 1 and {MAX_NUMERIC_PRECISION}, got {p}"
            ),
        ));
    }
    // A scale larger than the precision is legal here, and so is a negative
    // one — `numeric(10,-5)` rounds to the nearest hundred thousand and reads
    // back as itself. Copying SQL Server's `0 <= s <= p` would refuse a
    // declaration the engine accepts.
    if !(-MAX_NUMERIC_SCALE..=MAX_NUMERIC_SCALE).contains(&s) {
        return Err(arity(
            ty,
            format!(
                "the scale of `numeric` must be between -{MAX_NUMERIC_SCALE} and \
                 {MAX_NUMERIC_SCALE}, got {s}"
            ),
        ));
    }
    Ok(())
}

/// The values a column of this type may hold, if it may carry an `identity:`
/// at all; `None` if it may not.
///
/// The range and the permission are one answer rather than two, because every
/// caller that wants the first has already had to ask the second, and a pair of
/// functions is a pair that can disagree.
///
/// **Measured on 18.6**, and the engine says it in so many words: `identity
/// column type must be smallint, integer, or bigint`. Not the shape of its SQL
/// Server counterpart, which admits a `decimal` with scale zero — here
/// `numeric(10,0)` is refused like any other.
pub fn identity_range(ty: &ColumnType) -> Option<RangeInclusive<i64>> {
    match normalize(ty).ok()?.base.as_str() {
        "smallint" => Some(i64::from(i16::MIN)..=i64::from(i16::MAX)),
        "integer" => Some(i64::from(i32::MIN)..=i64::from(i32::MAX)),
        "bigint" => Some(i64::MIN..=i64::MAX),
        _ => None,
    }
}

/// The values the sequence behind an `identity:` will accept as its start.
///
/// **Not the column's range**, which is the answer that looks right and is
/// wrong in both directions. The model can spell only a seed and an increment,
/// so the sequence gets PostgreSQL's default bounds, and those depend on which
/// way it counts: ascending it is `1 .. type_max` — a seed of `0` fits every
/// integer type and is still refused, `START value (0) cannot be less than
/// MINVALUE (1)` — and descending it is `type_min .. -1`, where a seed of `5`
/// is refused for being *too large*. Both measured on 18.6.
///
/// (PITFALLS, "One property of a type standing in for what it holds".)
pub fn identity_seed_range(ty: &ColumnType, increment: i64) -> Option<RangeInclusive<i64>> {
    let held = identity_range(ty)?;
    Some(if increment < 0 {
        *held.start()..=-1
    } else {
        1..=*held.end()
    })
}

// ---------------------------------------------------------------------------
// Risk
// ---------------------------------------------------------------------------

/// The families a type belongs to for the purpose of judging a change.
///
/// A change within a family is a question of capacity; a change across families
/// is a conversion, and PostgreSQL refuses most of those outright — measured, a
/// twenty-by-twenty matrix of `ALTER TABLE ... ALTER COLUMN ... TYPE` on an
/// empty table, which is the engine's own answer to "is this conversion
/// possible at all" with no data to confuse it. The live suite runs that matrix
/// against a real server and asserts this file agrees with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// Exact numeric. Two shapes, because the engine has two: the native
    /// integer types hold a fixed set of whole numbers, and `numeric` holds a
    /// decimal range **plus `NaN`** — which is the difference that decides
    /// several of the answers below.
    Exact(Exact),
    /// Approximate numeric, described by the largest integer it holds
    /// **exactly** — 2^24 for `real`, 2^53 for `double precision`.
    ///
    /// Not a count of decimal digits, and that is the same correction one step
    /// further: a decimal that survives the engine's own printing is not a
    /// decimal the float holds. Measured, `0.1` stored in a `real` is
    /// `0.10000000149011612`, and ten of them sum to `1.0000001` where the
    /// exact sum is `1.0`.
    Approx {
        max_exact_int: i128,
    },
    Bool,
    /// A string, described by its length and whether it is blank-padded to a
    /// fixed width.
    Text {
        len: Len,
        fixed: bool,
    },
    Bytea,
    /// Date and time, described by which components it stores and how far its
    /// date part reaches.
    ///
    /// The reach is not decoration: measured, a `date` runs to 5874897 AD and a
    /// `timestamp` stops at 294276 AD, so `'300000-01-01'::date` into a
    /// `timestamp` is `date out of range for timestamp`. `last_year` is `None`
    /// for the types that hold no date at all.
    Temporal {
        has_date: bool,
        has_time: bool,
        has_offset: bool,
        last_year: Option<i64>,
    },
    /// `interval`, which carries a seconds precision the engine converts
    /// between. The unmodified type is `interval(6)` in every way that matters
    /// here, so it is spelled as one.
    Interval {
        precision: i64,
    },
    Uuid,
    /// `json` and `jsonb` are one family and not one type: the conversion
    /// between them loses data in one direction only.
    Json {
        binary: bool,
    },
    /// A type this catalogue does not know. Only a change to itself is safe,
    /// and `from == to` has already answered that.
    Unknown,
}

/// An exact numeric type, as the two shapes PostgreSQL actually has.
///
/// Kept apart rather than described by one set of numbers, because two of the
/// facts that matter are true of one shape and not the other: the integer types
/// are not powers of ten, and `numeric` holds `NaN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exact {
    /// `smallint`, `integer`, `bigint`: every whole number up to `max`, and
    /// nothing else. No `NaN`, no infinity.
    ///
    /// A magnitude and not a digit count, because these are not powers of ten:
    /// `numeric(10,0)` and `integer` are both "ten digits", and measured,
    /// `9999999999` into an `integer` is `integer out of range`.
    Integer { max: i128 },
    /// `numeric(p, s)`, described by the exponent its integer part reaches
    /// (`p - s`, which may be **negative** — `numeric(2,3)` holds values below
    /// `0.1`) and by the digits it keeps after the point. `None` is the
    /// unbounded declaration, which holds everything.
    Numeric {
        int_digits: Option<i64>,
        scale: Option<i64>,
    },
}

/// A string length. `text` and a bare `character varying` are unbounded, which
/// is why this cannot be an `i64` sentinel — a length of zero would compare the
/// wrong way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Len {
    Bounded(i64),
    Unbounded,
}

impl Len {
    /// Whether a value of length `self` always fits in a column of length
    /// `other`.
    fn fits_in(self, other: Len) -> bool {
        match (self, other) {
            (_, Len::Unbounded) => true,
            (Len::Unbounded, Len::Bounded(_)) => false,
            (Len::Bounded(a), Len::Bounded(b)) => a <= b,
        }
    }
}

/// `10^n` as a magnitude, or `None` when it is larger than any type here can
/// hold. `numeric`'s precision goes to 1000 and its scale to -1000, so the
/// exponent reaches 2000 and an unchecked `pow` would wrap — into a *small*
/// number, which would call an enormous type narrow enough to fit anywhere.
fn pow10(n: i64) -> Option<i128> {
    if n <= 0 {
        return Some(1);
    }
    u32::try_from(n).ok().and_then(|n| 10_i128.checked_pow(n))
}

/// How many decimal digits `max` is written with, which is the `p - s` a
/// `numeric` needs in order to hold every value up to it.
fn digits10(max: i128) -> i64 {
    let mut digits = 1;
    let mut rest = max / 10;
    while rest != 0 {
        digits += 1;
        rest /= 10;
    }
    digits
}

/// Whether every value of `from` fits in `to`.
fn exact_holds(from: Exact, to: Exact) -> bool {
    match (from, to) {
        (Exact::Integer { max: a }, Exact::Integer { max: b }) => a <= b,
        // A `numeric` wide enough for the integer's largest value holds all of
        // it, and a scale below zero would round it away.
        (Exact::Integer { max }, Exact::Numeric { int_digits, scale }) => {
            fits(Some(digits10(max)), int_digits) && fits(Some(0), scale)
        }
        // **Never**, whatever the widths. `NaN` is a value every `numeric`
        // holds — measured, `'NaN'::numeric(4,0)` is accepted — and no integer
        // type has one: `cannot convert NaN to smallint`. So a row nobody
        // looked at makes the statement fail, which is exactly what the
        // narrowing class is for.
        (Exact::Numeric { .. }, Exact::Integer { .. }) => false,
        (
            Exact::Numeric {
                int_digits: ad,
                scale: asc,
            },
            Exact::Numeric {
                int_digits: bd,
                scale: bsc,
            },
        ) => fits(ad, bd) && fits(asc, bsc),
    }
}

/// Whether a binary float that holds integers up to `max_exact_int` holds every
/// value of `from` exactly.
fn exact_in_float(from: Exact, max_exact_int: i128) -> bool {
    match from {
        Exact::Integer { max } => max <= max_exact_int,
        // A decimal fraction is not a binary fraction, so a positive scale is
        // out. What is left is whole numbers, and they have to fit below the
        // mantissa's first gap. `NaN` and infinity pass through unchanged
        // (measured), so neither is a reason to refuse this one.
        Exact::Numeric { int_digits, scale } => {
            scale.is_some_and(|s| s <= 0)
                && int_digits.is_some_and(|d| pow10(d).is_some_and(|p| p - 1 <= max_exact_int))
        }
    }
}

/// Whether a bound holds another, where `None` is unbounded: it holds
/// everything, and nothing but itself holds it.
fn fits<T: Ord>(from: Option<T>, to: Option<T>) -> bool {
    match (from, to) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(x), Some(y)) => x <= y,
    }
}

fn family(t: &ColumnType) -> Family {
    let int_arg = |i: usize| match t.args.get(i) {
        Some(TypeArg::Int(n)) => Some(*n),
        Some(TypeArg::Max | TypeArg::Ident(_)) | None => None,
    };
    let len = match t.args.first() {
        Some(TypeArg::Int(n)) => Len::Bounded(*n),
        Some(TypeArg::Max | TypeArg::Ident(_)) | None => Len::Unbounded,
    };
    let integer = |max: i128| Family::Exact(Exact::Integer { max });
    let temporal = |has_date, has_time, has_offset, last_year| Family::Temporal {
        has_date,
        has_time,
        has_offset,
        last_year,
    };
    // Measured on 18.6: `'5874897-01-01'::date` is accepted and
    // `'294276-12-31'::date::timestamp` is the last one that converts.
    const DATE_LAST_YEAR: Option<i64> = Some(5_874_897);
    const STAMP_LAST_YEAR: Option<i64> = Some(294_276);

    match t.base.as_str() {
        "smallint" => integer(i128::from(i16::MAX)),
        "integer" => integer(i128::from(i32::MAX)),
        "bigint" => integer(i128::from(i64::MAX)),
        // `numeric(p, s)` holds `p` significant digits with `s` of them after
        // the point, so its integer part reaches `10^(p - s)`. The subtraction
        // is kept **signed**: a scale larger than the precision is legal here,
        // and `numeric(2,3)` holds values below `0.1` while `numeric(2,4)`
        // holds values below `0.01`. Measured, `0.099` into the second is
        // `numeric field overflow`, so collapsing both to "no integer part"
        // would call that change safe.
        "numeric" => Family::Exact(Exact::Numeric {
            int_digits: int_arg(0).map(|p| p - int_arg(1).unwrap_or(0)),
            scale: int_arg(1),
        }),
        // 2^24 and 2^53: the largest integers with no gap below them. Measured,
        // `16777217` into a `real` reads back as `16777200`.
        "real" => Family::Approx {
            max_exact_int: 1 << 24,
        },
        "double precision" => Family::Approx {
            max_exact_int: 1 << 53,
        },
        "boolean" => Family::Bool,
        "character" => Family::Text { len, fixed: true },
        "character varying" => Family::Text { len, fixed: false },
        "text" => Family::Text {
            len: Len::Unbounded,
            fixed: false,
        },
        "bytea" => Family::Bytea,
        "date" => temporal(true, false, false, DATE_LAST_YEAR),
        "time without time zone" => temporal(false, true, false, None),
        "time with time zone" => temporal(false, true, true, None),
        "timestamp without time zone" => temporal(true, true, false, STAMP_LAST_YEAR),
        "timestamp with time zone" => temporal(true, true, true, STAMP_LAST_YEAR),
        "interval" => Family::Interval {
            precision: int_arg(0).unwrap_or(MAX_INTERVAL_PRECISION),
        },
        "uuid" => Family::Uuid,
        "json" => Family::Json { binary: false },
        "jsonb" => Family::Json { binary: true },
        _ => Family::Unknown,
    }
}

/// Whether the engine will convert one date-or-time type into another at all.
///
/// A table rather than a rule, because the engine's answer is not one:
/// `timestamptz` converts to `timetz` and `timestamp` does not, and no property
/// of the two types predicts it. Measured; the live suite re-measures it.
///
/// Takes the three components rather than the [`Family`], so that there is no
/// non-temporal value to have an opinion about: the caller has destructured
/// both sides before it can ask.
fn temporal_cast_exists(from: Components, to: Components) -> bool {
    const DATE: Components = (true, false, false);
    const TIME: Components = (false, true, false);
    const TIMETZ: Components = (false, true, true);
    const STAMP: Components = (true, true, false);
    const STAMPTZ: Components = (true, true, true);
    matches!(
        (from, to),
        (DATE, DATE | STAMP | STAMPTZ)
            | (TIME, TIME | TIMETZ)
            | (TIMETZ, TIME | TIMETZ)
            | (STAMP, DATE | TIME | STAMP | STAMPTZ)
            | (STAMPTZ, DATE | TIME | TIMETZ | STAMP | STAMPTZ)
    )
}

/// Whether the result of this change is decided by the applying session's
/// `TimeZone` rather than by anything declared.
///
/// True for two shapes, and both are the same question asked twice: a
/// date-or-time type gaining or losing its offset, and one keeping its offset
/// while the date part comes or goes.
/// The engine reads a naive value in the session's zone on the way in, and
/// writes one back out in it on the way out — **measured**, the same
/// `ALTER COLUMN t TYPE timestamptz` over a stored `2026-01-02 12:00` gives
/// `12:00:00+00` from a `UTC` session and `17:00:00+00` from
/// `America/New_York`. Two operators applying the same approved plan store two
/// different instants, and nothing afterwards can tell which happened.
///
/// [`change_risk`] already knows this — its temporal arm requires `ao == bo`
/// and says why — but it can only answer `Narrowing`, and `Narrowing` is a risk
/// class a human clears at the gate. What a human cleared was the loss, not the
/// zone: the zone was never in the plan to approve. So this is a separate
/// question with a separate answer, and the emitter refuses on it the way it
/// refuses a `USING` clause (ADR-0012 §5) — a transformation nobody declared is
/// not one a tool gets to choose.
///
/// Both types must already be normalized, as everywhere else here.
pub fn depends_on_the_session_time_zone(from: &ColumnType, to: &ColumnType) -> bool {
    match (family(from), family(to)) {
        (
            Family::Temporal {
                has_date: ad,
                has_offset: ao,
                ..
            },
            Family::Temporal {
                has_date: bd,
                has_offset: bo,
                ..
            },
        ) => {
            // A zone has to be involved at all — with no offset on either side
            // there is nothing for `TimeZone` to be read into, and `date` into
            // `timestamp` is midnight either way.
            //
            // And one pair with a zone is still not the session's: `timetz`
            // physically stores a local time *and* its offset, so dropping the
            // offset keeps the time that is already there. Measured,
            // `12:00:00+03` into `time` is `12:00:00` from a `UTC` session and
            // from an `America/New_York` one alike. `timestamptz` is the
            // opposite, and that is why the exception is this narrow: it holds
            // an *instant*, so writing it without a zone means choosing one —
            // measured, the same value into `timestamp` is `12:00:00` under
            // UTC and `07:00:00` under New York. What decides it is what the
            // type holds, not which way the offset went.
            let a_projection = ao && !bo && !ad && !bd;
            (ao || bo)
                // Then either end of the zone moves: the offset is gained or
                // lost, or the value is rebased because the *date* is. The
                // second is the one an earlier version of this missed, because
                // it read the hazard as "the offset changes" rather than as
                // "the session decides". `timestamptz` into `timetz` keeps its
                // offset and is still the session's answer — measured, one
                // stored `2026-01-02 12:00:00+00` becomes `12:00:00+00` from a
                // `UTC` session and `07:00:00-05` from `America/New_York`.
                //
                // On the pair rather than on one side, because both directions
                // are the same question: a date part appearing is a value
                // being placed in a day, and one disappearing is a value being
                // read out of one, and a zone decides both.
                && (ao != bo || ad != bd)
                && !a_projection
        }
        _ => false,
    }
}

/// A date-or-time type as `(has_date, has_time, has_offset)`.
type Components = (bool, bool, bool);

fn safe_if(condition: bool) -> TypeChangeRisk {
    if condition {
        TypeChangeRisk::Safe
    } else {
        TypeChangeRisk::Narrowing
    }
}

/// How safe changing `from` into `to` is. Both must already be normalized.
///
/// The question answered is "can this kind of change fail or lose data at all",
/// never "will it, given today's rows" — inspecting data is a runtime concern
/// (SPEC §7.2). When the answer is not clearly no, it is yes.
///
/// **It is not "how long will it take".** `integer -> bigint` is `Safe` here and
/// rewrites the whole table under an `AccessExclusiveLock`, measured at six
/// hundred times the cost of a change that does not. That cost belongs to the
/// estimate (SPEC 14.1) and is kept out of this classification on purpose:
/// folding an availability cost into a correctness class teaches a reviewer
/// that the classes do not mean what they say (ADR-0012 §3, DECISIONS 241).
pub fn change_risk(from: &ColumnType, to: &ColumnType) -> TypeChangeRisk {
    if from == to {
        return TypeChangeRisk::Safe;
    }
    let (a, b) = (family(from), family(to));

    match (a, b) {
        (Family::Exact(a), Family::Exact(b)) => safe_if(exact_holds(a, b)),

        // An exact value survives a binary float only if the float holds it
        // **exactly**. Not "do the decimal digits fit": measured, `0.1` in a
        // `real` is `0.10000000149011612` and ten of them sum to `1.0000001`
        // against an exact `1.0`, while `0.1::real::text` is `0.1` — the
        // engine's own printing hides it, and so does every round trip through
        // text.
        (Family::Exact(a), Family::Approx { max_exact_int }) => {
            safe_if(exact_in_float(a, max_exact_int))
        }
        (Family::Approx { max_exact_int: a }, Family::Approx { max_exact_int: b }) => {
            safe_if(b >= a)
        }
        // A float into an exact type rounds, measured: `1.5` becomes `2`.
        (Family::Approx { .. }, Family::Exact(..)) => TypeChangeRisk::Narrowing,

        (Family::Text { len: al, fixed: af }, Family::Text { len: bl, fixed: bf }) => {
            // Variable into fixed blank-pads every existing value to the
            // declared width, and the padding is not recoverable: a trailing
            // space a user stored is indistinguishable from one the engine
            // added.
            if !af && bf {
                return TypeChangeRisk::Narrowing;
            }
            safe_if(al.fits_in(bl))
        }

        // Everything in this catalogue converts to a string — measured, every
        // row of the matrix is accepted into `character`, `character varying`
        // and `text`. It is never `Safe`: the rendered form may not fit a
        // bounded target (measured, `integer` holding `1234567890` into
        // `character varying(9)` is `value too long`), the rendering of a
        // `bytea` or a `date` depends on session settings ADR-0013 spends
        // whole decisions on, and the round trip is not the identity.
        (
            Family::Exact { .. }
            | Family::Approx { .. }
            | Family::Bool
            | Family::Bytea
            | Family::Temporal { .. }
            | Family::Interval { .. }
            | Family::Uuid
            | Family::Json { .. },
            Family::Text { .. },
        ) => TypeChangeRisk::Narrowing,

        (
            Family::Temporal {
                has_date: ad,
                has_time: at,
                has_offset: ao,
                last_year: ay,
            },
            Family::Temporal {
                has_date: bd,
                has_time: bt,
                has_offset: bo,
                last_year: by,
            },
        ) => {
            if !temporal_cast_exists((ad, at, ao), (bd, bt, bo)) {
                return TypeChangeRisk::Incompatible;
            }
            // The target has to keep every component the source stores, and the
            // time zone has to stay as it was. Gaining or losing one is not a
            // widening even when it looks like one: the conversion uses the
            // *session's* `TimeZone`, so the same plan applied from two
            // machines writes two different instants — which is also why
            // ADR-0012 §4 measured it rewriting under one session and not the
            // other.
            //
            // And the target's calendar has to reach as far as the source's.
            // Measured, a `date` runs to 5874897 AD where a `timestamp` stops
            // at 294276 AD, so `'300000-01-01'` is a row that makes the change
            // fail — `date out of range for timestamp` — after a `Safe`
            // classification had waved it past the gate.
            let reaches = match (ay, by) {
                // The source holds no date, so there is none to lose.
                (None, _) => true,
                (Some(_), None) => false,
                (Some(a), Some(b)) => a <= b,
            };
            safe_if((!ad || bd) && (!at || bt) && ao == bo && reaches)
        }

        // `time without time zone` and nothing else, in both of these arms:
        // the engine converts neither `timetz` nor `timestamp` into an
        // interval, measured, so a rule phrased as "anything with a time part"
        // would call two refusals a conversion.
        //
        // Measured: `interval '30 hours'` into `time` is accepted and stores
        // `06:00:00`. Nothing fails, and a day and a half is gone.
        (
            Family::Interval { .. },
            Family::Temporal {
                has_date: false,
                has_time: true,
                has_offset: false,
                ..
            },
        ) => TypeChangeRisk::Narrowing,

        // The other way is a length of time keeping its length. Measured on
        // every boundary a `time` has — `00:00:00`, `24:00:00`, and
        // `23:59:59.999999` — each one reads back from the `interval`
        // unchanged, and there is no value of the source that has nowhere to
        // land.
        //
        // What can still lose is the seconds precision, and it is the target's
        // alone: a `time` is always microseconds here, because a declared
        // precision on it is refused outright (DECISIONS 241) — the model
        // cannot spell one. Measured, `12:34:56.654321` into `interval(6)` is
        // itself, into `interval(5)` is `12:34:56.65432`, and into
        // `interval(0)` is `12:34:57`: it rounds rather than truncates, which
        // makes a shorter interval lossy in the last place for almost every
        // value rather than for the rare one.
        (
            Family::Temporal {
                has_date: false,
                has_time: true,
                has_offset: false,
                ..
            },
            Family::Interval { precision },
        ) => safe_if(precision >= MAX_INTERVAL_PRECISION),

        // One `interval` into another is a question about the seconds
        // precision, which the engine will convert either way. Measured:
        // `interval(0)` into `interval(6)` keeps `00:00:01`, and `interval(6)`
        // into `interval(0)` turns `00:00:01.234567` into `00:00:01`.
        (Family::Interval { precision: a }, Family::Interval { precision: b }) => safe_if(b >= a),

        // `json -> jsonb` is the direction that loses: measured,
        // `{"a": 1,  "a": 2}` becomes `{"a": 2}` — duplicate keys, key order
        // and whitespace are all gone. The reverse renders the canonical text
        // of a value that already had it, and loses nothing.
        (Family::Json { binary: false }, Family::Json { binary: true }) => {
            TypeChangeRisk::Narrowing
        }
        (Family::Json { binary: true }, Family::Json { binary: false }) => TypeChangeRisk::Safe,

        // Everything else is a conversion this engine refuses outright, before
        // it looks at a single row: `character varying -> integer` is `column
        // "c" cannot be cast automatically to type integer`. The remedy the
        // engine names is a `USING` clause, and ADR-0012 §5 rules that out —
        // a cast pbps chose is a data transformation nobody declared.
        (
            Family::Exact { .. }
            | Family::Approx { .. }
            | Family::Bool
            | Family::Text { .. }
            | Family::Bytea
            | Family::Temporal { .. }
            | Family::Interval { .. }
            | Family::Uuid
            | Family::Json { .. }
            | Family::Unknown,
            _,
        ) => TypeChangeRisk::Incompatible,
    }
}

/// Whether this type's text rendering is the same in **every** session — the
/// question a length probe must answer before it may measure one.
///
/// A probe is issued before the deployment's transaction framing is
/// established, so it runs under the operator's own settings while the
/// statement it clears runs under the ones that framing pins (DECISIONS 267:
/// `DateStyle`, `TimeZone`, `IntervalStyle`, `timezone_abbreviations`,
/// `transform_null_equals`, `bytea_output`, `extra_float_digits`). Where a
/// rendering moves between the two, the length the probe measures is not the
/// length the `ALTER` will measure — and it goes wrong in **both** directions,
/// which is why the answer is an allow-list and not a correction. Where the
/// operator's rendering is the longer (a `bytea` under `escape`, a `timestamp`
/// under `Postgres`), the probe counts rows this engine would have taken and a
/// valid plan is refused. Where it is the shorter (an `interval` under
/// `sql_standard`, a `float8` under a lower `extra_float_digits`), the probe
/// counts nothing and clears a statement the engine then refuses — the exact
/// failure a probe exists to prevent, arriving through the probe.
///
/// **Measured on 18.6**, each value rendered under the pinned setting and
/// under another, as character counts:
///
/// ```text
/// bytea       '\x0102'              hex 6         escape 8
/// interval    '1 day 02:00:00'      postgres 14   sql_standard 9
/// timestamp   '2026-01-02 12:00'    ISO 19        Postgres 24
/// timestamptz the same, +00         UTC 22        Asia/Kolkata 25
/// float8      1.0/3.0               digits 1 18   digits 0 17   digits -5 12
/// ```
///
/// The pinned column is the left one, and it is not consistently the longer or
/// the shorter: `bytea` and the date-and-time types render longer unpinned,
/// `interval` and `float8` render shorter.
///
/// and, with all of those settings changed at once against the pinned ones,
/// `json` (37), `jsonb` (38), `uuid` (36), `boolean` (4) and `numeric` (10) do
/// not move. A `date` does not move either — every `DateStyle` this engine has
/// prints ten characters for one — but it stays excluded with the rest of its
/// family rather than being carved out, because that equality is a coincidence
/// of the styles that exist and not a property anything promises.
///
/// This is an **allow-list**, and deliberately: a type this catalogue does not
/// know renders however its own output function chooses, and `lc_monetary`,
/// which decides how `money` prints, is not pinned at all — `CANONICAL_PATH`
/// says why it cannot be. Silence for a rendering nobody measured is the
/// answer this repo wants; a guess is not.
fn renders_alike_everywhere(t: &ColumnType) -> bool {
    matches!(
        family(t),
        Family::Exact(..) | Family::Bool | Family::Text { .. } | Family::Uuid | Family::Json { .. }
    )
}

/// What a stored value must satisfy for `ALTER COLUMN … TYPE` to **fail** on
/// it — the predicate a pre-flight probe counts (SPEC §7.5), over `value`.
///
/// `None` where no count exists, which is not the same as "nothing can go
/// wrong": a change may lose data without ever failing, and those are listed
/// in [`crate::preflight`]'s own documentation rather than answered here with
/// a zero. Reducing a `numeric`'s scale rounds (measured, `1.55` into
/// `numeric(10,1)` is `1.6`), a float into an integer rounds (`1.5` is `2`),
/// a shorter `interval` rounds, `json` into `jsonb` drops duplicate keys and
/// whitespace, and `double precision` into `real` drops precision. Not one of
/// them raises, so there is no row to point at; the `narrowing` class is what
/// stops them at the gate.
///
/// `None` also where the count would be **measured wrong**: a length taken over
/// a rendering the session settings move is not the length the `ALTER` takes,
/// so a `bytea`, an `interval`, a date-and-time type or a binary float into a
/// bounded string gets no probe at all. `renders_alike_everywhere` holds that
/// list and the measurements behind it.
///
/// # Why this is a predicate and not a cast
///
/// The obvious probe — "count the rows a cast rejects" — cannot be written on
/// this engine, and writing it anyway is worse than having no probe.
/// **Measured on 18.6**: an explicit `CAST` to a bounded string *truncates*
/// where the `ALTER` *refuses*.
///
/// ```text
/// SELECT 'abcde'::varchar(4);                     -- 'abcd'
/// ALTER TABLE t ALTER COLUMN v TYPE varchar(4);   -- ERROR: value too long
/// ```
///
/// So a cast-based probe over a table holding `'abcde'` counts **zero** and
/// reports the change safe, and the statement it cleared then fails. That is
/// the shape [`crate::preflight`] exists to prevent, arriving through the one
/// construct that looks like the answer. The cast is an *explicit* conversion
/// and the `ALTER` is an *assignment*, and only the second is what runs.
///
/// Both types must already be normalized, as everywhere else here.
pub(crate) fn cannot_become(from: &ColumnType, to: &ColumnType, value: &str) -> Option<String> {
    // Nothing to count where the engine will not attempt the change at all:
    // `emit` refuses it by name with the `USING` clause spelled out
    // (ADR-0012 §5), and a probe beside that refusal would only argue with it.
    if change_risk(from, to) != TypeChangeRisk::Narrowing {
        return None;
    }
    // `NaN` and both infinities sort **greatest** here rather than outside the
    // order — measured, `'NaN'::numeric > 1e131071` is true, and so is
    // `'NaN'::float8 = 'NaN'::float8`, where C would say neither. So a plain
    // range test catches them, and where the target *accepts* one of them it
    // has to be taken back out by name or the probe refuses a change the
    // engine makes (measured: `'NaN'::float8` into `numeric(10,2)` is `NaN`).
    let exact = format!("({value})::numeric");
    let not_nan = format!("{exact} <> 'NaN'::numeric");
    let finite =
        format!("{not_nan} AND {exact} <> 'Infinity'::numeric AND {exact} <> '-Infinity'::numeric");
    // A binary float is measured in **its own domain**, never through
    // `numeric`. `float8::numeric` on this engine goes by way of the float's
    // shortest round-tripping decimal, not its exact value, so it is a
    // *rounding* and the rounding is large where the value is: **measured on
    // 18.6**, `(-9223372036854775808::float8)::numeric` is
    // `-9223372036854780000`, four thousand million out. Both boundary tests
    // below sit exactly where that error is biggest, and both got it wrong in
    // the direction that refuses a valid plan (DECISIONS 394):
    //
    // ```text
    // -9223372036854775808::float8 -> bigint   engine: stored exactly
    //                                          numeric domain: violation
    //  3.4028235677973362e38       -> real     engine: stored as 3.4028235e+38
    //                                          numeric domain: violation
    // ```
    //
    // The comparisons are the same numbers; only the domain changes. Every
    // threshold either arm needs is exactly representable as a `float8` — the
    // integer bounds by construction, and `2^128 - 2^103` because it asks for
    // 25 mantissa bits out of 53.
    let binary = format!("({value})::float8");
    let binary_finite = format!(
        "{binary} <> 'NaN'::float8 AND {binary} <> 'Infinity'::float8 AND \
         {binary} <> '-Infinity'::float8"
    );
    match (family(from), family(to)) {
        // A bounded string target, from a source whose rendering every session
        // agrees on. The value is measured after its **trailing spaces** are
        // taken off and nothing else: measured, `'abc  '` into `varchar(3)` is
        // `'abc'` and `E'abc\t'` into the same is `value too long`. `length`,
        // not `octet_length` — the bound is in characters, measured, `'王小明'`
        // is three of them and nine bytes and fits `varchar(3)`.
        //
        // The guard is the whole difference between a length this engine will
        // measure and one only the operator's session would:
        // `renders_alike_everywhere` carries the measurements and the reason
        // (DECISIONS 389).
        (
            _,
            Family::Text {
                len: Len::Bounded(n),
                ..
            },
        ) if renders_alike_everywhere(from) => {
            Some(format!("length(rtrim(({value})::text, ' ')) > {n}"))
        }

        // An integer target. The engine tests the value it would *store*, so
        // the test is on the rounded one: measured, `2147483647.4` into
        // `integer` is accepted and `2147483647.6` is `integer out of range`.
        (Family::Exact(Exact::Numeric { .. }), Family::Exact(Exact::Integer { max })) => {
            let min = -max - 1;
            Some(format!("round({exact}) > {max} OR round({exact}) < {min}"))
        }
        // The same question from a float, and **not** through `round`, which
        // rounds the other way. Measured, this engine rounds a float to an
        // integer half-to-even and a `numeric` half-away-from-zero: `0.5`,
        // `1.5` and `2.5` become `0`, `2` and `2`. So the boundary is
        // asymmetric and is written out rather than derived — measured,
        // `2147483647.5::float8` into `integer` is out of range and
        // `-2147483648.5::float8` is `-2147483648`, which fits.
        (Family::Approx { .. }, Family::Exact(Exact::Integer { max })) => {
            let min = -max - 1;
            // No guard for `NaN` or an infinity: this engine orders `NaN`
            // greatest among floats and `Infinity` next, so both fall out of
            // the upper test and `-Infinity` out of the lower — and the engine
            // refuses all three into an integer, measured (`cannot convert
            // NaN to integer`). Counting them is the right answer, not an
            // accident of the ordering.
            Some(format!(
                "{binary} >= {max}.5::float8 OR {binary} < {min}.5::float8"
            ))
        }

        // A bounded `numeric` target. Measured, the engine's own message says
        // which value it tests — "a field with precision 10, scale 4 must
        // round to an absolute value less than 10^6" — so the test is on the
        // value rounded to the target's scale, and `999999.995` stored as
        // `numeric(10,2)` fails into `numeric(10,4)` because it is already
        // `1000000.00`. An infinity is refused (`cannot hold an infinite
        // value`) and a `NaN` is **kept**, so only the first is counted.
        (
            Family::Exact(..) | Family::Approx { .. },
            Family::Exact(Exact::Numeric {
                int_digits: Some(digits),
                scale,
            }),
        ) => {
            let scale = scale.unwrap_or(0);
            Some(format!(
                "{not_nan} AND abs(round({exact}, {scale})) >= 10::numeric^{digits}"
            ))
        }

        // A binary float target, which overflows rather than saturating:
        // measured, `3.5e38` into `real` is `value out of range: overflow`.
        // The threshold is the midpoint above the largest value the target
        // holds, and it is written as the engine's own arithmetic rather than
        // as a decimal literal three hundred digits long. **Measured** by
        // bisection: the largest `double precision` that becomes a `real` is
        // `3.4028235677973362e38` and the smallest that overflows is
        // `2^128 - 2^103` exactly; the same construction one exponent range up
        // is the `double precision` bound, and the value just below it
        // converts while the value at it does not.
        //
        // An infinity passes straight through (measured, `'Infinity'::float8`
        // into `real` is `Infinity`) and so does a `NaN`, so both leave the
        // count.
        // The same question from a float, in the float's own domain. Only the
        // `real` target reaches here: `real -> double precision` is a widening
        // and `change_risk` has already answered `Safe`, so the threshold is
        // always `2^128 - 2^103` and always representable. Were the other one
        // ever reachable, `2::float8^1024` is `Infinity` and the guard above
        // has already taken every infinity out, so it would report no
        // violation rather than a wrong one.
        (Family::Approx { .. }, Family::Approx { max_exact_int }) => {
            let (base, mantissa) = if max_exact_int <= 1 << 24 {
                (128, 103)
            } else {
                (1024, 970)
            };
            Some(format!(
                "{binary_finite} AND abs({binary}) >= (2::float8^{base} - 2::float8^{mantissa})"
            ))
        }
        (_, Family::Approx { max_exact_int }) => {
            let (base, mantissa) = if max_exact_int <= 1 << 24 {
                (128, 103)
            } else {
                (1024, 970)
            };
            Some(format!(
                "{finite} AND abs({exact}) >= (2::numeric^{base} - 2::numeric^{mantissa})"
            ))
        }

        // A date the target's calendar cannot reach. Measured,
        // `'294276-12-31'::date` is the last one that becomes a `timestamp`
        // and `'294277-01-01'` is `date out of range for timestamp`.
        (
            Family::Temporal {
                last_year: Some(_), ..
            },
            Family::Temporal {
                last_year: Some(last),
                ..
            },
        ) => Some(format!("{value} > '{last}-12-31'::date")),

        // Everything else narrows without a row to point at; the list is in
        // this function's own documentation.
        _ => None,
    }
}

/// The spelling this engine puts in a routine's identity, from a declared one
/// (ADR-0009 §1, DECISIONS 301 and 303).
///
/// The rules and the measurements behind them are on
/// `Postgres::normalize_routine_arg`, which is the only caller. Total by
/// design: a spelling this catalogue does not know is the engine's to judge,
/// and the text goes back unchanged.
pub fn routine_arg(arg: &RoutineArg) -> RoutineArg {
    let (element, array) = peel_array(arg.as_str());
    let canonical = identity_element(element);
    let spelled = if array {
        format!("{canonical}[]")
    } else {
        canonical
    };
    // The parse cannot fail for anything this function builds — it folded a
    // valid argument, and `[]` is not a character `RoutineArg` refuses — but
    // "cannot fail" is not a reason to unwrap in a normalizer: the declared
    // text is the safe answer to give back if it ever does.
    spelled.parse().unwrap_or_else(|_| arg.clone())
}

/// The first name in `arg` that is over the engine's identifier limit, if
/// there is one — quoted or bare, with a doubled quote counted once.
///
/// The limit is enforced here because the engine does not enforce it, which
/// is the rule of DECISIONS 230 one layer down: **measured**, with a type
/// `dq.t…t` of 63 bytes, `CREATE FUNCTION dq.f(a dq.t…tx)` spelling one byte
/// more is accepted with a `NOTICE` nothing reads, the routine is identified
/// as `dq.f(dq.t…t)`, and the same statement run again is refused as already
/// existing. Declared, the key is the untruncated spelling; `module_oid` finds
/// nothing under it, the routine is planned as absent every time, and the
/// `CREATE` the plan emits is the one the engine refuses.
///
/// Every part that is not a name is short — a keyword, a modifier, a
/// dimension — so a run of identifier bytes over the limit is a name over
/// it, whatever it is a name of.
pub(crate) fn overlong_name(arg: &RoutineArg) -> Option<String> {
    let mut rest = arg.as_str();
    while !rest.is_empty() {
        if rest.starts_with('"') {
            let Some(end) = quoted_len(rest) else {
                break;
            };
            let inner = rest[1..end - 1].replace("\"\"", "\"");
            if inner.len() > crate::MAX_IDENT_BYTES {
                return Some(inner);
            }
            rest = &rest[end..];
            continue;
        }
        let len = rest
            .find(|c: char| !pbps_dialect::continues_ident(c))
            .unwrap_or(rest.len());
        if len > crate::MAX_IDENT_BYTES {
            return Some(rest[..len].to_owned());
        }
        // Step over the run and the byte that ended it.
        let step = rest[len..].chars().next().map_or(0, char::len_utf8);
        rest = &rest[len + step..];
    }
    None
}

/// Whether this catalogue knows the spelling — with or without a modifier.
///
/// Not the same question as [`routine_arg`], which is total and hands an
/// unknown spelling back unchanged: this one says *whether* it did that. The
/// caller is the emitter's parameter-list gate, which reads a parameter as
/// either `type` or `name type` and has to know when the first reading is
/// already the whole answer. `double precision` is: splitting it again would
/// read `double` as a name and `precision` as a type, and measured, with a
/// user type `mq.precision` in the database, `CREATE FUNCTION mq.b(double
/// precision)` still creates `mq.b(double precision)`. The engine does not
/// offer that reading, so neither may the gate.
pub(crate) fn catalogued(arg: &RoutineArg) -> bool {
    let (element, _) = peel_array(arg.as_str());
    folded(element).is_some() || folded(&without_modifier(element)).is_some()
}

/// `double precision[][]` -> `double precision`, and "it is an array".
///
/// One `[]` comes back however many went in, and a dimension is not part of
/// the identity: **measured**, `text[][]` and `text[3]` are both `text[]`.
///
/// The standard's spelling is an array too: **measured**, `text ARRAY`,
/// `text ARRAY[4]`, `int ARRAY [2]` and `character varying array` are
/// identified as `text[]`, `text[]`, `integer[]` and `character varying[]`,
/// while `text ARRAY[]` and `text[] ARRAY` are syntax errors — so the word is
/// peeled once, after the brackets and never before them. Left in, a declared
/// `text ARRAY` keyed a routine the catalog spells `text[]`, and the routine
/// the `CREATE` had just made was not found under its own key.
///
/// Whitespace is ASCII throughout, here and in the helpers below: to this
/// engine a non-breaking space is a name byte (DECISIONS 313), so `a\u{a0}array`
/// is a type name and not `a` with the keyword after it.
pub(crate) fn peel_array(text: &str) -> (&str, bool) {
    let mut element = ascii_trim_end(text);
    let mut array = false;
    while let Some(without) = element.strip_suffix(']') {
        let Some(open) = without.rfind('[') else {
            break;
        };
        // A `[` that is not opening a dimension is part of the name.
        if without[open + 1..].chars().any(|c| !c.is_ascii_digit()) {
            break;
        }
        element = ascii_trim_end(&without[..open]);
        array = true;
    }
    // The word has to be a word of its own: `myarray` is a name, and a quoted
    // name ends in `"`.
    let n = element.len();
    if n > 5
        && element.is_char_boundary(n - 5)
        && element[n - 5..].eq_ignore_ascii_case("array")
        && element[..n - 5].ends_with(|c: char| c.is_ascii_whitespace())
    {
        element = ascii_trim_end(&element[..n - 5]);
        array = true;
    }
    (element, array)
}

fn ascii_trim_end(text: &str) -> &str {
    text.trim_end_matches(|c: char| c.is_ascii_whitespace())
}

fn ascii_trim_start(text: &str) -> &str {
    text.trim_start_matches(|c: char| c.is_ascii_whitespace())
}

/// One argument's element type as `format_type` prints it.
///
/// Total, and in three attempts, because two of the three rules the engine
/// applies do not need a catalogue at all.
///
/// 1. **With the modifier**, because `float(24)` is `real` and `float` is
///    `double precision`: which type the engine resolves depends on the
///    argument, so throwing it away before asking would answer for the wrong
///    one.
/// 2. **Without it**, for the spellings that carry a modifier *inside* the
///    name — issue #130's `timestamp(3) with time zone`.
/// 3. **Without it, and unfolded.** Discarding the modifier is what the engine
///    does to *every* routine argument, catalogued or not, so a type this
///    closed catalogue does not carry still loses it. Left in, a declared
///    `bit varying(4)` never equalled the `bit varying` the catalog reads back,
///    and the routine was one to create and one to drop on every plan for
///    ever — the cry-wolf loop ADR-0002 names as the failure to avoid, which
///    is a different and worse thing from DECISIONS 303's one loud mismatch.
fn identity_element(element: &str) -> String {
    // A built-in written with its schema is the built-in: **measured**,
    // `pg_catalog.int4`, `PG_CATALOG.INT4`, `"pg_catalog".int4` and
    // `pg_catalog."int4"` are all identified as `integer`, and
    // `pg_catalog.varbit` as `bit varying` — the qualifier is dropped before
    // the name is folded, so a routine declared with any of them is keyed on
    // the identity `format_type` writes.
    let element = as_the_engine_spells(element);
    let element = element.strip_prefix("pg_catalog.").unwrap_or(&element);
    if let Some(identity) = built_in(element) {
        return identity;
    }
    // The catalog's own name for a built-in's array type: **measured**,
    // `_int4`, `pg_catalog._int4`, `"_int4"`, `_varbit`, `_numeric(10,2)` and
    // `_bpchar` are identified as `integer[]`, `integer[]`, `integer[]`,
    // `bit varying[]`, `numeric[]` and `character[]`. Built-ins only: a user
    // type's array is spelled the same way (`ar._my_type` is `ar.my_type[]`)
    // but so is a user type that merely starts with an underscore (`ar._solo`
    // is `ar._solo`), and which of the two a name is cannot be decided
    // offline — the engine is the normalizer for what this table does not
    // know (303).
    if let Some(rest) = element.strip_prefix('_')
        && let Some(base) = built_in(rest)
    {
        return format!("{base}[]");
    }
    quoted_where_the_engine_quotes(&without_modifier(element))
}

/// The identity of a built-in spelling — the column catalogue's, the
/// modifier discarded, the field qualifier discarded, or an alias the
/// catalogue lacks — or `None` where the spelling is not a built-in.
fn built_in(element: &str) -> Option<String> {
    if let Some(folded) = folded(element) {
        return Some(folded.base);
    }
    let bare = without_modifier(element);
    if let Some(folded) = folded(&bare) {
        return Some(folded.base);
    }
    // The one type this engine's grammar follows with words rather than a
    // parenthesis. **Measured**, a parameter declared
    // `interval hour to minute` is identified as `interval`, so the field
    // qualifier goes the same way a modifier does.
    if let Some(rest) = bare.strip_prefix("interval")
        && rest.starts_with(|c: char| c.is_ascii_whitespace())
    {
        return Some("interval".to_owned());
    }
    ROUTINE_ALIASES
        .iter()
        .find(|(alias, _)| *alias == bare)
        .map(|(_, identity)| (*identity).to_owned())
}

/// An unquoted name spelled the way the engine spells it in an identity:
/// quoted where its `quote_identifier` would quote it.
///
/// The other half of [`as_the_engine_spells`]. **Measured**, `CREATE
/// FUNCTION f(a s.Ätype)` and `(v r8.a\u{a0}b)` are accepted unquoted and
/// identified as `f(s."Ätype")` and `f(r8."a\u{a0}b")` — the byte kept and
/// the name quoted. Kept bare, the key was one `module_oid` compared against
/// `format_type` and never matched, so the routine the plan had just created
/// was not in the catalog to the next plan (313). Only a part that is one
/// unquoted identifier is touched; a spelling with a modifier or a space in
/// it is not a name this rule reads.
fn quoted_where_the_engine_quotes(bare: &str) -> String {
    let mut out = String::with_capacity(bare.len() + 2);
    let mut rest = bare;
    while !rest.is_empty() {
        let len = if rest.starts_with('"') {
            quoted_len(rest).unwrap_or(rest.len())
        } else {
            rest.find('.').unwrap_or(rest.len())
        };
        let part = &rest[..len];
        // The character class alone, not the keyword table: a keyword
        // written bare is a built-in the grammar admits — `bit(3)` is `bit` —
        // never a user name, which the engine would not accept unquoted.
        if !part.starts_with('"')
            && !part.is_empty()
            && part.chars().all(pbps_dialect::continues_ident)
            && !part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            out.push('"');
            out.push_str(part);
            out.push('"');
        } else {
            out.push_str(part);
        }
        rest = &rest[len..];
        if let Some(after) = rest.strip_prefix('.') {
            out.push('.');
            rest = after;
        }
    }
    out
}

/// A quoted name spelled the way the engine spells it in an identity: bare
/// where its `quote_identifier` leaves it bare, quoted everywhere else.
///
/// **Measured**, `zq."my_type"`, `"zq"."my_type"` and `zq."zone"` are
/// identified as `zq.my_type` and `zq.zone`, while `zq."select"`, `zq."int"`,
/// `zq."user"`, `zq."Order"`, `zq."möney"`, `zq."a$b"` and `zq."my""q"` keep
/// their quotes: the engine writes a name bare only when it is
/// `[a-z_][a-z0-9_]*` and not a keyword the grammar reserves in some
/// position. A quoted spelling of a plain name is therefore a second spelling
/// of the bare one — and a Unicode-escaped name (DECISIONS 313) is decoded to
/// the quoted form by the model, so without this step `U&"\006dy_type"` was a
/// key for a routine the catalog spells `my_type`.
fn as_the_engine_spells(element: &str) -> String {
    let mut out = String::with_capacity(element.len());
    let mut rest = element;
    while !rest.is_empty() {
        if !rest.starts_with('"') {
            let len = rest.find('"').unwrap_or(rest.len());
            out.push_str(&rest[..len]);
            rest = &rest[len..];
            continue;
        }
        let Some(end) = quoted_len(rest) else {
            out.push_str(rest);
            break;
        };
        let inner = rest[1..end - 1].replace("\"\"", "\"");
        if bare_to_the_engine(&inner) {
            out.push_str(&inner);
        } else {
            out.push_str(&rest[..end]);
        }
        rest = &rest[end..];
    }
    out
}

/// The length of the `"…"` at the front of `text`, a doubled quote being a
/// quote inside the name; `None` where it never closes.
fn quoted_len(text: &str) -> Option<usize> {
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

/// Whether the engine writes this name without quotes: `quote_identifier`'s
/// rule, which is the character class above and the keyword table below.
fn bare_to_the_engine(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && QUOTED_KEYWORDS.binary_search(&name).is_err()
}

/// Whether a lower-cased word can never stand unquoted as a name — for the
/// name scans, which read a bare word as a possible reference (DECISIONS
/// 316).
///
/// The engine's reserved category, less the words that *can* be a bare name
/// somewhere: **measured** on 18.6, with a table, a function and a type of
/// each name, every word of `pg_get_keywords() WHERE catcode = 'R'` is
/// refused as `FROM word`, as `word(1)` and as `::word`, except
/// `current_catalog`, `current_date`, `current_role`, `current_time`,
/// `current_timestamp`, `current_user`, `localtime`, `localtimestamp`,
/// `session_user`, `system_user` and `user`, which `FROM word` accepts. The
/// type-or-function-name category is not reserved in this sense: `FROM
/// between` and `FROM join` are accepted too. And after a dot any word is a
/// name — `FROM app.select` is accepted — so this is asked only of the bare
/// form.
pub(crate) fn is_reserved(word: &str) -> bool {
    RESERVED.binary_search(&word).is_ok()
}

/// The words `is_reserved` names, sorted for the search.
const RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "grant",
    "group",
    "having",
    "in",
    "initially",
    "intersect",
    "into",
    "lateral",
    "leading",
    "limit",
    "not",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "placing",
    "primary",
    "references",
    "returning",
    "select",
    "some",
    "symmetric",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "using",
    "variadic",
    "when",
    "where",
    "window",
    "with",
];

/// Every keyword the engine quotes when it is used as a name: the reserved,
/// type-or-function-name and column-name categories, which are the ones
/// `quote_identifier` does not let stand bare. The unreserved category is
/// left out because the engine leaves it out — measured, `zq."zone"` is
/// `zq.zone`.
///
/// Read from the engine, not from memory, and kept sorted for the search:
///
/// ```sql
/// SELECT word FROM pg_get_keywords() WHERE catcode <> 'U' ORDER BY word
/// ```
///
/// PostgreSQL 18.6, 164 rows. A keyword a later engine adds is a name this
/// table lets stand bare, which the catalog assertion after the `CREATE`
/// (ADR-0009 §3) reports as a routine not found under its key — loud, not
/// silent.
const QUOTED_KEYWORDS: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "between",
    "bigint",
    "binary",
    "bit",
    "boolean",
    "both",
    "case",
    "cast",
    "char",
    "character",
    "check",
    "coalesce",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "dec",
    "decimal",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "exists",
    "extract",
    "false",
    "fetch",
    "float",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "greatest",
    "group",
    "grouping",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "inout",
    "int",
    "integer",
    "intersect",
    "interval",
    "into",
    "is",
    "isnull",
    "join",
    "json",
    "json_array",
    "json_arrayagg",
    "json_exists",
    "json_object",
    "json_objectagg",
    "json_query",
    "json_scalar",
    "json_serialize",
    "json_table",
    "json_value",
    "lateral",
    "leading",
    "least",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "merge_action",
    "national",
    "natural",
    "nchar",
    "none",
    "normalize",
    "not",
    "notnull",
    "null",
    "nullif",
    "numeric",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "out",
    "outer",
    "overlaps",
    "overlay",
    "placing",
    "position",
    "precision",
    "primary",
    "real",
    "references",
    "returning",
    "right",
    "row",
    "select",
    "session_user",
    "setof",
    "similar",
    "smallint",
    "some",
    "substring",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "time",
    "timestamp",
    "to",
    "trailing",
    "treat",
    "trim",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "values",
    "varchar",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
    "xmlattributes",
    "xmlconcat",
    "xmlelement",
    "xmlexists",
    "xmlforest",
    "xmlnamespaces",
    "xmlparse",
    "xmlpi",
    "xmlroot",
    "xmlserialize",
    "xmltable",
];

/// Spellings the engine accepts for a routine argument and identifies as
/// something else, that the column catalogue does not carry.
///
/// The catalogue is closed on purpose (ADR-0011): a column of `varbit` is a
/// column this model does not hold. A routine argument is a wider language
/// (DECISIONS 301), and a spelling the catalogue does not know is passed
/// through as written (303) — which is right for a domain, and wrong for an
/// alias: **measured**, `varbit(4)` is identified as `bit varying`, so a
/// declaration keyed `f(varbit)` named a routine the `CREATE` never made, and
/// `module_oid` resolved nothing. Every row was measured through
/// `format_type`; the live suite keys a routine with each and asks the catalog.
///
/// Pinned to the catalogue by `a_routine_alias_is_one_the_column_catalogue_lacks`:
/// a row the catalogue folds already is a second spelling of one rule.
const ROUTINE_ALIASES: &[(&str, &str)] = &[
    ("varbit", "bit varying"),
    ("bpchar", "character"),
    ("nchar", "character"),
    ("national character", "character"),
    ("national char", "character"),
    ("char varying", "character varying"),
    ("nchar varying", "character varying"),
    ("national character varying", "character varying"),
    ("national char varying", "character varying"),
];

fn folded(text: &str) -> Option<ColumnType> {
    normalize(&text.parse::<ColumnType>().ok()?).ok()
}

/// `timestamp(3) with time zone` -> `timestamp with time zone`.
///
/// The parentheses are found **outside quotes**: a type created as
/// `"odd(name)"` is one this engine will hand back with its parentheses
/// intact, and cutting there would leave `"odd` — an argument that no longer
/// balances and no longer names anything.
fn without_modifier(element: &str) -> String {
    let (Some(open), Some(close)) = (code_find(element, '('), code_rfind(element, ')')) else {
        return element.to_owned();
    };
    if close < open {
        return element.to_owned();
    }
    let mut out = ascii_trim_end(&element[..open]).to_owned();
    let rest = ascii_trim_start(&element[close + 1..]);
    if !rest.is_empty() {
        out.push(' ');
        out.push_str(rest);
    }
    out
}

/// The first `c` that is not inside a quoted name, and the last.
fn code_find(text: &str, c: char) -> Option<usize> {
    outside_quotes(text)
        .find(|(_, ch)| *ch == c)
        .map(|(i, _)| i)
}

fn code_rfind(text: &str, c: char) -> Option<usize> {
    outside_quotes(text)
        .filter(|(_, ch)| *ch == c)
        .last()
        .map(|(i, _)| i)
}

fn outside_quotes(text: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut quoted = false;
    text.char_indices().filter(move |(_, ch)| {
        if *ch == '"' {
            // A doubled quote reads as two openers here, which lands on the
            // same state as one closer and one opener: still inside the name.
            quoted = !quoted;
            return false;
        }
        !quoted
    })
}

#[cfg(test)]
mod tests {

    fn arg(s: &str) -> String {
        let declared: pbps_model::RoutineArg = s.parse().expect("a routine argument parses");
        routine_arg(&declared).as_str().to_owned()
    }

    /// A name over the engine's byte limit is found wherever it is in the
    /// argument — bare, qualified, quoted with a doubled quote counted once,
    /// under a modifier or an array — and a name at the limit is not.
    #[test]
    fn a_name_over_the_byte_limit_is_found_wherever_the_argument_holds_it() {
        let at = "t".repeat(crate::MAX_IDENT_BYTES);
        let over = format!("{at}x");
        for spelled in [
            over.clone(),
            format!("app.{over}"),
            format!("{over}.t"),
            format!("app.\"{over}\""),
            format!("{over}(10)"),
            format!("app.{over}[]"),
            // Sixty-two bytes and a doubled quote, which is one byte to the
            // engine: at the limit. With `ä` in it the count is bytes.
            format!("\"{}\"\"x\"", "t".repeat(62)),
            format!("app.{}", "ä".repeat(32)),
        ] {
            let arg: RoutineArg = spelled.parse().expect("an argument");
            assert!(
                overlong_name(&arg).is_some(),
                "`{spelled}` holds a name over the limit"
            );
        }
        for spelled in [
            at.clone(),
            format!("app.{at}"),
            format!("app.\"{at}\""),
            format!("\"{}\"\"\"", "t".repeat(62)),
            format!("app.{}", "ä".repeat(31)),
            "character varying(10)".to_owned(),
            "timestamp with time zone[]".to_owned(),
        ] {
            let arg: RoutineArg = spelled.parse().expect("an argument");
            assert_eq!(overlong_name(&arg), None, "`{spelled}` is within the limit");
        }
    }

    /// Measured: a bare `select` is refused in every position and `user` in
    /// none; `zone` and `int` are keywords the engine lets stand as names.
    #[test]
    fn a_reserved_word_is_one_no_bare_position_takes() {
        for word in ["select", "from", "table", "with", "array", "false"] {
            assert!(is_reserved(word), "{word}");
        }
        for word in [
            "user",
            "current_date",
            "between",
            "join",
            "zone",
            "int",
            "customer",
        ] {
            assert!(!is_reserved(word), "{word}");
        }
        assert!(
            RESERVED.windows(2).all(|w| w[0] < w[1]),
            "sorted, for the search"
        );
    }

    /// Measured on 18.6: one function's twelve parameters, declared one way
    /// and identified another. The left column is what was written, the right
    /// is what `oid::regprocedure` printed back under the empty search path.
    #[test]
    fn a_declared_argument_folds_to_the_spelling_the_identity_carries() {
        for (declared, identity) in [
            // A modifier is discarded: `f(varchar(10))` and `f(varchar(20))`
            // are one function.
            ("varchar(10)", "character varying"),
            ("numeric(10,2)", "numeric"),
            ("char", "character"),
            ("timestamp(3) with time zone", "timestamp with time zone"),
            // Aliases fold through the same table a column uses.
            ("int4", "integer"),
            ("int", "integer"),
            ("timestamptz", "timestamp with time zone"),
            ("bool", "boolean"),
            // A dimension is not part of the identity, and neither is the
            // number of them.
            ("double precision[]", "double precision[]"),
            ("text[][]", "text[]"),
            ("int[3]", "integer[]"),
            // The standard's spelling, which the engine identifies the same
            // way — measured, with and without a dimension and with the space
            // the grammar allows before the bracket.
            ("text ARRAY", "text[]"),
            ("text ARRAY[4]", "text[]"),
            ("int ARRAY [2]", "integer[]"),
            ("character varying array", "character varying[]"),
            // `float(24)` is `real` and `float` is `double precision`, which is
            // why the modifier is not thrown away before the catalogue is
            // asked.
            ("float(24)", "real"),
            ("float", "double precision"),
            // A modifier is discarded whatever the type is, and these are not
            // in the column catalogue: left on, a declared `bit varying(4)`
            // never equals the `bit varying` the catalog reads back, and the
            // routine is one to create and one to drop on every plan for ever.
            ("bit varying(4)", "bit varying"),
            ("bit(3)", "bit"),
            ("m2.money(2)", "m2.money"),
            // Aliases the engine identifies as something else and the column
            // catalogue does not carry: measured through `format_type`.
            ("varbit", "bit varying"),
            ("varbit(4)", "bit varying"),
            // A built-in written with its schema is the built-in, measured.
            ("pg_catalog.int4", "integer"),
            ("pg_catalog.text", "text"),
            ("pg_catalog.varbit", "bit varying"),
            ("pg_catalog.name", "name"),
            ("\"pg_catalog\".int4", "integer"),
            ("pg_catalog.\"int4\"", "integer"),
            ("pg_catalog.timestamptz(3)", "timestamp with time zone"),
            // A quoted name is bare where the engine's `quote_identifier`
            // leaves it bare, and quoted where it does not — measured.
            ("zq.\"my_type\"", "zq.my_type"),
            ("\"zq\".\"my_type\"", "zq.my_type"),
            ("zq.\"zone\"", "zq.zone"),
            ("zq.\"my_type\"[]", "zq.my_type[]"),
            ("\"int4\"", "integer"),
            ("zq.\"select\"", "zq.\"select\""),
            ("zq.\"int\"", "zq.\"int\""),
            ("zq.\"user\"", "zq.\"user\""),
            ("zq.\"Order\"", "zq.\"Order\""),
            ("zq.\"möney\"", "zq.\"möney\""),
            ("zq.\"a$b\"", "zq.\"a$b\""),
            // And an unquoted one: measured, `dl.money$type` is identified as
            // `dl."money$type"`.
            ("dl.money$type", "dl.\"money$type\""),
            ("zq.\"my\"\"q\"", "zq.\"my\"\"q\""),
            ("zq.\"1a\"", "zq.\"1a\""),
            ("zq.\"a b\"", "zq.\"a b\""),
            // And an unquoted name is quoted where the engine quotes it —
            // measured, `s.Ätype` and `r8.a\u{a0}b` are identified as
            // `s."Ätype"` and `r8."a\u{a0}b"`; a plain one stays bare.
            ("s.Ätype", "s.\"Ätype\""),
            ("Ätype", "\"Ätype\""),
            ("r8.a\u{a0}b", "r8.\"a\u{a0}b\""),
            ("r8.x\u{a0}", "r8.\"x\u{a0}\""),
            ("s.my_type", "s.my_type"),
            ("S.MyType", "s.mytype"),
            ("s.Ätype[]", "s.\"Ätype\"[]"),
            // The catalog's own name for a built-in's array type, measured.
            ("_int4", "integer[]"),
            ("pg_catalog._int4", "integer[]"),
            ("\"_int4\"", "integer[]"),
            ("_INT4", "integer[]"),
            ("_varbit", "bit varying[]"),
            ("_text", "text[]"),
            ("_numeric(10,2)", "numeric[]"),
            ("_bpchar", "character[]"),
            ("bpchar(3)", "character"),
            ("nchar(2)", "character"),
            ("national character varying(5)", "character varying"),
            ("char varying(5)", "character varying"),
            // The one type this grammar follows with words rather than a
            // parenthesis. Measured: `interval hour to minute` is identified
            // as `interval`.
            ("interval hour to minute", "interval"),
            ("interval second(3)", "interval"),
        ] {
            assert_eq!(arg(declared), identity, "{declared}");
        }
    }

    /// A row the column catalogue folds already is one rule spelled twice,
    /// and the second spelling is the one that drifts.
    #[test]
    fn a_routine_alias_is_one_the_column_catalogue_lacks() {
        for (alias, identity) in ROUTINE_ALIASES {
            assert!(
                folded(alias).is_none(),
                "`{alias}` is in the column catalogue already"
            );
            assert_eq!(arg(alias), *identity, "{alias}");
            // And the identity is the engine's spelling, not another alias.
            assert_eq!(arg(identity), *identity, "{identity}");
        }
    }

    /// The engine is the normalizer, so a spelling this catalogue has never
    /// heard of goes back unchanged rather than being refused: refusing would
    /// refuse ADR-0009 §1's own example, and the model cannot tell a domain
    /// from a mistake. A disagreement is caught by the engine, loudly, inside
    /// the plan's transaction.
    #[test]
    fn a_spelling_the_catalogue_does_not_know_is_returned_unchanged() {
        for text in [
            // A quoted built-in whose case the engine keeps.
            "\"char\"",
            // A quoted name with parentheses of its own, which are part of the
            // name and not a modifier.
            "\"odd(name)\"",
            "m2.\"odd(name)\"",
            // A domain, an enum, a composite: qualified, because that is what
            // `format_type` prints under the empty search path.
            "m2.money_amount",
            "m2.mood",
            "m2.money_amount[]",
            // A name that merely ends in the array keyword's letters, and a
            // quoted name with the word inside it: neither is an array.
            "m2.myarray",
            "m2.\"my array\"",
            // A pseudo-type, which no column may ever be.
            "anyelement",
            "record",
            // A user type whose name starts with an underscore is left as
            // written: measured, `ar._my_type` is `ar.my_type[]` and
            // `ar._solo` is `ar._solo`, and only the engine can tell which.
            "ar._my_type",
            "ar._solo",
            // Types this catalogue does not carry, spelled as the engine
            // spells them.
            "bit varying",
            "inet",
            "tsvector",
            "interval",
        ] {
            assert_eq!(arg(text), text, "{text}");
        }
        // A non-breaking space is a name byte, not the gap before the
        // keyword, a field qualifier or a modifier — and a name holding one
        // is quoted, as the engine quotes it.
        for (text, spelled) in [
            ("m2.a\u{a0}array", "m2.\"a\u{a0}array\""),
            ("m2.x\u{a0}", "m2.\"x\u{a0}\""),
            ("interval\u{a0}hour", "\"interval\u{a0}hour\""),
        ] {
            assert_eq!(arg(text), spelled, "{text}");
        }
    }

    /// Idempotent, because the identity read back out of the catalog is fed
    /// through the same fold as the declared one — a normalizer that moved on
    /// the second pass would report drift on an unchanged routine.
    #[test]
    fn folding_an_argument_twice_says_what_folding_it_once_says() {
        for text in [
            "varchar(10)",
            "int4",
            "text[][]",
            "\"char\"",
            "m2.money_amount",
            "timestamp(3) with time zone",
            "bit varying(4)",
            "interval hour to minute",
            "\"odd(name)\"",
        ] {
            assert_eq!(arg(&arg(text)), arg(text), "{text}");
        }
    }

    /// `serial` is not a type (ADR-0011 Amendment 3), so the catalogue refuses
    /// it and the text goes back untouched rather than becoming `integer`:
    /// a routine argument spelled that way is one the engine will refuse, and
    /// silently rewriting it would key the declaration as a routine that is
    /// not the one the `CREATE` would make.
    #[test]
    fn a_spelling_that_is_not_a_type_is_not_quietly_made_into_one() {
        assert_eq!(arg("serial"), "serial");
        assert_eq!(arg("bigserial"), "bigserial");
    }
    use super::*;

    fn ty(s: &str) -> ColumnType {
        s.parse().expect("a type parses")
    }

    fn norm(s: &str) -> String {
        normalize(&ty(s))
            .unwrap_or_else(|e| panic!("`{s}` should normalize: {e}"))
            .to_string()
    }

    /// DECISIONS 394: a float's boundary is tested as a float.
    ///
    /// The two predicates a float source reaches must not mention `numeric` at
    /// all. `float8::numeric` rounds through the shortest decimal, and both of
    /// these sit exactly where that rounding is largest, so a `numeric` in
    /// either is the bug itself rather than a detail of it. Asserted on the
    /// text because the failure is invisible in the answer until a row sits on
    /// the boundary, and the live suite is what puts one there.
    #[test]
    fn a_float_source_is_measured_without_a_numeric_in_sight() {
        let predicate = |from: &str, to: &str| {
            let from = normalize(&ty(from)).expect("a source type normalizes");
            let to = normalize(&ty(to)).expect("a target type normalizes");
            cannot_become(&from, &to, "\"v\"")
                .unwrap_or_else(|| panic!("`{from}` -> `{to}` should have a predicate to count"))
        };
        for (from, to) in [
            ("double precision", "bigint"),
            ("double precision", "integer"),
            ("double precision", "smallint"),
            ("real", "integer"),
            ("double precision", "real"),
        ] {
            let got = predicate(from, to);
            assert!(
                !got.contains("numeric"),
                "`{from}` -> `{to}` measures a float through `numeric`: {got}"
            );
            assert!(got.contains("float8"), "{got}");
        }

        // And the other half: an exact source keeps the exact domain, because
        // that is the domain the `ALTER` itself converts in.
        for (from, to) in [
            ("numeric(30,0)", "bigint"),
            ("numeric", "real"),
            ("bigint", "real"),
        ] {
            let got = predicate(from, to);
            assert!(
                got.contains("numeric"),
                "`{from}` -> `{to}` is exact and must stay exact: {got}"
            );
        }
    }

    /// A length probe is taken only over a rendering every session prints
    /// alike (DECISIONS 389).
    ///
    /// The probe runs before the deployment pins its settings and the `ALTER`
    /// runs after, so a source whose `::text` moves with `bytea_output`,
    /// `IntervalStyle`, `DateStyle`, `TimeZone` or `extra_float_digits` would
    /// be measured under one rendering and converted under another — and the
    /// unpinned one is the longer, so the count refuses a plan this engine
    /// accepts. Both halves are asserted: the sources that keep their probe
    /// matter as much as the ones that lose it, or the gate could be a rule
    /// that switched every probe off.
    #[test]
    fn a_length_probe_is_taken_only_over_a_rendering_no_setting_moves() {
        let probe = |from: &str| {
            let from = normalize(&ty(from)).expect("a source type normalizes");
            let to = normalize(&ty("character varying(6)")).expect("a target type normalizes");
            cannot_become(&from, &to, "\"v\"")
        };
        for from in [
            "integer",
            "bigint",
            "numeric(12,2)",
            "boolean",
            "uuid",
            "text",
            "character(9)",
            "json",
            "jsonb",
        ] {
            let got = probe(from);
            assert!(
                got.as_deref().is_some_and(|p| p.contains("length(rtrim")),
                "`{from}` renders the same in every session, so it keeps its probe: {got:?}"
            );
        }
        for from in [
            "bytea",
            "interval",
            "date",
            "timestamp without time zone",
            "timestamp with time zone",
            "time without time zone",
            "real",
            "double precision",
        ] {
            assert_eq!(
                probe(from),
                None,
                "`{from}` renders differently under a setting the framing pins, \
                 so no length may be measured for it"
            );
        }
    }

    /// A qualified or quoted name is the type only under the spelling the
    /// catalog has; a grammar word, another schema, or a quote inside the
    /// name is not (DECISIONS 364).
    #[test]
    fn a_qualified_or_quoted_name_is_the_type_only_as_the_catalog_spells_it() {
        for (spelled, grammar) in [
            ("text", "text"),
            ("double precision", "double precision"),
            ("numeric(5, 2)", "numeric(5, 2)"),
            ("pg_catalog.text", "text"),
            ("PG_CATALOG.INT4", "int4"),
            ("\"pg_catalog\".\"text\"", "text"),
            ("\"pg_catalog\" . \"text\"", "text"),
            ("pg_catalog . varchar", "varchar"),
            ("\"bool\"", "bool"),
            ("\"numeric\"(5,2)", "numeric(5,2)"),
            ("pg_catalog.numeric (5,2)", "numeric(5,2)"),
            ("\"timestamptz\"", "timestamptz"),
        ] {
            assert_eq!(
                as_the_grammar_spells(spelled).as_deref(),
                Some(grammar),
                "{spelled}"
            );
        }
        for spelled in [
            "pg_catalog.integer",
            "\"integer\"",
            "\"TEXT\"",
            "\"Pg_Catalog\".text",
            "app.text",
            "public.text",
            "\"char\"",
            "pg_catalog.bpchar",
            "\"te\"\"xt\"",
            "\"unclosed",
            "pg_catalog.",
            "pg_catalog.text.more",
            "pg_catalog.text more",
            ".text",
            "\"double precision\"",
        ] {
            assert_eq!(as_the_grammar_spells(spelled), None, "{spelled}");
        }
    }

    fn refused(s: &str) -> String {
        normalize(&ty(s))
            .expect_err(&format!("`{s}` should be refused"))
            .to_string()
    }

    /// Every row measured on PostgreSQL 18.6 by declaring a column and reading
    /// `format_type(atttypid, atttypmod)` back. The live suite runs the same
    /// list against a real server; this one holds the answers so that a change
    /// to the table fails offline too.
    const ROUND_TRIP: &[(&str, &str)] = &[
        ("int", "integer"),
        ("int4", "integer"),
        ("integer", "integer"),
        ("int2", "smallint"),
        ("smallint", "smallint"),
        ("int8", "bigint"),
        ("bigint", "bigint"),
        ("decimal(10,2)", "numeric(10, 2)"),
        ("dec(10,2)", "numeric(10, 2)"),
        ("numeric(10,2)", "numeric(10, 2)"),
        ("numeric(10)", "numeric(10, 0)"),
        ("numeric", "numeric"),
        ("decimal", "numeric"),
        ("real", "real"),
        ("float4", "real"),
        ("float", "double precision"),
        ("float8", "double precision"),
        ("double precision", "double precision"),
        ("float(1)", "real"),
        ("float(24)", "real"),
        ("float(25)", "double precision"),
        ("float(53)", "double precision"),
        ("bool", "boolean"),
        ("boolean", "boolean"),
        ("char(5)", "character(5)"),
        ("character(5)", "character(5)"),
        ("char", "character(1)"),
        ("character", "character(1)"),
        ("varchar", "character varying"),
        ("varchar(9)", "character varying(9)"),
        ("character varying", "character varying"),
        ("character varying(9)", "character varying(9)"),
        ("text", "text"),
        ("date", "date"),
        ("bytea", "bytea"),
        ("time", "time without time zone"),
        ("timetz", "time with time zone"),
        ("time with time zone", "time with time zone"),
        ("timestamp", "timestamp without time zone"),
        ("timestamptz", "timestamp with time zone"),
        ("timestamp with time zone", "timestamp with time zone"),
        ("timestamp without time zone", "timestamp without time zone"),
        ("interval", "interval"),
        ("interval(3)", "interval(3)"),
        ("json", "json"),
        ("jsonb", "jsonb"),
        ("uuid", "uuid"),
    ];

    /// The catalogue's whole purpose: a declaration and the column the engine
    /// makes from it converge on one value. Without this every run reports a
    /// type change that is not there.
    #[test]
    fn every_spelling_normalizes_to_the_one_the_engine_reads_back() {
        for (declared, canonical) in ROUND_TRIP {
            assert_eq!(&norm(declared), canonical, "declared `{declared}`");
        }
    }

    /// Half of ADR-0011 Amendment 3's contract, and the half a table of pairs
    /// cannot show: normalizing what came out has to give the same thing back,
    /// or a schema read from the database would differ from itself.
    #[test]
    fn normalization_is_idempotent() {
        for (_, canonical) in ROUND_TRIP {
            assert_eq!(&norm(canonical), canonical);
        }
    }

    /// The catalogue is closed, and that is the point. A name it does not hold
    /// is refused rather than passed through: passed through, it would be a
    /// base name no catalog ever returns, so the column would read as changed
    /// on every run and no plan could ever fix it (ADR-0012 §1).
    #[test]
    fn a_name_the_catalogue_does_not_hold_is_refused_and_never_passed_through() {
        // `bpchar` is the trap in this group and the reason it is refused
        // rather than aliased to `character`: measured, `bpchar(5)` reads back
        // as `character(5)` but a bare `bpchar` reads back as `bpchar`, so one
        // alias would be right and the other wrong.
        for spelling in [
            "bpchar", "xml", "money", "inet", "cidr", "tsvector", "widget",
        ] {
            assert!(
                refused(spelling).contains("has no type"),
                "`{spelling}` is not in the catalogue and must say so"
            );
        }
    }

    /// Both array spellings fail, which is the whole requirement: ADR-0012 §1
    /// measured that `text[]` is refused by the loader and `text ARRAY` is
    /// **not**, because spaces are legal in a base name. The silently accepted
    /// one is the dangerous one.
    #[test]
    fn neither_array_spelling_is_accepted() {
        // The bracketed form never reaches the dialect: the model refuses it.
        assert!("text[]".parse::<ColumnType>().is_err());
        assert!("integer[]".parse::<ColumnType>().is_err());
        // The standard form parses, and this is where it stops.
        for spelling in ["text ARRAY", "integer array", "double precision array"] {
            let message = refused(spelling);
            assert!(message.contains("array"), "{message}");
            assert!(message.contains("dimension"), "{message}");
        }
        // A name that merely ends in those letters is not an array.
        assert!(refused("arrayish").contains("has no type"));
    }

    /// The modifier the engine spells inside the name. Measured:
    /// `timestamptz(3)` reads back as `timestamp(3) with time zone`, and
    /// `timestamp with time zone(3)` is a syntax error — so there is no value
    /// this normalizer could return that satisfies the contract.
    #[test]
    fn a_precision_on_a_time_type_is_refused_rather_than_spelled_wrongly() {
        for spelling in [
            "time(3)",
            "timetz(3)",
            "timestamp(3)",
            "timestamptz(3)",
            "timestamp without time zone(3)",
        ] {
            let message = refused(spelling);
            assert!(message.contains("does not implement"), "{message}");
            assert!(message.contains("inside"), "{message}");
        }
        // The engine's own spelling of the same thing does not even parse, so
        // both ways of writing it fail — neither is silently accepted.
        assert!("timestamp(3) with time zone".parse::<ColumnType>().is_err());
        // Without a precision they are ordinary types.
        assert_eq!(norm("timestamptz"), "timestamp with time zone");
    }

    /// The two string types disagree about the omitted length, and guessing
    /// one rule for both would declare a different column from the one the
    /// engine makes.
    #[test]
    fn an_omitted_length_is_filled_in_for_character_and_not_for_character_varying() {
        assert_eq!(norm("char"), "character(1)");
        assert_eq!(norm("varchar"), "character varying");
        // And `numeric` is a third answer again: unbounded with no argument,
        // scale zero with one.
        assert_eq!(norm("numeric"), "numeric");
        assert_eq!(norm("numeric(10)"), "numeric(10, 0)");
    }

    /// An argument the engine would **silently change** is refused here. That
    /// is the identifier-truncation shape: measured, `interval(7)` is stored as
    /// `interval(6)` with no error at all, so a declaration that kept it would
    /// read back as something else on every run.
    #[test]
    fn an_argument_the_engine_would_silently_change_is_refused() {
        let message = refused("interval(7)");
        assert!(message.contains("between 0 and 6"), "{message}");
        assert!(message.contains("does not refuse"), "{message}");
        assert_eq!(norm("interval(6)"), "interval(6)");
        assert_eq!(norm("interval(0)"), "interval(0)");
    }

    /// The bounds the engine itself refuses, refused here too — so that a
    /// declaration fails at `validate` rather than half way through an apply.
    #[test]
    fn an_argument_outside_the_engines_range_is_refused_before_the_apply() {
        for (spelling, needle) in [
            ("varchar(0)", "between 1 and 10485760"),
            ("varchar(10485761)", "between 1 and 10485760"),
            ("char(0)", "between 1 and 10485760"),
            ("numeric(0)", "between 1 and 1000"),
            ("numeric(1001)", "between 1 and 1000"),
            ("numeric(10,1001)", "between -1000 and 1000"),
            ("float(0)", "between 1 and 53"),
            ("float(54)", "between 1 and 53"),
            ("integer(5)", "takes no arguments"),
            ("text(5)", "takes no arguments"),
        ] {
            let message = refused(spelling);
            assert!(message.contains(needle), "`{spelling}`: {message}");
        }
    }

    /// A negative scale is legal on this engine, and copying SQL Server's
    /// `0 <= scale <= precision` would refuse a column it will happily make.
    /// Measured: `numeric(10,-5)` reads back as itself.
    #[test]
    fn a_negative_numeric_scale_is_accepted_because_the_engine_accepts_it() {
        assert_eq!(norm("numeric(10,-5)"), "numeric(10, -5)");
        assert_eq!(norm("numeric(10,20)"), "numeric(10, 20)");
    }

    /// `max` is SQL Server's word for unbounded. Here the unbounded form is
    /// the one with no argument at all, and accepting the other spelling would
    /// let a schema written for one engine load against the other.
    #[test]
    fn sql_servers_max_is_not_a_length_here() {
        let message = refused("varchar(max)");
        assert!(message.contains("one length"), "{message}");
    }

    /// The `serial` family is refused by name, and the refusal says what to
    /// write instead. Normalizing it to anything at all would produce a schema
    /// that differs from itself on every run (ADR-0011 Amendment 3).
    #[test]
    fn the_serial_family_is_refused_and_never_normalized() {
        for (declared, reads_back) in [
            ("serial", "integer"),
            ("serial4", "integer"),
            ("smallserial", "smallint"),
            ("serial2", "smallint"),
            ("bigserial", "bigint"),
            ("serial8", "bigint"),
        ] {
            let message = refused(declared);
            assert!(message.contains("macro, not a type"), "{message}");
            assert!(message.contains(reads_back), "{message}");
        }
        // A name that merely begins with the same letters is the catalogue's
        // business, and the catalogue does not hold it either.
        assert!(refused("serialized").contains("has no type"));
    }

    // -----------------------------------------------------------------------
    // Risk
    // -----------------------------------------------------------------------

    fn risk(from: &str, to: &str) -> TypeChangeRisk {
        change_risk(
            &normalize(&ty(from)).expect("from normalizes"),
            &normalize(&ty(to)).expect("to normalizes"),
        )
    }

    /// The twenty types of the measured matrix, in the order its rows are
    /// written below.
    const MATRIX_TYPES: &[&str] = &[
        "smallint",
        "integer",
        "bigint",
        "numeric(10,2)",
        "real",
        "double precision",
        "boolean",
        "character(5)",
        "character varying(9)",
        "text",
        "bytea",
        "date",
        "time without time zone",
        "time with time zone",
        "timestamp without time zone",
        "timestamp with time zone",
        "interval",
        "uuid",
        "json",
        "jsonb",
    ];

    /// What PostgreSQL 18.6 does with `ALTER TABLE ... ALTER COLUMN c TYPE ...`
    /// on an **empty** table: `Y` where the statement is accepted and `.` where
    /// it is refused. Empty on purpose — with no rows the only thing that can
    /// fail is the conversion itself, which is exactly the question
    /// `Incompatible` answers.
    const MATRIX: &[&str] = &[
        "YYYYYY.YYY..........",
        "YYYYYY.YYY..........",
        "YYYYYY.YYY..........",
        "YYYYYY.YYY..........",
        "YYYYYY.YYY..........",
        "YYYYYY.YYY..........",
        "......YYYY..........",
        ".......YYY..........",
        ".......YYY..........",
        ".......YYY..........",
        ".......YYYY.........",
        ".......YYY.Y..YY....",
        ".......YYY..YY..Y...",
        ".......YYY..YY......",
        ".......YYY.YY.YY....",
        ".......YYY.YYYYY....",
        ".......YYY..Y...Y...",
        ".......YYY.......Y..",
        ".......YYY........YY",
        ".......YYY........YY",
    ];

    /// `Incompatible` means *the engine refuses the conversion*, and it has to
    /// mean exactly that: a pair called incompatible that the engine accepts
    /// blocks a plan that would have worked, and a pair called narrowing that
    /// the engine refuses fails half way through an apply, after the changes
    /// before it have run.
    ///
    /// The live suite re-measures this matrix against a real server, so this
    /// test and that one disagree the moment either the engine or this file
    /// changes.
    #[test]
    fn incompatible_is_exactly_what_the_engine_refuses() {
        for (i, from) in MATRIX_TYPES.iter().enumerate() {
            let row: Vec<char> = MATRIX[i].chars().collect();
            assert_eq!(
                row.len(),
                MATRIX_TYPES.len(),
                "row `{from}` is the wrong width"
            );
            // Every type converts to itself, so the diagonal is the one cell
            // whose value is known without measuring — and a row transcribed
            // one column out of place is caught here rather than by whichever
            // assertion below happens to notice.
            assert_eq!(row[i], 'Y', "the diagonal of row `{from}`");
            for (j, to) in MATRIX_TYPES.iter().enumerate() {
                let engine_accepts = row[j] == 'Y';
                let judged = risk(from, to);
                assert_eq!(
                    judged != TypeChangeRisk::Incompatible,
                    engine_accepts,
                    "`{from}` -> `{to}` judged {judged:?}, engine accepts: {engine_accepts}"
                );
            }
        }
    }

    /// Widening inside one family is the case the gate must not stop.
    #[test]
    fn a_widening_within_a_family_is_safe() {
        for (from, to) in [
            // A `time` is a length of time, and an `interval` that keeps every
            // microsecond keeps it: measured, `00:00:00`, `24:00:00` and
            // `23:59:59.999999` all read back unchanged. A bare `interval` is
            // the full precision, so this is the common spelling.
            ("time", "interval"),
            ("time", "interval(6)"),
            ("smallint", "integer"),
            ("integer", "bigint"),
            ("integer", "numeric(20,0)"),
            ("integer", "numeric"),
            ("smallint", "real"),
            ("integer", "double precision"),
            ("real", "double precision"),
            ("varchar(10)", "varchar(20)"),
            ("varchar(20)", "text"),
            ("char(5)", "varchar(9)"),
            ("numeric(10,2)", "numeric(12,2)"),
            ("jsonb", "json"),
        ] {
            assert_eq!(risk(from, to), TypeChangeRisk::Safe, "`{from}` -> `{to}`");
        }
    }

    /// And the mirror, which is the case it must stop. Every one of these is
    /// accepted by the engine, so nothing but this classification stands
    /// between it and the data.
    #[test]
    fn a_change_that_can_lose_is_narrowing_even_where_the_engine_accepts_it() {
        for (from, to) in [
            // Measured: `100000` into a `smallint` is `smallint out of range`.
            ("integer", "smallint"),
            ("bigint", "integer"),
            ("numeric", "bigint"),
            // Measured: `1.5` into an `integer` stores `2`.
            ("double precision", "integer"),
            ("double precision", "real"),
            ("numeric(10,2)", "numeric(10,1)"),
            // Scale grows and the integer half shrinks with it. Measured:
            // `12345678.90` into `numeric(10,4)` is `numeric field overflow`,
            // though ADR-0012 §3 lists the same change as one that rewrites —
            // the two axes are not the same question.
            ("numeric(10,2)", "numeric(10,4)"),
            ("varchar(20)", "varchar(10)"),
            ("text", "varchar(20)"),
            // Blank-padding is not recoverable: a trailing space a user stored
            // and one the engine added are the same byte.
            ("varchar(20)", "char(20)"),
            // Measured: `1234567890` into `character varying(9)` is `value too
            // long`, so a number into a bounded string can fail outright.
            ("integer", "varchar(9)"),
            ("integer", "text"),
            ("bytea", "text"),
            ("uuid", "text"),
            ("timestamp", "date"),
            ("timestamp", "time"),
            // Measured: `interval '30 hours'` into `time` stores `06:00:00`.
            ("interval", "time"),
            // The other direction keeps the length; what it can lose is the
            // last places of the seconds. Measured, `12:34:56.654321` into
            // `interval(0)` rounds to `12:34:57`.
            ("time", "interval(0)"),
            ("time", "interval(5)"),
            // Measured: `{"a": 1,  "a": 2}` into `jsonb` becomes `{"a": 2}`.
            ("json", "jsonb"),
        ] {
            assert_eq!(
                risk(from, to),
                TypeChangeRisk::Narrowing,
                "`{from}` -> `{to}`"
            );
        }
    }

    /// A digit count is not a magnitude, and the integer types are not powers
    /// of ten. `smallint` reaches 32767, which needs **five** digits and not
    /// four, and `numeric(4,0)` cannot hold it.
    #[test]
    fn an_exact_type_is_measured_by_what_it_holds_and_not_by_its_digits() {
        for (from, to) in [
            ("smallint", "numeric(5,0)"),
            ("integer", "numeric(10,0)"),
            ("bigint", "numeric(19,0)"),
            ("integer", "numeric"),
            ("smallint", "integer"),
            ("integer", "bigint"),
        ] {
            assert_eq!(risk(from, to), TypeChangeRisk::Safe, "`{from}` -> `{to}`");
        }
        // One digit narrower and the largest value no longer fits.
        for (from, to) in [
            ("smallint", "numeric(4,0)"),
            ("integer", "numeric(9,0)"),
            ("bigint", "numeric(18,0)"),
            ("integer", "smallint"),
            ("bigint", "integer"),
            // A scale below zero rounds the integer away.
            ("smallint", "numeric(10,-2)"),
        ] {
            assert_eq!(
                risk(from, to),
                TypeChangeRisk::Narrowing,
                "`{from}` -> `{to}`"
            );
        }
    }

    /// A `numeric` never reaches an integer type safely, however narrow it is.
    /// Measured: `'NaN'::numeric(4,0)` is accepted — the precision does not
    /// exclude it — and the change is then `cannot convert NaN to smallint`, on
    /// a row nobody looked at, after the changes before it have run.
    #[test]
    fn a_numeric_never_reaches_an_integer_type_safely() {
        for (from, to) in [
            ("numeric(1,0)", "bigint"),
            ("numeric(4,0)", "smallint"),
            ("numeric(9,0)", "integer"),
            ("numeric(18,0)", "bigint"),
            ("numeric", "bigint"),
        ] {
            assert_eq!(
                risk(from, to),
                TypeChangeRisk::Narrowing,
                "`{from}` -> `{to}`"
            );
        }
    }

    /// A scale larger than the precision leaves no integer part, and the two
    /// types are still different: `numeric(2,3)` holds values below `0.1` and
    /// `numeric(2,4)` below `0.01`. Measured, `0.099` into the second is
    /// `numeric field overflow`, so an integer part clamped at "none" would
    /// have called that change safe.
    #[test]
    fn a_scale_larger_than_the_precision_still_bounds_the_value() {
        assert_eq!(
            risk("numeric(2,3)", "numeric(2,4)"),
            TypeChangeRisk::Narrowing
        );
        // Measured: `0.0099` the other way becomes `0.010`.
        assert_eq!(
            risk("numeric(2,4)", "numeric(2,3)"),
            TypeChangeRisk::Narrowing
        );
        // A wider precision at the same scale holds everything the narrower
        // one did.
        assert_eq!(risk("numeric(2,3)", "numeric(3,3)"), TypeChangeRisk::Safe);
    }

    /// An exact value survives a binary float only if the float holds it
    /// exactly. Measured: `0.1` in a `real` is `0.10000000149011612`, and ten
    /// of them sum to `1.0000001` where the exact sum is `1.0`. The engine's
    /// own printing rounds a single value back to `0.1` and hides it, which is
    /// why "does it round-trip through `::text`" is the wrong question.
    #[test]
    fn an_exact_type_reaches_a_float_safely_only_when_the_float_holds_it_exactly() {
        for (from, to) in [
            // A fraction is not a binary fraction.
            ("numeric(2,1)", "real"),
            ("numeric(2,1)", "double precision"),
            ("numeric(10,2)", "double precision"),
            // Measured: `16777217` into a `real` reads back as `16777200`, and
            // `9007199254740993` into a `double precision` as
            // `9007199254740990`.
            ("integer", "real"),
            ("bigint", "double precision"),
        ] {
            assert_eq!(
                risk(from, to),
                TypeChangeRisk::Narrowing,
                "`{from}` -> `{to}`"
            );
        }
        for (from, to) in [
            ("smallint", "real"),
            ("numeric(5,0)", "real"),
            ("integer", "double precision"),
            ("smallint", "double precision"),
            ("real", "double precision"),
        ] {
            assert_eq!(risk(from, to), TypeChangeRisk::Safe, "`{from}` -> `{to}`");
        }
    }

    /// A family that drops an argument answers the same for two types the
    /// engine tells apart. `interval` carries a seconds precision, and dropping
    /// it sent `interval(0) -> interval(6)` — a widening the engine performs
    /// without blinking — through to `Incompatible`, refusing a valid plan.
    #[test]
    fn an_interval_keeps_its_precision_in_the_judgement() {
        assert_eq!(risk("interval(0)", "interval(6)"), TypeChangeRisk::Safe);
        assert_eq!(risk("interval(3)", "interval"), TypeChangeRisk::Safe);
        // Measured: `00:00:01.234567` into `interval(0)` becomes `00:00:01`,
        // and into `interval(3)` becomes `00:00:01.235`.
        assert_eq!(
            risk("interval(6)", "interval(0)"),
            TypeChangeRisk::Narrowing
        );
        assert_eq!(risk("interval", "interval(3)"), TypeChangeRisk::Narrowing);
    }

    /// A `date` reaches further than a `timestamp`, so adding a time to it is
    /// not a widening. Measured on 18.6: `'5874897-01-01'::date` is accepted,
    /// `'294276-12-31'::date::timestamp` is the last one that converts, and
    /// `'300000-01-01'::date::timestamp` is `date out of range for timestamp`.
    ///
    /// This leaves no change between two date-or-time types classified `Safe`
    /// except a type to itself, which is the honest answer: every one of them
    /// drops a component, moves with the session's time zone, or runs off the
    /// end of the calendar.
    #[test]
    fn a_date_reaches_further_than_a_timestamp_so_it_is_not_widened_by_one() {
        assert_eq!(risk("date", "timestamp"), TypeChangeRisk::Narrowing);
        assert_eq!(risk("date", "timestamptz"), TypeChangeRisk::Narrowing);
        // A type to itself is still a change of nothing.
        assert_eq!(risk("date", "date"), TypeChangeRisk::Safe);
        assert_eq!(risk("timestamp", "timestamp"), TypeChangeRisk::Safe);
    }

    /// Gaining or losing a time zone is never a widening, however it looks.
    /// The conversion uses the **session's** `TimeZone`, so the same plan
    /// applied from two machines writes two different instants — which is why
    /// ADR-0012 §4 measured it rewriting the table under one session and not
    /// under the other.
    #[test]
    fn a_time_zone_gained_or_lost_is_not_a_widening() {
        for (from, to) in [
            ("timestamp", "timestamptz"),
            ("timestamptz", "timestamp"),
            ("time", "timetz"),
            ("timetz", "time"),
            ("date", "timestamptz"),
        ] {
            assert_eq!(
                risk(from, to),
                TypeChangeRisk::Narrowing,
                "`{from}` -> `{to}`"
            );
        }
    }

    fn normalized(name: &str) -> ColumnType {
        normalize(&ty(name)).expect("normalizes")
    }

    /// The zone question is asked of the pair, not of the risk class: every row
    /// below is `Narrowing` (above), and a human clearing a `Narrowing` at the
    /// gate has cleared the loss, not a reinterpretation nobody wrote down.
    #[test]
    fn a_conversion_that_gains_or_loses_the_zone_is_the_sessions_to_decide() {
        for (from, to) in [
            ("timestamp", "timestamptz"),
            ("timestamptz", "timestamp"),
            ("time", "timetz"),
            // No time part on one end and a zone gained on the other is the
            // same hazard: midnight, in whichever zone the session holds.
            ("date", "timestamptz"),
            ("timestamptz", "date"),
            // The offset survives and the answer is still the session's: the
            // date part is what moves, and the value is rebased to lose it.
            ("timestamptz", "timetz"),
            ("timetz", "timestamptz"),
        ] {
            assert!(
                depends_on_the_session_time_zone(&normalized(from), &normalized(to)),
                "`{from}` -> `{to}`"
            );
        }
    }

    /// The negative half, and the one that matters: this must not become a
    /// second name for "temporal". Every pair here converts under a rule the
    /// engine holds, not under a setting, and refusing one would refuse a
    /// declaration a plan is allowed to make.
    #[test]
    fn a_conversion_that_keeps_the_zone_where_it_was_is_not() {
        for (from, to) in [
            ("timestamp", "date"),
            ("date", "timestamp"),
            ("timestamptz", "timestamptz"),
            // The one pair with a zone that is still not the session's:
            // `timetz` stores the offset beside the local time, so dropping it
            // keeps what is already there. Refusing it would refuse a valid
            // plan, and the loss it does carry is what `Narrowing` is for.
            ("timetz", "time"),
            // Both ends carry an offset *and* a date part, so nothing is
            // rebased — the pin against the widened rule becoming "any two
            // types that can hold a zone".
            ("timetz", "timetz"),
            ("timestamp", "timestamp"),
            ("time", "time"),
            ("interval", "interval"),
            // Not temporal at all on one end: nothing to read a zone in.
            ("text", "timestamptz"),
            ("timestamptz", "text"),
            ("integer", "bigint"),
        ] {
            assert!(
                !depends_on_the_session_time_zone(&normalized(from), &normalized(to)),
                "`{from}` -> `{to}`"
            );
        }
    }

    /// `Safe` is about **loss**, not about duration. `integer -> bigint`
    /// rewrites the whole table under a lock that blocks readers, measured at
    /// six hundred times the cost of a change that does not, and it is still
    /// `Safe`: a rewrite loses nothing and cannot fail. The cost belongs to the
    /// estimate (SPEC 14.1, ADR-0012 §3, DECISIONS 241), and folding it in here
    /// would teach a reviewer that the classes do not mean what they say.
    #[test]
    fn a_rewrite_is_not_a_risk_class() {
        assert_eq!(risk("integer", "bigint"), TypeChangeRisk::Safe);
        assert_eq!(risk("varchar(20)", "text"), TypeChangeRisk::Safe);
        // And the converse: something that does *not* rewrite is still
        // narrowing when it can lose. `varchar(10) -> varchar(20)` is free and
        // safe; `text -> varchar(20)` rewrites and can fail; neither fact
        // decided the other.
        assert_eq!(risk("varchar(10)", "varchar(20)"), TypeChangeRisk::Safe);
        assert_eq!(risk("text", "varchar(20)"), TypeChangeRisk::Narrowing);
    }

    /// A type this catalogue does not know converts to nothing, including to
    /// another unknown. The judgement has to be the one that stops a plan.
    #[test]
    fn an_unknown_type_is_never_judged_safe() {
        let unknown = ty("widget");
        let other = ty("gadget");
        assert_eq!(change_risk(&unknown, &other), TypeChangeRisk::Incompatible);
        assert_eq!(
            change_risk(&unknown, &ty("integer")),
            TypeChangeRisk::Incompatible
        );
        // Except to itself, which is not a change at all.
        assert_eq!(change_risk(&unknown, &unknown), TypeChangeRisk::Safe);
    }
}
