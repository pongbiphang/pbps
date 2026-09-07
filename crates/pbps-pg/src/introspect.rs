//! Reading a live PostgreSQL back into the model: the pure half.
//!
//! The split is [`crate::catalog`]'s doc comment's, and it is the one the SQL
//! Server crate uses for the same reason: **only the query file talks to a
//! server**. It turns `pg_catalog` rows into the plain `Raw*` structs below and
//! hands them to [`assemble`], which is a function of its input and testable
//! with no server at all. Every rule about what the model can and cannot hold
//! lives here, where a test can reach it.
//!
//! # What this half must never do
//!
//! **Absent, empty and unreadable are three different things** (CLAUDE.md), and
//! introspection is where that rule earns its keep. A catalog row this code
//! cannot express does not vanish and does not become a plausible guess: it
//! becomes a [`Limitation`], carried out beside the schema so the caller can
//! show it. Drift that pbps cannot see is worse than drift it reports and
//! cannot fix, because only the second kind is visible to the operator.
//!
//! # Two ways of not being able to hold something
//!
//! A fact this model cannot carry is always named. What differs is whether the
//! object it belongs to is carried anyway, and the line is what the difference
//! is *about*:
//!
//! - **A property of the object itself** — `RESTRICT`, `DEFERRABLE`, a `gin`
//!   index, an expression index — means the object is **left out**. Carried, it
//!   would compare equal to one that behaves differently, and a plan would
//!   report no change while the behaviour stayed wrong. Left out, the plan is
//!   wrong in the direction that fails loudly on apply, and the warning says
//!   why.
//! - **A fact about the rows already there** — `NOT VALID` — means the object
//!   is **carried**, because the object itself is exactly what the model says.
//!   What recreating it would change is which rows get checked, and that is a
//!   plan that fails on apply rather than one that lies.
//!
//! # This list is sampled, not derived
//!
//! Everything below is a feature this model cannot hold, found one at a time —
//! several of them by review rather than by this code. That is the wrong shape
//! and it is knowingly the wrong shape: **enumerating what a model cannot hold
//! is an open set, and proving what it did hold is a closed one.** The next
//! engine release adds to the first list without touching the second;
//! `pg_constraint.contype = 'n'` is exactly that, having arrived in PostgreSQL
//! 18.
//!
//! The closed form needs an emitter to render an object back and compare it
//! against the engine's own text, so it belongs with Phase 5 step 4 and is
//! issue #168. Until then, every rule here is one a reader can check, and each
//! message names the fact rather than the flag.
//!
//! **It must not un-respell anything** (ADR-0009 §2). PostgreSQL rewrites what
//! it was given — `'x'` becomes `'x'::character varying`, `CHECK (amount >= 0)`
//! becomes `CHECK ((amount >= (0)::numeric))` — and the state's `declared`
//! record (DECISIONS 207-209) is what makes the comparison honest. A normalizer
//! here would hide exactly the difference the record exists to make visible, so
//! the three verbatim expressions — a column's default, a check's expression
//! and an index's filter — are carried through untouched.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use pbps_model::{
    CheckConstraint, Column, ColumnType, ForeignKey, Identity, Index, IndexColumn, PrimaryKey,
    ReferentialAction, Schema, Table, TableName, UniqueConstraint,
};

use crate::types;

/// One ordinary table, as `pg_class` has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTable {
    pub oid: i64,
    pub schema: String,
    pub name: String,
}

/// One live column of one table.
///
/// A dropped column never reaches here: the catalog keeps its slot with a
/// placeholder name and no readable type (ADR-0012 §6), and the query filters
/// `attisdropped`. `attnum` is kept all the same, because it is what
/// `pg_constraint.conkey` and `pg_index.indkey` refer to — and because the
/// dropped slot leaves a **hole** in it, so it is an identifier and never a
/// position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawColumn {
    pub table_oid: i64,
    pub attnum: i32,
    pub name: String,
    /// `format_type(atttypid, atttypmod)`: the engine's own spelling.
    pub ty: String,
    pub nullable: bool,
    /// `pg_get_expr(adbin, adrelid)`, verbatim, cast and all.
    pub default: Option<String>,
    /// `attidentity`: `a` for ALWAYS, `d` for BY DEFAULT, empty for neither.
    pub identity: Option<RawIdentity>,
    /// `attgenerated`: `s` for a stored generated column, empty for neither.
    pub generated: bool,
    /// The sequence this column merely *defaults from* — a `serial`'s, whose
    /// `pg_depend` entry is `deptype = 'a'`. An identity's sequence is not
    /// here: that one is part of the column, and arrives as [`RawIdentity`].
    pub owned_sequence: Option<String>,
    /// The column's collation, when it is not the one its type carries.
    pub collation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawIdentity {
    /// `true` for `GENERATED ALWAYS`, `false` for `GENERATED BY DEFAULT`.
    pub always: bool,
    pub seed: i64,
    pub increment: i64,
    /// The rest of the sequence, which [`Identity`] has nowhere to put.
    pub min: i64,
    pub max: i64,
    pub cycles: bool,
    /// `seqcache`. Not a bound but an allocation: with `CACHE n` a session
    /// takes `n` values at once, so a restart skips the ones it had not used
    /// and two sessions interleave in blocks rather than one at a time.
    pub cache: i64,
}

/// One row of `pg_constraint`, whatever its kind.
///
/// The kind is kept as the engine's own character rather than parsed into an
/// enum here, because the set is open at the engine's end: PostgreSQL 18 added
/// `n` for a catalogued NOT NULL, and a reader that had folded the characters
/// it knew into an enum and everything else into "a check" would have started
/// reporting one phantom check per NOT NULL column on an engine upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawConstraint {
    pub table_oid: i64,
    pub name: String,
    /// `contype`: `p` primary key, `u` unique, `f` foreign key, `c` check,
    /// `n` not null (PostgreSQL 18+), `x` exclusion, `t` constraint trigger.
    pub kind: char,
    /// `conkey`, the constrained columns as **attnums**.
    pub columns: Vec<i32>,
    /// `confkey`, the referenced columns as attnums, for a foreign key.
    pub ref_columns: Vec<i32>,
    /// `confrelid`, the referenced table.
    pub ref_table: Option<i64>,
    /// `confdeltype` and `confupdtype`: `a` NO ACTION, `r` RESTRICT,
    /// `c` CASCADE, `n` SET NULL, `d` SET DEFAULT.
    pub on_delete: char,
    pub on_update: char,
    /// `convalidated`. A constraint declared `NOT VALID` has never been checked
    /// against the rows already there.
    pub validated: bool,
    /// `condeferrable` and `condeferred`. A key whose check can be put off to
    /// the end of the transaction is a different key: rows that violate it may
    /// legally exist in the middle of one.
    pub deferrable: bool,
    pub deferred: bool,
    /// `pg_get_constraintdef`, verbatim: the whole clause, `CHECK (…)` and all.
    pub definition: String,
    /// `pg_get_expr(conbin, conrelid)`: the check's expression **without** the
    /// `CHECK (…)` around it, which is what the model holds.
    pub expression: Option<String>,
    /// `confmatchtype`: `s` MATCH SIMPLE (the default), `f` MATCH FULL,
    /// `p` MATCH PARTIAL (which this engine does not implement).
    pub match_type: char,
    /// `confdelsetcols`: the columns an `ON DELETE SET NULL (a, b)` touches.
    /// Empty for the ordinary form, which touches all of them.
    pub delete_set_columns: Vec<i32>,
    /// `conindid`: the index this constraint is enforced by, if any. It is how
    /// a unique index that *is* a constraint is told from one that is not.
    ///
    /// For a foreign key it is not the key's own index at all: **measured**, it
    /// is the unique index on the *referenced* table that the key points at.
    pub index_oid: Option<i64>,
    /// `conenforced` (PostgreSQL 18+). `false` is `NOT ENFORCED`: the engine
    /// records the constraint and checks nothing, ever.
    pub enforced: bool,
    /// `conperiod` (PostgreSQL 18+). `true` is `WITHOUT OVERLAPS` on a key or
    /// `PERIOD` on a foreign key, with the `contype` of an ordinary one.
    pub period: bool,
    /// `connoinherit`. `true` is a check that applies to this table's own rows
    /// and to no table that inherits from it.
    pub no_inherit: bool,
}

/// One row of `pg_index`, joined to what it needs from `pg_class` and `pg_am`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawIndex {
    pub oid: i64,
    pub table_oid: i64,
    pub name: String,
    pub unique: bool,
    pub primary: bool,
    pub exclusion: bool,
    /// `indisvalid`. A failed `CREATE INDEX CONCURRENTLY` leaves a row behind
    /// that the planner will not use.
    pub valid: bool,
    /// `indnullsnotdistinct`, which decides whether a unique index admits more
    /// than one null key.
    pub nulls_not_distinct: bool,
    /// `indnkeyatts`: how many of `columns` are key columns. The rest are the
    /// `INCLUDE` payload.
    pub key_count: usize,
    /// `indkey`, as attnums. A `0` is an expression rather than a column.
    pub columns: Vec<i32>,
    /// `indoption` per column; bit 0 is DESC, bit 1 is NULLS FIRST.
    pub options: Vec<i32>,
    /// `pg_get_expr(indpred, indrelid)`, verbatim.
    pub filter: Option<String>,
    /// Whether `indexprs` is set: the index is over an expression.
    pub has_expressions: bool,
    /// `amname`: `btree`, `hash`, `gin`, …
    pub method: String,
    /// Whether any key column uses an operator class that is not its type's
    /// default, or a collation that is not the column's own — `text_pattern_ops`
    /// and `COLLATE "C"`. Computed in the query, because what "default" means
    /// is a catalog lookup rather than a fact about the row.
    pub nondefault_column_options: bool,
}

