//! Tier 1 of #2563: a user function whose stored body provably reaches only
//! built-in code is admitted without trust or a pin.
//!
//! The existing proof (see the module docs above) refuses every function a role
//! wrote, because it could run anything. A `LANGUAGE sql` function written
//! `BEGIN ATOMIC` or `RETURN` is different in kind: PostgreSQL stores its
//! parsed body in `pg_proc.prosqlbody`, in the same node format as `conbin`, so
//! the decoder that proves a CHECK can prove the body as well, and a body that
//! calls another such function is proven by recursion. A function admitted this
//! way runs nothing but built-in code, which is what the proof already admits.
//! Everything else is refused by name with the reason: a string body (its text
//! is not a parsed tree), another language, a volatility other than IMMUTABLE,
//! `SECURITY DEFINER`, a type that is not built in, a body that is more than
//! plain expressions, a call cycle, a chain too deep, or a callee refused for
//! one of those.
//!
//! **One mechanism.** Both vetting sites (the type walk in `gather` and
//! `vet_in_transaction`) call [`expand`] on the trees they decoded. It appends
//! each admitted function's body, under the use that reaches it, to the same
//! list of trees the site was already judging, so the site's own checks (the
//! `RUNS_SQL` list, owner trust, volatility, user types) apply to the bodies
//! exactly as to the expression. The only new fact a site learns is which user
//! functions were admitted, and why each other one was not ([`Proven`]).
//!
//! **Reading.** Every function is read in the caller's transaction, a level at
//! a time (one statement per level). A missing `pg_proc` row is a failed read,
//! never a function that calls nothing. An empty body is its own answer: it
//! calls nothing and is admitted. A body the decoder cannot read is a refusal.
//!
//! **What is not here.** Nothing is pinned and nothing is trusted: whoever owns
//! an admitted function can still replace it, which the owner rule (b) of the
//! module docs already covers for every routine on the path. Functions that
//! cannot be proven are slice 2 (recorded trust).

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::Conn;

use super::{
    Failure, OperatorRow, Use, comparison_supports, is_builtin, operator_function, operator_rows,
};
use crate::resolver::capture::{BodyError, Bound, bound_body};
use crate::user_types::{Oid, need, oid_array};

/// The longest chain of user functions proven, the first called from the
/// expression. A chain deeper than this is refused, naming the function at the
/// limit: the engine's own stack limit is far above it, so this bounds only an
/// absurd catalog.
pub(super) const MAX_DEPTH: usize = 16;
/// The most user functions one expression may reach.
const MAX_FUNCTIONS: usize = 1_000;

/// The user functions a read proved built-in-only, and why it refused the rest.
#[derive(Debug, Default)]
pub(super) struct Proven {
    admitted: BTreeSet<Oid>,
    /// A sentence that continues "it": `has a string body ...`.
    refused: BTreeMap<Oid, String>,
}

impl Proven {
    /// Whether the function was proven to run only built-in code.
    pub(super) fn admits(&self, oid: Oid) -> bool {
        self.admitted.contains(&oid)
    }

    /// Why a user function was not admitted, when this read judged it.
    pub(super) fn refusal(&self, oid: Oid) -> Option<&str> {
        self.refused.get(&oid).map(String::as_str)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.admitted.is_empty()
    }

    fn absorb(&mut self, other: Proven) {
        self.admitted.extend(other.admitted);
        self.refused.extend(other.refused);
    }
}

/// A function row as far as the proof is concerned.
struct Row {
    identity: String,
    language: String,
    language_oid: Oid,
    kind: String,
    security_definer: bool,
    volatility: String,
    support: Oid,
    return_type: Oid,
    argument_types: Vec<Oid>,
    has_modes: bool,
    body: Option<String>,
}

/// What one function says by itself, before anything it calls is considered.
struct Node {
    identity: String,
    /// Why the function alone is refused, each a clause continuing "it".
    own: Vec<String>,
    bound: Option<Bound>,
    /// The user functions its body calls (operators and comparisons included).
    calls: BTreeSet<Oid>,
}

