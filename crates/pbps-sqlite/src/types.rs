//! The SQLite type table (#2950, item 2): what a declared type is, what it
//! does, and what changing it costs.
//!
//! SQLite takes any run of words as a column's type and keeps the text as
//! written. The text decides one thing, the column's *affinity*, by five rules
//! on the letters inside it, so `point` is an integer column, `charm` a text
//! column and `uuid` a numeric one. That is the opposite of the other two
//! engines, whose catalogue says which names exist. This module therefore
//! answers three questions separately:
//!
//! - [`affinity`]: what the engine does with a name. The five rules, nothing
//!   else, tested against a real SQLite.
//! - [`standing`]: whether pbps will write the name. One function and one
//!   policy (see its documentation); everything that decides it is there.
//! - [`normalize`]: the canonical form two spellings of one type share.
//!
//! Declared lengths and precisions are kept as declared and compared as
//! declared: SQLite stores `varchar(3)` as a column that holds six characters,
//! but a declaration that says 10 and a database that says 20 are two
//! different declarations (DEC-2950.2).

use pbps_dialect::{DialectError, TypeChangeRisk};
use pbps_model::{ColumnType, TypeArg};

use crate::keywords;

pub const DIALECT: &str = "sqlite";

/// The five column affinities. `Integer` and `Numeric` store every value
/// identically; they differ in a `CAST` to the type and in whether the column
/// can be the rowid (see [`rowid_alias`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    Integer,
    Text,
    /// No conversion at all; also the affinity of an empty type.
    Blob,
    Real,
    Numeric,
}

impl Affinity {
    /// The word SQLite's documentation uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Affinity::Integer => "INTEGER",
            Affinity::Text => "TEXT",
            Affinity::Blob => "BLOB",
            Affinity::Real => "REAL",
            Affinity::Numeric => "NUMERIC",
        }
    }
}

/// The affinity SQLite gives a declared type: the first of five rules that
/// matches, on the text of the type with case ignored. A word that merely
/// *contains* a rule's letters matches it, which is where `point` (INT) and
/// `charm` (CHAR) come from.
#[must_use]
pub fn affinity(declared: &str) -> Affinity {
    let text = declared.to_ascii_lowercase();
    let has = |needle: &str| text.contains(needle);
    if has("int") {
        Affinity::Integer
    } else if has("char") || has("clob") || has("text") {
        Affinity::Text
    } else if has("blob") || text.bytes().all(|b| b.is_ascii_whitespace()) {
        // Empty means no declared type. Only ASCII white space is white space
        // to SQLite; a no-break space is a name character, so a type made of
        // one is a name and falls through to numeric.
        Affinity::Blob
    } else if has("real") || has("floa") || has("doub") {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

/// [`affinity`] of a column type, which is the affinity of its name: the
/// arguments are digits and change nothing.
#[must_use]
pub fn affinity_of(ty: &ColumnType) -> Affinity {
    affinity(&ty.base)
}

/// Whether a column of this type can be the table's rowid: the declared type
/// is exactly `integer`, no arguments, and (elsewhere) the only primary key
/// column. `int`, `bigint` and `integer(5)` are ordinary columns that accept
/// NULL (measured). A column that is not a key is no alias under either name.
#[must_use]
pub fn rowid_alias(ty: &ColumnType) -> bool {
    ty.base == "integer" && ty.args.is_empty()
}

/// The type names the SQLite documentation lists as examples of its rules.
const DOCUMENTED: [&str; 28] = [
    "int",
    "integer",
    "tinyint",
    "smallint",
    "mediumint",
    "bigint",
    "unsigned big int",
    "int2",
    "int8",
    "character",
    "varchar",
    "varying character",
    "nchar",
    "native character",
    "nvarchar",
    "text",
    "clob",
    "blob",
    "real",
    "double",
    "double precision",
    "float",
    "numeric",
    "decimal",
    "boolean",
    "date",
    "datetime",
    // The one type STRICT adds beside the five it shares with the rest.
    "any",
];

/// Names that promise behaviour SQLite does not provide, with what they
/// promise. A name is on this list when the engine it comes from creates or
/// restricts values as part of the type and SQLite does neither: numbering
/// (`serial`, `identity`, `auto_increment`, `autoincrement`), a version that
/// changes by itself (`rowversion`), a set of allowed values (`enum`). SQLite
/// accepts every one of them, stores a plain column, and numbers nothing.
/// This is about promised behaviour and not about affinity: `uuid` and
/// `json` promise nothing SQLite lacks and are only unlisted.
const PROMISES: [(&str, &str); 11] = [
    ("serial", "it promises numbering and SQLite numbers nothing"),
    (
        "bigserial",
        "it promises numbering and SQLite numbers nothing",
    ),
    (
        "smallserial",
        "it promises numbering and SQLite numbers nothing",
    ),
    (
        "serial2",
        "it promises numbering and SQLite numbers nothing",
    ),
    (
        "serial4",
        "it promises numbering and SQLite numbers nothing",
    ),
    (
        "serial8",
        "it promises numbering and SQLite numbers nothing",
    ),
    (
        "identity",
        "it promises numbering and SQLite numbers nothing",
    ),
    (
        "auto_increment",
        "it promises numbering and SQLite numbers nothing",
    ),
    (
        "autoincrement",
        "it promises numbering and SQLite numbers nothing",
    ),
    (
        "rowversion",
        "it promises a version that changes by itself and SQLite changes nothing",
    ),
    (
        "enum",
        "it promises a set of allowed values and SQLite checks none",
    ),
];

/// Spellings of one type that cannot differ in effect, written as one word
/// order: a multi-word name and the form the documentation or another engine
/// uses for it. Each pair has one affinity, is outside STRICT's six, and has
/// no rowid alias, so nothing observable depends on which is written.
/// `int` and `integer` are deliberately not here: `integer` alone can be the
/// rowid.
const ALIASES: [(&str, &str); 4] = [
    ("double precision", "double"),
    ("character varying", "varchar"),
    ("varying character", "varchar"),
    ("native character", "nchar"),
];

/// What the policy says about a type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// A name the SQLite documentation lists.
    Documented,
    /// Any other name. SQLite accepts it with the affinity the rules give it,
    /// and pbps writes it. A later layer (`doctor`, a plan note) should say
    /// what that affinity does: `uuid` has NUMERIC affinity, so all-digit text
    /// is stored as a number.
    Unlisted,
    /// A name pbps will not write, with the reason.
    Refused(&'static str),
}

/// **The policy**, in one place: whether a (canonical) type name may be
/// written. It is provisional (#2950, question (b)).
///
/// 1. A word that is a reserved keyword is refused. SQLite reads a type as a
///    run of plain words, so `int not null` is a column with a constraint and
///    `int primary key` is a primary key, neither of which the declaration
///    said.
/// 2. A name with a word on the promise list is refused (see [`PROMISES`]),
///    wherever it stands: `int auto_increment` and `int identity(1,1)` are
///    accepted by SQLite as a type whose text contains the word.
/// 3. A documented name is [`Standing::Documented`].
/// 4. Every other name is [`Standing::Unlisted`].
///
/// To close the catalogue (option A, a documented name or nothing) change the
/// last line to return [`Standing::Refused`]; the policy table in the tests
/// (`unlisted_names_are_written_with_their_rule_affinity`) is the one test
/// that then changes.
#[must_use]
pub fn standing(base: &str) -> Standing {
    for word in base.split_ascii_whitespace() {
        if keywords::is_reserved(word) {
            return Standing::Refused(
                "it is a keyword, which ends the type name and starts a constraint",
            );
        }
        if let Some(&(_, why)) = PROMISES
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(word))
        {
            return Standing::Refused(why);
        }
    }
    if DOCUMENTED
        .iter()
        .any(|name| name.eq_ignore_ascii_case(base))
    {
        Standing::Documented
    } else {
        Standing::Unlisted
    }
}

fn invalid(message: String) -> DialectError {
    DialectError::Invalid {
        dialect: DIALECT,
        message,
    }
}

/// A word SQLite reads as one plain name, measured on 3.54.1: it starts with a
/// letter, `_` or any non-ASCII character, and goes on with letters, digits,
/// `_`, `$` and non-ASCII characters (`a$b`, `naïve` and `日本語` are accepted,
/// `$a`, `1a`, `a-b`, `a#b` are not). The model's parser is narrower (ASCII
/// letters, digits and `_`), but a type built another way, a saved plan or a
/// state snapshot, is held to the engine's rule and not the parser's.
fn is_word(word: &str) -> bool {
    let mut chars = word.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || !c.is_ascii())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii())
}

