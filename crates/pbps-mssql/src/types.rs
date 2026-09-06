//! The SQL Server type catalogue: what exists, how it is spelled canonically,
//! and how safe it is to change one type into another.
//!
//! # Why normalization has to be aggressive
//!
//! Diff compares normalized types for equality, and introspection (Phase 3) reads
//! types back out of `sys.types`, which reports the *stored* form — never the
//! spelling the user wrote. `INTEGER`, `DEC(18)` and `FLOAT(10)` come back as
//! `int`, `decimal(18,0)` and `real`. Unless the declared side is folded into the
//! same form first, every single run would report a type change that does not
//! exist, and drift detection would cry wolf daily.
//!
//! So normalization does three things: expand aliases, fill in the arguments SQL
//! Server fills in itself, and collapse the spellings the engine collapses.
//!
//! # Why the deprecated types are still accepted
//!
//! `text`, `ntext` and `image` have been deprecated since 2005, and no new
//! declaration should use them. They are accepted anyway, because `pbps pull` has
//! to be able to read in a database that already has them — refusing them would
//! lock exactly the legacy databases this tool exists to bring under control out
//! of adopting it. What they may not do (be a key, be indexed) is caught by
//! `validate`.

use pbps_dialect::{DialectError, TypeChangeRisk};
use pbps_model::{ColumnType, TypeArg};

pub const DIALECT: &str = "mssql";

/// What kind of argument list a type takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgShape {
    /// No arguments at all: `int`, `bit`, `date`.
    None,
    /// One length, `max` permitted: `varchar(100)`, `nvarchar(max)`.
    Length { max_len: i64, allows_max: bool },
    /// One optional precision: `datetime2(3)`, `time(7)`.
    Precision { max: i64, default: i64 },
    /// Optional precision and scale: `decimal(18, 2)`.
    PrecisionScale,
    /// `float(n)`, which SQL Server collapses to either `real` or `float(53)`.
    FloatPrecision,
}

/// Every type name SQL Server will accept in a column definition, with the shape
/// of its argument list. Aliases are resolved before this table is consulted.
const CATALOGUE: &[(&str, ArgShape)] = &[
    // Exact numerics.
    ("bit", ArgShape::None),
    ("tinyint", ArgShape::None),
    ("smallint", ArgShape::None),
    ("int", ArgShape::None),
    ("bigint", ArgShape::None),
    ("decimal", ArgShape::PrecisionScale),
    ("smallmoney", ArgShape::None),
    ("money", ArgShape::None),
    // Approximate numerics.
    ("real", ArgShape::None),
    ("float", ArgShape::FloatPrecision),
    // Date and time.
    ("date", ArgShape::None),
    ("time", ArgShape::Precision { max: 7, default: 7 }),
    ("smalldatetime", ArgShape::None),
    ("datetime", ArgShape::None),
    ("datetime2", ArgShape::Precision { max: 7, default: 7 }),
    ("datetimeoffset", ArgShape::Precision { max: 7, default: 7 }),
    // Character strings. `char`/`nchar` are fixed length, `varchar`/`nvarchar`
    // variable; only the variable ones accept `max`.
    (
        "char",
        ArgShape::Length {
            max_len: 8000,
            allows_max: false,
        },
    ),
    (
        "varchar",
        ArgShape::Length {
            max_len: 8000,
            allows_max: true,
        },
    ),
    (
        "nchar",
        ArgShape::Length {
            max_len: 4000,
            allows_max: false,
        },
    ),
    (
        "nvarchar",
        ArgShape::Length {
            max_len: 4000,
            allows_max: true,
        },
    ),
    // Binary strings.
    (
        "binary",
        ArgShape::Length {
            max_len: 8000,
            allows_max: false,
        },
    ),
    (
        "varbinary",
        ArgShape::Length {
            max_len: 8000,
            allows_max: true,
        },
    ),
    // Everything else.
    ("uniqueidentifier", ArgShape::None),
    ("xml", ArgShape::None),
    ("sql_variant", ArgShape::None),
    ("hierarchyid", ArgShape::None),
    ("geography", ArgShape::None),
    ("geometry", ArgShape::None),
    ("timestamp", ArgShape::None),
    ("sysname", ArgShape::None),
    // Deprecated since 2005, accepted so that `pull` can read legacy databases.
    ("text", ArgShape::None),
    ("ntext", ArgShape::None),
    ("image", ArgShape::None),
];