/// Everything one pull read, before any of it is interpreted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawCatalog {
    pub tables: Vec<RawTable>,
    pub columns: Vec<RawColumn>,
    pub constraints: Vec<RawConstraint>,
    pub indexes: Vec<RawIndex>,
}

/// One fact about the database that the model cannot hold.
///
/// It carries the table it belongs to so that a caller can tell a limitation
/// inside the managed set — which is drift it must not call clean — from one on
/// a table this project does not declare.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Limitation {
    pub table: TableName,
    pub detail: String,
}

/// The result of a pull: the schema, and everything that could not be said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pulled {
    pub schema: Schema,
    /// Every limitation, rendered, in the order a reader should see them.
    /// Never empty silence — see this module's own documentation.
    pub warnings: Vec<String>,
    pub limitations: Vec<Limitation>,
}

/// The engine's referential action characters.
///
/// `RESTRICT` has no [`ReferentialAction`] to map onto, and the difference is
/// not cosmetic: `NO ACTION` is deferrable and checked at the end of the
/// statement, `RESTRICT` is not. Folding one into the other would let a plan
/// silently replace a foreign key's behaviour, so it is refused and reported.
fn referential_action(c: char) -> Option<ReferentialAction> {
    match c {
        'a' => Some(ReferentialAction::NoAction),
        'c' => Some(ReferentialAction::Cascade),
        'n' => Some(ReferentialAction::SetNull),
        'd' => Some(ReferentialAction::SetDefault),
        _ => None,
    }
}

fn action_name(c: char) -> &'static str {
    match c {
        'a' => "NO ACTION",
        'r' => "RESTRICT",
        'c' => "CASCADE",
        'n' => "SET NULL",
        'd' => "SET DEFAULT",
        _ => "an action this engine spells with a character pbps does not know",
    }
}

/// The answers the whole pull shares, built once before any table is
/// assembled.
///
/// A struct rather than four parameters: each is a question one arm asks, and
/// the alternative was a signature long enough that adding the next one would
/// mean reading every call site to see what moved.
struct Lookups<'a> {
    /// Every table in the pull, by oid — a foreign key's target.
    tables: HashMap<i64, TableName>,
    /// Every column in the pull, by table and attnum. A foreign key's
    /// `confkey` names attnums on the **referenced** table, which the
    /// constrained table's own map cannot answer for (DECISIONS 250).
    columns: HashMap<(i64, i32), &'a str>,
    /// The indexes that admit at most one null key. A unique constraint is
    /// enforced by an index, so this is a property of the constraint too, and
    /// the constraint arm cannot see the index.
    nulls_not_distinct: BTreeSet<i64>,
    /// The indexes that carry an `INCLUDE` payload, for the same reason: a key
    /// constraint's payload lives on its backing index and nowhere in
    /// `pg_constraint`.
    covering: BTreeSet<i64>,
}

/// Everything a table's own assembly needs, gathered once.
struct Parts<'a> {
    name: TableName,
    /// attnum to column name. Built from the live columns, so a dropped slot
    /// is simply not in it — which is what makes an unresolvable attnum a
    /// reportable fact rather than an off-by-one.
    by_attnum: HashMap<i32, &'a str>,
}

impl Parts<'_> {
    /// The names behind a list of attnums, or the first attnum that has none.
    fn names(&self, attnums: &[i32]) -> Result<Vec<String>, i32> {
        attnums
            .iter()
            .map(|n| self.by_attnum.get(n).map(|s| (*s).to_owned()).ok_or(*n))
            .collect()
    }
}

/// Turns one pull's rows into a [`Schema`] and the list of what it could not
/// hold.
///
/// Pure: every decision here is a function of `raw`, and every one of them is
/// reachable from a unit test.
pub fn assemble(raw: &RawCatalog) -> Pulled {
    let mut pulled = Pulled::default();

    let columns_by_table = group(&raw.columns, |c| c.table_oid);
    let lookups = Lookups {
        tables: raw
            .tables
            .iter()
            .map(|t| (t.oid, TableName::new(&t.schema, &t.name)))
            .collect(),
        columns: raw
            .columns
            .iter()
            .map(|c| ((c.table_oid, c.attnum), c.name.as_str()))
            .collect(),
        nulls_not_distinct: raw
            .indexes
            .iter()
            .filter(|i| i.nulls_not_distinct)
            .map(|i| i.oid)
            .collect(),
        covering: raw
            .indexes
            .iter()
            .filter(|i| i.columns.len() > i.key_count)
            .map(|i| i.oid)
            .collect(),
    };
    let constraints_by_table = group(&raw.constraints, |c| c.table_oid);
    let indexes_by_table = group(&raw.indexes, |i| i.table_oid);

    // An index that enforces a constraint is that constraint, and reporting it
    // again under `indexes:` would make every primary key look like a primary
    // key plus a unique index — a difference the differ would then try to
    // remove, by dropping an index the engine will not let go of.
    //
    // Only the kinds whose index is *their own*. **Measured**: a foreign key's
    // `conindid` is the unique index on the **referenced** table that it points
    // at — an ordinary standalone index, belonging to another table and
    // enforcing nothing for this constraint. Skipping it dropped the only
    // uniqueness the foreign key is valid against, so the pull compared clean
    // and the schema it described could not be built.
    let constraint_indexes: BTreeSet<i64> = raw
        .constraints
        .iter()
        .filter(|c| matches!(c.kind, 'p' | 'u' | 'x'))
        .filter_map(|c| c.index_oid)
        .collect();

    for raw_table in &raw.tables {
        let name = TableName::new(&raw_table.schema, &raw_table.name);
        let raw_columns = columns_by_table
            .get(&raw_table.oid)
            .map_or(&[][..], Vec::as_slice);
        let parts = Parts {
            name: name.clone(),
            by_attnum: raw_columns
                .iter()
                .map(|c| (c.attnum, c.name.as_str()))
                .collect(),
        };

        let mut table = Table {
            columns: raw_columns
                .iter()
                .map(|c| (c.name.clone(), column(c, &parts, &mut pulled)))
                .collect(),
            ..Table::default()
        };

        for constraint in constraints_by_table
            .get(&raw_table.oid)
            .map_or(&[][..], Vec::as_slice)
        {
            add_constraint(constraint, &parts, &lookups, &mut table, &mut pulled);
        }
        for index in indexes_by_table
            .get(&raw_table.oid)
            .map_or(&[][..], Vec::as_slice)
        {
            if constraint_indexes.contains(&index.oid) {
                continue;
            }
            add_index(index, &parts, &mut table, &mut pulled);
        }

        pulled.schema.tables.insert(name, table);
    }

    pulled
}

fn group<T, K: Ord + Copy>(items: &[T], key: impl Fn(&T) -> K) -> BTreeMap<K, Vec<&T>> {
    let mut out: BTreeMap<K, Vec<&T>> = BTreeMap::new();
    for item in items {
        out.entry(key(item)).or_default().push(item);
    }
    out
}

/// The `CACHE` a sequence has when nothing asks for one — **measured**, and the
/// one a plan that recreates an identity would get.
const DEFAULT_SEQUENCE_CACHE: i64 = 1;

fn note(pulled: &mut Pulled, table: &TableName, detail: String) {
    pulled.warnings.push(detail.clone());
    pulled.limitations.push(Limitation {
        table: table.clone(),
        detail,
    });
}

