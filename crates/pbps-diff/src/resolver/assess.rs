//! Lightweight assessment (ADR-0016 §1, DEC-1515.1): which creation-time
//! binding questions a connected plan raises, answered from the typed plan
//! alone where that is sound, and per kept surface from the dependencies the
//! engine recorded where the caller read them (DEC-2987.1). No SQL is parsed
//! and no catalog is asked here: the facts arrive as data.

use super::prepare::{forward, invalidates, surfaces};
use pbps_model::module::Lexis;
use pbps_model::part_effects::PartEffect;
use pbps_model::resolver::{
    Ancestor, CandidateName, CatalogName, DependencyEdge, DependencyFacts, EngineNamedRelation,
    NameAvailability, NameSlotKind, ReadFact, Surface, SurfaceDependencies,
};
use pbps_model::{
    Change, ChangeSet, ColumnChange, ConstraintChange, ExtensionChange, GrantChange, GrantTarget,
    IdsIndex, IndexChange, ModuleChange, ModuleId, ObjectName, OwnedSequence, PartitionChange,
    PrimaryKey, Schema, Table, TableChange, TableName, TypeChange, TypeValueChange, ValueSource,
};
use std::collections::{BTreeMap, BTreeSet};

/// One question's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Answer {
    /// Nothing in the plan can move what this surface binds.
    Unaffected,
    /// The plan already recreates this surface from its declaration, so its
    /// creation binds afresh whatever the answer would have been.
    Rebuild,
    /// Only an engine can say whether a fresh creation binds differently.
    Resolve,
}

/// Why a kept surface was answered [`Answer::Unaffected`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnaffectedReason {
    /// The plan changes nothing a name lookup could reach (DEC-1515.1).
    NoNameMoved,
    /// The plan moves names, and the dependencies the engine recorded when it
    /// created this surface show that none of them is one it bound or could
    /// bind instead (DEC-2987.1).
    ByDependency,
    /// The engine did not bind this surface's body when it created it, so
    /// there is no creation-time binding to move. What the body finds when it
    /// runs is not this question, and is reported apart (DEC-2987.1).
    Unparsed,
}

/// Every surface the target holds that the plan keeps, by its declared
/// spelling, with its answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assessment {
    pub questions: BTreeMap<Surface, Answer>,
    unaffected_reasons: BTreeMap<Surface, UnaffectedReason>,
}

impl Assessment {
    #[must_use]
    pub fn requires_resolution(&self) -> bool {
        self.questions.values().any(|a| *a == Answer::Resolve)
    }

    #[must_use]
    pub fn count(&self, answer: Answer) -> usize {
        self.questions.values().filter(|a| **a == answer).count()
    }

    #[must_use]
    pub fn unaffected_reason(&self, surface: &Surface) -> Option<UnaffectedReason> {
        self.unaffected_reasons.get(surface).copied()
    }

    #[must_use]
    pub fn count_unaffected(&self, reason: UnaffectedReason) -> usize {
        self.unaffected_reasons
            .values()
            .filter(|r| **r == reason)
            .count()
    }

    /// The surfaces only an engine can answer, in order.
    pub fn unresolved(&self) -> impl Iterator<Item = &Surface> {
        self.questions
            .iter()
            .filter(|(_, a)| **a == Answer::Resolve)
            .map(|(s, _)| s)
    }
}

/// Answer each question `ordinary` raises for `base -> desired` (DEC-1515.1),
/// from the typed plan alone.
///
/// Deliberately coarse. A surface is unaffected only when the plan changes
/// nothing a name lookup could reach anywhere; any such change sends every
/// surface the plan does not already rebuild to the engine. Narrowing that by
/// path or by name alone would need the reasoning ADR-0016 leaves to the
/// engine: a qualified call, an overload, a column-notation reference or a
/// relation-namespace arrival each defeats a guess, and a wrong narrowing is
/// a silent wrong answer. [`assess_with_facts`] narrows it only from what the
/// engine itself recorded.
#[must_use]
pub fn assess(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    ordinary: &ChangeSet,
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
) -> Assessment {
    assess_with_facts(base, desired, ordinary, dialect, None)
}

/// What connected catalog facts [`assess_with_facts`] would consume for a plan.
///
/// Returned by [`needed_facts`], computed by the same classification pass as
/// [`assess_with_facts`]. `None` means no surface would be narrowed, so reading
/// the catalog costs no query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactsNeeded {
    /// Kept surfaces in their base (target) spelling that are neither rebuilt
    /// nor answered without facts.
    pub surfaces: BTreeSet<Surface>,
    /// Unqualified names arriving in the plan to test against the system
    /// catalog (including first-choice names of arriving engine-named
    /// relations and companion types).
    pub arriving_names: BTreeSet<String>,
    /// Touched engine-named relations whose actual catalog names must be read.
    /// The boolean is `true` when the owning table moves across schemas, so
    /// the actual relation name also arrives in the destination schema and
    /// must be checked against the system catalog.
    pub touched_engine_named: BTreeMap<EngineNamedRelation, bool>,
    /// Candidate first-choice names for arriving engine-named relations and
    /// companion types, each to be checked free in its schema.
    pub arriving_candidates: BTreeSet<CandidateName>,
}

/// What connected catalog facts [`assess_with_facts`] would consume for
/// `ordinary` on `base -> desired`, or `None` when no kept surface would be
/// narrowed (so callers issue no catalog query).
///
/// Computed by the same classification pass as [`assess_with_facts`].
#[must_use]
pub fn needed_facts(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    ordinary: &ChangeSet,
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
) -> Option<FactsNeeded> {
    with_dialect(dialect, |engine, hooks| {
        let classified = classify(base, desired, ordinary, engine, hooks);
        if !classified.effects.moves_bindings || classified.effects.exclude_narrowing {
            return None;
        }
        let surfaces: BTreeSet<Surface> = classified
            .kept
            .into_iter()
            .filter_map(|(surface, _, rebuilt)| (!rebuilt).then_some(surface))
            .collect();
        if surfaces.is_empty() {
            return None;
        }
        Some(FactsNeeded {
            surfaces,
            arriving_names: classified.effects.arriving_names,
            touched_engine_named: classified
                .effects
                .touched_engine_named
                .into_iter()
                .map(|(k, dest)| (k, dest.is_some()))
                .collect(),
            arriving_candidates: classified.effects.arriving_candidates,
        })
    })
}

/// [`assess`], narrowed per kept surface by the dependencies the engine
/// recorded (DEC-2987.1). With no facts every answer is [`assess`]'s.
///
/// A kept surface of a plan that moves names is unaffected only when all of
/// these hold; anything else, and anything unreadable, stays a question:
///
/// - the plan is of no class the facts do not describe (partitions, a type
///   rebuild, a package, a computed column, a schema's visibility, an unknown
///   engine-chosen name);
/// - none of its recorded edges points at an object the plan touches, a
///   column of one, or a descendant of one;
/// - no arriving name is the name of anything it has an edge on, nor one the
///   engine's own catalog holds, since an arrival earlier on the path would
///   be found first;
/// - its text mentions no arriving relation or type, which covers the forms
///   that bind and leave no edge.
#[must_use]
pub fn assess_with_facts(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    ordinary: &ChangeSet,
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
    facts: Option<&DependencyFacts>,
) -> Assessment {
    with_dialect(dialect, |engine, hooks| {
        assessed_with_facts(base, desired, ordinary, engine, hooks, facts)
    })
}

fn with_dialect<R>(
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
    run: impl FnOnce(&Engine<'_>, &Hooks<'_>) -> R,
) -> R {
    let lexicon = dialect.lexicon();
    let code_only = |definition: &str| dialect.code_only(definition);
    let lexis = Lexis {
        code_only: &code_only,
        quoted_identifiers: lexicon.quoted_identifiers,
        continues_ident: lexicon.identifier_continues,
        reserved: lexicon.reserved,
        bare_rank: &pbps_model::module::every_schema,
    };
    let normalize = |definition: &str| dialect.normalize_definition(definition);
    // By name, not by count: a relation of another kind must not stand in
    // for one of these.
    let extra_implicit_relations = |name: &TableName, table: &Table| {
        let unnamed_key = table
            .primary_key
            .as_ref()
            .filter(|pk| pk.name.is_none())
            .and_then(|_| dialect.unnamed_primary_key_name(name));
        let handled: BTreeSet<String> = table
            .columns
            .iter()
            .filter(|(_, c)| matches!(c.value_source, ValueSource::Identity(_)))
            .filter_map(|(column, _)| dialect.identity_sequence_name(name, column.raw()))
            .chain(unnamed_key)
            .collect();
        dialect
            .implicit_relation_names(name, table)
            .iter()
            .any(|implicit| !handled.contains(&implicit.name))
    };
    let unnamed_pk_name = |table: &TableName| dialect.unnamed_primary_key_name(table);
    let identity_seq_name =
        |table: &TableName, col: &str| dialect.identity_sequence_name(table, col);
    let owned_seq_name = |table: &TableName, col: &str, seq: &OwnedSequence| {
        dialect.owned_sequence_name(table, col, seq)
    };
    let companion_types = |name: &ObjectName| dialect.companion_type_names(name);
    let engine = Engine {
        indexes_are_relations: dialect.indexes_share_namespace_with_tables(),
        normalize: &normalize,
        filter_binds: dialect.filter_binds_expression(),
    };
    let hooks = Hooks {
        lexis: &lexis,
        extra_implicit_relations: &extra_implicit_relations,
        unnamed_pk_name: &unnamed_pk_name,
        identity_seq_name: &identity_seq_name,
        owned_seq_name: &owned_seq_name,
        companion_types: &companion_types,
    };
    run(&engine, &hooks)
}

/// The engine facts the assessment reads.
struct Engine<'a> {
    /// Whether an index's name is a relation's (DECISIONS 453).
    indexes_are_relations: bool,
    /// The comparison form of a module definition, as the differ compares
    /// one (SPEC §8.2).
    normalize: &'a dyn Fn(&str) -> String,
    /// Whether an index filter binds at creation (SQL Server's does not).
    filter_binds: bool,
}

/// What the narrowing asks the dialect: none of it is read without facts.
struct Hooks<'a> {
    /// The scanner the rebuild candidates are found with; the text test must
    /// read a body as that does.
    lexis: &'a Lexis<'a>,
    /// Whether the engine generates any implicit relation for `table` beyond
    /// an unnamed primary key and identity columns (which the facts cover).
    extra_implicit_relations: &'a dyn Fn(&TableName, &Table) -> bool,
    /// First-choice name for an unnamed primary key's backing index.
    unnamed_pk_name: &'a dyn Fn(&TableName) -> Option<String>,
    /// First-choice name for an identity column's sequence.
    identity_seq_name: &'a dyn Fn(&TableName, &str) -> Option<String>,
    /// First-choice name for an unnamed owned sequence.
    owned_seq_name: &'a dyn Fn(&TableName, &str, &OwnedSequence) -> Option<String>,
    /// First-choice companion type names for an arriving relation or type.
    companion_types: &'a dyn Fn(&ObjectName) -> Vec<ObjectName>,
}

/// [`assess`], with the engine facts it reads.
#[cfg(test)]
fn assessed_with(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    ordinary: &ChangeSet,
    engine: &Engine<'_>,
) -> Assessment {
    assessed_with_facts(
        base,
        desired,
        ordinary,
        engine,
        &Hooks {
            lexis: &pbps_model::module::SHARED,
            extra_implicit_relations: &|_, _| false,
            unnamed_pk_name: &|_| None,
            identity_seq_name: &|_, _| None,
            owned_seq_name: &|_, _, _| None,
            companion_types: &|_| Vec::new(),
        },
        None,
    )
}

struct Classified {
    effects: PlanEffects,
    /// `(base_surface, kept_surface, rebuilt)` for every kept surface.
    kept: Vec<(Surface, Surface, bool)>,
}

fn classify(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    ordinary: &ChangeSet,
    engine: &Engine<'_>,
    hooks: &Hooks<'_>,
) -> Classified {
    let after = surfaces(desired.schema, engine.filter_binds);
    let parts: Vec<_> = ordinary
        .changes
        .iter()
        .map(|p| super::part_effects(&p.change))
        .collect();
    let effects = PlanEffects::of(ordinary, base, desired, engine, hooks);
    // Asked once per base surface: indexed once (#1870).
    let base_names = base.ids.index();
    let desired_names = desired.ids.index();
    let mut kept_list = Vec::new();
    for surface in surfaces(base.schema, engine.filter_binds) {
        let kept = forward(&surface, &base_names, desired.ids);
        if !after.contains(&kept) {
            continue;
        }
        let rebuilt = replaced(&surface, &kept, &base_names, &desired_names)
            || ordinary.changes.iter().zip(&parts).any(|(p, facts)| {
                recreates(&p.change, facts, &surface, &kept, base, desired, engine)
            });
        kept_list.push((surface, kept, rebuilt));
    }
    Classified {
        effects,
        kept: kept_list,
    }
}

fn assessed_with_facts(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    ordinary: &ChangeSet,
    engine: &Engine<'_>,
    hooks: &Hooks<'_>,
    facts: Option<&DependencyFacts>,
) -> Assessment {
    let classified = classify(base, desired, ordinary, engine, hooks);
    let narrowing = facts.and_then(|f| Narrowing::prepare(f, &classified.effects));
    let mut questions = BTreeMap::new();
    let mut unaffected_reasons = BTreeMap::new();
    for (surface, kept, rebuilt) in classified.kept {
        let (answer, reason) = if rebuilt {
            (Answer::Rebuild, None)
        } else if !classified.effects.moves_bindings {
            (Answer::Unaffected, Some(UnaffectedReason::NoNameMoved))
        } else if classified.effects.exclude_narrowing {
            (Answer::Resolve, None)
        } else if let Some(why) = narrowing.as_ref().and_then(|n| {
            n.prove(
                &surface,
                &kept,
                desired.schema,
                engine.filter_binds,
                hooks.lexis,
            )
        }) {
            (Answer::Unaffected, Some(why))
        } else {
            (Answer::Resolve, None)
        };
        if let Some(why) = reason {
            unaffected_reasons.insert(kept.clone(), why);
        }
        questions.insert(kept, answer);
    }
    Assessment {
        questions,
        unaffected_reasons,
    }
}

/// The facts of one plan, once they are known to be usable for it at all.
struct Narrowing<'a> {
    facts: &'a DependencyFacts,
    effects: &'a PlanEffects,
    expanded_relations: BTreeSet<ObjectName>,
    arriving_names: BTreeSet<String>,
    arriving_relations_and_types: BTreeSet<ObjectName>,
}

impl<'a> Narrowing<'a> {
    /// `None` when no surface of this plan can be proven: the plan is excluded
    /// (including any within-plan candidate collision), the ancestor ties were
    /// not read, an arriving candidate name is held or unchecked, a touched
    /// engine-named relation's actual name is unknown or collides on move with
    /// an arriving candidate, or an arriving name is held by the engine's own
    /// catalog (or was not asked).
    fn prepare(facts: &'a DependencyFacts, effects: &'a PlanEffects) -> Option<Self> {
        if effects.exclude_narrowing {
            return None;
        }
        let ReadFact::Known(ancestors) = &facts.relation_ancestors else {
            return None;
        };
        if !effects
            .arriving_candidates
            .iter()
            .all(|c| matches!(facts.candidate_names.get(c), Some(NameAvailability::Free)))
        {
            return None;
        }
        let mut touched_relations = effects.touched_relations.clone();
        let mut arriving_names = effects.arriving_names.clone();
        let mut arriving_relations_and_types = effects.arriving_relations_and_types.clone();
        for (enr, move_to) in &effects.touched_engine_named {
            let Some(ReadFact::Known(actual)) = facts.engine_named_relations.get(enr) else {
                return None;
            };
            touched_relations.insert(actual.clone());
            if let Some(dest_schema) = move_to {
                let moved = ObjectName::new(dest_schema, &actual.name);
                // A moved engine-named relation landing on another arrival's
                // predicted first-choice name forces that arrival onto a
                // fallback the pre-plan catalog read could not see.
                if effects.arriving_candidates.iter().any(|c| c.name == moved) {
                    return None;
                }
                arriving_names.insert(actual.name.clone());
                arriving_relations_and_types.insert(moved);
            }
        }
        if !arriving_names
            .iter()
            .all(|n| matches!(facts.catalog_names.get(n), Some(CatalogName::Clear)))
        {
            return None;
        }
        let expanded_relations =
            expand_touched_relations(&touched_relations, &effects.touched_types, ancestors);
        Some(Self {
            facts,
            effects,
            expanded_relations,
            arriving_names,
            arriving_relations_and_types,
        })
    }

    /// Why this kept surface is unaffected, or `None` when it stays a
    /// question. An absent surface is not covered, which is not "no edges".
    fn prove(
        &self,
        base_surface: &Surface,
        kept_surface: &Surface,
        schema: &Schema,
        filter_binds: bool,
        lexis: &Lexis<'_>,
    ) -> Option<UnaffectedReason> {
        // An unparsed surface's edges are its header's, bound at creation
        // whatever the body's language, and are checked as a parsed one's.
        let (edges, reason) = match self.facts.surfaces.get(base_surface)? {
            SurfaceDependencies::Unreadable => return None,
            SurfaceDependencies::Unparsed(edges) => (edges, UnaffectedReason::Unparsed),
            SurfaceDependencies::Parsed(edges) => (edges, UnaffectedReason::ByDependency),
        };
        let touched = edges.iter().any(|e| match e {
            DependencyEdge::Relation(r) => self.expanded_relations.contains(r),
            DependencyEdge::Column(c) => {
                self.expanded_relations.contains(&c.table)
                    || self.effects.touched_types.contains(&c.table)
            }
            DependencyEdge::Routine(r) => self.effects.touched_routines.contains(r),
            DependencyEdge::Type(t) => {
                self.effects.touched_types.contains(t) || self.expanded_relations.contains(t)
            }
        });
        let found_first = self
            .arriving_names
            .iter()
            .any(|n| edges.iter().any(|e| e.matches_arriving_name(n)));
        if touched || found_first {
            return None;
        }
        // Asked of an unparsed surface too: its header is part of the text.
        if !self.arriving_relations_and_types.is_empty() {
            let texts = surface_texts(kept_surface, schema, filter_binds)?;
            if self.arriving_relations_and_types.iter().any(|a| {
                texts
                    .iter()
                    .any(|t| pbps_model::module::references_with(t, a, lexis))
            }) {
                return None;
            }
        }
        Some(reason)
    }
}