/// Appends, for each tree in `bounds`, the trees of every user function it calls
/// that is proven built-in-only, with everything those call, under the same use.
pub(super) async fn expand(
    conn: &mut Conn,
    major: u32,
    bounds: &mut Vec<(Use, Bound)>,
    proven: &mut Proven,
) -> Result<(), Failure> {
    let resolver = Resolver::read(conn, bounds.iter().map(|(_, bound)| bound)).await?;
    let mut wanted: Vec<BTreeSet<Oid>> = Vec::with_capacity(bounds.len());
    for (_, bound) in bounds.iter() {
        wanted.push(resolver.user_callees(bound)?);
    }
    let roots: BTreeSet<Oid> = wanted.iter().flatten().copied().collect();
    if roots.is_empty() {
        return Ok(());
    }
    let (found, bodies) = prove(conn, major, &roots).await?;
    let mut appended = Vec::new();
    for ((used_by, _), roots) in bounds.iter().zip(&wanted) {
        let mut reached = BTreeSet::new();
        for root in roots.iter().filter(|r| found.admits(**r)) {
            collect(*root, &bodies, &mut reached);
        }
        for oid in reached {
            if let Some((_, bound)) = bodies.get(&oid) {
                appended.push((used_by.clone(), bound.clone()));
            }
        }
    }
    bounds.extend(appended);
    proven.absorb(found);
    Ok(())
}

/// `root` and every function an admitted `root` calls, which are all admitted.
fn collect(root: Oid, bodies: &BTreeMap<Oid, (BTreeSet<Oid>, Bound)>, seen: &mut BTreeSet<Oid>) {
    if !seen.insert(root) {
        return;
    }
    if let Some((calls, _)) = bodies.get(&root) {
        for callee in calls {
            collect(*callee, bodies, seen);
        }
    }
}

/// What the catalog says about the operators and row comparisons of some
/// trees, read once for all of them: the function an operator runs, and the
/// comparison function of an operator family.
struct Resolver {
    operators: BTreeMap<Oid, OperatorRow>,
    supports: BTreeMap<(Oid, Oid), Oid>,
}

impl Resolver {
    async fn read<'a>(
        conn: &mut Conn,
        bounds: impl Iterator<Item = &'a Bound>,
    ) -> Result<Self, Failure> {
        let mut operators = BTreeSet::new();
        let mut pairs = BTreeSet::new();
        for bound in bounds {
            operators.extend(bound.operators.iter().map(|o| Oid::from(*o)));
            for (operator, family) in &bound.row_compares {
                operators.insert(Oid::from(*operator));
                pairs.insert((Oid::from(*operator), Oid::from(*family)));
            }
        }
        if operators.is_empty() {
            return Ok(Self {
                operators: BTreeMap::new(),
                supports: BTreeMap::new(),
            });
        }
        Ok(Self {
            operators: operator_rows(conn, &operators.into_iter().collect::<Vec<_>>()).await?,
            supports: comparison_supports(conn, &pairs.into_iter().collect::<Vec<_>>()).await?,
        })
    }

    /// The user functions a tree runs: its calls, the function each operator
    /// runs, and the comparison function of each row comparison. A built-in is
    /// judged by the site; only what a role wrote is read here. What the
    /// catalog does not answer is an error and never a function that is not
    /// there.
    fn user_callees(&self, bound: &Bound) -> Result<BTreeSet<Oid>, Failure> {
        let mut out: BTreeSet<Oid> = bound.routines.iter().map(|f| Oid::from(*f)).collect();
        for operator in bound.operators.iter().map(|o| Oid::from(*o)) {
            let row = self.operators.get(&operator).ok_or_else(|| {
                Failure::Uncertified(format!(
                    "the operator with OID {operator} is not in the catalog"
                ))
            })?;
            out.insert(operator_function(operator, row).map_err(Failure::Uncertified)?);
        }
        for (operator, family) in &bound.row_compares {
            let key = (Oid::from(*operator), Oid::from(*family));
            let support = self.supports.get(&key).ok_or_else(|| {
                Failure::Uncertified(format!(
                    "the operator with OID {} of a row comparison has no btree comparison \
                     function in its operator family (OID {})",
                    key.0, key.1
                ))
            })?;
            out.insert(*support);
        }
        out.retain(|oid| !is_builtin(*oid));
        Ok(out)
    }
}

