//! PostgreSQL heap storage parameters (#1441, DEC-1441.1): the closed list a
//! table may declare, and each value in one canonical spelling.
//!
//! `pg_class.reloptions` keeps the spelling it was given, and the engine
//! parses that spelling with C's rules (measured on 16.15 and 18.6):
//! `autovacuum_enabled=of` is `false`, `fillfactor=070` is octal 56 and
//! `0x14` is 20, `vacuum_index_cleanup=TRUE` is `on`. Text comparison would
//! call each of those a change, so both sides are parsed here, by the
//! engine's rules, into one spelling: `true`/`false`, a decimal integer, the
//! shortest decimal of a real, and `auto`/`on`/`off`.
//!
//! Indexes take their own per-method list ([`MethodPolicy::parameters`], #1442),
//! parsed by the same rules.
//!
//! `toast.*` parameters are not here: a table without a TOAST relation
//! silently discards them, so a declared one could never read back.

use crate::IndexMethod;

/// How the engine parses a parameter's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `parse_bool`: `true`, `yes`, `on`, `1` and their unambiguous
    /// prefixes, and the same for false.
    Bool,
    /// `strtol(…, 0)`: decimal, `0x` hexadecimal, leading-zero octal.
    Int,
    /// [`Kind::Int`] within the inclusive bounds the engine states for the
    /// parameter (BRIN's `pages_per_range`: 1 to 131072, `22023` outside).
    IntBetween(i64, i64),
    /// `strtod`.
    Real,
    /// `vacuum_index_cleanup`: `auto`, or a whole boolean word.
    IndexCleanup,
    /// A GiST `buffering`: `on`, `off` or `auto`, in any case and with nothing
    /// around it (measured on 16.15 and 18.6: `ON` is stored as `on`; `true`
    /// and ` on` are refused by the engine).
    Buffering,
}

/// Every heap parameter a table may declare, by name. Two are 18's
/// (`autovacuum_vacuum_max_threshold`, `vacuum_max_eager_freeze_failure_rate`):
/// a 16 server refuses them when the plan runs, inside its transaction.
pub const TABLE_PARAMETERS: &[(&str, Kind)] = &[
    ("autovacuum_analyze_scale_factor", Kind::Real),
    ("autovacuum_analyze_threshold", Kind::Int),
    ("autovacuum_enabled", Kind::Bool),
    ("autovacuum_freeze_max_age", Kind::Int),
    ("autovacuum_freeze_min_age", Kind::Int),
    ("autovacuum_freeze_table_age", Kind::Int),
    ("autovacuum_multixact_freeze_max_age", Kind::Int),
    ("autovacuum_multixact_freeze_min_age", Kind::Int),
    ("autovacuum_multixact_freeze_table_age", Kind::Int),
    ("autovacuum_vacuum_cost_delay", Kind::Real),
    ("autovacuum_vacuum_cost_limit", Kind::Int),
    ("autovacuum_vacuum_insert_scale_factor", Kind::Real),
    ("autovacuum_vacuum_insert_threshold", Kind::Int),
    ("autovacuum_vacuum_max_threshold", Kind::Int),
    ("autovacuum_vacuum_scale_factor", Kind::Real),
    ("autovacuum_vacuum_threshold", Kind::Int),
    ("fillfactor", Kind::Int),
    ("log_autovacuum_min_duration", Kind::Int),
    ("parallel_workers", Kind::Int),
    ("toast_tuple_target", Kind::Int),
    ("user_catalog_table", Kind::Bool),
    ("vacuum_index_cleanup", Kind::IndexCleanup),
    ("vacuum_max_eager_freeze_failure_rate", Kind::Real),
    ("vacuum_truncate", Kind::Bool),
];

/// How `name` is parsed, where it is a table parameter at all.
pub fn table_kind(name: &str) -> Option<Kind> {
    TABLE_PARAMETERS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, k)| *k)
}

/// `value` of table parameter `name` in its one canonical spelling, or why
/// it is not one.
pub fn canonical(name: &str, value: &str) -> Result<String, String> {
    let kind = table_kind(name).ok_or_else(|| {
        format!(
            "`{name}` is not a table storage parameter this model declares",
            name = crate::escape::Escaped(name),
        )
    })?;
    spelled(kind, name, value)
}

