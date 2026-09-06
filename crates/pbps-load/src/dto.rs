//! The YAML shape of the declaration files.
//!
//! These structures are deliberately **different** from [`pbps_model`]'s domain
//! model: YAML uses map keys as names, has defaults, and carries one-shot intent
//! annotations such as `renamed_from`. [`crate::convert`] owns the translation,
//! which is what keeps the model clean (see constraints 1 and 2 in CLAUDE.md).
//!
//! Every structure sets `deny_unknown_fields`: a misspelled field name is the
//! most common user error, and silently applying a default would let the
//! declaration and reality diverge without a sound.

use indexmap::IndexMap;
use serde_saphyr::Spanned;
use std::collections::BTreeMap;

use pbps_model::ReferentialAction;

/// Which kind of declaration a file holds.
///
/// Read first, with unknown fields deliberately **allowed**: this pass only
/// answers "table or module", and the real DTO for that answer is what rejects
/// a misspelled field — with a message naming the right shape rather than
/// "unknown field `colunms`" from whichever variant happened to be tried first.
#[derive(Debug, serde::Deserialize)]
pub struct KindProbe {
    #[serde(default)]
    pub table: Option<String>,
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub procedure: Option<String>,
    #[serde(default)]
    pub function: Option<String>,
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
}

/// One database role and its grants (ADR-0005).
///
/// `role: app_reader` names it — unqualified, because a role is a principal
/// and not an object in a schema. Grants are a map of target to permission
/// list: `dbo.customer: [select]`, or `schema::dbo: [select]` for a whole
/// schema.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoleDto {
    #[schemars(with = "String")]
    pub role: Spanned<String>,

    #[serde(default)]
    pub description: Option<String>,

    /// One-shot intent, as for [`TableDto::renamed_from`]: the role keeps its
    /// membership through the rename, which is why it is intent and not
    /// drop + add.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub renamed_from: Option<Spanned<String>>,

    /// Target to permissions. Order carries no meaning; `fmt` sorts both.
    #[serde(default)]
    #[schemars(schema_with = "grants_schema")]
    pub grants: BTreeMap<String, Vec<Spanned<String>>>,
}

/// The `grants` map as the editor sees it: any target, and a permission list
/// over the words the loader accepts. Built from `Permission::ALL` rather than
/// derived on `Permission`, which the model keeps free of `JsonSchema` (see
/// `ReferentialAction`, the one exception); listing the words here by hand
/// would recreate the drift the generation exists to prevent. The list is the
/// union of the engines' (ADR-0010 §6) — which word a dialect refuses is
/// `validate`'s finding, not the editor's.
///
/// Two branches, because the schema has two jobs and one list cannot do both.
/// `Permission::from_str` folds case and reads `_` or a space where the
/// canonical word has `-`, so that a user who types what the engine prints
/// (`VIEW DEFINITION`, `view_definition`) is not corrected. A closed list of
/// the canonical words alone would have an editor flag those, which the loader
/// accepts — so the pattern branch says what is accepted, and the `enum`
/// branch, listing only what `fmt` writes, is what an editor completes from
/// (DECISIONS 213).
fn grants_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let words: Vec<&str> = pbps_model::Permission::ALL
        .iter()
        .map(|p| p.as_str())
        .collect();
    let padding = whitespace_class();
    let accepted = format!(
        "^{padding}*({}){padding}*$",
        words
            .iter()
            .map(|w| spellings_of(w))
            .collect::<Vec<_>>()
            .join("|")
    );
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": {
            "type": "array",
            "items": {
                "type": "string",
                "anyOf": [
                    { "enum": words },
                    { "pattern": accepted }
                ]
            }
        }
    })
}

/// The characters `str::trim` removes, as a regular-expression class.
///
/// Not `\s`. JSON Schema's patterns are ECMA-262, whose `\s` is neither a
/// subset nor a superset of Rust's `char::is_whitespace`: it leaves out U+0085,
/// which `trim` removes, and takes in U+FEFF, which `trim` leaves in place. A
/// pattern written with `\s` would therefore refuse a padded declaration the
/// loader accepts *and* accept one it refuses — both halves of "the schema is
/// the loader" broken by the same two characters (DECISIONS 172, 213).
///
/// Scanned out of `char::is_whitespace` rather than listed, so the class is
/// whatever `trim` does on the compiler that built this binary and cannot go
/// stale against a later Unicode table.
///
/// The characters go in literally rather than as escapes, because the two
/// readers of this pattern share no escape syntax: ECMA-262 spells U+1680
/// `\u1680` and has no `\x{...}`, the `regex` family spells it `\x{1680}` and
/// has no `\u`. A literal character is one both read, and JSON's own escaping
/// keeps the ASCII control characters printable in the published file. None of
/// the set is a regular-expression metacharacter, so none needs escaping
/// inside the class.
fn whitespace_class() -> String {
    let mut class = String::from("[");
    class.extend((char::MIN..=char::MAX).filter(|c| c.is_whitespace()));
    class.push(']');
    class
}

