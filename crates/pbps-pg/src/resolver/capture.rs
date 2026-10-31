//! Private target-input capture components (ADR-0016; issue #612).
//!
//! Source stays in memory; a fresh authorized read can seal keyed fingerprints.
//! No saved-plan publication path is enabled by these components; the engine adapter must qualify the
//! requested read scope and the lifecycle must qualify non-snapshot inputs.

mod bindings;
mod body;
mod logical;
mod nodes;
pub(crate) mod policy_names;
mod properties;
mod queries;
mod read;
mod render;

mod profile;
mod scope;

use pbps_db::resolver::capture::ObjectIdentity;
use std::collections::{BTreeMap, BTreeSet};

/// What a stored expression's node tree binds, by class (#1523, #2046).
///
/// `pg_depend` is no substitute: it omits the pinned built-ins, so it cannot
/// say which built-in function a CHECK or a DEFAULT calls, and a gate that
/// admits only built-in code has to know exactly that. The tree carries every
/// occurrence, and the decoder refuses a node or a field it does not know
/// instead of certifying a partial set (see `nodes`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Bound {
    /// Every function the tree names: calls, cast functions, an operator's
    /// own function (`opfuncid`).
    pub routines: BTreeSet<u32>,
    /// Every operator an operator node names, which runs its
    /// `pg_operator.oprcode`.
    pub operators: BTreeSet<u32>,
    /// The operators of a row comparison, each with the operator family the
    /// engine finds it in. The executor does not call such an operator: it
    /// calls the family's btree comparison support function for the
    /// operator's types, so that is the code a row comparison runs.
    pub row_compares: BTreeSet<(u32, u32)>,
    /// Every type: the type a coercion or a literal names.
    pub types: BTreeSet<u32>,
    /// Domain coercions execute CHECKs; a CONST merely holds an already typed value.
    pub domain_coercions: BTreeSet<u32>,
    /// I/O coercions run the target's input function, including nested CHECKs.
    pub input_coercions: BTreeSet<u32>,
}

/// Where in the tree a row comparison's operator or family sits: the path to
/// the node and the position in its lists.
type RowCompareSlot = (Vec<String>, String);

/// The objects a stored expression binds, or why the tree cannot be certified
/// complete. The reason is the decoder's category only: a tree can hold a
/// private literal.
pub(crate) fn bound_objects(text: &str, major: u32) -> Result<Bound, String> {
    let tree = nodes::decode(text, major)
        .map_err(|e| format!("the stored expression is not one this release can read ({e:?})"))?;
    bind(&tree, major, false)
}

/// Why a stored function body is not a tree the input-path proof can read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BodyError {
    /// The decoder does not know the tree: not a body that runs nothing.
    Unreadable(String),
    /// A readable tree that is more than a list of plain `SELECT` expressions.
    NotPlain(String),
}

/// The objects a function's stored `prosqlbody` binds (#2563). The body of a
/// `BEGIN ATOMIC` or `RETURN` function is a parsed `Query` list in the same
/// node format as `conbin`, so it goes through the same decoder and the same
/// execution proof. It is admitted only as a list of `SELECT`s over no
/// relation: a table, view, CTE or subquery would bring in row security,
/// rules and further code that no expression node names (see `body`).
pub(crate) fn bound_body(text: &str, major: u32) -> Result<Bound, BodyError> {
    let tree = nodes::decode(text, major).map_err(|e| {
        BodyError::Unreadable(format!(
            "the stored body is not one this release can read ({e:?})"
        ))
    })?;
    body::check(&tree).map_err(BodyError::NotPlain)?;
    bind(&tree, major, true).map_err(BodyError::Unreadable)
}

