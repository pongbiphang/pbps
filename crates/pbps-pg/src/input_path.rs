//! May `ADD COLUMN` run this type's input path? (#1523, PR
//! C1; the design is #2046, layer 1.)
//!
//! `ALTER TABLE … ADD COLUMN c some_domain` can run the domain's CHECK constraints
//! and DEFAULT as the deploying role, even without rows for a fast default
//! (measured on 16 and 18, DEC-1523.4). Whoever wrote those expressions, or the
//! functions they call,
//! therefore runs code with the deployer's privileges. This module reads what is
//! needed to judge that before plan probes and apply statements (DEC-1523.1).
//!
//! # The two questions
//!
//! A type is admitted when both hold (#2046):
//!
//! - **(a) the code is built in.** Every function, every operator's function and
//!   every cast or coercion function that a CHECK or a DEFAULT on the input path
//!   calls has an OID below `FirstNormalObjectId` (16384), so initdb made it and
//!   no role can have written or replaced it, and it is not on the short list of
//!   built-ins that run SQL text or a type's code by name ([`RUNS_SQL`]). No
//!   user function runs, so no user SQL runs, and no trigger, rule, policy or
//!   view is reached: every route review kept finding by enumeration is absent by
//!   construction. A row comparison is the one operator the executor does not
//!   call: it runs its operator family's btree comparison function for the
//!   operator's types, so that function is the code judged, not the operator's.
//! - **(b) the types are owned by trusted roles.** Only the owner of a type can
//!   change what its input path runs, and so can every role that can act as that
//!   owner: by membership with `SET` or `INHERIT`, or by `ADMIN`, which can grant
//!   itself either. A role is trusted when it is a superuser or can `SET ROLE` to
//!   the deploying role or to a superuser (DEC-1663.1, the walk in
//!   [`crate::catalog`]), or when the caller has declared it trusted. A role that
//!   can act as an owner without being trusted is named in the refusal.
//!
//! Ownership applies to the whole input path, even where no code runs now:
//! an untrusted owner could add code after validation. Pins detect changes;
//! they add to owner trust rather than replacing it (DEC-2181.3).
//!
//! # The input path
//!
//! It is the type itself, a domain's base and any evaluated domain coercion
//! in a stored expression.
//! A NULL composite, array or range does not evaluate its parts; a statement
//! DEFAULT can construct them. Stored typed constants have already passed
//! input conversion, unlike ROW, array coercion and range constructors
//! (measured on 16 and 18). A cast to a domain follows that domain's CHECKs;
//! an I/O coercion also follows the target's reconstructed attributes, elements
//! and range bounds. A known node layout without an audited execution proof is
//! unreadable (DEC-2181.4).
//! Built-in types keep the same structural ownership walk. The set is closed
//! by reading, not by listing the kinds of object that might lead elsewhere.
//!
//! Every CHECK of every domain on the path counts, but only the declared type's
//! own DEFAULT does: `ADD COLUMN` evaluates the column type's default and no
//! other, so the default of a domain that type is made of, or that an expression
//! names, never runs (a domain over another is created with a copy of that
//! one's default, which is then its own). And not even that one when the
//! statement carries a DEFAULT of its own ([`Added::new`]), which replaces it.
//! Measured on 16 and 18, and pinned by live tests; counting them all would
//! refuse plans that run nothing.
//!
//! # Why the node trees, not `pg_depend`
//!
//! The engine's own record of what an expression calls is `pg_depend`, and it
//! was measured first (16.15 and 18.6: thirty CHECK shapes and eleven DEFAULT
//! shapes, then the live test that compares it with the trees). It agrees with
//! the trees on every function and operator a role wrote, and it is useless for
//! this question all the same: the engine records no dependency on the objects
//! initdb pinned, so no built-in function or operator a CHECK or a DEFAULT
//! calls (`length`, `>`, `now()`, `int8(integer)`, `query_to_xml`) is in it.
//! The gate has to know exactly which built-ins are called, to apply the list in
//! (a), so it reads the stored node trees (`pg_constraint.conbin`,
//! `pg_type.typdefaultbin`), which bind functions and operators by OID and carry
//! every occurrence. The decoder is the resolver's
//! ([`crate::resolver::capture`]): it refuses a node or a field it does not
//! know, and a tree it refuses leaves the type [`Unreadable`], never admitted.
//!
//! # What is not covered
//!
//! A built-in can pick code at run time from the *types* it is given: a
//! comparison of composites or arrays calls the comparison of each attribute's
//! or element's type, and `GREATEST` and `IN` do the same, an I/O coercion calls
//! the input and output functions of its types, a subscript calls the type's
//! subscript function. For a type the engine did not make, those are the support
//! functions and operator classes of a base type, which only a superuser can
//! create, so this gate treats them as trusted code and checks the owner (b) of
//! every type on the path. Declared input, output and subscript handlers still
//! require owner trust, including built-ins. Dynamic operator-class selection
//! keeps the superuser-created support-code contract.
//!
//! # Re-audit per major
//!
//! [`RUNS_SQL`] and the node decoder are audited per supported major (16 and 18
//! at the time of writing). A function in the list that the server does not have
//! leaves every type unreadable instead of silently shortening the list, and the
//! decoder refuses a major it was not measured on. Supporting a new major
//! therefore means re-auditing the list against its built-in function set and
//! re-measuring the trees; there is no catalog snapshot to keep in step.
//!
//! # One read
//!
//! Like [`crate::user_types`], a read is two passes that must agree, in a
//! transaction of its own or under a savepoint of the caller's, with an empty
//! `search_path` of its own. A pass that differs from the one before it is
//! [`Unreadable::Failed`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use pbps_db::{Conn, DbError, Param};
use pbps_model::TypeRef;

use crate::catalog::untrusted_role_walk;
use crate::pins::Within;
use crate::resolver::capture::{Bound, bound_objects};
use crate::resolver::scratch::FIRST_NORMAL_OBJECT_ID;
use crate::user_types::{OWN_TRANSACTION, Oid, UserType, need, oid_array};

/// The deepest chain of types read, and the most types read for one type. The
/// engine refuses a type that contains itself, so these bound only an absurd
/// catalog, which is refused as unreadable instead of walked forever.
const MAX_DEPTH: usize = 64;
const MAX_TYPES: usize = 10_000;

/// The built-in functions that run something the expression does not name,
/// found by signature (resolved with `to_regprocedure` on the server, never by a
/// raw OID, which differs between majors and clusters).
///
/// Every one is built in, so (a) alone would admit it. Each takes SQL text, a
/// table, a cursor or the name of a type, and runs it as the caller: the SQL can
/// call any function and read any table, and a domain named by `pg_input_*` runs
/// its CHECKs, so the code that runs is no longer the code the expression names.
///
/// - `query_to_xml`, `cursor_to_xml`, `table_to_xml`, `schema_to_xml` and
///   `database_to_xml`, each with `…schema` and `…_and_xmlschema` forms, run a
///   query, fetch a cursor or select from a table (a view, a table with row
///   security);
/// - `ts_stat` runs the query it is given, and so does `ts_rewrite(tsquery,
///   text)`;
/// - `pg_input_is_valid` and `pg_input_error_info` call the input function of a
///   type named by a text argument, which for a domain runs its CHECKs
///   (measured on 16 and 18).
///
/// Re-audit against the built-in function set of every new major before adding
/// it to the supported ones (see the module's "Re-audit per major"). The
/// 3-argument `ts_rewrite` is not listed: it rewrites with its arguments and
/// runs no SQL.
pub const RUNS_SQL: &[&str] = &[
    "pg_catalog.query_to_xml(text,boolean,boolean,text)",
    "pg_catalog.query_to_xmlschema(text,boolean,boolean,text)",
    "pg_catalog.query_to_xml_and_xmlschema(text,boolean,boolean,text)",
    "pg_catalog.cursor_to_xml(refcursor,integer,boolean,boolean,text)",
    "pg_catalog.cursor_to_xmlschema(refcursor,boolean,boolean,text)",
    "pg_catalog.table_to_xml(regclass,boolean,boolean,text)",
    "pg_catalog.table_to_xmlschema(regclass,boolean,boolean,text)",
    "pg_catalog.table_to_xml_and_xmlschema(regclass,boolean,boolean,text)",
    "pg_catalog.schema_to_xml(name,boolean,boolean,text)",
    "pg_catalog.schema_to_xmlschema(name,boolean,boolean,text)",
    "pg_catalog.schema_to_xml_and_xmlschema(name,boolean,boolean,text)",
    "pg_catalog.database_to_xml(boolean,boolean,text)",
    "pg_catalog.database_to_xmlschema(boolean,boolean,text)",
    "pg_catalog.database_to_xml_and_xmlschema(boolean,boolean,text)",
    "pg_catalog.ts_stat(text)",
    "pg_catalog.ts_stat(text,text)",
    "pg_catalog.ts_rewrite(tsquery,text)",
    "pg_catalog.pg_input_is_valid(text,text)",
    "pg_catalog.pg_input_error_info(text,text)",
];

/// What most refusals list at most before saying how many more there are.
const SHOWN: usize = 8;

// -- the result ---------------------------------------------------------------

/// The answer for one type. A failed read is [`InputPath::Unreadable`] and never
/// [`InputPath::Admitted`]: the only way to hold an [`Admitted`] is to be handed
/// one by [`read`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "an unreadable or refused input path must not be taken for an admitted one"]
pub enum InputPath {
    /// Nothing the type's input path runs is anyone's code but a trusted role's
    /// or the engine's.
    Admitted(Admitted),
    /// Why it is not: at least one reason, each naming what and whose.
    Refused(Vec<Reason>),
    /// The read could not tell. Not the same as refused, and never admitted.
    Unreadable(Unreadable),
}

/// The proof that a type's input path was read and is admitted. Held privately
/// so that no caller can build one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    runs_code: bool,
}

impl Admitted {
    /// Whether the input path runs any code at all (a CHECK or a DEFAULT
    /// somewhere on it). `false` is an enum, or a composite or array whose
    /// leaves have neither. Owner trust is required in either case.
    #[must_use]
    pub fn runs_code(&self) -> bool {
        self.runs_code
    }
}

/// Why a type's input path is not admitted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// A function or operator that is not built-in, SQL-free code.
    Routine(RoutineReason),
    /// A type owned by a role that is not trusted, or that an untrusted role can
    /// act as.
    Owner(OwnerReason),
    /// An untrusted owner or actor can change an input-path routine.
    RoutineOwner(RoutineOwnerReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RoutineKind {
    Function,
    Operator,
}

impl RoutineKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Operator => "operator",
        }
    }
}

/// Why a routine is not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Why {
    /// A function a role wrote (OID at or above 16384).
    NotBuiltin,
    /// A built-in on [`RUNS_SQL`].
    RunsSql,
}

/// Where in a type the expression sits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Part {
    /// A CHECK constraint of a domain, by name.
    Check {
        constraint: pbps_model::ObjectLocalName,
    },
    /// The declared domain's own DEFAULT.
    Default,
    /// The expression a recorded conversion retypes a column with (#2331).
    Conversion,
}