/// One permission word as a regular expression matching every spelling
/// `Permission::from_str` folds into it.
///
/// Written out per character rather than with a flag: JSON Schema's patterns
/// are ECMA-262, which has no inline `(?i)`, and a schema that carried one
/// would be a pattern every validator reads differently. `-` admits the two
/// separators the fold also accepts, and the leading and trailing whitespace
/// class the caller wraps this in stands for its `trim`.
fn spellings_of(word: &str) -> String {
    word.chars()
        .map(|c| match c {
            '-' => "[-_ ]".to_owned(),
            c if c.is_ascii_alphabetic() => {
                format!("[{}{}]", c.to_ascii_uppercase(), c)
            }
            // No word has one today — `every_permission_word_is_lowercase_ascii`
            // holds the assumption — and a character class is the spelling that
            // stays literal for the widest set of characters if one appears.
            c => format!("[{c}]"),
        })
        .collect()
}

/// One view, procedure, function or trigger (ADR-0002).
///
/// The leading key is both the kind and the name — `view: dbo.active_customer`
/// — for the same reason `table:` is: the file name carries no meaning, so the
/// identity has to be inside the file.
// The four kind keys are individually optional in Rust because serde has to
// read the file before it can say which one is present — but the loader
// requires exactly one, and a schema that did not say so would bless a file
// with none, or with two, that `convert_module` then refuses. The whole point
// of generating the schema is that it accepts exactly what the loader accepts,
// so the constraint is stated here rather than left to the derive.
//
// Each branch pins the key's *type* as well as its presence: JSON Schema's
// `required` is satisfied by an explicit null, which YAML writes as often as
// not (`view:` with nothing after it), and the loader would read that as
// absent.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
// Each branch pins the kind key's *type* as well as its presence: JSON Schema's
// `required` is satisfied by an explicit null, and YAML writes `view:` with
// nothing after it as often as not — which the loader reads as absent.
//
// `on:` is part of the same choice, not an independent field. The loader
// requires it on a trigger and rejects it on everything else, so a schema that
// left it optional everywhere blessed two documents `pbps validate` refuses: a
// trigger without a table, and a view with one. The principle this schema is
// generated for is that it refuses exactly what the loader refuses.
//
// Every branch pins the other three kind keys — and `on:` — to null, rather
// than leaving them unconstrained. `oneOf` means *exactly one* branch matches,
// so before the `on:` rule existed a two-kind document matched two branches and
// was refused for free. Adding `on: false` broke that: `trigger` + `on` +
// `view` was disqualified from the view branch by `on` and unconstrained in the
// trigger branch, so it matched exactly one and the schema blessed a file
// `convert_module` refuses. The exclusions have to be written out.
//
// `{"type": "null"}` rather than `false`, because that is the loader's own rule:
// a key counts as present only when it holds a string, and YAML writes
// `procedure:` with nothing after it — which serde reads as absent. `false`
// would refuse that line, making the schema stricter than the tool, which is a
// smaller failure than blessing what the tool refuses but a failure all the
// same.
#[schemars(extend("oneOf" = [
    serde_json::json!({
        "required": ["view"],
        "properties": {
            "view": {"type": "string"},
            "procedure": {"type": "null"}, "function": {"type": "null"},
            "trigger": {"type": "null"}, "on": {"type": "null"},
        },
    }),
    serde_json::json!({
        "required": ["procedure"],
        "properties": {
            "procedure": {"type": "string"},
            "view": {"type": "null"}, "function": {"type": "null"},
            "trigger": {"type": "null"}, "on": {"type": "null"},
        },
    }),
    serde_json::json!({
        "required": ["function"],
        "properties": {
            "function": {"type": "string"},
            "view": {"type": "null"}, "procedure": {"type": "null"},
            "trigger": {"type": "null"}, "on": {"type": "null"},
        },
    }),
    serde_json::json!({
        "required": ["trigger", "on"],
        "properties": {
            "trigger": {"type": "string"}, "on": {"type": "string"},
            "view": {"type": "null"}, "procedure": {"type": "null"},
            "function": {"type": "null"},
        },
    }),
]))]
pub struct ModuleDto {
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub view: Option<Spanned<String>>,
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub procedure: Option<Spanned<String>>,
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub function: Option<Spanned<String>>,
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub trigger: Option<Spanned<String>>,

    #[serde(default)]
    pub description: Option<String>,

