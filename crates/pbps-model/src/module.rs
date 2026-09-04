//! Modules: views, procedures, functions and triggers
//! ([ADR-0002](../../../docs/ADR-0002-module-model.md)).
//!
//! # Why these are not identity-tracked
//!
//! The identity machinery — uids, tombstones, human intent — exists because
//! columns carry data: a rename mistaken for a drop destroys it irreversibly.
//! A module carries no data. Dropping and recreating one is semantically
//! lossless and its complete definition is in git, so a rename is just drop +
//! add and the audit trail is the commit that did it. **Modules therefore never
//! appear in `schema.ids.json`.**
//!
//! That is a principled line, not an economy: the criterion is "does drop + add
//! destroy state that lives only in the environment?", and for a module the
//! answer is no.
//!
//! # What a definition holds
//!
//! The emitter composes the whole `CREATE OR ALTER` statement, so that SQL still
//! appears exactly once. What the user writes is everything after the part the
//! emitter can derive:
//!
//! | Kind | Emitted prefix | So `definition:` starts at |
//! |---|---|---|
//! | view | `CREATE OR ALTER VIEW <name> AS` | the `SELECT` |
//! | trigger | `CREATE OR ALTER TRIGGER <name> ON <on>` | `AFTER INSERT ...` |
//! | procedure | `CREATE OR ALTER PROCEDURE <name>` | the parameter list, then `AS` |
//! | function | `CREATE OR ALTER FUNCTION <name>` | the parameter list, then `RETURNS` |
//!
//! Procedures and functions keep their parameter list inside `definition`
//! because a parameter is part of the object's contract, and modelling T-SQL
//! parameter syntax would be parsing SQL — which this tool does not do (§8.2).

use std::collections::{BTreeMap, BTreeSet};

use crate::name::TableName;

/// The qualified name of a database object: `schema.object`.
///
/// Deliberately the same type as [`TableName`]: SQL Server keeps tables and
/// modules in **one** `sys.objects` namespace per schema, so "a view may not be
/// named after a table" is not a rule to remember but a consequence of the two
/// names having one type. [`check_names`] is that consequence made checkable.
pub type ObjectName = TableName;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ModuleKind {
    View,
    Procedure,
    Function,
    Trigger,
}

impl ModuleKind {
    /// The word the user writes as the file's leading key, and the one an error
    /// message uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            ModuleKind::View => "view",
            ModuleKind::Procedure => "procedure",
            ModuleKind::Function => "function",
            ModuleKind::Trigger => "trigger",
        }
    }

    pub const ALL: [ModuleKind; 4] = [
        ModuleKind::View,
        ModuleKind::Procedure,
        ModuleKind::Function,
        ModuleKind::Trigger,
    ];
}

impl std::fmt::Display for ModuleKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One view, procedure, function or trigger.
///
/// As everywhere else in the model, the container holds the name: a module's
/// name is the key in [`crate::Schema::modules`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Module {
    pub kind: ModuleKind,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// The table a trigger is attached to. `None` for every other kind.
    ///
    /// It is part of the state and not an annotation: moving a trigger to
    /// another table is a different object, and the engine cannot `ALTER` it
    /// across.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<ObjectName>,

    /// The body, kept verbatim and **never parsed**.
    ///
    /// The same rule as check and default expressions (SPEC §8.2): the database
    /// is the normalizer. After an apply the stored text is read back, so both
    /// sides of the drift check live in the engine's own space.
    pub definition: String,
}

/// Explicit creation-order edges, `module -> the modules it needs first`.
///
/// # Why this is not a field of [`Module`]
///
/// Order of creation is invisible in the database, so a `depends_on:` inside
/// the model would make a declared module compare unequal to the identical
/// module read back from the catalog — inviolable constraint 1, and drift
/// crying wolf on every run. It travels beside the model the way `strategy:`
/// does, and for the same reason: it says how to get there, not where to go.
pub type ModuleDeps = BTreeMap<ObjectName, BTreeSet<ObjectName>>;

/// The annotations that travel beside the model.
///
/// `pbps-load` returns them separately from the [`crate::Schema`], the differ
/// attaches or applies them, and neither can ever take part in a comparison of
/// two states.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hints {
    /// Per-table execution strategy (ADR-0003).
    pub strategies: crate::strategy::Strategies,
    /// Per-module creation-order edges (ADR-0002).
    pub module_deps: ModuleDeps,
}