/// One column, and everything about it the model has nowhere to put.
fn column(raw: &RawColumn, parts: &Parts, pulled: &mut Pulled) -> Column {
    // The type is the engine's own spelling, so it is already in the form
    // `normalize` promises to return (ADR-0011 Amendment 3). Parsing it can
    // still fail, and it must fail loudly: a column whose type pbps cannot
    // read is not a column with no type.
    let ty = raw.ty.parse::<ColumnType>().ok().filter(|t| {
        types::normalize(t)
            .map(|normalized| &normalized == t)
            .unwrap_or(false)
    });
    let ty = match ty {
        Some(ty) => ty,
        None => {
            note(
                pulled,
                &parts.name,
                format!(
                    "column `{}`.`{}` has the type `{}`, which this dialect's catalogue cannot \
                     spell. It is read back as an opaque type, so a plan will not try to change \
                     it and drift on it cannot be seen (issue #130).",
                    parts.name, raw.name, raw.ty
                ),
            );
            // Kept as the engine spelled it. A type pbps cannot normalize
            // compares unequal to every declaration, so the differ reports a
            // change it will refuse to emit rather than reporting nothing.
            ColumnType::new(raw.ty.clone(), Vec::new())
        }
    };

    // **Measured**: a generated column's expression is stored in `pg_attrdef`,
    // the same place an ordinary default lives, so the columns query hands it
    // back through `default_expr`. Kept, it would read back as
    // `DEFAULT (id * 2)` — a value computed once at insert where the live
    // column is recomputed on every write, and a declaration that recreates
    // the table would silently produce the first.
    if raw.generated {
        note(
            pulled,
            &parts.name,
            format!(
                "column `{}`.`{}` is a generated column, which this model does not hold. Its \
                 expression is `{}`, and it is **not** read back as a `default:` — a default is \
                 computed once when a row is inserted, and this is recomputed on every write.",
                parts.name,
                raw.name,
                raw.default.as_deref().unwrap_or("not readable")
            ),
        );
    }

    // A `serial` is not a type (DECISIONS 227): the column is an `integer`
    // whose default is `nextval(...)`, and the sequence that default needs is a
    // separate object with nowhere to live in this model. The column is carried
    // — it is exactly what the model says — and the sequence is named, because
    // a declaration pulled from here cannot recreate this table in an empty
    // database.
    if let Some(sequence) = &raw.owned_sequence {
        note(
            pulled,
            &parts.name,
            format!(
                "column `{}`.`{}` defaults from the sequence `{sequence}`, which it owns — the \
                 shape `serial` creates. This model holds the default and has nowhere to put the \
                 sequence, so a declaration pulled from here does not create it, and a rebuild \
                 of this table can drop it before the default is applied again.",
                parts.name, raw.name
            ),
        );
    }

    // A collation decides comparison, ordering and therefore which values a
    // unique key calls equal. Read back as an ordinary column, one collated
    // `"C"` compares equal to one that is not, and a recreated table accepts a
    // different set of values.
    if let Some(collation) = &raw.collation {
        note(
            pulled,
            &parts.name,
            format!(
                "column `{}`.`{}` is `COLLATE \"{collation}\"`, and this model holds only the \
                 type. Read back it is a column with the type's own collation, which orders and \
                 compares differently — so a unique key over it accepts a different set of \
                 values.",
                parts.name, raw.name
            ),
        );
    }

    let identity = raw.identity.map(|id| {
        if !id.always {
            note(
                pulled,
                &parts.name,
                format!(
                    "column `{}`.`{}` is `GENERATED BY DEFAULT AS IDENTITY`, and this model holds \
                     only the seed and the increment. Read back it is indistinguishable from \
                     `GENERATED ALWAYS`, which is the one a plan would emit.",
                    parts.name, raw.name
                ),
            );
        }
        // The bounds this engine gives a sequence the model does not spell
        // (`types::identity_seed_range`) are the ones a plan would recreate.
        // Anything else — a `MINVALUE`, a `MAXVALUE`, a `CYCLE` — reads back as
        // an identity that runs out, or wraps, somewhere else.
        let expected = types::identity_seed_range(&ty, id.increment);
        let bounds_are_the_default =
            expected.is_some_and(|range| id.min == *range.start() && id.max == *range.end());
        if !bounds_are_the_default || id.cycles {
            note(
                pulled,
                &parts.name,
                format!(
                    "column `{}`.`{}` has an `identity:` whose sequence runs {}..={}{}, and this \
                     model holds only the seed and the increment. Read back it is an identity \
                     with this engine's default bounds, which runs out — or wraps — somewhere \
                     else.",
                    parts.name,
                    raw.name,
                    id.min,
                    id.max,
                    if id.cycles { " and cycles" } else { "" }
                ),
            );
        }
        // `CACHE` is not a bound, so it earns its own sentence: what it
        // changes is not where the sequence stops but which values are handed
        // out, and how many are lost when a session ends.
        if id.cache != DEFAULT_SEQUENCE_CACHE {
            note(
                pulled,
                &parts.name,
                format!(
                    "column `{}`.`{}` has an `identity:` whose sequence is `CACHE {}`, and this \
                     model holds only the seed and the increment. Read back it is an identity \
                     with this engine's default `CACHE {DEFAULT_SEQUENCE_CACHE}`: recreating it \
                     changes how many values a session takes at once, and so how many are \
                     skipped when one ends.",
                    parts.name, raw.name, id.cache
                ),
            );
        }
        Identity {
            seed: id.seed,
            increment: id.increment,
        }
    });

    Column {
        ty,
        nullable: raw.nullable,
        // Verbatim, cast and all: see this module's own documentation — except
        // for a generated column, whose expression shares `pg_attrdef` with
        // the defaults and is not one.
        default: (!raw.generated).then(|| raw.default.clone()).flatten(),
        identity,
        description: None,
        deprecated: None,
    }
}

fn add_constraint(
    raw: &RawConstraint,
    parts: &Parts,
    lookups: &Lookups,
    table: &mut Table,
    pulled: &mut Pulled,
) {
    match raw.kind {
        // PostgreSQL 18 gives every NOT NULL a `pg_constraint` row. The column
        // already carries it, and a reader that let this fall through to the
        // check arm would report one phantom check per NOT NULL column.
        'n' => {}

        // `NOT ENFORCED` is not `NOT VALID`, and the difference is the whole
        // constraint: a `NOT VALID` one still checks every new row, while this
        // one checks nothing and never will. Carried as an ordinary constraint
        // it would be recreated as one that enforces, and the rebuilt schema
        // would start refusing writes the live database accepts.
        'p' | 'u' | 'f' | 'c' | 'x' if !raw.enforced => {
            note(
                pulled,
                &parts.name,
                format!(
                    "constraint `{}` on `{}` is `NOT ENFORCED`: the engine records it and checks \
                     nothing against it, ever. This model has no word for that — read back it is \
                     an ordinary constraint, which a plan would recreate as one that enforces — \
                     so it is left out. Its definition is `{}`.",
                    raw.name, parts.name, raw.definition
                ),
            );
        }

        // `NO INHERIT` is a check that stops at this table. Read back as an
        // ordinary one it would be recreated as a check the children have too,
        // which refuses rows they accept today. Found by sweeping for the
        // shape `conenforced` has: a flag beside an unchanged `contype`.
        'c' if raw.no_inherit => {
            note(
                pulled,
                &parts.name,
                format!(
                    "check constraint `{}` on `{}` is `NO INHERIT`: it holds for this table's own \
                     rows and for no table that inherits from it. This model holds only the \
                     expression, so it is left out rather than read back as a check a plan would \
                     recreate on the children too. Its definition is `{}`.",
                    raw.name, parts.name, raw.definition
                ),
            );
        }

        // A temporal key keeps the `contype` of an ordinary one, so nothing but
        // `conperiod` tells them apart. `WITHOUT OVERLAPS` makes uniqueness a
        // question about ranges rather than values, and `PERIOD` makes a
        // foreign key ask whether the referencing row's range is *covered*.
        'p' | 'u' | 'f' | 'c' | 'x' if raw.period => {
            note(
                pulled,
                &parts.name,
                format!(
                    "constraint `{}` on `{}` is temporal — `WITHOUT OVERLAPS` on a key, `PERIOD` \
                     on a foreign key — and it carries the `contype` of an ordinary one. What it \
                     asks is about ranges, not values, and this model holds only the columns, so \
                     it is left out rather than read back as the constraint it is not. Its \
                     definition is `{}`.",
                    raw.name, parts.name, raw.definition
                ),
            );
        }

        // A key whose check can be put off is a different key, and the model
        // holds neither word. Named on every kind that can carry it, before
        // the kind's own arm decides what to do with the rest of it.
        // A key constraint whose backing index carries an `INCLUDE` payload.
        // `PrimaryKey` and `UniqueConstraint` hold their key columns and
        // nothing else, so the payload cannot be read back — and it is the
        // reason the index exists in the shape it does.
        'p' | 'u'
            if raw
                .index_oid
                .is_some_and(|oid| lookups.covering.contains(&oid)) =>
        {
            note(
                pulled,
                &parts.name,
                format!(
                    "constraint `{}` on `{}` is enforced by an index with an `INCLUDE` payload, \
                     and this model holds a key constraint's key columns and nothing else. Read \
                     back it is the same constraint over a narrower index, so it is left out \
                     rather than read back as one that covers less than it does.",
                    raw.name, parts.name
                ),
            );
        }

        // A unique key whose index admits more than one null key is a
        // different key, and the model holds only "unique".
        'p' | 'u'
            if raw
                .index_oid
                .is_some_and(|oid| lookups.nulls_not_distinct.contains(&oid)) =>
        {
            note(
                pulled,
                &parts.name,
                format!(
                    "constraint `{}` on `{}` is enforced by a `NULLS NOT DISTINCT` index, so it \
                     admits at most one null key where an ordinary unique constraint admits any \
                     number. This model holds only `unique`, so the constraint is left out \
                     rather than read back as the weaker one it is not.",
                    raw.name, parts.name
                ),
            );
        }

        'p' | 'u' | 'f' if raw.deferrable => {
            note(
                pulled,
                &parts.name,
                format!(
                    "constraint `{}` on `{}` is `DEFERRABLE{}`, and this model holds neither \
                     word. Read back as an ordinary constraint it compares equal to one that is \
                     checked immediately — so a transaction that relies on `SET CONSTRAINTS`, or \
                     on rows that violate it in the middle of one, would break with no plan \
                     saying anything had changed. Its definition is `{}`.",
                    raw.name,
                    parts.name,
                    if raw.deferred {
                        " INITIALLY DEFERRED"
                    } else {
                        ""
                    },
                    raw.definition
                ),
            );
        }

        'p' => match parts.names(&raw.columns) {
            Ok(columns) => {
                table.primary_key = Some(PrimaryKey {
                    name: Some(raw.name.clone()),
                    columns,
                });
            }
            Err(attnum) => unresolved(pulled, parts, "primary key", &raw.name, attnum),
        },

        'u' => match parts.names(&raw.columns) {
            Ok(columns) => {
                table
                    .unique
                    .insert(raw.name.clone(), UniqueConstraint { columns });
            }
            Err(attnum) => unresolved(pulled, parts, "unique constraint", &raw.name, attnum),
        },

        'c' => {
            if !raw.validated {
                note(
                    pulled,
                    &parts.name,
                    format!(
                        "check constraint `{}` on `{}` is `NOT VALID`: it holds for new rows and \
                         has never been checked against the rows already there. This model holds \
                         only the expression, so it is read back as an ordinary check.",
                        raw.name, parts.name
                    ),
                );
            }
            // `pg_get_constraintdef` returns the whole clause — measured,
            // `CHECK ((id > 0))` — and `CheckConstraint::expression` is the
            // inside of it, which every emitter wraps. Stored whole it would
            // emit `CHECK (CHECK ((id > 0)))`, and no declaration a person
            // would write could ever converge with it.
            let Some(expression) = raw.expression.clone() else {
                note(
                    pulled,
                    &parts.name,
                    format!(
                        "check constraint `{}` on `{}` has no readable expression, so it is left \
                         out. Its definition is `{}`.",
                        raw.name, parts.name, raw.definition
                    ),
                );
                return;
            };
            table.checks.insert(
                raw.name.clone(),
                // Verbatim, including the parentheses and casts the engine
                // welded on.
                CheckConstraint { expression },
            );
        }

        'f' => add_foreign_key(raw, parts, lookups, table, pulled),

        'x' => note(
            pulled,
            &parts.name,
            format!(
                "`{}` on `{}` is an exclusion constraint, which this model does not hold. It is \
                 left out of the pull, so a plan cannot see it and `verify` cannot report a \
                 change to it.",
                raw.name, parts.name
            ),
        ),

        other => note(
            pulled,
            &parts.name,
            format!(
                "constraint `{}` on `{}` is of kind `{other}`, which this dialect does not read. \
                 Its definition is `{}`.",
                raw.name, parts.name, raw.definition
            ),
        ),
    }
}