/// The type and the part of it an expression is in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Use {
    /// The engine's identity for the type: `app.positive`.
    pub ty: String,
    pub part: Part,
}

impl fmt::Display for Use {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.part {
            Part::Check { constraint } => {
                write!(
                    f,
                    "the CHECK `{constraint}` of `{}`",
                    self.ty,
                    constraint = constraint.shown()
                )
            }
            Part::Default => write!(f, "the DEFAULT of `{}`", self.ty),
            Part::Conversion => write!(f, "the conversion of {}", self.ty),
        }
    }
}

/// A function or operator the input path calls that is not admitted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RoutineReason {
    pub kind: RoutineKind,
    /// The engine's identity, which for a function holds its argument types:
    /// `app.is_positive(integer)`.
    pub identity: String,
    /// For an operator, the identity of the function it runs and that is not
    /// admitted. `None` for a function.
    pub runs: Option<String>,
    /// The role that owns the routine (an operator's own owner).
    pub owner: pbps_model::RoleName,
    pub why: Why,
    pub used_by: Use,
    /// The implementation function, including operator support functions.
    pub oid: Oid,
}

/// A type on the input path that a role which is not trusted can change.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct OwnerReason {
    /// The engine's identity for the type: `app.positive`.
    pub ty: String,
    /// The type's owner.
    pub owner: pbps_model::RoleName,
    /// An untrusted role that can act as `owner`: `owner` itself when the owner
    /// is the untrusted role, a member of it otherwise.
    pub actor: pbps_model::RoleName,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RoutineOwnerReason {
    pub kind: RoutineKind,
    pub identity: String,
    pub owner: pbps_model::RoleName,
    pub actor: pbps_model::RoleName,
}

/// Why the read could not tell. Each is a different thing to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreadable {
    /// A role declared trusted does not exist. Not an empty grant: a name that
    /// trusts nobody is a typo, and a typo that is silently ignored is a gate
    /// nobody reads.
    TrustedRolesMissing(Vec<pbps_model::RoleName>),
    /// The read failed, or the engine holds something this release cannot
    /// certify (an expression node it does not know, a built-in function the
    /// audited list names and the server lacks). What it found is unknown.
    Failed(String),
}

impl InputPath {
    /// Whether the input path may be run by `ADD COLUMN`.
    #[must_use]
    pub fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted(_))
    }

    /// Why this potential input path requires refusal, regardless of row presence,
    /// or `None` when it can. Each refusal says which case it is.
    #[must_use]
    pub fn refusal(&self, reference: &TypeRef) -> Option<String> {
        let name = reference.shown().to_string();
        Some(match self {
            Self::Admitted(_) => return None,
            Self::Refused(reasons) => {
                let mut lines: Vec<String> = reasons
                    .iter()
                    .take(SHOWN)
                    .map(|reason| reason.to_string())
                    .collect();
                if reasons.len() > SHOWN {
                    lines.push(format!("and {} more", reasons.len() - SHOWN));
                }
                format!(
                    "the potential input path of type `{name}` requires eligible code and trusted \
                     owners regardless of row presence: {}. To admit an untrusted owner, \
                     list the named role in `trusted_roles` in `pbps.yml`; user-written \
                     routines require pin_type_routines: true, source review and pins",
                    lines.join("; ")
                )
            }
            Self::Unreadable(Unreadable::TrustedRolesMissing(roles)) => format!(
                "the type `{name}` cannot be checked: the role(s) {} declared trusted do not \
                 exist",
                quoted(roles)
            ),
            Self::Unreadable(Unreadable::Failed(why)) => format!(
                "the code the type `{name}` runs could not be determined ({why}); \
                 this is not a type that runs no code"
            ),
        })
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Routine(r) => {
                let what = match r.why {
                    Why::NotBuiltin => "is not built into PostgreSQL",
                    Why::RunsSql => "runs SQL or code named by its arguments",
                };
                match &r.runs {
                    None => write!(
                        f,
                        "{} calls the {} `{}` (owned by `{}`), which {what}",
                        r.used_by,
                        r.kind.as_str(),
                        r.identity,
                        r.owner.shown()
                    ),
                    Some(function) => write!(
                        f,
                        "{} uses the {} `{}` (owned by `{}`), which runs the function \
                         `{function}`, which {what}",
                        r.used_by,
                        r.kind.as_str(),
                        r.identity,
                        r.owner.shown()
                    ),
                }
            }
            Self::Owner(o) if o.owner == o.actor => write!(
                f,
                "the type `{}` is owned by `{}`, a role that is neither a superuser, nor able to \
                 become the deploying role or a superuser, nor declared trusted, and so it can \
                 change what that type runs",
                o.ty,
                o.owner.shown()
            ),
            Self::Owner(o) => write!(
                f,
                "the type `{}` is owned by `{}`, and `{}`, a role that is neither a superuser, \
                 nor able to become the deploying role or a superuser, nor declared trusted, can \
                 act as it (by membership, or by granting it to itself) and so change what that \
                 type runs",
                o.ty,
                o.owner.shown(),
                o.actor.shown()
            ),
            Self::RoutineOwner(o) => write!(
                f,
                "the type input {} `{}` is owned by `{}`, and untrusted role `{}` can act as that owner; pinning does not replace owner trust",
                o.kind.as_str(),
                o.identity,
                o.owner.shown(),
                o.actor.shown()
            ),
        }
    }
}

fn quoted(names: &[pbps_model::RoleName]) -> String {
    names
        .iter()
        .map(|n| format!("`{n}`", n = n.shown(),))
        .collect::<Vec<_>>()
        .join(", ")
}

// -- the classification (pure) -----------------------------------------------

/// Whether the engine made an object, by its OID: initdb creates every object
/// below `FirstNormalObjectId`, and no role can create one there. By OID and not
/// by schema, since a superuser can create a function in `pg_catalog` that has
/// an OID above it (#2061).
#[must_use]
fn is_builtin(oid: Oid) -> bool {
    oid < Oid::from(FIRST_NORMAL_OBJECT_ID)
}

/// Why a function is not admitted, or `None` when it is built-in, SQL-free code.
#[must_use]
fn why_not_admitted(oid: Oid, runs_sql: &BTreeSet<Oid>) -> Option<Why> {
    if runs_sql.contains(&oid) {
        Some(Why::RunsSql)
    } else if !is_builtin(oid) {
        Some(Why::NotBuiltin)
    } else {
        None
    }
}