/// Whether `text` is a signed number as SQLite's grammar reads one inside the
/// parentheses of a type, measured on 3.54.1: an optional `+` or `-`, then
/// optional white space (`- 1` is accepted), then decimal digits, a decimal
/// with a point (`1.5`, `.5`, `5.`), either with an exponent (`1e3`, `1E+3`,
/// `1.5e-2`), or `0x` and hex digits. Any length. Refused by the engine and
/// here: a second sign (`+-1`), `1e`, `0x`, `1.2.3`, `1_000`, a bare sign or
/// point, and digits that are not ASCII.
///
/// A comment between the sign and the digits is also valid SQLite. It is not
/// accepted here: the model splits arguments at every comma, so it cannot
/// carry a comment that holds one, and nothing writes such a type (see the
/// catalog reader's gap list in DEC-2950.2).
fn signed_number(text: &str) -> bool {
    let rest = text.strip_prefix(['+', '-']).map_or(text, |after| {
        after.trim_start_matches([' ', '\t', '\n', '\r', '\x0c'])
    });
    if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        return !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    let digits = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(at) => (&rest[..at], Some(&rest[at + 1..])),
        None => (rest, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some((whole, fraction)) => {
            digits(whole) && digits(fraction) && !(whole.is_empty() && fraction.is_empty())
        }
        None => !mantissa.is_empty() && digits(mantissa),
    };
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    mantissa_ok && exponent_ok
}

/// Whether SQLite takes this argument: a whole number, or a number the model
/// holds as text (decimals, exponents, hex, a number too large for 64 bits,
/// a space after the sign). `max` and names are syntax errors.
fn takes_argument(arg: &TypeArg) -> bool {
    match arg {
        TypeArg::Int(_) => true,
        TypeArg::Ident(text) => signed_number(text),
        TypeArg::Max => false,
    }
}

/// The canonical form of a declared type: words separated by one space, the
/// aliases of [`ALIASES`] replaced, the arguments exactly as declared. Refuses
/// what SQLite would take and mean something else by, or nothing by.
///
/// The result is idempotent, and it is the form the catalog read-back parses
/// to: the model lower-cases a type's base, SQLite returns the text as
/// written, and the two meet in this function.
///
/// Known gaps, valid SQLite this refuses, for the catalog reader to report by
/// name (DEC-2950.2): a column with no declared type; a comment inside the
/// parentheses; and, on purpose, an array spelling, a promise-list word and a
/// quoted keyword, which would say something else once written unquoted. Each
/// is pinned by a test that names it.
pub fn normalize(ty: &ColumnType) -> Result<ColumnType, DialectError> {
    if ty.type_ref().is_some() {
        return Err(invalid(format!(
            "`{}` names a user-defined type, which SQLite does not have; declare a type name",
            ty.shown()
        )));
    }
    if ty.is_array() {
        return Err(invalid(format!(
            "`{}` is an array type, which SQLite does not have; declare the element type, \
             or a separate table for the elements",
            ty.shown()
        )));
    }
    if ty.args_after_word().is_some() {
        return Err(invalid(format!(
            "`{}` puts its arguments inside the type name; SQLite takes them after the whole name",
            ty.shown()
        )));
    }
    let words: Vec<&str> = ty.base.split_ascii_whitespace().collect();
    if words.is_empty() {
        // Known gap: SQLite takes a column with no declared type (BLOB
        // affinity, no conversion) and a catalog read finds many of them, but
        // the model has no spelling for it: a type must not be empty, and the
        // empty string does not survive the saved plan's own round trip.
        return Err(invalid(format!(
            "`{}` is an empty type. SQLite accepts a column with no declared type (it keeps \
             every value as given), but a declaration cannot say so; declare `blob`",
            ty.shown()
        )));
    }
    if !words.iter().all(|w| is_word(w)) {
        return Err(invalid(format!(
            "`{}` is not a type name SQLite reads as one: words of letters, digits, `_`, `$` \
             and non-ASCII characters, not starting with a digit or `$`",
            ty.shown()
        )));
    }
    let spelled = words.join(" ");
    let base = ALIASES
        .iter()
        .find(|(from, _)| *from == spelled)
        .map_or(spelled, |(_, to)| (*to).to_owned());
    if let Standing::Refused(why) = standing(&base) {
        return Err(invalid(format!(
            "`{}` cannot be a column type here: {why}",
            ty.shown()
        )));
    }
    // SQLite takes one or two signed numbers and ignores them; a name, `max`,
    // a string or a third argument is a syntax error (measured).
    if ty.args.len() > 2 || !ty.args.iter().all(takes_argument) {
        return Err(invalid(format!(
            "`{}` has arguments SQLite does not take: at most two signed numbers",
            ty.shown()
        )));
    }
    Ok(ColumnType::new(base, ty.args.clone()))
}