fn add_foreign_key(
    raw: &RawConstraint,
    parts: &Parts,
    lookups: &Lookups,
    table: &mut Table,
    pulled: &mut Pulled,
) {
    // The same rule the check arm applies, and for the same reason: a key
    // added `NOT VALID` holds for new rows and has never been checked against
    // the rows already there. Read back as an ordinary key it compares equal to
    // one that has, and recreating it validates every existing row — which can
    // fail on data the live key legally tolerates.
    if !raw.validated {
        note(
            pulled,
            &parts.name,
            format!(
                "foreign key `{}` on `{}` is `NOT VALID`: rows already there have never been \
                 checked against it, and this model holds only the key. Read back it is an \
                 ordinary foreign key, which is one that would be validated on being recreated.",
                raw.name, parts.name
            ),
        );
    }

    let Some(references_table) = raw
        .ref_table
        .and_then(|oid| lookups.tables.get(&oid))
        .cloned()
    else {
        note(
            pulled,
            &parts.name,
            format!(
                "foreign key `{}` on `{}` references a table this pull did not read, so it is \
                 left out. Its definition is `{}`.",
                raw.name, parts.name, raw.definition
            ),
        );
        return;
    };

    // `ON DELETE SET NULL (a)` nulls one column; the model's `SetNull` nulls
    // every referencing column. Read back as the ordinary form, recreating the
    // key turns a targeted write into a wholesale one.
    if !raw.delete_set_columns.is_empty() {
        note(
            pulled,
            &parts.name,
            format!(
                "foreign key `{}` on `{}` names the columns its `ON DELETE SET` action touches, \
                 and this model holds only the action. Read back it would null or default every \
                 referencing column instead of the named ones, so it is left out. Its definition \
                 is `{}`.",
                raw.name, parts.name, raw.definition
            ),
        );
        return;
    }

    // MATCH SIMPLE is the default and the only one this model can mean.
    // MATCH FULL refuses a row whose referencing columns are partly null,
    // which MATCH SIMPLE accepts — so a composite key read back as an ordinary
    // one is a constraint that has quietly stopped rejecting those rows.
    if raw.match_type != 's' {
        note(
            pulled,
            &parts.name,
            format!(
                "foreign key `{}` on `{}` is `MATCH {}`, and this model holds only the default \
                 `MATCH SIMPLE`. The difference decides what happens to a row whose referencing \
                 columns are partly null — accepted by SIMPLE, refused by FULL — so the key is \
                 left out rather than read back as the one it is not.",
                raw.name,
                parts.name,
                match raw.match_type {
                    'f' => "FULL",
                    'p' => "PARTIAL",
                    other => return unknown_match(pulled, parts, &raw.name, other),
                }
            ),
        );
        return;
    }

    let Ok(columns) = parts.names(&raw.columns) else {
        unresolved(
            pulled,
            parts,
            "foreign key",
            &raw.name,
            *raw.columns.last().unwrap_or(&0),
        );
        return;
    };

    // The referenced columns are attnums **on the other table**, resolved
    // against that table's own columns.
    let references_columns = match reference_columns(raw, &lookups.columns) {
        Some(names) => names,
        None => {
            note(
                pulled,
                &parts.name,
                format!(
                    "foreign key `{}` on `{}` names columns on `{references_table}` that this \
                     pull cannot resolve, so it is left out. Its definition is `{}`.",
                    raw.name, parts.name, raw.definition
                ),
            );
            return;
        }
    };

    let (Some(on_delete), Some(on_update)) = (
        referential_action(raw.on_delete),
        referential_action(raw.on_update),
    ) else {
        note(
            pulled,
            &parts.name,
            format!(
                "foreign key `{}` on `{}` is `ON DELETE {}` `ON UPDATE {}`, and this model holds \
                 no `RESTRICT`. The difference is not cosmetic — `NO ACTION` is checked at the \
                 end of the statement and can be deferred, `RESTRICT` cannot — so the key is \
                 left out rather than read back as the action it is not.",
                raw.name,
                parts.name,
                action_name(raw.on_delete),
                action_name(raw.on_update)
            ),
        );
        return;
    };

    table.foreign_keys.insert(
        raw.name.clone(),
        ForeignKey {
            columns,
            references_table,
            references_columns,
            on_delete,
            on_update,
        },
    );
}

/// The referenced columns of a foreign key.
///
/// `confkey` holds attnums on the **referenced** table, which the constrained
/// table's map cannot answer for. A first version read them out of
/// `pg_get_constraintdef` instead, on the grounds that a second catalog join
/// would put the answer where no test can reach it. Measured, that parse is
/// wrong on a legal name: `FOREIGN KEY (x, y) REFERENCES q(x, "a)b")` stops
/// inside the quoted identifier, yields two items, passes a count check against
/// `confkey`, and records the column `"a`. The columns of every table in the
/// pull are already here, so they answer it instead — no parse, and no second
/// query either (DECISIONS 250, superseding 247).
fn reference_columns(
    raw: &RawConstraint,
    by_table_and_attnum: &HashMap<(i64, i32), &str>,
) -> Option<Vec<String>> {
    let table = raw.ref_table?;
    raw.ref_columns
        .iter()
        .map(|attnum| {
            by_table_and_attnum
                .get(&(table, *attnum))
                .map(|name| (*name).to_owned())
        })
        .collect()
}

/// A match type this engine has and this code has never seen.
fn unknown_match(pulled: &mut Pulled, parts: &Parts, name: &str, kind: char) {
    note(
        pulled,
        &parts.name,
        format!(
            "foreign key `{name}` on `{}` has the match type `{kind}`, which this dialect does \
             not know. It is left out rather than read back as the default one.",
            parts.name
        ),
    );
}

fn add_index(raw: &RawIndex, parts: &Parts, table: &mut Table, pulled: &mut Pulled) {
    if raw.method != "btree" {
        note(
            pulled,
            &parts.name,
            format!(
                "index `{}` on `{}` uses the `{}` method, and this model holds only the default \
                 one. It is left out of the pull, so a plan cannot see it.",
                raw.name, parts.name, raw.method
            ),
        );
        return;
    }
    if raw.has_expressions || raw.columns.contains(&0) {
        note(
            pulled,
            &parts.name,
            format!(
                "index `{}` on `{}` is over an expression, which this model does not hold. It is \
                 left out of the pull, so a plan cannot see it.",
                raw.name, parts.name
            ),
        );
        return;
    }
    // A failed `CREATE INDEX CONCURRENTLY` leaves a row behind that the
    // planner will not use. Read back as an index, a declaration of the same
    // shape compares clean — and the index the operator believes they have
    // does not exist.
    if !raw.valid {
        note(
            pulled,
            &parts.name,
            format!(
                "index `{}` on `{}` is `indisvalid = false`: the engine keeps the row and the \
                 planner does not use it, which is what a `CREATE INDEX CONCURRENTLY` that \
                 failed leaves behind. It is left out of the pull, so a declaration of the same \
                 shape reads as an index that is missing rather than one that is present.",
                raw.name, parts.name
            ),
        );
        return;
    }
    if raw.nondefault_column_options {
        note(
            pulled,
            &parts.name,
            format!(
                "index `{}` on `{}` orders a column by an operator class or a collation that is \
                 not its type's own — `text_pattern_ops` and `COLLATE \"C\"` are the two that \
                 come up — and `IndexColumn` holds only the name and the direction. Read back it \
                 is an ordinary index, which answers different queries and compares different \
                 values equal, so it is left out.",
                raw.name, parts.name
            ),
        );
        return;
    }
    if raw.nulls_not_distinct {
        note(
            pulled,
            &parts.name,
            format!(
                "index `{}` on `{}` is `NULLS NOT DISTINCT`, so it admits at most one null key \
                 where an ordinary unique index admits any number. This model holds only \
                 `unique:`, so the index is left out rather than read back as the weaker one it \
                 is not.",
                raw.name, parts.name
            ),
        );
        return;
    }
    if raw.exclusion {
        note(
            pulled,
            &parts.name,
            format!(
                "index `{}` on `{}` enforces an exclusion constraint, which this model does not \
                 hold.",
                raw.name, parts.name
            ),
        );
        return;
    }

    let key_count = raw.key_count.min(raw.columns.len());
    let (keys, included) = raw.columns.split_at(key_count);
    let (Ok(keys), Ok(include)) = (parts.names(keys), parts.names(included)) else {
        unresolved(
            pulled,
            parts,
            "index",
            &raw.name,
            *raw.columns.last().unwrap_or(&0),
        );
        return;
    };

    // Bit 0 is DESC and bit 1 is NULLS FIRST, and the model holds only the
    // first. A default `DESC` implies `NULLS FIRST` and a default `ASC` implies
    // `NULLS LAST`, so only the two crossed spellings are a difference — those
    // are reported rather than dropped.
    let mut columns = Vec::with_capacity(keys.len());
    for (position, name) in keys.into_iter().enumerate() {
        let option = raw.options.get(position).copied().unwrap_or(0);
        let descending = option & 1 == 1;
        let nulls_first = option & 2 == 2;
        if nulls_first != descending {
            note(
                pulled,
                &parts.name,
                format!(
                    "index `{}` on `{}` orders `{name}` `{}` `NULLS {}`, and this model holds \
                     only the direction. Read back it is the default ordering for that \
                     direction, which puts the nulls at the other end.",
                    raw.name,
                    parts.name,
                    if descending { "DESC" } else { "ASC" },
                    if nulls_first { "FIRST" } else { "LAST" },
                ),
            );
        }
        columns.push(IndexColumn { name, descending });
    }

    table.indexes.insert(
        raw.name.clone(),
        Index {
            columns,
            include,
            unique: raw.unique,
            // Verbatim: see this module's own documentation.
            filter: raw.filter.clone(),
        },
    );
}

