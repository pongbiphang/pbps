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
use std::str::FromStr;

use pbps_model::{
    CheckConstraint, Column, ColumnRef, ColumnType, ForeignKey, Identity, Index, IndexColumn,
    Module, ModuleId, ModuleKind, ObjectName, PrimaryKey, ReferentialAction, RoutineArg, RoutineId,
    Schema, Table, TableName, UniqueConstraint,
};

use crate::types;

/// One ordinary table, as `pg_class` has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTable {
    pub oid: i64,
    pub schema: String,
    pub name: String,
}

/// One module, with the text this engine deparses for it.
///
/// The whole deparsed statement, not the part a declaration holds: where the
/// prefix ends is a rule about this engine's grammar, and a rule is worth more
/// where it can be tested without a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawModule {
    pub oid: i64,
    /// `v`, `f`, `p` or `t` — `relkind` for a view, `prokind` for a routine.
    pub kind: char,
    pub schema: String,
    pub name: String,
    /// The table a trigger is on, empty for every other kind.
    pub on_table: String,
    pub definition: String,
}

/// One argument of one routine's identity, as `format_type` prints it under the
/// canonical empty `search_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawModuleArg {
    pub routine_oid: i64,
    pub position: i64,
    pub ty: String,
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
    /// The sequences this column's default *uses* without owning any of them —
    /// `DEFAULT nextval('app.s')` over a sequence somebody created separately.
    ///
    /// Pre-rendered as one list rather than a `Vec`, because no driver here
    /// reads a `text[]` and the only thing it is for is the message. It is
    /// `None` when the default uses no sequence the column does not own, which
    /// is not the same as the column having no default.
    pub default_sequences: Option<String>,
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
    /// `true` when some trigger that implements this constraint has a
    /// `tgenabled` other than `O`. A foreign key is enforced by triggers, and
    /// `ALTER TABLE ... DISABLE TRIGGER` stops them without touching
    /// `convalidated` or `conenforced`.
    pub triggers_not_ordinary: bool,
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
    pub modules: Vec<RawModule>,
    pub module_args: Vec<RawModuleArg>,
    pub roles: Vec<RawRole>,
    pub grants: Vec<RawGrant>,
    /// One row per argument of every routine a grant can name, in order — the
    /// same shape as [`RawCatalog::module_args`], and a wider set: a grant may
    /// be on a routine the module pull leaves out.
    pub routine_args: Vec<RawModuleArg>,
    pub default_acls: Vec<RawDefaultAcl>,
    pub other_grants: Vec<RawOtherGrant>,
    pub held_elsewhere: Vec<RawSharedDependency>,
}

/// A role held by something **outside this database** (ADR-0010 §4).
///
/// The rows of `pg_shdepend` whose `dbid` is not this database's — and the
/// reason a `DROP ROLE` refusal cannot be answered from one database's view.
/// **Measured**: with every grant this database holds revoked, the engine
/// still says `role "gr_reader" cannot be dropped because some objects depend
/// on it` / `DETAIL: 1 object in database otherdb`.
///
/// The objects cannot be named from here — their oids belong to that
/// database's catalog — so what is carried is the database and a count. That
/// is exactly as far as one connection can see, and reporting "nothing is
/// stopping it" instead would be the answer that reads as good news.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawSharedDependency {
    pub role: String,
    /// `None` is a dependency on a shared object — a database, a tablespace —
    /// which belongs to no one database.
    pub database: Option<String>,
    /// `pg_shdepend.deptype`: `o` owns, `a` is granted, `r` is named by a
    /// policy.
    pub deptype: char,
    pub objects: i64,
}

/// One principal that could hold a grant here (ADR-0005).
///
/// The cluster's own `pg_*` roles are left out by the query: the engine
/// refuses `CREATE ROLE` on such a name (measured, `role name "pg_thing" is
/// reserved`), `validate_role` refuses declaring one, and what they are
/// granted is the cluster's business rather than this database's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawRole {
    pub name: String,
    /// A superuser passes every privilege check without consulting an ACL, so
    /// what such a role is *granted* does not decide what it can do. Carried
    /// rather than filtered out, because a role silently missing from a pull
    /// is the failure this project is built to avoid.
    pub superuser: bool,
}

/// One `(grantee, permission)` pair, already expanded out of an `aclitem` by
/// the engine's own `aclexplode`.
///
/// One flat row per pair, rather than an ACL string per object, because the
/// `aclitem` text form is the engine's and parsing it here would be a second,
/// worse copy of `aclexplode` — the letters are positional, `m` arrived in
/// PostgreSQL 17, and a letter this code did not know would read as no
/// permission at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawGrant {
    /// `None` is PUBLIC — grantee oid `0`, which `regrole` renders as `-`.
    /// Never a role: PUBLIC is every principal in the cluster, so what it
    /// holds is reported as context and never compared (ADR-0010 §5).
    pub grantee: Option<String>,
    pub schema: String,
    /// `None` when the target is the schema itself.
    pub object: Option<String>,
    /// The `pg_proc` oid, where the object is a routine — and **not** its
    /// rendered signature.
    ///
    /// A signature aggregated into one string has to be split again to be
    /// used, and a comma is not a separator: measured, a type named
    /// `amount,type` renders as `cm."amount,type"`, so
    /// `pg_get_function_identity_arguments` and any `string_agg` of
    /// `format_type` both hand back a comma that belongs *inside* an argument.
    /// Split on it, each fragment failed to parse and a valid grant on a
    /// managed routine became targetless unexpressible state — which refuses
    /// the connected plan. The arguments therefore travel as rows
    /// ([`RawCatalog::routine_args`]), the way the module pull already carries
    /// them, and are never re-parsed out of one string.
    pub routine_oid: Option<i64>,
    /// Which catalog the row came from, and that catalog's own kind letter.
    ///
    /// A typed pair rather than one `char`, because the two alphabets overlap
    /// and the overlap is not harmless: `relkind` `f` is a foreign table and
    /// `prokind` `f` is a function, `relkind` `p` is a partitioned table and
    /// `prokind` `p` is a procedure. Read as one letter, a grant on a foreign
    /// table would have been read back as a grant on a function of that name.
    pub kind: GrantedKind,
    /// The engine's own word: `SELECT`, `EXECUTE`, `MAINTAIN`.
    pub permission: String,
    /// `WITH GRANT OPTION`, which the model does not hold.
    pub grantable: bool,
    /// `Some` for a grant on one column, which the object's own ACL does not
    /// show at all.
    pub column: Option<String>,
    /// Whether the stored ACL was **NULL** — that is, whether this row came
    /// out of `acldefault` rather than out of anything anybody granted.
    ///
    /// The distinction the model needs and the ACL text cannot carry. A NULL
    /// ACL is not an empty one (ADR-0010 §5), and it is not a set of grants
    /// either: it is the engine's zero point, which every object of that kind
    /// starts from. Compared as grants, the zero point deadlocks the tool on
    /// its own output — every function pbps creates arrives with `EXECUTE` to
    /// PUBLIC and every table with the owner's whole set, so the next plan
    /// would revoke what the apply before it had just produced.
    pub defaulted: bool,
    /// The object's owner, for the same reason: the owner's entry is the one
    /// `acldefault` puts there, not one anybody granted.
    pub owner: String,
}

/// What a grant's target is, by the catalog the row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GrantedKind {
    /// `pg_class.relkind`: `r` table, `v` view, `S` sequence, `m`
    /// materialized view, `p` partitioned table, `f` foreign table.
    Relation(char),
    /// `pg_proc.prokind`: `f` function, `p` procedure, `a` aggregate, `w`
    /// window function.
    Routine(char),
    #[default]
    Schema,
}

/// One grant in a catalog whose target the model cannot name at all
/// (ADR-0010 §6, and DECISIONS 105 on the other engine).
///
/// **Enumerated from the engine rather than from memory**, which this crate has
/// earned the hard way once already (`crate::modules::ATTACHED_BY_ADDRESS`).
/// PostgreSQL 18 has fourteen `aclitem[]` columns in `pg_catalog`, and the
/// query that says so is
///
/// ```sql
/// SELECT c.relname || '.' || a.attname
///   FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid
///  WHERE c.relnamespace = 'pg_catalog'::regnamespace AND c.relkind = 'r'
///    AND a.atttypid = 'aclitem[]'::regtype;
/// ```
///
/// which `every_catalog_that_holds_a_grant_is_read` runs against the live
/// server and compares with this reader's list. A fifteenth arriving in a
/// later release fails that test instead of going unnoticed.
///
/// A role that gained `USAGE ON LANGUAGE c`, or `SET ON PARAMETER`, has
/// changed — and a read that did not look would compare the grants it *did*
/// see and call the role clean, which is the failure DECISIONS 105 records on
/// SQL Server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawOtherGrant {
    /// `None` is PUBLIC.
    pub grantee: Option<String>,
    /// The kind of object, in the words a message uses: `a type`,
    /// `a language`.
    pub class: String,
    pub name: String,
    pub permission: String,
    pub grantable: bool,
    /// Who owns the object, or `None` where its catalog has no owner column
    /// (`pg_parameter_acl`).
    ///
    /// Carried for the zero point (DECISIONS 371): these ACLs are NULL until
    /// somebody touches them, and touching one writes the owner's own
    /// inherent entry beside the change.
    pub owner: Option<String>,
}

/// One `ALTER DEFAULT PRIVILEGES` entry (ADR-0010 §2).
///
/// Not a grant on anything: a standing instruction that objects **one role**
/// creates from now on arrive already granted. The model has no such thing —
/// a `schema::` target on the other engine covers present and future objects
/// whoever creates them — so each entry is reported rather than folded into
/// any role's set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawDefaultAcl {
    /// The role whose creations it covers, and the reason this is not
    /// portable: who runs the plan decides what the declaration means.
    pub grantor: String,
    /// `None` is every schema.
    pub in_schema: Option<String>,
    /// `r` tables and views, `S` sequences, `f` routines, `T` types,
    /// `n` schemas.
    pub objtype: char,
    /// The `aclitem[]` as the engine prints it.
    pub acl: String,
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
    /// Permissions the model cannot hold and a drift check must not call
    /// clean: a grant `WITH GRANT OPTION`, a column-level grant, a grant on a
    /// sequence, a permission on a class the model cannot name. Each is left
    /// out of the role's set — folded in or merely warned about, `verify`
    /// compares the sets that remain and says "no drift" about a role that has
    /// changed — and reported here beside the other differences
    /// (DECISIONS 95, 97).
    pub unexpressible: Vec<Unexpressible>,
}

/// One permission the model cannot hold, and enough about it for the caller to
/// decide whether it is any of this project's business.
///
/// The same type as the SQL Server pull's, for the same reason: the securable
/// travels with it, because filtered by role alone a column-level grant on
/// somebody else's table stopped every command — while the *plain* grant on
/// that same table was dropped by `scope`, whose recorded reason is that it is
/// that table's business (DECISIONS 176).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unexpressible {
    pub role: String,
    /// The securable, where the permission names one a declaration could.
    /// `None` for a class the model cannot name at all.
    pub target: Option<pbps_model::GrantTarget>,
    /// The difference, already rendered.
    pub what: String,
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
    /// Every table **in the pull**, by oid — a foreign key's target. A table
    /// the pull refuses is not here, so a key that points at one cannot
    /// resolve and is left out and named.
    tables: HashMap<i64, TableName>,
    /// Every column in the pull, by table and attnum. A foreign key's
    /// `confkey` names attnums on the **referenced** table, which the
    /// constrained table's own map cannot answer for (DECISIONS 252).
    ///
    /// Refused tables are absent here too. Either exclusion alone would leave
    /// the key out — measured, by removing each and watching the test still
    /// pass, and both and watching it fail — and both are here because what
    /// these maps describe is "the tables in the pull", not "the tables the
    /// catalog returned". A reader who removes one as dead weight has changed
    /// what the struct means.
    columns: HashMap<(i64, i32), &'a str>,
    /// The indexes that admit at most one null key. A unique constraint is
    /// enforced by an index, so this is a property of the constraint too, and
    /// the constraint arm cannot see the index.
    nulls_not_distinct: BTreeSet<i64>,
    /// The indexes that carry an `INCLUDE` payload, for the same reason: a key
    /// constraint's payload lives on its backing index and nowhere in
    /// `pg_constraint`.
    covering: BTreeSet<i64>,
    /// The indexes that reached the pull — as an index, or as the key
    /// constraint they enforce. Empty until the first pass has run, which is
    /// why the foreign keys are the second one: a foreign key is legal only
    /// against a uniqueness that is there, and whether it is there is not known
    /// until the constraint and index arms have had their say.
    surviving_indexes: BTreeSet<i64>,
}