/// What changing an index storage parameter in place does, by the engine
/// (DEC-2448.1). Measured per parameter and per method, never assumed from
/// another method's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParameterChange {
    /// `ALTER INDEX … SET`/`RESET`: nothing is rebuilt.
    Alter,
    /// The engine takes no in-place change, or it is not measured to keep the
    /// index sound: the part is dropped and created again with the final
    /// parameters in its `CREATE`.
    Rebuild,
    /// Neither is safe, so the change is refused by name.
    Refuse,
}

/// The table lock an in-place parameter change takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IndexLock {
    ShareUpdateExclusive,
    AccessExclusive,
}

/// One storage parameter of an index method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexParameter {
    pub name: &'static str,
    pub kind: Kind,
    pub change: ParameterChange,
    /// The lock this parameter's change takes where it is not the method's:
    /// GiST's `buffering` takes `AccessExclusive` and its `fillfactor` the
    /// lighter one (measured on 16.15 and 18.6, DEC-2448.2).
    pub lock: Option<IndexLock>,
}

/// What this model holds of one index access method: the capabilities the
/// engine reports for it, the key types and operator classes it is held over,
/// and its storage parameters with their change verdicts (DEC-2448.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodPolicy {
    pub method: IndexMethod,
    /// `pg_indexam_has_property(…, 'can_unique')`.
    pub unique: bool,
    /// `can_include`.
    pub include: bool,
    /// `can_order`: a key may be `ASC` or `DESC`.
    pub ordered: bool,
    /// Whether a key may be an expression rather than a column.
    pub expression_keys: bool,
    /// The one key type the method is held over, as a normalised base type;
    /// `None` for any type its default class covers.
    pub key_type: Option<&'static str>,
    /// The engine's own non-default classes the model holds, bare.
    pub classes: &'static [&'static str],
    /// The method's default class for `key_type`, which a declaration leaves
    /// out and never spells; `None` where it depends on the key's type.
    pub default_class: Option<&'static str>,
    /// Whether a column key may be compared under a collation of its own
    /// (`COLLATE` on a key). GIN's `jsonb` has none.
    pub collation: bool,
    /// `pg_indexam_has_property(…, 'can_exclude')`: the method may back an
    /// exclusion constraint (DEC-2448.3). Measured: B-tree and GiST can; GIN
    /// and BRIN refuse ("does not support exclusion constraints").
    pub exclusion: bool,
    /// For a method held over a few column types only: each admitted base type
    /// and the engine class its key must name (`None`: the type's default
    /// class, so none is written). Empty where the method is not held by type
    /// (DEC-2448.2).
    pub typed_columns: &'static [(&'static str, Option<&'static str>)],
    /// The lock an in-place change of one of its parameters takes.
    pub lock: IndexLock,
    pub parameters: &'static [IndexParameter],
}

impl MethodPolicy {
    /// The parameter `name`, if this method declares it.
    pub fn parameter(&self, name: &str) -> Option<&'static IndexParameter> {
        self.parameters.iter().find(|p| p.name == name)
    }

    /// The strongest lock an in-place change of `names` takes: each
    /// parameter's own, or the method's.
    pub fn lock_of<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> IndexLock {
        names
            .into_iter()
            .map(|name| {
                self.parameter(name)
                    .and_then(|p| p.lock)
                    .unwrap_or(self.lock)
            })
            .max()
            .unwrap_or(self.lock)
    }

    /// The most severe verdict among `names` and the first name that has it.
    /// A name this method does not declare is `Alter`: the loader already
    /// refused it, so the differ never has to guess.
    pub fn change_of<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> (ParameterChange, Option<&'a str>) {
        let mut worst = (ParameterChange::Alter, None);
        for name in names {
            let verdict = self
                .parameter(name)
                .map_or(ParameterChange::Alter, |p| p.change);
            if verdict > worst.0 {
                worst = (verdict, Some(name));
            }
        }
        worst
    }
}

const fn param(name: &'static str, kind: Kind) -> IndexParameter {
    // Every parameter this model holds was measured on 16 and 18 to change in
    // place and keep the relfilenode (#1442, DEC-1442.1; re-measured for
    // DEC-2448.1).
    IndexParameter {
        name,
        kind,
        change: ParameterChange::Alter,
        lock: None,
    }
}