/// What the gate read of one type, before it is judged.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Facts {
    /// Some domain on the path has a CHECK or a DEFAULT.
    runs_code: bool,
    /// Includes built-ins: rollback does not undo every non-immutable call.
    non_immutable: bool,
    /// The routines that are not admitted, with where each is used.
    routines: Vec<RoutineReason>,
    /// All structural types, even when the fill evaluates no code in them.
    types: Vec<OwnedType>,
    /// Includes built-ins and operator implementations, before code eligibility.
    routine_owners: Vec<OwnedRoutine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedType {
    oid: Oid,
    ty: String,
    owner: pbps_model::RoleName,
    owner_oid: Oid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedRoutine {
    kind: RoutineKind,
    identity: String,
    owner: pbps_model::RoleName,
    owner_oid: Oid,
}

/// The verdict on what was read. `actors` maps an owner to the untrusted roles
/// that can act as it. Key by OID so a rename cannot hide an untrusted owner.
fn verdict(facts: Facts, actors: &BTreeMap<Oid, Vec<pbps_model::RoleName>>) -> InputPath {
    let mut reasons: Vec<Reason> = facts.routines.into_iter().map(Reason::Routine).collect();
    for t in &facts.types {
        for actor in actors.get(&t.owner_oid).into_iter().flatten() {
            reasons.push(Reason::Owner(OwnerReason {
                ty: t.ty.clone(),
                owner: t.owner.clone(),
                actor: actor.clone(),
            }));
        }
    }
    for routine in facts.routine_owners {
        for actor in actors.get(&routine.owner_oid).into_iter().flatten() {
            reasons.push(Reason::RoutineOwner(RoutineOwnerReason {
                kind: routine.kind,
                identity: routine.identity.clone(),
                owner: routine.owner.clone(),
                actor: actor.clone(),
            }));
        }
    }
    reasons.sort();
    reasons.dedup();
    if reasons.is_empty() {
        InputPath::Admitted(Admitted {
            runs_code: facts.runs_code,
        })
    } else {
        InputPath::Refused(reasons)
    }
}

/// The read both passes agree on, or a failed read when they do not.
fn settled(first: InputPath, second: InputPath) -> InputPath {
    if first == second {
        first
    } else {
        InputPath::Unreadable(Unreadable::Failed(
            "the type changed while it was being read".to_owned(),
        ))
    }
}

/// A column to add, as far as the gate is concerned.
#[derive(Debug, Clone, Copy)]
pub struct Added<'a> {
    ty: &'a UserType,
    own_default: bool,
    fill: Fill,
    array: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fill {
    Null,
    EmptyArray,
    StatementDefault,
    TypeDefault,
}

impl<'a> Added<'a> {
    /// `own_default` is whether the `ADD COLUMN` statement carries a DEFAULT of
    /// its own. The engine then uses it and never the domain's, so the domain's
    /// DEFAULT is not code the statement runs; its CHECKs still run on the
    /// value. The statement's own default is the plan's expression and not part
    /// of the type's input path.
    #[must_use]
    pub fn new(ty: &'a UserType, own_default: bool) -> Self {
        let fill = if own_default {
            Fill::StatementDefault
        } else if matches!(&ty.definition().body, crate::user_types::Body::Domain(d) if d.default.is_some())
        {
            Fill::TypeDefault
        } else {
            Fill::Null
        };
        Self {
            ty,
            own_default,
            fill,
            array: false,
        }
    }

    /// An explicit NULL default still shadows the type default, but constructs
    /// no attributes or elements (measured on 16 and 18).
    #[must_use]
    pub fn null_fill(mut self) -> Self {
        self.fill = Fill::Null;
        self
    }

    /// A proven-empty array coerces no elements (measured on 16 and 18).
    /// Retain all structural owner checks while skipping element code.
    #[must_use]
    pub fn empty_array_fill(mut self) -> Self {
        self.fill = Fill::EmptyArray;
        self
    }

    /// An array's NULL fill does not coerce an element or use its DEFAULT.
    #[must_use]
    pub fn array(mut self, array: bool) -> Self {
        self.array = array;
        if array && self.fill == Fill::TypeDefault {
            self.fill = Fill::Null;
        }
        self
    }
}

// -- the read -----------------------------------------------------------------

/// Reads whether `ADD COLUMN` over existing rows may run each column's type's
/// input path, one answer per column, in order.
///
/// `trusted` are the roles a human has declared trusted, over and above the
/// engine's rule (a superuser, or a role that can `SET ROLE` to the deploying
/// role or to a superuser). It extends (b) only: it never admits a function.
/// A name that is not a role is [`Unreadable::TrustedRolesMissing`] for every
/// type, however many there are; [`missing_roles`] asks the same question alone.
///
/// In a transaction of its own that is one snapshot of the catalog tables; in
/// the caller's it is the caller's. A read that fails is
/// [`Unreadable::Failed`] for every type: nothing was learned, and nothing is
/// reported as admitted.
pub async fn read(
    conn: &mut Conn,
    within: Within,
    trusted: &[pbps_model::RoleName],
    added: &[Added<'_>],
) -> Vec<InputPath> {
    match read_all(conn, within, trusted, added).await {
        Ok(reads) => reads,
        Err(e) => added
            .iter()
            .map(|_| InputPath::Unreadable(Unreadable::Failed(e.to_string())))
            .collect(),
    }
}

/// The names in `roles` that are not roles of this cluster, sorted. A role list
/// that cannot be read is an error and never an empty answer.
pub async fn missing_roles(
    conn: &mut Conn,
    roles: &[pbps_model::RoleName],
) -> Result<Vec<pbps_model::RoleName>, DbError> {
    let json = json(roles)?;
    let rows = conn
        .query_with(
            "SELECT DISTINCT e.name AS name
               FROM pg_catalog.jsonb_array_elements_text($1::pg_catalog.text::pg_catalog.jsonb)
                      AS e(name)
              WHERE NOT EXISTS (SELECT FROM pg_catalog.pg_roles r
                                 WHERE r.rolname::pg_catalog.text OPERATOR(pg_catalog.=) e.name)
              ORDER BY 1",
            &[Param::Str(&json)],
        )
        .await?;
    rows.iter()
        .map(|row| Ok(need::<&str>(row, "name")?.into()))
        .collect()
}

fn json<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, DbError> {
    serde_json::to_string(value)
        .map_err(|e| DbError::BadRow(format!("a list could not be sent as JSON: {e}")))
}

const SAVEPOINT: &str = "pbps_input_path";

async fn read_all(
    conn: &mut Conn,
    within: Within,
    trusted: &[pbps_model::RoleName],
    added: &[Added<'_>],
) -> Result<Vec<InputPath>, DbError> {
    match within {
        Within::OwnTransaction => {
            conn.begin(&OWN_TRANSACTION).await?;
            let result = read_each(conn, trusted, added).await;
            let closed = conn.rollback(&OWN_TRANSACTION).await;
            let reads = result?;
            closed?;
            Ok(reads)
        }
        Within::CallersTransaction => {
            conn.execute(&format!("SAVEPOINT {SAVEPOINT}")).await?;
            match read_each(conn, trusted, added).await {
                Ok(reads) => {
                    conn.execute(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))
                        .await?;
                    conn.execute(&format!("RELEASE SAVEPOINT {SAVEPOINT}"))
                        .await?;
                    Ok(reads)
                }
                Err(e) => {
                    // Leave the caller's transaction usable if it can be; the
                    // read's own error is the one reported.
                    let _ = conn
                        .execute(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))
                        .await;
                    let _ = conn
                        .execute(&format!("RELEASE SAVEPOINT {SAVEPOINT}"))
                        .await;
                    Err(e)
                }
            }
        }
    }
}

async fn read_each(
    conn: &mut Conn,
    trusted: &[pbps_model::RoleName],
    added: &[Added<'_>],
) -> Result<Vec<InputPath>, DbError> {
    // Names in the stored expressions' text, in the identities and in the
    // deparsed output are qualified unless the path shows them.
    conn.query("SELECT pg_catalog.set_config('search_path', '', true) AS was_set")
        .await?;
    // Read the whole set twice and compare (see `user_types`): the queries of a
    // pass are separate statements and in a READ COMMITTED transaction share no
    // snapshot.
    let first = pass(conn, trusted, added).await?;
    let second = pass(conn, trusted, added).await?;
    Ok(first
        .into_iter()
        .zip(second)
        .map(|(first, second)| settled(first, second))
        .collect())
}

/// A pre-approval probe may not execute non-immutable input-path code, even
/// built-ins admitted by the deployment gate. Read facts twice as the gate
/// does; this is catalog inspection, never evaluation of the expressions.
pub(crate) async fn probes_are_immutable(
    conn: &mut Conn,
    types: &[UserType],
) -> Result<bool, DbError> {
    if types.is_empty() {
        return Ok(true);
    }
    conn.begin(&OWN_TRANSACTION).await?;
    let result = async {
        conn.query("SELECT pg_catalog.set_config('search_path', '', true) AS was_set")
            .await?;
        let mut passes = Vec::new();
        for _ in 0..2 {
            let major = server_major(conn).await?;
            let denied = runs_sql_oids(conn).await?.map_err(DbError::BadRow)?;
            let mut facts = Vec::new();
            for ty in types {
                for own_default in [true, false] {
                    facts.push(
                        gather(conn, major, &denied, Added::new(ty, own_default))
                            .await
                            .map_err(|e| match e {
                                Failure::Read(e) => e,
                                Failure::Uncertified(why) => DbError::BadRow(why),
                            })?,
                    );
                }
            }
            passes.push(facts);
        }
        if passes[0] != passes[1] {
            return Err(DbError::BadRow(
                "the input path changed while its probe eligibility was being read".to_owned(),
            ));
        }
        Ok(passes[0].iter().all(|f| !f.non_immutable))
    }
    .await;
    let closed = conn.rollback(&OWN_TRANSACTION).await;
    let immutable = result?;
    closed?;
    Ok(immutable)
}

/// Why one type cannot be certified, or that the whole read failed.
enum Failure {
    /// The engine or the connection failed: no type was learned.
    Read(DbError),
    /// This type holds something the read cannot certify; the others are fine.
    Uncertified(String),
}

impl From<DbError> for Failure {
    fn from(e: DbError) -> Self {
        Self::Read(e)
    }
}

async fn pass(
    conn: &mut Conn,
    trusted: &[pbps_model::RoleName],
    added: &[Added<'_>],
) -> Result<Vec<InputPath>, DbError> {
    let missing = missing_roles(conn, trusted).await?;
    if !missing.is_empty() {
        return Ok(added
            .iter()
            .map(|_| InputPath::Unreadable(Unreadable::TrustedRolesMissing(missing.clone())))
            .collect());
    }
    let major = server_major(conn).await?;
    let denied = match runs_sql_oids(conn).await? {
        Ok(denied) => denied,
        Err(why) => {
            return Ok(added
                .iter()
                .map(|_| InputPath::Unreadable(Unreadable::Failed(why.clone())))
                .collect());
        }
    };
    let mut gathered: Vec<Result<Facts, String>> = Vec::with_capacity(added.len());
    for column in added {
        gathered.push(match gather(conn, major, &denied, *column).await {
            Ok(facts) => Ok(facts),
            Err(Failure::Read(e)) => return Err(e),
            Err(Failure::Uncertified(why)) => Err(why),
        });
    }
    // Who can act as the owner of each type that matters: one walk of the
    // cluster's roles for all of them.
    let wanted: BTreeSet<Oid> = gathered
        .iter()
        .flatten()
        .flat_map(|f| {
            f.types
                .iter()
                .map(|t| t.owner_oid)
                .chain(f.routine_owners.iter().map(|r| r.owner_oid))
        })
        .collect();
    let actors = untrusted_actors(conn, trusted, &wanted).await?;
    Ok(gathered
        .into_iter()
        .map(|facts| match facts {
            Ok(facts) => verdict(facts, &actors),
            Err(why) => InputPath::Unreadable(Unreadable::Failed(why)),
        })
        .collect())
}

/// Binds a recorded conversion's expression, and judges what it bound, before
/// anything evaluates it (#2331, DEC-2331.1).
///
/// The expression runs as the deploying role, in the pre-flight over every row
/// and then in the `ALTER`, so non-built-in code in it would run before anyone
/// had judged it. It is bound first against a temporary table holding the one
/// column, wrapped as `CHECK (((<using>) IS NULL) OR true)`: the engine parses
/// and resolves it as it will in the `ALTER`, keeps the whole tree unsimplified
/// in `conbin` (measured on 16 and 18: the user function's `funcid` stays), and
/// refuses any name but the column's own -- a second column is `column "d" does
/// not exist`, a subquery `cannot use subquery in check constraint`. Then the
/// tree goes through the decoder and the judgement of DEC-2181.1: no function,
/// operator or cast that a role wrote, none on [`RUNS_SQL`], no user-defined or
/// domain type in it.
///
/// In a transaction of its own that is rolled back, so the table never
/// outlives this call. `Err` is the refusal, in words, with no server text: it
/// can quote a value.
pub async fn vet_conversion(
    conn: &mut Conn,
    column: &pbps_model::ColumnRef,
    from: &pbps_model::ColumnType,
    using: &str,
) -> Result<(), String> {
    let shown = column.shown().to_string();
    let shown = shown.as_str();
    let ty = crate::types::normalize(from)
        .map_err(|e| format!("the conversion of {shown} cannot be vetted: {e}"))?
        .raw()
        .to_string();
    let quoted = crate::quote(column.name.raw())
        .map_err(|e| format!("the conversion of {shown} cannot be vetted: {e}"))?;
    let (column, ty) = (quoted.as_str(), ty.as_str());
    const WRITABLE: pbps_dialect::TransactionFraming = pbps_dialect::TransactionFraming {
        begin: std::borrow::Cow::Borrowed("BEGIN ISOLATION LEVEL REPEATABLE READ;"),
        commit: std::borrow::Cow::Borrowed("COMMIT;"),
        rollback: std::borrow::Cow::Borrowed("ROLLBACK;"),
    };
    let unvetted = |why: String| format!("the conversion `USING {using}` of {shown} {why}");
    conn.begin(&WRITABLE)
        .await
        .map_err(|_| unvetted("could not be vetted: no transaction to bind it in".into()))?;
    let result = vet_in_transaction(conn, column, ty, using, shown).await;
    let closed = conn.rollback(&WRITABLE).await;
    result?;
    closed.map_err(|_| unvetted("could not be vetted: its transaction did not close".into()))
}

async fn vet_in_transaction(
    conn: &mut Conn,
    column: &str,
    ty: &str,
    using: &str,
    shown: &str,
) -> Result<(), String> {
    let refuse = |why: &str| format!("the conversion `USING {using}` of {shown} {why}");
    // The write path the statement runs under is the session's own, set by the
    // apply's pins, so a name binds here as it will there.
    //
    // **One statement, by the protocol and not by reading the text** (round 3
    // review). `execute` is a simple-protocol batch, and text such as
    // `NULL) IS NULL) OR true)); COMMIT; SELECT f(); --` closes this statement
    // and runs the rest before the vet has judged anything. `execute_with` goes
    // through the extended protocol, where the server refuses a string of more
    // than one command (SQLSTATE 42601, "cannot insert multiple commands into a
    // prepared statement"), so the same text is refused as an expression that
    // does not bind. No scanner for `;` or comments is written (DEC-1635.4):
    // they are the expression's own to contain, in a string literal say.
    if let Err(e) = conn
        .execute_with(
            &format!(
                "CREATE TEMPORARY TABLE pg_temp.pbps_conversion ({column} {ty}, \
                 CONSTRAINT pbps_conversion CHECK ((({using}) IS NULL) OR true)) ON COMMIT DROP"
            ),
            &[],
        )
        .await
    {
        let code = e
            .server_error_code()
            .unwrap_or_else(|| "unknown".to_owned());
        return Err(refuse(&format!(
            "does not bind against the column alone (SQLSTATE {code}): it may use only the \
             column itself, no other column, subquery or object the engine cannot resolve there"
        )));
    }
    // One statement can still be closed into more than one clause of the
    // `CREATE TABLE`: `1) IS NULL) OR true), d integer DEFAULT (((1` adds a
    // column. Nothing there is evaluated at creation (the table has no row, and
    // a default, a check and a generated expression run on one), but the table
    // is then not the one column and one check this vet reads the tree of, so
    // it is refused here, before the tree is read, as an expression that is
    // more than one.
    let shape = conn
        .query(
            "SELECT
               (SELECT count(*) FROM pg_catalog.pg_attribute a
                 WHERE a.attrelid OPERATOR(pg_catalog.=) r.oid
                   AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped)::pg_catalog.int4 AS columns,
               (SELECT count(*) FROM pg_catalog.pg_constraint c
                 WHERE c.conrelid OPERATOR(pg_catalog.=) r.oid)::pg_catalog.int4 AS constraints,
               (SELECT count(*) FROM pg_catalog.pg_attrdef d
                 WHERE d.adrelid OPERATOR(pg_catalog.=) r.oid)::pg_catalog.int4 AS defaults,
               (SELECT count(*) FROM pg_catalog.pg_inherits i
                 WHERE i.inhrelid OPERATOR(pg_catalog.=) r.oid
                    OR i.inhparent OPERATOR(pg_catalog.=) r.oid)::pg_catalog.int4 AS inherits,
               (SELECT count(*) FROM pg_catalog.pg_index x
                 WHERE x.indrelid OPERATOR(pg_catalog.=) r.oid)::pg_catalog.int4 AS indexes,
               (r.relkind OPERATOR(pg_catalog.=) 'r'
                 AND r.relpersistence OPERATOR(pg_catalog.=) 't')::pg_catalog.int4 AS plain
               FROM pg_catalog.pg_class r
              WHERE r.oid OPERATOR(pg_catalog.=) 'pg_temp.pbps_conversion'::pg_catalog.regclass",
        )
        .await
        .map_err(|_| refuse("could not be vetted: the table it was bound in could not be read"))?;
    let counts: Option<Vec<i32>> = shape.first().and_then(|row| {
        [
            "columns",
            "constraints",
            "defaults",
            "inherits",
            "indexes",
            "plain",
        ]
        .iter()
        .map(|name| row.try_get::<i32>(name).ok().flatten())
        .collect()
    });
    if counts.as_deref() != Some(&[1, 1, 0, 0, 0, 1]) {
        return Err(refuse(
            "is more than one expression: it closes the check it is bound in and adds to the \
             statement, and it may be the one expression and nothing else",
        ));
    }
    let rows = conn
        .query(
            "SELECT c.conbin::pg_catalog.text AS expr
               FROM pg_catalog.pg_constraint c
              WHERE c.conrelid OPERATOR(pg_catalog.=) 'pg_temp.pbps_conversion'::pg_catalog.regclass
                AND c.conname OPERATOR(pg_catalog.=) 'pbps_conversion'",
        )
        .await
        .map_err(|_| refuse("could not be vetted: its bound form could not be read"))?;
    let text = rows
        .first()
        .and_then(|row| row.try_get::<&str>("expr").ok().flatten())
        .ok_or_else(|| refuse("could not be vetted: its bound form was not stored"))?
        .to_owned();
    let major = server_major(conn)
        .await
        .map_err(|_| refuse("could not be vetted: the server version is unreadable"))?;
    let denied = runs_sql_oids(conn)
        .await
        .map_err(|_| refuse("could not be vetted: the audited list could not be read"))?
        .map_err(|why| refuse(&format!("could not be vetted: {why}")))?;
    let bound = bound_objects(&text, major)
        .map_err(|why| refuse(&format!("could not be vetted: {why}")))?;
    // Built-in types only: a user-defined or domain type in the tree runs its
    // own input path (v1 scope).
    if bound.domain_coercions.iter().any(|_| true)
        || bound
            .types
            .iter()
            .chain(&bound.input_coercions)
            .any(|oid| !is_builtin(Oid::from(*oid)))
    {
        return Err(refuse(
            "names a type that is not built into PostgreSQL, or coerces to a domain, which \
             runs code of its own",
        ));
    }
    let used_by = Use {
        ty: shown.to_owned(),
        part: Part::Conversion,
    };
    let functions: BTreeMap<Oid, Vec<Use>> = bound
        .routines
        .iter()
        .map(|f| (Oid::from(*f), vec![used_by.clone()]))
        .collect();
    let operators: BTreeMap<Oid, Vec<Use>> = bound
        .operators
        .iter()
        .map(|o| (Oid::from(*o), vec![used_by.clone()]))
        .collect();
    let row_compares: BTreeMap<(Oid, Oid), Vec<Use>> = bound
        .row_compares
        .iter()
        .map(|(o, f)| ((Oid::from(*o), Oid::from(*f)), vec![used_by.clone()]))
        .collect();
    let (reasons, _, _) = unadmitted_routines(conn, &denied, &functions, &operators, &row_compares)
        .await
        .map_err(|failure| match failure {
            Failure::Read(_) => refuse("could not be vetted: the catalog could not be read"),
            Failure::Uncertified(why) => refuse(&format!("could not be vetted: {why}")),
        })?;
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{}; nothing has been changed",
            reasons
                .iter()
                .map(|r| Reason::Routine(r.clone()).to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ))
    }
}

/// `server_version_num` / 10000: 16 or 18, the majors the decoder was measured
/// on. Any other is refused later, by the decoder.
async fn server_major(conn: &mut Conn) -> Result<u32, DbError> {
    let rows = conn
        .query(
            "SELECT pg_catalog.current_setting('server_version_num')::pg_catalog.int4 AS version",
        )
        .await?;
    let version: i32 = rows
        .first()
        .map(|row| need(row, "version"))
        .transpose()?
        .ok_or_else(|| DbError::BadRow("`server_version_num` returned no row".to_owned()))?;
    u32::try_from(version / 10_000)
        .map_err(|_| DbError::BadRow(format!("`server_version_num` is {version}")))
}

/// The OIDs of [`RUNS_SQL`] on this server. A function the server does not have
/// is not a shorter list but a list that was not audited for this release, and
/// it leaves the answer unreadable.
async fn runs_sql_oids(conn: &mut Conn) -> Result<Result<BTreeSet<Oid>, String>, DbError> {
    let json = json(RUNS_SQL)?;
    let rows = conn
        .query_with(
            "SELECT e.sig AS sig,
                    pg_catalog.to_regprocedure(e.sig)::pg_catalog.oid::pg_catalog.int8 AS oid
               FROM pg_catalog.jsonb_array_elements_text($1::pg_catalog.text::pg_catalog.jsonb)
                      AS e(sig)",
            &[Param::Str(&json)],
        )
        .await?;
    let mut resolved = Vec::with_capacity(rows.len());
    for row in &rows {
        resolved.push((
            need::<&str>(row, "sig")?.to_owned(),
            row.try_get::<i64>("oid")?,
        ));
    }
    Ok(resolve_runs_sql(&resolved))
}

/// [`RUNS_SQL`] as the server resolved it: each signature with its OID, or none.
/// A signature the server does not have, or has as something a role wrote, is
/// not a shorter list but a list that does not describe this server.
fn resolve_runs_sql(resolved: &[(String, Option<Oid>)]) -> Result<BTreeSet<Oid>, String> {
    let mut out = BTreeSet::new();
    let mut absent = Vec::new();
    for (sig, oid) in resolved {
        match oid {
            Some(oid) if is_builtin(*oid) => {
                out.insert(*oid);
            }
            Some(_) | None => absent.push(sig.as_str()),
        }
    }
    if absent.is_empty() {
        Ok(out)
    } else {
        Err(format!(
            "this server has no built-in {} of the audited list of functions that run SQL; the \
             list has to be re-audited for this release",
            absent.join(", ")
        ))
    }
}

// -- the types on the input path ----------------------------------------------

/// A row of `pg_type` for a type the path reaches.
#[derive(Debug, Clone)]
struct TypeRow {
    oid: Oid,
    /// The engine's identity: `app.positive`, `app.positive[]`, `integer`.
    name: String,
    typtype: String,
    owner: pbps_model::RoleName,
    owner_oid: Oid,
    /// A domain's DEFAULT as the stored node tree's text.
    default: Option<String>,
    handlers: Vec<Oid>,
}

/// What the input path of `root` reaches and what runs on it.
async fn gather(
    conn: &mut Conn,
    major: u32,
    runs_sql: &BTreeSet<Oid>,
    added: Added<'_>,
) -> Result<Facts, Failure> {
    let root = added.ty.oid();
    let own_default = added.own_default;
    let mut owners = BTreeMap::new();
    let mut handlers = BTreeSet::new();
    let mut types: BTreeMap<Oid, TypeRow> = BTreeMap::new();
    // Where each routine is used, by OID.
    let mut functions: BTreeMap<Oid, Vec<Use>> = BTreeMap::new();
    let mut operators: BTreeMap<Oid, Vec<Use>> = BTreeMap::new();
    // Operator and operator family of a row comparison.
    let mut row_compares: BTreeMap<(Oid, Oid), Vec<Use>> = BTreeMap::new();
    let mut runs_code = false;

    let mut roots = BTreeMap::from([(root, added.fill == Fill::StatementDefault)]);
    let mut constructed = BTreeSet::new();
    let mut rounds = 0;
    while !roots.is_empty() {
        rounds += 1;
        if rounds > MAX_DEPTH {
            return Err(Failure::Uncertified(format!(
                "types are nested more than {MAX_DEPTH} deep"
            )));
        }
        // Fill shape controls evaluation, never trust: a currently code-free
        // owner can add code between validation and DDL (DEC-2181.3).
        let structural = reach(conn, &roots.keys().copied().collect::<Vec<_>>(), true).await?;
        if let Some(gone) = roots
            .keys()
            .find(|r| !structural.iter().any(|t| t.oid == **r))
        {
            return Err(Failure::Read(DbError::BadRow(format!(
                "type {gone} disappeared while it was being read"
            ))));
        }
        for ty in &structural {
            handlers.extend(ty.handlers.iter().copied().filter(|oid| *oid != 0));
        }
        owners.extend(structural.into_iter().map(|t| {
            (
                t.oid,
                OwnedType {
                    oid: t.oid,
                    ty: t.name,
                    owner: t.owner,
                    owner_oid: t.owner_oid,
                },
            )
        }));
        if owners.len() > MAX_TYPES {
            return Err(Failure::Uncertified(format!(
                "a type is made of more than {MAX_TYPES} types"
            )));
        }
        if added.array && matches!(added.fill, Fill::Null | Fill::EmptyArray) {
            break;
        }
        let mut reached = BTreeMap::new();
        for constructs in [false, true] {
            let wanted: Vec<Oid> = roots
                .iter()
                .filter_map(|(oid, input)| (*input == constructs).then_some(*oid))
                .collect();
            if wanted.is_empty() {
                continue;
            }
            for ty in reach(conn, &wanted, constructs).await? {
                if constructs {
                    constructed.insert(ty.oid);
                }
                reached.insert(ty.oid, ty);
            }
        }
        // A type the walk was asked to start from and did not find has been
        // dropped under the read: a failed read, not an absence.
        if let Some(gone) = roots.keys().find(|r| !reached.contains_key(*r)) {
            return Err(Failure::Read(DbError::BadRow(format!(
                "type {gone} disappeared while it was being read"
            ))));
        }
        let fresh: Vec<TypeRow> = reached
            .into_values()
            .filter(|t| !types.contains_key(&t.oid))
            .collect();
        let mut bounds: Vec<(Use, Bound)> = Vec::new();
        let domains: Vec<&TypeRow> = fresh.iter().filter(|t| t.typtype == "d").collect();
        for domain in &domains {
            // Only the declared type's own DEFAULT is evaluated: `ADD COLUMN`
            // reads the column's type for one, and a domain it is made of, or
            // that an expression names, contributes its CHECKs and never its
            // DEFAULT (measured on 16 and 18). A domain created over another
            // takes a copy of that one's default at creation, so what the
            // declared domain would have inherited is already its own.
            //
            // Nor does it when the statement carries a DEFAULT of its own,
            // which the engine uses instead.
            if domain.oid != root || own_default {
                continue;
            }
            if let Some(text) = &domain.default {
                runs_code = true;
                let used_by = Use {
                    ty: domain.name.clone(),
                    part: Part::Default,
                };
                bounds.push((used_by.clone(), certify(text, major, &used_by)?));
            }
        }
        if !domains.is_empty() {
            let wanted: Vec<Oid> = domains.iter().map(|t| t.oid).collect();
            for (domain, constraint, text) in checks(conn, &wanted).await? {
                runs_code = true;
                let used_by = Use {
                    ty: domains
                        .iter()
                        .find(|t| t.oid == domain)
                        .map(|t| t.name.clone())
                        .unwrap_or_default(),
                    part: Part::Check { constraint },
                };
                bounds.push((used_by.clone(), certify(&text, major, &used_by)?));
            }
        }
        let mut next = BTreeMap::new();
        for (used_by, bound) in bounds {
            for f in &bound.routines {
                functions
                    .entry(Oid::from(*f))
                    .or_default()
                    .push(used_by.clone());
            }
            for o in &bound.operators {
                operators
                    .entry(Oid::from(*o))
                    .or_default()
                    .push(used_by.clone());
            }
            for (operator, family) in &bound.row_compares {
                row_compares
                    .entry((Oid::from(*operator), Oid::from(*family)))
                    .or_default()
                    .push(used_by.clone());
            }
            // A domain coercion checks the domain without reconstructing its
            // already typed attributes/elements. I/O input reconstructs them
            // too, even when this type was already reached without them.
            for ty in &bound.domain_coercions {
                let ty = Oid::from(*ty);
                if !types.contains_key(&ty) && !fresh.iter().any(|t| t.oid == ty) {
                    next.entry(ty).or_insert(false);
                }
            }
            for ty in &bound.input_coercions {
                let ty = Oid::from(*ty);
                if !constructed.contains(&ty) {
                    next.insert(ty, true);
                }
            }
        }
        types.extend(fresh.into_iter().map(|t| (t.oid, t)));
        if types.len() > MAX_TYPES {
            return Err(Failure::Uncertified(format!(
                "a type is made of more than {MAX_TYPES} types"
            )));
        }
        roots = next;
    }

    let (routines, non_immutable, mut routine_owners) =
        unadmitted_routines(conn, runs_sql, &functions, &operators, &row_compares).await?;
    let handler_rows = function_rows(conn, &handlers.iter().copied().collect::<Vec<_>>()).await?;
    for oid in handlers {
        let row = handler_rows.get(&oid).ok_or_else(|| {
            Failure::Uncertified(format!(
                "the type handler with OID {oid} is not in the catalog"
            ))
        })?;
        routine_owners.push(OwnedRoutine {
            kind: RoutineKind::Function,
            identity: row.identity.clone(),
            owner: row.owner.clone(),
            owner_oid: row.owner_oid,
        });
    }
    Ok(Facts {
        runs_code,
        non_immutable,
        routines,
        types: owners.into_values().collect(),
        routine_owners,
    })
}

/// A tree decoded, or the type's reason for not being certifiable.
fn certify(text: &str, major: u32, used_by: &Use) -> Result<Bound, Failure> {
    bound_objects(text, major).map_err(|why| Failure::Uncertified(format!("{used_by}: {why}")))
}

/// Every type reached from `roots` by what a type is made of, the roots
/// included where evaluated: a domain's base always, and a composite's
/// attributes, array's element or range's subtype for a constructing fill.
/// Built-in types keep their base and element edges. One statement, so one snapshot.
async fn reach(conn: &mut Conn, roots: &[Oid], constructs: bool) -> Result<Vec<TypeRow>, DbError> {
    let rows = conn
        .query(&format!(
            "WITH RECURSIVE reach(oid) AS (
                 SELECT u.o FROM pg_catalog.unnest({roots}) AS u(o)
               UNION
                 SELECT e.oid
                   FROM reach r
                   JOIN pg_catalog.pg_type t ON t.oid = r.oid
                  CROSS JOIN LATERAL (
                      SELECT t.typelem AS oid
                       WHERE {constructs} AND EXISTS (SELECT FROM pg_catalog.pg_type el
                                      WHERE el.oid = t.typelem AND el.typarray = t.oid)
                      UNION ALL
                      SELECT t.typbasetype WHERE t.typtype = 'd'
                      UNION ALL
                      SELECT a.atttypid FROM pg_catalog.pg_attribute a
                       WHERE {constructs} AND t.typtype = 'c' AND a.attrelid = t.typrelid
                         AND a.attnum > 0 AND NOT a.attisdropped
                      UNION ALL
                      SELECT g.rngsubtype FROM pg_catalog.pg_range g
                       WHERE {constructs} AND (g.rngtypid = t.oid OR g.rngmultitypid = t.oid)
                  ) e)
             SELECT t.oid::pg_catalog.int8 AS oid,
                    i.identity AS name,
                    t.typtype::pg_catalog.text AS typtype,
                    ow.rolname::pg_catalog.text AS owner,
                    ow.oid::pg_catalog.int8 AS owner_oid,
                    t.typdefaultbin::pg_catalog.text AS default_expr,
                    pg_catalog.json_build_array(t.typinput::pg_catalog.oid::pg_catalog.int8,
                        t.typoutput::pg_catalog.oid::pg_catalog.int8,
                        t.typsubscript::pg_catalog.oid::pg_catalog.int8)::pg_catalog.text AS handlers
               FROM reach r
               JOIN pg_catalog.pg_type t ON t.oid = r.oid
               JOIN pg_catalog.pg_roles ow ON ow.oid = t.typowner
              CROSS JOIN LATERAL pg_catalog.pg_identify_object(
                        'pg_catalog.pg_type'::pg_catalog.regclass, t.oid, 0) i
              ORDER BY t.oid
              LIMIT {limit}",
            roots = oid_array(roots),
            limit = MAX_TYPES + 1,
        ))
        .await?;
    if rows.len() > MAX_TYPES {
        return Err(DbError::BadRow(format!(
            "a type is made of more than {MAX_TYPES} types"
        )));
    }
    rows.iter()
        .map(|row| {
            let typtype = need::<&str>(row, "typtype")?.to_owned();
            Ok(TypeRow {
                oid: need(row, "oid")?,
                name: need::<&str>(row, "name")?.to_owned(),
                owner: need::<&str>(row, "owner")?.into(),
                owner_oid: need(row, "owner_oid")?,
                // Only a domain has a default of this kind.
                default: if typtype == "d" {
                    row.try_get::<&str>("default_expr")?.map(str::to_owned)
                } else {
                    None
                },
                typtype,
                handlers: serde_json::from_str(need::<&str>(row, "handlers")?).map_err(|e| {
                    DbError::BadRow(format!("the type handlers could not be read: {e}"))
                })?,
            })
        })
        .collect()
}