/// Everything a table's own assembly needs, gathered once.
#[derive(Clone)]
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

    // Which tables can be recorded at all, decided **before** anything is built
    // that names one. A table left out for a name the declaration cannot write
    // must not be reachable as a foreign key's target either: the key would
    // point at a table that is not in the pull, and its own declaration would
    // carry the very name that was refused.
    //
    // The per-table `continue` below cannot do this. It runs after the lookups
    // are built, and a foreign key is assembled from the *referencing* table's
    // turn — which may come first.
    let refused: BTreeSet<i64> = raw
        .tables
        .iter()
        .filter(|t| {
            let name = TableName::new(&t.schema, &t.name);
            let columns = columns_by_table.get(&t.oid).map_or(&[][..], Vec::as_slice);
            match a_name_the_declaration_cannot_write(&name, columns) {
                Some(detail) => {
                    note(&mut pulled, &name, detail);
                    true
                }
                None => false,
            }
        })
        .map(|t| t.oid)
        .collect();

    let mut lookups = Lookups {
        tables: raw
            .tables
            .iter()
            .filter(|t| !refused.contains(&t.oid))
            .map(|t| (t.oid, TableName::new(&t.schema, &t.name)))
            .collect(),
        columns: raw
            .columns
            .iter()
            .filter(|c| !refused.contains(&c.table_oid))
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
        surviving_indexes: BTreeSet::new(),
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

    let mut surviving: BTreeSet<i64> = BTreeSet::new();
    let mut deferred: Vec<(Parts, &RawConstraint)> = Vec::new();
    for raw_table in &raw.tables {
        let name = TableName::new(&raw_table.schema, &raw_table.name);
        let raw_columns = columns_by_table
            .get(&raw_table.oid)
            .map_or(&[][..], Vec::as_slice);
        if refused.contains(&raw_table.oid) {
            continue;
        }
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
            // Foreign keys wait for the second pass. See `surviving_indexes`.
            if constraint.kind == 'f' {
                deferred.push((parts.clone(), *constraint));
                continue;
            }
            if add_constraint(constraint, &parts, &lookups, &mut table, &mut pulled)
                && let Some(oid) = constraint.index_oid
            {
                surviving.insert(oid);
            }
        }
        for index in indexes_by_table
            .get(&raw_table.oid)
            .map_or(&[][..], Vec::as_slice)
        {
            if constraint_indexes.contains(&index.oid) {
                continue;
            }
            if add_index(index, &parts, &mut table, &mut pulled) {
                surviving.insert(index.oid);
            }
        }

        pulled.schema.tables.insert(name, table);
    }

    // The second pass. A foreign key is enforced against a uniqueness on the
    // referenced table, and the engine records which one in `conindid`. If that
    // index did not reach the pull — because the key constraint enforcing it
    // has an `INCLUDE` payload, or is `NULLS NOT DISTINCT`, or any of the other
    // reasons this file leaves one out — then the schema described here cannot
    // be built: adding the foreign key back would fail for want of the
    // uniqueness nothing mentions.
    lookups.surviving_indexes = surviving;
    for (parts, constraint) in deferred {
        // Taken out and put back rather than borrowed in place: `note` writes
        // to the same `Pulled` the table lives in.
        let Some(mut table) = pulled.schema.tables.remove(&parts.name) else {
            continue;
        };
        add_constraint(constraint, &parts, &lookups, &mut table, &mut pulled);
        pulled.schema.tables.insert(parts.name.clone(), table);
    }

    let args_by_routine = group(&raw.module_args, |a| a.routine_oid);
    for raw_module in &raw.modules {
        add_module(raw_module, &args_by_routine, &mut pulled);
    }

    add_roles(raw, &mut pulled);

    pulled
}

/// The roles and what each holds in this database (ADR-0005, ADR-0010 §5).
///
/// Every role the query returned is inserted, grants or none: the managed-set
/// cut is the ids file's, later, and a role that has had its last grant
/// revoked must read as "a role with nothing" and not as "no such role".
///
/// # NULL is not empty — and it is not a set of grants either
///
/// A PostgreSQL object with no explicit grants has a **NULL** ACL, and the
/// engine reads a NULL ACL as the built-in default for the object's kind:
/// measured, a fresh function has `proacl IS NULL` and a role holding only
/// `USAGE` on the schema can execute it, because the default is
/// `{=X/owner,owner=X/owner}` — PUBLIC gets `EXECUTE`. Reading NULL as "no
/// privileges" would report a function as closed where it is open to everyone,
/// which is the member of *absent, empty and unreadable* that reads as good
/// news.
///
/// So the reader expands it — with the engine's own `acldefault`, not with a
/// table written here, because the answer moves with the release: `MAINTAIN`
/// joined the relation default in PostgreSQL 17, measured `arwdDxt` on 16.15
/// against `arwdDxtm` on 18.6.
///
/// **But the default is the zero point, not drift** (ADR-0010 §5, which
/// measured this for PUBLIC and refused to route it down the unexpressible
/// path). The same argument settles the owner, whom that section does not
/// name: every table pbps creates arrives owned by the deploying account with
/// the owner's whole set, and every function it creates arrives with `EXECUTE`
/// to PUBLIC. Compared as grants, the plan after a successful apply would
/// revoke what that apply had just produced — a rule that makes the tool's own
/// output unplannable is broken, not safe.
///
/// The line is therefore drawn at *who put the entry there*:
///
/// | ACL entry | Treated as |
/// |---|---|
/// | out of `acldefault` (`defaulted`) | the zero point — reported, never compared |
/// | the object's owner | the same: the entry `acldefault` puts there |
/// | PUBLIC | context (§5): not a role, so never drift and never a gate |
/// | anything else | a grant, compared for a managed role |
///
/// Nothing is dropped: what is not compared is reported, because a revocation
/// on this engine is the *absence* of an entry rather than a row, and silence
/// about the zero point is silence about the one act that leaves no trace.
fn add_roles(raw: &RawCatalog, pulled: &mut Pulled) {
    for r in &raw.roles {
        pulled
            .schema
            .roles
            .insert(r.name.clone(), pbps_model::Role::default());
        if r.superuser {
            pulled.warnings.push(format!(
                "role `{}` is a superuser, which passes every privilege check without consulting \
                 an ACL: what it is granted here does not decide what it can do",
                r.name
            ));
        }
    }

    // The arguments of every routine a grant can name, in order, as the
    // catalog spells each type. Never one joined string: a comma can be part
    // of an argument (`cm."amount,type"`), so a rendered signature cannot be
    // split back into the list it came from.
    let mut signatures: BTreeMap<i64, Vec<&str>> = BTreeMap::new();
    for arg in &raw.routine_args {
        signatures.entry(arg.routine_oid).or_default().push(&arg.ty);
    }

    let mut public_executes: Vec<String> = Vec::new();
    let mut closed_to_public: BTreeSet<String> = raw
        .grants
        .iter()
        .filter(|g| matches!(g.kind, GrantedKind::Routine('f' | 'p')) && !g.defaulted)
        .map(|g| target_label(g, &signatures))
        .collect();

    for g in &raw.grants {
        if g.grantee.is_none() {
            // PUBLIC. Context, never drift (ADR-0010 §5): it is not a role,
            // it cannot be declared, and comparing it would report every
            // database's default `EXECUTE` on every function as a difference.
            if matches!(g.kind, GrantedKind::Routine('f' | 'p')) && g.permission == "EXECUTE" {
                closed_to_public.remove(&target_label(g, &signatures));
                public_executes.push(target_label(g, &signatures));
            } else {
                pulled.warnings.push(format!(
                    "PUBLIC holds {} on {}, which is every principal in the cluster and not a \
                     role this project can declare (ADR-0010 §5)",
                    g.permission,
                    target_label(g, &signatures)
                ));
            }
            continue;
        }
        // The zero point. Reported below as one line per shape rather than one
        // per object: a database has as many of these as it has objects, and a
        // report nobody can read is a report nobody reads.
        if g.defaulted || g.grantee.as_deref() == Some(g.owner.as_str()) {
            continue;
        }
        let grantee = g.grantee.as_deref().unwrap_or_default();
        // A grantee outside the roles read — a `pg_*` role, or one the query
        // filtered — still holds what it holds. Reported rather than dropped.
        if !pulled.schema.roles.contains_key(grantee) {
            pulled.warnings.push(format!(
                "`{grantee}` holds {} on {}, and is not a role this project can declare",
                g.permission,
                target_label(g, &signatures)
            ));
            continue;
        }
        let unexpressible = |pulled: &mut Pulled, target, what: String| {
            pulled.unexpressible.push(Unexpressible {
                role: grantee.to_owned(),
                target,
                what,
            });
        };
        // The target first, and every finding below carries it. `target: None`
        // means *this grant has no target a declaration could name* — and
        // `deploy::unexpressible_permissions` keeps every targetless finding
        // whatever role or object it is about, because a permission on the
        // database itself belongs to no object at all. Reported without one, a
        // limitation on somebody else's table refused every connected plan,
        // while the ordinary grant beside it on that same table was dropped as
        // none of this project's business (DECISIONS 176).
        let target = match target_of(g, &signatures) {
            Ok(target) => target,
            Err(what) => {
                unexpressible(pulled, None, format!("role {grantee}: {what}"));
                continue;
            }
        };
        // The string form is what a snapshot carries, and in it a `(` opens a
        // routine signature: a legal PostgreSQL object name may contain one,
        // and `app."sales(archive)"` reads back as a grant on a routine
        // (DECISIONS 205, which measured this shape on the other engine). The
        // structured target is kept — only its *string* form is ambiguous, and
        // the target is what scopes the report to the managed set
        // (`unexpressible_permissions`).
        if target
            .to_string()
            .parse::<pbps_model::GrantTarget>()
            .as_ref()
            != Ok(&target)
        {
            unexpressible(
                pulled,
                Some(target),
                format!(
                    "role {grantee}: {} on {} is on an object whose name contains a parenthesis \
                     or a period, which a declaration cannot spell — written out it reads back \
                     as a different target; the declarations cannot express it",
                    g.permission,
                    target_label(g, &signatures)
                ),
            );
            continue;
        }
        // An object this pull did not record: a table whose shape the reader
        // refuses, a routine with an argument the model cannot hold, a
        // definition that came back unreadable. The grant is still there, and
        // written into the role it would name a target no declaration in the
        // project has — making `validate` refuse the very project `pull` just
        // wrote. *Absent, empty and unreadable are three different things*:
        // this is the one the role's set must not read as ordinary.
        if !recorded(&pulled.schema, &target) {
            unexpressible(
                pulled,
                Some(target),
                format!(
                    "role {grantee}: {} on {} is on an object this pull did not record, so the \
                     declarations cannot express it",
                    g.permission,
                    target_label(g, &signatures)
                ),
            );
            continue;
        }
        if let Some(column) = &g.column {
            // Measured while building ADR-0009 §3: after `GRANT SELECT (a) ON
            // m9.v`, `pg_class.relacl` is NULL and the grant lives in
            // `pg_attribute.attacl`. An object-level reader sees nothing at
            // all, which is why this is reported rather than approximated by a
            // grant on the whole object — that would be a *widening* the
            // declarations then plan.
            unexpressible(
                pulled,
                Some(target),
                format!(
                    "role {grantee}: {} on column `{column}` of {} is a column-level grant, which \
                     the declarations cannot express",
                    g.permission,
                    target_label(g, &signatures)
                ),
            );
            continue;
        }
        let Ok(permission) = g.permission.parse::<pbps_model::Permission>() else {
            unexpressible(
                pulled,
                Some(target),
                format!(
                    "role {grantee}: {} on {} is not a permission this model holds; the \
                     declarations cannot express it",
                    g.permission,
                    target_label(g, &signatures)
                ),
            );
            continue;
        };
        if g.grantable {
            // `WITH GRANT OPTION` — a `*` in the ACL — lets the grantee grant
            // it onward, which the model does not hold. Folded in as a plain
            // grant, `verify` would compare the two as equal and call a role
            // that can hand out `SELECT` the same as one that cannot.
            unexpressible(
                pulled,
                Some(target),
                format!(
                    "role {grantee}: {} on {} is `WITH GRANT OPTION`, which the declarations \
                     cannot express",
                    g.permission,
                    target_label(g, &signatures)
                ),
            );
            continue;
        }
        if let Some(role) = pulled.schema.roles.get_mut(grantee) {
            role.grants.entry(target).or_default().insert(permission);
        }
    }

    // The two facts about PUBLIC and routines, each as one line. The first is
    // the exposure ADR-0010 §5 names — every routine with no explicit ACL is
    // executable by every principal in the cluster — and the second is the
    // hardening that undoes it, which is *the absence of a row* and would be
    // silence in any report that only listed what the catalog holds.
    if !public_executes.is_empty() {
        pulled.warnings.push(format!(
            "PUBLIC can execute {}: {}. That is this engine's default for a routine \
             (`acldefault('f', owner)` is `{{=X/owner,owner=X/owner}}`), not something anyone \
             granted, so it is reported rather than compared (ADR-0010 §5)",
            plural(public_executes.len(), "routine"),
            listed(&public_executes)
        ));
    }
    if !closed_to_public.is_empty() {
        let closed: Vec<String> = closed_to_public.into_iter().collect();
        pulled.warnings.push(format!(
            "`EXECUTE` has been revoked from PUBLIC on {}: {}. The declarations cannot express \
             that — a revocation here is the absence of the engine's default rather than a row — \
             and a rebuild restores the default, so an ordinary edit would reopen a routine \
             somebody deliberately closed (ADR-0009 §3, ADR-0010 §5)",
            plural(closed.len(), "routine"),
            listed(&closed)
        ));
    }

    // The catalogs whose targets no declaration can name. Grouped, because a
    // database has as many of these as it has types: one line per
    // (role, class, permission), naming the objects it covers.
    let mut others: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
    for g in &raw.other_grants {
        let grantee = match g.grantee.as_deref() {
            // PUBLIC holds `USAGE` on every built-in type and on `sql` and
            // `plpgsql` in every database there is. Context, and not even
            // interesting context: it is the same in every database, and
            // listing it would bury the rows that are not.
            None => continue,
            Some(grantee) => grantee,
        };
        // The zero point again (DECISIONS 371), on the catalogs that carry no
        // `acldefault` expansion because they need none: every one of these
        // ACLs is NULL until somebody touches it. Measured on 18.6, `REVOKE
        // USAGE ON TYPE ot.money_kind FROM PUBLIC` leaves
        // `{ot_owner=U/ot_owner}` — the owner's inherent `USAGE`, written by
        // the engine and not by anyone. Reported as unnameable state, it would
        // refuse every plan connected to a role that owns a type, for a
        // privilege nobody granted.
        if Some(grantee) == g.owner.as_deref() {
            continue;
        }
        if !pulled.schema.roles.contains_key(grantee) {
            continue;
        }
        let permission = if g.grantable {
            format!("{} WITH GRANT OPTION", g.permission)
        } else {
            g.permission.clone()
        };
        others
            .entry((grantee.to_owned(), g.class.clone(), permission))
            .or_default()
            .push(g.name.clone());
    }
    for ((role, class, permission), names) in others {
        pulled.unexpressible.push(Unexpressible {
            role: role.clone(),
            // No target: the model cannot name one of these at all, which is
            // the point. A role that gained one out of band has changed even
            // where every grant the model *does* hold still matches
            // (DECISIONS 105).
            target: None,
            what: format!(
                "role {role}: {permission} on {class} is not something the declarations can \
                 name — {}",
                listed(&names)
            ),
        });
    }

    for held in &raw.held_elsewhere {
        // ADR-0010 §4. Reported by the pull because it is the fact this
        // database's catalog hides: everything `pull` otherwise says about a
        // role is what *this* database holds, and a reader who took that for
        // the whole of it would plan a `drop-role` the cluster will refuse.
        pulled.warnings.push(
            crate::roles::DropBlocker {
                role: held.role.clone(),
                database: held.database.clone(),
                deptype: held.deptype,
                objects: held.objects,
            }
            // `None`: every row here is one this connection cannot look
            // inside, which is what the query selects.
            .rendered(None),
        );
    }

    for d in &raw.default_acls {
        // ADR-0010 §2. Not a grant on anything that exists: a standing
        // instruction attached to one *creating role*, which is exactly why
        // the other engine's schema-level grant does not translate. Reported
        // so that a rebuild's arriving grants, and a `pull` that shows a role
        // holding less than it will hold tomorrow, are both visible.
        pulled.warnings.push(format!(
            "`ALTER DEFAULT PRIVILEGES FOR ROLE {}` in {} grants {} on {} that role creates from \
             now on; the declarations have no such thing, because on this engine who creates an \
             object decides what it arrives with (ADR-0010 §2)",
            d.grantor,
            match &d.in_schema {
                Some(s) => format!("schema `{s}`"),
                None => "every schema".to_owned(),
            },
            d.acl,
            default_acl_objects(d.objtype),
        ));
    }
}