/// [`param`] whose change takes `lock`, not the method's.
const fn param_locked(name: &'static str, kind: Kind, lock: IndexLock) -> IndexParameter {
    IndexParameter {
        name,
        kind,
        change: ParameterChange::Alter,
        lock: Some(lock),
    }
}

/// [`param`] whose change the engine cannot make in place, or not so that the
/// index uses it, and which takes `lock` where it is altered.
const fn param_verdict(
    name: &'static str,
    kind: Kind,
    change: ParameterChange,
    lock: IndexLock,
) -> IndexParameter {
    IndexParameter {
        name,
        kind,
        change,
        lock: Some(lock),
    }
}

static BTREE: MethodPolicy = MethodPolicy {
    method: IndexMethod::Btree,
    unique: true,
    include: true,
    ordered: true,
    expression_keys: true,
    key_type: None,
    classes: &[],
    default_class: None,
    collation: true,
    exclusion: true,
    typed_columns: &[],
    lock: IndexLock::ShareUpdateExclusive,
    parameters: &[
        param("deduplicate_items", Kind::Bool),
        param("fillfactor", Kind::Int),
    ],
};

static GIN: MethodPolicy = MethodPolicy {
    method: IndexMethod::Gin,
    unique: false,
    include: false,
    ordered: false,
    expression_keys: false,
    key_type: Some("jsonb"),
    classes: &["jsonb_path_ops"],
    default_class: Some("jsonb_ops"),
    collation: false,
    exclusion: false,
    typed_columns: &[],
    lock: IndexLock::AccessExclusive,
    parameters: &[
        param("fastupdate", Kind::Bool),
        param("gin_pending_list_limit", Kind::Int),
    ],
};

/// GiST (DEC-2448.2), measured on 16.15 and 18.6. No unique, no order, `INCLUDE`
/// of columns only. Of the column types this model declares only `tsvector`
/// and `tsquery` have a GiST default class and `inet`/`cidr` take `inet_ops`
/// (`inet_ops` is the one non-default class of the engine's own); every other
/// type is indexed through an expression such as `int4range(a, b)`. Both
/// parameters change in place and keep the relation file; `buffering` only
/// matters at build, which `SET` records without effect, as a B-tree's
/// `fillfactor` does for the pages already built (DEC-1442.1). The locks
/// differ by parameter: `fillfactor` takes `ShareUpdateExclusive` and
/// `buffering` `AccessExclusive` (measured on both servers).
static GIST: MethodPolicy = MethodPolicy {
    method: IndexMethod::Gist,
    unique: false,
    include: true,
    ordered: false,
    expression_keys: true,
    key_type: None,
    classes: &["inet_ops"],
    default_class: None,
    collation: true,
    exclusion: true,
    typed_columns: &[
        ("tsvector", None),
        ("tsquery", None),
        ("inet", Some("inet_ops")),
        ("cidr", Some("inet_ops")),
    ],
    lock: IndexLock::ShareUpdateExclusive,
    parameters: &[
        param_locked("buffering", Kind::Buffering, IndexLock::AccessExclusive),
        param("fillfactor", Kind::Int),
    ],
};

/// BRIN (DEC-2449.1), measured on 16.15 and 18.6. No unique, no `INCLUDE`, no
/// order. Held over a column of a type the model declares and the engine gives a
/// default class (the minmax ones and `inet`/`cidr`'s inclusion), or over an
/// expression; the default class is the only one held (the multi and Bloom
/// classes are the next part). `pages_per_range` is `Rebuild`: `ALTER … SET`
/// records the value and leaves the metapage at the old granularity until a
/// `REINDEX`, and `RESET` keeps the physical one, so only a rebuild makes the
/// declared value the one in use. `autosummarize` is a runtime flag and changes
/// in place. Both take `AccessExclusive` on the index.
static BRIN: MethodPolicy = MethodPolicy {
    method: IndexMethod::Brin,
    unique: false,
    include: false,
    ordered: false,
    expression_keys: true,
    exclusion: false,
    key_type: None,
    classes: &[],
    default_class: None,
    collation: false,
    typed_columns: &[
        ("smallint", None),
        ("integer", None),
        ("bigint", None),
        ("numeric", None),
        ("real", None),
        ("double precision", None),
        ("character", None),
        ("character varying", None),
        ("text", None),
        ("bytea", None),
        ("date", None),
        ("time without time zone", None),
        ("time with time zone", None),
        ("timestamp without time zone", None),
        ("timestamp with time zone", None),
        ("interval", None),
        ("uuid", None),
        ("macaddr", None),
        ("inet", None),
        ("cidr", None),
    ],
    lock: IndexLock::AccessExclusive,
    parameters: &[
        param_verdict(
            "pages_per_range",
            Kind::IntBetween(1, 131072),
            ParameterChange::Rebuild,
            IndexLock::AccessExclusive,
        ),
        param_locked("autosummarize", Kind::Bool, IndexLock::AccessExclusive),
    ],
};