/// Whether `definition` mentions `name`, by a best-effort identifier scan.
///
/// # Why scanning text does not break "expressions are never parsed"
///
/// §8.2's rule is about **comparison**: pbps must never decide that two
/// definitions differ by understanding what they say. This asks a much smaller
/// question — does this text contain this identifier — and its answer only
/// picks a creation order. The failure mode is safe by construction: a wrong
/// order fails the CREATE inside the plan's transaction, everything rolls back,
/// and the environment is unchanged. The escape hatch for the cases it gets
/// wrong is [`ModuleDeps`].
pub fn references(definition: &str, name: &ObjectName) -> bool {
    let haystack = scannable(definition);
    let schema = name.schema.to_ascii_lowercase();
    let object = name.name.to_ascii_lowercase();

    // The qualified form, and the bare one — a definition written inside its
    // own schema very often omits the qualifier.
    contains_word(&haystack, &format!("{schema}.{object}")) || contains_word(&haystack, &object)
}

/// The definition with everything that is not code blanked out.
///
/// String literals and both comment forms are replaced by spaces, character for
/// character, so line structure and offsets survive. A **quoted identifier**
/// (`[name]`, or `"name"` under QUOTED_IDENTIFIER ON, which is the only setting
/// pbps manages) is passed through untouched: it is a name, which is exactly
/// what the callers are looking for.
///
/// # Why this is still not parsing SQL
///
/// It answers only "is this position inside a literal or a comment", which
/// every SQL lexer agrees on and no dialect argues about. Nothing here
/// understands what the code *says* — that rule (§8.2) is about comparison, and
/// this feeds two questions that are not comparisons: which modules a
/// definition names, and whether it contains a `GO`. Both were asked of the raw
/// text before, where a name inside a comment invented a dependency edge and a
/// `GO` inside a literal refused a valid procedure.
pub fn code_only(definition: &str) -> String {
    lexical_code(definition, true)
}

/// The definition with literals, comments, and quoted identifiers blanked.
///
/// [`code_only`] retains quoted identifiers because dependency and batch scans
/// need their names. Keyword detection needs the opposite: `[null]` and
/// `"try_cast"` are identifiers, not the SQL constructs their contents happen
/// to spell.
pub(crate) fn code_without_quoted_identifiers(definition: &str) -> String {
    lexical_code(definition, false)
}

fn lexical_code(definition: &str, keep_quoted_identifiers: bool) -> String {
    enum At {
        Code,
        /// Inside `'...'`: blanked, because its contents are data.
        Literal,
        /// Inside `[...]` or `"..."`: its contents are a name.
        Ident(char),
        Line,
        /// Carrying how many characters have been consumed, so that the `*` of
        /// the opener cannot also close it (`/*/`).
        Block(usize),
    }
    let mut out = String::with_capacity(definition.len());
    let mut at = At::Code;
    let bytes = definition.as_bytes();
    // A newline always survives: it ends a line comment, and the `GO` check
    // reads lines.
    fn blank(out: &mut String, ch: char) {
        if ch == '\n' {
            out.push('\n');
        } else {
            for _ in 0..ch.len_utf8() {
                out.push(' ');
            }
        }
    }
    for (i, ch) in definition.char_indices() {
        match at {
            At::Literal => {
                // A doubled `''` needs no special case: the first closes and the
                // second opens again, and everything between is blanked either
                // way.
                if ch == '\'' {
                    at = At::Code;
                }
                blank(&mut out, ch);
            }
            At::Ident(q) => {
                if if q == '[' { ch == ']' } else { ch == q } {
                    at = At::Code;
                }
                if keep_quoted_identifiers {
                    out.push(ch);
                } else {
                    blank(&mut out, ch);
                }
            }
            At::Line => {
                if ch == '\n' {
                    at = At::Code;
                }
                blank(&mut out, ch);
            }
            At::Block(seen) => {
                at = if ch == '/' && seen >= 2 && bytes[i - 1] == b'*' {
                    At::Code
                } else {
                    At::Block(seen + 1)
                };
                blank(&mut out, ch);
            }
            At::Code => {
                let next = bytes.get(i + ch.len_utf8()).copied();
                match (ch, next) {
                    ('-', Some(b'-')) => {
                        at = At::Line;
                        blank(&mut out, ch);
                    }
                    ('/', Some(b'*')) => {
                        at = At::Block(0);
                        blank(&mut out, ch);
                    }
                    ('\'', _) => {
                        at = At::Literal;
                        blank(&mut out, ch);
                    }
                    ('[' | '"', _) => {
                        at = At::Ident(ch);
                        if keep_quoted_identifiers {
                            out.push(ch);
                        } else {
                            blank(&mut out, ch);
                        }
                    }
                    _ => out.push(ch),
                }
            }
        }
    }
    out
}