/// Alias to canonical name. The right-hand side is what `sys.types` reports, so
/// that a declaration and an introspected column converge on one spelling.
const ALIASES: &[(&str, &str)] = &[
    ("integer", "int"),
    ("dec", "decimal"),
    ("numeric", "decimal"),
    ("character", "char"),
    ("char varying", "varchar"),
    ("character varying", "varchar"),
    ("national char", "nchar"),
    ("national character", "nchar"),
    ("national char varying", "nvarchar"),
    ("national character varying", "nvarchar"),
    ("national text", "ntext"),
    ("binary varying", "varbinary"),
    ("double precision", "float"),
    // `rowversion` is the modern name, but `timestamp` is what the catalogue
    // views report, and the catalogue is what introspection has to agree with.
    ("rowversion", "timestamp"),
];

fn shape_of(base: &str) -> Option<ArgShape> {
    CATALOGUE.iter().find(|(n, _)| *n == base).map(|(_, s)| *s)
}

fn canonical_base(base: &str) -> &str {
    ALIASES
        .iter()
        .find(|(a, _)| *a == base)
        .map(|(_, c)| *c)
        .unwrap_or(base)
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

/// Expands aliases, fills in the arguments SQL Server fills in itself, and
/// collapses the spellings the engine collapses.
pub fn normalize(ty: &ColumnType) -> Result<ColumnType, DialectError> {
    // `ColumnType` already lowercases and squeezes whitespace inside arguments,
    // but a multi-word base name such as `national  character` can still carry
    // runs of spaces.
    let base_words: Vec<&str> = ty.base.split_whitespace().collect();
    let base = base_words.join(" ");
    let base = canonical_base(&base).to_owned();
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

        ArgShape::Length {
            max_len,
            allows_max,
        } => match ty.args.as_slice() {
            // SQL Server's own default for an omitted length in a column
            // definition is 1 — surprising, but silently guessing something
            // friendlier would make the declaration disagree with the database.
            [] => vec![TypeArg::Int(1)],
            [TypeArg::Max] if allows_max => vec![TypeArg::Max],
            [TypeArg::Max] => {
                return Err(arity(ty, format!("`{base}` has no `max` form")));
            }
            [TypeArg::Int(n)] if *n >= 1 && *n <= max_len => vec![TypeArg::Int(*n)],
            [TypeArg::Int(n)] => {
                return Err(arity(
                    ty,
                    format!("the length must be between 1 and {max_len}, got {n}"),
                ));
            }
            _ => return Err(arity(ty, format!("`{base}` takes one length"))),
        },

        ArgShape::Precision { max, default } => match ty.args.as_slice() {
            [] => vec![TypeArg::Int(default)],
            [TypeArg::Int(n)] if *n >= 0 && *n <= max => vec![TypeArg::Int(*n)],
            [TypeArg::Int(n)] => {
                return Err(arity(
                    ty,
                    format!("the precision must be between 0 and {max}, got {n}"),
                ));
            }
            _ => return Err(arity(ty, format!("`{base}` takes one precision"))),
        },

        ArgShape::PrecisionScale => match ty.args.as_slice() {
            [] => vec![TypeArg::Int(18), TypeArg::Int(0)],
            [TypeArg::Int(p)] => {
                check_decimal(ty, *p, 0)?;
                vec![TypeArg::Int(*p), TypeArg::Int(0)]
            }
            [TypeArg::Int(p), TypeArg::Int(s)] => {
                check_decimal(ty, *p, *s)?;
                vec![TypeArg::Int(*p), TypeArg::Int(*s)]
            }
            _ => {
                return Err(arity(
                    ty,
                    format!("`{base}` takes a precision and an optional scale"),
                ));
            }
        },

        // `float(n)` is not stored as written: 1..=24 becomes `real` and
        // 25..=53 becomes `float(53)`. Normalizing to what is stored is what
        // stops `float(30)` from looking like a change every single run.
        ArgShape::FloatPrecision => match ty.args.as_slice() {
            [] => return Ok(ColumnType::new("float", vec![TypeArg::Int(53)])),
            [TypeArg::Int(n)] if (1..=24).contains(n) => {
                return Ok(ColumnType::simple("real"));
            }
            [TypeArg::Int(n)] if (25..=53).contains(n) => vec![TypeArg::Int(53)],
            [TypeArg::Int(n)] => {
                return Err(arity(
                    ty,
                    format!("the precision of `float` must be between 1 and 53, got {n}"),
                ));
            }
            _ => return Err(arity(ty, "`float` takes one precision")),
        },
    };

    Ok(ColumnType::new(base, args))
}

