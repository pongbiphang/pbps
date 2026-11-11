//! The column-type table every engine fills in, and the skeleton that reads it.
//!
//! Each engine keeps its facts about declared column types as two const tables
//! (which names exist and what argument list each takes, and which spellings
//! are aliases of which), and normalises a declaration against them in the same
//! order: squeeze whitespace in the base name, resolve the alias, look the
//! shape up, then fill omitted arguments and check ranges. Only the parameters
//! differ between engines, so they live here as data on [`ArgShape`] and
//! [`TypeTable`], and the engine keeps only the rules that are its own.
//!
//! # Why the steps are separate functions
//!
//! Some engine rules sit between the steps. PostgreSQL resolves `float` after
//! the alias lookup and before the shape lookup, because which type it becomes
//! depends on its argument; SQL Server's `float` shape is found by the lookup
//! and then handled by the engine. [`TypeTable::alias`], [`TypeTable::shape_of`]
//! and [`TypeTable::fill`] let an engine run its own rule at either point, and
//! [`TypeTable::lookup`] and [`TypeTable::normalize`] chain them for an engine
//! with nothing in between.
//!
//! Nothing here assumes an engine: the error text is built from the parameters,
//! never from a dialect name, so the wording an engine had before it used this
//! module is reproduced from its row.

use pbps_model::{ColumnType, TypeArg};

use crate::DialectError;

/// What an omitted argument list means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Omitted {
    /// The declaration reads back with no arguments at all. Filling any in
    /// would declare a different type from the one that comes back.
    Empty,
    /// The engine supplies this value, and the declaration reads back with it.
    /// For [`ArgShape::Scale`] it is the precision, and the scale filled in
    /// with it is zero.
    Fill(i64),
}

/// How a `max` argument is treated by a length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxWord {
    /// `max` is the unbounded form and is kept as written.
    Allowed,
    /// The engine has the word but not for this type, and says so.
    Refused,
    /// The engine has no such word, so `max` is refused like any other
    /// argument list that is not one length. Its refusal also names the bare
    /// form as the unbounded one.
    Absent,
}

/// What an engine does with a precision above the bound it documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Excess {
    /// It refuses the declaration.
    Refused,
    /// It stores the bound and says nothing, so a declaration that kept the
    /// larger number would read back as something else.
    Clamped,
}

/// The values a scale may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleRange {
    /// `-max..=max`: the scale is checked on its own and may be negative.
    Signed(i64),
    /// `0..=precision`: a scale larger than the precision is refused.
    UpToPrecision,
}

/// What kind of argument list a type takes, with the bounds that differ between
/// engines carried on the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgShape {
    /// No arguments at all: `integer`, `text`, `date`.
    None,
    /// One length in `1..=max`, or none.
    Length {
        max: i64,
        omitted: Omitted,
        max_word: MaxWord,
    },
    /// One precision in `0..=max`, or none (fractional-seconds digits).
    Precision {
        max: i64,
        omitted: Omitted,
        excess: Excess,
    },
    /// A precision in `1..=max_precision` and an optional scale. A precision
    /// alone has scale zero.
    Scale {
        max_precision: i64,
        scale: ScaleRange,
        omitted: Omitted,
    },
    /// A spelling whose arguments the engine reads itself after the lookup,
    /// such as `float(n)`, whose argument decides which type it is.
    /// [`TypeTable::fill`] hands its arguments back unjudged.
    Engine,
}

/// What a table does with a spelling its catalogue does not hold.
#[derive(Debug, Clone, Copy)]
pub enum Unknown {
    /// The declaration is refused with [`DialectError::UnknownType`]. A closed
    /// catalogue refuses, because a pass-through would name a type no catalog
    /// ever returns and be reported as a change on every run.
    Refuse,
    /// Any spelling maps to a canonical catalogue name by a rule, as a type
    /// affinity does. The rule is not consulted for a spelling the catalogue
    /// already holds, and it must name a catalogue row.
    Map(fn(&str) -> &'static str),
}

/// An engine's type facts.
#[derive(Debug, Clone, Copy)]
pub struct TypeTable {
    /// The engine's name as its errors spell it.
    pub dialect: &'static str,
    /// Every name the engine admits, spelled as it spells it back.
    pub catalogue: &'static [(&'static str, ArgShape)],
    /// Alias to canonical name; the right-hand side is a catalogue name.
    pub aliases: &'static [(&'static str, &'static str)],
    pub unknown: Unknown,
}