/// Reads and judges `roots` and every user function they reach, level by level.
/// Returns the verdicts and, for each admitted function, what it calls and
/// what its body binds.
async fn prove(
    conn: &mut Conn,
    major: u32,
    roots: &BTreeSet<Oid>,
) -> Result<(Proven, BTreeMap<Oid, (BTreeSet<Oid>, Bound)>), Failure> {
    let mut nodes: BTreeMap<Oid, Node> = BTreeMap::new();
    let mut frontier = roots.clone();
    let mut level = 0;
    while !frontier.is_empty() {
        if nodes.len() + frontier.len() > MAX_FUNCTIONS {
            return Err(Failure::Uncertified(format!(
                "an expression reaches more than {MAX_FUNCTIONS} user functions"
            )));
        }
        let rows = read_rows(conn, &frontier).await?;
        let mut level_nodes: Vec<(Oid, Node)> = Vec::with_capacity(frontier.len());
        for oid in &frontier {
            // Absent is a failed read, not a function that calls nothing.
            let row = rows.get(oid).ok_or_else(|| {
                Failure::Uncertified(format!("the function with OID {oid} is not in the catalog"))
            })?;
            level_nodes.push((*oid, judge(row, major)));
        }
        // A function first reached after `MAX_DEPTH` calls ends a call path of
        // more than `MAX_DEPTH` functions, so it is not read further: every
        // function above it on that path is refused through it. The read level
        // only bounds the reading; admission rests on the longest call path of
        // each function (`verdict`), so naming a chain's functions directly
        // cannot hide its depth.
        if level >= MAX_DEPTH {
            for (oid, mut node) in level_nodes {
                node.own = vec![format!(
                    "is nested more than {MAX_DEPTH} deep in a chain of user functions"
                )];
                node.bound = None;
                nodes.insert(oid, node);
            }
            break;
        }
        // A function already refused for its own defect is not explored: what
        // it calls cannot admit it.
        let resolver = Resolver::read(
            conn,
            level_nodes
                .iter()
                .filter(|(_, node)| node.own.is_empty())
                .filter_map(|(_, node)| node.bound.as_ref()),
        )
        .await?;
        let mut next = BTreeSet::new();
        for (oid, mut node) in level_nodes {
            if let Some(bound) = node.bound.as_ref().filter(|_| node.own.is_empty()) {
                node.calls = resolver.user_callees(bound)?;
                next.extend(node.calls.iter().filter(|c| !nodes.contains_key(*c)));
            }
            nodes.insert(oid, node);
        }
        next.retain(|oid| !nodes.contains_key(oid));
        frontier = next;
        level += 1;
    }
    let mut state = BTreeMap::new();
    for oid in nodes.keys() {
        verdict(*oid, &nodes, &mut state, MAX_DEPTH);
    }
    let mut proven = Proven::default();
    let mut bodies = BTreeMap::new();
    for (oid, node) in &nodes {
        if state.get(oid).is_some_and(|v| v.is_admitted()) {
            proven.admitted.insert(*oid);
            if let Some(bound) = &node.bound {
                bodies.insert(*oid, (node.calls.clone(), bound.clone()));
            }
        } else {
            proven.refused.insert(
                *oid,
                format!("it {}", clause(*oid, &nodes, &state, &mut BTreeSet::new())),
            );
        }
    }
    Ok((proven, bodies))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    Visiting,
    /// Admitted, with the number of functions on its longest call path
    /// (itself included).
    Admitted(usize),
    /// Admitted in every other way, but its longest call path is this many
    /// functions, more than the bound `.1`.
    TooDeep(usize, usize),
    /// Refused for its own defect or for the first refused function it calls.
    Refused(Option<Oid>),
}

impl Verdict {
    fn is_admitted(self) -> bool {
        matches!(self, Self::Admitted(_))
    }
}