/// The shared proof. `body` admits what only a function body holds: its
/// statements' target lists and its arguments as `PARAM` nodes.
fn bind(tree: &nodes::Value, major: u32, body: bool) -> Result<Bound, String> {
    let mut bound = Bound::default();
    // A row comparison lists its operators and their families side by side, so
    // each pair is found by the node and the position they share.
    let mut compared: BTreeMap<RowCompareSlot, (Option<u32>, Option<u32>)> = BTreeMap::new();
    for reference in nodes::references(tree, major).map_err(|e| {
        format!("the stored expression has a reference this release cannot read ({e:?})")
    })? {
        if reference.node == "ROWCOMPAREEXPR"
            && matches!(
                reference.class,
                nodes::ReferenceClass::Operator | nodes::ReferenceClass::OperatorFamily
            )
        {
            let mut path = reference.path.clone();
            let (Some(position), Some(_field)) = (path.pop(), path.pop()) else {
                return Err("a row comparison's reference has no position".to_owned());
            };
            let entry = compared.entry((path, position)).or_default();
            if reference.class == nodes::ReferenceClass::Operator {
                entry.0 = Some(reference.oid);
            } else {
                entry.1 = Some(reference.oid);
            }
            continue;
        }
        // A relation, collation, role, constraint or operator class an
        // expression names is not code of its own: what runs is the function
        // or the operator that takes it, and that is listed above.
        match reference.class {
            nodes::ReferenceClass::Routine => bound.routines.insert(reference.oid),
            nodes::ReferenceClass::Operator => bound.operators.insert(reference.oid),
            nodes::ReferenceClass::Type => {
                if reference.node == "COERCETODOMAIN" {
                    bound.domain_coercions.insert(reference.oid);
                }
                bound.types.insert(reference.oid)
            }
            nodes::ReferenceClass::Relation
            | nodes::ReferenceClass::Collation
            | nodes::ReferenceClass::Role
            | nodes::ReferenceClass::Constraint
            | nodes::ReferenceClass::OperatorClass
            | nodes::ReferenceClass::OperatorFamily => false,
        };
    }
    for (operator, family) in compared.into_values() {
        match (operator, family) {
            (Some(operator), Some(family)) => {
                bound.row_compares.insert((operator, family));
            }
            // An operator with no family, or the reverse, is a tree this cannot
            // read to the end.
            (Some(_), None) | (None, Some(_)) | (None, None) => {
                return Err("a row comparison's operators and families do not pair".to_owned());
            }
        }
    }
    if body {
        for expression in body::expressions(tree)? {
            input_coercions(expression, &mut bound.input_coercions, true)?;
        }
    } else {
        input_coercions(tree, &mut bound.input_coercions, false)?;
    }
    Ok(bound)
}