pub(crate) fn policy_of(method: IndexMethod) -> &'static MethodPolicy {
    match method {
        IndexMethod::Brin => &BRIN,
        IndexMethod::Gist => &GIST,
        IndexMethod::Btree => &BTREE,
        IndexMethod::Gin => &GIN,
    }
}

/// `value` of index parameter `name`, for an index of `method`, in its one
/// canonical spelling, or why it is not one. A key's and a unique
/// constraint's index is a B-tree.
pub fn canonical_index(method: IndexMethod, name: &str, value: &str) -> Result<String, String> {
    let kind = method
        .policy()
        .parameter(name)
        .map(|p| p.kind)
        .ok_or_else(|| {
            format!(
                "`{name}` is not a storage parameter of a {} index this model declares",
                method.as_str(),
                name = crate::escape::Escaped(name),
            )
        })?;
    spelled(kind, name, value)
}

fn spelled(kind: Kind, name: &str, value: &str) -> Result<String, String> {
    let read = match kind {
        Kind::Bool => parse_bool(value).map(|b| b.to_string()),
        Kind::Int => parse_int(value).map(|i| i.to_string()),
        Kind::IntBetween(lo, hi) => parse_int(value)
            .filter(|i| (lo..=hi).contains(i))
            .map(|i| i.to_string()),
        // `+ 0.0` turns a negative zero into zero, which `to_string` would
        // otherwise spell `-0` (#1477 review).
        Kind::Real => parse_real(value).map(|r| (r + 0.0).to_string()),
        Kind::Buffering => {
            let word = value.to_lowercase();
            ["on", "off", "auto"]
                .contains(&word.as_str())
                .then_some(word)
        }
        Kind::IndexCleanup => {
            let word = value.trim().to_lowercase();
            if word == "auto" {
                Some(word)
            } else {
                whole_bool(&word).map(|b| if b { "on" } else { "off" }.to_owned())
            }
        }
    };
    read.ok_or_else(|| {
        format!(
            "`{value}` is not a value of storage parameter `{name}`",
            name = crate::escape::Escaped(name),
            value = crate::escape::Escaped(value),
        )
    })
}

/// PostgreSQL's `parse_bool`: case-insensitive, surrounding whitespace
/// allowed, and any prefix of a word that names one value (`tr`, `of`, but
/// not `o`).
fn parse_bool(value: &str) -> Option<bool> {
    let v = value.trim().to_lowercase();
    if v.is_empty() {
        return None;
    }
    let prefix = |word: &str, min: usize| v.len() >= min && word.starts_with(v.as_str());
    if prefix("true", 1) || prefix("yes", 1) || prefix("on", 2) || v == "1" {
        Some(true)
    } else if prefix("false", 1) || prefix("no", 1) || prefix("off", 2) || v == "0" {
        Some(false)
    } else {
        None
    }
}