    /// The table a trigger is on. Meaningless — and rejected — on anything else.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub on: Option<Spanned<String>>,

    /// The body. What it starts with depends on the kind; see
    /// [`pbps_model::Module`].
    pub definition: String,

    /// Modules that must be created first, where the identifier scan cannot see
    /// the dependency. Persistent, like `strategy:`, and preserved by `fmt`.
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub depends_on: Vec<Spanned<String>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TableDto {
    #[schemars(with = "String")]
    pub table: Spanned<String>,

    #[serde(default)]
    pub description: Option<String>,

    /// One-shot intent: the name this table was renamed from. Removed from the
    /// file once `pbps plan` has absorbed it into the identity file.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub renamed_from: Option<Spanned<String>>,

    /// Execution hints (ADR-0003). Persistent, unlike `renamed_from`: `fmt`
    /// keeps it.
    #[serde(default)]
    pub strategy: Option<StrategyDto>,

    /// `IndexMap` preserves the declaration order in the document, which decides
    /// the column layout of `CREATE TABLE`.
    pub columns: IndexMap<String, ColumnDto>,

    #[serde(default)]
    pub primary_key: Option<PrimaryKeyDto>,

    #[serde(default)]
    pub unique: BTreeMap<String, Vec<String>>,

    #[serde(default)]
    pub foreign_keys: BTreeMap<String, ForeignKeyDto>,

    /// Constraint name to check expression.
    #[serde(default)]
    pub checks: BTreeMap<String, String>,

    #[serde(default)]
    pub indexes: BTreeMap<String, IndexDto>,

    /// Declared reference data (ADR-0004). Absent on almost every table: it is
    /// the opt-in that lets the tool touch rows at all.
    #[serde(default)]
    pub data: Option<DataDto>,
}

/// The `data:` block.
///
/// `mode` is required rather than defaulted. The two modes differ by whether a
/// row the declaration does not mention is deleted, and a default would decide
/// that for the user in the file where it is least visible.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DataDto {
    #[schemars(with = "String")]
    pub mode: Spanned<String>,

    /// Primary-key value to the row's other columns.
    ///
    /// The key is read as a string whatever the YAML scalar looked like, which
    /// is why an integer key (`1:`) works: the primary-key column's declared
    /// type is what says how to render it, and it says so in one place
    /// (`pbps_model::RowKey`).
    pub rows: BTreeMap<String, BTreeMap<String, ValueDto>>,
}

/// One cell, as written: `null`, a boolean, an integer, or text. A decimal
/// must be quoted (`'1.50'`) — see `pbps_model::Value` for why.
///
/// The variants are tried in order and the order is the whole design. `Text`
/// is last so that it catches only what nothing else did; `Float` sits ahead
/// of it so that a bare `1.50` is *caught and refused* rather than silently
/// becoming the string `"1.50"` in some YAML parsers and the number 1.5 in
/// others. `convert` turns that arm into a diagnostic asking for quotes.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ValueDto {
    Null,
    Bool(bool),
    Int(i64),
    /// Exists to be refused with a good message. **Kept out of the published
    /// schema**: an editor validating against it would otherwise accept a
    /// bare `1.50` that every `pbps` command then rejects, and the whole point
    /// of generating the schema from these types is that the two agree.
    #[schemars(skip)]
    Float(f64),
    Text(String),
}