/// A function is admitted when it has no defect of its own, everything it calls
/// is admitted and its longest call path has at most `max` functions. A
/// function reached again while it is still being judged is a call cycle, and
/// refuses every function on the way back to it. Each answer follows from the
/// call graph alone, so none depends on the order the functions were read in.
fn verdict(
    oid: Oid,
    nodes: &BTreeMap<Oid, Node>,
    state: &mut BTreeMap<Oid, Verdict>,
    max: usize,
) -> Verdict {
    match state.get(&oid) {
        Some(Verdict::Visiting) => return Verdict::Refused(Some(oid)),
        Some(done) => return *done,
        None => {}
    }
    state.insert(oid, Verdict::Visiting);
    let node = &nodes[&oid];
    let result = if !node.own.is_empty() {
        Verdict::Refused(None)
    } else {
        let mut deepest = 0;
        let mut refused = None;
        for callee in &node.calls {
            match verdict(*callee, nodes, state, max) {
                Verdict::Admitted(depth) => deepest = deepest.max(depth),
                Verdict::Visiting | Verdict::TooDeep(..) | Verdict::Refused(_) => {
                    refused.get_or_insert(*callee);
                }
            }
        }
        match refused {
            Some(callee) => Verdict::Refused(Some(callee)),
            None if deepest + 1 > max => Verdict::TooDeep(deepest + 1, max),
            None => Verdict::Admitted(deepest + 1),
        }
    };
    state.insert(oid, result);
    result
}

/// What is wrong with `oid`, as a clause continuing "it", following the callee
/// that refused it. `seen` ends the walk at a call cycle.
fn clause(
    oid: Oid,
    nodes: &BTreeMap<Oid, Node>,
    state: &BTreeMap<Oid, Verdict>,
    seen: &mut BTreeSet<Oid>,
) -> String {
    seen.insert(oid);
    let node = &nodes[&oid];
    match state.get(&oid) {
        Some(Verdict::Refused(Some(callee))) if node.own.is_empty() => {
            let callee_node = &nodes[callee];
            if seen.contains(callee) {
                format!(
                    "calls `{}`, which is a call cycle back to this chain",
                    callee_node.identity
                )
            } else {
                format!(
                    "calls `{}`, which {}",
                    callee_node.identity,
                    clause(*callee, nodes, state, seen)
                )
            }
        }
        Some(Verdict::TooDeep(depth, max)) => {
            format!("starts a chain of {depth} user functions, nested more than {max} deep")
        }
        _ => node.own.join(" and "),
    }
}

/// The checks a function must pass on its own, then its body's.
fn judge(row: &Row, major: u32) -> Node {
    let mut own = Vec::new();
    let sql = row.language == "sql" && is_builtin(row.language_oid);
    if !sql {
        own.push(format!(
            "is written in the language `{}`, and only a LANGUAGE sql function with a parsed \
             body can be proven built-in",
            row.language
        ));
    }
    if row.kind != "f" {
        own.push("is not a plain function (an aggregate, window function or procedure)".to_owned());
    }
    if row.security_definer {
        own.push("is SECURITY DEFINER".to_owned());
    }
    match row.volatility.as_str() {
        "i" => {}
        "s" => own.push("is declared STABLE, not IMMUTABLE".to_owned()),
        "v" => own.push("is declared VOLATILE, not IMMUTABLE".to_owned()),
        other => own.push(format!("has the unknown volatility `{other}`")),
    }
    if row.support != 0 {
        own.push("has a planner support function".to_owned());
    }
    if row.has_modes
        || !is_builtin(row.return_type)
        || row.argument_types.iter().any(|t| !is_builtin(*t))
    {
        own.push(
            "names a type that is not built into PostgreSQL, or an OUT parameter, in its \
             signature"
                .to_owned(),
        );
    }
    let mut bound = None;
    if sql {
        match &row.body {
            None => own.push(
                "has a string body (AS '...' or AS $$...$$), which the catalog keeps as text \
                 and not as a parsed tree; only a BEGIN ATOMIC or RETURN body can be proven"
                    .to_owned(),
            ),
            Some(text) => match bound_body(text, major) {
                Ok(read) if read.types.iter().all(|t| is_builtin(Oid::from(*t))) => {
                    bound = Some(read);
                }
                Ok(_) => own.push(
                    "has a body that names a type that is not built into PostgreSQL".to_owned(),
                ),
                Err(BodyError::NotPlain(why)) => {
                    own.push(format!("has a body that is not plain: {why}"))
                }
                Err(BodyError::Unreadable(why)) => {
                    own.push(format!("has a body that could not be read: {why}"));
                }
            },
        }
    }
    Node {
        identity: row.identity.clone(),
        own,
        bound,
        calls: BTreeSet::new(),
    }
}