/// The CHECK constraints of the domains, as their stored node trees. Every
/// other kind of constraint is a failed read: `NOT NULL` (which 18 also keeps
/// as a row) runs nothing and is left out, and a kind this release does not
/// know could.
async fn checks(
    conn: &mut Conn,
    domains: &[Oid],
) -> Result<Vec<(Oid, pbps_model::ObjectLocalName, String)>, Failure> {
    let rows = conn
        .query(&format!(
            "SELECT c.contypid::pg_catalog.int8 AS dom,
                    c.conname::pg_catalog.text AS name,
                    c.contype::pg_catalog.text AS contype,
                    c.conbin::pg_catalog.text AS expr
               FROM pg_catalog.pg_constraint c
              WHERE c.contypid = ANY({}) AND c.contype <> 'n'
              ORDER BY 1, 2",
            oid_array(domains)
        ))
        .await?;
    let mut out = Vec::new();
    for row in &rows {
        let name = need::<&str>(row, "name")?;
        let text = check_text(
            name,
            need::<&str>(row, "contype")?,
            row.try_get::<&str>("expr")?,
        )
        .map_err(Failure::Uncertified)?;
        out.push((need(row, "dom")?, name.into(), text));
    }
    Ok(out)
}

/// The stored tree of a domain's constraint, if it is a CHECK that has one. Any
/// other kind of constraint, and a CHECK with no tree, cannot be certified, and
/// is not a CHECK that runs nothing.
fn check_text(name: &str, contype: &str, expr: Option<&str>) -> Result<String, String> {
    if contype != "c" {
        return Err(format!(
            "the constraint `{name}` of a domain is of kind `{contype}`, which this release does \
             not know"
        ));
    }
    expr.map(str::to_owned)
        .ok_or_else(|| format!("the CHECK `{name}` has no stored expression"))
}