/// Knowing a node's layout does not prove what it executes (DEC-2181.4).
/// Keep the input-path proof closed independently of the resolver's decoder.
fn input_coercions(
    value: &nodes::Value,
    inputs: &mut BTreeSet<u32>,
    body: bool,
) -> Result<(), String> {
    use nodes::Value;
    match value {
        Value::Node(node) => {
            match node.tag.as_str() {
                // These hold values which have already passed conversion.
                "CONST" | "COERCETODOMAINVALUE" | "CASETESTEXPR" => return Ok(()),
                "COERCEVIAIO" => {
                    inputs.insert(
                        node.number("resulttype").map_err(|_| {
                            "an I/O coercion has no readable target type".to_owned()
                        })?,
                    );
                }
                // Calls and cast functions are bound by OID above. Domain
                // coercions add their CHECK path; array coercions execute the
                // explicit elemexpr, not an implicit target input conversion.
                "FUNCEXPR" | "OPEXPR" | "DISTINCTEXPR" | "NULLIFEXPR" | "SCALARARRAYOPEXPR"
                | "ROWCOMPAREEXPR" | "COERCETODOMAIN" | "ARRAYCOERCEEXPR" => {}
                // These copy, extract, rearrange or assemble already typed
                // values. Any conversion is in a child expression.
                "RELABELTYPE" | "CONVERTROWTYPEEXPR" | "FIELDSELECT" | "FIELDSTORE" | "ROWEXPR"
                | "ARRAYEXPR" | "COLLATEEXPR" | "NAMEDARGEXPR" | "BOOLEXPR" | "BOOLEANTEST"
                | "NULLTEST" | "CASEEXPR" | "CASEWHEN" | "COALESCEEXPR" => {}
                // A column read. A domain's CHECK cannot hold one, so this
                // only ever admits a recorded conversion's expression, bound
                // against its one column (#2331): the read copies a value.
                "VAR" => {}
                // An argument of the function whose body this is: a copy of a
                // value its caller already passed. Only the external kind, and
                // only in a body, where no subplan or sublink can be.
                "PARAM"
                    if body && node.fields.get("paramkind") == Some(&Value::Atom("0".into())) => {}
                // C1's superuser-installed base-type/operator-class support
                // boundary applies to these dispatchers (input_path's module
                // docs). Neither performs an implicit domain input conversion.
                "MINMAXEXPR" | "SUBSCRIPTINGREF" => {}
                // Fixed engine operations. JSON constructors expose RETURNING
                // conversions as children; JSONEXPR does not, so it is refused
                // below rather than treated as another constructor.
                "SQLVALUEFUNCTION"
                | "XMLEXPR"
                | "JSONCONSTRUCTOREXPR"
                | "JSONFORMAT"
                | "JSONRETURNING"
                | "JSONISPREDICATE" => {}
                _ => {
                    return Err(format!(
                        "node `{}` has no audited input-path code proof",
                        node.tag
                    ));
                }
            }
            for child in node.fields.values() {
                input_coercions(child, inputs, body)?;
            }
        }
        Value::List(values) => {
            for child in values {
                input_coercions(child, inputs, body)?;
            }
        }
        Value::Null | Value::Atom(_) | Value::Datum => {}
    }
    Ok(())
}

/// A complete set of candidates sharing a name, or all names in a namespace.
/// The set includes every overload; signatures cannot filter its membership.
/// Capturing a set is not proof that a planner selected every required set.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct CandidateSet {
    pub class: CandidateClass,
    pub namespace: Option<String>,
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub enum CandidateClass {
    Relation,
    Routine,
    Type,
    Operator,
    Collation,
    OperatorClass,
    OperatorFamily,
    Cast,
    Extension,
}

impl CandidateClass {
    const ALL: [Self; 9] = [
        Self::Relation,
        Self::Routine,
        Self::Type,
        Self::Operator,
        Self::Collation,
        Self::OperatorClass,
        Self::OperatorFamily,
        Self::Cast,
        Self::Extension,
    ];

    /// The class a sealed predicate names by its catalog.
    fn of_catalog(catalog: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|class| class.catalog() == catalog)
    }

    fn catalog(self) -> &'static str {
        match self {
            Self::Relation => "pg_class",
            Self::Routine => "pg_proc",
            Self::Type => "pg_type",
            Self::Operator => "pg_operator",
            Self::Collation => "pg_collation",
            Self::OperatorClass => "pg_opclass",
            Self::OperatorFamily => "pg_opfamily",
            Self::Cast => "pg_cast",
            Self::Extension => "pg_extension",
        }
    }
}

/// Actual retained roots and complete candidate predicates to read. This is
/// an input request, never a caller-supplied completeness certificate.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CaptureScope {
    pub retained: BTreeSet<ObjectIdentity>,
    pub candidates: BTreeSet<CandidateSet>,
}

/// A dropped routine's declared signature as the plan's `DROP` spells it,
/// with the write path that `DROP` runs under and the kind it must be (`f`
/// or `p`). The capture identifies it in its own snapshot, so the routine a
/// plan drops is read coherently with the candidates it is assessed against
/// (#1124, #1126, #1148).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct DroppedSignature {
    pub spelled: String,
    pub path: String,
    pub kind: &'static str,
}