/// Lower-cases, drops the quoting characters and closes the gaps around dots,
/// so that `[Dbo] . [V]` and `dbo.v` become one string to search.
fn scannable(definition: &str) -> String {
    let lowered = code_only(definition).to_ascii_lowercase();
    let unquoted: String = lowered.chars().filter(|c| !"[]\"`".contains(*c)).collect();
    let mut out = String::with_capacity(unquoted.len());
    for (i, ch) in unquoted.char_indices() {
        if ch.is_whitespace() {
            let before = unquoted[..i].chars().next_back();
            let after = unquoted[i + ch.len_utf8()..]
                .chars()
                .find(|c| !c.is_whitespace());
            // Whitespace that only separates a qualifier from its dot is
            // noise; everywhere else it is a boundary and must be kept.
            if before == Some('.') || after == Some('.') {
                continue;
            }
        }
        out.push(ch);
    }
    out
}

/// Whether `needle` occurs with no identifier character on either side.
fn contains_word(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        let start = from + at;
        let end = start + needle.len();
        if !is_ident_char(haystack[..start].chars().next_back())
            && !is_ident_char(haystack[end..].chars().next())
        {
            return true;
        }
        from = end;
    }
    false
}

/// A dot counts: `dbo.active_customer` must not match inside
/// `sales.dbo.active_customer`, and `active_customer` must not match inside
/// `dbo.active_customer` — the qualified needle is tried first and answers that
/// case properly.
fn is_ident_char(c: Option<char>) -> bool {
    matches!(c, Some(c) if c.is_alphanumeric() || c == '_' || c == '@' || c == '#' || c == '.')
}

/// The order in which modules must be created: a module comes after everything
/// it references.
///
/// Cycles cannot be ordered, and the tool does not pretend otherwise: the
/// members of one are emitted in name order, which is deterministic, and the
/// engine has the last word inside the plan's transaction. (A genuine cycle
/// between views is not creatable by any order.)
pub fn creation_order(
    modules: &BTreeMap<ObjectName, Module>,
    deps: &ModuleDeps,
) -> Vec<ObjectName> {
    let names: Vec<ObjectName> = modules.keys().cloned().collect();

    // needs[a] = the modules `a` must follow.
    let mut needs: BTreeMap<&ObjectName, BTreeSet<&ObjectName>> = BTreeMap::new();
    for name in &names {
        let module = &modules[name];
        let mut set: BTreeSet<&ObjectName> = BTreeSet::new();
        for other in &names {
            if other == name {
                continue;
            }
            let declared = deps.get(name).is_some_and(|d| d.contains(other));
            // A trigger's target is not named in its definition — the emitter
            // writes it into the `ON` clause — so the `on:` has to be read
            // directly. It matters only when the target is itself a module: a
            // trigger on a view has to be created after that view.
            let attached = module.on.as_ref().is_some_and(|t| t == other);
            if declared || attached || references(&module.definition, other) {
                set.insert(other);
            }
        }
        // A trigger on a *table* plays no part here: tables are created by an
        // earlier ordering class in any case.
        needs.insert(name, set);
    }

    let mut done: BTreeSet<&ObjectName> = BTreeSet::new();
    let mut out: Vec<ObjectName> = Vec::new();
    // Kahn's algorithm, taking the name-least ready module each round so that
    // two runs over the same declarations produce the same plan.
    loop {
        let ready: Vec<&ObjectName> = names
            .iter()
            .filter(|n| !done.contains(n))
            .filter(|n| needs[*n].iter().all(|d| done.contains(d)))
            .collect();
        if ready.is_empty() {
            break;
        }
        for n in ready {
            done.insert(n);
            out.push(n.clone());
        }
    }
    // Whatever is left is in a cycle: deterministic order, and the engine
    // decides.
    for n in &names {
        if !done.contains(n) {
            out.push(n.clone());
        }
    }
    out
}