// -- the routines -------------------------------------------------------------

struct OperatorRow {
    identity: String,
    owner: pbps_model::RoleName,
    owner_oid: Oid,
    /// The function the operator runs.
    code: Oid,
}

/// The function an operator runs. An operator with none (a shell) is not an
/// operator that runs nothing.
fn operator_function(oid: Oid, row: &OperatorRow) -> Result<Oid, String> {
    if row.code == 0 {
        Err(format!(
            "the operator `{}` (OID {oid}) has no function",
            row.identity
        ))
    } else {
        Ok(row.code)
    }
}

struct FunctionRow {
    identity: String,
    owner: pbps_model::RoleName,
    owner_oid: Oid,
    volatility: String,
}

/// The functions and operators of the path that are not admitted, with where
/// each is used, and whether any executed routine is non-immutable. Built-ins
/// also need a catalog read: deployment eligibility alone does not make them
/// safe to execute in a plan-time probe.
async fn unadmitted_routines(
    conn: &mut Conn,
    runs_sql: &BTreeSet<Oid>,
    functions: &BTreeMap<Oid, Vec<Use>>,
    operators: &BTreeMap<Oid, Vec<Use>>,
    row_compares: &BTreeMap<(Oid, Oid), Vec<Use>>,
) -> Result<(Vec<RoutineReason>, bool, Vec<OwnedRoutine>), Failure> {
    let mut operator_ids: BTreeSet<Oid> = operators.keys().copied().collect();
    operator_ids.extend(row_compares.keys().map(|(operator, _)| *operator));
    let operator_rows = operator_rows(conn, &operator_ids.into_iter().collect::<Vec<_>>()).await?;
    for oid in operators.keys() {
        let row = operator_rows.get(oid).ok_or_else(|| {
            Failure::Uncertified(format!("the operator with OID {oid} is not in the catalog"))
        })?;
        operator_function(*oid, row).map_err(Failure::Uncertified)?;
    }
    let supports =
        comparison_supports(conn, &row_compares.keys().copied().collect::<Vec<_>>()).await?;
    let mut wanted: BTreeSet<Oid> = BTreeSet::new();
    for oid in functions
        .keys()
        .chain(
            operators
                .keys()
                .filter_map(|o| operator_rows.get(o).map(|row| &row.code)),
        )
        .chain(supports.values())
    {
        wanted.insert(*oid);
    }
    let function_rows = function_rows(conn, &wanted.iter().copied().collect::<Vec<_>>()).await?;
    let function = |oid: Oid| -> Result<&FunctionRow, Failure> {
        function_rows.get(&oid).ok_or_else(|| {
            Failure::Uncertified(format!("the function with OID {oid} is not in the catalog"))
        })
    };
    let mut non_immutable = false;
    for oid in &wanted {
        match function(*oid)?.volatility.as_str() {
            "i" => {}
            "s" | "v" => non_immutable = true,
            other => {
                return Err(Failure::Uncertified(format!(
                    "the function with OID {oid} has unknown volatility `{other}`"
                )));
            }
        }
    }
    // Retain all routine owners before any built-in eligibility shortcut.
    let mut owners: Vec<_> = function_rows
        .values()
        .map(|r| OwnedRoutine {
            kind: RoutineKind::Function,
            identity: r.identity.clone(),
            owner: r.owner.clone(),
            owner_oid: r.owner_oid,
        })
        .collect();
    owners.extend(operator_rows.values().map(|r| OwnedRoutine {
        kind: RoutineKind::Operator,
        identity: r.identity.clone(),
        owner: r.owner.clone(),
        owner_oid: r.owner_oid,
    }));

    // An operator node also names the function it runs (`opfuncid`), which is
    // checked like any other so that a node that disagrees with the catalog
    // cannot hide a function. It is reported once, with its operator.
    let beside_operator: BTreeSet<(Oid, &Use)> = operators
        .iter()
        .filter_map(|(oid, uses)| operator_rows.get(oid).map(|row| (row.code, uses)))
        .flat_map(|(code, uses)| uses.iter().map(move |used_by| (code, used_by)))
        .collect();
    let mut out = Vec::new();
    for (oid, uses) in functions {
        let Some(why) = why_not_admitted(*oid, runs_sql) else {
            continue;
        };
        let row = function(*oid)?;
        for used_by in uses {
            if beside_operator.contains(&(*oid, used_by)) {
                continue;
            }
            out.push(RoutineReason {
                oid: *oid,
                kind: RoutineKind::Function,
                identity: row.identity.clone(),
                runs: None,
                owner: row.owner.clone(),
                why,
                used_by: used_by.clone(),
            });
        }
    }
    for (oid, uses) in operators {
        let row = operator_rows.get(oid).ok_or_else(|| {
            Failure::Uncertified(format!("the operator with OID {oid} is not in the catalog"))
        })?;
        let Some(why) = why_not_admitted(row.code, runs_sql) else {
            continue;
        };
        let runs = function(row.code)?;
        for used_by in uses {
            out.push(RoutineReason {
                oid: row.code,
                kind: RoutineKind::Operator,
                identity: row.identity.clone(),
                runs: Some(runs.identity.clone()),
                owner: row.owner.clone(),
                why,
                used_by: used_by.clone(),
            });
        }
    }
    // A row comparison runs its operator family's btree comparison function for
    // the operator's types, never the operator's own function.
    for ((operator, family), uses) in row_compares {
        let row = operator_rows.get(operator).ok_or_else(|| {
            Failure::Uncertified(format!(
                "the operator with OID {operator} is not in the catalog"
            ))
        })?;
        let support = supports.get(&(*operator, *family)).ok_or_else(|| {
            Failure::Uncertified(format!(
                "the operator `{}` of a row comparison has no btree comparison function in its \
                 operator family (OID {family})",
                row.identity
            ))
        })?;
        let Some(why) = why_not_admitted(*support, runs_sql) else {
            continue;
        };
        let runs = function(*support)?;
        for used_by in uses {
            out.push(RoutineReason {
                oid: *support,
                kind: RoutineKind::Operator,
                identity: row.identity.clone(),
                runs: Some(runs.identity.clone()),
                owner: row.owner.clone(),
                why,
                used_by: used_by.clone(),
            });
        }
    }
    Ok((out, non_immutable, owners))
}