/// A boolean written as a whole word, the only form `vacuum_index_cleanup`
/// takes besides `auto`.
fn whole_bool(word: &str) -> Option<bool> {
    match word {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// PostgreSQL's `parse_int`: C's `strtol` with base 0 (surrounding
/// whitespace, a sign, then `0x` hexadecimal, a leading `0` octal, or
/// decimal), and where that stops at `.`, `e` or `E`, the whole value read
/// again as a real and rounded half to even, as `rint` does: `70.5` is 70
/// and `7e1` is 70 (measured on 16 and 18, #1477 review).
fn parse_int(value: &str) -> Option<i64> {
    let v = value.trim();
    let (integer, rest) = strtol(v)?;
    if rest.is_empty() {
        return Some(integer);
    }
    if !rest.starts_with(['.', 'e', 'E']) {
        return None;
    }
    // A hexadecimal real (`0x46.0`) is `strtod`'s too; `parse_real` reads
    // no hexadecimal, so such a value is refused rather than misread.
    let real = parse_real(v)?.round_ties_even();
    (real >= i64::MIN as f64 && real <= i64::MAX as f64).then_some(real as i64)
}

/// C's `strtol(…, 0)` over `v`: the value read and what follows it, or
/// `None` where no digit was read or the value overflows.
fn strtol(v: &str) -> Option<(i64, &str)> {
    let (negative, unsigned) = match v.as_bytes().first()? {
        b'-' => (true, &v[1..]),
        b'+' => (false, &v[1..]),
        _ => (false, v),
    };
    let hex = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
        .filter(|h| h.starts_with(|c: char| c.is_ascii_hexdigit()));
    let (radix, digits) = match hex {
        Some(h) => (16, h),
        None if unsigned.starts_with('0') => (8, unsigned),
        None => (10, unsigned),
    };
    let end = digits
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let mut magnitude: i64 = 0;
    for c in digits[..end].chars() {
        let d = i64::from(c.to_digit(radix)?);
        magnitude = magnitude.checked_mul(i64::from(radix))?.checked_add(d)?;
    }
    Some((
        if negative { -magnitude } else { magnitude },
        &digits[end..],
    ))
}

/// A decimal real, as `strtod` reads it, refused where `strtod` sets
/// `ERANGE`: an overflow, and an underflow to zero or below the smallest
/// normal value, which the engine refuses rather than reading as 0 (measured
/// on 16 and 18, #1477 review). The hexadecimal and infinite forms `strtod`
/// also takes are not read: no bound of these parameters admits infinity,
/// and a hexadecimal one is left to fail where it is read, as a value this
/// reader cannot spell.
fn parse_real(value: &str) -> Option<f64> {
    let v = value.trim();
    if v.is_empty() || v.contains(['x', 'X', 'n', 'N', 'i', 'I']) {
        return None;
    }
    let real = v.parse::<f64>().ok().filter(|r| r.is_finite())?;
    let mantissa = v.split(['e', 'E']).next().unwrap_or_default();
    let written_nonzero = mantissa.contains(|c: char| ('1'..='9').contains(&c));
    let underflow = (real == 0.0 && written_nonzero) || (real != 0.0 && !real.is_normal());
    (!underflow).then_some(real)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What btree and GIN hold, as data: the capabilities the engine reports
    /// (measured live against `pg_indexam_has_property` too), the key type
    /// and classes, the lock, and a verdict for every parameter (DEC-2448.1).
    #[test]
    fn the_policy_table_holds_what_btree_and_gin_always_did() {
        let b = IndexMethod::Btree.policy();
        assert_eq!(b.method, IndexMethod::Btree);
        assert!(b.unique && b.include && b.ordered && b.expression_keys);
        assert_eq!(
            (b.key_type, b.classes, b.default_class),
            (None, &[][..], None)
        );
        assert_eq!(b.lock, IndexLock::ShareUpdateExclusive);
        let g = IndexMethod::Gin.policy();
        assert_eq!(g.method, IndexMethod::Gin);
        assert!(!g.unique && !g.include && !g.ordered && !g.expression_keys);
        assert_eq!(g.key_type, Some("jsonb"));
        assert_eq!(
            (g.classes, g.default_class),
            (&["jsonb_path_ops"][..], Some("jsonb_ops"))
        );
        assert_eq!(g.lock, IndexLock::AccessExclusive);
        let names = |p: &MethodPolicy| p.parameters.iter().map(|x| x.name).collect::<Vec<_>>();
        assert_eq!(names(b), ["deduplicate_items", "fillfactor"]);
        assert_eq!(names(g), ["fastupdate", "gin_pending_list_limit"]);
        for method in IndexMethod::ALL {
            let policy = method.policy();
            // BRIN's `pages_per_range` is the one parameter that is not altered.
            assert!(
                policy.parameters.iter().all(|p| {
                    p.change == ParameterChange::Alter
                        || (method == IndexMethod::Brin && p.name == "pages_per_range")
                }),
                "nothing else is rebuilt or refused yet: {}",
                method.as_str()
            );
            assert_eq!(IndexMethod::from_am_name(method.as_str()), Some(method));
        }
    }

    /// GiST's row, measured on 16.15 and 18.6 (DEC-2448.2): what the engine
    /// reports for the method, its two parameters both altered in place, and
    /// the column types it is held over with the class each needs.
    #[test]
    fn the_gist_policy_row_is_what_the_engine_reports() {
        let g = IndexMethod::Gist.policy();
        assert_eq!(g.method, IndexMethod::Gist);
        assert!(!g.unique && g.include && !g.ordered && g.expression_keys && g.collation);
        assert_eq!(g.lock, IndexLock::ShareUpdateExclusive);
        // `buffering` takes the heavier lock; the strongest of a change wins.
        assert_eq!(g.lock_of(["fillfactor"]), IndexLock::ShareUpdateExclusive);
        assert_eq!(g.lock_of(["buffering"]), IndexLock::AccessExclusive);
        assert_eq!(
            g.lock_of(["fillfactor", "buffering"]),
            IndexLock::AccessExclusive
        );
        assert_eq!(g.lock_of([]), g.lock);
        assert_eq!(g.classes, &["inet_ops"]);
        assert_eq!(g.default_class, None);
        assert_eq!(
            g.typed_columns,
            &[
                ("tsvector", None),
                ("tsquery", None),
                ("inet", Some("inet_ops")),
                ("cidr", Some("inet_ops")),
            ]
        );
        assert!(
            g.parameters
                .iter()
                .all(|p| p.change == ParameterChange::Alter)
        );
        // Negative: btree and GIN are not held by type, and GIN has no collation.
        assert!(IndexMethod::Btree.policy().typed_columns.is_empty());
        assert!(!IndexMethod::Gin.policy().collation);
    }

    /// BRIN's row, measured on 16.15 and 18.6 (DEC-2449.1): what the engine
    /// reports for the method, `pages_per_range` rebuilt (a native `ALTER`
    /// leaves the metapage at the old granularity until a `REINDEX`) and
    /// `autosummarize` altered, both under `AccessExclusive`, and the column
    /// types that have a default class.
    #[test]
    fn the_brin_policy_row_is_what_the_engine_reports() {
        let b = IndexMethod::Brin.policy();
        assert_eq!(b.method, IndexMethod::Brin);
        assert!(!b.unique && !b.include && !b.ordered && b.expression_keys && !b.collation);
        assert_eq!((b.classes, b.default_class), (&[][..], None));
        assert_eq!(b.lock, IndexLock::AccessExclusive);
        assert_eq!(
            b.change_of(["pages_per_range"]),
            (ParameterChange::Rebuild, Some("pages_per_range"))
        );
        assert_eq!(b.change_of(["autosummarize"]).0, ParameterChange::Alter);
        // The strongest verdict of a change wins.
        assert_eq!(
            b.change_of(["autosummarize", "pages_per_range"]).0,
            ParameterChange::Rebuild
        );
        assert_eq!(
            b.lock_of(["autosummarize", "pages_per_range"]),
            IndexLock::AccessExclusive
        );
        let typed: Vec<_> = b.typed_columns.iter().map(|(t, _)| *t).collect();
        // The measured defaults: `inet` and `cidr` through inclusion, the rest
        // minmax; no class is ever required of a column.
        for held in [
            "integer",
            "text",
            "timestamp with time zone",
            "inet",
            "cidr",
            "uuid",
        ] {
            assert!(typed.contains(&held), "{held}");
        }
        assert!(b.typed_columns.iter().all(|(_, class)| class.is_none()));
        // Negative: the types the engine gives no BRIN class are not held.
        for none in ["boolean", "json", "jsonb", "tsvector", "xml"] {
            assert!(!typed.contains(&none), "{none}");
        }
    }

    /// `pages_per_range` is a whole number from 1 to 131072 (the engine's
    /// `22023` outside), in the engine's `strtol` spellings; `autosummarize` is a
    /// boolean word kept as its one canonical spelling (`yes` reads `true`).
    #[test]
    fn a_brin_pages_per_range_is_bounded_and_autosummarize_is_a_boolean() {
        for (given, canonical) in [
            ("1", "1"),
            ("128", "128"),
            ("0x20", "32"),
            ("131072", "131072"),
            ("1.5", "2"),
        ] {
            assert_eq!(
                canonical_index(IndexMethod::Brin, "pages_per_range", given).unwrap(),
                canonical
            );
        }
        for bad in ["0", "-1", "131073", "x", "", "0.4"] {
            assert!(
                canonical_index(IndexMethod::Brin, "pages_per_range", bad).is_err(),
                "{bad:?}"
            );
        }
        for (given, canonical) in [
            ("on", "true"),
            ("yes", "true"),
            ("off", "false"),
            ("0", "false"),
        ] {
            assert_eq!(
                canonical_index(IndexMethod::Brin, "autosummarize", given).unwrap(),
                canonical
            );
        }
        assert!(canonical_index(IndexMethod::Brin, "autosummarize", "maybe").is_err());
        // Negative: another method's parameters, and R2's, are not BRIN's here.
        for other in ["fillfactor", "values_per_range", "n_distinct_per_range"] {
            assert!(
                canonical_index(IndexMethod::Brin, other, "1").is_err(),
                "{other}"
            );
        }
    }

    /// `buffering` is `on`, `off` or `auto` in any case, and nothing else:
    /// the engine stores `ON` as `on` and refuses `true` and ` on`.
    #[test]
    fn a_gist_buffering_has_three_spellings_and_no_others() {
        for (given, canonical) in [("on", "on"), ("OFF", "off"), ("Auto", "auto")] {
            assert_eq!(
                canonical_index(IndexMethod::Gist, "buffering", given).unwrap(),
                canonical
            );
        }
        for bad in ["true", "1", " on", "maybe", ""] {
            assert!(
                canonical_index(IndexMethod::Gist, "buffering", bad).is_err(),
                "{bad:?}"
            );
        }
        assert_eq!(
            canonical_index(IndexMethod::Gist, "fillfactor", "070").unwrap(),
            "56"
        );
    }

    /// Negative: another method's parameter, and a method the model does not
    /// hold, are not this method's.
    #[test]
    fn a_parameter_belongs_to_its_method_only() {
        let b = IndexMethod::Btree.policy();
        assert!(b.parameter("fillfactor").is_some());
        assert!(b.parameter("fastupdate").is_none());
        assert!(IndexMethod::Gin.policy().parameter("fillfactor").is_none());
        assert!(canonical_index(IndexMethod::Gin, "fillfactor", "70").is_err());
        assert_eq!(IndexMethod::from_am_name("gist"), Some(IndexMethod::Gist));
        assert_eq!(IndexMethod::from_am_name("brin"), Some(IndexMethod::Brin));
        assert_eq!(IndexMethod::from_am_name("hash"), None);
        assert!(IndexMethod::Gist.policy().parameter("fastupdate").is_none());
        assert!(canonical_index(IndexMethod::Gist, "fastupdate", "on").is_err());
        assert_eq!(IndexMethod::from_am_name("BTREE"), None);
    }

    /// The worst verdict among the names decides, and names the first that
    /// has it; a name the method does not declare is `Alter`.
    #[test]
    fn the_worst_parameter_verdict_decides_and_is_named() {
        static P: [IndexParameter; 3] = [
            IndexParameter {
                name: "a",
                kind: Kind::Int,
                change: ParameterChange::Alter,
                lock: None,
            },
            IndexParameter {
                name: "r",
                kind: Kind::Int,
                change: ParameterChange::Rebuild,
                lock: None,
            },
            IndexParameter {
                name: "x",
                kind: Kind::Int,
                change: ParameterChange::Refuse,
                lock: None,
            },
        ];
        let policy = MethodPolicy {
            parameters: &P,
            ..*IndexMethod::Btree.policy()
        };
        assert_eq!(policy.change_of([]), (ParameterChange::Alter, None));
        assert_eq!(
            policy.change_of(["a", "unknown"]),
            (ParameterChange::Alter, None)
        );
        assert_eq!(
            policy.change_of(["a", "r"]),
            (ParameterChange::Rebuild, Some("r"))
        );
        assert_eq!(
            policy.change_of(["r", "x", "a"]),
            (ParameterChange::Refuse, Some("x"))
        );
    }

    /// Each spelling the engine reads as one value is one value here, by the
    /// engine's own rules (measured on 16 and 18).
    #[test]
    fn every_spelling_the_engine_reads_alike_is_one_value() {
        for (name, spellings, value) in [
            (
                "autovacuum_enabled",
                &["off", "OF", "false", "f", "no", "0", " False "][..],
                "false",
            ),
            (
                "autovacuum_enabled",
                &["on", "tr", "TRUE", "y", "yes", "1"][..],
                "true",
            ),
            (
                "fillfactor",
                &["56", "070", "0x38", "+56", " 56 "][..],
                "56",
            ),
            (
                "autovacuum_vacuum_scale_factor",
                &["0.01", "1e-2", ".01", "0.010"][..],
                "0.01",
            ),
            // Negative zero is zero: kept as `-0`, a bare `-0` would load back
            // as the integer 0 and never round-trip (#1477 review).
            (
                "autovacuum_vacuum_scale_factor",
                &["0", "-0", "-0.0", "0e5"][..],
                "0",
            ),
            // An integer `strtol` stops short of at `.` or `e` is read again
            // as a real and rounded half to even, as `parse_int` does:
            // `70.5` is 70 (measured on 16 and 18, #1477 review).
            (
                "fillfactor",
                &["70.0", "7e1", "70.5", "070.0", "69.5"][..],
                "70",
            ),
            ("fillfactor", &["71.5"][..], "72"),
            (
                "vacuum_index_cleanup",
                &["on", "TRUE", "yes", "1"][..],
                "on",
            ),
            (
                "vacuum_index_cleanup",
                &["off", "false", "no", "0"][..],
                "off",
            ),
            ("vacuum_index_cleanup", &["auto", "AUTO"][..], "auto"),
        ] {
            for spelling in spellings {
                assert_eq!(
                    canonical(name, spelling).as_deref(),
                    Ok(value),
                    "{name}={spelling}"
                );
            }
        }
    }

    /// Each index method takes its own parameters, read by the same rules,
    /// and refuses the other's (#1442).
    #[test]
    fn an_index_takes_its_own_methods_parameters() {
        let b = IndexMethod::Btree;
        let g = IndexMethod::Gin;
        assert_eq!(canonical_index(b, "fillfactor", "070").as_deref(), Ok("56"));
        assert_eq!(
            canonical_index(b, "deduplicate_items", "of").as_deref(),
            Ok("false")
        );
        assert_eq!(
            canonical_index(g, "fastupdate", "TRUE").as_deref(),
            Ok("true")
        );
        assert_eq!(
            canonical_index(g, "gin_pending_list_limit", "7e1").as_deref(),
            Ok("70")
        );
        for (method, name) in [
            (b, "fastupdate"),
            (g, "fillfactor"),
            (b, "autovacuum_enabled"),
            (g, "bogus"),
        ] {
            assert!(
                canonical_index(method, name, "1").is_err(),
                "{method:?} {name}"
            );
        }
    }

    /// What the engine refuses, or this reader cannot spell, is refused by
    /// name: never read as some value it is not.
    #[test]
    fn a_name_or_value_the_engine_would_not_take_is_refused() {
        for (name, value) in [
            ("bogus", "1"),
            ("toast.autovacuum_enabled", "false"),
            ("autovacuum_enabled", "o"),
            ("autovacuum_enabled", "maybe"),
            ("fillfactor", "08"),
            ("fillfactor", "1s"),
            ("fillfactor", ""),
            ("autovacuum_vacuum_scale_factor", "inf"),
            ("autovacuum_vacuum_scale_factor", "0x1p-3"),
            ("vacuum_index_cleanup", "tr"),
            // `strtod` underflow, which the engine refuses rather than reading
            // as 0 (#1477 review).
            ("autovacuum_vacuum_scale_factor", "1e-400"),
            ("autovacuum_vacuum_scale_factor", "1e-320"),
            // Neither a fraction nor an exponent where `strtol` stopped.
            ("fillfactor", "70x"),
            ("fillfactor", "08.5"),
        ] {
            assert!(canonical(name, value).is_err(), "{name}={value}");
        }
    }
}