/// Transitive closure of touched relations through ancestor ties (cycle-safe).
fn expand_touched_relations(
    touched_relations: &BTreeSet<ObjectName>,
    touched_types: &BTreeSet<ObjectName>,
    ancestors: &BTreeMap<ObjectName, BTreeSet<Ancestor>>,
) -> BTreeSet<ObjectName> {
    let mut expanded = touched_relations.clone();
    loop {
        let before = expanded.len();
        for (descendant, parents) in ancestors {
            if !expanded.contains(descendant)
                && parents.iter().any(|a| match a {
                    Ancestor::Relation(r) => expanded.contains(r),
                    Ancestor::Type(t) => touched_types.contains(t) || expanded.contains(t),
                })
            {
                expanded.insert(descendant.clone());
            }
        }
        if expanded.len() == before {
            return expanded;
        }
    }
}

/// Expression texts of a kept surface in `schema`, or `None` if absent.
fn surface_texts<'a>(
    surface: &Surface,
    schema: &'a Schema,
    filter_binds: bool,
) -> Option<Vec<&'a str>> {
    match surface {
        Surface::Module(id) => schema.modules.get(id).map(|m| vec![m.definition.as_str()]),
        Surface::Default(col) => {
            let table = schema.tables.get(&col.table)?;
            if let Some(column) = table.columns.get(&col.name) {
                match &column.value_source {
                    ValueSource::Default(e) => return Some(vec![e.as_str()]),
                    ValueSource::Generated(g) => return Some(vec![g.expression.as_str()]),
                    ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => {}
                }
            }
            table
                .partition_of
                .as_ref()
                .and_then(|p| p.columns.get(&col.name)?.default.as_deref())
                .or_else(|| table.computed.get(&col.name).map(|c| c.expression.as_str()))
                .map(|e| vec![e])
        }
        Surface::Check { table, name } => schema
            .tables
            .get(table)?
            .checks
            .get(name)
            .map(|c| vec![c.expression.as_str()]),
        Surface::Index { table, name } => {
            let index = schema.tables.get(table)?.indexes.get(name)?;
            let mut texts: Vec<&str> = index
                .columns
                .iter()
                .filter_map(|c| c.key.expression())
                .collect();
            if filter_binds && let Some(filter) = index.filter.as_deref() {
                texts.push(filter);
            }
            (!texts.is_empty()).then_some(texts)
        }
        Surface::Namespace(_) | Surface::Table(_) | Surface::Column(_) | Surface::Type(_) => None,
    }
}

/// Whether the surface found under the same spelling belongs to another table
/// or column: one the plan drops and creates again under a new UID, as when
/// an environment skipped the revision that dropped the old one. Its new
/// surface is created from the declaration, so it is a rebuild, not a kept
/// surface (#1526 review). Only recorded UIDs that differ prove it; a UID
/// either side lacks proves nothing, and the surface stays a question.
fn replaced(surface: &Surface, kept: &Surface, from: &IdsIndex<'_>, to: &IdsIndex<'_>) -> bool {
    let differ = |a: Option<&pbps_model::Uid>, b: Option<&pbps_model::Uid>| matches!((a, b), (Some(a), Some(b)) if a != b);
    match (surface, kept) {
        (Surface::Default(before), Surface::Default(after)) => {
            differ(from.column_uid(before), to.column_uid(after))
        }
        (Surface::Check { table: before, .. }, Surface::Check { table: after, .. })
        | (Surface::Index { table: before, .. }, Surface::Index { table: after, .. }) => {
            differ(from.table_uid(before), to.table_uid(after))
        }
        _ => false,
    }
}