fn check_decimal(ty: &ColumnType, p: i64, s: i64) -> Result<(), DialectError> {
    if !(1..=38).contains(&p) {
        return Err(arity(
            ty,
            format!("the precision of `decimal` must be between 1 and 38, got {p}"),
        ));
    }
    if s < 0 || s > p {
        return Err(arity(
            ty,
            format!("the scale of `decimal` must be between 0 and the precision ({p}), got {s}"),
        ));
    }
    Ok(())
}

/// Whether a type may hold a `NOT NULL` IDENTITY.
pub fn can_be_identity(ty: &ColumnType) -> bool {
    let Ok(t) = normalize(ty) else {
        return false;
    };
    match t.base.as_str() {
        "tinyint" | "smallint" | "int" | "bigint" => true,
        // decimal is allowed only with a scale of zero.
        "decimal" => matches!(t.args.get(1), Some(TypeArg::Int(0))),
        _ => false,
    }
}

/// Whether a type can appear in an index key or a key constraint.
///
/// The LOB types cannot; `varchar(max)` and friends cannot either. Being unable
/// to index is a property of the type, so it belongs here rather than in the
/// validator that consults it.
pub fn is_indexable(ty: &ColumnType) -> bool {
    let Ok(t) = normalize(ty) else {
        return false;
    };
    !matches!(t.base.as_str(), "text" | "ntext" | "image" | "xml") && !t.is_max()
}

// ---------------------------------------------------------------------------
// Risk
// ---------------------------------------------------------------------------

/// The families a type belongs to for the purpose of judging a change.
///
/// A change within a family is a question of capacity; a change across families
/// is a conversion, and conversions are judged conservatively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// Exact numeric, described by how many digits fit either side of the point.
    Exact {
        int_digits: i64,
        scale: i64,
    },
    /// Approximate numeric, described by significant decimal digits.
    Approx {
        digits: i64,
    },
    /// A string, described by its length, whether it is unicode, and whether it
    /// is blank-padded to a fixed width.
    Text {
        len: Len,
        unicode: bool,
        fixed: bool,
    },
    Binary {
        len: Len,
        fixed: bool,
    },
    /// Date and time, described by which components it stores, how precise the
    /// time part is, and how wide the date range is.
    Temporal {
        has_date: bool,
        has_time: bool,
        has_offset: bool,
        precision: i64,
        range: i64,
    },
    /// Everything with no ordering at all: only a change to itself is safe.
    Opaque,
}

/// A string or binary length. `Max` is unbounded, which is why it cannot be an
/// `i64` sentinel — a length of zero would compare the wrong way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Len {
    Bounded(i64),
    Max,
}

impl Len {
    /// Whether a value of length `self` always fits in a column of length
    /// `other`.
    fn fits_in(self, other: Len) -> bool {
        match (self, other) {
            (_, Len::Max) => true,
            (Len::Max, Len::Bounded(_)) => false,
            (Len::Bounded(a), Len::Bounded(b)) => a <= b,
        }
    }
}