/// `1 routine` / `4 routines`.
fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// The names, capped, with the rest counted rather than printed.
///
/// A count alone sends the reader to write the query themselves; the whole
/// list of every routine in a large database is a wall nobody reads. Ten and a
/// remainder is the shape that answers "which ones" for the cases that have an
/// answer and stays one paragraph for the ones that do not.
fn listed(names: &[String]) -> String {
    const SHOWN: usize = 10;
    let mut out = names
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        out.push_str(&format!(" and {} more", names.len() - SHOWN));
    }
    out
}

/// What an `ALTER DEFAULT PRIVILEGES` entry's object type covers, in words.
fn default_acl_objects(objtype: char) -> &'static str {
    match objtype {
        'r' => "the tables and views",
        'S' => "the sequences",
        'f' => "the functions and procedures",
        'T' => "the types",
        'n' => "the schemas",
        _ => "the objects",
    }
}

/// The target a grant names, or why the model cannot name it.
/// Whether the pull recorded the object a grant is on.
///
/// The assembly ahead of this one leaves objects out — a table with a shape
/// the reader refuses, a routine whose argument the model cannot spell — and
/// their ACL rows arrive here all the same. `pbps_model::role::check` refuses
/// a grant on a target the project does not declare, so a role written with
/// one would make the schema `pull` just produced fail its own validation.
///
/// **In the target's own namespace.** An `Object` here came from a relation
/// row — `target_of` builds one for no other kind — and relations and routines
/// are two namespaces on this engine, so a routine of the same name does not
/// answer for a table that was left out. Counted, a hidden table `app.f`
/// beside a surviving routine `app.f(integer)` would put `SELECT ON app.f`
/// into the role, and `validate::role` reads that bare name in the relation
/// namespace and finds nothing there — the schema `pull` wrote, refused by
/// this dialect's own check (DECISIONS 382).
fn recorded(schema: &pbps_model::Schema, target: &pbps_model::GrantTarget) -> bool {
    match target {
        pbps_model::GrantTarget::Object(o) => {
            schema.tables.contains_key(o)
                || schema.modules.iter().any(|(id, m)| {
                    m.kind == ModuleKind::View && id.referenced_name().as_ref() == Some(o)
                })
        }
        pbps_model::GrantTarget::Routine(r) => schema
            .modules
            .keys()
            .any(|id| matches!(id, ModuleId::Routine(other) if other == r)),
        // Not an object the pull assembles: the model holds no list of
        // schemas, and a `schema::` target names one directly.
        pbps_model::GrantTarget::Schema(_) => true,
    }
}

fn target_of(
    g: &RawGrant,
    signatures: &BTreeMap<i64, Vec<&str>>,
) -> Result<pbps_model::GrantTarget, String> {
    let Some(object) = g.object.as_deref() else {
        return Ok(pbps_model::GrantTarget::Schema(g.schema.clone()));
    };
    let name = pbps_model::ObjectName::new(g.schema.clone(), object.to_owned());
    match g.kind {
        // A table or a view: the two the model declares, and the two that
        // share `GRANT ... ON TABLE`.
        GrantedKind::Relation('r' | 'v') => Ok(pbps_model::GrantTarget::Object(name)),
        // ADR-0010 §7, and the other half of why `serial` is refused at load
        // (#77). A `serial` column creates a sequence the declaration never
        // named, and inserting into such a column needs a privilege on it —
        // measured, `permission denied for sequence ser_id_seq` where the
        // same insert into an identity column succeeds. The model has no
        // sequence to grant on, so the grant is reported rather than dropped:
        // dropped, `pull` would write a role that cannot insert.
        GrantedKind::Relation('S') => Err(format!(
            "{} on sequence `{name}` is a grant on a sequence, which this model does not declare \
             — an identity column needs no such grant and a `serial` column does, which is why \
             `serial` is refused at load (ADR-0010 §7)",
            g.permission
        )),
        // A relation this model does not hold. **Measured**, a `GRANT SELECT`
        // on a materialized view and on a partitioned table both land in
        // `relacl` — so leaving them out of the read reported the role as
        // holding nothing on them, which is *absent* reading as *empty*.
        GrantedKind::Relation(other) => Err(format!(
            "{} on `{name}` is on {}, which this model does not declare",
            g.permission,
            relation_kind(other)
        )),
        // A routine, written with its signature: a name is not an identity
        // where the kind overloads (ADR-0009 §1).
        GrantedKind::Routine('f' | 'p') => {
            let mut args = Vec::new();
            for spelled in routine_args(g, signatures) {
                match spelled.parse::<RoutineArg>() {
                    Ok(arg) => args.push(arg),
                    // The characters a declaration's argument admits are a
                    // closed set; one outside it is a routine whose grant the
                    // model cannot write down. Named rather than dropped —
                    // and named as the *whole* argument, because the reason
                    // this is a list and not a split string is that an
                    // argument may contain a comma.
                    Err(_) => {
                        return Err(format!(
                            "{} on `{name}` is on a routine whose argument `{spelled}` a \
                             declaration cannot spell",
                            g.permission
                        ));
                    }
                }
            }
            Ok(pbps_model::GrantTarget::Routine(
                pbps_model::RoutineId::new(name, args),
            ))
        }
        GrantedKind::Routine(other) => Err(format!(
            "{} on `{name}` is on {}, which this model does not declare",
            g.permission,
            routine_kind(other)
        )),
        // The schema arm is taken above, where `object` is `None`. A schema
        // row with an object name is a query and a struct that have drifted
        // apart, and it says so rather than picking one.
        GrantedKind::Schema => Err(format!(
            "{} on `{name}` came back as a grant on a schema that also names an object",
            g.permission
        )),
    }
}

/// A `pg_class.relkind` in the words a message uses.
fn relation_kind(relkind: char) -> &'static str {
    match relkind {
        'm' => "a materialized view",
        'p' => "a partitioned table",
        'f' => "a foreign table",
        'c' => "a composite type",
        _ => "a relation of a kind this reader does not know",
    }
}

/// A `pg_proc.prokind` in the same words.
fn routine_kind(prokind: char) -> &'static str {
    match prokind {
        'a' => "an aggregate",
        'w' => "a window function",
        _ => "a routine of a kind this reader does not know",
    }
}

/// A grant's target as a message names it, before the model has decided
/// whether it can hold one.
fn target_label(g: &RawGrant, signatures: &BTreeMap<i64, Vec<&str>>) -> String {
    match (&g.object, g.routine_oid) {
        (Some(object), Some(_)) => format!(
            "`{}.{object}({})`",
            g.schema,
            routine_args(g, signatures).join(", ")
        ),
        (Some(object), None) => format!("`{}.{object}`", g.schema),
        (None, _) => format!("schema `{}`", g.schema),
    }
}