/// Whether this change recreates the surface because its declaration says
/// so. A module rebuilt with an unchanged declaration is ADR-0013's candidate
/// rebuild, which is the question itself, not its answer.
fn recreates(
    change: &Change,
    parts: &[PartEffect<'_>],
    surface: &Surface,
    kept: &Surface,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    engine: &Engine<'_>,
) -> bool {
    if let (Change::Module(ModuleChange::AlterModule { id, .. }), Surface::Module(m)) =
        (change, kept)
    {
        return id == m && declared_differently(id, base, desired, engine);
    }
    // A change names a surface by whichever spelling it acts under; a kept
    // surface has one of each only across a rename.
    invalidates(parts, surface) || invalidates(parts, kept)
}

/// Whether the declaration of a module differs, as the differ decides it:
/// after the dialect's normalization. A layout-only edit is no change there,
/// so the rebuild an arrival makes of that module is still the candidate
/// one, a question for the engine (#1526 review).
fn declared_differently(
    id: &pbps_model::ModuleId,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    engine: &Engine<'_>,
) -> bool {
    match (base.schema.modules.get(id), desired.schema.modules.get(id)) {
        (Some(was), Some(now)) => {
            was.kind != now.kind
                || (engine.normalize)(&was.definition) != (engine.normalize)(&now.definition)
        }
        _ => true,
    }
}

/// Everything the assessment needs from a plan's changes, derived in one
/// exhaustive pass so binding movement, excluded classes, touched objects, and
/// arriving names can never disagree.
#[derive(Default)]
struct PlanEffects {
    moves_bindings: bool,
    exclude_narrowing: bool,
    touched_relations: BTreeSet<ObjectName>,
    touched_routines: BTreeSet<ObjectName>,
    touched_types: BTreeSet<ObjectName>,
    arriving_names: BTreeSet<String>,
    arriving_relations_and_types: BTreeSet<ObjectName>,
    /// Explicitly arriving relations and types, checked against candidate
    /// first-choice names for within-plan collisions.
    explicit_arrivals: BTreeSet<ObjectName>,
    /// Explicitly arriving constraint names in their schema, checked against
    /// constraint-backed engine-named relation candidates.
    arriving_constraints: BTreeSet<ObjectName>,
    /// Touched engine-named relations whose actual catalog names must be read.
    /// Value is `Some(dest_schema)` when a cross-schema `RenameTable` moves
    /// the relation into `dest_schema` under its existing catalog name.
    touched_engine_named: BTreeMap<EngineNamedRelation, Option<String>>,
    /// Candidate first-choice names for arriving engine-named relations and
    /// companion types.
    arriving_candidates: BTreeSet<CandidateName>,
}

#[derive(Clone, Copy)]
enum TableRelationMode<'a> {
    Arriving,
    Touched,
    MovingTo(&'a str),
}

/// Whether an index or a key on `table` is copied to other tables, or is such
/// a copy: the engine makes one per partition under a name it chooses, and
/// drops them with the parent's.
fn copies_indexes(table: &TableName, base: crate::Side<'_>, desired: crate::Side<'_>) -> bool {
    [base, desired].iter().any(|side| {
        side.schema
            .tables
            .get(table)
            .is_some_and(|t| t.partition_by.is_some() || t.partition_of.is_some())
    })
}

fn is_excluded_table(table: &Table) -> bool {
    table.partition_by.is_some() || table.partition_of.is_some() || !table.computed.is_empty()
}

impl PlanEffects {
    fn of(
        ordinary: &ChangeSet,
        base: crate::Side<'_>,
        desired: crate::Side<'_>,
        engine: &Engine<'_>,
        hooks: &Hooks<'_>,
    ) -> Self {
        let mut effects = Self::default();
        for planned in &ordinary.changes {
            effects.record(&planned.change, base, desired, engine, hooks);
        }
        // A first choice another arrival of the same plan also claims (an
        // explicit relation or type, or a constraint for a constraint-backed
        // engine-named relation) forces the engine onto a fallback name nothing
        // here knows. The catalog cannot say so: neither exists yet.
        if effects.arriving_candidates.iter().any(|c| {
            effects.explicit_arrivals.contains(&c.name)
                || (c.kind == NameSlotKind::ConstraintRelation
                    && effects.arriving_constraints.contains(&c.name))
        }) {
            effects.exclude_narrowing = true;
        }
        effects
    }

    fn arrive_rel_or_type(&mut self, name: ObjectName) {
        self.arriving_names.insert(name.name.clone());
        self.arriving_relations_and_types.insert(name.clone());
        self.explicit_arrivals.insert(name);
    }

    fn arrive_with_companions(&mut self, name: ObjectName, hooks: &Hooks<'_>) {
        let companions = (hooks.companion_types)(&name);
        self.arrive_rel_or_type(name);
        for comp in companions {
            self.add_candidate(CandidateName {
                kind: NameSlotKind::CompanionType,
                name: comp,
            });
        }
    }

    fn add_candidate(&mut self, candidate: CandidateName) {
        // Two arrivals of one plan wanting the same first-choice name (two
        // long names clipped alike, or an engine-named relation and a
        // companion type sharing a name) go to one of them and the other takes
        // a fallback name nothing here knows. The catalog cannot say so:
        // neither exists yet.
        if self
            .arriving_candidates
            .iter()
            .any(|existing| existing.name == candidate.name)
        {
            self.exclude_narrowing = true;
        }
        self.arriving_names.insert(candidate.name.name.clone());
        self.arriving_relations_and_types
            .insert(candidate.name.clone());
        self.arriving_candidates.insert(candidate);
    }

    fn touch_module(&mut self, id: &ModuleId) {
        match id {
            ModuleId::Named(obj) => {
                self.touched_relations.insert(obj.clone());
            }
            ModuleId::Routine(r) => {
                self.touched_routines.insert(r.name.clone());
            }
            ModuleId::Trigger { .. } => {}
        }
    }

    fn record_table_relations(
        &mut self,
        table_name: &TableName,
        table: &Table,
        hooks: &Hooks<'_>,
        mode: TableRelationMode<'_>,
    ) {
        let names = table
            .indexes
            .keys()
            .chain(table.unique.keys())
            .chain(table.exclude.keys())
            .chain(table.primary_key.as_ref().and_then(|pk| pk.name.as_ref()))
            .map(|n| n.raw().to_owned());
        for name in names {
            match mode {
                TableRelationMode::Arriving => {
                    self.arrive_rel_or_type(ObjectName::new(&table_name.schema, name));
                }
                TableRelationMode::Touched => {
                    self.touched_relations
                        .insert(ObjectName::new(&table_name.schema, name));
                }
                TableRelationMode::MovingTo(dest_schema) => {
                    self.touched_relations
                        .insert(ObjectName::new(&table_name.schema, &name));
                    self.arrive_rel_or_type(ObjectName::new(dest_schema, name));
                }
            }
        }
        if let TableRelationMode::Arriving | TableRelationMode::MovingTo(_) = mode {
            let dest_schema = match mode {
                TableRelationMode::MovingTo(s) => s,
                TableRelationMode::Arriving | TableRelationMode::Touched => &table_name.schema,
            };
            for cname in table
                .checks
                .keys()
                .chain(table.foreign_keys.keys())
                .chain(table.unique.keys())
                .chain(table.exclude.keys())
                .chain(table.primary_key.as_ref().and_then(|pk| pk.name.as_ref()))
            {
                self.arriving_constraints
                    .insert(ObjectName::new(dest_schema, cname.raw()));
            }
        }
        if table
            .primary_key
            .as_ref()
            .is_some_and(|pk| pk.name.is_none())
        {
            match mode {
                TableRelationMode::Arriving => self.arrive_unnamed_pk(table_name, hooks),
                TableRelationMode::Touched => {
                    self.touched_engine_named
                        .insert(EngineNamedRelation::PrimaryKey(table_name.clone()), None);
                }
                TableRelationMode::MovingTo(dest_schema) => {
                    self.touched_engine_named.insert(
                        EngineNamedRelation::PrimaryKey(table_name.clone()),
                        Some(dest_schema.to_owned()),
                    );
                }
            }
        }
        self.exclude_narrowing |= (hooks.extra_implicit_relations)(table_name, table);
        // A sequence a column owns is a relation the table brings or takes
        // with it.
        for (col_name, spec) in &table.columns {
            self.record_column_sequence(table_name, col_name.raw(), spec, hooks, mode);
        }
    }

    fn arrive_unnamed_pk(&mut self, table: &TableName, hooks: &Hooks<'_>) {
        match (hooks.unnamed_pk_name)(table) {
            Some(first) => self.add_candidate(CandidateName {
                kind: NameSlotKind::ConstraintRelation,
                name: ObjectName::new(&table.schema, first),
            }),
            None => self.exclude_narrowing = true,
        }
    }

    fn record_pk(
        &mut self,
        table: &TableName,
        pk: Option<&PrimaryKey>,
        hooks: &Hooks<'_>,
        arriving: bool,
    ) {
        match pk.map(|p| &p.name) {
            Some(Some(n)) if arriving => {
                let rel = ObjectName::new(&table.schema, n.raw());
                self.arriving_constraints.insert(rel.clone());
                self.arrive_rel_or_type(rel);
            }
            Some(Some(n)) => {
                self.touched_relations
                    .insert(ObjectName::new(&table.schema, n.raw()));
            }
            Some(None) if arriving => self.arrive_unnamed_pk(table, hooks),
            Some(None) => {
                self.touched_engine_named
                    .insert(EngineNamedRelation::PrimaryKey(table.clone()), None);
            }
            None => {}
        }
    }

    /// A sequence the column owns under a declared name is touched or arrives
    /// with it. One the engine names (an identity column's, or a sequence
    /// declared without a name) is resolved from catalog facts: its actual
    /// name when touched, and its first-choice availability when arriving.
    /// Sequences have no row-type array companion (measured on PostgreSQL 16
    /// and 18), so none brings a companion type.
    fn record_column_sequence(
        &mut self,
        table: &TableName,
        col_name: &str,
        column: &pbps_model::Column,
        hooks: &Hooks<'_>,
        mode: TableRelationMode<'_>,
    ) {
        let first_choice = match &column.value_source {
            ValueSource::Sequence(OwnedSequence {
                name: Some(declared),
                ..
            }) => {
                let sequence = ObjectName::new(&table.schema, declared);
                match mode {
                    TableRelationMode::Arriving => self.arrive_rel_or_type(sequence),
                    TableRelationMode::Touched => {
                        self.touched_relations.insert(sequence);
                    }
                    TableRelationMode::MovingTo(dest_schema) => {
                        self.touched_relations.insert(sequence);
                        self.arrive_rel_or_type(ObjectName::new(dest_schema, declared));
                    }
                }
                return;
            }
            ValueSource::Sequence(seq @ OwnedSequence { name: None, .. }) => {
                (hooks.owned_seq_name)(table, col_name, seq)
            }
            ValueSource::Identity(_) => (hooks.identity_seq_name)(table, col_name),
            ValueSource::None | ValueSource::Default(_) | ValueSource::Generated(_) => return,
        };
        let key = EngineNamedRelation::ColumnSequence(table.column(col_name));
        match mode {
            TableRelationMode::Arriving => match first_choice {
                Some(first) => self.add_candidate(CandidateName {
                    kind: NameSlotKind::Relation,
                    name: ObjectName::new(&table.schema, first),
                }),
                None => self.exclude_narrowing = true,
            },
            TableRelationMode::Touched => {
                self.touched_engine_named.insert(key, None);
            }
            TableRelationMode::MovingTo(dest_schema) => {
                self.touched_engine_named
                    .insert(key, Some(dest_schema.to_owned()));
            }
        }
    }

    /// Classify one change into binding movement, excluded-class fallback,
    /// touched base objects, and arriving names. Listed in full so a new kind
    /// of change must be classified here rather than defaulting either way.
    fn record(
        &mut self,
        change: &Change,
        base: crate::Side<'_>,
        desired: crate::Side<'_>,
        engine: &Engine<'_>,
        hooks: &Hooks<'_>,
    ) {
        match change {
            // Classes that move names and that the facts do not describe, so
            // every kept surface stays a question whatever was read.
            //
            // A detach renames the clones of its parent's indexes and keys to
            // the declaration's names, and its table leaves the parent's
            // inheritance; an attach builds whichever of them the table lacks
            // under names the engine chooses, and its table joins it (#1545).
            // Partitions are PostgreSQL's alone, where an index's name is a
            // relation's.
            Change::Partition(
                PartitionChange::RebuildPartitionedTable(_)
                | PartitionChange::DetachPartition { .. }
                | PartitionChange::AttachPartition { .. },
            )
            // A computed column is SQL Server's, which supplies no facts. A
            // numbering change creates, renames or drops a sequence, a
            // relation in its schema's namespace that an unqualified relation
            // lookup on the path finds (#1889), under a name this plan may
            // not hold.
            | Change::Column(
                ColumnChange::AddComputedColumn { .. }
                | ColumnChange::DropComputedColumn { .. }
                | ColumnChange::AlterColumnNumbering { .. },
            )
            // A rebuild brings a type's temporary name in, moves columns to
            // it and takes the old one out (DEC-1635.7).
            | Change::TypeValue(TypeValueChange::EnumRebuild { .. }) => {
                self.moves_bindings = true;
                self.exclude_narrowing = true;
            }
            // Classes the facts do not describe and that move nothing by
            // themselves: beside a change that does, every kept surface stays
            // a question.
            //
            // A package create **can** move a binding: it may add an operator
            // or function that matches an argument type better than what the
            // path found before (measured on 16 and 18: intarray's `@>` on
            // `int[]`; a stored view, default, check and `BEGIN ATOMIC` body
            // keep their binding, while later statements and string-bodied or
            // plpgsql routines bind the package's operator at call time;
            // citext adds nothing that rebinds). DEC-1515.1's rule would make
            // that move bindings, and it does not on purpose: that sends
            // every kept surface to a resolver that cannot describe a package
            // the target lacks, and `resolution::resolve` refuses that by
            // name. So the assessment's "unaffected" count does not cover a
            // create; answering it soundly needs the resolver to hold the
            // package (DEC-2447.6, a later slice). Do not "fix" this to move
            // bindings.
            //
            // An **update** is the same answer for the same reason: its
            // scratch holds the version the target holds, so a plan that also
            // needs the resolver is refused by name first (DEC-2447.10).
            Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(
                PartitionChange::CopyTableRows(_)
                | PartitionChange::AnalyzeTableTree(_)
                | PartitionChange::SetPartitionDefault { .. }
                | PartitionChange::SetPartitionNotNull { .. },
            )
            | Change::Extension(
                ExtensionChange::CreateExtension { .. }
                | ExtensionChange::UpdateExtension { .. }
                | ExtensionChange::SetExtensionSchema { .. },
            ) => self.exclude_narrowing = true,
            // Relations, their row types, and backing relation-namespace objects.
            Change::Table(TableChange::CreateTable {
                name,
                table,
                beside_default,
                ..
            }) => {
                self.moves_bindings = true;
                self.exclude_narrowing |= beside_default.is_some() || is_excluded_table(table);
                self.arrive_with_companions(name.clone(), hooks);
                if engine.indexes_are_relations {
                    self.record_table_relations(name, table, hooks, TableRelationMode::Arriving);
                }
            }
            Change::Table(TableChange::DropTable {
                name, detach_from, ..
            }) => {
                self.moves_bindings = true;
                let base_table = base.schema.tables.get(name);
                self.exclude_narrowing |=
                    detach_from.is_some() || base_table.is_some_and(is_excluded_table);
                self.touched_relations.insert(name.clone());
                if engine.indexes_are_relations
                    && let Some(t) = base_table
                {
                    self.record_table_relations(name, t, hooks, TableRelationMode::Touched);
                }
            }
            Change::Table(TableChange::RenameTable { from, to, .. }) => {
                self.moves_bindings = true;
                self.exclude_narrowing |=
                    base.schema.tables.get(from).is_some_and(is_excluded_table)
                        || desired.schema.tables.get(to).is_some_and(is_excluded_table);
                self.touched_relations.insert(from.clone());
                self.arrive_with_companions(to.clone(), hooks);
                // A move to another schema carries the table's indexes, keys
                // and owned sequences with it: each leaves one schema's
                // namespace and arrives in the other's under its catalog name.
                if engine.indexes_are_relations && from.schema != to.schema {
                    match base.schema.tables.get(from) {
                        Some(table) => {
                            self.record_table_relations(
                                from,
                                table,
                                hooks,
                                TableRelationMode::MovingTo(&to.schema),
                            );
                        }
                        None => self.exclude_narrowing = true,
                    }
                }
            }
            // A column change touches its table, not the column: a statement
            // can bind a table's columns and record an edge on the table only
            // (a `MERGE` target), and a row-type reference reads every column.
            Change::Column(ColumnChange::AddColumn {
                table,
                name,
                column,
                ..
            }) => {
                self.moves_bindings = true;
                self.touched_relations.insert(table.clone());
                if engine.indexes_are_relations {
                    self.record_column_sequence(
                        table,
                        name.raw(),
                        column,
                        hooks,
                        TableRelationMode::Arriving,
                    );
                }
            }
            Change::Column(ColumnChange::DropColumn { column, .. }) => {
                self.moves_bindings = true;
                self.touched_relations.insert(column.table.clone());
                // The sequence the column owns goes with it.
                if engine.indexes_are_relations
                    && let Some(spec) = base
                        .schema
                        .tables
                        .get(&column.table)
                        .and_then(|t| t.columns.get(&column.name))
                {
                    self.record_column_sequence(
                        &column.table,
                        column.name.raw(),
                        spec,
                        hooks,
                        TableRelationMode::Touched,
                    );
                }
            }
            Change::Column(ColumnChange::AlterColumnType { column, .. }) => {
                self.moves_bindings = true;
                self.touched_relations.insert(column.table.clone());
            }
            Change::Column(ColumnChange::RenameColumn { table, .. }) => {
                self.moves_bindings = true;
                self.touched_relations.insert(table.clone());
            }
            Change::Module(ModuleChange::CreateModule { id, .. }) => {
                self.moves_bindings = true;
                match id {
                    ModuleId::Named(obj) => self.arrive_with_companions(obj.clone(), hooks),
                    ModuleId::Routine(r) => {
                        self.arriving_names.insert(r.name.name.clone());
                    }
                    ModuleId::Trigger { on, name } => {
                        // A constraint trigger creates a constraint row under
                        // its name in the table's schema. The declaration is
                        // the trigger's text, which does not say which kind it
                        // is, so every trigger counts: that can only keep a
                        // question, never drop one.
                        self.arriving_constraints
                            .insert(ObjectName::new(&on.schema, name));
                    }
                }
            }
            Change::Module(ModuleChange::DropModule { id, .. }) => {
                self.moves_bindings = true;
                self.touch_module(id);
            }
            Change::Module(ModuleChange::AlterModule { id, .. }) => {
                if declared_differently(id, base, desired, engine) {
                    self.moves_bindings = true;
                    self.touch_module(id);
                }
            }
            // Each creates or drops an index. Where an index's name is a
            // relation's (PostgreSQL), an unqualified relation lookup on the
            // path finds it first, and one that bound it, such as a `regclass`
            // constant, finds another relation once it is gone. Where an index
            // is named per table (SQL Server), no lookup reaches it. An
            // unnamed key's index has a name the engine chooses.
            Change::Constraint(ConstraintChange::SetPrimaryKey {
                table, from, to, ..
            }) => {
                if engine.indexes_are_relations {
                    self.moves_bindings = true;
                    self.exclude_narrowing |= copies_indexes(table, base, desired);
                    self.record_pk(table, from.as_ref(), hooks, false);
                    self.record_pk(table, to.as_ref(), hooks, true);
                }
            }
            Change::Constraint(ConstraintChange::AddUnique { table, name, .. }) => {
                if engine.indexes_are_relations {
                    self.moves_bindings = true;
                    self.exclude_narrowing |= copies_indexes(table, base, desired);
                    let rel = ObjectName::new(&table.schema, name.raw());
                    self.arriving_constraints.insert(rel.clone());
                    self.arrive_rel_or_type(rel);
                }
            }
            Change::Index(IndexChange::AddIndex { table, name, .. }) => {
                if engine.indexes_are_relations {
                    self.moves_bindings = true;
                    self.exclude_narrowing |= copies_indexes(table, base, desired);
                    self.arrive_rel_or_type(ObjectName::new(&table.schema, name.raw()));
                }
            }
            Change::Constraint(ConstraintChange::DropUnique { table, name })
            | Change::Index(IndexChange::DropIndex { table, name }) => {
                if engine.indexes_are_relations {
                    self.moves_bindings = true;
                    self.exclude_narrowing |= copies_indexes(table, base, desired);
                    self.touched_relations
                        .insert(ObjectName::new(&table.schema, name.raw()));
                }
            }
            Change::Constraint(ConstraintChange::SetExclusion {
                table,
                name,
                from,
                to,
            }) => {
                if engine.indexes_are_relations {
                    self.moves_bindings = true;
                    self.exclude_narrowing |= copies_indexes(table, base, desired);
                    let rel = ObjectName::new(&table.schema, name.raw());
                    if from.is_some() {
                        self.touched_relations.insert(rel.clone());
                    }
                    if to.is_some() {
                        self.arriving_constraints.insert(rel.clone());
                        self.arrive_rel_or_type(rel);
                    }
                }
            }
            // A schema the creating role may not use is skipped by the
            // lookup, for every object in it: no edge says which surfaces.
            Change::Permission(
                GrantChange::Grant { target, .. } | GrantChange::Revoke { target, .. },
            ) => match target {
                GrantTarget::Schema(_) => {
                    self.moves_bindings = true;
                    self.exclude_narrowing = true;
                }
                GrantTarget::Object(_) | GrantTarget::Routine(_) | GrantTarget::Sequence(_) => {}
            },
            // A type moves no existing lookup by DEC-1515.1's rule, and that
            // answer is kept. Beside a change that does move names, its name
            // is one more that arrives or is touched.
            Change::Type(TypeChange::CreateType { name, .. }) => {
                self.arrive_with_companions(ObjectName::new(name.schema(), name.name()), hooks);
            }
            Change::Type(TypeChange::DropType { name, .. }) => {
                self.touched_types
                    .insert(ObjectName::new(name.schema(), name.name()));
            }
            Change::Type(TypeChange::RenameType { from, to, .. }) => {
                self.touched_types
                    .insert(ObjectName::new(from.schema(), from.name()));
                self.arrive_with_companions(ObjectName::new(to.schema(), to.name()), hooks);
            }
            Change::Constraint(
                ConstraintChange::AddCheck { table, name, .. }
                | ConstraintChange::AddForeignKey { table, name, .. },
            ) => {
                // Moves no binding itself, but claims a constraint name in its
                // schema that an arriving unnamed primary key would yield to.
                self.arriving_constraints
                    .insert(ObjectName::new(&table.schema, name.raw()));
            }
            // An expression's own definition is its own question; changing it
            // brings no name anywhere. A check's or foreign key's name is a
            // constraint's, which no lookup in an expression reaches. A role
            // and a search object move no existing lookup.
            //
            // A package **drop** moves nothing here, for the reason a create
            // does not and one measured more: moving bindings sent every kept
            // surface to a resolver whose scratch holds the package through
            // its externals file, and the closing evidence then omitted a
            // prerequisite the plan removes ("resolver evidence omits a
            // required prerequisite", an internal error, on 16 and 18). What a
            // drop does to a kept surface is bounded by the engine instead: a
            // stored view, default, check or SQL-bodied routine that uses a
            // member is a dependent that makes `RESTRICT` refuse (and the plan
            // refuses it first, by name); only a body bound at call time, such
            // as plpgsql's, finds nothing once the member is gone, which is
            // the resolver's documented observation limit (DEC-2447.8).
            Change::Constraint(
                ConstraintChange::DropCheck { .. } | ConstraintChange::DropForeignKey { .. },
            )
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Column(
                ColumnChange::AlterColumnDefault { .. }
                | ColumnChange::AlterColumnExpression { .. }
                | ColumnChange::AlterColumnNullability { .. }
                | ColumnChange::SetColumnDeprecated { .. },
            )
            | Change::Permission(GrantChange::PublicExecution { .. })
            | Change::Extension(ExtensionChange::DropExtension { .. })
            | Change::TableProperty(_)
            | Change::Policy(_)
            | Change::Row(_)
            | Change::TypeValue(
                TypeValueChange::AddEnumValue { .. }
                | TypeValueChange::AlterDomain { .. }
                | TypeValueChange::RenameEnumValue { .. },
            )
            | Change::Role(_)
            | Change::Search(_) => {}
        }
    }
}

#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use pbps_dialect::{Dialect as _, MinimalDialect};
    use pbps_model::ColumnChange;
    use pbps_model::ConstraintChange;
    use pbps_model::ExtensionChange;
    use pbps_model::GrantChange;
    use pbps_model::IndexChange;
    use pbps_model::ModuleChange;
    use pbps_model::PartitionChange;
    use pbps_model::RoleChange;
    use pbps_model::TableChange;
    use pbps_model::{
        CheckConstraint, Column, Hints, IdsFile, Module, ModuleKind, Permission, PlannedChange,
        Schema, Table,
    };
    use std::collections::BTreeSet;

    fn ids(schema: &Schema, old: &IdsFile) -> IdsFile {
        crate::resolve(
            schema,
            old,
            &[],
            &crate::Context {
                operator: "1515".into(),
                today: "2026-10-05".into(),
            },
        )
        .unwrap()
        .ids
    }

    fn view(definition: &str) -> Module {
        Module {
            kind: ModuleKind::View,
            description: None,
            definition: definition.into(),
            options: Default::default(),
        }
    }

    /// A table with a default and a CHECK, and a view over it: one surface
    /// of each kind the table and the module carry.
    fn bound() -> Schema {
        let mut schema = Schema::default();
        let mut table = Table::default();
        table
            .columns
            .insert("id".into(), Column::new("int".parse().unwrap()));
        let mut n = Column::new("int".parse().unwrap());
        n.value_source = pbps_model::ValueSource::Default("0".into());
        table.columns.insert("n".into(), n);
        table.checks.insert(
            "ck_n".into(),
            CheckConstraint {
                expression: "n >= 0".into(),
            },
        );
        schema.tables.insert("app.t".parse().unwrap(), table);
        schema
            .modules
            .insert("app.v".parse().unwrap(), view("SELECT id FROM app.t"));
        schema
    }

    fn assessed(base: &Schema, desired: &Schema) -> Assessment {
        let base_ids = ids(base, &IdsFile::default());
        let desired_ids = ids(desired, &base_ids);
        let base = crate::Side {
            schema: base,
            ids: &base_ids,
        };
        let desired = crate::Side {
            schema: desired,
            ids: &desired_ids,
        };
        let ordinary = crate::diff(base, desired, &MinimalDialect, &Hints::default()).unwrap();
        assess(base, desired, &ordinary, &MinimalDialect)
    }

    fn minimal(definition: &str) -> String {
        MinimalDialect.normalize_definition(definition)
    }

    fn engine(indexes_are_relations: bool) -> Engine<'static> {
        Engine {
            indexes_are_relations,
            normalize: &minimal,
            filter_binds: true,
        }
    }

    /// The changes on an engine whose index names are relation names.
    fn with(base: &Schema, changes: Vec<Change>) -> Assessment {
        with_indexes(base, changes, true)
    }

    fn with_indexes(
        base: &Schema,
        changes: Vec<Change>,
        indexes_are_relations: bool,
    ) -> Assessment {
        let base_ids = ids(base, &IdsFile::default());
        let side = crate::Side {
            schema: base,
            ids: &base_ids,
        };
        let ordinary = ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        };
        assessed_with(side, side, &ordinary, &engine(indexes_are_relations))
    }

    fn surface(name: &str) -> Surface {
        match name {
            "default" => Surface::Default(
                "app.t"
                    .parse::<pbps_model::TableName>()
                    .unwrap()
                    .column("n"),
            ),
            "check" => Surface::Check {
                table: "app.t".parse().unwrap(),
                name: "ck_n".into(),
            },
            "view" => Surface::Module("app.v".parse().unwrap()),
            other => panic!("no fixture surface {other}"),
        }
    }

    #[test]
    fn a_plan_that_moves_no_name_answers_every_question_without_an_engine() {
        let base = bound();
        let grant = |target| {
            Change::Permission(GrantChange::Grant {
                role: "reader".into(),
                target,
                permissions: BTreeSet::from([Permission::Select]),
            })
        };
        let found = with(
            &base,
            vec![
                grant(GrantTarget::Object("app.t".parse().unwrap())),
                Change::Role(RoleChange::CreateRole {
                    uid: "r_aaaaaa".parse().unwrap(),
                    name: "reader".into(),
                }),
            ],
        );
        assert!(!found.requires_resolution(), "{found:?}");
        assert_eq!(found.count(Answer::Unaffected), 3, "{found:?}");
    }

    /// A package create alone stays "not needed": the resolver cannot describe a
    /// package the target lacks, so answering `Resolve` for it would make every
    /// package create in a project with a kept surface unplannable. The create
    /// beside a change that does move names is the case `resolution::resolve`
    /// refuses by name (DEC-2447.6); the same table change without the create
    /// still goes to the engine, so this negative does not hide it.
    #[test]
    fn a_package_create_alone_needs_no_resolver_and_a_table_change_still_does() {
        let base = bound();
        let package = || {
            Change::Extension(ExtensionChange::CreateExtension {
                uid: "e_aaaaaa".parse().unwrap(),
                name: "citext".into(),
                definition: pbps_model::Extension {
                    schema: "ext".into(),
                    version: "1.6".into(),
                },
                requires: BTreeSet::new(),
            })
        };
        let alone = with(&base, vec![package()]);
        assert!(!alone.requires_resolution(), "{alone:?}");
        assert_eq!(alone.count(Answer::Unaffected), 3, "{alone:?}");
        let table = || {
            Change::Column(ColumnChange::AddColumn {
                uid: "c_aaaaaa".parse().unwrap(),
                table: "app.t".parse().unwrap(),
                name: "extra".into(),
                column: Box::new(Column::new("int".parse().unwrap())),
            })
        };
        assert!(with(&base, vec![table()]).requires_resolution());
        assert!(with(&base, vec![package(), table()]).requires_resolution());
    }

    /// A package drop, like a create, is not a question for the resolver: the
    /// engine's `RESTRICT` bounds what it can do to a kept surface, and sending
    /// the surfaces to a scratch that cannot describe the package leaving broke
    /// the closing evidence (DEC-2447.8). Pinned so that a change to `true` is
    /// made on purpose, with the evidence able to hold a package's removal.
    #[test]
    fn a_package_drop_and_a_create_alone_need_no_resolver() {
        let base = bound();
        let definition = pbps_model::Extension {
            schema: "ext".into(),
            version: "1.6".into(),
        };
        let drop = Change::Extension(ExtensionChange::DropExtension {
            uid: "e_aaaaaa".parse().unwrap(),
            name: "citext".into(),
            definition: definition.clone(),
            requirers: BTreeSet::new(),
            releases: BTreeSet::new(),
            index_releases: Default::default(),
            search_releases: Default::default(),
        });
        let found = with(&base, vec![drop]);
        assert!(!found.requires_resolution(), "{found:?}");
        assert_eq!(found.count(Answer::Unaffected), 3, "{found:?}");
        // Negative: a table change beside it still does.
        let table = Change::Column(ColumnChange::AddColumn {
            uid: "c_aaaaaa".parse().unwrap(),
            table: "app.t".parse().unwrap(),
            name: "extra".into(),
            column: Box::new(Column::new("int".parse().unwrap())),
        });
        let drop = Change::Extension(ExtensionChange::DropExtension {
            uid: "e_aaaaaa".parse().unwrap(),
            name: "citext".into(),
            definition,
            requirers: BTreeSet::new(),
            releases: BTreeSet::new(),
            index_releases: Default::default(),
            search_releases: Default::default(),
        });
        assert!(with(&base, vec![drop, table]).requires_resolution());
    }

    /// Ordinary table changes are not exempt (ADR-0016 §1): a column that
    /// arrives can capture an unqualified reference or a column-notation
    /// call, and nothing short of the engine says which.
    #[test]
    fn an_added_column_sends_every_kept_surface_to_the_engine() {
        let base = bound();
        let mut desired = base.clone();
        desired
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns
            .insert("label".into(), Column::new("text".parse().unwrap()));
        let found = assessed(&base, &desired);
        assert!(found.requires_resolution());
        for name in ["default", "check", "view"] {
            assert_eq!(found.questions[&surface(name)], Answer::Resolve, "{name}");
        }
    }

    /// ADR-0013's rebuild of an unchanged module is the candidate test's
    /// guess. Read as proof, it would answer the very question the resolver
    /// exists to ask, and the irrelevant arrival of #230 would rebuild again.
    #[test]
    fn a_candidate_rebuild_is_a_question_not_an_answer() {
        let base = bound();
        let found = with(
            &base,
            vec![
                Change::Module(ModuleChange::CreateModule {
                    id: "app.f()".parse().unwrap(),
                    module: Box::new(Module {
                        kind: ModuleKind::Function,
                        description: None,
                        definition: "RETURNS int LANGUAGE sql RETURN 1".into(),
                        options: Default::default(),
                    }),
                }),
                Change::Module(ModuleChange::AlterModule {
                    id: "app.v".parse().unwrap(),
                    module: Box::new(view("SELECT id FROM app.t")),
                }),
            ],
        );
        assert_eq!(found.questions[&surface("view")], Answer::Resolve);
    }

    #[test]
    fn a_declared_rebuild_is_answered_and_the_rest_still_need_the_engine() {
        let base = bound();
        let mut desired = base.clone();
        desired
            .modules
            .insert("app.v".parse().unwrap(), view("SELECT id, n FROM app.t"));
        desired.modules.insert(
            "app.f()".parse().unwrap(),
            Module {
                kind: ModuleKind::Function,
                description: None,
                definition: "RETURNS int LANGUAGE sql RETURN 1".into(),
                options: Default::default(),
            },
        );
        let found = assessed(&base, &desired);
        assert_eq!(found.questions[&surface("view")], Answer::Rebuild);
        assert_eq!(found.questions[&surface("check")], Answer::Resolve);
        assert_eq!(found.questions[&surface("default")], Answer::Resolve);
        // The routine the plan creates binds at creation like any new object;
        // only what the target already holds is a question.
        assert!(
            !found
                .questions
                .contains_key(&Surface::Module("app.f()".parse().unwrap()))
        );
    }

    /// A layout-only edit is no declared change to the differ, which compares
    /// definitions normalized: the rebuild an arrival makes of that module
    /// is still the candidate one, and a question. Read bytewise, it would
    /// answer the question and skip the selected resolver (#1526 review). A
    /// real edit is still a declared rebuild.
    #[test]
    fn a_layout_only_edit_leaves_a_candidate_rebuild_a_question() {
        let base = bound();
        let answer = |definition: &str| {
            let mut desired = base.clone();
            desired
                .modules
                .insert("app.v".parse().unwrap(), view(definition));
            let base_ids = ids(&base, &IdsFile::default());
            let desired_ids = ids(&desired, &base_ids);
            let ordinary = ChangeSet {
                changes: [
                    Change::Module(ModuleChange::CreateModule {
                        id: "app.f()".parse().unwrap(),
                        module: Box::new(Module {
                            kind: ModuleKind::Function,
                            description: None,
                            definition: "RETURNS int LANGUAGE sql RETURN 1".into(),
                            options: Default::default(),
                        }),
                    }),
                    Change::Module(ModuleChange::AlterModule {
                        id: "app.v".parse().unwrap(),
                        module: Box::new(view(definition)),
                    }),
                ]
                .into_iter()
                .map(PlannedChange::new)
                .collect(),
            };
            assessed_with(
                crate::Side {
                    schema: &base,
                    ids: &base_ids,
                },
                crate::Side {
                    schema: &desired,
                    ids: &desired_ids,
                },
                &ordinary,
                &engine(true),
            )
            .questions[&surface("view")]
        };
        assert_eq!(answer("SELECT  id\n    FROM app.t"), Answer::Resolve);
        assert_eq!(answer("SELECT id, n FROM app.t"), Answer::Rebuild);
    }

    #[test]
    fn a_rewritten_default_is_a_rebuild_not_a_question() {
        let base = bound();
        let mut desired = base.clone();
        let table = desired.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
        table.columns.get_mut("n").unwrap().value_source =
            pbps_model::ValueSource::Default("1".into());
        table
            .columns
            .insert("label".into(), Column::new("text".parse().unwrap()));
        let found = assessed(&base, &desired);
        assert_eq!(found.questions[&surface("default")], Answer::Rebuild);
        assert_eq!(found.questions[&surface("check")], Answer::Resolve);
    }

    #[test]
    fn a_target_without_bound_surfaces_raises_no_question() {
        let mut base = Schema::default();
        let mut table = Table::default();
        table
            .columns
            .insert("id".into(), Column::new("int".parse().unwrap()));
        base.tables.insert("app.t".parse().unwrap(), table);
        let mut desired = base.clone();
        desired
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns
            .insert("label".into(), Column::new("text".parse().unwrap()));
        let found = assessed(&base, &desired);
        assert!(found.questions.is_empty(), "{found:?}");
        assert!(!found.requires_resolution());
    }

    /// A partition's own default is a binding surface like a column's
    /// (#1578 review): a module arriving makes it a question for the engine,
    /// where reading only the partition's columns, which it has none of,
    /// found nothing to ask. A partition with no default of its own raises
    /// none, and one the plan rewrites is a rebuild.
    #[test]
    fn a_partitions_own_default_is_a_question_when_bindings_move() {
        let tree = |own: Option<&str>| {
            let mut schema = Schema::default();
            let mut parent = Table::default();
            parent
                .columns
                .insert("id".into(), Column::new("int".parse().unwrap()));
            parent.partition_by = Some(pbps_model::PartitionBy {
                external: false,
                columns: vec!["id".into()],
                strategy: Default::default(),
            });
            schema.tables.insert("app.ev".parse().unwrap(), parent);
            let partition = Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: "app.ev".parse().unwrap(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: own
                        .map(|default| {
                            (
                                "id".into(),
                                pbps_model::PartitionColumn {
                                    default: Some(default.to_owned()),
                                    not_null: false,
                                },
                            )
                        })
                        .into_iter()
                        .collect(),
                }),
                ..Table::default()
            };
            schema.tables.insert("app.ev_1".parse().unwrap(), partition);
            schema
        };
        let arrives = |base: &Schema| {
            let mut desired = base.clone();
            desired
                .modules
                .insert("app.v".parse().unwrap(), view("SELECT 1 AS n"));
            assessed(base, &desired)
        };
        let base = tree(Some("app.f(1)"));
        let found = arrives(&base);
        assert_eq!(
            found.questions[&Surface::Default("app.ev_1.id".parse().unwrap())],
            Answer::Resolve,
            "{found:?}"
        );
        assert!(found.requires_resolution());
        let found = arrives(&tree(None));
        assert!(found.questions.is_empty(), "{found:?}");
        // Its default rewritten by the same plan is rebuilt, not asked
        // (#1607 review), while bindings still move.
        let mut desired = tree(Some("app.f(2)"));
        desired
            .modules
            .insert("app.v".parse().unwrap(), view("SELECT 1 AS n"));
        let found = assessed(&base, &desired);
        assert_eq!(
            found.questions[&Surface::Default("app.ev_1.id".parse().unwrap())],
            Answer::Rebuild,
            "{found:?}"
        );
    }

    /// An index's name leaves the relation namespace with it: a surface
    /// that bound it, a `regclass` constant for one, finds another relation
    /// afterwards, and only the engine can say which (#1526 review). A
    /// constraint's name is no relation's.
    #[test]
    fn a_dropped_index_name_moves_lookups_and_a_dropped_check_does_not() {
        let base = bound();
        let table: pbps_model::TableName = "app.t".parse().unwrap();
        for change in [
            Change::Index(IndexChange::DropIndex {
                table: table.clone(),
                name: "ix_gone".into(),
            }),
            Change::Constraint(ConstraintChange::DropUnique {
                table: table.clone(),
                name: "uq_gone".into(),
            }),
        ] {
            assert!(
                with(&base, vec![change.clone()]).requires_resolution(),
                "{change:?}"
            );
        }
        for change in [
            Change::Constraint(ConstraintChange::DropCheck {
                table: table.clone(),
                name: "ck_gone".into(),
            }),
            Change::Constraint(ConstraintChange::DropForeignKey {
                table,
                name: "fk_gone".into(),
            }),
        ] {
            assert!(
                !with(&base, vec![change.clone()]).requires_resolution(),
                "{change:?}"
            );
        }
    }

    /// A table dropped and created again under its old name, with a new UID,
    /// takes its default and check with it: the new ones are created from the
    /// declarations, so they are rebuilds and ask the engine nothing, even
    /// though the drop and create move lookups elsewhere (#1526 review).
    #[test]
    fn a_surface_on_a_replaced_table_is_a_rebuild_not_a_kept_question() {
        let base = bound();
        let mut base_only = base.clone();
        base_only.modules.clear();
        let base_ids = ids(&base_only, &IdsFile::default());
        let mut desired_ids = base_ids.clone();
        let table: pbps_model::TableName = "app.t".parse().unwrap();
        let old = base_ids.table_uid(&table).unwrap().clone();
        desired_ids.tables.remove(&old);
        desired_ids.tables.insert(
            pbps_model::Uid::generate(pbps_model::UidKind::Table),
            table.clone(),
        );
        let columns: Vec<_> = desired_ids.columns.keys().cloned().collect();
        for uid in columns {
            let column = desired_ids.columns.remove(&uid).unwrap();
            desired_ids.columns.insert(
                pbps_model::Uid::generate(pbps_model::UidKind::Column),
                column,
            );
        }
        let ordinary = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::Table(TableChange::DropTable {
                    name: table.clone(),
                    uid: old,
                    detach_from: None,
                })),
                PlannedChange::new(Change::Table(TableChange::CreateTable {
                    uid: desired_ids.table_uid(&table).unwrap().clone(),
                    name: table.clone(),
                    table: Box::new(base_only.tables[&table].clone()),
                    beside_default: None,
                    moves_at_most: None,
                })),
            ],
        };
        let assessment = assessed_with(
            crate::Side {
                schema: &base_only,
                ids: &base_ids,
            },
            crate::Side {
                schema: &base_only,
                ids: &desired_ids,
            },
            &ordinary,
            &engine(true),
        );
        assert!(!assessment.requires_resolution(), "{assessment:?}");
        // The same surfaces kept under their recorded UIDs are questions.
        let kept = assessed_with(
            crate::Side {
                schema: &base_only,
                ids: &base_ids,
            },
            crate::Side {
                schema: &base_only,
                ids: &base_ids,
            },
            &ordinary,
            &engine(true),
        );
        assert!(kept.requires_resolution(), "{kept:?}");
    }

    /// Where an index is named per table, as on SQL Server, adding or dropping
    /// one moves no lookup, so a selected resolver is not asked about it
    /// (DECISIONS 453; #1526 review).
    #[test]
    fn an_index_named_per_table_moves_no_lookup() {
        let base = bound();
        let table: pbps_model::TableName = "app.t".parse().unwrap();
        for change in [
            Change::Index(IndexChange::DropIndex {
                table: table.clone(),
                name: "ix_gone".into(),
            }),
            Change::Constraint(ConstraintChange::DropUnique {
                table: table.clone(),
                name: "uq_gone".into(),
            }),
        ] {
            assert!(
                !with_indexes(&base, vec![change.clone()], false).requires_resolution(),
                "{change:?}"
            );
            assert!(
                with_indexes(&base, vec![change.clone()], true).requires_resolution(),
                "{change:?}"
            );
        }
    }

    /// A detach renames its partition's index clones to the declaration's
    /// names, which are relation names: the engine is asked, as for any
    /// index that arrives or leaves. A grant on the same table moves nothing.
    #[test]
    fn a_detached_partition_moves_lookups() {
        let base = bound();
        let table: pbps_model::TableName = "app.t_2026".parse().unwrap();
        let detach = Change::Partition(PartitionChange::DetachPartition {
            uid: pbps_model::Uid::generate(pbps_model::UidKind::Table),
            table: table.clone(),
            parent: "app.t".parse().unwrap(),
            names: vec![pbps_model::DetachedName {
                kind: pbps_model::DetachedKind::Index,
                parent: "ix_t".into(),
                name: Some("ix_t_2026".into()),
            }],
            shape: Box::new(base.tables[&"app.t".parse().unwrap()].clone()),
        });
        let found = with(&base, vec![detach]);
        assert!(found.requires_resolution(), "{found:?}");
        let grant = Change::Permission(GrantChange::Grant {
            role: "reader".into(),
            target: GrantTarget::Object(table),
            permissions: BTreeSet::from([Permission::Select]),
        });
        assert!(!with(&base, vec![grant]).requires_resolution());
    }

    /// The lookup skips a schema its role may not use, so who may use one is
    /// part of what it finds. Who may read a table is not.
    #[test]
    fn a_schema_grant_moves_lookups_and_an_object_grant_does_not() {
        let base = bound();
        let grant = |target| {
            Change::Permission(GrantChange::Grant {
                role: "owner".into(),
                target,
                permissions: BTreeSet::from([Permission::Usage]),
            })
        };
        let revoke = |target| {
            Change::Permission(GrantChange::Revoke {
                role: "owner".into(),
                target,
                permissions: BTreeSet::from([Permission::Usage]),
            })
        };
        for change in [
            grant(GrantTarget::Schema("ext".into())),
            revoke(GrantTarget::Schema("ext".into())),
        ] {
            assert!(
                with(&base, vec![change.clone()]).requires_resolution(),
                "{change:?}"
            );
        }
        assert!(
            !with(
                &base,
                vec![grant(GrantTarget::Object("app.t".parse().unwrap()))]
            )
            .requires_resolution()
        );
    }
}