/// The function rows for `oids`, in one statement and so one snapshot.
async fn read_rows(conn: &mut Conn, oids: &BTreeSet<Oid>) -> Result<BTreeMap<Oid, Row>, Failure> {
    let wanted: Vec<Oid> = oids.iter().copied().collect();
    let rows = conn
        .query(&format!(
            "SELECT p.oid::pg_catalog.int8 AS oid,
                    i.identity AS identity,
                    l.lanname::pg_catalog.text AS language,
                    l.oid::pg_catalog.int8 AS language_oid,
                    p.prokind::pg_catalog.text AS kind,
                    p.prosecdef AS security_definer,
                    p.provolatile::pg_catalog.text AS volatility,
                    p.prosupport::pg_catalog.oid::pg_catalog.int8 AS support,
                    p.prorettype::pg_catalog.int8 AS return_type,
                    p.proargtypes::pg_catalog.text AS argument_types,
                    (p.proargmodes IS NOT NULL) AS has_modes,
                    p.prosqlbody::pg_catalog.text AS body
               FROM pg_catalog.pg_proc p
               JOIN pg_catalog.pg_language l ON l.oid = p.prolang
              CROSS JOIN LATERAL pg_catalog.pg_identify_object(
                        'pg_catalog.pg_proc'::pg_catalog.regclass, p.oid, 0) i
              WHERE p.oid = ANY({})",
            oid_array(&wanted)
        ))
        .await?;
    let mut out = BTreeMap::new();
    for row in &rows {
        let argument_types = need::<&str>(row, "argument_types")?
            .split_whitespace()
            .map(|t| {
                t.parse::<Oid>().map_err(|_| {
                    Failure::Uncertified(format!("an argument type `{t}` is not an OID"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        out.insert(
            need(row, "oid")?,
            Row {
                identity: need::<&str>(row, "identity")?.to_owned(),
                language: need::<&str>(row, "language")?.to_owned(),
                language_oid: need(row, "language_oid")?,
                kind: need::<&str>(row, "kind")?.to_owned(),
                security_definer: need(row, "security_definer")?,
                volatility: need::<&str>(row, "volatility")?.to_owned(),
                support: need(row, "support")?,
                return_type: need(row, "return_type")?,
                argument_types,
                has_modes: need(row, "has_modes")?,
                body: row.try_get::<&str>("body")?.map(str::to_owned),
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::capture::body_tests::{ATOMIC_BODY_18, RETURN_BODY_16, RETURN_BODY_18};

    /// A function that passes every check on its own: `LANGUAGE sql`, a plain
    /// function, IMMUTABLE, built-in types, a parsed body.
    fn plain() -> Row {
        Row {
            identity: "app.is_valid(text)".to_owned(),
            language: "sql".to_owned(),
            language_oid: 14,
            kind: "f".to_owned(),
            security_definer: false,
            volatility: "i".to_owned(),
            support: 0,
            return_type: 16,
            argument_types: vec![25],
            has_modes: false,
            body: Some(RETURN_BODY_18.to_owned()),
        }
    }

    fn defects(row: &Row, major: u32) -> Vec<String> {
        judge(row, major).own
    }

    #[test]
    fn a_function_that_passes_every_check_is_read_and_calls_nothing_yet() {
        for (text, major) in [(RETURN_BODY_16, 16), (RETURN_BODY_18, 18)] {
            let row = Row {
                body: Some(text.to_owned()),
                ..plain()
            };
            let node = judge(&row, major);
            assert!(node.own.is_empty(), "{major}: {:?}", node.own);
            assert!(node.bound.is_some(), "{major}");
            // The user callees are read from the catalog afterwards.
            assert!(node.calls.is_empty());
        }
        // A `BEGIN ATOMIC` list of statements is the same answer.
        let row = Row {
            body: Some(ATOMIC_BODY_18.to_owned()),
            ..plain()
        };
        assert!(judge(&row, 18).own.is_empty());
    }

    #[test]
    fn each_defect_of_a_function_is_named_and_none_hides_another() {
        let cases: [(&str, Row, &str); 10] = [
            (
                "a string body",
                Row {
                    body: None,
                    ..plain()
                },
                "has a string body",
            ),
            (
                "plpgsql",
                Row {
                    language: "plpgsql".to_owned(),
                    language_oid: 13_647,
                    body: None,
                    ..plain()
                },
                "language `plpgsql`",
            ),
            (
                "a language that only calls itself sql",
                Row {
                    language_oid: 16_400,
                    ..plain()
                },
                "language `sql`",
            ),
            (
                "STABLE",
                Row {
                    volatility: "s".to_owned(),
                    ..plain()
                },
                "declared STABLE, not IMMUTABLE",
            ),
            (
                "VOLATILE",
                Row {
                    volatility: "v".to_owned(),
                    ..plain()
                },
                "declared VOLATILE, not IMMUTABLE",
            ),
            (
                "SECURITY DEFINER",
                Row {
                    security_definer: true,
                    ..plain()
                },
                "SECURITY DEFINER",
            ),
            (
                "an aggregate",
                Row {
                    kind: "a".to_owned(),
                    ..plain()
                },
                "not a plain function",
            ),
            (
                "a planner support function",
                Row {
                    support: 16_500,
                    ..plain()
                },
                "planner support function",
            ),
            (
                "a user type in the signature",
                Row {
                    argument_types: vec![16_450],
                    ..plain()
                },
                "type that is not built into PostgreSQL",
            ),
            (
                "an OUT parameter",
                Row {
                    has_modes: true,
                    ..plain()
                },
                "an OUT parameter",
            ),
        ];
        for (what, row, phrase) in cases {
            let found = defects(&row, 18);
            assert!(
                found.iter().any(|d| d.contains(phrase)),
                "{what}: {found:?} lacks `{phrase}`"
            );
        }
        // Several defects at once are all reported.
        let row = Row {
            volatility: "s".to_owned(),
            security_definer: true,
            ..plain()
        };
        let found = defects(&row, 18);
        assert_eq!(found.len(), 2, "{found:?}");
        // Negative: the plain row has none.
        assert!(defects(&plain(), 18).is_empty());
    }

    #[test]
    fn a_body_the_decoder_cannot_read_or_that_is_not_plain_is_a_refusal_not_a_pass() {
        let unreadable = Row {
            body: Some("garbage".to_owned()),
            ..plain()
        };
        let found = defects(&unreadable, 18);
        assert!(found[0].contains("could not be read"), "{found:?}");
        assert!(judge(&unreadable, 18).bound.is_none());
        let reading = Row {
            body: Some(RETURN_BODY_18.replace(":rtable <>", ":rtable (1)")),
            ..plain()
        };
        let found = defects(&reading, 18);
        assert!(
            found[0].contains("not plain") && found[0].contains("`rtable`"),
            "{found:?}"
        );
        // A major the decoder was not measured on is unreadable, never "no calls".
        assert!(defects(&plain(), 17)[0].contains("could not be read"));
    }

    #[test]
    fn a_body_naming_a_user_type_is_refused_even_when_it_is_otherwise_plain() {
        // The same body with its result type rewritten to a role's type.
        let body = RETURN_BODY_18.replace(":opresulttype 23", ":opresulttype 16999");
        let row = Row {
            body: Some(body),
            ..plain()
        };
        let found = defects(&row, 18);
        assert!(
            found.iter().any(|d| d.contains("body that names a type")),
            "{found:?}"
        );
    }

    fn node(identity: &str, own: &[&str], calls: &[Oid]) -> Node {
        Node {
            identity: identity.to_owned(),
            own: own.iter().map(|d| (*d).to_owned()).collect(),
            bound: Some(Bound::default()),
            calls: calls.iter().copied().collect(),
        }
    }

    fn resolve(nodes: &BTreeMap<Oid, Node>) -> BTreeMap<Oid, Verdict> {
        let mut state = BTreeMap::new();
        for oid in nodes.keys() {
            verdict(*oid, nodes, &mut state, MAX_DEPTH);
        }
        state
    }

    #[test]
    fn a_function_is_admitted_only_when_everything_it_calls_is() {
        // 1 calls 2 and 3; 2 calls 4; 3 calls 4 (a diamond, not a cycle).
        let nodes = BTreeMap::from([
            (1, node("app.a()", &[], &[2, 3])),
            (2, node("app.b()", &[], &[4])),
            (3, node("app.c()", &[], &[4])),
            (4, node("app.d()", &[], &[])),
        ]);
        assert!(resolve(&nodes).values().all(|v| v.is_admitted()));
        // Negative: one refused leaf refuses everything above it, and nothing
        // that does not call it.
        let mut nodes = nodes;
        nodes.insert(
            4,
            node("app.d()", &["is declared STABLE, not IMMUTABLE"], &[]),
        );
        nodes.insert(5, node("app.e()", &[], &[]));
        let state = resolve(&nodes);
        for refused in [1, 2, 3, 4] {
            assert!(matches!(state[&refused], Verdict::Refused(_)), "{refused}");
        }
        assert!(state[&5].is_admitted());
        let sentence = clause(1, &nodes, &state, &mut BTreeSet::new());
        assert_eq!(
            sentence,
            "calls `app.b()`, which calls `app.d()`, which is declared STABLE, not IMMUTABLE"
        );
    }

    #[test]
    fn a_call_cycle_refuses_every_function_on_it_and_every_caller_of_it() {
        // 1 calls 2, 2 calls 3, 3 calls 2; 4 calls itself; 5 calls the cycle.
        let nodes = BTreeMap::from([
            (1, node("app.one()", &[], &[2])),
            (2, node("app.two()", &[], &[3])),
            (3, node("app.three()", &[], &[2])),
            (4, node("app.four()", &[], &[4])),
            (5, node("app.five()", &[], &[3])),
            (6, node("app.six()", &[], &[])),
        ]);
        let state = resolve(&nodes);
        for refused in [1, 2, 3, 4, 5] {
            assert!(matches!(state[&refused], Verdict::Refused(_)), "{refused}");
        }
        // Negative: a function outside every cycle is still admitted.
        assert!(state[&6].is_admitted());
        for oid in [2, 4] {
            let sentence = clause(oid, &nodes, &state, &mut BTreeSet::new());
            assert!(sentence.contains("call cycle"), "{oid}: {sentence}");
        }
        assert!(
            clause(1, &nodes, &state, &mut BTreeSet::new())
                .starts_with("calls `app.two()`, which calls `app.three()`")
        );
    }

    #[test]
    fn a_function_refused_for_its_own_defect_reports_that_and_not_its_callees() {
        let nodes = BTreeMap::from([
            (
                1,
                node(
                    "app.f()",
                    &["is SECURITY DEFINER", "has a planner support function"],
                    &[2],
                ),
            ),
            (2, node("app.g()", &[], &[])),
        ]);
        let state = resolve(&nodes);
        assert_eq!(
            clause(1, &nodes, &state, &mut BTreeSet::new()),
            "is SECURITY DEFINER and has a planner support function"
        );
        assert!(state[&2].is_admitted());
    }

    #[test]
    fn the_proof_keeps_admitted_and_refused_apart_and_merges_reads() {
        let mut first = Proven::default();
        first.admitted.insert(1);
        first.refused.insert(2, "it is SECURITY DEFINER".to_owned());
        let mut second = Proven::default();
        second.admitted.insert(3);
        assert!(!second.is_empty());
        first.absorb(second);
        assert!(first.admits(1) && first.admits(3));
        assert!(!first.admits(2));
        assert_eq!(first.refusal(2), Some("it is SECURITY DEFINER"));
        // Negative: a function that was never judged is neither admitted nor
        // given a reason.
        assert!(!first.admits(9));
        assert_eq!(first.refusal(9), None);
        assert!(Proven::default().is_empty());
    }

    #[test]
    fn what_an_admitted_function_calls_is_collected_once_through_a_diamond_and_a_cycle() {
        let body = |calls: &[Oid]| (calls.iter().copied().collect(), Bound::default());
        let bodies = BTreeMap::from([
            (1, body(&[2, 3])),
            (2, body(&[4])),
            (3, body(&[4])),
            (4, body(&[])),
            (9, body(&[9])),
        ]);
        let mut seen = BTreeSet::new();
        collect(1, &bodies, &mut seen);
        assert_eq!(seen, BTreeSet::from([1, 2, 3, 4]));
        // Negative: a function that is not reached is not collected, and a
        // function that calls itself does not loop.
        let mut seen = BTreeSet::new();
        collect(9, &bodies, &mut seen);
        assert_eq!(seen, BTreeSet::from([9]));
    }

    /// The deepest call path of `chain` functions: 1 calls 2 ... calls `chain`.
    fn chain(chain: Oid) -> BTreeMap<Oid, Node> {
        (1..=chain)
            .map(|oid| {
                let calls: Vec<Oid> = if oid < chain { vec![oid + 1] } else { vec![] };
                (oid, node(&format!("app.f{oid}()"), &[], &calls))
            })
            .collect()
    }

    fn resolve_to(nodes: &BTreeMap<Oid, Node>, max: usize) -> BTreeMap<Oid, Verdict> {
        let mut state = BTreeMap::new();
        for oid in nodes.keys() {
            verdict(*oid, nodes, &mut state, max);
        }
        state
    }

    #[test]
    fn the_depth_is_the_longest_call_path_and_not_the_shortest_or_the_read_level() {
        // A chain of exactly the bound is admitted from its first function.
        let state = resolve_to(&chain(3), 3);
        assert_eq!(state[&1], Verdict::Admitted(3));
        assert_eq!(state[&3], Verdict::Admitted(1));
        // One more function and the first is refused, and only the first:
        // every other function of it is still a chain within the bound.
        let state = resolve_to(&chain(4), 3);
        assert_eq!(state[&1], Verdict::TooDeep(4, 3));
        for oid in [2, 3, 4] {
            assert!(state[&oid].is_admitted(), "{oid}");
        }
        let nodes = chain(4);
        assert_eq!(
            clause(1, &nodes, &state, &mut BTreeSet::new()),
            "starts a chain of 4 user functions, nested more than 3 deep"
        );
        // Two paths of different lengths to the same function: `top` calls
        // `leaf` directly (2 functions) and through `m1` and `m2` (4). Only the
        // longer one counts, whichever is found first.
        let nodes = BTreeMap::from([
            (1, node("app.top()", &[], &[4, 2])),
            (2, node("app.m1()", &[], &[3])),
            (3, node("app.m2()", &[], &[4])),
            (4, node("app.leaf()", &[], &[])),
        ]);
        assert_eq!(resolve_to(&nodes, 4)[&1], Verdict::Admitted(4));
        assert_eq!(resolve_to(&nodes, 3)[&1], Verdict::TooDeep(4, 3));
        // A function above a too-deep one is refused by naming it.
        let nodes = BTreeMap::from([
            (5, node("app.above()", &[], &[1])),
            (1, node("app.top()", &[], &[4, 2])),
            (2, node("app.m1()", &[], &[3])),
            (3, node("app.m2()", &[], &[4])),
            (4, node("app.leaf()", &[], &[])),
        ]);
        let state = resolve_to(&nodes, 3);
        assert!(matches!(state[&5], Verdict::Refused(Some(1))));
        assert_eq!(
            clause(5, &nodes, &state, &mut BTreeSet::new()),
            "calls `app.top()`, which starts a chain of 4 user functions, nested more than 3 deep"
        );
        // Negative: the order of the functions does not change any answer.
        let reordered: BTreeMap<Oid, Node> = nodes
            .iter()
            .map(|(oid, n)| {
                (
                    100 - oid,
                    Node {
                        identity: n.identity.clone(),
                        own: n.own.clone(),
                        bound: None,
                        calls: n.calls.iter().map(|c| 100 - c).collect(),
                    },
                )
            })
            .collect();
        let again = resolve_to(&reordered, 3);
        for (oid, verdict) in &state {
            assert_eq!(
                std::mem::discriminant(verdict),
                std::mem::discriminant(&again[&(100 - oid)]),
                "{oid}"
            );
        }
    }
}