fn family(t: &ColumnType) -> Family {
    let arg0 = t.args.first();
    let int_arg = |i: usize| match t.args.get(i) {
        Some(TypeArg::Int(n)) => *n,
        Some(TypeArg::Max) | Some(TypeArg::Ident(_)) | None => 0,
    };
    let len = || match arg0 {
        Some(TypeArg::Max) => Len::Max,
        Some(TypeArg::Int(n)) => Len::Bounded(*n),
        Some(TypeArg::Ident(_)) | None => Len::Bounded(1),
    };

    let exact = |int_digits, scale| Family::Exact { int_digits, scale };
    let text = |unicode, fixed| Family::Text {
        len: len(),
        unicode,
        fixed,
    };
    let temporal = |has_date, has_time, has_offset, precision, range| Family::Temporal {
        has_date,
        has_time,
        has_offset,
        precision,
        range,
    };

    match t.base.as_str() {
        // `bit` holds 0/1, so one digit and no fraction: it widens into every
        // other exact numeric, and nothing widens into it.
        "bit" => exact(1, 0),
        "tinyint" => exact(3, 0),
        "smallint" => exact(5, 0),
        "int" => exact(10, 0),
        "bigint" => exact(19, 0),
        "decimal" => exact(int_arg(0) - int_arg(1), int_arg(1)),
        "smallmoney" => exact(6, 4),
        "money" => exact(15, 4),
        "real" => Family::Approx { digits: 7 },
        "float" => Family::Approx { digits: 15 },
        "char" => text(false, true),
        "varchar" => text(false, false),
        "nchar" => text(true, true),
        "nvarchar" => text(true, false),
        // sysname takes no arguments, but its capacity is nvarchar(128), not
        // the default length of an unparameterized string declaration.
        "sysname" => Family::Text {
            len: Len::Bounded(128),
            unicode: true,
            fixed: false,
        },
        "text" => Family::Text {
            len: Len::Max,
            unicode: false,
            fixed: false,
        },
        "ntext" => Family::Text {
            len: Len::Max,
            unicode: true,
            fixed: false,
        },
        "binary" => Family::Binary {
            len: len(),
            fixed: true,
        },
        "varbinary" | "timestamp" => Family::Binary {
            len: len(),
            fixed: false,
        },
        "image" => Family::Binary {
            len: Len::Max,
            fixed: false,
        },
        "date" => temporal(true, false, false, 0, 2),
        "time" => temporal(false, true, false, int_arg(0), 2),
        // smalldatetime rounds to the minute and only spans 1900-2079; datetime
        // rounds to ~3ms and spans 1753-9999; the datetime2 family spans 0001-9999.
        "smalldatetime" => temporal(true, true, false, 0, 0),
        "datetime" => temporal(true, true, false, 3, 1),
        "datetime2" => temporal(true, true, false, int_arg(0), 2),
        "datetimeoffset" => temporal(true, true, true, int_arg(0), 2),
        _ => Family::Opaque,
    }
}

/// How safe changing `from` into `to` is. Both must already be normalized.
///
/// The question answered is "can this kind of change fail or lose data at all",
/// never "will it, given today's rows" — inspecting data is a runtime concern
/// (SPEC §7.2). When the answer is not clearly no, it is yes.
pub fn change_risk(from: &ColumnType, to: &ColumnType) -> TypeChangeRisk {
    if from == to {
        return TypeChangeRisk::Safe;
    }
    let (a, b) = (family(from), family(to));

    match (a, b) {
        (
            Family::Exact {
                int_digits: ai,
                scale: asc,
            },
            Family::Exact {
                int_digits: bi,
                scale: bsc,
            },
        ) => {
            // Both halves have to grow: losing integer digits overflows, losing
            // scale rounds. Either is a loss.
            safe_if(bi >= ai && bsc >= asc)
        }

        // An exact value survives a float only if every digit it can hold still
        // fits in the float's significant digits.
        (Family::Exact { int_digits, scale }, Family::Approx { digits }) => {
            safe_if(int_digits + scale <= digits)
        }
        (Family::Approx { digits: a }, Family::Approx { digits: b }) => safe_if(b >= a),
        // A float into an exact type rounds, and its range is far wider.
        (Family::Approx { .. }, Family::Exact { .. }) => TypeChangeRisk::Narrowing,

        (
            Family::Text {
                len: al,
                unicode: au,
                fixed: af,
            },
            Family::Text {
                len: bl,
                unicode: bu,
                fixed: bf,
            },
        ) => {
            // Unicode into non-unicode drops whatever the target collation
            // cannot represent, whatever the lengths are.
            if au && !bu {
                return TypeChangeRisk::Narrowing;
            }
            // Variable into fixed pads every existing value with blanks, which
            // changes the data even when nothing is truncated.
            if !af && bf {
                return TypeChangeRisk::Narrowing;
            }
            safe_if(al.fits_in(bl))
        }

        (Family::Binary { len: al, fixed: af }, Family::Binary { len: bl, fixed: bf }) => {
            // Growing a fixed binary also appends zero bytes: the payload and
            // its hash change even though SQL Server compares it equal to the
            // old value. Capacity alone cannot establish byte preservation.
            if bf && (!af || al != bl) {
                return TypeChangeRisk::Narrowing;
            }
            safe_if(al.fits_in(bl))
        }

        (
            Family::Temporal {
                has_date: ad,
                has_time: at,
                has_offset: ao,
                precision: ap,
                range: ar,
            },
            Family::Temporal {
                has_date: bd,
                has_time: bt,
                has_offset: bo,
                precision: bp,
                range: br,
            },
        ) => {
            // The target has to keep every component the source stores, be at
            // least as precise, and span at least as wide a range.
            let keeps_components = (!ad || bd) && (!at || bt) && (!ao || bo);
            safe_if(keeps_components && bp >= ap && br >= ar)
        }

        // Numeric to text always succeeds in SQL Server, but the rendered form
        // may not fit and the round trip is not the identity, so it needs
        // approval rather than being blocked outright.
        (Family::Exact { .. } | Family::Approx { .. }, Family::Text { .. })
        | (Family::Temporal { .. }, Family::Text { .. }) => TypeChangeRisk::Narrowing,

        // Everything else is a conversion that the engine itself can refuse
        // halfway through: text holding a non-number, a guid into an int.
        (
            Family::Exact { .. }
            | Family::Approx { .. }
            | Family::Text { .. }
            | Family::Binary { .. }
            | Family::Temporal { .. }
            | Family::Opaque,
            _,
        ) => TypeChangeRisk::Incompatible,
    }
}