/// `deny_unknown_fields` is what turns a typo into an error instead of a
/// silent no-op — the whole point of ADR-0003's "validate rejects unknown keys".
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StrategyDto {
    #[serde(default)]
    pub online: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ColumnDto {
    #[serde(rename = "type")]
    #[schemars(with = "String")]
    pub ty: Spanned<String>,

    #[serde(default = "yes")]
    pub nullable: bool,

    /// The default-value expression, kept verbatim.
    #[serde(default)]
    pub default: Option<String>,

    /// `[seed, increment]`
    #[serde(default)]
    pub identity: Option<[i64; 2]>,

    #[serde(default)]
    pub description: Option<String>,

    /// Present means deprecated; the value is the reason. The date comes from
    /// git and is not written here.
    #[serde(default)]
    pub deprecated: Option<String>,

    /// One-shot intent, as for [`TableDto::renamed_from`].
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub renamed_from: Option<Spanned<String>>,
}

const fn yes() -> bool {
    true
}

/// A primary key may be written as a bare column list, leaving the name to the
/// database, or with an explicit constraint name.
///
/// The named form is not there for looks: when `pbps pull` reverse-generates from
/// an existing database, losing the original constraint name would make the next
/// diff want to rename it.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum PrimaryKeyDto {
    Columns(Vec<String>),
    Named { name: String, columns: Vec<String> },
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForeignKeyDto {
    pub columns: Vec<String>,

    /// `dbo.region(region_id)` or `dbo.region(a, b)`.
    #[schemars(with = "String")]
    pub references: Spanned<String>,

    #[serde(default)]
    pub on_delete: ReferentialAction,

    #[serde(default)]
    pub on_update: ReferentialAction,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexDto {
    /// Each entry is `column` or `column desc`.
    #[schemars(with = "Vec<String>")]
    pub columns: Vec<Spanned<String>>,

    #[serde(default)]
    pub include: Vec<String>,

    #[serde(default)]
    pub unique: bool,

    /// The filtered-index predicate. The YAML key is `where`, which is a Rust
    /// keyword, hence the rename.
    #[serde(rename = "where", default)]
    pub filter: Option<String>,
}

// The doc comment below becomes the schema's own `description`, which an editor
// shows the user — so it says what a declaration file is, not how this type is
// used. The maintainer's note goes here instead:
//
// Only `JsonSchema` is derived. The loader never deserializes through this,
// because `KindProbe` reads the leading key first so that a misspelled field is
// reported against the shape the user meant rather than whichever variant
// happened to be tried first. This type exists so the *schema* says what the
// loader accepts, in one place, generated from the very structures it reads.

/// One pbps declaration file: a table, one view, procedure, function or
/// trigger, or a database role. The leading key is both the kind and the name.
#[derive(schemars::JsonSchema)]
#[serde(untagged)]
#[schemars(title = "pbps declaration")]
pub enum DeclarationFile {
    Table(TableDto),
    Module(ModuleDto),
    Role(RoleDto),
}

#[cfg(test)]
mod tests {
    /// The editor schema must refuse what the loader refuses. A `number` arm
    /// here would let an editor bless `pct: 1.50` that `validate` then rejects.
    #[test]
    fn the_published_value_schema_has_no_floating_point_arm() {
        let v = serde_json::to_value(schemars::schema_for!(super::ValueDto)).unwrap();
        let text = v.to_string();
        assert!(!text.contains("\"number\""), "{text}");
        assert!(!text.contains("double"), "{text}");
        // And the negative case: the arms that are valid are still there.
        for arm in ["\"null\"", "\"boolean\"", "\"integer\"", "\"string\""] {
            assert!(text.contains(arm), "{arm} missing from {text}");
        }
    }

    /// `spellings_of` writes a character class per letter and leaves anything
    /// else as one; a word carrying a regex metacharacter would reach the
    /// pattern as syntax. No word does, and this is where that stops being an
    /// assumption.
    #[test]
    fn every_permission_word_is_lowercase_ascii() {
        for permission in pbps_model::Permission::ALL {
            let word = permission.as_str();
            assert!(
                word.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "`{word}` is not lowercase ASCII and `-`"
            );
        }
    }

    /// The class the pattern pads with holds exactly the characters `trim`
    /// removes, and each of them literally.
    ///
    /// The exactness is the point in both directions: a character missing from
    /// it makes the schema refuse a declaration the loader accepts, and one
    /// too many makes it bless a declaration the loader refuses. This is where
    /// `\s` would fail — it is short U+0085 and long U+FEFF — so the class is
    /// compared against `char::is_whitespace` itself, which is the question
    /// `trim` asks.
    #[test]
    fn the_padding_class_holds_exactly_the_characters_trim_removes() {
        let class = super::whitespace_class();
        let inside = class
            .strip_prefix('[')
            .and_then(|c| c.strip_suffix(']'))
            .expect("the padding is a character class");
        let expected: String = (char::MIN..=char::MAX)
            .filter(|c| c.is_whitespace())
            .collect();
        assert_eq!(inside, expected);
        assert!(
            inside.contains('\u{85}'),
            "U+0085, which ECMA-262 `\\s` omits"
        );
        assert!(
            !inside.contains('\u{feff}'),
            "U+FEFF, which ECMA-262 `\\s` takes and `trim` does not"
        );
        // A metacharacter would change what the class matches rather than being
        // one of its members; none of the set is one today, and the class is
        // written without escaping on that basis.
        assert!(!inside.contains(['\\', ']', '^', '-']));
    }

    /// The pattern is the loader's fold written out: a letter matches either
    /// case, a `-` matches the two separators the fold also reads, and the
    /// caller's whitespace class stands for its `trim`.
    #[test]
    fn a_word_becomes_a_class_per_letter_and_the_separators_it_folds() {
        assert_eq!(super::spellings_of("usage"), "[Uu][Ss][Aa][Gg][Ee]");
        assert_eq!(
            super::spellings_of("view-definition"),
            "[Vv][Ii][Ee][Ww][-_ ][Dd][Ee][Ff][Ii][Nn][Ii][Tt][Ii][Oo][Nn]"
        );
    }
}