/// A base name resolved against a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The canonical catalogue name.
    pub base: String,
    pub shape: ArgShape,
}

impl TypeTable {
    /// Joins the words of a base name with single spaces. `ColumnType` squeezes
    /// whitespace inside arguments, but a multi-word name such as
    /// `double  precision` can still carry runs of spaces.
    #[must_use]
    pub fn squeeze(base: &str) -> String {
        base.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The canonical name of an alias, or the name itself when it is not one.
    /// Expects a squeezed name.
    #[must_use]
    pub fn alias<'a>(&'a self, base: &'a str) -> &'a str {
        self.aliases
            .iter()
            .find(|(alias, _)| *alias == base)
            .map_or(base, |(_, canonical)| *canonical)
    }

    /// Looks a canonical name up. A name the catalogue does not hold is
    /// refused, or mapped by the table's rule.
    pub fn shape_of(&self, ty: &ColumnType, base: &str) -> Result<Resolved, DialectError> {
        let held = |name: &str| {
            self.catalogue
                .iter()
                .find(|(held, _)| *held == name)
                .map(|(_, shape)| *shape)
        };
        if let Some(shape) = held(base) {
            return Ok(Resolved {
                base: base.to_owned(),
                shape,
            });
        }
        if let Unknown::Map(rule) = self.unknown {
            let mapped = rule(base);
            // A rule that names a row the catalogue lacks is a table bug, and
            // is refused as unknown rather than passed through to a plan.
            if let Some(shape) = held(mapped) {
                return Ok(Resolved {
                    base: mapped.to_owned(),
                    shape,
                });
            }
        }
        Err(DialectError::UnknownType {
            dialect: self.dialect,
            ty: ty.raw().to_string(),
        })
    }

    /// Squeeze, alias and shape lookup, in that order.
    pub fn lookup(&self, ty: &ColumnType) -> Result<Resolved, DialectError> {
        let squeezed = Self::squeeze(&ty.base);
        self.shape_of(ty, self.alias(&squeezed))
    }

    /// The arguments a declaration reads back with: omitted ones filled as the
    /// row says, every one range-checked.
    pub fn fill(&self, ty: &ColumnType, found: &Resolved) -> Result<Vec<TypeArg>, DialectError> {
        let base = found.base.as_str();
        let arity = |detail: String| DialectError::BadTypeArity {
            dialect: self.dialect,
            ty: ty.raw().to_string(),
            detail,
        };
        let int = |n: i64| vec![TypeArg::Int(n)];
        let filled = |omitted: Omitted| match omitted {
            Omitted::Empty => Vec::new(),
            Omitted::Fill(n) => int(n),
        };
        match found.shape {
            ArgShape::None => {
                if !ty.args.is_empty() {
                    return Err(arity(format!("`{base}` takes no arguments")));
                }
                Ok(Vec::new())
            }
            ArgShape::Engine => Ok(ty.args.clone()),
            ArgShape::Length {
                max,
                omitted,
                max_word,
            } => {
                let one_length = || {
                    arity(match max_word {
                        MaxWord::Absent => {
                            format!("`{base}` takes one length, or none for the unbounded form")
                        }
                        MaxWord::Allowed | MaxWord::Refused => format!("`{base}` takes one length"),
                    })
                };
                match ty.args.as_slice() {
                    [] => Ok(filled(omitted)),
                    [TypeArg::Max] => match max_word {
                        MaxWord::Allowed => Ok(vec![TypeArg::Max]),
                        MaxWord::Refused => Err(arity(format!("`{base}` has no `max` form"))),
                        MaxWord::Absent => Err(one_length()),
                    },
                    [TypeArg::Int(n)] if (1..=max).contains(n) => Ok(int(*n)),
                    [TypeArg::Int(n)] => Err(arity(format!(
                        "the length must be between 1 and {max}, got {n}"
                    ))),
                    _ => Err(one_length()),
                }
            }
            ArgShape::Precision {
                max,
                omitted,
                excess,
            } => match ty.args.as_slice() {
                [] => Ok(filled(omitted)),
                [TypeArg::Int(n)] if (0..=max).contains(n) => Ok(int(*n)),
                [TypeArg::Int(n)] => Err(arity(match excess {
                    Excess::Refused => {
                        format!("the precision must be between 0 and {max}, got {n}")
                    }
                    Excess::Clamped => format!(
                        "the precision of `{base}` must be between 0 and \
                         {max}, got {n}. The engine does not refuse a larger \
                         one -- it stores precision {max} and says nothing, \
                         so a declaration that kept it would read back as something else"
                    ),
                })),
                _ => Err(arity(format!("`{base}` takes one precision"))),
            },
            ArgShape::Scale {
                max_precision,
                scale,
                omitted,
            } => {
                let check = |p: i64, s: i64| {
                    if !(1..=max_precision).contains(&p) {
                        return Err(arity(format!(
                            "the precision of `{base}` must be between 1 and {max_precision}, \
                             got {p}"
                        )));
                    }
                    match scale {
                        ScaleRange::Signed(max) if !(-max..=max).contains(&s) => {
                            Err(arity(format!(
                                "the scale of `{base}` must be between -{max} and {max}, got {s}"
                            )))
                        }
                        ScaleRange::UpToPrecision if s < 0 || s > p => Err(arity(format!(
                            "the scale of `{base}` must be between 0 and the precision \
                             ({p}), got {s}"
                        ))),
                        ScaleRange::Signed(_) | ScaleRange::UpToPrecision => {
                            Ok(vec![TypeArg::Int(p), TypeArg::Int(s)])
                        }
                    }
                };
                match ty.args.as_slice() {
                    [] => Ok(match omitted {
                        Omitted::Empty => Vec::new(),
                        Omitted::Fill(p) => vec![TypeArg::Int(p), TypeArg::Int(0)],
                    }),
                    [TypeArg::Int(p)] => check(*p, 0),
                    [TypeArg::Int(p), TypeArg::Int(s)] => check(*p, *s),
                    _ => Err(arity(match omitted {
                        Omitted::Empty => format!(
                            "`{base}` takes a precision and an optional scale, or nothing at all"
                        ),
                        Omitted::Fill(_) => {
                            format!("`{base}` takes a precision and an optional scale")
                        }
                    })),
                }
            }
        }
    }

    /// The whole skeleton for an engine with no rule of its own between the
    /// steps: lookup, then fill. The result carries no argument position, so a
    /// type that needs one is the engine's to build.
    pub fn normalize(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
        let found = self.lookup(ty)?;
        let args = self.fill(ty, &found)?;
        Ok(ColumnType::new(found.base, args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUND: Omitted = Omitted::Fill(1);

    const CATALOGUE: &[(&str, ArgShape)] = &[
        ("flag", ArgShape::None),
        (
            "short text",
            ArgShape::Length {
                max: 255,
                omitted: ROUND,
                max_word: MaxWord::Refused,
            },
        ),
        (
            "long text",
            ArgShape::Length {
                max: 255,
                omitted: Omitted::Empty,
                max_word: MaxWord::Allowed,
            },
        ),
        (
            "bare text",
            ArgShape::Length {
                max: 255,
                omitted: Omitted::Empty,
                max_word: MaxWord::Absent,
            },
        ),
        (
            "stamp",
            ArgShape::Precision {
                max: 6,
                omitted: Omitted::Fill(6),
                excess: Excess::Refused,
            },
        ),
        (
            "moment",
            ArgShape::Precision {
                max: 6,
                omitted: Omitted::Empty,
                excess: Excess::Clamped,
            },
        ),
        (
            "fixed",
            ArgShape::Scale {
                max_precision: 38,
                scale: ScaleRange::UpToPrecision,
                omitted: Omitted::Fill(18),
            },
        ),
        (
            "loose",
            ArgShape::Scale {
                max_precision: 1000,
                scale: ScaleRange::Signed(1000),
                omitted: Omitted::Empty,
            },
        ),
        ("approx", ArgShape::Engine),
    ];

    const ALIASES: &[(&str, &str)] = &[
        ("boolean", "flag"),
        ("national text", "long text"),
        ("dec", "fixed"),
    ];

    const REFUSING: TypeTable = TypeTable {
        dialect: "toy",
        catalogue: CATALOGUE,
        aliases: ALIASES,
        unknown: Unknown::Refuse,
    };

    fn parse(text: &str) -> ColumnType {
        text.parse().expect("the test spellings parse")
    }

    fn normalized(table: &TypeTable, text: &str) -> String {
        table
            .normalize(&parse(text))
            .expect("the spelling normalizes")
            .shown()
            .to_string()
    }

    fn refusal(table: &TypeTable, text: &str) -> String {
        table
            .normalize(&parse(text))
            .expect_err("the spelling is refused")
            .to_string()
    }

    #[test]
    fn aliases_resolve_to_the_catalogue_name() {
        assert_eq!(normalized(&REFUSING, "boolean"), "flag");
        assert_eq!(normalized(&REFUSING, "dec"), "fixed(18, 0)");
        assert_eq!(REFUSING.alias("national text"), "long text");
        assert_eq!(REFUSING.alias("flag"), "flag");
    }

    #[test]
    fn extra_spaces_in_a_multi_word_name_do_not_hide_the_alias() {
        let ty = ColumnType::new("national    text", Vec::new());
        let found = REFUSING.lookup(&ty).expect("the alias is found");
        assert_eq!(found.base, "long text");
        assert_eq!(TypeTable::squeeze(" a   b  "), "a b");
    }

    #[test]
    fn omitted_arguments_are_filled_or_stay_empty_as_the_row_says() {
        assert_eq!(normalized(&REFUSING, "short text"), "short text(1)");
        assert_eq!(normalized(&REFUSING, "long text"), "long text");
        assert_eq!(normalized(&REFUSING, "stamp"), "stamp(6)");
        assert_eq!(normalized(&REFUSING, "moment"), "moment");
        assert_eq!(normalized(&REFUSING, "fixed"), "fixed(18, 0)");
        assert_eq!(normalized(&REFUSING, "loose"), "loose");
        assert_eq!(normalized(&REFUSING, "fixed(10)"), "fixed(10, 0)");
        assert_eq!(normalized(&REFUSING, "loose(10, -5)"), "loose(10, -5)");
    }

    #[test]
    fn max_is_kept_where_the_row_allows_it() {
        assert_eq!(normalized(&REFUSING, "long text(max)"), "long text(max)");
    }

    #[test]
    fn normalizing_is_idempotent() {
        for text in [
            "boolean",
            "dec",
            "dec(10, 2)",
            "short text",
            "long text(max)",
            "bare text(4)",
            "stamp",
            "moment(3)",
            "loose(10, -5)",
        ] {
            let once = REFUSING.normalize(&parse(text)).expect("normalizes");
            let twice = REFUSING.normalize(&once).expect("normalizes again");
            assert_eq!(once, twice, "{text}");
        }
    }

    #[test]
    fn an_unknown_spelling_is_refused_under_refuse() {
        assert_eq!(refusal(&REFUSING, "blob"), "toy has no type `blob`");
    }

    #[test]
    fn a_length_out_of_range_is_refused_with_the_bounds() {
        assert_eq!(
            refusal(&REFUSING, "short text(256)"),
            "wrong number of arguments for `short text(256)` in toy: \
             the length must be between 1 and 255, got 256"
        );
        assert!(refusal(&REFUSING, "short text(0)").contains("got 0"));
    }

    #[test]
    fn a_precision_out_of_range_is_refused_in_the_wording_of_its_excess() {
        assert!(
            refusal(&REFUSING, "stamp(7)")
                .ends_with("in toy: the precision must be between 0 and 6, got 7")
        );
        let clamped = refusal(&REFUSING, "moment(7)");
        assert!(clamped.contains("the precision of `moment` must be between 0 and 6, got 7."));
        assert!(clamped.contains("stores precision 6 and says nothing"));
    }

    #[test]
    fn a_scale_out_of_range_is_refused_by_the_rule_of_its_row() {
        assert!(
            refusal(&REFUSING, "fixed(39)")
                .contains("the precision of `fixed` must be between 1 and 38, got 39")
        );
        assert!(
            refusal(&REFUSING, "fixed(5,6)")
                .contains("the scale of `fixed` must be between 0 and the precision (5), got 6")
        );
        assert!(refusal(&REFUSING, "fixed(5,-1)").contains("got -1"));
        assert!(
            refusal(&REFUSING, "loose(10,1001)")
                .contains("the scale of `loose` must be between -1000 and 1000, got 1001")
        );
    }

    #[test]
    fn max_is_refused_where_the_row_does_not_allow_it() {
        assert!(refusal(&REFUSING, "short text(max)").ends_with("`short text` has no `max` form"));
        assert!(
            refusal(&REFUSING, "bare text(max)")
                .ends_with("`bare text` takes one length, or none for the unbounded form")
        );
    }

    #[test]
    fn too_many_arguments_are_refused() {
        assert!(refusal(&REFUSING, "flag(1)").ends_with("`flag` takes no arguments"));
        assert!(refusal(&REFUSING, "short text(1,2)").ends_with("`short text` takes one length"));
        assert!(refusal(&REFUSING, "stamp(1,2)").ends_with("`stamp` takes one precision"));
        assert!(
            refusal(&REFUSING, "fixed(1,2,3)")
                .ends_with("`fixed` takes a precision and an optional scale")
        );
        assert!(
            refusal(&REFUSING, "loose(1,2,3)")
                .ends_with("`loose` takes a precision and an optional scale, or nothing at all")
        );
    }

    #[test]
    fn an_engine_shape_hands_its_arguments_back_unjudged() {
        let ty = parse("approx(30)");
        let found = REFUSING.lookup(&ty).expect("the row is held");
        assert_eq!(found.shape, ArgShape::Engine);
        assert_eq!(
            REFUSING.fill(&ty, &found).expect("passes through"),
            vec![TypeArg::Int(30)]
        );
    }

    fn affinity(spelling: &str) -> &'static str {
        if spelling.contains("char") {
            "long text"
        } else {
            "flag"
        }
    }

    const MAPPING: TypeTable = TypeTable {
        dialect: "toy",
        catalogue: CATALOGUE,
        aliases: ALIASES,
        unknown: Unknown::Map(affinity),
    };

    #[test]
    fn an_unknown_spelling_maps_by_the_rule_instead_of_being_refused() {
        let found = MAPPING.lookup(&parse("varchar")).expect("the rule maps it");
        assert_eq!(found.base, "long text");
        assert_eq!(normalized(&MAPPING, "whatever"), "flag");
    }

    #[test]
    fn a_known_spelling_is_not_passed_to_the_rule() {
        // The rule would send `fixed` and `dec` to `flag`, which has no
        // arguments; reaching it would drop the filled precision.
        assert_eq!(normalized(&MAPPING, "fixed"), "fixed(18, 0)");
        assert_eq!(normalized(&MAPPING, "dec"), "fixed(18, 0)");
    }

    #[test]
    fn a_rule_that_names_a_missing_row_is_refused_not_passed_through() {
        fn broken(_: &str) -> &'static str {
            "no such row"
        }
        let table = TypeTable {
            unknown: Unknown::Map(broken),
            ..MAPPING
        };
        assert_eq!(refusal(&table, "blob"), "toy has no type `blob`");
    }
}