/// The btree comparison function (support function 1) of the operator family
/// for each (operator, family) of a row comparison, found by the types the
/// operator has in the family, as the executor finds it.
async fn comparison_supports(
    conn: &mut Conn,
    pairs: &[(Oid, Oid)],
) -> Result<BTreeMap<(Oid, Oid), Oid>, DbError> {
    if pairs.is_empty() {
        return Ok(BTreeMap::new());
    }
    let operators: Vec<Oid> = pairs.iter().map(|(operator, _)| *operator).collect();
    let families: Vec<Oid> = pairs.iter().map(|(_, family)| *family).collect();
    let rows = conn
        .query(&format!(
            "SELECT o.amopopr::pg_catalog.int8 AS operator,
                    o.amopfamily::pg_catalog.int8 AS family,
                    p.amproc::pg_catalog.oid::pg_catalog.int8 AS support
               FROM pg_catalog.pg_amop o
               JOIN pg_catalog.pg_am am ON am.oid = o.amopmethod AND am.amname = 'btree'
               JOIN pg_catalog.pg_amproc p
                 ON p.amprocfamily = o.amopfamily
                AND p.amproclefttype = o.amoplefttype
                AND p.amprocrighttype = o.amoprighttype
                AND p.amprocnum = 1
              WHERE o.amopopr = ANY({}) AND o.amopfamily = ANY({})",
            oid_array(&operators),
            oid_array(&families)
        ))
        .await?;
    let wanted: BTreeSet<(Oid, Oid)> = pairs.iter().copied().collect();
    let mut out = BTreeMap::new();
    for row in &rows {
        let key = (need(row, "operator")?, need(row, "family")?);
        if wanted.contains(&key) {
            out.insert(key, need(row, "support")?);
        }
    }
    Ok(out)
}

async fn operator_rows(
    conn: &mut Conn,
    oids: &[Oid],
) -> Result<BTreeMap<Oid, OperatorRow>, DbError> {
    if oids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows = conn
        .query(&format!(
            "SELECT o.oid::pg_catalog.int8 AS oid,
                    i.identity AS identity,
                    ow.rolname::pg_catalog.text AS owner,
                    ow.oid::pg_catalog.int8 AS owner_oid,
                    o.oprcode::pg_catalog.oid::pg_catalog.int8 AS code
               FROM pg_catalog.pg_operator o
               JOIN pg_catalog.pg_roles ow ON ow.oid = o.oprowner
              CROSS JOIN LATERAL pg_catalog.pg_identify_object(
                        'pg_catalog.pg_operator'::pg_catalog.regclass, o.oid, 0) i
              WHERE o.oid = ANY({})",
            oid_array(oids)
        ))
        .await?;
    rows.iter()
        .map(|row| {
            Ok((
                need(row, "oid")?,
                OperatorRow {
                    identity: need::<&str>(row, "identity")?.to_owned(),
                    owner: need::<&str>(row, "owner")?.into(),
                    owner_oid: need(row, "owner_oid")?,
                    code: need(row, "code")?,
                },
            ))
        })
        .collect()
}

async fn function_rows(
    conn: &mut Conn,
    oids: &[Oid],
) -> Result<BTreeMap<Oid, FunctionRow>, DbError> {
    if oids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows = conn
        .query(&format!(
            "SELECT p.oid::pg_catalog.int8 AS oid,
                    i.identity AS identity,
                    p.provolatile::pg_catalog.text AS volatility,
                    ow.rolname::pg_catalog.text AS owner,
                    ow.oid::pg_catalog.int8 AS owner_oid
               FROM pg_catalog.pg_proc p
               JOIN pg_catalog.pg_roles ow ON ow.oid = p.proowner
              CROSS JOIN LATERAL pg_catalog.pg_identify_object(
                        'pg_catalog.pg_proc'::pg_catalog.regclass, p.oid, 0) i
              WHERE p.oid = ANY({})",
            oid_array(oids)
        ))
        .await?;
    rows.iter()
        .map(|row| {
            Ok((
                need(row, "oid")?,
                FunctionRow {
                    identity: need::<&str>(row, "identity")?.to_owned(),
                    owner: need::<&str>(row, "owner")?.into(),
                    owner_oid: need(row, "owner_oid")?,
                    volatility: need::<&str>(row, "volatility")?.to_owned(),
                },
            ))
        })
        .collect()
}

// -- the owners ---------------------------------------------------------------