fn safe_if(cond: bool) -> TypeChangeRisk {
    if cond {
        TypeChangeRisk::Safe
    } else {
        TypeChangeRisk::Narrowing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }
    fn norm(s: &str) -> String {
        normalize(&ty(s)).unwrap().to_string()
    }
    fn risk(from: &str, to: &str) -> TypeChangeRisk {
        change_risk(&normalize(&ty(from)).unwrap(), &normalize(&ty(to)).unwrap())
    }

    #[test]
    fn aliases_become_the_name_the_catalogue_reports() {
        assert_eq!(norm("integer"), "int");
        assert_eq!(norm("INTEGER"), "int");
        assert_eq!(norm("numeric(10,2)"), "decimal(10, 2)");
        assert_eq!(norm("dec(10)"), "decimal(10, 0)");
        assert_eq!(norm("character varying(50)"), "varchar(50)");
        assert_eq!(norm("national character varying(50)"), "nvarchar(50)");
        assert_eq!(norm("rowversion"), "timestamp");
    }

    /// A multi-word alias survives whatever spacing the user typed.
    #[test]
    fn extra_spaces_inside_a_multi_word_name_do_not_hide_the_alias() {
        assert_eq!(norm("national   character   varying(10)"), "nvarchar(10)");
        assert_eq!(norm("double  precision"), "float(53)");
    }

    /// Introspection reads the arguments the engine filled in, so the declared
    /// side has to fill in the same ones or every run reports a phantom change.
    #[test]
    fn omitted_arguments_are_filled_in_as_the_engine_fills_them() {
        assert_eq!(norm("decimal"), "decimal(18, 0)");
        assert_eq!(norm("decimal(9)"), "decimal(9, 0)");
        assert_eq!(norm("datetime2"), "datetime2(7)");
        assert_eq!(norm("time"), "time(7)");
        assert_eq!(norm("datetimeoffset"), "datetimeoffset(7)");
        // Surprising, but it is what SQL Server does with a bare `varchar` in a
        // column definition.
        assert_eq!(norm("varchar"), "varchar(1)");
        assert_eq!(norm("nchar"), "nchar(1)");
    }

    /// `float(n)` is not stored as written: the engine keeps either `real` or
    /// `float(53)`, and normalizing to anything else guarantees a phantom diff.
    #[test]
    fn float_collapses_to_what_is_actually_stored() {
        assert_eq!(norm("float"), "float(53)");
        assert_eq!(norm("float(53)"), "float(53)");
        assert_eq!(norm("float(30)"), "float(53)");
        assert_eq!(norm("float(24)"), "real");
        assert_eq!(norm("float(1)"), "real");
        assert_eq!(norm("real"), "real");
    }

    #[test]
    fn normalizing_is_idempotent() {
        for s in [
            "integer",
            "float(30)",
            "decimal",
            "nvarchar(max)",
            "datetime2",
            "rowversion",
        ] {
            let once = normalize(&ty(s)).unwrap();
            let twice = normalize(&once).unwrap();
            assert_eq!(once, twice, "{s} did not settle after one pass");
        }
    }

    #[test]
    fn unknown_and_malformed_types_are_refused() {
        assert!(normalize(&ty("jsonb")).is_err(), "that is PostgreSQL");
        assert!(normalize(&ty("serial")).is_err());
        assert!(normalize(&ty("int(10)")).is_err(), "int takes no arguments");
        assert!(
            normalize(&ty("nvarchar(5000)")).is_err(),
            "nvarchar caps at 4000"
        );
        assert!(
            normalize(&ty("varchar(9000)")).is_err(),
            "varchar caps at 8000"
        );
        assert!(normalize(&ty("nvarchar(0)")).is_err());
        assert!(normalize(&ty("decimal(39,0)")).is_err());
        assert!(
            normalize(&ty("decimal(5,7)")).is_err(),
            "scale exceeds precision"
        );
        assert!(normalize(&ty("datetime2(8)")).is_err());
        assert!(normalize(&ty("float(54)")).is_err());
    }

    /// `max` is only a form of the variable-length types; `char(max)` is not a
    /// thing, and accepting it would generate SQL the server rejects.
    #[test]
    fn max_belongs_only_to_the_variable_length_types() {
        assert_eq!(norm("nvarchar(max)"), "nvarchar(max)");
        assert_eq!(norm("varbinary(max)"), "varbinary(max)");
        assert!(normalize(&ty("char(max)")).is_err());
        assert!(normalize(&ty("nchar(max)")).is_err());
        assert!(normalize(&ty("binary(max)")).is_err());
    }

    /// They are twenty years deprecated, but a database that already has them is
    /// exactly the kind this tool exists to adopt.
    #[test]
    fn the_deprecated_lob_types_are_still_readable() {
        for s in ["text", "ntext", "image"] {
            assert!(normalize(&ty(s)).is_ok(), "{s} should still load");
            assert!(!is_indexable(&ty(s)), "{s} cannot be part of a key");
        }
    }

    #[test]
    fn only_integers_and_scale_free_decimals_can_be_identity() {
        for s in ["tinyint", "smallint", "int", "bigint", "decimal(18,0)"] {
            assert!(can_be_identity(&ty(s)), "{s} should be allowed");
        }
        for s in [
            "decimal(18,2)",
            "nvarchar(10)",
            "float",
            "bit",
            "uniqueidentifier",
        ] {
            assert!(!can_be_identity(&ty(s)), "{s} should not be allowed");
        }
    }

    #[test]
    fn max_length_columns_cannot_be_indexed() {
        assert!(is_indexable(&ty("nvarchar(400)")));
        assert!(!is_indexable(&ty("nvarchar(max)")));
        assert!(!is_indexable(&ty("xml")));
    }

    // ---- risk ----

    #[test]
    fn widening_an_integer_is_safe_and_narrowing_is_not() {
        assert_eq!(risk("tinyint", "int"), TypeChangeRisk::Safe);
        assert_eq!(risk("int", "bigint"), TypeChangeRisk::Safe);
        assert_eq!(risk("bit", "tinyint"), TypeChangeRisk::Safe);
        assert_eq!(risk("bigint", "int"), TypeChangeRisk::Narrowing);
        assert_eq!(risk("int", "smallint"), TypeChangeRisk::Narrowing);
    }

    #[test]
    fn a_decimal_must_grow_on_both_sides_of_the_point() {
        assert_eq!(risk("decimal(10,2)", "decimal(12,2)"), TypeChangeRisk::Safe);
        assert_eq!(risk("decimal(10,2)", "decimal(12,4)"), TypeChangeRisk::Safe);
        // Same total width, but two integer digits were traded away for scale.
        assert_eq!(
            risk("decimal(10,2)", "decimal(10,4)"),
            TypeChangeRisk::Narrowing
        );
        // Losing scale rounds every existing value.
        assert_eq!(
            risk("decimal(12,4)", "decimal(12,2)"),
            TypeChangeRisk::Narrowing
        );
    }

    #[test]
    fn crossing_between_exact_and_approximate_follows_the_digits() {
        assert_eq!(risk("int", "float"), TypeChangeRisk::Safe);
        assert_eq!(
            risk("int", "real"),
            TypeChangeRisk::Narrowing,
            "7 digits is not 10"
        );
        assert_eq!(risk("bigint", "float"), TypeChangeRisk::Narrowing);
        assert_eq!(risk("real", "float"), TypeChangeRisk::Safe);
        assert_eq!(risk("float", "real"), TypeChangeRisk::Narrowing);
        // A float into an exact type rounds, whatever the widths are.
        assert_eq!(risk("real", "decimal(38,10)"), TypeChangeRisk::Narrowing);
    }

    #[test]
    fn money_is_compared_as_the_decimal_it_is() {
        assert_eq!(risk("smallmoney", "money"), TypeChangeRisk::Safe);
        assert_eq!(risk("money", "smallmoney"), TypeChangeRisk::Narrowing);
        assert_eq!(risk("money", "decimal(19,4)"), TypeChangeRisk::Safe);
        assert_eq!(risk("money", "decimal(18,4)"), TypeChangeRisk::Narrowing);
    }

    #[test]
    fn lengthening_a_string_is_safe_and_shortening_is_not() {
        assert_eq!(risk("nvarchar(50)", "nvarchar(100)"), TypeChangeRisk::Safe);
        assert_eq!(
            risk("nvarchar(100)", "nvarchar(50)"),
            TypeChangeRisk::Narrowing
        );
        assert_eq!(risk("nvarchar(100)", "nvarchar(max)"), TypeChangeRisk::Safe);
        // `max` is unbounded, so going to any fixed length can truncate — the
        // direction must not come out backwards just because `max` has no number.
        assert_eq!(
            risk("nvarchar(max)", "nvarchar(4000)"),
            TypeChangeRisk::Narrowing
        );
    }

    #[test]
    fn sysname_has_the_capacity_of_nvarchar_128_without_length_arguments() {
        assert_eq!(norm("sysname"), "sysname");
        assert!(normalize(&ty("sysname(128)")).is_err());
        for (from, to, expected) in [
            ("sysname", "nvarchar(10)", TypeChangeRisk::Narrowing),
            ("sysname", "nvarchar(127)", TypeChangeRisk::Narrowing),
            ("sysname", "nvarchar(128)", TypeChangeRisk::Safe),
            ("sysname", "nvarchar(max)", TypeChangeRisk::Safe),
            ("nvarchar(50)", "sysname", TypeChangeRisk::Safe),
            ("nvarchar(128)", "sysname", TypeChangeRisk::Safe),
            ("nvarchar(129)", "sysname", TypeChangeRisk::Narrowing),
            ("nvarchar(max)", "sysname", TypeChangeRisk::Narrowing),
            ("sysname", "varchar(128)", TypeChangeRisk::Narrowing),
            ("sysname", "nchar(128)", TypeChangeRisk::Narrowing),
        ] {
            assert_eq!(risk(from, to), expected, "{from} -> {to}");
        }
    }

    /// Going from unicode to non-unicode loses whatever the target collation
    /// cannot represent, no matter how much longer the target is.
    #[test]
    fn dropping_unicode_is_never_safe_however_long_the_target() {
        assert_eq!(
            risk("nvarchar(10)", "varchar(4000)"),
            TypeChangeRisk::Narrowing
        );
        assert_eq!(
            risk("nvarchar(10)", "varchar(max)"),
            TypeChangeRisk::Narrowing
        );
        assert_eq!(risk("varchar(10)", "nvarchar(10)"), TypeChangeRisk::Safe);
    }

    /// `char` blank-pads, so moving into one rewrites every stored value even
    /// when nothing is truncated.
    #[test]
    fn moving_into_a_fixed_width_string_changes_the_data() {
        assert_eq!(risk("varchar(10)", "char(100)"), TypeChangeRisk::Narrowing);
        assert_eq!(risk("char(10)", "varchar(10)"), TypeChangeRisk::Safe);
        assert_eq!(risk("char(10)", "char(20)"), TypeChangeRisk::Safe);
        assert_eq!(
            risk("varbinary(10)", "binary(20)"),
            TypeChangeRisk::Narrowing
        );
    }

    #[test]
    fn fixed_binary_growth_requires_approval_for_the_added_padding_bytes() {
        for (from, to, expected) in [
            ("binary(8)", "binary(16)", TypeChangeRisk::Narrowing),
            ("binary(16)", "binary(8)", TypeChangeRisk::Narrowing),
            ("binary(8)", "binary(8)", TypeChangeRisk::Safe),
            ("varbinary(8)", "binary(8)", TypeChangeRisk::Narrowing),
            ("varbinary(8)", "binary(16)", TypeChangeRisk::Narrowing),
            ("binary(8)", "varbinary(16)", TypeChangeRisk::Safe),
            ("varbinary(8)", "varbinary(16)", TypeChangeRisk::Safe),
            ("binary(8)", "varbinary(4)", TypeChangeRisk::Narrowing),
            ("char(8)", "char(16)", TypeChangeRisk::Safe),
        ] {
            assert_eq!(risk(from, to), expected, "{from} -> {to}");
        }
    }

    #[test]
    fn a_date_type_must_keep_every_component_precision_and_range() {
        assert_eq!(risk("date", "datetime2(7)"), TypeChangeRisk::Safe);
        assert_eq!(risk("time(3)", "time(7)"), TypeChangeRisk::Safe);
        assert_eq!(risk("time(7)", "time(3)"), TypeChangeRisk::Narrowing);
        assert_eq!(risk("smalldatetime", "datetime"), TypeChangeRisk::Safe);
        assert_eq!(risk("datetime", "datetime2(7)"), TypeChangeRisk::Safe);
        // datetime2 spans 0001-9999 and datetime only 1753-9999, so the reverse
        // loses both precision and range.
        assert_eq!(risk("datetime2(7)", "datetime"), TypeChangeRisk::Narrowing);
        // The offset has nowhere to go.
        assert_eq!(
            risk("datetimeoffset(7)", "datetime2(7)"),
            TypeChangeRisk::Narrowing
        );
        assert_eq!(
            risk("datetime2(7)", "datetimeoffset(7)"),
            TypeChangeRisk::Safe
        );
        // A date has no time of day to keep, so it cannot go into a `time`.
        assert_eq!(risk("date", "time(7)"), TypeChangeRisk::Narrowing);
    }

    /// A conversion the engine can refuse halfway through is worse than one that
    /// merely truncates, and the plan should say which it is.
    #[test]
    fn a_conversion_that_can_fail_is_reported_as_incompatible() {
        assert_eq!(risk("nvarchar(10)", "int"), TypeChangeRisk::Incompatible);
        assert_eq!(
            risk("uniqueidentifier", "bigint"),
            TypeChangeRisk::Incompatible
        );
        assert_eq!(
            risk("nvarchar(10)", "varbinary(10)"),
            TypeChangeRisk::Incompatible
        );
        assert_eq!(risk("xml", "nvarchar(max)"), TypeChangeRisk::Incompatible);
        // Numbers always render as text, so that direction only truncates.
        assert_eq!(risk("int", "nvarchar(5)"), TypeChangeRisk::Narrowing);
    }

    /// Spelling a type differently is not a change, and this is the single most
    /// important property in the file: without it every plan is full of noise.
    #[test]
    fn respelling_a_type_is_not_a_change() {
        for (a, b) in [
            ("integer", "int"),
            ("numeric(18,0)", "decimal"),
            ("double precision", "float(53)"),
            ("float(10)", "real"),
            ("NVARCHAR(100)", "nvarchar(100)"),
            ("rowversion", "timestamp"),
        ] {
            assert_eq!(risk(a, b), TypeChangeRisk::Safe, "{a} -> {b}");
            assert_eq!(
                normalize(&ty(a)).unwrap(),
                normalize(&ty(b)).unwrap(),
                "{a} and {b} must normalize alike"
            );
        }
    }
}