/// Problems that need the whole schema to see.
///
/// Both of these would otherwise surface only at apply time, as an engine error
/// on a database that is half-changed:
///
/// - a module named after a table (or after another module): SQL Server keeps
///   them in one namespace, so the `CREATE` fails;
/// - a trigger on a table nobody declares: the tool would be managing a trigger
///   on an object it does not manage, and a `pull` of that environment would
///   not reproduce it.
pub fn check_names(schema: &crate::schema::Schema) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, module) in &schema.modules {
        if schema.tables.contains_key(name) {
            problems.push(format!(
                "`{name}` is declared both as a table and as a {}; SQL Server keeps tables and \
                 modules in one namespace per schema",
                module.kind
            ));
        }
        match (module.kind, &module.on) {
            (ModuleKind::Trigger, None) => problems.push(format!(
                "trigger `{name}` does not say which table it is on (`on:`)"
            )),
            // A view is as valid a target as a table: SQL Server supports
            // `INSTEAD OF` triggers on views, and `pull` reconstructs the `on:`
            // from what it finds — so refusing one here would make a database
            // that has one impossible to round-trip.
            (ModuleKind::Trigger, Some(target))
                if !schema.tables.contains_key(target)
                    && !matches!(
                        schema.modules.get(target).map(|m| m.kind),
                        Some(ModuleKind::View)
                    ) =>
            {
                problems.push(format!(
                    "trigger `{name}` is on `{target}`, which is not declared here as a table or \
                     a view"
                ));
            }
            (ModuleKind::Trigger, Some(_)) => {}
            (kind, Some(table)) => problems.push(format!(
                "`{name}` is a {kind} and cannot be `on: {table}`; only a trigger names a table"
            )),
            (_, None) => {}
        }
        if module.definition.trim().is_empty() {
            problems.push(format!("`{name}` has an empty definition"));
        }
    }
    problems
}