#[cfg(test)]
mod dependency_facts_tests {
    use super::*;
    use pbps_dialect::MinimalDialect;
    use pbps_model::{
        CheckConstraint, Column, ComputedColumn, DetachedKind, DetachedName, EnumRebuildStep,
        Extension, Hints, IdsFile, Index, IndexColumn, IndexKey, ManagedType, Module, ModuleKind,
        PartitionBound, PartitionBy, PartitionOf, Permission, PlannedChange, Rebuild,
        RebuildIdentities, RebuildPartitionedTable, Schema, Table, Uid, UidKind,
    };

    fn resolve_ids(schema: &Schema, old: &IdsFile) -> IdsFile {
        crate::resolve(
            schema,
            old,
            &[],
            &crate::Context {
                operator: "2987".into(),
                today: "2026-11-27".into(),
            },
        )
        .unwrap()
        .ids
    }

    fn view(definition: &str) -> Module {
        Module {
            kind: ModuleKind::View,
            description: None,
            definition: definition.into(),
            options: Default::default(),
        }
    }

    fn routine(definition: &str) -> Module {
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: definition.into(),
            options: Default::default(),
        }
    }

    fn table_with_columns(cols: &[&str]) -> Table {
        let mut table = Table::default();
        for col in cols {
            table
                .columns
                .insert((*col).into(), Column::new("int".parse().unwrap()));
        }
        table
    }

    fn two_table_schema() -> Schema {
        let mut schema = Schema::default();
        schema
            .tables
            .insert("app.t".parse().unwrap(), table_with_columns(&["id", "k"]));
        schema
            .tables
            .insert("app.u".parse().unwrap(), table_with_columns(&["id", "k"]));
        schema
            .modules
            .insert("app.v_t".parse().unwrap(), view("SELECT id FROM app.t"));
        schema
            .modules
            .insert("app.v_u".parse().unwrap(), view("SELECT id FROM app.u"));
        schema
    }

    fn run(
        base: &Schema,
        desired: &Schema,
        changes: Vec<Change>,
        facts: Option<&DependencyFacts>,
    ) -> Assessment {
        let base_ids = resolve_ids(base, &IdsFile::default());
        let desired_ids = resolve_ids(desired, &base_ids);
        let ordinary = ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        };
        assess_with_facts(
            crate::Side {
                schema: base,
                ids: &base_ids,
            },
            crate::Side {
                schema: desired,
                ids: &desired_ids,
            },
            &ordinary,
            &MinimalDialect,
            facts,
        )
    }

    fn add_column(table: &str, col: &str) -> Change {
        Change::Column(ColumnChange::AddColumn {
            uid: Uid::generate(UidKind::Column),
            table: table.parse().unwrap(),
            name: col.into(),
            column: Box::new(Column::new("int".parse().unwrap())),
        })
    }

    fn parsed(edges: impl IntoIterator<Item = DependencyEdge>) -> SurfaceDependencies {
        SurfaceDependencies::Parsed(edges.into_iter().collect())
    }

    fn module_surface(id: &str) -> Surface {
        Surface::Module(id.parse().unwrap())
    }

    fn clear_facts(surfaces: BTreeMap<Surface, SurfaceDependencies>) -> DependencyFacts {
        DependencyFacts {
            surfaces,
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::new(),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        }
    }

    fn plain_hooks<'a>(lexis: &'a Lexis<'a>, extra_implicit: bool) -> Hooks<'a> {
        Hooks {
            lexis,
            extra_implicit_relations: if extra_implicit {
                &|_, _| true
            } else {
                &|_, _| false
            },
            unnamed_pk_name: &|_| None,
            identity_seq_name: &|_, _| None,
            owned_seq_name: &|_, _, _| None,
            companion_types: &|_| Vec::new(),
        }
    }

    /// Negative sweep: with no facts, `assess_with_facts(..., None)` agrees
    /// identically with `assess(...)` across non-moving, moving, rebuild, and
    /// excluded-class plans, and every `Unaffected` carries `NoNameMoved`.
    #[test]
    fn without_facts_every_answer_is_unchanged_and_unaffected_is_no_name_moved() {
        let mut base = two_table_schema();
        let mut n = Column::new("int".parse().unwrap());
        n.value_source = ValueSource::Default("0".into());
        base.tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns
            .insert("n".into(), n);
        base.tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .checks
            .insert(
                "ck_n".into(),
                CheckConstraint {
                    expression: "n >= 0".into(),
                },
            );

        let base_ids = resolve_ids(&base, &IdsFile::default());
        let side = crate::Side {
            schema: &base,
            ids: &base_ids,
        };

        let cases: Vec<(&str, Vec<Change>)> = vec![
            (
                "object grant (no move)",
                vec![Change::Permission(GrantChange::Grant {
                    role: "reader".into(),
                    target: GrantTarget::Object("app.t".parse().unwrap()),
                    permissions: BTreeSet::from([Permission::Select]),
                })],
            ),
            ("add column", vec![add_column("app.t", "extra")]),
            (
                "drop column",
                vec![Change::Column(ColumnChange::DropColumn {
                    uid: Uid::generate(UidKind::Column),
                    column: "app.t.k".parse().unwrap(),
                })],
            ),
            (
                "create table",
                vec![Change::Table(TableChange::CreateTable {
                    uid: Uid::generate(UidKind::Table),
                    name: "app.new_t".parse().unwrap(),
                    table: Box::new(table_with_columns(&["id"])),
                    beside_default: None,
                    moves_at_most: None,
                })],
            ),
            (
                "create routine",
                vec![Change::Module(ModuleChange::CreateModule {
                    id: "app.f()".parse().unwrap(),
                    module: Box::new(routine("RETURNS int LANGUAGE sql RETURN 1")),
                })],
            ),
            (
                "schema grant",
                vec![Change::Permission(GrantChange::Grant {
                    role: "owner".into(),
                    target: GrantTarget::Schema("ext".into()),
                    permissions: BTreeSet::from([Permission::Usage]),
                })],
            ),
            (
                "create extension alone",
                vec![Change::Extension(ExtensionChange::CreateExtension {
                    uid: Uid::generate(UidKind::Extension),
                    name: "citext".into(),
                    definition: Extension {
                        schema: "ext".into(),
                        version: "1.6".into(),
                    },
                    requires: BTreeSet::new(),
                })],
            ),
            (
                "add computed column",
                vec![Change::Column(ColumnChange::AddComputedColumn {
                    table: "app.t".parse().unwrap(),
                    name: "c".into(),
                    computed: ComputedColumn {
                        expression: "id + 1".into(),
                        persisted: false,
                        not_null: false,
                    },
                })],
            ),
        ];

        for (label, changes) in cases {
            let ordinary = ChangeSet {
                changes: changes.into_iter().map(PlannedChange::new).collect(),
            };
            let legacy = assess(side, side, &ordinary, &MinimalDialect);
            let none_facts = assess_with_facts(side, side, &ordinary, &MinimalDialect, None);
            assert_eq!(legacy, none_facts, "{label}");
            for (surface, answer) in &none_facts.questions {
                match answer {
                    Answer::Unaffected => assert_eq!(
                        none_facts.unaffected_reason(surface),
                        Some(UnaffectedReason::NoNameMoved),
                        "{label}: {surface:?}"
                    ),
                    Answer::Rebuild | Answer::Resolve => {
                        assert_eq!(none_facts.unaffected_reason(surface), None, "{label}");
                    }
                }
            }
        }

        // Also check diff-driven declared rebuild beside a moving change.
        let mut desired = base.clone();
        desired
            .modules
            .insert("app.v_t".parse().unwrap(), view("SELECT id, k FROM app.t"));
        desired
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns
            .insert("label".into(), Column::new("text".parse().unwrap()));
        let desired_ids = resolve_ids(&desired, &base_ids);
        let desired_side = crate::Side {
            schema: &desired,
            ids: &desired_ids,
        };
        let ordinary = crate::diff(side, desired_side, &MinimalDialect, &Hints::default()).unwrap();
        assert_eq!(
            assess(side, desired_side, &ordinary, &MinimalDialect),
            assess_with_facts(side, desired_side, &ordinary, &MinimalDialect, None)
        );
    }

    /// Negative-first: a surface with an edge on the changed table, on a column
    /// of it, on its row type, or on a descendant of it stays `Resolve`; a view
    /// with no edge on any of those is `Unaffected` with `ByDependency`.
    #[test]
    fn relation_column_row_type_and_descendant_edges_stay_resolve_while_unrelated_is_unaffected() {
        let mut schema = Schema::default();
        for name in ["app.parent", "app.child", "app.grandchild", "app.other"] {
            schema
                .tables
                .insert(name.parse().unwrap(), table_with_columns(&["id", "note"]));
        }
        schema.modules.insert(
            "app.v_direct".parse().unwrap(),
            view("SELECT * FROM app.parent"),
        );
        schema.modules.insert(
            "app.v_col".parse().unwrap(),
            view("SELECT id FROM app.parent"),
        );
        schema.modules.insert(
            "app.v_rowtype".parse().unwrap(),
            view("SELECT NULL::app.parent"),
        );
        schema.modules.insert(
            "app.v_child".parse().unwrap(),
            view("SELECT * FROM app.child"),
        );
        schema.modules.insert(
            "app.v_grandchild".parse().unwrap(),
            view("SELECT * FROM app.grandchild"),
        );
        schema.modules.insert(
            "app.v_other".parse().unwrap(),
            view("SELECT id FROM app.other"),
        );

        let parent: TableName = "app.parent".parse().unwrap();
        let child: TableName = "app.child".parse().unwrap();
        let grandchild: TableName = "app.grandchild".parse().unwrap();
        let other: TableName = "app.other".parse().unwrap();

        let surfaces = BTreeMap::from([
            (
                module_surface("app.v_direct"),
                parsed([DependencyEdge::Relation(parent.clone())]),
            ),
            (
                module_surface("app.v_col"),
                parsed([DependencyEdge::Column(parent.column("id"))]),
            ),
            (
                module_surface("app.v_rowtype"),
                parsed([DependencyEdge::Type(parent.clone())]),
            ),
            (
                module_surface("app.v_child"),
                parsed([DependencyEdge::Relation(child.clone())]),
            ),
            (
                module_surface("app.v_grandchild"),
                parsed([DependencyEdge::Column(grandchild.column("id"))]),
            ),
            (
                module_surface("app.v_other"),
                parsed([DependencyEdge::Column(other.column("id"))]),
            ),
        ]);
        let ancestors = BTreeMap::from([
            (child.clone(), BTreeSet::from([Ancestor::Relation(parent)])),
            (grandchild, BTreeSet::from([Ancestor::Relation(child)])),
        ]);
        let facts = DependencyFacts {
            surfaces,
            relation_ancestors: ReadFact::Known(ancestors),
            catalog_names: BTreeMap::new(),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        };

        let found = run(
            &schema,
            &schema,
            vec![add_column("app.parent", "extra")],
            Some(&facts),
        );
        for resolve in [
            "app.v_direct",
            "app.v_col",
            "app.v_rowtype",
            "app.v_child",
            "app.v_grandchild",
        ] {
            let s = module_surface(resolve);
            assert_eq!(found.questions[&s], Answer::Resolve, "{resolve}");
            assert_eq!(found.unaffected_reason(&s), None, "{resolve}");
        }
        let unrelated = module_surface("app.v_other");
        assert_eq!(found.questions[&unrelated], Answer::Unaffected);
        assert_eq!(
            found.unaffected_reason(&unrelated),
            Some(UnaffectedReason::ByDependency)
        );

        // Direction check: changing the child does NOT touch a reader of the parent alone.
        let child_change = run(
            &schema,
            &schema,
            vec![add_column("app.child", "child_only")],
            Some(&facts),
        );
        assert_eq!(
            child_change.questions[&module_surface("app.v_child")],
            Answer::Resolve
        );
        assert_eq!(
            child_change.questions[&module_surface("app.v_grandchild")],
            Answer::Resolve
        );
        assert_eq!(
            child_change.questions[&module_surface("app.v_direct")],
            Answer::Unaffected
        );
        assert_eq!(
            child_change.unaffected_reason(&module_surface("app.v_direct")),
            Some(UnaffectedReason::ByDependency)
        );
    }

    /// A typed table (`OF comp`): changing a composite type touches readers
    /// of tables typed `OF` it, while leaving unrelated readers `Unaffected`.
    #[test]
    fn changing_a_composite_type_touches_readers_of_tables_typed_of_it() {
        let mut schema = Schema::default();
        schema
            .tables
            .insert("app.ot".parse().unwrap(), table_with_columns(&["a"]));
        schema
            .tables
            .insert("app.other".parse().unwrap(), table_with_columns(&["a"]));
        schema
            .modules
            .insert("app.v_typed".parse().unwrap(), view("SELECT * FROM app.ot"));
        schema.modules.insert(
            "app.v_other".parse().unwrap(),
            view("SELECT a FROM app.other"),
        );

        let ot: TableName = "app.ot".parse().unwrap();
        let other: TableName = "app.other".parse().unwrap();
        let comp: ObjectName = "app.comp".parse().unwrap();
        let facts = DependencyFacts {
            surfaces: BTreeMap::from([
                (
                    module_surface("app.v_typed"),
                    parsed([DependencyEdge::Relation(ot.clone())]),
                ),
                (
                    module_surface("app.v_other"),
                    parsed([DependencyEdge::Column(other.column("a"))]),
                ),
            ]),
            relation_ancestors: ReadFact::Known(BTreeMap::from([(
                ot,
                BTreeSet::from([Ancestor::Type(comp)]),
            )])),
            catalog_names: BTreeMap::from([("comp_v2".into(), CatalogName::Clear)]),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        };

        // Paired with a moving change on an unrelated third table so the plan
        // enters the per-surface rule.
        let found = run(
            &schema,
            &schema,
            vec![
                add_column("app.unrelated", "x"),
                Change::Type(TypeChange::RenameType {
                    uid: Uid::generate(UidKind::Type),
                    from: "app.comp".parse().unwrap(),
                    to: "app.comp_v2".parse().unwrap(),
                    definition: Box::new(ManagedType::Enum {
                        values: vec!["a".into()],
                    }),
                }),
            ],
            Some(&facts),
        );
        assert_eq!(
            found.questions[&module_surface("app.v_typed")],
            Answer::Resolve
        );
        assert_eq!(
            found.questions[&module_surface("app.v_other")],
            Answer::Unaffected
        );
        assert_eq!(
            found.unaffected_reason(&module_surface("app.v_other")),
            Some(UnaffectedReason::ByDependency)
        );
    }

    /// An arriving routine whose name equals a column edge's name,
    /// a type edge's name, a routine edge's name, a relation edge's name, or a
    /// catalog-held (or unreadable/unchecked) name stays `Resolve`; a routine
    /// with a clear catalog name and no matching edge is `Unaffected`.
    #[test]
    fn arriving_routine_matching_column_type_routine_or_catalog_name_stays_resolve() {
        let mut schema = Schema::default();
        schema.tables.insert(
            "app.t".parse().unwrap(),
            table_with_columns(&["id", "note"]),
        );
        schema.modules.insert(
            "app.v_col".parse().unwrap(),
            view("SELECT note(t) FROM app.t"),
        );
        schema.modules.insert(
            "app.v_type".parse().unwrap(),
            view("SELECT d(id) FROM app.t"),
        );
        schema.modules.insert(
            "app.v_fn".parse().unwrap(),
            view("SELECT calc(id) FROM app.t"),
        );
        schema
            .modules
            .insert("app.v_plain".parse().unwrap(), view("SELECT id FROM app.t"));

        let t: TableName = "app.t".parse().unwrap();
        let base_surfaces = BTreeMap::from([
            (
                module_surface("app.v_col"),
                parsed([DependencyEdge::Column(t.column("note"))]),
            ),
            (
                module_surface("app.v_type"),
                parsed([
                    DependencyEdge::Column(t.column("id")),
                    DependencyEdge::Type("app.d".parse().unwrap()),
                ]),
            ),
            (
                module_surface("app.v_fn"),
                parsed([
                    DependencyEdge::Column(t.column("id")),
                    DependencyEdge::Routine("app.calc".parse().unwrap()),
                ]),
            ),
            (
                module_surface("app.v_plain"),
                parsed([DependencyEdge::Column(t.column("id"))]),
            ),
        ]);

        let create_fn = |sig: &str| {
            Change::Module(ModuleChange::CreateModule {
                id: sig.parse().unwrap(),
                module: Box::new(routine("RETURNS int LANGUAGE sql RETURN 1")),
            })
        };

        // 1. Arriving routine `note(app.t)` hits `v_col` via its column edge `note`,
        //    leaving the other views unaffected.
        let facts = DependencyFacts {
            surfaces: base_surfaces.clone(),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::from([("note".into(), CatalogName::Clear)]),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        };
        let found = run(
            &schema,
            &schema,
            vec![create_fn("app.note(app.t)")],
            Some(&facts),
        );
        assert_eq!(
            found.questions[&module_surface("app.v_col")],
            Answer::Resolve
        );
        assert_eq!(
            found.questions[&module_surface("app.v_type")],
            Answer::Unaffected
        );
        assert_eq!(
            found.questions[&module_surface("app.v_plain")],
            Answer::Unaffected
        );

        // 2. Arriving routine `d(int)` hits `v_type` via its type edge `d`.
        let facts = DependencyFacts {
            surfaces: base_surfaces.clone(),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::from([("d".into(), CatalogName::Clear)]),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        };
        let found = run(
            &schema,
            &schema,
            vec![create_fn("app.d(int)")],
            Some(&facts),
        );
        assert_eq!(
            found.questions[&module_surface("app.v_type")],
            Answer::Resolve
        );
        assert_eq!(
            found.questions[&module_surface("app.v_plain")],
            Answer::Unaffected
        );

        // 3. Arriving overload `calc(text)` hits `v_fn` via its routine edge `calc`.
        let facts = DependencyFacts {
            surfaces: base_surfaces.clone(),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::from([("calc".into(), CatalogName::Clear)]),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        };
        let found = run(
            &schema,
            &schema,
            vec![create_fn("app.calc(text)")],
            Some(&facts),
        );
        assert_eq!(
            found.questions[&module_surface("app.v_fn")],
            Answer::Resolve
        );
        assert_eq!(
            found.questions[&module_surface("app.v_plain")],
            Answer::Unaffected
        );

        // 4. Arriving routine with a catalog-held name (e.g. `uuid` or `lower`),
        //    an unreadable catalog check, or an unchecked (absent) name sends
        //    every kept surface to `Resolve`.
        for (name, status) in [
            ("uuid", Some(CatalogName::Held)),
            ("lower", Some(CatalogName::Unreadable)),
            ("unchecked", None),
        ] {
            let mut catalog_names = BTreeMap::new();
            if let Some(s) = status {
                catalog_names.insert(name.to_owned(), s);
            }
            let facts = DependencyFacts {
                surfaces: base_surfaces.clone(),
                relation_ancestors: ReadFact::Known(BTreeMap::new()),
                catalog_names,
                engine_named_relations: BTreeMap::new(),
                candidate_names: BTreeMap::new(),
            };
            let found = run(
                &schema,
                &schema,
                vec![create_fn(&format!("app.{name}(text)"))],
                Some(&facts),
            );
            assert_eq!(
                found.count(Answer::Resolve),
                base_surfaces.len(),
                "{name} / {status:?}: {found:?}"
            );
        }

        // 5. Positive control: an arriving routine with a clear catalog name
        //    matching no edge leaves every covered parsed surface `Unaffected`.
        let facts = DependencyFacts {
            surfaces: base_surfaces,
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::from([("unrelated_helper".into(), CatalogName::Clear)]),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        };
        let found = run(
            &schema,
            &schema,
            vec![create_fn("app.unrelated_helper(int)")],
            Some(&facts),
        );
        assert!(!found.requires_resolution(), "{found:?}");
        assert_eq!(found.count_unaffected(UnaffectedReason::ByDependency), 4);
    }

    /// A surface with NO edge on an arriving relation or type name
    /// still stays `Resolve` when its SQL text mentions that name (reusing the
    /// existing identifier scanner); surfaces whose text does not mention it
    /// (or only mentions it in a comment, literal, or another schema) are
    /// `Unaffected`.
    #[test]
    fn surface_text_mentioning_arriving_relation_or_type_without_edge_stays_resolve() {
        let mut schema = Schema::default();
        let mut table = table_with_columns(&["id"]);
        let mut col_def = Column::new("int".parse().unwrap());
        col_def.value_source =
            ValueSource::Default("CASE WHEN ARRAY[]::n[] IS NULL THEN 0 ELSE 1 END".into());
        table.columns.insert("d_col".into(), col_def);
        table.checks.insert(
            "ck_arr".into(),
            CheckConstraint {
                expression: "ARRAY[]::n[] IS NULL".into(),
            },
        );
        table.indexes.insert(
            "ix_expr".into(),
            Index {
                columns: vec![IndexColumn {
                    key: IndexKey::Expression("(ARRAY[]::n[] IS NULL)".into()),
                    descending: false,
                    opclass: None,
                    collation: None,
                    opclass_parameters: BTreeMap::new(),
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
                filegroup: None,
            },
        );
        schema.tables.insert("app.host".parse().unwrap(), table);

        // Views with zero recorded edges: an empty typed array, json_value
        // RETURNING dom, comment/literal-only mention, and other-schema qualified.
        schema.modules.insert(
            "app.v_f1".parse().unwrap(),
            view("SELECT 1 AS r WHERE ARRAY[]::n[] IS NULL"),
        );
        schema.modules.insert(
            "app.v_f2".parse().unwrap(),
            view("SELECT json_value(j, '$.a' RETURNING dom) FROM app.host"),
        );
        schema.modules.insert(
            "app.v_comment".parse().unwrap(),
            view("SELECT 1 AS r -- n dom\nWHERE 'n' <> 'dom'"),
        );
        schema.modules.insert(
            "app.v_other_schema".parse().unwrap(),
            view("SELECT 1 AS r WHERE ARRAY[]::late.n[] IS NULL"),
        );

        let host: TableName = "app.host".parse().unwrap();
        let default_s = Surface::Default(host.column("d_col"));
        let check_s = Surface::Check {
            table: host.clone(),
            name: "ck_arr".into(),
        };
        let index_s = Surface::Index {
            table: host,
            name: "ix_expr".into(),
        };

        let surfaces = BTreeMap::from([
            (default_s.clone(), parsed([])),
            (check_s.clone(), parsed([])),
            (index_s.clone(), parsed([])),
            (module_surface("app.v_f1"), parsed([])),
            (module_surface("app.v_f2"), parsed([])),
            (module_surface("app.v_comment"), parsed([])),
            (module_surface("app.v_other_schema"), parsed([])),
        ]);
        let facts = DependencyFacts {
            surfaces,
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::from([
                ("n".into(), CatalogName::Clear),
                ("dom".into(), CatalogName::Clear),
            ]),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        };

        // Relation `early.n` arrives and type `early.dom` arrives.
        let found = run(
            &schema,
            &schema,
            vec![
                Change::Table(TableChange::CreateTable {
                    uid: Uid::generate(UidKind::Table),
                    name: "early.n".parse().unwrap(),
                    table: Box::new(table_with_columns(&["y"])),
                    beside_default: None,
                    moves_at_most: None,
                }),
                Change::Type(TypeChange::CreateType {
                    uid: Uid::generate(UidKind::Type),
                    name: "early.dom".parse().unwrap(),
                    definition: Box::new(ManagedType::Enum {
                        values: vec!["x".into()],
                    }),
                }),
            ],
            Some(&facts),
        );

        for s in [
            default_s,
            check_s,
            index_s,
            module_surface("app.v_f1"),
            module_surface("app.v_f2"),
        ] {
            assert_eq!(found.questions[&s], Answer::Resolve, "{s:?}");
        }
        assert_eq!(
            found.questions[&module_surface("app.v_comment")],
            Answer::Unaffected
        );
        assert_eq!(
            found.unaffected_reason(&module_surface("app.v_comment")),
            Some(UnaffectedReason::ByDependency)
        );
        assert_eq!(
            found.questions[&module_surface("app.v_other_schema")],
            Answer::Unaffected
        );
    }

    /// Absent (uncovered) and unreadable stay `Resolve`; unparsed is
    /// `Unaffected` with `UnaffectedReason::Unparsed`; parsed with
    /// an empty edge set is `Unaffected` when nothing arrives that it mentions
    /// (and `Resolve` when something it mentions arrives). Unreadable ancestor
    /// ties force `Resolve` even for parsed-empty surfaces.
    #[test]
    fn uncovered_unreadable_unparsed_and_parsed_empty_states_behave_distinctly() {
        let mut schema = Schema::default();
        schema
            .tables
            .insert("app.t".parse().unwrap(), table_with_columns(&["id"]));
        schema
            .modules
            .insert("app.v_uncovered".parse().unwrap(), view("SELECT 1"));
        schema
            .modules
            .insert("app.v_unreadable".parse().unwrap(), view("SELECT 1"));
        schema.modules.insert(
            "app.f_unparsed()".parse().unwrap(),
            routine("RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END; $$"),
        );
        schema.modules.insert(
            "app.v_empty".parse().unwrap(),
            view("SELECT 1 AS constant_only"),
        );

        let surfaces = BTreeMap::from([
            (
                module_surface("app.v_unreadable"),
                SurfaceDependencies::Unreadable,
            ),
            (
                module_surface("app.f_unparsed()"),
                SurfaceDependencies::Unparsed(BTreeSet::new()),
            ),
            (module_surface("app.v_empty"), parsed([])),
        ]);
        let facts = clear_facts(surfaces.clone());

        let found = run(
            &schema,
            &schema,
            vec![add_column("app.t", "extra")],
            Some(&facts),
        );
        assert_eq!(
            found.questions[&module_surface("app.v_uncovered")],
            Answer::Resolve
        );
        assert_eq!(
            found.questions[&module_surface("app.v_unreadable")],
            Answer::Resolve
        );
        assert_eq!(
            found.questions[&module_surface("app.f_unparsed()")],
            Answer::Unaffected
        );
        assert_eq!(
            found.unaffected_reason(&module_surface("app.f_unparsed()")),
            Some(UnaffectedReason::Unparsed)
        );
        assert_eq!(
            found.questions[&module_surface("app.v_empty")],
            Answer::Unaffected
        );
        assert_eq!(
            found.unaffected_reason(&module_surface("app.v_empty")),
            Some(UnaffectedReason::ByDependency)
        );
        assert_eq!(found.count_unaffected(UnaffectedReason::NoNameMoved), 0);
        assert_eq!(found.count_unaffected(UnaffectedReason::ByDependency), 1);
        assert_eq!(found.count_unaffected(UnaffectedReason::Unparsed), 1);

        // Negative: unreadable ancestor ties force every kept surface to `Resolve`.
        let unreadable_ancestors = DependencyFacts {
            surfaces,
            relation_ancestors: ReadFact::Unreadable,
            catalog_names: BTreeMap::new(),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        };
        let found = run(
            &schema,
            &schema,
            vec![add_column("app.t", "extra")],
            Some(&unreadable_ancestors),
        );
        assert_eq!(found.count(Answer::Resolve), 4, "{found:?}");
    }

    /// A routine's header is bound at creation whatever its body's language,
    /// so an unparsed routine whose text names an arriving type stays a
    /// question; one that does not is unaffected beside the same plan.
    #[test]
    fn an_unparsed_routine_naming_an_arriving_type_in_its_text_stays_resolve() {
        let mut schema = Schema::default();
        schema.modules.insert(
            "app.f_plain()".parse().unwrap(),
            routine("RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END; $$"),
        );
        schema.modules.insert(
            "app.f_returns()".parse().unwrap(),
            routine("RETURNS money2 LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END; $$"),
        );
        let mut facts = clear_facts(BTreeMap::from([
            (
                module_surface("app.f_plain()"),
                SurfaceDependencies::Unparsed(BTreeSet::new()),
            ),
            (
                module_surface("app.f_returns()"),
                SurfaceDependencies::Unparsed(BTreeSet::new()),
            ),
        ]));
        facts
            .catalog_names
            .insert("money2".into(), CatalogName::Clear);
        let found = run(
            &schema,
            &schema,
            vec![Change::Table(TableChange::CreateTable {
                uid: Uid::generate(UidKind::Table),
                name: "early.money2".parse().unwrap(),
                table: Box::new(table_with_columns(&["a"])),
                beside_default: None,
                moves_at_most: None,
            })],
            Some(&facts),
        );
        assert_eq!(
            found.questions[&module_surface("app.f_returns()")],
            Answer::Resolve
        );
        assert_eq!(
            found.unaffected_reason(&module_surface("app.f_plain()")),
            Some(UnaffectedReason::Unparsed)
        );
    }

    /// Excluded plan classes stay today's answer for every kept surface even
    /// with perfect empty facts covering every surface.
    #[test]
    fn every_excluded_plan_class_keeps_todays_answer_even_with_perfect_facts() {
        let schema = two_table_schema();
        let perfect = clear_facts(BTreeMap::from([
            (module_surface("app.v_t"), parsed([])),
            (module_surface("app.v_u"), parsed([])),
        ]));

        // Sanity control: a plain column add on an unrelated table narrows both
        // covered surfaces to `Unaffected` with these exact facts.
        let control = run(
            &schema,
            &schema,
            vec![add_column("app.other", "x")],
            Some(&perfect),
        );
        assert!(!control.requires_resolution(), "{control:?}");
        assert_eq!(control.count_unaffected(UnaffectedReason::ByDependency), 2);

        let excluded_cases: Vec<(&str, Vec<Change>)> = vec![
            (
                "partition detach",
                vec![Change::Partition(PartitionChange::DetachPartition {
                    uid: Uid::generate(UidKind::Table),
                    table: "app.p_1".parse().unwrap(),
                    parent: "app.p".parse().unwrap(),
                    names: vec![DetachedName {
                        kind: DetachedKind::Index,
                        parent: "ix_p".into(),
                        name: Some("ix_p_1".into()),
                    }],
                    shape: Box::new(table_with_columns(&["id"])),
                })],
            ),
            (
                "partition attach",
                vec![Change::Partition(PartitionChange::AttachPartition {
                    uid: Uid::generate(UidKind::Table),
                    table: "app.p_1".parse().unwrap(),
                    parent: "app.p".parse().unwrap(),
                    bound: PartitionBound::Default,
                    key: vec!["id".into()],
                    shape: Box::new(table_with_columns(&["id"])),
                    rehearsal: None,
                })],
            ),
            (
                "partitioned table rebuild",
                vec![Change::Partition(PartitionChange::RebuildPartitionedTable(
                    Box::new(RebuildPartitionedTable {
                        table: "app.p".parse().unwrap(),
                        intent: Rebuild {
                            source: RebuildIdentities {
                                tables: BTreeMap::new(),
                                columns: BTreeMap::new(),
                            },
                            replacement: RebuildIdentities {
                                tables: BTreeMap::new(),
                                columns: BTreeMap::new(),
                            },
                            replacement_root: Uid::generate(UidKind::Table),
                            continues: BTreeMap::new(),
                            target_checksum: "x".into(),
                            max_rows: 1,
                            allowed_at: "2026-11-27T00:00:00Z".into(),
                            reason: "repartition".parse().unwrap(),
                            operator: "2987".into(),
                            types: BTreeMap::new(),
                            conversions: BTreeMap::new(),
                            exclusions: BTreeMap::new(),
                        },
                        earlier: Vec::new(),
                        before: BTreeMap::new(),
                        after: BTreeMap::new(),
                    }),
                ))],
            ),
            (
                "create partitioned table",
                vec![Change::Table(TableChange::CreateTable {
                    uid: Uid::generate(UidKind::Table),
                    name: "app.part_parent".parse().unwrap(),
                    table: Box::new(Table {
                        partition_by: Some(PartitionBy {
                            strategy: Default::default(),
                            columns: vec!["id".into()],
                            external: false,
                        }),
                        ..table_with_columns(&["id"])
                    }),
                    beside_default: None,
                    moves_at_most: None,
                })],
            ),
            (
                "create partition child",
                vec![Change::Table(TableChange::CreateTable {
                    uid: Uid::generate(UidKind::Table),
                    name: "app.part_child".parse().unwrap(),
                    table: Box::new(Table {
                        partition_of: Some(PartitionOf {
                            parent: "app.part_parent".parse().unwrap(),
                            bound: PartitionBound::Default,
                            columns: BTreeMap::new(),
                        }),
                        ..Table::default()
                    }),
                    beside_default: None,
                    moves_at_most: None,
                })],
            ),
            (
                "enum rebuild",
                vec![Change::TypeValue(TypeValueChange::EnumRebuild {
                    uid: Uid::generate(UidKind::Type),
                    name: "app.mood".parse().unwrap(),
                    tmp: "app.mood__pbps_new".parse().unwrap(),
                    step: EnumRebuildStep::Create {
                        definition: Box::new(ManagedType::Enum {
                            values: vec!["ok".into()],
                        }),
                        removed: Vec::new(),
                        ordinal: 0,
                        over: None,
                        why: None,
                    },
                })],
            ),
            (
                "extension arriving beside moving change",
                vec![
                    Change::Extension(ExtensionChange::CreateExtension {
                        uid: Uid::generate(UidKind::Extension),
                        name: "citext".into(),
                        definition: Extension {
                            schema: "ext".into(),
                            version: "1.6".into(),
                        },
                        requires: BTreeSet::new(),
                    }),
                    add_column("app.other", "x"),
                ],
            ),
            (
                "extension arriving alone (stays today's Unaffected per DEC-2447.6)",
                vec![Change::Extension(ExtensionChange::CreateExtension {
                    uid: Uid::generate(UidKind::Extension),
                    name: "citext".into(),
                    definition: Extension {
                        schema: "ext".into(),
                        version: "1.6".into(),
                    },
                    requires: BTreeSet::new(),
                })],
            ),
            (
                "sql server add computed column",
                vec![Change::Column(ColumnChange::AddComputedColumn {
                    table: "app.other".parse().unwrap(),
                    name: "c".into(),
                    computed: ComputedColumn {
                        expression: "id + 1".into(),
                        persisted: false,
                        not_null: false,
                    },
                })],
            ),
            (
                "sql server drop computed column",
                vec![Change::Column(ColumnChange::DropComputedColumn {
                    table: "app.other".parse().unwrap(),
                    name: "c".into(),
                    computed: ComputedColumn {
                        expression: "id + 1".into(),
                        persisted: false,
                        not_null: false,
                    },
                })],
            ),
            (
                "schema grant",
                vec![Change::Permission(GrantChange::Grant {
                    role: "owner".into(),
                    target: GrantTarget::Schema("ext".into()),
                    permissions: BTreeSet::from([Permission::Usage]),
                })],
            ),
        ];

        for (label, changes) in excluded_cases {
            let without_facts = run(&schema, &schema, changes.clone(), None);
            let with_facts = run(&schema, &schema, changes, Some(&perfect));
            assert_eq!(with_facts, without_facts, "{label}");
        }
    }

    /// The `MERGE`-shaped case: a surface holding an edge on the relation
    /// alone (no column edge for the target column `k`) stays `Resolve` when
    /// column `k` is dropped or retyped, while a surface with no edge on `u`
    /// is `Unaffected`.
    #[test]
    fn merge_shaped_relation_only_edge_stays_resolve_on_column_drop_and_retype() {
        let mut schema = two_table_schema();
        schema.modules.insert(
            "app.c_merge()".parse().unwrap(),
            routine(
                "RETURNS void LANGUAGE sql BEGIN ATOMIC \
                 MERGE INTO app.u USING app.t ON app.u.id = app.t.id \
                 WHEN MATCHED THEN UPDATE SET k = app.t.k; END",
            ),
        );

        let t: TableName = "app.t".parse().unwrap();
        let u: TableName = "app.u".parse().unwrap();
        let facts = clear_facts(BTreeMap::from([
            // MERGE records only relation-level edge on `app.u` (nothing on `u.k`).
            (
                module_surface("app.c_merge()"),
                parsed([
                    DependencyEdge::Relation(u.clone()),
                    DependencyEdge::Column(t.column("id")),
                    DependencyEdge::Column(t.column("k")),
                ]),
            ),
            (
                module_surface("app.v_t"),
                parsed([DependencyEdge::Column(t.column("id"))]),
            ),
            (
                module_surface("app.v_u"),
                parsed([DependencyEdge::Column(u.column("id"))]),
            ),
        ]));

        let drop_k = Change::Column(ColumnChange::DropColumn {
            uid: Uid::generate(UidKind::Column),
            column: u.column("k"),
        });
        let retype_k = Change::Column(ColumnChange::AlterColumnType {
            uid: Uid::generate(UidKind::Column),
            column: u.column("k"),
            from: "int".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
            from_collation: None,
            to_collation: None,
            through_external_partitions: false,
            using: None,
        });

        for change in [drop_k, retype_k] {
            let found = run(&schema, &schema, vec![change.clone()], Some(&facts));
            assert_eq!(
                found.questions[&module_surface("app.c_merge()")],
                Answer::Resolve,
                "{change:?}"
            );
            assert_eq!(
                found.questions[&module_surface("app.v_u")],
                Answer::Resolve,
                "{change:?}"
            );
            assert_eq!(
                found.questions[&module_surface("app.v_t")],
                Answer::Unaffected,
                "{change:?}"
            );
            assert_eq!(
                found.unaffected_reason(&module_surface("app.v_t")),
                Some(UnaffectedReason::ByDependency),
                "{change:?}"
            );
        }
    }

    /// Relation-namespace indexes (`indexes_are_relations == true`): an added
    /// index arrives as a relation name (catching text mentions and matching
    /// edges), and a dropped index touches that index relation without
    /// touching unrelated surfaces.
    #[test]
    fn relation_namespace_index_changes_narrow_by_index_relation_and_text_scan() {
        let mut schema = two_table_schema();
        schema.modules.insert(
            "app.v_mentions_ix".parse().unwrap(),
            view("SELECT 1 WHERE ARRAY[]::ix_new[] IS NULL"),
        );
        let base_ids = resolve_ids(&schema, &IdsFile::default());
        let side = crate::Side {
            schema: &schema,
            ids: &base_ids,
        };
        let t: TableName = "app.t".parse().unwrap();
        let ix_old: ObjectName = "app.ix_old".parse().unwrap();
        let facts = DependencyFacts {
            surfaces: BTreeMap::from([
                (
                    module_surface("app.v_t"),
                    parsed([DependencyEdge::Relation(ix_old)]),
                ),
                (
                    module_surface("app.v_u"),
                    parsed([DependencyEdge::Column(t.column("id"))]),
                ),
                (module_surface("app.v_mentions_ix"), parsed([])),
            ]),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::from([("ix_new".into(), CatalogName::Clear)]),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        };
        let normalize = |d: &str| pbps_dialect::Dialect::normalize_definition(&MinimalDialect, d);
        let eng = Engine {
            indexes_are_relations: true,
            normalize: &normalize,
            filter_binds: true,
        };
        let code_only = |d: &str| pbps_dialect::Dialect::code_only(&MinimalDialect, d);
        let lexicon = pbps_dialect::Dialect::lexicon(&MinimalDialect);
        let lexis = Lexis {
            code_only: &code_only,
            quoted_identifiers: lexicon.quoted_identifiers,
            continues_ident: lexicon.identifier_continues,
            reserved: lexicon.reserved,
            bare_rank: &pbps_model::module::every_schema,
        };
        let hooks = plain_hooks(&lexis, false);

        let drop_ix = ChangeSet {
            changes: vec![PlannedChange::new(Change::Index(IndexChange::DropIndex {
                table: t.clone(),
                name: "ix_old".into(),
            }))],
        };
        let found = assessed_with_facts(side, side, &drop_ix, &eng, &hooks, Some(&facts));
        assert_eq!(found.questions[&module_surface("app.v_t")], Answer::Resolve);
        assert_eq!(
            found.questions[&module_surface("app.v_u")],
            Answer::Unaffected
        );

        let add_ix = ChangeSet {
            changes: vec![PlannedChange::new(Change::Index(IndexChange::AddIndex {
                table: t,
                name: "ix_new".into(),
                index: Box::new(Index {
                    columns: Vec::new(),
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                    method: Default::default(),
                    storage_parameters: Default::default(),
                    filegroup: None,
                }),
                clustered: false,
            }))],
        };
        let found = assessed_with_facts(side, side, &add_ix, &eng, &hooks, Some(&facts));
        assert_eq!(
            found.questions[&module_surface("app.v_mentions_ix")],
            Answer::Resolve
        );
        assert_eq!(
            found.questions[&module_surface("app.v_u")],
            Answer::Unaffected
        );
    }

    /// A sequence a column owns is a relation the column and its table bring
    /// and take with them: a surface with an edge on it is not proven when the
    /// column or the table is dropped, nor when a table arrives with one of
    /// that name. Negative: a sequence the engine names, and a table with a
    /// relation the engine names, are matched by no name, so nothing is
    /// proven.
    #[test]
    fn an_owned_sequence_is_touched_and_arrives_with_its_column_and_table() {
        let owning = |name: Option<&str>| {
            let mut column = Column::new("int".parse().unwrap());
            column.value_source = ValueSource::Sequence(pbps_model::OwnedSequence {
                name: name.map(str::to_owned),
                ..pbps_model::OwnedSequence::serial()
            });
            column
        };
        let serial = || owning(Some("t_n_seq"));
        let mut schema = two_table_schema();
        let t: TableName = "app.t".parse().unwrap();
        schema
            .tables
            .get_mut(&t)
            .unwrap()
            .columns
            .insert("n".into(), serial());
        let facts = DependencyFacts {
            surfaces: BTreeMap::from([
                (
                    module_surface("app.v_t"),
                    parsed([DependencyEdge::Relation("app.t_n_seq".parse().unwrap())]),
                ),
                (
                    module_surface("app.v_u"),
                    parsed([DependencyEdge::Column(
                        "app.u".parse::<TableName>().unwrap().column("id"),
                    )]),
                ),
            ]),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::from([
                ("t".into(), CatalogName::Clear),
                ("t_n_seq".into(), CatalogName::Clear),
            ]),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        };
        let normalize = |d: &str| pbps_dialect::Dialect::normalize_definition(&MinimalDialect, d);
        let eng = Engine {
            indexes_are_relations: true,
            normalize: &normalize,
            filter_binds: true,
        };
        let assessed = |base: &Schema, changes: Vec<Change>, chooses_a_name: bool| {
            let ids = resolve_ids(base, &IdsFile::default());
            let side = crate::Side {
                schema: base,
                ids: &ids,
            };
            assessed_with_facts(
                side,
                side,
                &ChangeSet {
                    changes: changes.into_iter().map(PlannedChange::new).collect(),
                },
                &eng,
                &plain_hooks(&pbps_model::module::SHARED, chooses_a_name),
                Some(&facts),
            )
        };
        let drop_column = || {
            vec![Change::Column(ColumnChange::DropColumn {
                uid: Uid::generate(UidKind::Column),
                column: t.column("n"),
            })]
        };
        let mut arriving = Table::default();
        arriving.columns.insert("n".into(), serial());
        let plans = [
            ("drop column", drop_column()),
            (
                "drop table",
                vec![Change::Table(TableChange::DropTable {
                    uid: Uid::generate(UidKind::Table),
                    name: "app.u".parse().unwrap(),
                    detach_from: None,
                })],
            ),
            (
                "create table",
                vec![Change::Table(TableChange::CreateTable {
                    uid: Uid::generate(UidKind::Table),
                    // In another schema: only the sequence's name can match.
                    name: "early.t".parse().unwrap(),
                    table: Box::new(arriving),
                    beside_default: None,
                    moves_at_most: None,
                })],
            ),
        ];
        for (label, changes) in plans {
            let found = assessed(&schema, changes, false);
            let (hit, miss) = if label == "drop table" {
                // `app.u` holds no sequence: its reader is touched by the
                // drop, and the sequence's reader is proven.
                ("app.v_u", "app.v_t")
            } else {
                ("app.v_t", "app.v_u")
            };
            assert_eq!(
                found.questions[&module_surface(hit)],
                Answer::Resolve,
                "{label}"
            );
            assert_eq!(
                found.unaffected_reason(&module_surface(miss)),
                Some(UnaffectedReason::ByDependency),
                "{label}"
            );
        }
        // A table holding the sequence takes it when it goes.
        let drop_t = || {
            vec![Change::Table(TableChange::DropTable {
                uid: Uid::generate(UidKind::Table),
                name: t.clone(),
                detach_from: None,
            })]
        };
        let found = assessed(&schema, drop_t(), false);
        assert_eq!(found.questions[&module_surface("app.v_t")], Answer::Resolve);
        assert_eq!(
            found.unaffected_reason(&module_surface("app.v_u")),
            Some(UnaffectedReason::ByDependency)
        );
        // A relation the engine names with the table proves nothing.
        let found = assessed(&schema, drop_t(), true);
        assert_eq!(found.count(Answer::Unaffected), 0);
        // Nor does a sequence the engine names.
        let mut unnamed = schema.clone();
        unnamed
            .tables
            .get_mut(&t)
            .unwrap()
            .columns
            .insert("n".into(), owning(None));
        let found = assessed(&unnamed, drop_column(), false);
        assert_eq!(found.count(Answer::Unaffected), 0);
    }

    /// An unparsed routine's header is bound at creation: its edges are
    /// checked as a parsed surface's. An arriving overload of a function a
    /// parameter default calls, and a change to a table a parameter is typed
    /// by, each leave it a question; an unrelated plan does not.
    #[test]
    fn an_unparsed_routine_is_held_by_the_edges_of_its_header() {
        let mut schema = two_table_schema();
        schema.modules.insert(
            "app.f_header()".parse().unwrap(),
            routine("RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END; $$"),
        );
        let facts = DependencyFacts {
            surfaces: BTreeMap::from([(
                module_surface("app.f_header()"),
                SurfaceDependencies::Unparsed(BTreeSet::from([
                    DependencyEdge::Routine("app.g".parse().unwrap()),
                    DependencyEdge::Type("app.t".parse().unwrap()),
                ])),
            )]),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::from([
                ("g".into(), CatalogName::Clear),
                ("h".into(), CatalogName::Clear),
            ]),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        };
        let arriving = |sig: &str| {
            vec![Change::Module(ModuleChange::CreateModule {
                id: sig.parse().unwrap(),
                module: Box::new(routine("RETURNS int LANGUAGE sql RETURN 1")),
            })]
        };
        let plans = [
            (
                "an overload of the default's function",
                arriving("early.g(int)"),
                false,
            ),
            (
                "a column of the parameter's table",
                vec![add_column("app.t", "extra")],
                false,
            ),
            ("another function", arriving("early.h(int)"), true),
            (
                "a column of another table",
                vec![add_column("app.u", "extra")],
                true,
            ),
        ];
        for (label, changes, proven) in plans {
            let found = run(&schema, &schema, changes, Some(&facts));
            assert_eq!(
                found.unaffected_reason(&module_surface("app.f_header()")),
                proven.then_some(UnaffectedReason::Unparsed),
                "{label}"
            );
        }
    }

    /// A table moved to another schema carries its indexes and owned
    /// sequences: each leaves the old schema's namespace and arrives in the
    /// new one's. A surface bound to a relation of that name elsewhere, and
    /// one bound to the carried relation, stay questions; a rename within a
    /// schema carries nothing.
    #[test]
    fn a_table_moved_to_another_schema_carries_its_relations_with_it() {
        let mut schema = two_table_schema();
        let t: TableName = "app.t".parse().unwrap();
        let mut owning = Column::new("int".parse().unwrap());
        owning.value_source = ValueSource::Sequence(pbps_model::OwnedSequence {
            name: Some("s".into()),
            ..pbps_model::OwnedSequence::serial()
        });
        schema
            .tables
            .get_mut(&t)
            .unwrap()
            .columns
            .insert("n".into(), owning);
        for (id, text) in [
            ("app.v_later", "SELECT 1 AS later"),
            ("app.v_carried", "SELECT 2 AS carried"),
            ("app.v_other", "SELECT 3 AS other"),
        ] {
            schema.modules.insert(id.parse().unwrap(), view(text));
        }
        let base_ids = resolve_ids(&schema, &IdsFile::default());
        let side = crate::Side {
            schema: &schema,
            ids: &base_ids,
        };
        let facts = DependencyFacts {
            surfaces: BTreeMap::from([
                (
                    module_surface("app.v_later"),
                    parsed([DependencyEdge::Relation("late.s".parse().unwrap())]),
                ),
                (
                    module_surface("app.v_carried"),
                    parsed([DependencyEdge::Relation("app.s".parse().unwrap())]),
                ),
                (
                    module_surface("app.v_other"),
                    parsed([DependencyEdge::Column(
                        "app.u".parse::<TableName>().unwrap().column("id"),
                    )]),
                ),
            ]),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::from([
                ("t".into(), CatalogName::Clear),
                ("t2".into(), CatalogName::Clear),
                ("s".into(), CatalogName::Clear),
            ]),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        };
        let normalize = |d: &str| pbps_dialect::Dialect::normalize_definition(&MinimalDialect, d);
        let eng = Engine {
            indexes_are_relations: true,
            normalize: &normalize,
            filter_binds: true,
        };
        let renamed = |to: &str| {
            assessed_with_facts(
                side,
                side,
                &ChangeSet {
                    changes: vec![PlannedChange::new(Change::Table(
                        TableChange::RenameTable {
                            uid: Uid::generate(UidKind::Table),
                            from: t.clone(),
                            to: to.parse().unwrap(),
                            defaults: Vec::new(),
                        },
                    ))],
                },
                &eng,
                &plain_hooks(&pbps_model::module::SHARED, false),
                Some(&facts),
            )
        };
        let moved = renamed("early.t");
        for surface in ["app.v_later", "app.v_carried"] {
            assert_eq!(moved.questions[&module_surface(surface)], Answer::Resolve);
        }
        assert_eq!(
            moved.unaffected_reason(&module_surface("app.v_other")),
            Some(UnaffectedReason::ByDependency)
        );
        let within = renamed("app.t2");
        for surface in ["app.v_later", "app.v_carried", "app.v_other"] {
            assert_eq!(
                within.unaffected_reason(&module_surface(surface)),
                Some(UnaffectedReason::ByDependency),
                "{surface}"
            );
        }
    }

    #[test]
    fn needed_facts_returns_none_when_no_surface_can_be_narrowed_and_lists_only_asked_surfaces() {
        let base = two_table_schema();
        let base_ids = resolve_ids(&base, &IdsFile::default());
        let side = crate::Side {
            schema: &base,
            ids: &base_ids,
        };
        // Non-moving plan -> None (no query needed).
        let grant_only = ChangeSet {
            changes: vec![PlannedChange::new(Change::Permission(GrantChange::Grant {
                role: "r".into(),
                target: GrantTarget::Object("app.t".parse().unwrap()),
                permissions: BTreeSet::from([Permission::Select]),
            }))],
        };
        assert_eq!(needed_facts(side, side, &grant_only, &MinimalDialect), None);

        // Excluded class -> None.
        let schema_grant = ChangeSet {
            changes: vec![PlannedChange::new(Change::Permission(GrantChange::Grant {
                role: "r".into(),
                target: GrantTarget::Schema("app".into()),
                permissions: BTreeSet::from([Permission::Usage]),
            }))],
        };
        assert_eq!(
            needed_facts(side, side, &schema_grant, &MinimalDialect),
            None
        );

        // Moving plan with one rebuilt view and one kept view -> asks only for the kept view.
        let mut desired = base.clone();
        desired
            .modules
            .insert("app.v_t".parse().unwrap(), view("SELECT id, k FROM app.t"));
        let desired_ids = resolve_ids(&desired, &base_ids);
        let desired_side = crate::Side {
            schema: &desired,
            ids: &desired_ids,
        };
        let ordinary = ChangeSet {
            changes: vec![
                PlannedChange::new(add_column("app.t", "extra")),
                PlannedChange::new(Change::Module(ModuleChange::AlterModule {
                    id: "app.v_t".parse().unwrap(),
                    module: Box::new(view("SELECT id, k FROM app.t")),
                })),
            ],
        };
        let needed = needed_facts(side, desired_side, &ordinary, &MinimalDialect).unwrap();
        assert_eq!(needed.surfaces, BTreeSet::from([module_surface("app.v_u")]));
        assert!(needed.arriving_names.is_empty());
        assert!(needed.touched_engine_named.is_empty());
        assert!(needed.arriving_candidates.is_empty());
    }

    /// An index on a partitioned table, or on a partition, has copies the
    /// engine names and drops with it: nothing here names them, so such a
    /// plan is not narrowed. The same index on a plain table is.
    #[test]
    fn an_index_change_on_a_partitioned_table_is_not_narrowed() {
        let plain = two_table_schema();
        let table: TableName = "app.t".parse().unwrap();
        let mut parent = plain.clone();
        parent.tables.get_mut(&table).unwrap().partition_by = Some(pbps_model::PartitionBy {
            external: false,
            columns: vec!["id".into()],
            strategy: Default::default(),
        });
        let normalize = |d: &str| pbps_dialect::Dialect::normalize_definition(&MinimalDialect, d);
        let eng = Engine {
            indexes_are_relations: true,
            normalize: &normalize,
            filter_binds: true,
        };
        let hooks = plain_hooks(&pbps_model::module::SHARED, false);
        let facts = DependencyFacts {
            surfaces: BTreeMap::from([(
                module_surface("app.v_u"),
                parsed([DependencyEdge::Column(
                    "app.u".parse::<TableName>().unwrap().column("id"),
                )]),
            )]),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::new(),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::new(),
        };
        let changes = [
            Change::Index(IndexChange::DropIndex {
                table: table.clone(),
                name: "t_w".into(),
            }),
            Change::Constraint(ConstraintChange::DropUnique {
                table: table.clone(),
                name: "t_k".into(),
            }),
        ];
        for change in changes {
            let ordinary = ChangeSet {
                changes: vec![PlannedChange::new(change)],
            };
            let answer = |schema: &Schema| {
                let ids = resolve_ids(schema, &IdsFile::default());
                let side = crate::Side { schema, ids: &ids };
                assessed_with_facts(side, side, &ordinary, &eng, &hooks, Some(&facts)).questions
                    [&module_surface("app.v_u")]
            };
            assert_eq!(answer(&plain), Answer::Unaffected);
            assert_eq!(answer(&parent), Answer::Resolve);
        }
    }

    /// Two tables of one plan whose names clip to the same companion name:
    /// the second takes a fallback the facts cannot name, so nothing is
    /// narrowed, even with the first choice reported free.
    #[test]
    fn two_arrivals_wanting_one_companion_name_are_not_narrowed() {
        let schema = two_table_schema();
        let base_ids = resolve_ids(&schema, &IdsFile::default());
        let side = crate::Side {
            schema: &schema,
            ids: &base_ids,
        };
        let normalize = |d: &str| pbps_dialect::Dialect::normalize_definition(&MinimalDialect, d);
        let eng = Engine {
            indexes_are_relations: true,
            normalize: &normalize,
            filter_binds: true,
        };
        // Every name's companion is its first character, so `ta` and `tb`
        // want the same one and `x` wants another.
        let companion_types = |name: &ObjectName| {
            vec![ObjectName::new(
                &name.schema,
                format!("_{}", &name.name[..1]),
            )]
        };
        let hooks = Hooks {
            lexis: &pbps_model::module::SHARED,
            extra_implicit_relations: &|_, _| false,
            unnamed_pk_name: &|_| None,
            identity_seq_name: &|_, _| None,
            owned_seq_name: &|_, _, _| None,
            companion_types: &companion_types,
        };
        let create = |names: &[&str]| ChangeSet {
            changes: names
                .iter()
                .map(|name| {
                    PlannedChange::new(Change::Table(TableChange::CreateTable {
                        uid: Uid::generate(UidKind::Table),
                        name: name.parse().unwrap(),
                        table: Box::new(table_with_columns(&["id"])),
                        beside_default: None,
                        moves_at_most: None,
                    }))
                })
                .collect(),
        };
        let facts = DependencyFacts {
            surfaces: BTreeMap::from([(
                module_surface("app.v_u"),
                parsed([DependencyEdge::Column(
                    "app.u".parse::<TableName>().unwrap().column("id"),
                )]),
            )]),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: ["ta", "tb", "x", "_t", "_x"]
                .into_iter()
                .map(|n| (n.to_owned(), CatalogName::Clear))
                .collect(),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::from([
                (
                    CandidateName {
                        kind: NameSlotKind::CompanionType,
                        name: "early._t".parse().unwrap(),
                    },
                    NameAvailability::Free,
                ),
                (
                    CandidateName {
                        kind: NameSlotKind::CompanionType,
                        name: "early._x".parse().unwrap(),
                    },
                    NameAvailability::Free,
                ),
            ]),
        };
        let answer = |names: &[&str]| {
            assessed_with_facts(side, side, &create(names), &eng, &hooks, Some(&facts)).questions
                [&module_surface("app.v_u")]
        };
        assert_eq!(answer(&["early.ta", "early.x"]), Answer::Unaffected);
        assert_eq!(answer(&["early.ta", "early.tb"]), Answer::Resolve);
        // A relation declared under another arrival's companion name, in
        // either order.
        assert_eq!(answer(&["early.x", "early._x"]), Answer::Resolve);
        assert_eq!(answer(&["early._x", "early.x"]), Answer::Resolve);
    }

    #[test]
    fn arriving_companion_type_name_catches_bound_and_text_readers_and_refuses_when_held() {
        let mut schema = two_table_schema();
        schema
            .modules
            .insert("app.v_arr".parse().unwrap(), view("SELECT 1 AS a"));
        schema.modules.insert(
            "app.v_empty_arr".parse().unwrap(),
            view("SELECT 1 AS a WHERE ARRAY[]::_t[] IS NULL"),
        );
        let base_ids = resolve_ids(&schema, &IdsFile::default());
        let side = crate::Side {
            schema: &schema,
            ids: &base_ids,
        };
        let normalize = |d: &str| pbps_dialect::Dialect::normalize_definition(&MinimalDialect, d);
        let eng = Engine {
            indexes_are_relations: true,
            normalize: &normalize,
            filter_binds: true,
        };
        let companion_types =
            |name: &ObjectName| vec![ObjectName::new(&name.schema, format!("_{}", name.name))];
        let hooks = Hooks {
            lexis: &pbps_model::module::SHARED,
            extra_implicit_relations: &|_, _| false,
            unnamed_pk_name: &|_| None,
            identity_seq_name: &|_, _| None,
            owned_seq_name: &|_, _, _| None,
            companion_types: &companion_types,
        };
        let create = ChangeSet {
            changes: vec![PlannedChange::new(Change::Table(
                TableChange::CreateTable {
                    uid: Uid::generate(UidKind::Table),
                    name: "early.t".parse().unwrap(),
                    table: Box::new(table_with_columns(&["id"])),
                    beside_default: None,
                    moves_at_most: None,
                },
            ))],
        };
        let cand = CandidateName {
            kind: NameSlotKind::CompanionType,
            name: "early._t".parse().unwrap(),
        };
        let make_facts = |avail: Option<NameAvailability>| DependencyFacts {
            surfaces: BTreeMap::from([
                (
                    module_surface("app.v_arr"),
                    parsed([DependencyEdge::Type("late._t".parse().unwrap())]),
                ),
                (module_surface("app.v_empty_arr"), parsed([])),
                (
                    module_surface("app.v_u"),
                    parsed([DependencyEdge::Column(
                        "app.u".parse::<TableName>().unwrap().column("id"),
                    )]),
                ),
                (
                    module_surface("app.v_t"),
                    parsed([DependencyEdge::Column(
                        "app.t".parse::<TableName>().unwrap().column("id"),
                    )]),
                ),
            ]),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::from([
                ("t".into(), CatalogName::Clear),
                ("_t".into(), CatalogName::Clear),
            ]),
            engine_named_relations: BTreeMap::new(),
            candidate_names: avail
                .map(|a| BTreeMap::from([(cand.clone(), a)]))
                .unwrap_or_default(),
        };
        // First choice `_t` is free: readers of `late._t` and text mentions of `_t`
        // stay `Resolve`, while unrelated `v_u` is proven `Unaffected`.
        let free = make_facts(Some(NameAvailability::Free));
        let found = assessed_with_facts(side, side, &create, &eng, &hooks, Some(&free));
        assert_eq!(
            found.questions[&module_surface("app.v_arr")],
            Answer::Resolve
        );
        assert_eq!(
            found.questions[&module_surface("app.v_empty_arr")],
            Answer::Resolve
        );
        assert_eq!(
            found.unaffected_reason(&module_surface("app.v_u")),
            Some(UnaffectedReason::ByDependency)
        );
        // Negative: when `_t` is held, unreadable, or unchecked, nothing is narrowed.
        for bad in [
            Some(NameAvailability::Held),
            Some(NameAvailability::Unreadable),
            None,
        ] {
            let f = make_facts(bad);
            let found = assessed_with_facts(side, side, &create, &eng, &hooks, Some(&f));
            assert_eq!(found.count(Answer::Unaffected), 0, "{bad:?}");
        }
    }

    #[test]
    fn engine_named_relations_narrow_when_known_and_free_and_refuse_when_held_or_unknown() {
        let mut schema = two_table_schema();
        let t: TableName = "app.t".parse().unwrap();
        let mut id_col = Column::new("int".parse().unwrap());
        id_col.value_source = ValueSource::Identity(pbps_model::Identity::new(1, 1));
        schema
            .tables
            .get_mut(&t)
            .unwrap()
            .columns
            .insert("id".into(), id_col.clone());
        schema.tables.get_mut(&t).unwrap().primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
            filegroup: None,
        });
        let base_ids = resolve_ids(&schema, &IdsFile::default());
        let side = crate::Side {
            schema: &schema,
            ids: &base_ids,
        };
        let normalize = |d: &str| pbps_dialect::Dialect::normalize_definition(&MinimalDialect, d);
        let eng = Engine {
            indexes_are_relations: true,
            normalize: &normalize,
            filter_binds: true,
        };
        let pk_name = |tbl: &TableName| Some(format!("{}_pkey", tbl.name));
        let seq_name = |tbl: &TableName, col: &str| Some(format!("{}_{col}_seq", tbl.name));
        let owned_name = |tbl: &TableName, col: &str, seq: &OwnedSequence| {
            Some(
                seq.name
                    .clone()
                    .unwrap_or_else(|| format!("{}_{col}_seq", tbl.name)),
            )
        };
        let companion_types =
            |name: &ObjectName| vec![ObjectName::new(&name.schema, format!("_{}", name.name))];
        let hooks = Hooks {
            lexis: &pbps_model::module::SHARED,
            extra_implicit_relations: &|_, _| false,
            unnamed_pk_name: &pk_name,
            identity_seq_name: &seq_name,
            owned_seq_name: &owned_name,
            companion_types: &companion_types,
        };

        // 1. Arriving table with unnamed PK, identity column, and unnamed owned
        //    sequence: sequences and indexes bring no companion array type.
        let mut arriving = Table::default();
        arriving.columns.insert("id".into(), id_col);
        let mut ser_col = Column::new("int".parse().unwrap());
        ser_col.value_source = ValueSource::Sequence(OwnedSequence::serial());
        arriving.columns.insert("n".into(), ser_col);
        arriving.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
            filegroup: None,
        });
        let create = ChangeSet {
            changes: vec![PlannedChange::new(Change::Table(
                TableChange::CreateTable {
                    uid: Uid::generate(UidKind::Table),
                    name: "early.fresh".parse().unwrap(),
                    table: Box::new(arriving),
                    beside_default: None,
                    moves_at_most: None,
                },
            ))],
        };
        let effects = PlanEffects::of(&create, side, side, &eng, &hooks);
        let pk_cand = CandidateName {
            kind: NameSlotKind::ConstraintRelation,
            name: "early.fresh_pkey".parse().unwrap(),
        };
        let seq_cand = CandidateName {
            kind: NameSlotKind::Relation,
            name: "early.fresh_id_seq".parse().unwrap(),
        };
        let ser_cand = CandidateName {
            kind: NameSlotKind::Relation,
            name: "early.fresh_n_seq".parse().unwrap(),
        };
        let table_comp = CandidateName {
            kind: NameSlotKind::CompanionType,
            name: "early._fresh".parse().unwrap(),
        };
        // Only the table brings an array type; the PK index and sequences do not.
        assert_eq!(
            effects.arriving_candidates,
            BTreeSet::from([
                table_comp.clone(),
                pk_cand.clone(),
                seq_cand.clone(),
                ser_cand.clone(),
            ])
        );
        let mut facts = DependencyFacts {
            surfaces: BTreeMap::from([
                (
                    module_surface("app.v_t"),
                    parsed([DependencyEdge::Relation("late.fresh_pkey".parse().unwrap())]),
                ),
                (
                    module_surface("app.v_u"),
                    parsed([DependencyEdge::Column(
                        "app.u".parse::<TableName>().unwrap().column("id"),
                    )]),
                ),
            ]),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: [
                "fresh",
                "_fresh",
                "fresh_pkey",
                "fresh_id_seq",
                "fresh_n_seq",
            ]
            .into_iter()
            .map(|n| (n.to_owned(), CatalogName::Clear))
            .collect(),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::from([
                (table_comp, NameAvailability::Free),
                (pk_cand.clone(), NameAvailability::Free),
                (seq_cand, NameAvailability::Free),
                (ser_cand, NameAvailability::Free),
            ]),
        };
        let found = assessed_with_facts(side, side, &create, &eng, &hooks, Some(&facts));
        assert_eq!(found.questions[&module_surface("app.v_t")], Answer::Resolve);
        assert_eq!(
            found.unaffected_reason(&module_surface("app.v_u")),
            Some(UnaffectedReason::ByDependency)
        );
        // Held first choice refuses narrowing for the whole plan.
        facts
            .candidate_names
            .insert(pk_cand, NameAvailability::Held);
        let held = assessed_with_facts(side, side, &create, &eng, &hooks, Some(&facts));
        assert_eq!(held.count(Answer::Unaffected), 0);

        // 2. Touched (dropped) table with unnamed PK and identity column whose
        //    actual catalog names were suffixed (`t_pkey1`, `t_id_seq1`).
        let drop_t = ChangeSet {
            changes: vec![PlannedChange::new(Change::Table(TableChange::DropTable {
                uid: Uid::generate(UidKind::Table),
                name: t.clone(),
                detach_from: None,
            }))],
        };
        let mut drop_facts = DependencyFacts {
            surfaces: BTreeMap::from([
                (
                    module_surface("app.v_t"),
                    parsed([DependencyEdge::Relation("app.t_id_seq1".parse().unwrap())]),
                ),
                (
                    module_surface("app.v_u"),
                    parsed([DependencyEdge::Column(
                        "app.u".parse::<TableName>().unwrap().column("id"),
                    )]),
                ),
            ]),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: BTreeMap::new(),
            engine_named_relations: BTreeMap::from([
                (
                    EngineNamedRelation::PrimaryKey(t.clone()),
                    ReadFact::Known("app.t_pkey1".parse().unwrap()),
                ),
                (
                    EngineNamedRelation::ColumnSequence(t.column("id")),
                    ReadFact::Known("app.t_id_seq1".parse().unwrap()),
                ),
            ]),
            candidate_names: BTreeMap::new(),
        };
        let found = assessed_with_facts(side, side, &drop_t, &eng, &hooks, Some(&drop_facts));
        assert_eq!(found.questions[&module_surface("app.v_t")], Answer::Resolve);
        assert_eq!(
            found.unaffected_reason(&module_surface("app.v_u")),
            Some(UnaffectedReason::ByDependency)
        );
        // Unreadable actual name refuses narrowing completely.
        drop_facts.engine_named_relations.insert(
            EngineNamedRelation::PrimaryKey(t.clone()),
            ReadFact::Unreadable,
        );
        let unread = assessed_with_facts(side, side, &drop_t, &eng, &hooks, Some(&drop_facts));
        assert_eq!(unread.count(Answer::Unaffected), 0);

        // 3. Cross-schema move carries the actual engine-named relation names
        //    into the destination schema and refuses if a moved actual name
        //    lands on another arrival's predicted first choice.
        let move_t = ChangeSet {
            changes: vec![PlannedChange::new(Change::Table(
                TableChange::RenameTable {
                    uid: Uid::generate(UidKind::Table),
                    from: t.clone(),
                    to: "early.moved".parse().unwrap(),
                    defaults: Vec::new(),
                },
            ))],
        };
        let move_facts = DependencyFacts {
            surfaces: BTreeMap::from([
                (
                    module_surface("app.v_t"),
                    parsed([DependencyEdge::Relation("late.t_pkey1".parse().unwrap())]),
                ),
                (
                    module_surface("app.v_u"),
                    parsed([DependencyEdge::Column(
                        "app.u".parse::<TableName>().unwrap().column("id"),
                    )]),
                ),
            ]),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: ["moved", "_moved", "t_pkey1", "t_id_seq1"]
                .into_iter()
                .map(|n| (n.to_owned(), CatalogName::Clear))
                .collect(),
            engine_named_relations: BTreeMap::from([
                (
                    EngineNamedRelation::PrimaryKey(t.clone()),
                    ReadFact::Known("app.t_pkey1".parse().unwrap()),
                ),
                (
                    EngineNamedRelation::ColumnSequence(t.column("id")),
                    ReadFact::Known("app.t_id_seq1".parse().unwrap()),
                ),
            ]),
            candidate_names: BTreeMap::from([(
                CandidateName {
                    kind: NameSlotKind::CompanionType,
                    name: "early._moved".parse().unwrap(),
                },
                NameAvailability::Free,
            )]),
        };
        let moved = assessed_with_facts(side, side, &move_t, &eng, &hooks, Some(&move_facts));
        assert_eq!(moved.questions[&module_surface("app.v_t")], Answer::Resolve);
        assert_eq!(
            moved.unaffected_reason(&module_surface("app.v_u")),
            Some(UnaffectedReason::ByDependency)
        );
        // Same move paired with a new table whose unnamed PK wants the moved
        // actual name (`early.t_pkey1`): refuses narrowing because the moved
        // PK forces the arriving table's unnamed PK onto a fallback.
        let mut collided_tbl = table_with_columns(&["id"]);
        collided_tbl.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
            filegroup: None,
        });
        let move_and_collide = ChangeSet {
            changes: vec![
                move_t.changes[0].clone(),
                PlannedChange::new(Change::Table(TableChange::CreateTable {
                    uid: Uid::generate(UidKind::Table),
                    name: "early.other".parse().unwrap(),
                    table: Box::new(collided_tbl),
                    beside_default: None,
                    moves_at_most: None,
                })),
            ],
        };
        let pk_colliding = |tbl: &TableName| {
            if tbl.name == "other" {
                Some("t_pkey1".to_owned())
            } else {
                Some(format!("{}_pkey", tbl.name))
            }
        };
        let collide_hooks = Hooks {
            unnamed_pk_name: &pk_colliding,
            ..hooks
        };
        let mut move_collide_facts = move_facts;
        move_collide_facts
            .catalog_names
            .insert("other".into(), CatalogName::Clear);
        move_collide_facts
            .catalog_names
            .insert("_other".into(), CatalogName::Clear);
        move_collide_facts.candidate_names.insert(
            CandidateName {
                kind: NameSlotKind::CompanionType,
                name: "early._other".parse().unwrap(),
            },
            NameAvailability::Free,
        );
        move_collide_facts.candidate_names.insert(
            CandidateName {
                kind: NameSlotKind::ConstraintRelation,
                name: "early.t_pkey1".parse().unwrap(),
            },
            NameAvailability::Free,
        );
        let collided = assessed_with_facts(
            side,
            side,
            &move_and_collide,
            &eng,
            &collide_hooks,
            Some(&move_collide_facts),
        );
        assert_eq!(collided.count(Answer::Unaffected), 0);
    }

    #[test]
    fn within_plan_candidate_collisions_keep_plan_level_answer() {
        let schema = two_table_schema();
        let base_ids = resolve_ids(&schema, &IdsFile::default());
        let side = crate::Side {
            schema: &schema,
            ids: &base_ids,
        };
        let normalize = |d: &str| pbps_dialect::Dialect::normalize_definition(&MinimalDialect, d);
        let eng = Engine {
            indexes_are_relations: true,
            normalize: &normalize,
            filter_binds: true,
        };
        // Short clipping rules so collisions are readable in small fixtures:
        // PK uses the first char (`<c>_pkey`), identity seq uses `<tbl>_<col>_seq`,
        // companion uses `_<name>`.
        let pk_name = |tbl: &TableName| Some(format!("{}_pkey", &tbl.name[..1]));
        let seq_name = |tbl: &TableName, col: &str| Some(format!("{}_{col}_seq", tbl.name));
        let companion_types =
            |name: &ObjectName| vec![ObjectName::new(&name.schema, format!("_{}", name.name))];
        let hooks = Hooks {
            lexis: &pbps_model::module::SHARED,
            extra_implicit_relations: &|_, _| false,
            unnamed_pk_name: &pk_name,
            identity_seq_name: &seq_name,
            owned_seq_name: &|_, _, _| None,
            companion_types: &companion_types,
        };
        let table_with_unnamed_pk = || {
            let mut tbl = table_with_columns(&["id"]);
            tbl.primary_key = Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
                filegroup: None,
            });
            tbl
        };
        let table_with_identity = || {
            let mut tbl = Table::default();
            let mut id_col = Column::new("int".parse().unwrap());
            id_col.value_source = ValueSource::Identity(pbps_model::Identity::new(1, 1));
            tbl.columns.insert("id".into(), id_col);
            tbl
        };
        let create_tbl = |name: &str, tbl: Table| {
            PlannedChange::new(Change::Table(TableChange::CreateTable {
                uid: Uid::generate(UidKind::Table),
                name: name.parse().unwrap(),
                table: Box::new(tbl),
                beside_default: None,
                moves_at_most: None,
            }))
        };
        let add_check = |table: &str, name: &str| {
            PlannedChange::new(Change::Constraint(ConstraintChange::AddCheck {
                table: table.parse().unwrap(),
                name: name.into(),
                constraint: CheckConstraint {
                    expression: "id > 0".into(),
                },
            }))
        };
        let facts = DependencyFacts {
            surfaces: BTreeMap::from([(
                module_surface("app.v_u"),
                parsed([DependencyEdge::Column(
                    "app.u".parse::<TableName>().unwrap().column("id"),
                )]),
            )]),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: [
                "a1",
                "a2",
                "b1",
                "a_pkey",
                "b_pkey",
                "b1_id_seq",
                "_a1",
                "_a2",
                "_b1",
                "_a_pkey",
                "_b1_id_seq",
            ]
            .into_iter()
            .map(|n| (n.to_owned(), CatalogName::Clear))
            .collect(),
            engine_named_relations: BTreeMap::new(),
            candidate_names: BTreeMap::from([
                (
                    CandidateName {
                        kind: NameSlotKind::ConstraintRelation,
                        name: "early.a_pkey".parse().unwrap(),
                    },
                    NameAvailability::Free,
                ),
                (
                    CandidateName {
                        kind: NameSlotKind::ConstraintRelation,
                        name: "early.b_pkey".parse().unwrap(),
                    },
                    NameAvailability::Free,
                ),
                (
                    CandidateName {
                        kind: NameSlotKind::Relation,
                        name: "early.b1_id_seq".parse().unwrap(),
                    },
                    NameAvailability::Free,
                ),
                (
                    CandidateName {
                        kind: NameSlotKind::CompanionType,
                        name: "early._a1".parse().unwrap(),
                    },
                    NameAvailability::Free,
                ),
                (
                    CandidateName {
                        kind: NameSlotKind::CompanionType,
                        name: "early._a2".parse().unwrap(),
                    },
                    NameAvailability::Free,
                ),
                (
                    CandidateName {
                        kind: NameSlotKind::CompanionType,
                        name: "early._b1".parse().unwrap(),
                    },
                    NameAvailability::Free,
                ),
                (
                    CandidateName {
                        kind: NameSlotKind::CompanionType,
                        name: "early._a_pkey".parse().unwrap(),
                    },
                    NameAvailability::Free,
                ),
                (
                    CandidateName {
                        kind: NameSlotKind::CompanionType,
                        name: "early._b1_id_seq".parse().unwrap(),
                    },
                    NameAvailability::Free,
                ),
            ]),
        };
        let answer = |changes: Vec<PlannedChange>| {
            let cs = ChangeSet { changes };
            assessed_with_facts(side, side, &cs, &eng, &hooks, Some(&facts)).questions
                [&module_surface("app.v_u")]
        };

        // Positive: two tables whose unnamed PKs take distinct names narrow.
        assert_eq!(
            answer(vec![
                create_tbl("early.a1", table_with_unnamed_pk()),
                create_tbl("early.b1", table_with_unnamed_pk()),
            ]),
            Answer::Unaffected
        );
        // Two tables whose unnamed PKs want the same first choice collide.
        assert_eq!(
            answer(vec![
                create_tbl("early.a1", table_with_unnamed_pk()),
                create_tbl("early.a2", table_with_unnamed_pk()),
            ]),
            Answer::Resolve
        );
        // Unnamed PK colliding with an explicit arrival, in either order.
        assert_eq!(
            answer(vec![
                create_tbl("early.a1", table_with_unnamed_pk()),
                create_tbl("early.a_pkey", table_with_columns(&["id"])),
            ]),
            Answer::Resolve
        );
        assert_eq!(
            answer(vec![
                create_tbl("early.a_pkey", table_with_columns(&["id"])),
                create_tbl("early.a1", table_with_unnamed_pk()),
            ]),
            Answer::Resolve
        );
        // Unnamed PK colliding with an arriving non-relation constraint, in either order.
        assert_eq!(
            answer(vec![
                create_tbl("early.a1", table_with_unnamed_pk()),
                add_check("early.other", "a_pkey"),
            ]),
            Answer::Resolve
        );
        assert_eq!(
            answer(vec![
                add_check("early.other", "a_pkey"),
                create_tbl("early.a1", table_with_unnamed_pk()),
            ]),
            Answer::Resolve
        );
        // Identity sequence colliding with an explicit arrival, in either order.
        assert_eq!(
            answer(vec![
                create_tbl("early.b1", table_with_identity()),
                create_tbl("early.b1_id_seq", table_with_columns(&["id"])),
            ]),
            Answer::Resolve
        );
        assert_eq!(
            answer(vec![
                create_tbl("early.b1_id_seq", table_with_columns(&["id"])),
                create_tbl("early.b1", table_with_identity()),
            ]),
            Answer::Resolve
        );
        // An identity sequence does not yield to a non-relation CHECK constraint.
        assert_eq!(
            answer(vec![
                create_tbl("early.b1", table_with_identity()),
                add_check("early.other", "b1_id_seq"),
            ]),
            Answer::Unaffected
        );
        // Cross-kind collision: a companion array type (`_a_pkey` from table
        // `early.a_pkey`) and an unnamed PK wanting `_a_pkey` collide via the
        // same mechanism, and `needed_facts` returns `None` when colliding.
        let pk_underscore = |_: &TableName| Some("_a_pkey".to_owned());
        let cross_hooks = Hooks {
            unnamed_pk_name: &pk_underscore,
            ..hooks
        };
        let cross_cs = ChangeSet {
            changes: vec![
                create_tbl("early.a_pkey", table_with_columns(&["id"])),
                create_tbl("early.b1", table_with_unnamed_pk()),
            ],
        };
        let cross_effects = PlanEffects::of(&cross_cs, side, side, &eng, &cross_hooks);
        assert!(cross_effects.exclude_narrowing);
        assert_eq!(
            assessed_with_facts(side, side, &cross_cs, &eng, &cross_hooks, Some(&facts)).questions
                [&module_surface("app.v_u")],
            Answer::Resolve
        );

        // `SetPrimaryKey` dropping an unnamed PK touches `PrimaryKey(table)`,
        // and adding an unnamed PK requests a `ConstraintRelation` candidate.
        let unnamed_pk = PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
            filegroup: None,
        };
        let replace_pk = ChangeSet {
            changes: vec![PlannedChange::new(Change::Constraint(
                ConstraintChange::SetPrimaryKey {
                    table: "early.a1".parse().unwrap(),
                    from: Some(unnamed_pk.clone()),
                    to: Some(unnamed_pk),
                    nonclustered: false,
                },
            ))],
        };
        let pk_effects = PlanEffects::of(&replace_pk, side, side, &eng, &hooks);
        assert!(
            pk_effects
                .touched_engine_named
                .contains_key(&EngineNamedRelation::PrimaryKey(
                    "early.a1".parse().unwrap()
                ))
        );
        assert!(pk_effects.arriving_candidates.contains(&CandidateName {
            kind: NameSlotKind::ConstraintRelation,
            name: "early.a_pkey".parse().unwrap(),
        }));
    }
}
