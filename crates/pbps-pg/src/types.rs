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
use pbps_model::{ColumnType, TypeArg};

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

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(s: &str) -> ColumnType {
        s.parse().expect("a type parses")
    }

    fn norm(s: &str) -> String {
        normalize(&ty(s))
            .unwrap_or_else(|e| panic!("`{s}` should normalize: {e}"))
            .to_string()
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