/// Every `depends_on:` target has to be a declared module.
///
/// [`creation_order`] considers dependencies only between modules it is
/// iterating, so an unknown name is silently a no-op. The declaration then
/// passes every check while the ordering edge its author asked for does not
/// exist — and the failure surfaces much later, as an apply that emits the
/// dependent module first (the same argument as ADR-0003's rejection of unknown
/// `strategy:` keys).
pub fn check_dependencies(schema: &crate::schema::Schema, deps: &ModuleDeps) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, on) in deps {
        for target in on {
            if target == name {
                problems.push(format!("`{name}` lists itself in `depends_on`"));
            } else if !schema.modules.contains_key(target) {
                problems.push(format!(
                    "`{name}` depends on `{target}`, which is not a declared module; the ordering \
                     it asks for would silently not happen"
                ));
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Schema, Table};

    fn n(s: &str) -> ObjectName {
        s.parse().unwrap()
    }

    fn module(kind: ModuleKind, definition: &str) -> Module {
        Module {
            kind,
            description: None,
            on: None,
            definition: definition.to_owned(),
        }
    }

    fn view(definition: &str) -> Module {
        module(ModuleKind::View, definition)
    }

    #[test]
    fn a_qualified_reference_is_found_in_every_spelling() {
        for definition in [
            "SELECT * FROM dbo.active_customer",
            "SELECT * FROM [dbo].[active_customer]",
            "select a from DBO.ACTIVE_CUSTOMER c",
            "SELECT * FROM active_customer",
        ] {
            assert!(
                references(definition, &n("dbo.active_customer")),
                "{definition}"
            );
        }
    }

    /// A name that merely appears inside a longer identifier is not a
    /// reference; treating it as one would invent an edge and, with enough of
    /// them, a cycle.
    #[test]
    fn a_name_inside_a_longer_identifier_is_not_a_reference() {
        for definition in [
            "SELECT * FROM dbo.active_customer_archive",
            "SELECT * FROM dbo.old_active_customer",
            "SELECT @active_customer",
        ] {
            assert!(
                !references(definition, &n("dbo.active_customer")),
                "{definition}"
            );
        }
        assert!(!references("SELECT 1", &n("dbo.active_customer")));
    }

    fn modules(specs: &[(&str, &str)]) -> BTreeMap<ObjectName, Module> {
        specs
            .iter()
            .map(|(name, def)| (n(name), view(def)))
            .collect()
    }

    /// A view over a view has to be created second, or the CREATE fails.
    #[test]
    fn a_referenced_module_is_created_first() {
        let m = modules(&[
            ("dbo.top", "SELECT * FROM dbo.middle"),
            ("dbo.middle", "SELECT * FROM dbo.base"),
            ("dbo.base", "SELECT * FROM dbo.customer"),
        ]);
        assert_eq!(
            creation_order(&m, &ModuleDeps::default()),
            vec![n("dbo.base"), n("dbo.middle"), n("dbo.top")]
        );
    }

    /// The escape hatch has to work where the scan sees nothing — a view
    /// reached only through a synonym, say.
    #[test]
    fn an_explicit_dependency_orders_what_the_scan_cannot_see() {
        let m = modules(&[("dbo.a", "SELECT 1"), ("dbo.b", "SELECT 2")]);
        let mut deps = ModuleDeps::default();
        deps.insert(n("dbo.a"), BTreeSet::from([n("dbo.b")]));
        assert_eq!(creation_order(&m, &deps), vec![n("dbo.b"), n("dbo.a")]);
    }

    /// Unrelated modules must come out in one fixed order, or two runs over the
    /// same declarations would produce plans that diff against each other.
    #[test]
    fn independent_modules_come_out_in_a_stable_order() {
        let m = modules(&[
            ("dbo.z", "SELECT 1"),
            ("dbo.a", "SELECT 2"),
            ("dbo.m", "SELECT 3"),
        ]);
        let first = creation_order(&m, &ModuleDeps::default());
        assert_eq!(first, vec![n("dbo.a"), n("dbo.m"), n("dbo.z")]);
        for _ in 0..5 {
            assert_eq!(creation_order(&m, &ModuleDeps::default()), first);
        }
    }

    /// A cycle cannot be created in any order. It must not hang or drop a
    /// module either: everything is emitted, and the engine gives the verdict.
    #[test]
    fn a_cycle_still_yields_every_module_once() {
        let m = modules(&[
            ("dbo.a", "SELECT * FROM dbo.b"),
            ("dbo.b", "SELECT * FROM dbo.a"),
        ]);
        let order = creation_order(&m, &ModuleDeps::default());
        assert_eq!(order.len(), 2);
        assert!(order.contains(&n("dbo.a")) && order.contains(&n("dbo.b")));
    }

    #[test]
    fn a_module_named_after_a_table_is_reported() {
        let mut schema = Schema::default();
        schema.tables.insert(n("dbo.customer"), Table::default());
        schema.modules.insert(n("dbo.customer"), view("SELECT 1"));
        let problems = check_names(&schema);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("one namespace"), "{problems:?}");
    }

    #[test]
    fn a_trigger_must_name_a_declared_table() {
        let mut schema = Schema::default();
        let mut trigger = module(ModuleKind::Trigger, "AFTER INSERT AS SELECT 1");
        schema.modules.insert(n("dbo.trg"), trigger.clone());
        assert!(check_names(&schema)[0].contains("does not say which table"));

        trigger.on = Some(n("dbo.absent"));
        schema.modules.insert(n("dbo.trg"), trigger.clone());
        assert!(check_names(&schema)[0].contains("not declared here"));

        schema.tables.insert(n("dbo.absent"), Table::default());
        assert!(check_names(&schema).is_empty());
    }

    /// SQL Server allows `INSTEAD OF` triggers on views, and `pull` rebuilds
    /// the `on:` from whatever it finds — so a target that is a declared view
    /// has to load, or such a database could never be round-tripped.
    #[test]
    fn a_trigger_may_be_attached_to_a_declared_view() {
        let mut schema = Schema::default();
        schema.modules.insert(n("dbo.v"), view("SELECT 1 AS one"));
        let mut trigger = module(ModuleKind::Trigger, "INSTEAD OF INSERT AS SELECT 1");
        trigger.on = Some(n("dbo.v"));
        schema.modules.insert(n("dbo.trg"), trigger);
        assert!(
            check_names(&schema).is_empty(),
            "{:?}",
            check_names(&schema)
        );

        // And the view has to exist before the trigger can be attached to it.
        // The definition never names it — the emitter writes it into the `ON`
        // clause — so only `on:` can supply that edge.
        let order = creation_order(&schema.modules, &ModuleDeps::default());
        assert_eq!(order, vec![n("dbo.v"), n("dbo.trg")]);
    }

    /// A target that is neither a declared table nor a declared view is still
    /// refused: the trigger would be created on an object pbps does not manage.
    #[test]
    fn a_trigger_on_a_procedure_is_still_refused() {
        let mut schema = Schema::default();
        schema
            .modules
            .insert(n("dbo.p"), module(ModuleKind::Procedure, "AS SELECT 1"));
        let mut trigger = module(ModuleKind::Trigger, "AFTER INSERT AS SELECT 1");
        trigger.on = Some(n("dbo.p"));
        schema.modules.insert(n("dbo.trg"), trigger);
        assert!(
            check_names(&schema)[0].contains("not declared here as a table or a view"),
            "{:?}",
            check_names(&schema)
        );
    }

    /// A name inside a comment or a string is not a reference. Inventing the
    /// edge is worse than missing one: it can close a cycle, and a cycle falls
    /// back to name order — while `depends_on:` can only *add* edges, so the
    /// user has no way to take the invented one back.
    #[test]
    fn a_name_in_a_comment_or_a_literal_is_not_a_dependency() {
        let target: ObjectName = "dbo.active_customer".parse().unwrap();
        for definition in [
            "SELECT 1 -- superseded by dbo.active_customer",
            "/* see dbo.active_customer */ SELECT 1",
            "SELECT 'dbo.active_customer' AS note",
        ] {
            assert!(
                !references(definition, &target),
                "{definition} must not count as a reference"
            );
        }
        // A quoted identifier still does: it is a name, not text.
        assert!(references("SELECT * FROM [dbo].[active_customer]", &target));
        assert!(references(
            "SELECT * FROM \"dbo\".\"active_customer\"",
            &target
        ));
    }

    /// An ordering edge that silently does not exist is the failure this check
    /// is for: the declaration looks right and the apply emits in the wrong
    /// order much later.
    #[test]
    fn a_depends_on_target_that_is_not_declared_is_refused() {
        let mut schema = Schema::default();
        schema.modules.insert(n("dbo.a"), view("SELECT 1"));
        schema.modules.insert(n("dbo.b"), view("SELECT 2"));

        let mut deps = ModuleDeps::default();
        deps.insert(n("dbo.b"), [n("dbo.a")].into_iter().collect());
        assert!(check_dependencies(&schema, &deps).is_empty());

        deps.insert(n("dbo.b"), [n("dbo.typo")].into_iter().collect());
        assert!(
            check_dependencies(&schema, &deps)[0].contains("not a declared module"),
            "{:?}",
            check_dependencies(&schema, &deps)
        );

        deps.insert(n("dbo.b"), [n("dbo.b")].into_iter().collect());
        assert!(check_dependencies(&schema, &deps)[0].contains("lists itself"));
    }

    /// `on:` on a view would read as if it did something; it does not, and a
    /// declaration that quietly means nothing is the failure mode this tool is
    /// built to avoid.
    #[test]
    fn only_a_trigger_may_name_a_table() {
        let mut schema = Schema::default();
        let mut v = view("SELECT 1");
        v.on = Some(n("dbo.customer"));
        schema.modules.insert(n("dbo.v"), v);
        assert!(check_names(&schema)[0].contains("only a trigger"));
    }

    #[test]
    fn an_empty_definition_is_reported() {
        let mut schema = Schema::default();
        schema.modules.insert(n("dbo.v"), view("   \n"));
        assert!(check_names(&schema)[0].contains("empty definition"));
    }
}