/// An attnum with no live column behind it.
///
/// This is the third thing CLAUDE.md's rule names — unreadable — and it must
/// not read as either of the other two. It happens when a pull reads the
/// constraints of a table whose columns it could not read, which is what a
/// permission the connection lacks looks like from here.
fn unresolved(pulled: &mut Pulled, parts: &Parts, kind: &str, name: &str, attnum: i32) {
    note(
        pulled,
        &parts.name,
        format!(
            "{kind} `{name}` on `{}` names column number {attnum}, which this pull did not read \
             back. It is left out rather than read back with a column missing, because a key with \
             fewer columns than it has is a different key.",
            parts.name
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(oid: i64, name: &str) -> RawTable {
        RawTable {
            oid,
            schema: "app".to_owned(),
            name: name.to_owned(),
        }
    }

    fn col(table_oid: i64, attnum: i32, name: &str, ty: &str) -> RawColumn {
        RawColumn {
            table_oid,
            attnum,
            name: name.to_owned(),
            ty: ty.to_owned(),
            nullable: true,
            default: None,
            identity: None,
            generated: false,
            owned_sequence: None,
            collation: None,
        }
    }

    fn constraint(table_oid: i64, name: &str, kind: char) -> RawConstraint {
        RawConstraint {
            table_oid,
            name: name.to_owned(),
            kind,
            columns: Vec::new(),
            ref_columns: Vec::new(),
            ref_table: None,
            on_delete: 'a',
            on_update: 'a',
            validated: true,
            deferrable: false,
            deferred: false,
            definition: String::new(),
            expression: None,
            match_type: 's',
            delete_set_columns: Vec::new(),
            index_oid: None,
            enforced: true,
            period: false,
            no_inherit: false,
        }
    }

    fn index(oid: i64, table_oid: i64, name: &str) -> RawIndex {
        RawIndex {
            oid,
            table_oid,
            name: name.to_owned(),
            unique: false,
            primary: false,
            exclusion: false,
            valid: true,
            nulls_not_distinct: false,
            key_count: 1,
            columns: vec![1],
            options: vec![0],
            filter: None,
            has_expressions: false,
            method: "btree".to_owned(),
            nondefault_column_options: false,
        }
    }

    fn only(pulled: &Pulled) -> &Table {
        pulled.schema.tables.values().next().expect("one table")
    }

    /// The measured shape of a table with a dropped column: the slot is gone
    /// from the pull, and `attnum` keeps the hole, so everything that refers to
    /// a column refers to it by number and not by position.
    #[test]
    fn a_column_is_found_by_its_attnum_and_never_by_its_position() {
        let mut pk = constraint(1, "t_pk", 'p');
        pk.columns = vec![4];
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            // attnum 3 was dropped: 1, 2, 4.
            columns: vec![
                col(1, 1, "a", "integer"),
                col(1, 2, "b", "integer"),
                col(1, 4, "d", "integer"),
            ],
            constraints: vec![pk],
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert!(pulled.warnings.is_empty(), "{:?}", pulled.warnings);
        assert_eq!(
            only(&pulled).primary_key.as_ref().unwrap().columns,
            ["d"],
            "attnum 4 is the third column, and taking it as the fourth would \
             have found nothing at all"
        );
    }

    /// An attnum with no column behind it is the third thing in CLAUDE.md's
    /// rule. It must not silently shorten the key.
    #[test]
    fn a_key_naming_a_column_the_pull_did_not_read_is_reported_and_left_out() {
        let mut pk = constraint(1, "t_pk", 'p');
        pk.columns = vec![1, 9];
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "a", "integer")],
            constraints: vec![pk],
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert!(only(&pulled).primary_key.is_none());
        assert_eq!(pulled.limitations.len(), 1);
        assert!(pulled.warnings[0].contains("column number 9"));
    }

    /// PostgreSQL 18 catalogues every NOT NULL as a constraint row. Read as a
    /// check, each one would be a phantom constraint the differ tries to drop.
    #[test]
    fn a_catalogued_not_null_is_not_read_back_as_a_check() {
        let mut not_null = constraint(1, "t_a_not_null", 'n');
        not_null.columns = vec![1];
        not_null.definition = "NOT NULL a".to_owned();
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "a", "integer")],
            constraints: vec![not_null],
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert!(only(&pulled).checks.is_empty());
        assert!(pulled.warnings.is_empty(), "{:?}", pulled.warnings);
    }

    /// A constraint kind nobody has taught this reader about is reported, not
    /// dropped and not guessed at. The NOT NULL row above is what this rule
    /// exists for: it arrived in an engine upgrade.
    #[test]
    fn a_constraint_kind_this_reader_does_not_know_is_reported() {
        let mut odd = constraint(1, "t_odd", 'z');
        odd.definition = "SOMETHING (a)".to_owned();
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "a", "integer")],
            constraints: vec![odd],
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert_eq!(pulled.limitations.len(), 1);
        assert!(pulled.warnings[0].contains("kind `z`"));
        assert!(pulled.warnings[0].contains("SOMETHING (a)"));
    }

    /// The index behind a primary key or a unique constraint is that
    /// constraint. Reported twice, the differ would try to drop an index the
    /// engine does not let go of.
    #[test]
    fn the_index_that_enforces_a_constraint_is_not_reported_again_as_an_index() {
        let mut pk = constraint(1, "t_pk", 'p');
        pk.columns = vec![1];
        pk.index_oid = Some(50);
        let mut backing = index(50, 1, "t_pk");
        backing.unique = true;
        backing.primary = true;
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "a", "integer")],
            constraints: vec![pk],
            indexes: vec![backing, index(51, 1, "t_a_ix")],
        };
        let pulled = assemble(&raw);
        assert!(only(&pulled).primary_key.is_some());
        assert_eq!(only(&pulled).indexes.keys().collect::<Vec<_>>(), ["t_a_ix"]);
    }

    /// `RESTRICT` is not `NO ACTION`: one is checked at the end of the
    /// statement and can be deferred, the other cannot. The model holds only
    /// the first, so the key is reported rather than read back as the wrong
    /// one.
    #[test]
    fn a_foreign_key_the_model_cannot_spell_the_action_of_is_left_out_and_named() {
        let mut fk = constraint(1, "t_fk", 'f');
        fk.columns = vec![1];
        fk.ref_columns = vec![1];
        fk.ref_table = Some(2);
        fk.on_delete = 'r';
        fk.definition = "FOREIGN KEY (a) REFERENCES app.other(x) ON DELETE RESTRICT".to_owned();
        let raw = RawCatalog {
            tables: vec![table(1, "t"), table(2, "other")],
            columns: vec![col(1, 1, "a", "integer"), col(2, 1, "x", "integer")],
            constraints: vec![fk],
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert!(
            pulled.schema.tables[&TableName::new("app", "t")]
                .foreign_keys
                .is_empty()
        );
        assert!(pulled.warnings[0].contains("RESTRICT"));
    }

    /// `confkey` names attnums on the **referenced** table, so they are
    /// resolved against that table's own columns. The name here contains the
    /// `)` that the first version of this — a parse of
    /// `pg_get_constraintdef` — stopped inside of, recording `\"a` and passing
    /// its own count check.
    #[test]
    fn a_foreign_keys_referenced_columns_come_from_the_referenced_table() {
        let mut fk = constraint(1, "t_fk", 'f');
        fk.columns = vec![1, 2];
        fk.ref_columns = vec![1, 2];
        fk.ref_table = Some(2);
        fk.definition =
            "FOREIGN KEY (a, b) REFERENCES app.other(\"a)b\", z) ON UPDATE CASCADE".to_owned();
        fk.on_update = 'c';
        let raw = RawCatalog {
            tables: vec![table(1, "t"), table(2, "other")],
            columns: vec![
                col(1, 1, "a", "integer"),
                col(1, 2, "b", "integer"),
                col(2, 1, "a)b", "integer"),
                col(2, 2, "z", "integer"),
            ],
            constraints: vec![fk],
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        let key = &pulled.schema.tables[&TableName::new("app", "t")].foreign_keys["t_fk"];
        assert_eq!(key.references_columns, ["a)b", "z"]);
        assert_eq!(key.on_update, ReferentialAction::Cascade);
        assert!(pulled.warnings.is_empty(), "{:?}", pulled.warnings);
    }

    /// An attnum on the referenced table that this pull did not read is the
    /// third thing again: the key is left out and named, never shortened.
    #[test]
    fn a_referenced_column_the_pull_did_not_read_leaves_the_key_out() {
        let mut fk = constraint(1, "t_fk", 'f');
        fk.columns = vec![1];
        fk.ref_columns = vec![1, 2];
        fk.ref_table = Some(2);
        fk.definition = "FOREIGN KEY (a) REFERENCES app.other(x)".to_owned();
        let raw = RawCatalog {
            tables: vec![table(1, "t"), table(2, "other")],
            columns: vec![col(1, 1, "a", "integer"), col(2, 1, "x", "integer")],
            constraints: vec![fk],
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert!(
            pulled.schema.tables[&TableName::new("app", "t")]
                .foreign_keys
                .is_empty()
        );
        assert!(pulled.warnings[0].contains("cannot resolve"));
    }

    /// The key columns and the `INCLUDE` payload are one list in the catalog,
    /// split at `indnkeyatts`.
    #[test]
    fn an_index_splits_its_key_from_its_include_at_the_catalogs_own_count() {
        let mut ix = index(50, 1, "t_ix");
        ix.key_count = 2;
        ix.columns = vec![3, 1, 2];
        ix.options = vec![1, 0, 0];
        ix.filter = Some("(c IS NOT NULL)".to_owned());
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![
                col(1, 1, "a", "integer"),
                col(1, 2, "b", "integer"),
                col(1, 3, "c", "integer"),
            ],
            constraints: Vec::new(),
            indexes: vec![ix],
        };
        let pulled = assemble(&raw);
        let read = &only(&pulled).indexes["t_ix"];
        assert_eq!(read.columns[0].name, "c");
        assert!(read.columns[0].descending, "indoption bit 0 is DESC");
        assert_eq!(read.columns[1].name, "a");
        assert_eq!(read.include, ["b"]);
        assert_eq!(read.filter.as_deref(), Some("(c IS NOT NULL)"));
    }

    /// `DESC` defaults to `NULLS FIRST` and `ASC` to `NULLS LAST`, so only the
    /// two crossed spellings are a difference the model cannot hold.
    #[test]
    fn only_a_null_ordering_that_is_not_its_directions_default_is_reported() {
        // Measured: the engine prints the null ordering only for the two
        // crossed spellings, `a DESC NULLS LAST` (1) and `a NULLS FIRST` (2).
        // `a` (0) and `a DESC` (3) print none, which is what makes them the
        // defaults for their direction.
        for (option, reported) in [(0, false), (1, true), (2, true), (3, false)] {
            let mut ix = index(50, 1, "t_ix");
            ix.options = vec![option];
            let raw = RawCatalog {
                tables: vec![table(1, "t")],
                columns: vec![col(1, 1, "a", "integer")],
                constraints: Vec::new(),
                indexes: vec![ix],
            };
            let pulled = assemble(&raw);
            assert_eq!(
                !pulled.warnings.is_empty(),
                reported,
                "indoption {option}: {:?}",
                pulled.warnings
            );
            assert!(only(&pulled).indexes.contains_key("t_ix"));
        }
    }

    /// An index this model cannot hold is named and left out, never quietly
    /// turned into an ordinary one — a `gin` index read back as a btree is a
    /// plan that drops it and builds the wrong thing.
    #[test]
    fn an_index_the_model_cannot_hold_is_named_and_left_out() {
        for (name, mutate) in [
            (
                "method",
                (|ix: &mut RawIndex| ix.method = "gin".to_owned()) as fn(&mut RawIndex),
            ),
            ("expression", |ix: &mut RawIndex| ix.has_expressions = true),
            ("expression column", |ix: &mut RawIndex| {
                ix.columns = vec![0];
            }),
            ("exclusion", |ix: &mut RawIndex| ix.exclusion = true),
        ] {
            let mut ix = index(50, 1, "t_ix");
            mutate(&mut ix);
            let raw = RawCatalog {
                tables: vec![table(1, "t")],
                columns: vec![col(1, 1, "a", "integer")],
                constraints: Vec::new(),
                indexes: vec![ix],
            };
            let pulled = assemble(&raw);
            assert!(only(&pulled).indexes.is_empty(), "{name}");
            assert_eq!(pulled.limitations.len(), 1, "{name}");
        }
    }

    /// The three verbatim expressions arrive as the engine respelled them, and
    /// this half must not tidy any of them (ADR-0009 §2, ADR-0013 §4).
    #[test]
    fn the_respelled_expressions_are_carried_through_untouched() {
        let mut column = col(1, 1, "amount", "numeric(10,2)");
        column.default = Some("(0)::numeric".to_owned());
        let mut check = constraint(1, "t_ck", 'c');
        check.definition = "CHECK ((amount >= (0)::numeric))".to_owned();
        check.expression = Some("(amount >= (0)::numeric)".to_owned());
        let mut ix = index(50, 1, "t_ix");
        ix.filter = Some("(amount IS NOT NULL)".to_owned());
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![column],
            constraints: vec![check],
            indexes: vec![ix],
        };
        let pulled = assemble(&raw);
        let read = only(&pulled);
        assert_eq!(
            read.columns["amount"].default.as_deref(),
            Some("(0)::numeric")
        );
        assert_eq!(
            read.checks["t_ck"].expression, "(amount >= (0)::numeric)",
            "the expression, not the `CHECK (…)` clause an emitter wraps it in"
        );
        assert_eq!(
            read.indexes["t_ix"].filter.as_deref(),
            Some("(amount IS NOT NULL)")
        );
    }

    /// A type the catalogue cannot spell is the one #130 is about, and it is
    /// read back as itself with a word said. Silence here would be a column
    /// pbps believes it understands.
    #[test]
    fn a_type_the_catalogue_cannot_spell_is_kept_and_named() {
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "when", "timestamp(3) with time zone")],
            constraints: Vec::new(),
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert_eq!(pulled.limitations.len(), 1);
        assert!(pulled.warnings[0].contains("issue #130"));
        assert_eq!(
            only(&pulled).columns["when"].ty.to_string(),
            "timestamp(3) with time zone",
            "kept as the engine spelled it, so the differ reports a change it \
             will refuse to emit rather than reporting nothing"
        );
    }

    /// `GENERATED BY DEFAULT` and `GENERATED ALWAYS` read back the same, and a
    /// pull that did not say so would call them equal.
    #[test]
    fn an_identity_the_model_cannot_tell_apart_is_named() {
        for (always, reported) in [(true, false), (false, true)] {
            let mut column = col(1, 1, "id", "integer");
            column.identity = Some(RawIdentity {
                always,
                seed: 5,
                increment: 2,
                min: 1,
                max: i64::from(i32::MAX),
                cycles: false,
                cache: 1,
            });
            let raw = RawCatalog {
                tables: vec![table(1, "t")],
                columns: vec![column],
                constraints: Vec::new(),
                indexes: Vec::new(),
            };
            let pulled = assemble(&raw);
            assert_eq!(!pulled.warnings.is_empty(), reported, "always={always}");
            assert_eq!(
                only(&pulled).columns["id"].identity,
                Some(Identity {
                    seed: 5,
                    increment: 2
                })
            );
        }
    }

    /// A generated column has no home in the model, and reading it back as an
    /// ordinary column with a default it does not have would be worse than
    /// saying so.
    #[test]
    fn a_generated_column_is_named() {
        let mut column = col(1, 1, "total", "integer");
        column.generated = true;
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![column],
            constraints: Vec::new(),
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert_eq!(pulled.limitations.len(), 1);
        assert!(pulled.warnings[0].contains("generated column"));
    }

    /// **Measured**: a generated column's expression lives in `pg_attrdef`,
    /// where an ordinary default lives, so the query hands it back as one.
    /// Kept, it reads back as `DEFAULT (id * 2)` — computed once on insert,
    /// where the live column is recomputed on every write.
    #[test]
    fn a_generated_columns_expression_is_never_read_back_as_a_default() {
        let mut column = col(1, 1, "total", "integer");
        column.generated = true;
        column.default = Some("(id * 2)".to_owned());
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![column],
            constraints: Vec::new(),
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert_eq!(only(&pulled).columns["total"].default, None);
        assert!(pulled.warnings[0].contains("(id * 2)"));
        assert!(pulled.warnings[0].contains("every write"));
    }

    /// A key whose check can be deferred is a different key, and the model has
    /// neither word. It is left out, like every other property of an object
    /// this model cannot hold.
    #[test]
    fn a_deferrable_key_is_left_out_and_named_whatever_kind_it_is() {
        for kind in ['p', 'u', 'f'] {
            let mut c = constraint(1, "t_key", kind);
            c.columns = vec![1];
            c.ref_columns = vec![1];
            c.ref_table = Some(1);
            c.definition = "UNIQUE (a) DEFERRABLE INITIALLY DEFERRED".to_owned();
            c.deferrable = true;
            c.deferred = true;
            let raw = RawCatalog {
                tables: vec![table(1, "t")],
                columns: vec![col(1, 1, "a", "integer")],
                constraints: vec![c],
                indexes: Vec::new(),
            };
            let pulled = assemble(&raw);
            let read = only(&pulled);
            assert!(read.primary_key.is_none(), "{kind}");
            assert!(read.unique.is_empty(), "{kind}");
            assert!(read.foreign_keys.is_empty(), "{kind}");
            assert_eq!(pulled.limitations.len(), 1, "{kind}");
            assert!(pulled.warnings[0].contains("DEFERRABLE INITIALLY DEFERRED"));
        }
    }

    /// `DEFERRABLE` without `INITIALLY DEFERRED` is still a key `SET
    /// CONSTRAINTS` can move, so it is named too — and the message says which
    /// of the two it is.
    #[test]
    fn a_deferrable_key_that_is_initially_immediate_is_named_as_that() {
        let mut c = constraint(1, "t_key", 'u');
        c.columns = vec![1];
        c.deferrable = true;
        c.deferred = false;
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "a", "integer")],
            constraints: vec![c],
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert!(only(&pulled).unique.is_empty());
        assert!(
            pulled.warnings[0].contains("`DEFERRABLE`"),
            "{:?}",
            pulled.warnings
        );
        assert!(!pulled.warnings[0].contains("INITIALLY DEFERRED"));
    }

    /// A foreign key added `NOT VALID` is carried — the key is exactly what the
    /// model says — and named, because recreating it validates rows the live
    /// key legally tolerates. The same rule the check arm follows.
    #[test]
    fn a_not_valid_foreign_key_is_carried_and_named() {
        let mut fk = constraint(1, "t_fk", 'f');
        fk.columns = vec![1];
        fk.ref_columns = vec![1];
        fk.ref_table = Some(2);
        fk.validated = false;
        fk.definition = "FOREIGN KEY (a) REFERENCES app.other(x) NOT VALID".to_owned();
        let raw = RawCatalog {
            tables: vec![table(1, "t"), table(2, "other")],
            columns: vec![col(1, 1, "a", "integer"), col(2, 1, "x", "integer")],
            constraints: vec![fk],
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert!(
            pulled.schema.tables[&TableName::new("app", "t")]
                .foreign_keys
                .contains_key("t_fk")
        );
        assert_eq!(pulled.limitations.len(), 1);
        assert!(pulled.warnings[0].contains("NOT VALID"));
    }

    /// `pg_get_constraintdef` returns the whole clause and the model holds its
    /// inside. Stored whole, an emitter that wraps it produces
    /// `CHECK (CHECK (…))`, and `amount >= 0` — what a person writes — could
    /// never converge with the live schema.
    #[test]
    fn a_check_is_stored_as_its_expression_and_not_as_the_whole_clause() {
        let mut check = constraint(1, "t_ck", 'c');
        check.definition = "CHECK ((id > 0))".to_owned();
        check.expression = Some("(id > 0)".to_owned());
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "id", "integer")],
            constraints: vec![check],
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert_eq!(only(&pulled).checks["t_ck"].expression, "(id > 0)");
        assert!(pulled.warnings.is_empty(), "{:?}", pulled.warnings);
    }

    /// And a check with no readable expression is the third thing again.
    #[test]
    fn a_check_with_no_readable_expression_is_left_out_and_named() {
        let mut check = constraint(1, "t_ck", 'c');
        check.definition = "CHECK ((id > 0))".to_owned();
        check.expression = None;
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "id", "integer")],
            constraints: vec![check],
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert!(only(&pulled).checks.is_empty());
        assert!(pulled.warnings[0].contains("no readable expression"));
    }

    /// `MATCH FULL` refuses a row whose referencing columns are partly null,
    /// which the default accepts. Read back as the default it is a constraint
    /// that has quietly stopped refusing them.
    #[test]
    fn a_foreign_key_whose_match_type_is_not_the_default_is_left_out_and_named() {
        for (match_type, expected) in [('s', true), ('f', false), ('p', false), ('z', false)] {
            let mut fk = constraint(1, "t_fk", 'f');
            fk.columns = vec![1];
            fk.ref_columns = vec![1];
            fk.ref_table = Some(2);
            fk.match_type = match_type;
            let raw = RawCatalog {
                tables: vec![table(1, "t"), table(2, "other")],
                columns: vec![col(1, 1, "a", "integer"), col(2, 1, "x", "integer")],
                constraints: vec![fk],
                indexes: Vec::new(),
            };
            let pulled = assemble(&raw);
            assert_eq!(
                pulled.schema.tables[&TableName::new("app", "t")]
                    .foreign_keys
                    .contains_key("t_fk"),
                expected,
                "match type `{match_type}`"
            );
        }
    }

    /// A failed `CREATE INDEX CONCURRENTLY` leaves a row the planner will not
    /// use. Read back as an index, a declaration of the same shape compares
    /// clean and the operator has no index at all.
    #[test]
    fn an_index_the_planner_will_not_use_is_left_out_and_named() {
        let mut ix = index(50, 1, "t_ix");
        ix.valid = false;
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "a", "integer")],
            constraints: Vec::new(),
            indexes: vec![ix],
        };
        let pulled = assemble(&raw);
        assert!(only(&pulled).indexes.is_empty());
        assert!(pulled.warnings[0].contains("indisvalid = false"));
    }

    /// `NULLS NOT DISTINCT` decides whether a unique key admits more than one
    /// null, and the model holds only `unique`. Both the bare index and the
    /// constraint its index enforces are left out.
    #[test]
    fn a_unique_key_that_admits_one_null_is_left_out_whichever_shape_it_has() {
        let mut ix = index(50, 1, "t_ix");
        ix.unique = true;
        ix.nulls_not_distinct = true;
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "a", "integer")],
            constraints: Vec::new(),
            indexes: vec![ix.clone()],
        };
        let pulled = assemble(&raw);
        assert!(only(&pulled).indexes.is_empty());
        assert!(pulled.warnings[0].contains("NULLS NOT DISTINCT"));

        // And the same index standing behind a unique constraint.
        let mut uq = constraint(1, "t_uq", 'u');
        uq.columns = vec![1];
        uq.index_oid = Some(50);
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "a", "integer")],
            constraints: vec![uq],
            indexes: vec![ix],
        };
        let pulled = assemble(&raw);
        assert!(only(&pulled).unique.is_empty());
        assert!(only(&pulled).indexes.is_empty());
        assert!(pulled.warnings.iter().any(|w| w.contains("`t_uq`")));
    }

    /// A `serial` column is an `integer` with a `nextval(...)` default and a
    /// sequence beside it that this model has nowhere to put. The column is
    /// carried; the sequence is named.
    #[test]
    fn a_column_that_defaults_from_a_sequence_it_owns_is_carried_and_named() {
        let mut column = col(1, 1, "id", "integer");
        column.default = Some("nextval('app.t_id_seq'::regclass)".to_owned());
        column.owned_sequence = Some("t_id_seq".to_owned());
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![column],
            constraints: Vec::new(),
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert_eq!(
            only(&pulled).columns["id"].default.as_deref(),
            Some("nextval('app.t_id_seq'::regclass)")
        );
        assert_eq!(pulled.limitations.len(), 1);
        assert!(pulled.warnings[0].contains("t_id_seq"));
    }

    /// A collation decides which values compare equal, so a unique key over a
    /// collated column accepts a different set of them. The column is carried
    /// — it is the type the model says — and the collation is named.
    #[test]
    fn a_column_collated_other_than_its_type_is_named() {
        let mut column = col(1, 1, "name", "text");
        column.collation = Some("C".to_owned());
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![column],
            constraints: Vec::new(),
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert_eq!(only(&pulled).columns["name"].ty.to_string(), "text");
        assert_eq!(pulled.limitations.len(), 1);
        assert!(pulled.warnings[0].contains("COLLATE"));
    }

    /// `Identity` holds a seed and an increment. The rest of the sequence —
    /// where it stops, whether it wraps — reads back as this engine's defaults,
    /// which run out somewhere else.
    #[test]
    fn an_identity_whose_sequence_is_not_the_default_one_is_named() {
        let default_max = i64::from(i32::MAX);
        for (min, max, cycles, cache, reported) in [
            (1, default_max, false, 1, false),
            (5, 99, false, 1, true),
            (1, default_max, true, 1, true),
            (1, 99, false, 1, true),
            // Not a bound: `CACHE` decides which values are handed out and how
            // many a session that ends takes with it.
            (1, default_max, false, 100, true),
        ] {
            let mut column = col(1, 1, "id", "integer");
            column.nullable = false;
            column.identity = Some(RawIdentity {
                always: true,
                seed: 1,
                increment: 1,
                min,
                max,
                cycles,
                cache,
            });
            let raw = RawCatalog {
                tables: vec![table(1, "t")],
                columns: vec![column],
                constraints: Vec::new(),
                indexes: Vec::new(),
            };
            let pulled = assemble(&raw);
            assert_eq!(
                !pulled.warnings.is_empty(),
                reported,
                "{min}..={max} cycles={cycles} cache={cache}: {:?}",
                pulled.warnings
            );
            assert!(only(&pulled).columns["id"].identity.is_some());
        }
    }

    /// `ON DELETE SET NULL (a)` nulls one column; `SetNull` nulls all of them.
    #[test]
    fn a_foreign_key_whose_set_action_names_columns_is_left_out_and_named() {
        let mut fk = constraint(1, "t_fk", 'f');
        fk.columns = vec![1, 2];
        fk.ref_columns = vec![1, 2];
        fk.ref_table = Some(2);
        fk.on_delete = 'n';
        fk.delete_set_columns = vec![1];
        fk.definition =
            "FOREIGN KEY (a, b) REFERENCES app.other(x, y) ON DELETE SET NULL (a)".to_owned();
        let raw = RawCatalog {
            tables: vec![table(1, "t"), table(2, "other")],
            columns: vec![
                col(1, 1, "a", "integer"),
                col(1, 2, "b", "integer"),
                col(2, 1, "x", "integer"),
                col(2, 2, "y", "integer"),
            ],
            constraints: vec![fk],
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert!(
            pulled.schema.tables[&TableName::new("app", "t")]
                .foreign_keys
                .is_empty()
        );
        assert!(pulled.warnings[0].contains("ON DELETE SET"));
    }

    /// `text_pattern_ops` and `COLLATE "C"` answer different queries and
    /// compare different values equal, and `IndexColumn` holds a name and a
    /// direction.
    #[test]
    fn an_index_ordered_by_something_other_than_its_columns_own_is_left_out() {
        let mut ix = index(50, 1, "t_ix");
        ix.nondefault_column_options = true;
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "a", "text")],
            constraints: Vec::new(),
            indexes: vec![ix],
        };
        let pulled = assemble(&raw);
        assert!(only(&pulled).indexes.is_empty());
        assert!(pulled.warnings[0].contains("operator class"));
    }

    /// A key constraint's `INCLUDE` payload lives on its backing index and
    /// nowhere in `pg_constraint`. The index is skipped because it *is* the
    /// constraint, so without this the payload goes with it.
    #[test]
    fn a_key_constraint_whose_index_covers_more_than_its_key_is_left_out() {
        let mut pk = constraint(1, "t_pk", 'p');
        pk.columns = vec![1];
        pk.index_oid = Some(50);
        let mut backing = index(50, 1, "t_pk");
        backing.unique = true;
        backing.primary = true;
        backing.key_count = 1;
        backing.columns = vec![1, 2];
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "a", "integer"), col(1, 2, "b", "integer")],
            constraints: vec![pk],
            indexes: vec![backing],
        };
        let pulled = assemble(&raw);
        assert!(only(&pulled).primary_key.is_none());
        assert!(only(&pulled).indexes.is_empty());
        assert!(pulled.warnings[0].contains("INCLUDE"));
    }

    /// A foreign key's `conindid` is the unique index on the table it points
    /// at, not one of its own. Skipping it would drop a real index — the very
    /// one the foreign key needs to exist.
    #[test]
    fn a_foreign_keys_backing_index_belongs_to_the_referenced_table_and_stays() {
        let mut fk = constraint(2, "c_fk", 'f');
        fk.columns = vec![1];
        fk.ref_columns = vec![1];
        fk.ref_table = Some(1);
        // The referenced table's standalone unique index, which the engine
        // records here because it is what makes the key legal.
        fk.index_oid = Some(50);
        let mut referenced = index(50, 1, "p_uq");
        referenced.unique = true;
        referenced.columns = vec![1];
        referenced.key_count = 1;
        let raw = RawCatalog {
            tables: vec![table(1, "p"), table(2, "c")],
            columns: vec![col(1, 1, "a", "integer"), col(2, 1, "a", "integer")],
            constraints: vec![fk],
            indexes: vec![referenced],
        };
        let pulled = assemble(&raw);
        let p = &pulled.schema.tables[&TableName::new("app", "p")];
        assert!(
            p.indexes.contains_key("p_uq"),
            "the referenced table's own index is not the foreign key's: {:?}",
            p.indexes
        );
        assert_eq!(
            pulled.schema.tables[&TableName::new("app", "c")]
                .foreign_keys
                .len(),
            1
        );
        assert!(pulled.warnings.is_empty(), "{:?}", pulled.warnings);
    }

    /// `NOT ENFORCED` checks nothing, ever. `NOT VALID` checks every new row.
    /// Reading the first as the second recreates a constraint that starts
    /// refusing writes the live database accepts.
    #[test]
    fn a_constraint_the_engine_does_not_enforce_is_left_out_and_named() {
        for kind in ['c', 'f', 'p', 'u'] {
            let mut c = constraint(1, "t_c", kind);
            c.columns = vec![1];
            c.expression = Some("(a > 0)".to_owned());
            c.definition = "CHECK ((a > 0)) NOT ENFORCED".to_owned();
            if kind == 'f' {
                c.ref_columns = vec![1];
                c.ref_table = Some(1);
            }
            c.enforced = false;
            let raw = RawCatalog {
                tables: vec![table(1, "t")],
                columns: vec![col(1, 1, "a", "integer")],
                constraints: vec![c],
                indexes: Vec::new(),
            };
            let pulled = assemble(&raw);
            let t = only(&pulled);
            assert!(
                t.checks.is_empty()
                    && t.foreign_keys.is_empty()
                    && t.primary_key.is_none()
                    && t.unique.is_empty(),
                "{kind}: {t:?}"
            );
            assert!(
                pulled.warnings.iter().any(|w| w.contains("NOT ENFORCED")),
                "{kind}: {:?}",
                pulled.warnings
            );
        }
    }

    /// A `NO INHERIT` check stops at this table; an ordinary one reaches every
    /// table that inherits from it.
    #[test]
    fn a_check_that_stops_at_this_table_is_left_out_and_named() {
        let mut c = constraint(1, "t_ck", 'c');
        c.expression = Some("(a > 0)".to_owned());
        c.definition = "CHECK ((a > 0)) NO INHERIT".to_owned();
        c.no_inherit = true;
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "a", "integer")],
            constraints: vec![c],
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert!(only(&pulled).checks.is_empty());
        assert!(
            pulled.warnings[0].contains("NO INHERIT"),
            "{:?}",
            pulled.warnings
        );

        // The negative case: the same check without the flag is an ordinary
        // one, carried and unremarked.
        let mut ordinary = constraint(1, "t_ck", 'c');
        ordinary.expression = Some("(a > 0)".to_owned());
        let raw = RawCatalog {
            constraints: vec![ordinary],
            ..raw
        };
        let pulled = assemble(&raw);
        assert_eq!(only(&pulled).checks.len(), 1);
        assert!(pulled.warnings.is_empty(), "{:?}", pulled.warnings);
    }

    /// A temporal key keeps an ordinary key's `contype`, so nothing but
    /// `conperiod` tells them apart.
    #[test]
    fn a_temporal_key_is_left_out_and_named_rather_than_read_as_an_ordinary_one() {
        for kind in ['p', 'u', 'f'] {
            let mut c = constraint(1, "t_pk", kind);
            c.columns = vec![1, 2];
            c.definition = "PRIMARY KEY (id, valid WITHOUT OVERLAPS)".to_owned();
            if kind == 'f' {
                c.ref_columns = vec![1, 2];
                c.ref_table = Some(1);
            }
            c.period = true;
            let raw = RawCatalog {
                tables: vec![table(1, "t")],
                columns: vec![col(1, 1, "id", "integer"), col(1, 2, "valid", "daterange")],
                constraints: vec![c],
                indexes: Vec::new(),
            };
            let pulled = assemble(&raw);
            let t = only(&pulled);
            assert!(
                t.primary_key.is_none() && t.unique.is_empty() && t.foreign_keys.is_empty(),
                "{kind}: {t:?}"
            );
            assert!(
                pulled.warnings.iter().any(|w| w.contains("temporal")),
                "{kind}: {:?}",
                pulled.warnings
            );
            // The negative case: the same constraint without the flag is an
            // ordinary key, and reporting it would be noise nobody reads.
            let mut ordinary = constraint(1, "t_pk", kind);
            ordinary.columns = vec![1, 2];
            if kind == 'f' {
                ordinary.ref_columns = vec![1, 2];
                ordinary.ref_table = Some(1);
            }
            let raw = RawCatalog {
                constraints: vec![ordinary],
                ..raw
            };
            let ordinary = assemble(&raw);
            assert!(
                !ordinary.warnings.iter().any(|w| w.contains("temporal")),
                "{kind}: {:?}",
                ordinary.warnings
            );
        }
    }

    /// Every limitation carries its table, so a caller can tell drift inside
    /// the managed set from a fact about somebody else's table.
    #[test]
    fn every_limitation_names_the_table_it_belongs_to() {
        let mut column = col(2, 1, "total", "integer");
        column.generated = true;
        let raw = RawCatalog {
            tables: vec![table(1, "plain"), table(2, "odd")],
            columns: vec![col(1, 1, "a", "integer"), column],
            constraints: Vec::new(),
            indexes: Vec::new(),
        };
        let pulled = assemble(&raw);
        assert_eq!(pulled.limitations.len(), 1);
        assert_eq!(pulled.limitations[0].table, TableName::new("app", "odd"));
        assert_eq!(pulled.warnings.len(), pulled.limitations.len());
    }

    /// A table with nothing on it is a table, not a warning. The negative case
    /// for every rule above.
    #[test]
    fn an_ordinary_table_produces_no_warning_at_all() {
        let mut pk = constraint(1, "t_pk", 'p');
        pk.columns = vec![1];
        pk.index_oid = Some(50);
        let mut backing = index(50, 1, "t_pk");
        backing.unique = true;
        backing.primary = true;
        let mut column = col(1, 1, "id", "integer");
        column.nullable = false;
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![column, col(1, 2, "note", "text")],
            constraints: vec![pk],
            indexes: vec![backing],
        };
        let pulled = assemble(&raw);
        assert!(pulled.warnings.is_empty(), "{:?}", pulled.warnings);
        assert!(pulled.limitations.is_empty());
        let read = only(&pulled);
        assert_eq!(read.columns.len(), 2);
        assert!(!read.columns["id"].nullable);
        assert!(read.columns["note"].nullable);
        assert!(read.indexes.is_empty());
    }
}