/// The argument types of the routine this grant is on, in order.
///
/// Empty for a routine that takes none — which is a routine all the same, and
/// is why `RoutineId` keeps the parentheses.
fn routine_args<'a>(g: &RawGrant, signatures: &BTreeMap<i64, Vec<&'a str>>) -> Vec<&'a str> {
    g.routine_oid
        .and_then(|oid| signatures.get(&oid))
        .cloned()
        .unwrap_or_default()
}

/// One module, or a note saying why it is not one.
///
/// Nothing here ever leaves a module out silently: the three ways this can fail
/// — a kind the reader does not know, an argument type that is not one, and a
/// deparsed statement whose prefix is not the shape this engine writes — each
/// produce a warning naming the object. A module read as absent is a plan that
/// creates it, on top of the one that is already there.
fn add_module(
    raw: &RawModule,
    args_by_routine: &BTreeMap<i64, Vec<&RawModuleArg>>,
    pulled: &mut Pulled,
) {
    let here = ObjectName::new(&raw.schema, &raw.name);
    let (kind, id, definition) = match raw.kind {
        'v' => (
            ModuleKind::View,
            ModuleId::Named(here.clone()),
            // `pg_get_viewdef` returns the query and ends it with a `;`, and
            // the declaration holds what follows `AS` — where a `;` would end
            // the `CREATE` statement rather than the query inside it.
            Some(raw.definition.trim().trim_end_matches(';').trim_end()),
        ),
        kind @ ('f' | 'p') => {
            let mut args = Vec::new();
            for arg in args_by_routine.get(&raw.oid).map_or(&[][..], Vec::as_slice) {
                match arg.ty.parse::<RoutineArg>() {
                    Ok(parsed) => args.push(parsed),
                    Err(e) => {
                        return note(
                            pulled,
                            &here,
                            format!(
                                "`{here}` is a routine whose argument {} this model cannot hold as \
                                 an identity: {e}. It is left out of the pull, because a routine \
                                 keyed by the wrong signature is one whose `DROP` names another \
                                 object.",
                                arg.position
                            ),
                        );
                    }
                }
            }
            let prefix = if kind == 'f' {
                "CREATE OR REPLACE FUNCTION "
            } else {
                "CREATE OR REPLACE PROCEDURE "
            };
            (
                if kind == 'f' {
                    ModuleKind::Function
                } else {
                    ModuleKind::Procedure
                },
                ModuleId::Routine(RoutineId::new(here.clone(), args)),
                after_the_name(&raw.definition, prefix),
            )
        }
        't' => (
            ModuleKind::Trigger,
            ModuleId::Trigger {
                on: ObjectName::new(&raw.schema, &raw.on_table),
                name: raw.name.clone(),
            },
            after_the_name(&raw.definition, "CREATE TRIGGER "),
        ),
        other => {
            return note(
                pulled,
                &here,
                format!(
                    "`{here}` is a module of a kind this reader does not know (`{other}`). It is \
                     left out of the pull rather than read back as one of the kinds it is not."
                ),
            );
        }
    };

    let Some(definition) = definition.filter(|d| !d.is_empty()) else {
        return note(
            pulled,
            &here,
            format!(
                "`{here}` is a module whose definition this reader could not separate from the \
                 statement the engine deparsed for it. It is left out of the pull rather than \
                 recorded with a body that is not its own — an empty definition would read as a \
                 module with nothing in it, and the next plan would write that back."
            ),
        );
    };

    // The same round trip the tables are asked for: a module id is written as
    // text and read back by parsing it, and PostgreSQL will give a name that
    // does not survive that — a view called `f(int)`, a schema called `a.b`.
    if ModuleId::from_str(&id.to_string()).as_ref() != Ok(&id) {
        return note(
            pulled,
            &here,
            format!(
                "`{id}` is a module whose identity the declaration format cannot write back: it \
                 is stored as text and read by parsing it, and this one does not survive that. It \
                 is left out of the pull entirely rather than pulled into a schema that will not \
                 load."
            ),
        );
    }

    pulled.schema.modules.insert(
        id,
        Module {
            kind,
            description: None,
            definition: definition.to_owned(),
        },
    );
}

/// The part of a deparsed statement a declaration holds: everything after the
/// object's name.
///
/// **Measured on 18.6**, the three shapes this has to cut:
///
/// ```text
/// CREATE OR REPLACE FUNCTION m4."odd Name"(a integer)⏎ RETURNS integer …
/// CREATE OR REPLACE PROCEDURE m4.p(a integer)⏎ LANGUAGE sql …
/// CREATE TRIGGER "audit x" AFTER INSERT ON m4.t FOR EACH ROW …
/// ```
///
/// The name is stepped over rather than searched for. Looking for the first
/// `(` finds the wrong one in `"f(x)"."g"`, and rebuilding the name to compare
/// against would have to reproduce the engine's own quoting rules — which is
/// the deparser's job, not this reader's. `None` where the text is not this
/// shape at all, so that the caller can say so rather than record an empty
/// body (DECISIONS 304).
fn after_the_name<'a>(deparsed: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = deparsed.trim_start().strip_prefix(prefix)?;
    Some(after_a_qualified_name(rest)?.trim())
}

fn after_a_qualified_name(text: &str) -> Option<&str> {
    let mut rest = after_one_identifier(text)?;
    while let Some(next) = rest.strip_prefix('.') {
        rest = after_one_identifier(next)?;
    }
    Some(rest)
}

fn after_one_identifier(text: &str) -> Option<&str> {
    let Some(mut rest) = text.strip_prefix('"') else {
        // A bare identifier is what the deparser writes when no quoting is
        // needed, and it ends at the first character that cannot continue one.
        let end = text
            .find(|c: char| !pbps_dialect::continues_ident(c))
            .unwrap_or(text.len());
        return (end > 0).then(|| &text[end..]);
    };
    loop {
        let close = rest.find('"')?;
        let tail = &rest[close + 1..];
        // A doubled quote is a quote inside the name and does not close it.
        match tail.strip_prefix('"') {
            Some(after) => rest = after,
            None => return Some(tail),
        }
    }
}

fn group<T, K: Ord + Copy>(items: &[T], key: impl Fn(&T) -> K) -> BTreeMap<K, Vec<&T>> {
    let mut out: BTreeMap<K, Vec<&T>> = BTreeMap::new();
    for item in items {
        out.entry(key(item)).or_default().push(item);
    }
    out
}

/// The names of this table that the declaration format cannot write back, if
/// any.
///
/// A `TableName` is written as `schema.name` and read back by splitting on
/// every `.`, and a `ColumnRef` the same way with three parts. PostgreSQL will
/// happily give a schema called `"a.b"`, and then a pull that succeeded
/// produces a schema whose own file will not load — a table named `a.b.t`
/// parses as three parts and is refused, or worse, collides with the table `t`
/// in schema `a.b` reached the other way round.
///
/// So the round trip is *performed*, not reasoned about, and a name that does
/// not survive it takes its table out of the pull with a warning. The column
/// names are asked the same question, because a column is what a rename intent
/// and an ids file address, and a table whose column cannot be written is no
/// more usable than one whose own name cannot.
fn a_name_the_declaration_cannot_write(name: &TableName, columns: &[&RawColumn]) -> Option<String> {
    // The same question of a column's *type*. A spelling the catalogue cannot
    // read is kept as the engine wrote it (issue #130) — and `app.money_amount`
    // and `timestamp(3) with time zone` are both written out as themselves and
    // both refused on the way back, one for the dot and one for the words after
    // the parenthesis. Carrying them produces exactly the failure the name
    // check exists to prevent, one field over.
    // The question is whether the type comes back as **the same value**, not
    // whether it parses: `bit(3)` stored opaque writes out as `bit(3)` and
    // parses back as `bit` with an argument, which is a different type.
    //
    // Every offending column, not the first: the table is out either way, and
    // an operator reading this is deciding what to do about the columns, not
    // about the guard.
    let unwritable: Vec<String> = columns
        .iter()
        .filter(|c| !survives_the_declaration(&stored_type(&c.ty).unwrap_or_else(|opaque| opaque)))
        .map(|c| format!("`{}` (`{}`)", c.name, c.ty))
        .collect();
    if !unwritable.is_empty() {
        return Some(format!(
            "`{name}` has {} whose type this dialect's catalogue cannot spell and the \
             declaration format cannot write back either — a type is stored as its own spelling \
             and read by parsing it, and these do not survive that: {}. The whole table is left \
             out of the pull, because a schema that can be written and not loaded is worse than \
             one that says a table is missing (issue #130).",
            if unwritable.len() == 1 {
                "a column"
            } else {
                "columns"
            },
            unwritable.join(", ")
        ));
    }
    if TableName::from_str(&name.to_string()).as_ref() != Ok(name) {
        return Some(format!(
            "`{name}` is a table whose name the declaration format cannot write back: it is \
             stored as `schema.table` and read by splitting on every `.`, and this one does not \
             survive that. It is left out of the pull entirely rather than pulled into a schema \
             that will not load."
        ));
    }
    columns.iter().find_map(|c| {
        let reference = ColumnRef::new(name.clone(), &c.name);
        (ColumnRef::from_str(&reference.to_string()).as_ref() != Ok(&reference)).then(|| {
            format!(
                "`{name}` has the column `{}`, whose name the declaration format cannot write \
                 back: a column is addressed as `schema.table.column` and read by splitting on \
                 every `.`. The whole table is left out of the pull, because a table missing one \
                 column is a table a plan would add it to.",
                c.name
            )
        })
    })
}

/// The [`ColumnType`] a column will be recorded with, and whether the catalogue
/// could read it. One function, because the guard that refuses a type and the
/// code that stores it have to be looking at the same value — a check on
/// something *like* what is stored is a check on nothing.
///
/// `Err` is the opaque case (issue #130): the engine's spelling kept whole as
/// the base, with no arguments. A type pbps cannot normalize compares unequal
/// to every declaration, so the differ reports a change it will refuse to emit
/// rather than reporting nothing.
fn stored_type(ty: &str) -> Result<ColumnType, ColumnType> {
    // The type is the engine's own spelling, so it is already in the form
    // `normalize` promises to return (ADR-0011 Amendment 3). Parsing it can
    // still fail, and it must fail loudly: a column whose type pbps cannot
    // read is not a column with no type.
    ty.parse::<ColumnType>()
        .ok()
        .filter(|t| types::normalize(t).is_ok_and(|normalized| &normalized == t))
        .ok_or_else(|| ColumnType::new(ty.to_owned(), Vec::new()))
}

/// Whether a type survives being written to a declaration and read back as
/// **the same value**.
///
/// Not "does it parse". Measured, `bit(3)` is a legal spelling this catalogue
/// does not hold, so it is stored opaque — base `bit(3)`, no arguments — and it
/// writes out as `bit(3)` and parses back as base `bit` with the argument 3.
/// That parses perfectly and is a different type, so a schema written and
/// reloaded is not the schema that was pulled, and anywhere equality falls back
/// to the raw spelling it is a difference no plan can act on.
fn survives_the_declaration(ty: &ColumnType) -> bool {
    ColumnType::from_str(&String::from(ty.clone())).as_ref() == Ok(ty)
}

/// The `CACHE` a sequence has when nothing asks for one — **measured**, and the
/// one a plan that recreates an identity would get.
const DEFAULT_SEQUENCE_CACHE: i64 = 1;

/// [`note`], for the arms that are expressions rather than blocks. Always
/// `false`: a constraint that earns a warning here is one that was left out.
fn note_false(pulled: &mut Pulled, table: &TableName, detail: String) -> bool {
    note(pulled, table, detail);
    false
}

fn note(pulled: &mut Pulled, table: &TableName, detail: String) {
    pulled.warnings.push(detail.clone());
    pulled.limitations.push(Limitation {
        table: table.clone(),
        detail,
    });
}