pub use pbps_db::resolver::capture::{CaptureError, Uncovered};

mod manifest;
mod ownership;
pub use manifest::{BindingRecord, CapturedInputs, CompiledCapture, SealError};

/// The canonical property rule every sealed PostgreSQL prerequisite uses.
pub(crate) const INPUT_RULE: &str = properties::RULE;
pub use ownership::RecordedOwnership;

mod assess;
pub use assess::{Managed, Paths, assess, managed_scope, scope};

mod apply;
pub use apply::{capture_closing, recheck_opening};

mod staged;
pub use pbps_db::resolver::capture::{Assessment, Verdict};
pub use staged::{StageError, Staged, compare as compare_staged};

/// Read coherent catalog inputs and release the transaction before returning.
/// The caller discards the connection on cancellation; no publication path
/// exists here.
pub async fn capture(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
) -> Result<CapturedInputs, CaptureError> {
    capture_identifying(connection, scope, &BTreeSet::new()).await
}

/// [`capture`], also identifying each dropped signature in the same snapshot.
pub async fn capture_identifying(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
    dropped: &BTreeSet<DroppedSignature>,
) -> Result<CapturedInputs, CaptureError> {
    let mut prepared = None;
    let read = read::owned(connection, dropped, |catalog, major| {
        let mut result = scope::prepare(catalog, major, scope)?;
        let selection = std::mem::take(&mut result.render);
        prepared = Some(result);
        Ok(selection)
    })
    .await?;
    manifest::finish(
        read,
        prepared.ok_or(CaptureError::Incomplete)?,
        scope.clone(),
    )
    .map_err(CaptureError::Coverage)
}

/// Qualify recorded ownership while the final coherent read still holds its
/// private relkind/prokind/dependency facts. The key and Schema/IdsFile are
/// fixed at this producer entry; the returned capture has no rekey method.
pub async fn capture_identifying_qualified(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
    dropped: &BTreeSet<DroppedSignature>,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    roles: Option<&crate::resolver::authorization::RoleMap>,
    recorded: &RecordedOwnership<'_>,
) -> Result<(CapturedInputs, pbps_model::resolver::InputManifest), CaptureError> {
    let captured = capture_identifying(connection, scope, dropped).await?;
    let ownership = ownership::classify(&captured, recorded).map_err(CaptureError::Coverage)?;
    let manifest = captured
        .seal_with_roles(key, roles, Some(&ownership))
        .map_err(|_| CaptureError::Incomplete)?;
    Ok((captured, manifest))
}

/// Fix the key, role map and recorded identity roots at the authorized
/// scratch read. The private result exposes no raw or rekey operation.
pub async fn capture_identifying_for_plan(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
    dropped: &BTreeSet<DroppedSignature>,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    roles: &crate::resolver::authorization::RoleMap,
    recorded: &RecordedOwnership<'_>,
) -> Result<CompiledCapture, CaptureError> {
    let captured = capture_identifying(connection, scope, dropped).await?;
    let ownership = ownership::classify(&captured, recorded).map_err(CaptureError::Coverage)?;
    Ok(CompiledCapture::new(captured, key, roles, ownership))
}

/// A new owned snapshot is required on every recheck; cached rows cannot
/// answer whether candidate additions or property changes invalidated inputs.
pub async fn recapture(
    connection: &mut impl pbps_db::transport::QueryConnection,
    previous: &CapturedInputs,
) -> Result<
    (
        CapturedInputs,
        Vec<pbps_db::resolver::capture::CaptureDifference>,
    ),
    CaptureError,
> {
    let signatures = previous.dropped().keys().cloned().collect();
    let current = capture_identifying(connection, previous.scope(), &signatures).await?;
    let differences = previous.compare(&current);
    Ok((current, differences))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod body_tests;

mod baseline;

mod session;

#[cfg(test)]
mod ordering_tests;

#[cfg(test)]
mod metadata_tests;