/// The only types a STRICT table accepts, which it then enforces.
pub const STRICT_TYPES: [&str; 6] = ["int", "integer", "real", "text", "blob", "any"];

/// Whether a STRICT table accepts this (normalized) type: one of the six
/// names with no arguments. A declaration cannot ask for STRICT yet (the model
/// has no flag); this is the rule it will be checked against.
#[must_use]
pub fn strict_allows(ty: &ColumnType) -> bool {
    ty.args.is_empty() && STRICT_TYPES.contains(&ty.base.as_str())
}

/// What a change of declared type costs. Judged on the normalized types; one
/// that does not normalize ends the question as `Incompatible`, never carried
/// on in its declared form.
///
/// The change itself needs the table rebuilt and is refused by name until the
/// rebuild exists; this says whether the *values* move.
///
/// - Equal: `Safe`. A different length or precision with the same name:
///   `Safe`, since SQLite enforces none.
/// - **Into** the rowid alias (`int` to `integer`, `integer(5)` to `integer`,
///   `numeric` to `integer`): `Narrowing`. An `int` primary key may hold NULLs
///   and a rebuild as `integer primary key` turns each into a number (measured:
///   two NULLs became 1 and 2). The function sees types and not keys, so this
///   holds for a column that is not a key too, where no value moves; that is
///   the conservative side.
/// - **Out of** the alias (`integer` to `int`, `numeric` or `integer(5)`): the
///   ordinary storage rule below. An `integer primary key` holds no NULL and
///   keeps every value under any other name (measured, rebuilt by hand into
///   each). What does change is a later insert that omits the key: it stores
///   NULL, no longer a number. That is not a stored value and is for a plan
///   note, not this gate.
/// - Same storage behaviour (`Integer` and `Numeric` store alike; any two
///   names of one other affinity): `Safe`.
/// - Any other change of affinity rewrites stored values (a number becomes
///   text, `0012` becomes 12): `Narrowing`.
#[must_use]
pub fn change_risk(from: &ColumnType, to: &ColumnType) -> TypeChangeRisk {
    let (Ok(from), Ok(to)) = (normalize(from), normalize(to)) else {
        return TypeChangeRisk::Incompatible;
    };
    if from == to {
        return TypeChangeRisk::Safe;
    }
    if !rowid_alias(&from) && rowid_alias(&to) {
        return TypeChangeRisk::Narrowing;
    }
    // Which affinities store a value the same way.
    let storage = |t: &ColumnType| match affinity_of(t) {
        Affinity::Integer | Affinity::Numeric => Affinity::Numeric,
        Affinity::Text => Affinity::Text,
        Affinity::Blob => Affinity::Blob,
        Affinity::Real => Affinity::Real,
    };
    if storage(&from) == storage(&to) {
        TypeChangeRisk::Safe
    } else {
        TypeChangeRisk::Narrowing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Fresh;

    fn ty(text: &str) -> ColumnType {
        text.parse().expect("a parseable type")
    }

    fn mem() -> Fresh {
        Fresh::memory()
    }

    /// The documented names and the affinity SQLite's documentation gives
    /// each.
    const DOCUMENTED_AFFINITY: [(&str, Affinity); 27] = [
        ("int", Affinity::Integer),
        ("integer", Affinity::Integer),
        ("tinyint", Affinity::Integer),
        ("smallint", Affinity::Integer),
        ("mediumint", Affinity::Integer),
        ("bigint", Affinity::Integer),
        ("unsigned big int", Affinity::Integer),
        ("int2", Affinity::Integer),
        ("int8", Affinity::Integer),
        ("character", Affinity::Text),
        ("varchar", Affinity::Text),
        ("varying character", Affinity::Text),
        ("nchar", Affinity::Text),
        ("native character", Affinity::Text),
        ("nvarchar", Affinity::Text),
        ("text", Affinity::Text),
        ("clob", Affinity::Text),
        ("blob", Affinity::Blob),
        ("real", Affinity::Real),
        ("double", Affinity::Real),
        ("double precision", Affinity::Real),
        ("float", Affinity::Real),
        ("numeric", Affinity::Numeric),
        ("decimal", Affinity::Numeric),
        ("boolean", Affinity::Numeric),
        ("date", Affinity::Numeric),
        ("datetime", Affinity::Numeric),
    ];

    /// What the engine does with a declared type, observed two ways that
    /// together tell all five affinities apart. `CAST('1.5' AS t)` and
    /// `CAST('12' AS t)` give (1, 12) for INTEGER, (1.5, 12) for NUMERIC,
    /// (1.5, 12.0) for REAL, text for TEXT and bytes for BLOB. And a column
    /// of that type stores `'12'`, `5`, and `5.0` by its affinity.
    fn observed(declared: &str) -> Affinity {
        let mut fresh = mem();
        let c = fresh.connection();
        let class = |sql: &str| {
            let rows = c.query(sql, &[]).expect("cast");
            rows[0].text(0).expect("typeof").to_owned()
        };
        let a = class(&format!(
            "SELECT typeof(CAST('1.5' AS {declared})) || ':' || CAST('1.5' AS {declared})"
        ));
        let b = class(&format!(
            "SELECT typeof(CAST('12' AS {declared})) || ':' || CAST('12' AS {declared})"
        ));
        match (a.as_str(), b.as_str()) {
            ("integer:1", "integer:12") => Affinity::Integer,
            ("real:1.5", "integer:12") => Affinity::Numeric,
            ("real:1.5", "real:12.0") => Affinity::Real,
            ("text:1.5", "text:12") => Affinity::Text,
            ("blob:1.5", "blob:12") => Affinity::Blob,
            other => panic!("{declared:?} behaved as no affinity: {other:?}"),
        }
    }

    /// The type of a column storing each probe, the observable part of
    /// affinity on a real column.
    fn stored(declared: &str, strict: bool) -> Option<Vec<String>> {
        let mut fresh = mem();
        let c = fresh.connection();
        let ddl = format!(
            "CREATE TABLE t(c {declared}){}",
            if strict { " STRICT" } else { "" }
        );
        c.execute_batch(&ddl).ok()?;
        let mut out = Vec::new();
        for literal in ["'12'", "5", "5.0", "'abc'"] {
            c.execute_batch(&format!("DELETE FROM t; INSERT INTO t VALUES ({literal})"))
                .ok()?;
            let rows = c.query("SELECT typeof(c) FROM t", &[]).expect("typeof");
            out.push(rows[0].text(0).expect("text").to_owned());
        }
        Some(out)
    }

    /// The five rules, on the spellings where they surprise.
    #[test]
    fn the_five_rules_give_each_spelling_its_affinity() {
        for (declared, expected) in DOCUMENTED_AFFINITY {
            assert_eq!(affinity(declared), expected, "{declared}");
        }
        // `INT` is checked first, so these are integers though they say more.
        for declared in ["point", "floating point", "hint", "print", "interval"] {
            assert_eq!(affinity(declared), Affinity::Integer, "{declared}");
        }
        assert_eq!(affinity("charm"), Affinity::Text);
        assert_eq!(affinity("blobby"), Affinity::Blob);
        assert_eq!(affinity(""), Affinity::Blob);
        for declared in [
            "string",
            "uuid",
            "json",
            "timestamp",
            "money",
            "bit",
            "foo",
            "serial",
        ] {
            assert_eq!(affinity(declared), Affinity::Numeric, "{declared}");
        }
        assert_eq!(affinity("VARCHAR"), Affinity::Text, "case is ignored");
    }

    /// The rules against the engine, over every documented name, the
    /// surprises, the unlisted names, quoted forms and arguments: the
    /// function says what SQLite does, not what the documentation says it does.
    #[test]
    fn the_affinity_function_agrees_with_the_engine() {
        let mut corpus: Vec<String> = DOCUMENTED_AFFINITY
            .iter()
            .map(|(n, _)| (*n).to_owned())
            .collect();
        for extra in [
            "point",
            "floating point",
            "hint",
            "charm",
            "blobby",
            "string",
            "uuid",
            "json",
            "timestamp",
            "money",
            "bit",
            "bytea",
            "foo",
            "serial",
            "any",
            "varchar(10)",
            "decimal(10,5)",
            "character varying(10)",
            "VARCHAR( 10 )",
            "Integer",
            "TEXT",
            "unsigned  big   int",
            "int unsigned",
            "timestamp with time zone",
            "double(53)",
            "real(8)",
            // A no-break space is a name character to SQLite, not white
            // space: a type made only of it is a name, and numeric.
            "\u{a0}",
            "\u{a0}\u{2003}",
        ] {
            corpus.push(extra.to_owned());
        }
        for declared in &corpus {
            assert_eq!(observed(declared), affinity(declared), "{declared}");
        }
        // An empty type cannot be CAST to; a column stores it with no conversion.
        assert_eq!(
            stored("", false).expect("a column with no type"),
            ["text", "integer", "real", "text"]
        );
        // A negative control: the observation does tell the five apart.
        assert_ne!(observed("int"), observed("text"));
        assert_ne!(observed("numeric"), observed("real"));
        assert_ne!(observed("int"), observed("numeric"));
    }

    /// A column of INTEGER and of NUMERIC affinity stores every probe alike,
    /// which is the claim that makes a change between them `Safe`; REAL and
    /// TEXT do not.
    #[test]
    fn integer_and_numeric_store_alike_and_the_others_do_not() {
        assert_eq!(stored("int", false), stored("numeric", false));
        assert_ne!(stored("int", false), stored("real", false));
        assert_ne!(stored("int", false), stored("text", false));
        assert_ne!(stored("text", false), stored("blob", false));
    }

    /// The documented names are real: the engine takes each, with the affinity
    /// the documentation gives it, and the policy calls each documented.
    #[test]
    fn every_documented_name_is_accepted_documented_and_normalizes_to_itself_or_its_alias() {
        for (name, expected) in DOCUMENTED_AFFINITY {
            assert!(
                stored(name, false).is_some(),
                "{name} is accepted by the engine"
            );
            assert_eq!(standing(name), Standing::Documented, "{name}");
            assert_eq!(affinity(name), expected);
            let normalized = normalize(&ty(name)).expect(name);
            assert_eq!(affinity_of(&normalized), expected, "{name}");
        }
        assert_eq!(standing("any"), Standing::Documented);
        // Negative: a name that is only close is not documented.
        assert_eq!(standing("integers"), Standing::Unlisted);
        assert_eq!(standing("varchars"), Standing::Unlisted);
    }

    /// **The policy table.** The one place a decision about which names are
    /// written is tested. Closing the catalogue (option A) changes the
    /// `Unlisted` rows to `Refused` and nothing else.
    #[test]
    fn unlisted_names_are_written_with_their_rule_affinity() {
        use Standing::{Documented, Unlisted};
        let table: [(&str, Standing); 13] = [
            ("int", Documented),
            ("varchar", Documented),
            ("any", Documented),
            ("uuid", Unlisted),
            ("json", Unlisted),
            ("timestamp", Unlisted),
            ("money", Unlisted),
            ("bytea", Unlisted),
            ("foo", Unlisted),
            ("point", Unlisted),
            ("timestamp with time zone", Unlisted),
            ("int unsigned", Unlisted),
            ("string", Unlisted),
        ];
        for (name, expected) in table {
            assert_eq!(standing(name), expected, "{name}");
            if expected == Unlisted {
                // The affinity an unlisted name has is the rules' own, and the
                // engine agrees.
                assert_eq!(observed(name), affinity(name), "{name}");
                assert!(normalize(&ty(name)).is_ok(), "{name} is written");
            }
        }
    }

    /// The deny list and the keyword rule: each refused spelling is accepted
    /// by SQLite as a column of a plain affinity, which is why the refusal is
    /// the only guard, and the engine does not do what the name promises.
    #[test]
    fn names_that_promise_what_sqlite_does_not_do_are_refused() {
        for name in [
            "serial",
            "bigserial",
            "smallserial",
            "serial2",
            "serial4",
            "serial8",
            "identity",
            "auto_increment",
            "autoincrement",
            "rowversion",
            "enum",
            "int identity",
            "bigint unsigned auto_increment",
            "int auto_increment",
            "int identity(1,1)",
        ] {
            let parsed = ty(name);
            let error = normalize(&parsed).expect_err(name).to_string();
            assert!(
                error.contains("cannot be a column type here"),
                "{name}: {error}"
            );
            assert!(
                matches!(standing(&parsed.base), Standing::Refused(_)),
                "{name}"
            );
        }
        // The engine takes them and numbers nothing.
        for declared in ["serial", "int auto_increment", "int identity(1,1)"] {
            let mut fresh = mem();
            let c = fresh.connection();
            c.execute_batch(&format!("CREATE TABLE t(a {declared}, b)"))
                .expect("accepted");
            c.execute_batch("INSERT INTO t(b) VALUES (1), (2)")
                .expect("insert");
            let rows = c.query("SELECT count(a) FROM t", &[]).expect("count");
            assert_eq!(
                rows[0].integer(0).expect("n"),
                0,
                "{declared} numbered a row"
            );
        }
        // Negative: words that merely contain a listed word are not refused.
        for fine in ["serialize", "identity2", "enumerated", "rowversioned"] {
            assert!(normalize(&ty(fine)).is_ok(), "{fine}");
        }
    }

    /// A keyword inside a type is read by SQLite as the start of a constraint,
    /// so the declaration would say one thing and the table another.
    #[test]
    fn a_keyword_in_a_type_is_refused_because_sqlite_reads_it_as_a_constraint() {
        for name in [
            "int not null",
            "int primary key",
            "int unique",
            "int default",
            "text collate",
            "int references",
        ] {
            assert!(normalize(&ty(name)).is_err(), "{name}");
        }
        let mut fresh = mem();
        let c = fresh.connection();
        c.execute_batch("CREATE TABLE t(a int not null)")
            .expect("accepted");
        assert!(
            c.execute_batch("INSERT INTO t VALUES (NULL)").is_err(),
            "it is a NOT NULL column"
        );
        // Negative: a type with no keyword and several words is fine.
        assert!(normalize(&ty("unsigned big int")).is_ok());
    }

    /// Aliases, canonical spelling, and arguments kept as declared.
    #[test]
    fn normalizing_collapses_aliases_and_keeps_declared_lengths() {
        for (declared, canonical) in [
            ("double precision", "double"),
            ("character varying(10)", "varchar(10)"),
            ("varying character(255)", "varchar(255)"),
            ("native character(70)", "nchar(70)"),
            ("unsigned  big   int", "unsigned big int"),
            ("INTEGER", "integer"),
            ("VarChar(10)", "varchar(10)"),
            ("decimal(10, 2)", "decimal(10, 2)"),
        ] {
            assert_eq!(
                normalize(&ty(declared)).expect(declared),
                ty(canonical),
                "{declared}"
            );
        }
        // `int` and `integer` stay two types (the rowid alias), and lengths
        // are compared as declared.
        assert_ne!(
            normalize(&ty("int")).expect("int"),
            normalize(&ty("integer")).expect("integer")
        );
        assert_ne!(
            normalize(&ty("varchar(10)")).expect("v"),
            normalize(&ty("varchar(20)")).expect("v")
        );
        assert_ne!(
            normalize(&ty("varchar(10)")).expect("v"),
            normalize(&ty("text")).expect("t")
        );
        // Idempotent.
        for declared in [
            "double precision",
            "character varying(10)",
            "uuid",
            "int",
            "decimal(10,2)",
        ] {
            let once = normalize(&ty(declared)).expect(declared);
            assert_eq!(normalize(&once).expect("again"), once, "{declared}");
        }
    }

    /// What `normalize` refuses, each with a reason that names the type.
    #[test]
    fn normalizing_refuses_what_sqlite_would_misread() {
        for declared in [
            "text[]",
            "app.mood",
            "varchar(max)",
            "varchar(a)",
            "varchar(1,2,3)",
            "varchar('5')",
            "varchar(1e)",
            "varchar(0x)",
            "varchar(+-1)",
            "varchar(1.2.3)",
            "varchar(1_000)",
            "timestamp(3) with time zone",
        ] {
            let parsed: Result<ColumnType, _> = declared.parse();
            let Ok(parsed) = parsed else { continue };
            let error = normalize(&parsed).expect_err(declared);
            assert!(
                matches!(
                    error,
                    DialectError::Invalid {
                        dialect: "sqlite",
                        ..
                    }
                ),
                "{declared}: {error:?}"
            );
            assert!(
                error.to_string().contains(&parsed.shown().to_string()),
                "{declared}: {error}"
            );
        }
        // The reason names what the type is, not only that it is refused.
        for (declared, word) in [("app.mood", "user-defined"), ("text[]", "array")] {
            let error = normalize(&ty(declared)).expect_err(declared).to_string();
            assert!(error.contains(word), "{declared}: {error}");
        }
        // Types a saved plan could carry that the parser would not make.
        for base in ["1abc", "a-b", "a;b", "", "a.b", "$a", "a#b"] {
            let built = ColumnType::new(base, Vec::new());
            assert!(normalize(&built).is_err(), "{base:?}");
        }
        // Negative: the same arguments on a documented name are fine.
        assert!(normalize(&ty("varchar(10)")).is_ok());
        assert!(normalize(&ty("decimal(10,5)")).is_ok());
        assert!(normalize(&ty("int(5)")).is_ok());
    }

    /// The test the other two engines' `serial` rule exists for: a normalized
    /// type, written into a column and read back through the catalog, parses
    /// and normalizes to the same type. No phantom change on every run.
    #[test]
    fn a_normalized_type_survives_a_round_trip_through_the_catalog() {
        for declared in [
            "int",
            "integer",
            "bigint",
            "unsigned big int",
            "varchar(10)",
            "decimal(10, 2)",
            "character varying(10)",
            "double precision",
            "uuid",
            "json",
            "timestamp",
            "TEXT",
            "Integer",
            "VarChar( 10 )",
            "native character(70)",
            "int2",
            "blob",
            "any",
        ] {
            let normalized = normalize(&ty(declared)).expect(declared);
            let mut fresh = mem();
            let c = fresh.connection();
            // Concatenated, not formatted: this is the SQL the engine is
            // asked to run, and the type goes in exactly as it is written.
            c.execute_batch(&["CREATE TABLE t(a ", &normalized.raw().to_string(), ")"].concat())
                .expect(declared);
            let rows = c
                .query("SELECT type FROM pragma_table_xinfo('t')", &[])
                .expect("read back");
            let read: ColumnType = rows[0].text(0).expect("type").parse().expect("parse");
            assert_eq!(
                normalize(&read).expect("read back normalizes"),
                normalized,
                "{declared}"
            );
        }
    }

    /// STRICT's six against the engine, and the ones it refuses.
    #[test]
    fn strict_allows_exactly_what_a_strict_table_accepts() {
        for declared in [
            "int",
            "integer",
            "real",
            "text",
            "blob",
            "any",
            "INT",
            "Integer",
            "varchar(10)",
            "int(5)",
            "text(5)",
            "numeric",
            "boolean",
            "datetime",
            "double",
            "float",
            "serial",
            "unsigned int",
            "decimal(10,2)",
        ] {
            let parsed = ty(declared);
            let engine = mem()
                .connection()
                .execute_batch(
                    &["CREATE TABLE t(c ", &parsed.raw().to_string(), ") STRICT"].concat(),
                )
                .is_ok();
            assert_eq!(strict_allows(&parsed), engine, "{declared}");
        }
        assert!(strict_allows(&ty("integer")) && !strict_allows(&ty("varchar(10)")));
        assert_eq!(STRICT_TYPES.len(), 6);
    }

    /// Only `integer` with no arguments is the rowid, on the engine.
    #[test]
    fn only_a_plain_integer_primary_key_is_the_rowid() {
        for (declared, alias) in [
            ("integer", true),
            ("Integer", true),
            ("int", false),
            ("bigint", false),
            ("integer(5)", false),
            ("numeric", false),
        ] {
            assert_eq!(rowid_alias(&ty(declared)), alias, "{declared}");
            let mut fresh = mem();
            let c = fresh.connection();
            c.execute_batch(&format!("CREATE TABLE t(a {declared} PRIMARY KEY, b)"))
                .expect(declared);
            c.execute_batch("INSERT INTO t(a, b) VALUES (NULL, 1)")
                .expect("null key");
            let rows = c.query("SELECT count(a) FROM t", &[]).expect("count");
            // A rowid takes the NULL as a request for a number; any other
            // column keeps it.
            assert_eq!(
                rows[0].integer(0).expect("n"),
                i64::from(alias),
                "{declared}"
            );
        }
    }

    /// Forms for the parentheses of a type, with what SQLite 3.54.1 did with
    /// each (measured). `true` means `CREATE TABLE t(a varchar(<form>))` works.
    const ARGUMENT_FORMS: [(&str, bool); 47] = [
        ("10", true),
        ("0", true),
        ("-0", true),
        ("-1", true),
        ("+5", true),
        (" 5 ", true),
        ("1,2", true),
        ("-1,-2", true),
        ("1.5", true),
        ("-1.5", true),
        (".5", true),
        ("5.", true),
        ("5.e3", true),
        ("1e3", true),
        ("1E3", true),
        ("1E+3", true),
        ("1e-2", true),
        ("+1.5e+2", true),
        (".5e1", true),
        ("0x10", true),
        ("0X10", true),
        ("-0x10", true),
        ("00012", true),
        ("1e0003", true),
        ("99999999999999999999", true),
        ("-9223372036854775809", true),
        ("1.5,-2.5", true),
        ("- 1", true),
        ("+ 5", true),
        ("-  1.5", true),
        ("- 0x10", true),
        ("1,2,3", false),
        ("a", false),
        ("max", false),
        ("'5'", false),
        ("NULL", false),
        ("1_000", false),
        ("1 2", false),
        ("--1", false),
        ("+-1", false),
        ("0x", false),
        ("1e", false),
        ("1.2.3", false),
        ("1e+", false),
        (".", false),
        ("+", false),
        ("-", false),
    ];

    /// Type arguments, against the engine in both directions: a form SQLite
    /// accepts is accepted here and one it refuses is refused. The model
    /// carries every accepted form (a whole number as `Int`, anything else as
    /// text), so nothing the engine takes is left out, and what each form
    /// reads back as parses to the type that was written.
    #[test]
    fn arguments_are_accepted_exactly_when_the_engine_accepts_them() {
        for (form, engine_accepts) in ARGUMENT_FORMS {
            let declared = ["varchar(", form, ")"].concat();
            let engine = mem()
                .connection()
                .execute_batch(&["CREATE TABLE t(a ", &declared, ")"].concat())
                .is_ok();
            assert_eq!(engine, engine_accepts, "the engine on {declared}");
            let parsed: ColumnType = declared.parse().expect("the model carries the form");
            assert_eq!(
                normalize(&parsed).is_ok(),
                engine,
                "normalize on {declared}"
            );
            if engine {
                // Written back in the model's own spelling, read back, parsed.
                let normalized = normalize(&parsed).expect("accepted");
                let mut fresh = mem();
                let c = fresh.connection();
                c.execute_batch(
                    &["CREATE TABLE t(a ", &normalized.raw().to_string(), ")"].concat(),
                )
                .expect("the model's spelling is valid SQL");
                let rows = c
                    .query("SELECT type FROM pragma_table_xinfo('t')", &[])
                    .expect("read back");
                let read: ColumnType = rows[0].text(0).expect("type").parse().expect("parse");
                assert_eq!(
                    normalize(&read).expect("normalizes"),
                    normalized,
                    "{declared}"
                );
            }
        }
        // Negative: a third argument and `max` are refused by both, and `Int`
        // with a sign is a whole number the model carries.
        assert!(normalize(&ty("varchar(-1)")).is_ok());
        assert!(normalize(&ty("decimal(10,-2)")).is_ok());
        assert!(normalize(&ty("varchar(1,2,3)")).is_err());
        assert!(normalize(&ty("varchar(max)")).is_err());
    }

    /// A comment inside the parentheses is valid SQLite and is refused here:
    /// the model splits arguments at every comma, so it cannot carry one that
    /// holds a comma, and nothing writes such a type. Known gap for the
    /// catalog reader (item 4), which reports such a column by name.
    #[test]
    fn a_comment_inside_the_parentheses_is_a_known_gap() {
        for form in ["1/**/", "-/**/1"] {
            let declared = ["varchar(", form, ")"].concat();
            assert!(
                mem()
                    .connection()
                    .execute_batch(&["CREATE TABLE t(a ", &declared, ")"].concat())
                    .is_ok(),
                "the engine accepts {declared}"
            );
            assert!(normalize(&ty(&declared)).is_err(), "{declared}");
        }
    }

    /// Valid SQLite that is refused on purpose, because written unquoted it
    /// would say something else: an array spelling, a promise-list word and a
    /// quoted keyword. The engine takes all three and an existing database can
    /// carry them, so they are named here as gaps for the catalog reader to
    /// report, not left to look like oversights.
    #[test]
    fn valid_sqlite_that_is_refused_on_purpose_is_named() {
        for (declared, read_back) in [
            ("text[]", "text[]"),
            ("int auto_increment", "int auto_increment"),
            ("\"not null\"", "not null"),
        ] {
            let mut fresh = mem();
            let c = fresh.connection();
            c.execute_batch(&["CREATE TABLE t(a ", declared, ")"].concat())
                .expect("the engine accepts it");
            let rows = c
                .query("SELECT type FROM pragma_table_xinfo('t')", &[])
                .expect("type");
            assert_eq!(rows[0].text(0).expect("type"), read_back);
            let parsed: ColumnType = read_back.parse().expect("the model parses the read-back");
            assert!(normalize(&parsed).is_err(), "{declared}");
        }
    }

    /// Type words, against the engine: SQLite takes letters, digits, `_`, `$`
    /// and every non-ASCII character in a word, and not a leading digit or
    /// `$`. A type built outside the parser (a saved plan, the catalog
    /// reader) is held to that, not to the parser's narrower rule.
    #[test]
    fn type_words_are_accepted_exactly_when_the_engine_accepts_them() {
        for word in [
            "a$",
            "a$b$",
            "é1",
            "a\u{a0}b",
            "_1",
            "日本",
            "Ωmega",
            "a\u{2003}b",
            "x\u{200b}y",
            "tYpe_1",
            "$a",
            "9",
            "1a",
            "a#b",
            "a@b",
            "a:b",
            "a?b",
            "a-b",
            "a;b",
            "a.b",
        ] {
            let engine = mem()
                .connection()
                .execute_batch(&["CREATE TABLE t(a ", word, ")"].concat())
                .is_ok();
            let built = ColumnType::new(word, Vec::new());
            assert_eq!(normalize(&built).is_ok(), engine, "{word:?}");
        }
        // The same words with a space between them are two words, and a
        // non-ASCII space is part of one word, as the engine reads it.
        assert!(normalize(&ColumnType::new("a$b c1", Vec::new())).is_ok());
        assert_eq!(
            normalize(&ColumnType::new("a\u{a0}b", Vec::new()))
                .expect("one word")
                .base,
            "a\u{a0}b"
        );
    }

    /// A column with no declared type is valid SQLite and common in existing
    /// databases (BLOB affinity). The model cannot hold the empty type: its
    /// parser refuses it and a saved plan's own round trip would fail. Known
    /// gap for the catalog reader, which reports such a column by name;
    /// `normalize` says so instead of calling it a bad name.
    #[test]
    fn a_column_with_no_declared_type_is_a_known_gap() {
        let mut fresh = mem();
        let c = fresh.connection();
        c.execute_batch("CREATE TABLE t(a, b int)")
            .expect("a typeless column");
        let rows = c
            .query("SELECT type FROM pragma_table_xinfo('t') ORDER BY cid", &[])
            .expect("types");
        assert_eq!(rows[0].text(0).expect("empty text"), "");
        assert_eq!(affinity(""), Affinity::Blob);
        assert!("".parse::<ColumnType>().is_err(), "the parser refuses it");
        let built = ColumnType::new("", Vec::new());
        assert!(
            ColumnType::try_from(String::from(built.clone())).is_err(),
            "and a serialized one could not be read back"
        );
        let error = normalize(&built).expect_err("empty").to_string();
        assert!(
            error.contains("empty type") && error.contains("no declared type"),
            "{error}"
        );
    }

    /// What a change of declared type costs, with the engine's facts behind
    /// each class.
    #[test]
    fn a_type_change_costs_what_it_does_to_stored_values() {
        use TypeChangeRisk::{Incompatible, Narrowing, Safe};
        let risk = |from: &str, to: &str| change_risk(&ty(from), &ty(to));
        assert_eq!(risk("int", "int"), Safe);
        assert_eq!(
            risk("varchar(10)", "varchar(20)"),
            Safe,
            "no length is enforced"
        );
        assert_eq!(risk("varchar(10)", "text"), Safe, "one affinity");
        assert_eq!(
            risk("character varying(10)", "varchar(10)"),
            Safe,
            "one canonical type"
        );
        assert_eq!(risk("int", "bigint"), Safe);
        assert_eq!(
            risk("int", "numeric"),
            Safe,
            "INTEGER and NUMERIC store alike"
        );
        // Into the rowid alias: stored NULLs would become numbers.
        for from in ["int", "bigint", "numeric", "integer(5)"] {
            assert_eq!(
                risk(from, "integer"),
                Narrowing,
                "{from} into the rowid alias"
            );
        }
        // Out of it: no value moves, so the ordinary storage rule applies.
        for to in ["int", "bigint", "numeric", "integer(5)"] {
            assert_eq!(
                risk("integer", to),
                Safe,
                "integer out of the rowid alias to {to}"
            );
        }
        assert_eq!(
            risk("integer", "text"),
            Narrowing,
            "out of the alias, but a number becomes text"
        );
        assert_eq!(
            risk("integer", "real"),
            Narrowing,
            "out of the alias, but a number becomes real"
        );
        assert_eq!(risk("int", "text"), Narrowing, "a number becomes text");
        assert_eq!(risk("text", "int"), Narrowing, "`0012` becomes 12");
        assert_eq!(risk("real", "numeric"), Narrowing);
        assert_eq!(risk("numeric", "real"), Narrowing);
        assert_eq!(risk("text", "blob"), Narrowing);
        // A type that does not normalize is never `Safe`, in either position.
        assert_eq!(risk("serial", "int"), Incompatible);
        assert_eq!(risk("int", "serial"), Incompatible);
        assert_eq!(
            risk("serial", "serial"),
            Incompatible,
            "equal is not enough"
        );
        assert_eq!(risk("varchar(10)", "text[]"), Incompatible);
    }

    /// The rowid claim, rebuilt by hand on the engine in both directions, for a
    /// key column and a column that is not a key: create the new table, copy,
    /// compare values, storage classes and NULL counts.
    #[test]
    fn a_rebuild_into_the_rowid_changes_values_and_out_of_it_does_not() {
        fn count(c: &pbps_db::sqlite::Connection, sql: &str) -> i64 {
            c.query(sql, &[]).expect("count")[0].integer(0).expect("n")
        }
        let identical = |c: &pbps_db::sqlite::Connection| {
            count(
                c,
                "SELECT count(*) FROM (SELECT a, typeof(a), b FROM old EXCEPT SELECT a, typeof(a), b FROM new)",
            ) == 0
                && count(
                    c,
                    "SELECT count(*) FROM (SELECT a, typeof(a), b FROM new EXCEPT SELECT a, typeof(a), b FROM old)",
                ) == 0
        };
        // Into the alias: an `int` key holding NULLs.
        let mut fresh = mem();
        let c = fresh.connection();
        c.execute_batch(
            "CREATE TABLE old(a int PRIMARY KEY, b); \
             INSERT INTO old VALUES (NULL,'n1'),(NULL,'n2'),(7,'k7'); \
             CREATE TABLE new(a integer PRIMARY KEY, b); INSERT INTO new SELECT a, b FROM old;",
        )
        .expect("rebuild into the alias");
        assert_eq!(count(c, "SELECT count(*) FROM old WHERE a IS NULL"), 2);
        assert_eq!(
            count(c, "SELECT count(*) FROM new WHERE a IS NULL"),
            0,
            "the NULLs became numbers"
        );
        assert!(!identical(c));
        // Out of the alias, key column: no NULL to lose, every value and class kept.
        for target in ["int", "numeric", "integer(5)", "bigint"] {
            let mut fresh = mem();
            let c = fresh.connection();
            c.execute_batch(
                &[
                    "CREATE TABLE old(a integer PRIMARY KEY, b); \
                     INSERT INTO old VALUES (1,'x'),(5,'y'),(NULL,'z'); \
                     CREATE TABLE new(a ",
                    target,
                    " PRIMARY KEY, b); INSERT INTO new SELECT a, b FROM old;",
                ]
                .concat(),
            )
            .expect("rebuild out of the alias");
            assert!(identical(c), "integer key to {target} key");
            assert_eq!(
                count(c, "SELECT count(*) FROM new WHERE a IS NULL"),
                0,
                "{target}"
            );
        }
        // Non-key columns, either direction: NULLs, numbers and text all kept.
        for (from, to) in [
            ("integer", "int"),
            ("int", "integer"),
            ("integer", "numeric"),
            ("numeric", "integer"),
        ] {
            let mut fresh = mem();
            let c = fresh.connection();
            c.execute_batch(
                &[
                    "CREATE TABLE old(a ",
                    from,
                    ", b); INSERT INTO old VALUES (NULL,'n'),(3,'t'),('x','s'),(2.5,'f'); CREATE TABLE new(a ",
                    to,
                    ", b); INSERT INTO new SELECT a, b FROM old;",
                ]
                .concat(),
            )
            .expect("rebuild a non-key column");
            assert!(identical(c), "non-key {from} to {to}");
            assert_eq!(
                count(c, "SELECT count(*) FROM new WHERE a IS NULL"),
                1,
                "{from} to {to}"
            );
        }
        // The one thing that does change out of the alias: an insert that omits the key.
        let omitted = |declared: &str| {
            let mut fresh = mem();
            let c = fresh.connection();
            c.execute_batch(
                &[
                    "CREATE TABLE t(a ",
                    declared,
                    " PRIMARY KEY, b); INSERT INTO t(b) VALUES ('x');",
                ]
                .concat(),
            )
            .expect("insert");
            count(c, "SELECT count(a) FROM t")
        };
        assert_eq!((omitted("integer"), omitted("int")), (1, 0));
    }
}