/// One column, and everything about it the model has nowhere to put.
fn column(raw: &RawColumn, parts: &Parts, pulled: &mut Pulled) -> Column {
    let ty = match stored_type(&raw.ty) {
        Ok(ty) => ty,
        Err(opaque) => {
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
            opaque
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

    // The other half of that: a default that uses a sequence the column does
    // **not** own. `pg_depend` records this one from the `pg_attrdef` row, not
    // from the sequence to the column, so the join that finds a `serial`'s
    // sequence cannot see it — and the difference matters more, not less: the
    // sequence is nobody's to recreate, and `nextval` resolves its argument as
    // a `regclass` at execution, so the table cannot even be created without
    // it.
    if let Some(sequences) = &raw.default_sequences {
        note(
            pulled,
            &parts.name,
            format!(
                "column `{}`.`{}` defaults from the sequence {sequences}, which it does not own. \
                 This model holds the default and has nowhere to put the sequence, so a \
                 declaration pulled from here creates a table whose default names an object that \
                 is not there — and `nextval` resolves that name when the table is created, not \
                 when a row is inserted.",
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

/// Returns whether the constraint reached the pull. For a key constraint that
/// is what says its backing index is there; for the rest the answer is unused
/// and honest anyway.
fn add_constraint(
    raw: &RawConstraint,
    parts: &Parts,
    lookups: &Lookups,
    table: &mut Table,
    pulled: &mut Pulled,
) -> bool {
    match raw.kind {
        // PostgreSQL 18 gives every NOT NULL a `pg_constraint` row. The column
        // already carries it, and a reader that let this fall through to the
        // check arm would report one phantom check per NOT NULL column.
        'n' => false,

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
            false
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
            false
        }

        // A foreign key is not a fact in `pg_constraint`, it is triggers. A
        // superuser's `DISABLE TRIGGER ALL` stops them and leaves the row
        // saying `convalidated` and `conenforced` — so the pull would report
        // referential integrity that is not being applied, and writes may
        // already have left orphans behind it.
        'p' | 'u' | 'f' | 'c' | 'x' if raw.triggers_not_ordinary => {
            note(
                pulled,
                &parts.name,
                format!(
                    "constraint `{}` on `{}` is implemented by triggers that are not in the \
                     ordinary enable mode — `DISABLE TRIGGER` and the replica modes both land \
                     here — while the catalog still calls the constraint validated and enforced. \
                     This model holds only the constraint, so it is left out rather than read \
                     back as one whose checks are running. Its definition is `{}`.",
                    raw.name, parts.name, raw.definition
                ),
            );
            false
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
            false
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
            false
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
            false
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
            false
        }

        'p' => match parts.names(&raw.columns) {
            Ok(columns) => {
                table.primary_key = Some(PrimaryKey {
                    name: Some(raw.name.clone()),
                    columns,
                });
                true
            }
            Err(attnum) => {
                unresolved(pulled, parts, "primary key", &raw.name, attnum);
                false
            }
        },

        'u' => match parts.names(&raw.columns) {
            Ok(columns) => {
                table
                    .unique
                    .insert(raw.name.clone(), UniqueConstraint { columns });
                true
            }
            Err(attnum) => {
                unresolved(pulled, parts, "unique constraint", &raw.name, attnum);
                false
            }
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
                return false;
            };
            table.checks.insert(
                raw.name.clone(),
                // Verbatim, including the parentheses and casts the engine
                // welded on.
                CheckConstraint { expression },
            );
            false
        }

        'f' => add_foreign_key(raw, parts, lookups, table, pulled),

        'x' => note_false(
            pulled,
            &parts.name,
            format!(
                "`{}` on `{}` is an exclusion constraint, which this model does not hold. It is \
                 left out of the pull, so a plan cannot see it and `verify` cannot report a \
                 change to it.",
                raw.name, parts.name
            ),
        ),

        other => note_false(
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
) -> bool {
    // A foreign key is enforced against a uniqueness on the referenced table,
    // and `conindid` says which index that is. If it did not reach the pull —
    // the key constraint enforcing it has an `INCLUDE` payload, or is `NULLS
    // NOT DISTINCT`, or any of the other reasons this file leaves one out —
    // then the schema described here cannot be built: adding this key back
    // would fail for want of a uniqueness nothing mentions.
    if raw
        .index_oid
        .is_some_and(|oid| !lookups.surviving_indexes.contains(&oid))
    {
        note(
            pulled,
            &parts.name,
            format!(
                "foreign key `{}` on `{}` is enforced against a uniqueness on the referenced \
                 table that this pull left out, so it is left out too — read back it would be a \
                 key with nothing to point at, and recreating this schema would fail on it. Its \
                 definition is `{}`.",
                raw.name, parts.name, raw.definition
            ),
        );
        return false;
    }
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
        return false;
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
        return false;
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
                    other => {
                        unknown_match(pulled, parts, &raw.name, other);
                        return false;
                    }
                }
            ),
        );
        return false;
    }

    let Ok(columns) = parts.names(&raw.columns) else {
        unresolved(
            pulled,
            parts,
            "foreign key",
            &raw.name,
            *raw.columns.last().unwrap_or(&0),
        );
        return false;
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
            return false;
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
        return false;
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
    true
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
/// query either (DECISIONS 252, superseding 249).
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

/// Returns whether the index reached the pull, which is what tells a foreign
/// key whether the uniqueness it is enforced against is there.
fn add_index(raw: &RawIndex, parts: &Parts, table: &mut Table, pulled: &mut Pulled) -> bool {
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
        return false;
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
        return false;
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
        return false;
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
        return false;
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
        return false;
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
        return false;
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
        return false;
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
    true
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

    fn raw_module(kind: char, name: &str, definition: &str) -> RawModule {
        RawModule {
            oid: 1,
            kind,
            schema: "app".into(),
            name: name.into(),
            on_table: String::new(),
            definition: definition.into(),
        }
    }

    fn modules(raw: RawCatalog) -> Pulled {
        assemble(&raw)
    }

    fn role(name: &str) -> RawRole {
        RawRole {
            name: name.to_owned(),
            superuser: false,
        }
    }

    /// One expanded ACL row, of the shape the query returns: an explicit grant
    /// by somebody other than the owner.
    fn grant(
        grantee: Option<&str>,
        object: Option<&str>,
        kind: GrantedKind,
        permission: &str,
    ) -> RawGrant {
        RawGrant {
            grantee: grantee.map(str::to_owned),
            schema: "app".to_owned(),
            object: object.map(str::to_owned),
            // Oid 1 is the fixture's one routine; `signature` gives it the
            // arguments a test needs, and a test that gives it none leaves
            // the routine `f()`.
            routine_oid: matches!(kind, GrantedKind::Routine(_)).then_some(1),
            kind,
            permission: permission.to_owned(),
            grantable: false,
            column: None,
            defaulted: false,
            owner: "deploy".to_owned(),
        }
    }

    /// The argument rows for the fixture's single routine, oid 1.
    fn signature(types: &[&str]) -> Vec<RawModuleArg> {
        types
            .iter()
            .enumerate()
            .map(|(at, ty)| RawModuleArg {
                routine_oid: 1,
                position: at as i64 + 1,
                ty: (*ty).to_owned(),
            })
            .collect()
    }

    fn pulled_role<'a>(pulled: &'a Pulled, name: &str) -> &'a pbps_model::Role {
        pulled.schema.roles.get(name).expect("the role was pulled")
    }

    /// The objects the grant fixtures name, as the assembly ahead of
    /// `add_roles` records them: the table `app.customer`, the view
    /// `app.recent`, and one routine `app.f` of the given kind and signature.
    ///
    /// A fixture with no objects in it would not exercise the ordinary path at
    /// all — a grant on something this pull did not record is reported, never
    /// folded into a role's set, which is what `recorded` is for.
    fn declaring(kind: char, args: &[&str]) -> RawCatalog {
        RawCatalog {
            tables: vec![RawTable {
                oid: 10,
                schema: "app".to_owned(),
                name: "customer".to_owned(),
            }],
            modules: vec![
                RawModule {
                    oid: 2,
                    ..raw_module('v', "recent", " SELECT 1;")
                },
                raw_module(
                    kind,
                    "f",
                    &format!(
                        "CREATE OR REPLACE {} app.f({})\n LANGUAGE sql\nAS $$ SELECT 1 $$\n",
                        if kind == 'f' { "FUNCTION" } else { "PROCEDURE" },
                        args.join(", ")
                    ),
                ),
            ],
            module_args: signature(args),
            routine_args: signature(args),
            ..RawCatalog::default()
        }
    }

    /// The read-back of the ordinary case, and the one every other test here
    /// is a deviation from.
    #[test]
    fn a_grant_is_read_back_under_the_target_a_declaration_would_write() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("app_reader")],
            grants: vec![
                grant(Some("app_reader"), None, GrantedKind::Schema, "USAGE"),
                grant(
                    Some("app_reader"),
                    Some("customer"),
                    GrantedKind::Relation('r'),
                    "SELECT",
                ),
                grant(
                    Some("app_reader"),
                    Some("recent"),
                    GrantedKind::Relation('v'),
                    "SELECT",
                ),
                grant(
                    Some("app_reader"),
                    Some("f"),
                    GrantedKind::Routine('f'),
                    "EXECUTE",
                ),
            ],
            ..declaring('f', &["integer", "text"])
        });
        let targets: Vec<String> = pulled_role(&pulled, "app_reader")
            .grants
            .keys()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            targets,
            [
                "app.customer",
                "app.recent",
                "app.f(integer,text)",
                "schema::app"
            ]
        );
    }

    /// ADR-0010 §5, and the reason the reader asks the engine rather than
    /// carrying a table of defaults: a NULL ACL is expanded, and what it
    /// expands to is the zero point rather than a set of grants. Compared as
    /// grants it would deadlock the tool on its own output — every table pbps
    /// creates arrives with the owner's whole set.
    #[test]
    fn the_engines_default_is_reported_and_never_compared_as_a_grant() {
        let defaulted = |grantee: Option<&str>, permission: &str| RawGrant {
            defaulted: true,
            ..grant(grantee, Some("f"), GrantedKind::Routine('f'), permission)
        };
        let pulled = assemble(&RawCatalog {
            roles: vec![role("deploy")],
            grants: vec![
                defaulted(Some("deploy"), "EXECUTE"),
                defaulted(None, "EXECUTE"),
            ],
            ..RawCatalog::default()
        });
        assert!(pulled_role(&pulled, "deploy").grants.is_empty());
        // Reported, though: silence about the default is silence about the
        // fact that every principal in the cluster can execute it.
        let said = pulled.warnings.join("\n");
        assert!(said.contains("PUBLIC can execute 1 routine"), "{said}");
        assert!(said.contains("app.f()"), "{said}");
        assert!(
            pulled.unexpressible.is_empty(),
            "{:?}",
            pulled.unexpressible
        );
    }

    /// The owner's entry in an ACL somebody else made explicit is still the
    /// engine's, not a grant. Without this a single `GRANT SELECT` to a reader
    /// would materialise the owner's eight permissions as declared ones.
    #[test]
    fn the_owners_own_entry_is_the_zero_point_even_in_an_explicit_acl() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("deploy"), role("app_reader")],
            grants: vec![
                grant(
                    Some("deploy"),
                    Some("customer"),
                    GrantedKind::Relation('r'),
                    "SELECT",
                ),
                grant(
                    Some("deploy"),
                    Some("customer"),
                    GrantedKind::Relation('r'),
                    "MAINTAIN",
                ),
                grant(
                    Some("app_reader"),
                    Some("customer"),
                    GrantedKind::Relation('r'),
                    "SELECT",
                ),
            ],
            ..declaring('f', &[])
        });
        assert!(pulled_role(&pulled, "deploy").grants.is_empty());
        assert_eq!(pulled_role(&pulled, "app_reader").grants.len(), 1);
    }

    /// The revocation that is not a row. Measured, `REVOKE EXECUTE ... FROM
    /// PUBLIC` leaves `{postgres=X/postgres}` — so what says it happened is
    /// an explicit ACL with no PUBLIC entry in it, and a reader that only
    /// listed what the catalog holds would say nothing at all.
    #[test]
    fn execute_revoked_from_public_is_reported_although_it_is_the_absence_of_a_row() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("deploy")],
            grants: vec![grant(
                Some("deploy"),
                Some("f"),
                GrantedKind::Routine('f'),
                "EXECUTE",
            )],
            ..RawCatalog::default()
        });
        let said = pulled.warnings.join("\n");
        assert!(said.contains("revoked from PUBLIC"), "{said}");
        assert!(said.contains("app.f()"), "{said}");
    }

    /// The three ADR-0005 shapes, on this engine's catalogs. Each is left out
    /// of the role's set and reported, because folded in `verify` compares
    /// what remains and calls a changed role clean.
    #[test]
    fn what_the_model_cannot_hold_is_reported_and_never_folded_into_the_grants() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("app_reader")],
            grants: vec![
                RawGrant {
                    grantable: true,
                    ..grant(
                        Some("app_reader"),
                        Some("customer"),
                        GrantedKind::Relation('r'),
                        "SELECT",
                    )
                },
                RawGrant {
                    column: Some("email".to_owned()),
                    ..grant(
                        Some("app_reader"),
                        Some("customer"),
                        GrantedKind::Relation('r'),
                        "SELECT",
                    )
                },
                grant(
                    Some("app_reader"),
                    Some("customer_id_seq"),
                    GrantedKind::Relation('S'),
                    "USAGE",
                ),
            ],
            ..declaring('f', &[])
        });
        assert!(pulled_role(&pulled, "app_reader").grants.is_empty());
        let what: Vec<&str> = pulled
            .unexpressible
            .iter()
            .map(|u| u.what.as_str())
            .collect();
        assert_eq!(what.len(), 3, "{what:?}");
        assert!(
            what.iter().any(|w| w.contains("WITH GRANT OPTION")),
            "{what:?}"
        );
        assert!(
            what.iter().any(|w| w.contains("column-level grant")),
            "{what:?}"
        );
        // ADR-0010 §7: the other half of why `serial` is refused at load.
        assert!(what.iter().any(|w| w.contains("sequence")), "{what:?}");
        assert!(pulled.unexpressible.iter().all(|u| u.role == "app_reader"));
    }

    /// Every limitation on an object carries that object as its target, and
    /// only a grant with no nameable target at all is targetless.
    ///
    /// The managed-set cut keeps every targetless finding whatever it is about
    /// — a permission on the database itself belongs to no object, and a role
    /// that gained one has changed. So a column-level grant on somebody
    /// else's table, reported without a target, refused every connected plan
    /// while the ordinary grant beside it on that same table was dropped as
    /// none of this project's business (DECISIONS 176).
    #[test]
    fn a_limitation_on_an_object_carries_that_object_as_its_target() {
        let on_customer = |permission: &str| {
            grant(
                Some("app_reader"),
                Some("customer"),
                GrantedKind::Relation('r'),
                permission,
            )
        };
        let pulled = assemble(&RawCatalog {
            roles: vec![role("app_reader")],
            grants: vec![
                RawGrant {
                    column: Some("email".to_owned()),
                    ..on_customer("SELECT")
                },
                RawGrant {
                    grantable: true,
                    ..on_customer("INSERT")
                },
                // A word this engine has and the model does not hold.
                on_customer("CONNECT"),
            ],
            ..declaring('f', &[])
        });
        let customer: pbps_model::GrantTarget =
            "app.customer".parse().expect("a grant target parses");
        assert_eq!(pulled.unexpressible.len(), 3, "{:?}", pulled.unexpressible);
        assert!(
            pulled
                .unexpressible
                .iter()
                .all(|u| u.target.as_ref() == Some(&customer)),
            "{:?}",
            pulled.unexpressible
        );
    }

    /// A comma is not a separator. **Measured**, a type named `amount,type`
    /// renders as `cm."amount,type"`, so the comma that separates arguments
    /// and the comma inside one are the same character — and a signature read
    /// back as one string and split again turned a valid grant on a managed
    /// routine into targetless unexpressible state, which refuses the
    /// connected plan.
    #[test]
    fn a_comma_inside_an_argument_type_does_not_split_the_signature() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("app_reader")],
            grants: vec![grant(
                Some("app_reader"),
                Some("f"),
                GrantedKind::Routine('f'),
                "EXECUTE",
            )],
            ..declaring('f', &["app.\"amount,type\"", "integer"])
        });
        assert_eq!(
            pulled_role(&pulled, "app_reader")
                .grants
                .keys()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["app.f(app.\"amount,type\",integer)"]
        );
        assert!(
            pulled.unexpressible.is_empty(),
            "{:?}",
            pulled.unexpressible
        );
    }

    /// And an argument the declaration really cannot spell is named whole,
    /// which is the other half of not splitting: half an argument in an error
    /// message sends the reader to a type that does not exist. An unbalanced
    /// quote is the shape `RoutineArg` refuses — a quoted name with a comma or
    /// a parenthesis inside it is perfectly spellable, which is why the test
    /// above exists at all.
    #[test]
    fn an_argument_a_declaration_cannot_spell_is_named_in_full() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("app_reader")],
            grants: vec![grant(
                Some("app_reader"),
                Some("f"),
                GrantedKind::Routine('f'),
                "EXECUTE",
            )],
            routine_args: signature(&["app.\"never closed"]),
            ..RawCatalog::default()
        });
        assert!(pulled_role(&pulled, "app_reader").grants.is_empty());
        assert_eq!(pulled.unexpressible.len(), 1, "{:?}", pulled.unexpressible);
        assert!(
            pulled.unexpressible[0].what.contains("app.\"never closed"),
            "{}",
            pulled.unexpressible[0].what
        );
    }

    /// The kinds this model does not declare still hold real grants —
    /// **measured**, a `GRANT SELECT` on a materialized view and on a
    /// partitioned table both land in `relacl` — so each is reported. Left out
    /// of the read they would have made the role look as though it held
    /// nothing there, which is *absent* reading as *empty*.
    #[test]
    fn a_grant_on_a_relation_kind_this_model_does_not_declare_is_reported() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("app_reader")],
            grants: vec![
                grant(
                    Some("app_reader"),
                    Some("mv"),
                    GrantedKind::Relation('m'),
                    "SELECT",
                ),
                grant(
                    Some("app_reader"),
                    Some("parent"),
                    GrantedKind::Relation('p'),
                    "SELECT",
                ),
                grant(
                    Some("app_reader"),
                    Some("remote"),
                    GrantedKind::Relation('f'),
                    "SELECT",
                ),
                grant(
                    Some("app_reader"),
                    Some("agg"),
                    GrantedKind::Routine('a'),
                    "EXECUTE",
                ),
            ],
            ..RawCatalog::default()
        });
        assert!(pulled_role(&pulled, "app_reader").grants.is_empty());
        let what: Vec<&str> = pulled
            .unexpressible
            .iter()
            .map(|u| u.what.as_str())
            .collect();
        assert_eq!(what.len(), 4, "{what:?}");
        for named in [
            "a materialized view",
            "a partitioned table",
            "a foreign table",
            "an aggregate",
        ] {
            assert!(what.iter().any(|w| w.contains(named)), "{named}: {what:?}");
        }
    }

    /// The `relkind` letters and the `prokind` letters overlap, and the
    /// overlap is not harmless: `f` is a foreign table in one alphabet and a
    /// function in the other, `p` a partitioned table and a procedure. Read as
    /// one letter, a grant on a foreign table came back as a grant on a
    /// function of that name — a target the declarations may well have, and
    /// therefore a grant the next plan would compare.
    #[test]
    fn the_relkind_and_prokind_alphabets_are_not_read_as_one() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("app_reader")],
            grants: vec![
                grant(
                    Some("app_reader"),
                    Some("f"),
                    GrantedKind::Relation('f'),
                    "SELECT",
                ),
                grant(
                    Some("app_reader"),
                    Some("f"),
                    GrantedKind::Routine('p'),
                    "EXECUTE",
                ),
            ],
            ..declaring('p', &["integer"])
        });
        // The foreign table is unexpressible; the procedure is a grant.
        assert_eq!(
            pulled_role(&pulled, "app_reader")
                .grants
                .keys()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["app.f(integer)"]
        );
        assert_eq!(pulled.unexpressible.len(), 1, "{:?}", pulled.unexpressible);
        assert!(
            pulled.unexpressible[0].what.contains("a foreign table"),
            "{}",
            pulled.unexpressible[0].what
        );
    }

    /// A grant in a catalog whose target no declaration can name — a type, a
    /// language, a parameter. Reported per role, class and permission, because
    /// a database has as many of these as it has types; a role that gained one
    /// out of band has changed, and a reader that never looked would compare
    /// the grants it did see and call it clean (DECISIONS 105).
    #[test]
    fn a_grant_on_a_class_the_model_cannot_name_is_reported_grouped() {
        let other = |grantee: Option<&str>, class: &str, name: &str| RawOtherGrant {
            grantee: grantee.map(str::to_owned),
            class: class.to_owned(),
            name: name.to_owned(),
            permission: "USAGE".to_owned(),
            grantable: false,
            owner: Some("someone_else".to_owned()),
        };
        let pulled = assemble(&RawCatalog {
            roles: vec![role("app_reader")],
            other_grants: vec![
                other(Some("app_reader"), "a type", "app.money"),
                other(Some("app_reader"), "a type", "app.code"),
                other(Some("app_reader"), "a procedural language", "plpgsql"),
                // PUBLIC holds `USAGE` on every built-in type in every
                // database there is: the same everywhere, so listing it would
                // bury the rows that are not.
                other(None, "a type", "text"),
                // And a role this project cannot declare is not its business.
                other(Some("someone_else"), "a type", "app.money"),
            ],
            ..RawCatalog::default()
        });
        let what: Vec<&str> = pulled
            .unexpressible
            .iter()
            .map(|u| u.what.as_str())
            .collect();
        assert_eq!(what.len(), 2, "{what:?}");
        assert!(pulled.unexpressible.iter().all(|u| u.role == "app_reader"));
        assert!(pulled.unexpressible.iter().all(|u| u.target.is_none()));
        let types = what
            .iter()
            .find(|w| w.contains("a type"))
            .expect("the type line");
        assert!(types.contains("app.money"), "{types}");
        assert!(types.contains("app.code"), "{types}");
    }

    /// The zero point reaches those catalogs too (DECISIONS 371). Every one of
    /// them is NULL until somebody touches it, and **measured on 18.6**,
    /// `REVOKE USAGE ON TYPE ot.money_kind FROM PUBLIC` turns a NULL `typacl`
    /// into `{ot_owner=U/ot_owner}` — the owner's own inherent `USAGE`,
    /// written by the engine. Read as a grant it says a managed role holds
    /// something unnameable, which refuses every plan connected to it.
    #[test]
    fn the_owners_own_entry_in_an_unnameable_class_is_the_zero_point() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("ot_owner")],
            other_grants: vec![RawOtherGrant {
                grantee: Some("ot_owner".to_owned()),
                class: "a type".to_owned(),
                name: "app.money_kind".to_owned(),
                permission: "USAGE".to_owned(),
                grantable: false,
                owner: Some("ot_owner".to_owned()),
            }],
            ..RawCatalog::default()
        });
        assert!(
            pulled.unexpressible.is_empty(),
            "{:?}",
            pulled.unexpressible
        );
    }

    /// A grant on an object the assembly ahead of this one left out — a table
    /// whose column type the declaration cannot write back, a routine whose
    /// argument the model cannot hold. The grant is real and the object is
    /// not in the pull, so folding it into the role would write a project
    /// whose own `validate` refuses it, naming a target no declaration has.
    #[test]
    fn a_grant_on_an_object_this_pull_did_not_record_is_reported_not_folded_in() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("app_reader")],
            grants: vec![
                grant(Some("app_reader"), None, GrantedKind::Schema, "USAGE"),
                grant(
                    Some("app_reader"),
                    Some("customer"),
                    GrantedKind::Relation('r'),
                    "SELECT",
                ),
            ],
            // No tables and no modules: the table the grant is on was left out.
            ..RawCatalog::default()
        });
        assert_eq!(
            pulled_role(&pulled, "app_reader")
                .grants
                .keys()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["schema::app"],
            "the schema grant stands; the one on the missing table does not"
        );
        assert_eq!(pulled.unexpressible.len(), 1, "{:?}", pulled.unexpressible);
        assert!(
            pulled.unexpressible[0].what.contains("did not record"),
            "{}",
            pulled.unexpressible[0].what
        );
    }

    /// And the object it did not record is looked for in the target's own
    /// namespace. A relation ACL row becomes an `Object`, and a routine of the
    /// same name is a different object on this engine — measured, a table
    /// `co.f` and a function `co.f(integer)` coexist. Counted as an answer,
    /// the hidden table's grant went into the role and `validate::role` then
    /// refused the schema this pull had just written, looking for a relation
    /// `app.f` that is not there.
    #[test]
    fn a_relation_left_out_is_not_answered_for_by_a_routine_of_the_same_name() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("app_reader")],
            grants: vec![
                grant(Some("app_reader"), None, GrantedKind::Schema, "USAGE"),
                // The routine `app.f(integer)` is in the pull; a table of that
                // name would be too, and this fixture leaves it out.
                grant(
                    Some("app_reader"),
                    Some("f"),
                    GrantedKind::Relation('r'),
                    "SELECT",
                ),
                grant(
                    Some("app_reader"),
                    Some("f"),
                    GrantedKind::Routine('f'),
                    "EXECUTE",
                ),
            ],
            ..declaring('f', &["integer"])
        });
        assert_eq!(
            pulled_role(&pulled, "app_reader")
                .grants
                .keys()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["app.f(integer)", "schema::app"],
            "the routine grant stands; the one on the missing table does not"
        );
        assert_eq!(pulled.unexpressible.len(), 1, "{:?}", pulled.unexpressible);
        assert!(
            pulled.unexpressible[0].what.contains("did not record"),
            "{}",
            pulled.unexpressible[0].what
        );
    }

    /// The target crosses a snapshot as its string form, and in that form a
    /// `(` opens a routine signature. **Measured**: `app.sales(archive)` is a
    /// legal table name this dialect writes back unchanged, and it parses back
    /// as `GrantTarget::Routine(app.sales(archive))` — a different object.
    /// Recorded, the grant would reload as one on a routine, or not at all
    /// (DECISIONS 205, the same shape on the other engine).
    #[test]
    fn a_target_whose_written_form_reads_back_as_another_object_is_reported() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("app_reader")],
            tables: vec![RawTable {
                oid: 11,
                schema: "app".to_owned(),
                name: "sales(archive)".to_owned(),
            }],
            grants: vec![grant(
                Some("app_reader"),
                Some("sales(archive)"),
                GrantedKind::Relation('r'),
                "SELECT",
            )],
            ..RawCatalog::default()
        });
        // The table itself is in the pull: its name survives the *declaration*
        // format, which is what keeps it there — only the grant target's form
        // is ambiguous.
        assert!(
            pulled
                .schema
                .tables
                .contains_key(&"app.sales(archive)".parse().expect("a table name parses")),
            "{:?}",
            pulled.schema.tables.keys().collect::<Vec<_>>()
        );
        assert!(pulled_role(&pulled, "app_reader").grants.is_empty());
        assert_eq!(pulled.unexpressible.len(), 1, "{:?}", pulled.unexpressible);
        assert!(
            pulled.unexpressible[0].what.contains("parenthesis"),
            "{}",
            pulled.unexpressible[0].what
        );
    }

    /// `WITH GRANT OPTION` survives into the message on those classes too: a
    /// role that can hand `USAGE` on a type onward is not the same as one that
    /// merely holds it.
    #[test]
    fn a_grant_option_on_an_unnameable_class_is_said_rather_than_flattened() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("app_reader")],
            other_grants: vec![RawOtherGrant {
                grantee: Some("app_reader".to_owned()),
                class: "a type".to_owned(),
                name: "app.money".to_owned(),
                permission: "USAGE".to_owned(),
                grantable: true,
                owner: Some("someone_else".to_owned()),
            }],
            ..RawCatalog::default()
        });
        assert_eq!(pulled.unexpressible.len(), 1);
        assert!(
            pulled.unexpressible[0].what.contains("WITH GRANT OPTION"),
            "{}",
            pulled.unexpressible[0].what
        );
    }

    /// A role with no grants is a role with no grants — not a missing one.
    /// The two are the difference between "nothing was revoked" and "somebody
    /// dropped the role", and only one of them is good news.
    #[test]
    fn a_role_holding_nothing_is_pulled_as_a_role_and_not_left_out() {
        let pulled = assemble(&RawCatalog {
            roles: vec![role("app_reader")],
            ..RawCatalog::default()
        });
        assert!(pulled.schema.roles.contains_key("app_reader"));
        assert!(pulled_role(&pulled, "app_reader").grants.is_empty());
    }

    /// ADR-0010 §2. Not a grant on anything that exists, and not something any
    /// declaration can say — so it is reported, with the role whose creations
    /// it covers, which is the fact that makes it unportable.
    #[test]
    fn a_default_privileges_entry_names_the_role_whose_creations_it_covers() {
        let pulled = assemble(&RawCatalog {
            default_acls: vec![RawDefaultAcl {
                grantor: "owner_a".to_owned(),
                in_schema: Some("app".to_owned()),
                objtype: 'r',
                acl: "{all_reader=r/owner_a}".to_owned(),
            }],
            ..RawCatalog::default()
        });
        let said = pulled.warnings.join("\n");
        assert!(said.contains("FOR ROLE owner_a"), "{said}");
        assert!(said.contains("the tables and views"), "{said}");
        assert!(said.contains("who creates an object"), "{said}");
    }

    /// A superuser's grants do not decide what it can do, and a pull that
    /// silently left it out would be the failure this project is built to
    /// avoid.
    #[test]
    fn a_superuser_is_pulled_and_named_rather_than_filtered_out() {
        let pulled = assemble(&RawCatalog {
            roles: vec![RawRole {
                name: "deploy".to_owned(),
                superuser: true,
            }],
            ..RawCatalog::default()
        });
        assert!(pulled.schema.roles.contains_key("deploy"));
        assert!(
            pulled.warnings.iter().any(|w| w.contains("superuser")),
            "{:?}",
            pulled.warnings
        );
    }

    /// Measured on 18.6, one shape per kind. The declaration holds everything
    /// after the name, so what this asserts is where the cut falls — including
    /// on a name the deparser had to quote, which is the case a search for the
    /// first `(` gets wrong.
    #[test]
    fn a_deparsed_statement_is_cut_where_the_declaration_begins() {
        let raw = RawCatalog {
            modules: vec![
                raw_module(
                    'v',
                    "v",
                    " SELECT id,\n    a\n   FROM app.t\n  WHERE a IS NULL;",
                ),
                raw_module(
                    'f',
                    "odd Name",
                    "CREATE OR REPLACE FUNCTION app.\"odd Name\"(a integer)\n RETURNS integer\n \
                     LANGUAGE sql\nAS $function$ SELECT a $function$\n",
                ),
                raw_module(
                    'p',
                    "p",
                    "CREATE OR REPLACE PROCEDURE app.p(a integer)\n LANGUAGE sql\nAS $procedure$ \
                     SELECT 1 $procedure$\n",
                ),
                RawModule {
                    on_table: "t".into(),
                    ..raw_module(
                        't',
                        "audit x",
                        "CREATE TRIGGER \"audit x\" AFTER INSERT ON app.t FOR EACH ROW EXECUTE \
                         FUNCTION app.trf()",
                    )
                },
            ],
            module_args: vec![RawModuleArg {
                routine_oid: 1,
                position: 1,
                ty: "integer".into(),
            }],
            ..RawCatalog::default()
        };
        let pulled = modules(raw);
        let got: Vec<(String, String)> = pulled
            .schema
            .modules
            .iter()
            .map(|(id, m)| (id.to_string(), m.definition.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                // The `;` `pg_get_viewdef` ends the query with is not part of
                // what follows `AS`, and neither is the layout it leads with.
                (
                    "app.v".to_owned(),
                    "SELECT id,\n    a\n   FROM app.t\n  WHERE a IS NULL".to_owned(),
                ),
                (
                    "app.odd Name(integer)".to_owned(),
                    "(a integer)\n RETURNS integer\n LANGUAGE sql\nAS $function$ SELECT a \
                     $function$"
                        .to_owned()
                ),
                (
                    "app.p(integer)".to_owned(),
                    "(a integer)\n LANGUAGE sql\nAS $procedure$ SELECT 1 $procedure$".to_owned()
                ),
                (
                    "app.t.audit x".to_owned(),
                    "AFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.trf()".to_owned()
                ),
            ],
            "{pulled:#?}"
        );
        assert!(pulled.warnings.is_empty(), "{:?}", pulled.warnings);
    }

    /// Absent, empty and unreadable are three different things. A module the
    /// reader cannot cut is named and left out — never recorded with an empty
    /// body, which the next plan would write back over a working object.
    #[test]
    fn a_statement_this_reader_cannot_cut_is_named_and_never_read_as_empty() {
        for definition in [
            // Not the prefix this engine writes.
            "CREATE FUNCTION app.f(a integer) RETURNS integer",
            // The prefix, and then nothing that is a name.
            "CREATE OR REPLACE FUNCTION (a integer)",
            // A quoted name nothing closes.
            "CREATE OR REPLACE FUNCTION app.\"f(a integer)",
            // The name, and nothing after it.
            "CREATE OR REPLACE FUNCTION app.f",
            "",
        ] {
            let pulled = modules(RawCatalog {
                modules: vec![raw_module('f', "f", definition)],
                ..RawCatalog::default()
            });
            assert!(
                pulled.schema.modules.is_empty(),
                "`{definition}` produced a module"
            );
            assert_eq!(pulled.warnings.len(), 1, "`{definition}`");
            assert!(
                pulled.warnings[0].contains("app.f"),
                "{}",
                pulled.warnings[0]
            );
        }
    }

    /// The same question the tables are asked: a name the declaration format
    /// cannot write back takes its object out of the pull. A view called
    /// `f(int)` reads back as a routine with an argument list, which is a
    /// different object under a key nothing would ever match.
    #[test]
    fn a_module_whose_name_the_declaration_cannot_write_is_left_out_and_named() {
        let pulled = modules(RawCatalog {
            modules: vec![raw_module('v', "f(int)", "SELECT 1")],
            ..RawCatalog::default()
        });
        assert!(pulled.schema.modules.is_empty(), "{pulled:#?}");
        assert_eq!(pulled.warnings.len(), 1, "{:?}", pulled.warnings);
        assert!(
            pulled.warnings[0].contains("cannot write back"),
            "{}",
            pulled.warnings[0]
        );
    }

    /// A routine keyed by the wrong signature is a routine whose `DROP` names
    /// another object, so an argument spelling this model cannot hold takes it
    /// out rather than being dropped from the list.
    #[test]
    fn a_routine_argument_the_model_cannot_hold_takes_the_routine_out() {
        let pulled = modules(RawCatalog {
            modules: vec![raw_module(
                'f',
                "f",
                "CREATE OR REPLACE FUNCTION app.f(a integer) RETURNS integer",
            )],
            module_args: vec![RawModuleArg {
                routine_oid: 1,
                position: 1,
                ty: "integer; DROP TABLE t".into(),
            }],
            ..RawCatalog::default()
        });
        assert!(pulled.schema.modules.is_empty(), "{pulled:#?}");
        assert_eq!(pulled.warnings.len(), 1, "{:?}", pulled.warnings);
        assert!(
            pulled.warnings[0].contains("app.f"),
            "{}",
            pulled.warnings[0]
        );
    }

    /// A kind the reader has never seen is not read back as one of the kinds it
    /// is not — the failure that would recreate a materialized view as a view.
    #[test]
    fn a_kind_this_reader_does_not_know_is_named_rather_than_guessed() {
        let pulled = modules(RawCatalog {
            modules: vec![raw_module('m', "mv", "SELECT 1")],
            ..RawCatalog::default()
        });
        assert!(pulled.schema.modules.is_empty(), "{pulled:#?}");
        assert_eq!(pulled.warnings.len(), 1, "{:?}", pulled.warnings);
    }
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
            default_sequences: None,
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
            triggers_not_ordinary: false,
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
                ..RawCatalog::default()
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
                ..RawCatalog::default()
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
            ..RawCatalog::default()
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

    /// A type the catalogue cannot spell is the one #130 is about, and #130
    /// says what the pull owes it: "`pull` will have to report it as unmanaged
    /// rather than adopt it". Kept as the engine spelled it, the column made a
    /// schema that can be written and not loaded — `app.money_amount` is
    /// refused for the dot and `timestamp(3) with time zone` for the words
    /// after the parenthesis. Silence would be worse than either.
    #[test]
    fn a_type_the_catalogue_cannot_spell_takes_its_table_out_and_is_named() {
        for spelling in [
            "timestamp(3) with time zone",
            "app.money_amount",
            // The one that *parses* and comes back a different value: stored
            // opaque as the base `bit(3)` with no arguments, it reads back as
            // the base `bit` with the argument 3. A guard that asked only
            // whether it parses let this through.
            "bit(3)",
        ] {
            let raw = RawCatalog {
                tables: vec![table(1, "t")],
                columns: vec![col(1, 1, "when", spelling)],
                constraints: Vec::new(),
                indexes: Vec::new(),
                ..RawCatalog::default()
            };
            let pulled = assemble(&raw);
            assert!(pulled.schema.tables.is_empty(), "{spelling}");
            assert_eq!(pulled.limitations.len(), 1, "{spelling}");
            assert!(pulled.warnings[0].contains("issue #130"), "{spelling}");
            assert!(pulled.warnings[0].contains(spelling), "{spelling}");
        }

        // The negative case: a spelling the catalogue reads is carried and
        // earns nothing.
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "when", "timestamp with time zone")],
            constraints: Vec::new(),
            indexes: Vec::new(),
            ..RawCatalog::default()
        };
        let pulled = assemble(&raw);
        assert_eq!(
            only(&pulled).columns["when"].ty.to_string(),
            "timestamp with time zone"
        );
        assert!(pulled.warnings.is_empty(), "{:?}", pulled.warnings);
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
                ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
                ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
                ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
                ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
                ..RawCatalog::default()
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

    /// A default over somebody else's sequence is not a `serial`, and the
    /// dependency that records it hangs off the default rather than the column.
    #[test]
    fn a_default_over_a_sequence_the_column_does_not_own_is_carried_and_named() {
        let mut column = col(1, 1, "id", "integer");
        column.default = Some("nextval('app.s'::regclass)".to_owned());
        column.default_sequences = Some("`app.s`".to_owned());
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![column],
            constraints: Vec::new(),
            indexes: Vec::new(),
            ..RawCatalog::default()
        };
        let pulled = assemble(&raw);
        // Carried: the default is exactly what the model says. Named: the
        // sequence it needs is not in the pull and cannot be created from it.
        assert_eq!(
            only(&pulled).columns["id"].default.as_deref(),
            Some("nextval('app.s'::regclass)")
        );
        assert!(
            pulled.warnings[0].contains("does not own"),
            "{:?}",
            pulled.warnings
        );
        assert!(pulled.warnings[0].contains("`app.s`"));

        // The negative case: an ordinary default earns nothing.
        let mut plain = col(1, 1, "id", "integer");
        plain.default = Some("7".to_owned());
        let raw = RawCatalog {
            columns: vec![plain],
            ..raw
        };
        assert!(assemble(&raw).warnings.is_empty());
    }

    /// A foreign key is triggers. `DISABLE TRIGGER` stops them and leaves the
    /// catalog row saying the constraint is validated and enforced.
    #[test]
    fn a_constraint_whose_triggers_are_not_running_is_left_out_and_named() {
        let mut fk = constraint(1, "t_fk", 'f');
        fk.columns = vec![1];
        fk.ref_columns = vec![1];
        fk.ref_table = Some(1);
        fk.definition = "FOREIGN KEY (a) REFERENCES app.t(a)".to_owned();
        fk.triggers_not_ordinary = true;
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "a", "integer")],
            constraints: vec![fk],
            indexes: Vec::new(),
            ..RawCatalog::default()
        };
        let pulled = assemble(&raw);
        assert!(only(&pulled).foreign_keys.is_empty());
        assert!(
            pulled.warnings[0].contains("ordinary enable mode"),
            "{:?}",
            pulled.warnings
        );

        // The negative case: the same key with its triggers running is an
        // ordinary foreign key.
        let mut running = constraint(1, "t_fk", 'f');
        running.columns = vec![1];
        running.ref_columns = vec![1];
        running.ref_table = Some(1);
        let raw = RawCatalog {
            constraints: vec![running],
            ..raw
        };
        let pulled = assemble(&raw);
        assert_eq!(only(&pulled).foreign_keys.len(), 1);
        assert!(pulled.warnings.is_empty(), "{:?}", pulled.warnings);
    }

    /// A name the pull can read and the declaration format cannot write is a
    /// schema that loads back as something else, or not at all.
    #[test]
    fn a_name_that_does_not_survive_the_declaration_format_takes_its_table_out() {
        // A schema with a period in it. Legal in PostgreSQL as `"a.b"`, and
        // `a.b.t` reads back as three parts.
        let mut odd = table(1, "t");
        odd.schema = "a.b".to_owned();
        let raw = RawCatalog {
            tables: vec![odd],
            columns: vec![col(1, 1, "x", "integer")],
            constraints: Vec::new(),
            indexes: Vec::new(),
            ..RawCatalog::default()
        };
        let pulled = assemble(&raw);
        assert!(
            pulled.schema.tables.is_empty(),
            "{:?}",
            pulled.schema.tables
        );
        assert!(
            pulled.warnings[0].contains("cannot write back"),
            "{:?}",
            pulled.warnings
        );

        // And a column, addressed as `schema.table.column` by every rename
        // intent and every ids file.
        let raw = RawCatalog {
            tables: vec![table(1, "t")],
            columns: vec![col(1, 1, "a.b", "integer")],
            constraints: Vec::new(),
            indexes: Vec::new(),
            ..RawCatalog::default()
        };
        let pulled = assemble(&raw);
        assert!(
            pulled.schema.tables.is_empty(),
            "{:?}",
            pulled.schema.tables
        );
        assert!(
            pulled.warnings[0].contains("`a.b`"),
            "{:?}",
            pulled.warnings
        );

        // The negative case: ordinary names round-trip and earn nothing.
        let raw = RawCatalog {
            columns: vec![col(1, 1, "a", "integer")],
            ..raw
        };
        let pulled = assemble(&raw);
        assert_eq!(pulled.schema.tables.len(), 1);
        assert!(pulled.warnings.is_empty(), "{:?}", pulled.warnings);
    }

    /// A table the pull refuses is not a table a foreign key may point at.
    /// The referencing table is assembled on its own turn, which can come
    /// first, so the decision has to be made before anything is built.
    #[test]
    fn a_foreign_key_pointing_at_a_refused_table_is_left_out_and_named() {
        // The referencing table comes first, so its turn is taken before the
        // refused one's would have been.
        let mut target = table(2, "t");
        target.schema = "a.b".to_owned();
        let mut fk = constraint(1, "c_fk", 'f');
        fk.columns = vec![1];
        fk.ref_columns = vec![1];
        fk.ref_table = Some(2);
        let raw = RawCatalog {
            tables: vec![table(1, "c"), target],
            columns: vec![col(1, 1, "a", "integer"), col(2, 1, "a", "integer")],
            constraints: vec![fk],
            indexes: Vec::new(),
            ..RawCatalog::default()
        };
        let pulled = assemble(&raw);
        assert_eq!(pulled.schema.tables.len(), 1, "{:?}", pulled.schema.tables);
        assert!(
            pulled.schema.tables[&TableName::new("app", "c")]
                .foreign_keys
                .is_empty(),
            "a key pointing at a table that is not in the pull is not a key"
        );
        // Both facts reach the operator: the table that could not be written,
        // and the key that could not be kept because of it.
        assert!(
            pulled
                .warnings
                .iter()
                .any(|w| w.contains("cannot write back")),
            "{:?}",
            pulled.warnings
        );
        assert!(
            pulled.warnings.iter().any(|w| w.contains("c_fk")),
            "{:?}",
            pulled.warnings
        );

        // The negative case: the same key at a table whose name round-trips.
        let raw = RawCatalog {
            tables: vec![table(1, "c"), table(2, "t")],
            ..raw
        };
        let pulled = assemble(&raw);
        assert_eq!(
            pulled.schema.tables[&TableName::new("app", "c")]
                .foreign_keys
                .len(),
            1
        );
        assert!(pulled.warnings.is_empty(), "{:?}", pulled.warnings);
    }

    /// A foreign key is legal only against a uniqueness that is there. When the
    /// key constraint enforcing it is left out — here for an `INCLUDE` payload
    /// — the key has nothing to point at, and the schema described cannot be
    /// built.
    #[test]
    fn a_foreign_key_whose_referenced_uniqueness_was_left_out_goes_with_it() {
        // The referencing table is first, so its turn comes before the
        // referenced table's constraint has been decided.
        let mut fk = constraint(1, "c_fk", 'f');
        fk.columns = vec![1];
        fk.ref_columns = vec![1];
        fk.ref_table = Some(2);
        fk.index_oid = Some(50);
        let mut pk = constraint(2, "p_pk", 'p');
        pk.columns = vec![1];
        pk.index_oid = Some(50);
        let mut backing = index(50, 2, "p_pk");
        backing.unique = true;
        backing.primary = true;
        backing.key_count = 1;
        backing.columns = vec![1, 2];
        let raw = RawCatalog {
            tables: vec![table(1, "c"), table(2, "p")],
            columns: vec![
                col(1, 1, "a", "integer"),
                col(2, 1, "a", "integer"),
                col(2, 2, "b", "integer"),
            ],
            constraints: vec![fk, pk],
            indexes: vec![backing],
            ..RawCatalog::default()
        };
        let pulled = assemble(&raw);
        assert!(
            pulled.schema.tables[&TableName::new("app", "p")]
                .primary_key
                .is_none()
        );
        assert!(
            pulled.schema.tables[&TableName::new("app", "c")]
                .foreign_keys
                .is_empty(),
            "a key against a uniqueness that is not in the pull is not a key"
        );
        assert!(
            pulled.warnings.iter().any(|w| w.contains("INCLUDE")),
            "{:?}",
            pulled.warnings
        );
        assert!(
            pulled
                .warnings
                .iter()
                .any(|w| w.contains("uniqueness on the referenced table")),
            "{:?}",
            pulled.warnings
        );

        // The negative case: the same key against a primary key that survives.
        let mut pk = constraint(2, "p_pk", 'p');
        pk.columns = vec![1];
        pk.index_oid = Some(50);
        let mut backing = index(50, 2, "p_pk");
        backing.unique = true;
        backing.primary = true;
        backing.key_count = 1;
        backing.columns = vec![1];
        let mut fk = constraint(1, "c_fk", 'f');
        fk.columns = vec![1];
        fk.ref_columns = vec![1];
        fk.ref_table = Some(2);
        fk.index_oid = Some(50);
        let raw = RawCatalog {
            constraints: vec![fk, pk],
            indexes: vec![backing],
            ..raw
        };
        let pulled = assemble(&raw);
        assert_eq!(
            pulled.schema.tables[&TableName::new("app", "c")]
                .foreign_keys
                .len(),
            1
        );
        assert!(pulled.warnings.is_empty(), "{:?}", pulled.warnings);
    }

    /// The round trip is asked of the value that is stored, and it asks for
    /// equality. Parsing is not the question: a spelling can parse into a
    /// different type than the one written out.
    #[test]
    fn a_type_that_parses_back_as_a_different_value_does_not_survive() {
        // Opaque, because this catalogue does not hold `bit`. Written out and
        // read back it is `bit` with an argument, which is not this value.
        let opaque = stored_type("bit(3)").expect_err("the catalogue cannot read it");
        assert_eq!(opaque.base, "bit(3)");
        assert!(opaque.args.is_empty());
        assert!(
            ColumnType::from_str(&String::from(opaque.clone())).is_ok(),
            "it parses, which is why parsing was the wrong question"
        );
        assert!(!survives_the_declaration(&opaque));

        // And the two that do not even parse.
        for spelling in ["app.money_amount", "timestamp(3) with time zone"] {
            let opaque = stored_type(spelling).expect_err(spelling);
            assert!(!survives_the_declaration(&opaque), "{spelling}");
        }

        // The negative case: a type the catalogue reads survives, and so does
        // one it reads with arguments.
        for spelling in ["integer", "numeric(10,2)", "timestamp with time zone"] {
            let ty = stored_type(spelling).expect(spelling);
            assert!(survives_the_declaration(&ty), "{spelling}");
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
            ..RawCatalog::default()
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
                ..RawCatalog::default()
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
            ..RawCatalog::default()
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
            ..RawCatalog::default()
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