/// For each owner, the roles that are not trusted and can act as it: the
/// owner itself when it is one of them, and every untrusted role that reaches
/// the owner by membership with `SET`, `INHERIT` or `ADMIN`. It is the walk of
/// DEC-1663.1 ([`untrusted_role_walk`]) with `trusted` as the roles a human
/// declared trusted.
async fn untrusted_actors(
    conn: &mut Conn,
    trusted: &[pbps_model::RoleName],
    owners: &BTreeSet<Oid>,
) -> Result<BTreeMap<Oid, Vec<pbps_model::RoleName>>, DbError> {
    let mut out: BTreeMap<Oid, Vec<pbps_model::RoleName>> = BTreeMap::new();
    if owners.is_empty() {
        return Ok(out);
    }
    let declared = json(trusted)?;
    let wanted = oid_array(&owners.iter().copied().collect::<Vec<_>>());
    let rows = conn
        .query_with(
            &format!(
                "WITH RECURSIVE {walk}
                 SELECT DISTINCT o.oid::pg_catalog.int8 AS owner_oid,
                                 a.rolname::pg_catalog.text AS actor
                   FROM pg_catalog.pg_roles o
                   JOIN reach r ON r.oid OPERATOR(pg_catalog.=) o.oid
                   JOIN others a ON a.oid OPERATOR(pg_catalog.=) r.origin
                  WHERE o.oid OPERATOR(pg_catalog.=) ANY({wanted})
                  ORDER BY 1, 2",
                walk = untrusted_role_walk(
                    "ARRAY(SELECT pg_catalog.jsonb_array_elements_text(\
                     $1::pg_catalog.text::pg_catalog.jsonb))"
                ),
            ),
            &[Param::Str(&declared)],
        )
        .await?;
    for row in &rows {
        out.entry(need(row, "owner_oid")?)
            .or_default()
            .push(need::<&str>(row, "actor")?.into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_domain_check_use_escapes_its_local_name_once() {
        let used = super::Use {
            ty: "app.positive".into(),
            part: super::Part::Check {
                constraint: "ck\nline\\slash".into(),
            },
        };
        assert_eq!(
            used.to_string(),
            r"the CHECK `ck\nline\\slash` of `app.positive`"
        );
        assert_ne!(
            used,
            super::Use {
                ty: "app.positive".into(),
                part: super::Part::Check {
                    constraint: r"ck\nline\\slash".into()
                }
            }
        );
    }

    #[test]
    fn owner_actor_and_missing_trust_diagnostics_show_role_names_once() {
        let name = pbps_model::RoleName::from("role\nline\\\u{202e}名前");
        let shown = r"role\nline\\\u{202e}名前";
        let same = super::Reason::Owner(super::OwnerReason {
            ty: "app.t".into(),
            owner: name.clone(),
            actor: name.clone(),
        });
        assert_eq!(
            same.to_string(),
            format!(
                "the type `app.t` is owned by `{shown}`, a role that is neither a superuser, nor able to become the deploying role or a superuser, nor declared trusted, and so it can change what that type runs"
            )
        );
        let different = super::Reason::Owner(super::OwnerReason {
            ty: "app.t".into(),
            owner: "owner".into(),
            actor: name.clone(),
        });
        assert_eq!(
            different.to_string(),
            format!(
                "the type `app.t` is owned by `owner`, and `{shown}`, a role that is neither a superuser, nor able to become the deploying role or a superuser, nor declared trusted, can act as it (by membership, or by granting it to itself) and so change what that type runs"
            )
        );
        let missing =
            super::InputPath::Unreadable(super::Unreadable::TrustedRolesMissing(vec![name]));
        assert_eq!(
            missing
                .refusal(&pbps_model::TypeRef::new("app", "t").unwrap())
                .unwrap(),
            format!(
                "the type `app.t` cannot be checked: the role(s) `{shown}` declared trusted do not exist"
            )
        );
        assert!(
            super::InputPath::Admitted(super::Admitted { runs_code: false })
                .refusal(&pbps_model::TypeRef::new("app", "t").unwrap())
                .is_none()
        );
    }

    use super::*;

    fn used(ty: &str) -> Use {
        Use {
            ty: ty.to_owned(),
            part: Part::Check {
                constraint: format!("{ty}_check").into(),
            },
        }
    }

    fn user_function() -> RoutineReason {
        RoutineReason {
            oid: 16384,
            kind: RoutineKind::Function,
            identity: "app.is_positive(integer)".to_owned(),
            runs: None,
            owner: "alice".to_owned().into(),
            why: Why::NotBuiltin,
            used_by: used("app.positive"),
        }
    }

    fn owned(oid: Oid, ty: &str, owner: &str) -> OwnedType {
        OwnedType {
            oid,
            ty: ty.to_owned(),
            owner: owner.to_owned().into(),
            owner_oid: oid,
        }
    }

    fn no_actors() -> BTreeMap<Oid, Vec<pbps_model::RoleName>> {
        BTreeMap::new()
    }

    #[test]
    fn the_engines_own_objects_end_at_first_normal_object_id() {
        assert!(is_builtin(1));
        assert!(is_builtin(16383));
        assert!(!is_builtin(16384));
        assert!(!is_builtin(16385));
        // The constant is the resolver's, not a second copy of the number.
        assert_eq!(Oid::from(FIRST_NORMAL_OBJECT_ID), 16384);
    }

    #[test]
    fn a_function_is_admitted_only_when_built_in_and_not_on_the_runs_sql_list() {
        let denied = BTreeSet::from([2924, 6210]);
        assert_eq!(why_not_admitted(1317, &denied), None);
        assert_eq!(why_not_admitted(16383, &denied), None);
        assert_eq!(why_not_admitted(16384, &denied), Some(Why::NotBuiltin));
        assert_eq!(why_not_admitted(1_213_810, &denied), Some(Why::NotBuiltin));
        // Built in and still refused: the list is the second condition.
        assert_eq!(why_not_admitted(2924, &denied), Some(Why::RunsSql));
        assert_eq!(why_not_admitted(6210, &denied), Some(Why::RunsSql));
        // Negative: a function not on the list is not refused for being near it.
        assert_eq!(why_not_admitted(2925, &denied), None);
    }

    #[test]
    fn the_runs_sql_list_names_each_function_once_and_by_signature() {
        let mut seen = BTreeSet::new();
        for sig in RUNS_SQL {
            assert!(seen.insert(*sig), "{sig} is listed twice");
            // `schema.name(argument types)`, never a raw OID.
            assert!(sig.starts_with("pg_catalog."), "{sig}");
            assert!(sig.ends_with(')') && sig.contains('('), "{sig}");
            assert!(
                !sig.contains(' '),
                "{sig} would not parse the same everywhere"
            );
        }
        // The families the design names (#2046) are all here.
        for family in [
            "query_to_xml(",
            "query_to_xmlschema(",
            "query_to_xml_and_xmlschema(",
            "cursor_to_xml(",
            "cursor_to_xmlschema(",
            "table_to_xml(",
            "table_to_xmlschema(",
            "table_to_xml_and_xmlschema(",
            "schema_to_xml(",
            "schema_to_xmlschema(",
            "schema_to_xml_and_xmlschema(",
            "database_to_xml(",
            "database_to_xmlschema(",
            "database_to_xml_and_xmlschema(",
            "ts_stat(text)",
            "ts_stat(text,text)",
            "ts_rewrite(tsquery,text)",
        ] {
            assert!(
                RUNS_SQL.iter().any(|s| s.contains(family)),
                "{family} is missing"
            );
        }
        // Negative: the rewrite that runs no SQL is not refused.
        assert!(
            !RUNS_SQL
                .iter()
                .any(|s| s.contains("tsquery,tsquery,tsquery"))
        );
    }

    #[test]
    fn a_code_free_path_requires_trusted_owners_before_admission() {
        let facts = Facts {
            runs_code: false,
            non_immutable: false,
            routines: Vec::new(),
            routine_owners: Vec::new(),
            types: vec![owned(40001, "app.mood", "mallory")],
        };
        let actors = BTreeMap::from([(40001, vec!["mallory".into()])]);
        let InputPath::Refused(reasons) = verdict(facts.clone(), &actors) else {
            panic!("a code-free path still requires trusted owners");
        };
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            verdict(facts, &no_actors()),
            InputPath::Admitted(Admitted { runs_code: false })
        );
    }

    #[test]
    fn a_path_that_runs_code_is_admitted_when_it_is_built_in_and_owned_by_the_trusted() {
        let facts = Facts {
            runs_code: true,
            non_immutable: false,
            routine_owners: Vec::new(),
            routines: Vec::new(),
            types: vec![owned(40002, "app.positive", "deployer")],
        };
        let InputPath::Admitted(admitted) = verdict(facts, &no_actors()) else {
            panic!("built-in code on a trusted type must be admitted");
        };
        assert!(admitted.runs_code());
    }

    #[test]
    fn skipping_empty_array_elements_retains_the_target_owner_check() {
        let facts = Facts {
            runs_code: false,
            non_immutable: false,
            routine_owners: Vec::new(),
            routines: Vec::new(),
            types: vec![owned(40002, "app.checked", "owner_role")],
        };
        let actors = BTreeMap::from([(40002, vec!["member".into()])]);
        let InputPath::Refused(reasons) = verdict(facts.clone(), &actors) else {
            panic!("an empty array still requires a trusted target owner");
        };
        assert!(reasons[0].to_string().contains("member"));
        assert_eq!(
            verdict(facts, &no_actors()),
            InputPath::Admitted(Admitted { runs_code: false })
        );
    }

    #[test]
    fn a_user_routine_refuses_a_path_that_runs_code_and_is_named() {
        let facts = Facts {
            runs_code: true,
            non_immutable: false,
            routine_owners: Vec::new(),
            routines: vec![user_function()],
            types: vec![owned(40002, "app.positive", "deployer")],
        };
        let InputPath::Refused(reasons) = verdict(facts, &no_actors()) else {
            panic!("a user function must refuse");
        };
        assert_eq!(reasons, vec![Reason::Routine(user_function())]);
        let message = reasons[0].to_string();
        assert!(message.contains("app.is_positive(integer)"), "{message}");
        assert!(message.contains("alice"), "{message}");
        assert!(message.contains("app.positive_check"), "{message}");
    }

    #[test]
    fn an_untrusted_owner_refuses_and_every_untrusted_actor_is_named() {
        let facts = Facts {
            runs_code: true,
            non_immutable: false,
            routine_owners: Vec::new(),
            routines: Vec::new(),
            types: vec![
                owned(40002, "app.positive", "owner_role"),
                owned(40003, "app.pair", "deployer"),
            ],
        };
        let actors = BTreeMap::from([(40002, vec!["member_a".into(), "owner_role".into()])]);
        let InputPath::Refused(reasons) = verdict(facts, &actors) else {
            panic!("an untrusted actor must refuse");
        };
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        let text: Vec<String> = reasons.iter().map(ToString::to_string).collect();
        assert!(
            text.iter()
                .any(|t| t.contains("owned by `owner_role`, a role that is neither")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|t| t.contains("`member_a`") && t.contains("can act as it")),
            "{text:?}"
        );
        // Negative: the type owned by a trusted role is not named.
        assert!(!text.iter().any(|t| t.contains("app.pair")), "{text:?}");
    }

    #[test]
    fn reasons_are_sorted_and_not_repeated() {
        let facts = Facts {
            runs_code: true,
            non_immutable: false,
            routine_owners: Vec::new(),
            routines: vec![user_function(), user_function()],
            types: Vec::new(),
        };
        let InputPath::Refused(reasons) = verdict(facts, &no_actors()) else {
            panic!("refused");
        };
        assert_eq!(reasons.len(), 1);
    }

    #[test]
    fn two_passes_that_differ_are_a_failed_read_and_two_that_agree_are_the_read() {
        let admitted = || InputPath::Admitted(Admitted { runs_code: true });
        let refused = || InputPath::Refused(vec![Reason::Routine(user_function())]);
        assert_eq!(settled(admitted(), admitted()), admitted());
        assert_eq!(settled(refused(), refused()), refused());
        let InputPath::Unreadable(Unreadable::Failed(why)) = settled(admitted(), refused()) else {
            panic!("passes that differ must not be an answer");
        };
        assert!(why.contains("changed while"), "{why}");
    }

    #[test]
    fn only_an_admitted_answer_is_admitted() {
        let reference = TypeRef::new("app", "positive").unwrap();
        let admitted = InputPath::Admitted(Admitted { runs_code: true });
        assert!(admitted.is_admitted());
        assert_eq!(admitted.refusal(&reference), None);
        for not in [
            InputPath::Refused(vec![Reason::Routine(user_function())]),
            InputPath::Unreadable(Unreadable::Failed("boom".to_owned())),
            InputPath::Unreadable(Unreadable::TrustedRolesMissing(vec![
                "ghost".to_owned().into(),
            ])),
        ] {
            assert!(!not.is_admitted(), "{not:?}");
            assert!(not.refusal(&reference).is_some(), "{not:?}");
        }
    }

    #[test]
    fn each_outcome_has_its_own_message() {
        let reference = TypeRef::new("app", "positive").unwrap();
        let refused = InputPath::Refused(vec![Reason::Routine(user_function())])
            .refusal(&reference)
            .unwrap();
        let missing = InputPath::Unreadable(Unreadable::TrustedRolesMissing(vec![
            "ghost".to_owned().into(),
        ]))
        .refusal(&reference)
        .unwrap();
        let failed = InputPath::Unreadable(Unreadable::Failed("boom".to_owned()))
            .refusal(&reference)
            .unwrap();
        assert!(refused.contains("app.is_positive(integer)"), "{refused}");
        assert!(
            missing.contains("`ghost`") && missing.contains("do not exist"),
            "{missing}"
        );
        // An unreadable answer says it is not a type that runs nothing.
        assert!(
            failed.contains("boom") && failed.contains("not a type that runs no code"),
            "{failed}"
        );
        assert_ne!(refused, missing);
        assert_ne!(missing, failed);
    }

    #[test]
    fn a_long_refusal_lists_some_reasons_and_counts_the_rest() {
        let reference = TypeRef::new("app", "positive").unwrap();
        let reasons: Vec<Reason> = (0..SHOWN + 5)
            .map(|i| {
                Reason::Owner(OwnerReason {
                    ty: "app.positive".to_owned(),
                    owner: "owner_role".to_owned().into(),
                    actor: format!("member_{i:02}").into(),
                })
            })
            .collect();
        let message = InputPath::Refused(reasons).refusal(&reference).unwrap();
        assert!(message.contains("member_00"), "{message}");
        assert!(
            !message.contains(&format!("member_{:02}", SHOWN + 4)),
            "{message}"
        );
        assert!(message.contains("and 5 more"), "{message}");
    }

    /// The text the engine stores for `CHECK (length(VALUE) < 100)` on a text
    /// domain: 16.15 and 18.6 (`:location` differs).
    const LENGTH_CHECK_16: &str = "{OPEXPR :opno 97 :opfuncid 66 :opresulttype 16 :opretset false :opcollid 0 :inputcollid 0 :args ({FUNCEXPR :funcid 1317 :funcresulttype 23 :funcretset false :funcvariadic false :funcformat 0 :funccollid 0 :inputcollid 100 :args ({COERCETODOMAINVALUE :typeId 25 :typeMod -1 :collation 100 :location 51}) :location 44} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 60 :constvalue 4 [ 100 0 0 0 0 0 0 0 ]}) :location 58}";
    const LENGTH_CHECK_18: &str = "{OPEXPR :opno 97 :opfuncid 66 :opresulttype 16 :opretset false :opcollid 0 :inputcollid 0 :args ({FUNCEXPR :funcid 1317 :funcresulttype 23 :funcretset false :funcvariadic false :funcformat 0 :funccollid 0 :inputcollid 100 :args ({COERCETODOMAINVALUE :typeId 25 :typeMod -1 :collation 100 :location -1}) :location -1} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 100 0 0 0 0 0 0 0 ]}) :location -1}";
    /// `CHECK (app.is_pos(VALUE))`, the function's OID being 1213810.
    const USER_FUNCTION_CHECK_18: &str = "{FUNCEXPR :funcid 1213810 :funcresulttype 16 :funcretset false :funcvariadic false :funcformat 0 :funccollid 0 :inputcollid 0 :args ({COERCETODOMAINVALUE :typeId 23 :typeMod -1 :collation 0 :location -1}) :location -1}";
    /// `CHECK (VALUE::app.other IS NOT NULL)`, the domain's OID being 1213815.
    const DOMAIN_CAST_CHECK_18: &str = "{NULLTEST :arg {COERCETODOMAIN :arg {COERCETODOMAINVALUE :typeId 23 :typeMod -1 :collation 0 :location -1} :resulttype 1213815 :resulttypmod -1 :resultcollid 0 :coercionformat 1 :location -1} :nulltesttype 1 :argisrow false :location -1}";

    #[test]
    fn a_stored_check_yields_its_functions_operators_and_types_on_both_majors() {
        for (text, major) in [(LENGTH_CHECK_16, 16), (LENGTH_CHECK_18, 18)] {
            let bound = bound_objects(text, major).expect("a measured tree decodes");
            // `length(text)` is function 1317 and `<` is operator 97, whose
            // function is 66: all of them built in, none in `pg_depend`.
            assert_eq!(bound.routines, BTreeSet::from([66, 1317]), "{major}");
            assert_eq!(bound.operators, BTreeSet::from([97]), "{major}");
            assert!(bound.types.contains(&25), "{major}");
            assert!(bound.routines.iter().all(|f| is_builtin(Oid::from(*f))));
        }
    }

    /// `CHECK (ROW(VALUE, 1) OPERATOR(app.@<@) ROW(5, 5))` over a user operator
    /// class: the operator is 1498445 and its family 1498446 (16: 196995 and
    /// 196996, `:rctype` for `:cmptype`).
    const ROW_COMPARE_18: &str = "{ROWCOMPAREEXPR :cmptype 1 :opnos (o 1498445 1498445) :opfamilies (o 1498446 1498446) :inputcollids (o 0 0) :largs ({COERCETODOMAINVALUE :typeId 23 :typeMod -1 :collation 0 :location -1} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]}) :rargs ({CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 5 0 0 0 0 0 0 0 ]} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 5 0 0 0 0 0 0 0 ]})}";
    const ROW_COMPARE_16: &str = "{ROWCOMPAREEXPR :rctype 1 :opnos (o 196995 196995) :opfamilies (o 196996 196996) :inputcollids (o 0 0) :largs ({COERCETODOMAINVALUE :typeId 23 :typeMod -1 :collation 0 :location 45} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 52 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]}) :rargs ({CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 77 :constvalue 4 [ 5 0 0 0 0 0 0 0 ]} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 80 :constvalue 4 [ 5 0 0 0 0 0 0 0 ]})}";

    #[test]
    fn a_row_comparison_is_its_operator_and_family_and_not_a_call_of_the_operator() {
        for (text, major, pair) in [
            (ROW_COMPARE_18, 18, (1_498_445, 1_498_446)),
            (ROW_COMPARE_16, 16, (196_995, 196_996)),
        ] {
            let bound = bound_objects(text, major).expect("a measured tree decodes");
            // The executor calls the family's comparison function, so the
            // operator is not listed as one that runs its own function.
            assert!(bound.operators.is_empty(), "{major}: {bound:?}");
            assert!(bound.routines.is_empty(), "{major}: {bound:?}");
            assert_eq!(bound.row_compares, BTreeSet::from([pair]), "{major}");
        }
        // Negative: operators and families that do not pair are an error and
        // never a smaller set.
        let unpaired =
            ROW_COMPARE_18.replace(":opfamilies (o 1498446 1498446)", ":opfamilies (o 1498446)");
        assert!(bound_objects(&unpaired, 18).is_err());
    }

    #[test]
    fn a_user_function_and_a_named_domain_are_found_in_the_tree() {
        let bound = bound_objects(USER_FUNCTION_CHECK_18, 18).unwrap();
        assert_eq!(bound.routines, BTreeSet::from([1_213_810]));
        assert!(!is_builtin(1_213_810));
        let bound = bound_objects(DOMAIN_CAST_CHECK_18, 18).unwrap();
        assert!(bound.types.contains(&1_213_815), "{bound:?}");
        assert!(bound.routines.is_empty());
    }

    #[test]
    fn a_list_that_does_not_describe_the_server_is_not_a_shorter_list() {
        let sigs = |oids: &[Option<Oid>]| -> Vec<(String, Option<Oid>)> {
            oids.iter()
                .enumerate()
                .map(|(i, oid)| (format!("pg_catalog.f{i}(text)"), *oid))
                .collect()
        };
        assert_eq!(
            resolve_runs_sql(&sigs(&[Some(2924), Some(6210)])),
            Ok(BTreeSet::from([2924, 6210]))
        );
        // Negative: one the server lacks, and one that is not built in (a role
        // made a function of that name in `pg_catalog`), each fail the list.
        let missing = resolve_runs_sql(&sigs(&[Some(2924), None])).unwrap_err();
        assert!(missing.contains("pg_catalog.f1(text)") && missing.contains("re-audited"));
        assert!(resolve_runs_sql(&sigs(&[Some(2924), Some(16384)])).is_err());
        assert!(
            resolve_runs_sql(&sigs(&[None, None]))
                .unwrap_err()
                .contains("f0")
        );
    }

    #[test]
    fn what_cannot_be_certified_is_never_certified_as_nothing() {
        let used_by = used("app.positive");
        // A tree the decoder refuses: a type is unreadable, not call-free.
        for text in ["", "garbage", "{NOSUCHNODE :x 1}"] {
            let Err(Failure::Uncertified(why)) = certify(text, 18, &used_by) else {
                panic!("`{text}` must not be certified");
            };
            assert!(why.contains("app.positive_check"), "{why}");
        }
        assert!(matches!(
            certify(LENGTH_CHECK_18, 18, &used_by),
            Ok(Bound { .. })
        ));
        // A constraint that is not a CHECK, and a CHECK with no tree.
        assert_eq!(check_text("c", "c", Some("{X}")), Ok("{X}".to_owned()));
        assert!(
            check_text("c", "x", Some("{X}"))
                .unwrap_err()
                .contains("kind `x`")
        );
        assert!(
            check_text("c", "c", None)
                .unwrap_err()
                .contains("no stored expression")
        );
        // An operator with no function.
        let shell = OperatorRow {
            identity: "app.@+@(integer,integer)".to_owned(),
            owner: "alice".to_owned().into(),
            owner_oid: 42,
            code: 0,
        };
        assert!(operator_function(40000, &shell).is_err());
        let real = OperatorRow {
            code: 1_213_810,
            ..shell
        };
        assert_eq!(operator_function(40000, &real), Ok(1_213_810));
    }

    #[test]
    fn a_tree_the_decoder_cannot_certify_is_an_error_and_never_an_empty_set() {
        // Negative: a major it was not measured on, a node it does not know and
        // text that is not a tree each fail; none reads as "calls nothing".
        assert!(bound_objects(LENGTH_CHECK_18, 17).is_err());
        assert!(bound_objects("{NOSUCHNODE :x 1}", 18).is_err());
        assert!(bound_objects("not a node tree", 18).is_err());
        assert!(bound_objects("", 18).is_err());
        // A node with a field removed.
        let truncated = LENGTH_CHECK_18.replace(" :opcollid 0", "");
        assert!(bound_objects(&truncated, 18).is_err());
    }

    #[test]
    fn a_known_node_layout_without_an_execution_proof_is_refused() {
        for major in [16, 18] {
            // The generic resolver decodes this identity-sequence node. That
            // is not an input-path proof of the code the node itself runs.
            let why = bound_objects("{NEXTVALUEEXPR :seqid 40001 :typeId 20}", major)
                .expect_err("a known layout cannot substitute for an execution proof");
            assert!(why.contains("NEXTVALUEEXPR"), "{major}: {why}");
            assert!(bound_objects(LENGTH_CHECK_18, major).is_ok());
        }
    }
}
