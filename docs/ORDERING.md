# How a plan's order is decided

An audit of every pair of `Change` kinds that can meet in one plan, by how the
order between them is decided (#1351). It exists because a fixed position per
kind of change cannot express a direction that depends on content. Found one
review round at a time, those combinations do not run out: #1285 took about
twenty rounds. The set of change kinds is finite, so the question can be
answered once for all of them.

Read this before adding a `Change` variant, an ordering rule or engine.
A new variant needs a row in [the table below](#what-each-change-requires);
`every_change_kind_has_a_row_in_the_ordering_audit` refuses one that has none.

## Planning ownership

Successful differ, expansion and settlement results are immutable
`order::PlanningPlan` values. Order keeps the payload, original logical sources
and the latest stage's proof together; callers supply facts, typed rewrites or
constraints. `propose` starts fresh non-executable logical input, and cannot
recertify an external payload copy for emission. Raw settlement stays inside
order. Structural transformations invalidate the previous positional proof;
each existing settlement records its own edges, without a constraint union.
`OrderedPlan` has a private constructor and no public downgrade or generic
empty-schedule finalizer (DEC-1959.5).

Scratch settles an engine-owned, SQL-free blueprint before lowering.
Function/phase compatibility and
generated constraints retain their separate settlements. The real engine's
opaque associated input prevents a caller-defined engine from manufacturing
its scratch result. LoweredPlan keeps immutable operation groups, source IDs
and statements and is branded by the emitter, so a mock emitter cannot supply
a real engine's executable collection. Planning probes return metadata only.
The completed shared declaration/catalog route lives in `order::deployment`.
A successful differ result retains its preceding sort/rename provenance;
raw proposals remain non-executable even after all shared stages. Existing
connected continuations take only immutable typed results and order applies
their proposals. The resolver's complete route and final ordinal remap retain
its original expansion IDs and exact proof. No stage unions its constraints.

New artifacts require `order::seal(OrderedPlan, metadata)`; loaded wire DTOs
require checked `order::admit` and retain their reviewed order as-is.
`ArtifactView` gates writing and executable checksums. The DTO exposes only
an unchecked `wire_fingerprint` for admission and attempt identity. SQL bytes,
saved fields, checksum bytes, version and refusal priority are unchanged.
Admission owns apply's version/checksum/origin/dialect and payload checks;
request-mode, resume and risk gates follow it. Lowering reads the input's
bound mode, and sealing rejects metadata with a different mode. Explain
reviews wire data and obtains only count metadata, so previews remain
explainable without becoming executable.
CLI lowering retains concrete PostgreSQL/SQL Server variants and immutable
operation groups through scripts, bootstrap, dev rehearsal, transactional,
staged and resume execution. No public raw emit/render entry, mutable plan or
statement-vector conversion grants execution authority. Compiler-negative
doctests cover these boundaries and run explicitly in CI. The release
verifier remains off (DEC-1959.5).

The canonical deployment route also consumes RLS policy-dependent facts before
certification. The final policy/flag setup and catalog-owner release settlement
remain after the existing rewrite schedule and before creation-risk refresh.
`order::row_security` selects the same opening-UID releases and retains the
existing combined effect, generated, slot and module-catalog graph only for
those releases. PostgreSQL reads the facts; the CLI cannot write back a raw
permutation or mint authority by running this intermediate stage alone.

## The mechanism

The planner compiles logical changes into an executable, engine-branded plan.
The stages below describe the ownership boundaries, not permission to combine
the existing settlements or change their order (DEC-1959.5, DEC-1959.6).

1. **Operations.** A logical change has a stable source identity. Its operations
   carry their own payload, `ObjectKey`, `Base | Final` incarnation and origin:
   whole, split part, teardown or restoration. Facts from the baseline and
   desired schema resolve ownership and the other end of a change; they do not
   replace the step's payload. A raw proposal is non-executable.
2. **Expansion.** A named obligation may require additional operations. Typed
   rewrites add only parts of the existing logical change or its named
   obligations; they cannot replace a rename with a drop and create. The driver
   checks intent, scope, execution mode, each part's risk and targeted progress,
   with a plan-size budget as a backstop. New-table parts and partition-default
   restorations use this same mechanism. Added origins remain explainable and
   are not added to the saved-plan wire format (DEC-1958.2, DEC-1959.2,
   DEC-1959.4).
3. **Effects and constraints.** Portable effects describe resources created,
   removed, read, written and rewritten, and requirements on them. The dialect
   adds engine effects. A resource index builds candidate edges; there is no
   unconditional all-operation-pairs scan. Name claims/releases compete in
   engine-defined slots, using observed holders and name equality. Declared,
   lexical, catalog, binding and bounded-search evidence supplies the other
   constraints. Two writes to a resource retain their base relative order.
4. **Settlement.** `Schedule::settle` orders operations by their constraints
   and names cycle or unmet-obligation refusals. The expansion driver also
   checks its own obligations. The existing `Sink` ready-set choice uses the
   base position; a held operation sinks
   after what it waits for.
   This is not a new rank or preference rule. Expansions and connected
   continuations retain their existing individual settlements and refusal
   points, without a constraint union. Only order applies their proposals.
   Compatibility chains preserve old peer order solely for byte-identical
   output (DEC-1959.1, DEC-1959.3, DEC-1959.4).
5. **Lowering.** The completed canonical route produces `OrderedPlan<D>` for
   its original engine. Lowering consumes certified input and returns an
   immutable `LoweredPlan` branded by the concrete emitter. Each operation's
   statements form an indivisible group in emitter order, with source IDs and
   execution/name metadata. Scratch first settles its opaque SQL-free
   blueprint. Neither rendering nor apply can accept a reordered raw
   `ChangeSet`, and nothing reorders statements after lowering.
6. **Verification and sealing.** Operation replay checks requirements against
   the baseline after settlement; statement-level name checks use the emitter's
   intermediate-name metadata. These independent checks run in tests/debug
   builds; the release verifier remains off. They cannot discover an effect
   nobody declared: live commutation tests and mutation audits measure that
   gap. Sealing accepts the certified result; admission is the separate checked
   route for a reviewed saved artifact and preserves its order. Neither adds
   proof/origins to checksum bytes (DEC-1959.5).

The canonical declaration/catalog entry point is
`order::deployment::schedule::schedule`.
CLI adapters supply observed facts and preserve re-diff/probe timing. A fact
not read is distinct from a fact read and empty: unavailable stages report
their skip. Resolver, connected catalog continuations and the shared route
finish inside order. Some historical helper names and scoped compatibility
predicates remain; a name is not a separate list-reordering authority.
Bounded namespace search still proposes `Search` edges, and the PostgreSQL
engine-generated-name trial remains scratch reconstruction rather than a
generic slot allocator. These are retained limits, not extra scheduling passes.

Explicit intermediate input reads carry `Holds` requirements from the shared
slot description (DEC-2298.9). Each read follows the claim giving its UID that
address and precedes the release taking it away. Slot replay checks its holder
before the operation; ordinary inputs retain their existing origin rule.
An incarnation's removal ends its tree and column values: Base or bound-current
readers precede that removal, while a later incarnation's reader waits for it.
Independent value replay checks the same lifetime at every read.

### Membership-scoped effect contract

`operation::symbolic` declares recursive property writes, membership
incarnations, transfers and explicit own-value restorations (DEC-2296.1).
The parent UID and property identify a version chain; operation identities,
not matching definition text or the current list positions, identify its
transitions. Binding incarnations, own/inherited/local ownership and physical
birth are distinct facts. Engines supply normalized definitions and the
observed overwrite/preserve/adopt policy; the model parses no SQL.

`order::membership` indexes the declarations and compiles each move's allowed
cuts in those chains. An interval requires its lower predecessor before the
move and its upper successor after it. A move compatible with both sides
gets no preference edge. A fork, missing fact, non-convex interval or
unrepresented disjunction is a named obligation with the separate-plans
remedy. Other property/dependency constraints may form an ordinary cycle;
cycles containing these boundary constraints retain that remedy too; an
unrelated cycle does not. Clause order inside an indivisible operation stays
fixed, so an unsatisfied intra-operation boundary is an obligation.
A restoration must qualify its changed ownership through the matching
parent/child/property transfer; unrelated moves cannot supply that fact.
Qualification checks the combined declared membership links for new nesting
or cycles and checks transfer targets after removing the complete source set;
source swaps are atomic, while occupied untouched targets are obligations.
Recursive targets need known aliases, values and physical lifetimes, including
members with no move. Each changing membership must declare coverage of the
recursive properties it carries. Overlapping recursive chains, transfers or
restorations that overwrite a versioned input without a shared version chain
remain named obligations; matching endpoint text cannot supply that chain.
Test-only generated contracts enumerate every permitted topological order
against independent replay, including supported and unqualified declarations.
Restorations are grouped by physical `(child, property)` cell, including
clauses with different parent keys. The existing declared write/write order
(operation identity, then clause) chains that cell's restorations; matching
keys never imply that distinct cells share a dependency. The last restoration
names its final value and event version. A reaching recursive suffix must
precede that terminal event, even when it preserves the value. Parent-only
writes and writes after detach cannot reach the cell and impose no such edge.
Intermediate restorations may precede later value writes, while retaining
the existing physical-incarnation boundary across creation/retirement.
Non-convex restoration cuts remain named obligations. Move cuts must agree on
the complete physical endpoint, including event version and origin; several
value-compatible endpoints do not authorize choosing a preferred event and
remain a named obligation with the separate-plans remedy.
Test-only determinism checks compare complete final states for every permitted
order and independently check recursive, transfer and terminal restoration
goals. They retain event versions, origins, bindings, ownership and membership.
The corpus varies logical keys independently of physical aliases for recursive
writes, transfers and restorations. Necessity checks require replay failure
witnesses or a checked named certificate: another dependency path, same-cell
write/write order, an intermediate restoration's physical incarnation, or the
terminal restoration's value/event goal. The last certificate independently
checks that reversing the edge loses that declared goal; concrete projection
alone does not check a stationary restoration's terminal event.
`Schedule::of_graph` imports only the symbolic contract's explicit unmet
obligations. Ordinary `Graph::unmet` requirements remain observational
`unprovided` facts under DEC-1954.6 and DEC-1956.3, even in a mixed graph.
A symbolic contract obligation cannot be discharged by merely observing it.

A concrete projection replays membership at each position and expands a
recursive write only over the members then present. Transfers retain
physical origin while changing managed scope; a drop retires the child.
Independent Final replay checks the actual writes, transfers and goals,
without importing a concrete child union from graph compilation. The
non-executable `Pending` lookup distinguishes pending facts from missing
sources; it grants no executable authority, as compiler-negative tests pin.
Its borrowed operations/effects cannot be mutated underneath a live view.

A transfer can carry an engine-supplied `Merge` table over complete parent
and candidate values (DEC-2296.2). It reads the parent property at its boundary;
the cut compiler compares each result with the declared child goal. A unique
side imposes its edge, both compatible sides impose no preference, and neither
is a named obligation. Concrete replay uses the same merge at the actual
position. Unknown or overlapping rows never mean preserve-the-candidate.
PG mappings live in engine effects; the model contains no engine-specific
merge or catalog query. The soundness, necessity and complete-state determinism
corpus also enumerates merges and both parent-version cuts.

The contract is not activated for guarded plans yet. `attached()` and
`detached_names()`, early weave/source projections, shipping engine clauses
and boundary probe grounding remain the column/part PRs' integration work.
Those PRs carry typed obligations to the existing effect settlement; they
must not choose a preliminary order to construct their effects. Existing
stages, Sink, brands, wire fields and the release-verifier policy stay intact.
Test-only small-order enumeration checks soundness and admitted-interval
completeness; it is not a shipping search/ordering stage.

Every measured silent-difference case in an implementation PR's scope needs
live readback on PG16/18, not a sample. In a combined transactional plan the
first parent ALTER holds the tree's ACCESS EXCLUSIVE lock until commit;
attach validation runs within that longer lock window. The eventual plan
text must say so, and the operator can still choose separate plans.

### Base ordering policy

The differ sorts every planned change by, in order:

1. **Class**, `order_key` in `crates/pbps-diff/src/schema_diff.rs`:

   | Class | Changes |
   |---|---|
   | 0 | `DropPolicy` before every module-drop rank; `DropModule`, `DropRole`; P: `SetReplicaIdentity` to a target that stands, at (0, 1) |
   | 1 | `RenameTable`, `RenameRole` |
   | 2 | `DropIndex`, `DropUnique`, `DropForeignKey`, `DropCheck`, `SetPrimaryKey { to: None }`; `DropComputedColumn` at the class's end (2, 4) |
   | 3 | `RenameColumn` |
   | 4 | `Revoke` |
   | 5 | `DropColumn` |
   | 6 | `DropTable`; an `AlterColumnNumbering` giving up, renaming, or (on a column neither tightened nor retyped) turning into an identity an owned sequence, at (6, 1 + its depth in the chain of names they free) |
   | 7 | `CreateTable` |
   | 8 | `AddColumn` |
   | 9 (`COLUMN_ALTERATIONS`) | `AlterColumnType`, `AlterColumnNullability`, `AlterColumnDefault`, `AlterColumnExpression`, `AlterColumnNumbering`, `SetPartitionDefault`, `SetPartitionNotNull`; `AddComputedColumn` at the class's end (9, 3); P: `SetTablePersistence` after it, by the foreign-key graph |
   | 10 | `SetColumnDeprecated`; P: `SetStorageParameters`, `SetIndexStorageParameters` |
   | 11 | `InsertRow`, `UpdateRow` |
   | 12 | `DeleteRow` |
   | 13 | A partitioned parent's `AddIndex` first (13, 0); `SetPrimaryKey { to: Some }`, `AddUnique`, `AddForeignKey`, `AddCheck`, `AddIndex`; P: the other `SetReplicaIdentity`, last |
   | 14 | `CreateModule`, `AlterModule`; `PublicExecution` immediately after its routine's create (#687); P: `CreatePolicy` at (14, 2), then `SetRowSecurity` at (14, 3) |
   | 15 | `CreateRole` |
   | 16 | `Grant` |
   | 17 | `SetDataMode`, `SetEnsureKeys` |

2. **A rank inside the class**, `sort_class` and `dependency_rank` in the same
   file:
   - a drop that frees a name a rename claims goes before the rename (DECISIONS
     474), and a generated column's drop goes first among them (DEC-1168.1);
   - a generated column is dropped before the ordinary columns of class 5, and
     added after every in-place alteration of class 9 (DEC-1168.1);
   - inside class 9, an old default is dropped before a retype, and a new one
     set after it. Nullability is relaxed before an expression change and
     tightened after it (DEC-1168.1). A numbering source given up, or an
     identity turned into a sequence, goes first, at −3, and any other
     numbering change last, at +2 (DEC-1889.1);
   - modules are ranked by `creation_order_with` (DECISIONS 311, 315);
     `DropPolicy` has rank `-drop_rank.len() - 1`, before every negative
     module-drop rank (DEC-1173.2);
   - reference rows by the foreign keys between their tables;
   - role drops by membership, holder before member (`member_depth`,
     DECISIONS 127);
   - a foreign key comes after the key it references;
   - a partition is created after every other creation of class 7, its
     parent's included, at (7, 2) (DEC-1170.1);
   - a DEFAULT partition's own NOT NULL, tightened in a plan that moves rows
     out of it, comes at (9, 6), after every create that moves them, (9, 5)
     included (DEC-1547.1);
   - a clustered index comes before the other indexes of its table
     (DEC-1178.1);
   - a replica identity whose target the plan adds, or whose columns become
     NOT NULL in it, comes after every addition of class 13; any other comes
     first, at (0, 1) (DEC-1444.1).
3. **`subject()`, then the change's rendering**, which only breaks ties.

## Adding an engine

A third engine supplies the following ordering inputs; it does not write a
reorder pass or manufacture an executable plan (DEC-1959.6).

| Input | Engine responsibility | Shared consumer |
| --- | --- | --- |
| `Dialect::engine_effects` | Pure effects specific to the engine, reading each operation's own payload and supplied snapshots; include lexer/binding semantics that portable effects cannot state | Resource-indexed edges and operation replay |
| Shared [`pbps_dialect::slots::slots`](../crates/pbps-dialect/src/slots.rs#L366) mapper | Supply namespace sharing and overload policy through the `Dialect::shares_namespace_with_tables`, `Dialect::indexes_share_namespace_with_tables`, `Dialect::constraints_share_namespace_with_tables` and `Dialect::overloads` hooks; the mapper uses an address's supplied signature for overloaded routines | Slot claims, releases and intermediate-name checks |
| Slot equality | A catalog fact under the target's comparison/collation rules; exact comparison only where the engine contract establishes it | `CatalogFacts` / `Same`; unknown equality is skipped and reported, never assumed unequal |
| Catalog facts | Actual occupants, releases, module/column/role bindings and dependencies, plus capabilities needed by an expansion; distinguish unread from read-empty | Pure order proposals and connected canonical continuations; I/O stays in the engine/CLI seam |
| Statement lowering metadata | Transaction and batch requirements, intermediate `renames`/`role_renames`, `creates` and `releases`, plus execution guards such as row writes or index builds | Immutable lowering groups, rendering, checkpointing and statement-level verification |
| Engine-specific expansions | Named obligations and typed teardown/restoration/split parts, with per-part risk, equivalence, progress and budget checks | The existing expansion driver and `settle` |

An operation's group retains its source ID and origin even when it emits no
statements. Admitted artifacts use their wire step IDs; skipped unsealed
logical provenance is not reconstructed. Batch separation (`own_batch`) does
not imply a commit; `transactional` is a separate requirement. Intermediate names are supplied by
the emitter, never reconstructed by parsing SQL. Engines preserve their
concrete type through certified lowering, rendering, sealing and apply.

An ordinary plan remains one transaction and refuses non-transactional
statements ([`reject_non_transactional`](../crates/pbps-cli/src/deploy.rs#L9907)).
A staged plan contains exactly one logical change: planning refuses both
multiple logical changes and an empty plan, and skips that transactional
statement check ([planning limit](../crates/pbps-cli/src/deploy.rs#L7600)).
Its lowered statements run in order, each committed, with the existing
checkpoints and resume checks
([staged executor](../crates/pbps-cli/src/deploy.rs#L8908), ADR-0003).
A new engine whose single change lowers to several non-transactional
statements must use this staged route and make each statement resumable or
idempotent under the existing protocol. Several logical changes needing
non-transactional execution remain refused until a multi-transaction design
exists. Statement metadata and batch boundaries do not authorize additional
commits in an ordinary plan.

## Method

Two changes can only constrain each other's order through what one of them
requires and the other provides or takes away. So the audit is built in two
steps:

1. List, for each change kind, what has to hold before it runs, and what it
   creates, removes or rewrites ([the table below](#what-each-change-requires)).
2. A pair of kinds interacts when one side's effect meets the other side's
   requirement. Every other pair is **independent**. Each entry of the
   *Requires before it* column is matched against the *Creates, removes or
   rewrites* column of every kind, so each requirement has at least one row
   below, or none of the 33 kinds can affect it. Every arm of
   `dependency_rank` and every exception in `sort_class` that is not zero has
   a row too, since each one is a pair whose order its class alone does not
   decide. Each interacting pair falls
   into one of four classes:
   - **Fixed direction.** Whenever they interact, one must precede the other,
     whatever the content. Position can express it, and the audit checks that
     the current order does.
   - **Content-dependent direction.** The direction depends on what an
     expression or a body reads or calls. Position cannot express it. It
     belongs to dependency-graph ordering (#615), and its corners are recorded
     on #1350.
   - **Value flow.** A pre-flight probe of the later change reads values the
     earlier change rewrites. The general rule: project the change where the
     probe can (a retype through a cast); otherwise report the probe unchecked,
     never judged on the old values.
   - **Engine-sourced.** The edge comes from the catalog (`pg_depend`,
     `sys.sql_expression_dependencies`) at plan time, so it is exact, not
     positional.

## How the code states what a change requires

The table below is executable. `pbps_model::operation::effects` reports, for
each operation, what it creates, removes and rewrites (objects, by key and
instance, and the values under a key) and what it requires: that an object
exists, that one instance of it stands, that it is gone, or, for a check or a
key, that the values under it are final. `Dialect::engine_effects` adds what
only the engine's lexer or catalog knows (DEC-1954.3). From those,
`pbps_diff::order` draws the edges (a creation before what requires it, a
requirement before the removal, a removal before what needs it gone, a
rewrite before its readers, and two writes of one resource in their base
order), replays the operations from the baseline and refuses an order they
contradict (DEC-1954.6). It runs beside ordering in tests and debug
builds. `order::schedule::Schedule::settle` applies the constraints supplied
by the existing canonical stages. It uses Kahn's algorithm, the lowest
position first among the changes that are
ready, so a change an edge holds back sinks to right after what it waits for
and nothing else moves (DEC-1956.2). Each edge carries one `OrderReason`
(DEC-1956.1). The verifier settles the effect graph over every order it
observes and requires the order back, so an edge the order reverses is
named there (DEC-1956.3). The resolver's whole plan is settled over the
effects, the name slots' handovers, the annotations and what only its
evidence states: a surface's bindings and the grants its binding steps need
(DEC-1968.1, DEC-1968.2). The last column of the table
is what the code reports for the kind, and `ordering_audit` refuses a
difference, so a new kind or a new effect updates both (DEC-1954.1).

Names are stated the same way, apart from the effects, because a name is an
address and not an identity. `pbps_model::operation::names` reports, for each
operation, the names it claims and releases. `pbps_dialect::slots::names`
adds a constraint trigger's companion names and its clones' names from the
dialect's existing answers (DEC-2100.1). The shared
`pbps_dialect::slots::slots` mapper uses the dialect's namespace hooks to
identify every slot each name holds. Whether two spellings are one name is the
engine's rule, or the catalog's answer under a collation (DEC-1964.1).
`pbps_diff::order::slots` then does three things beside every order
(DEC-1964.2):
- orders a name's first holder before every other claim, and every other
  release before its last holder;
- replays the order and refuses a claim on a name still held;
- refuses a move whose intermediate name is held at that statement.

PostgreSQL also has `Types(schema)`, separate from `Relations(schema)`.
Declared enums/domains hold only Types; tables and views hold both; indexes
and sequences hold only Relations. Catalog composites hold Types. Cross-schema
table moves claim both intermediate names; type moves claim intermediate Types
names, observed from the emitter. Generated array names retain #1503's rule.

## What each change requires

`P` is PostgreSQL and `S` is SQL Server; no marker means both. Every change
also requires the names it uses to be the ones in force at its position. That
requirement is common to all of them, so it is listed once,
[below](#names-in-force), rather than in every row.

| Change | Class | Requires before it | Creates, removes or rewrites | Effects (checked) |
|---|---|---|---|---|
| `DropModule` | 0 | Its dependents gone. Planned policy drops first. P: views, routines, triggers, defaults, checks, indexes and generated columns, found in the catalog. S: schema-bound views | Removes the module, frees its name | removes |
| `CreateType` | 7, before tables; domain-base depth first | P: its declared base's final incarnation; its Types name free | Creates the type | Is, creates |
| `DropType` | 18 | P: its columns/tables and dependent domains gone or retyped; its modules gone | Removes the type, releases its Types name | Exists, Absent, removes |
| `RenameType` | 1 | P: the target and intermediate Types names free | Rewrites the same uid at its final name | Exists, rewrites |
| `DropRole` | 0 | P: performed by hand (ADR-0010 §3). S: no members, no owned objects | Removes the role | removes |
| `RenameTable` | 1 | The target name free. P: no retained managed policy whose deparsed predicate changes | The table's new name | Exists, rewrites |
| `RenameRole` | 1 | P: performed by hand. S: the target name free | The role's new name | Exists, rewrites |
| `DropIndex`, `DropUnique`, `DropForeignKey`, `DropCheck`, `SetPrimaryKey { to: None }` | 2 | A foreign key on a key is dropped before the key | Frees names; releases columns for a rename, drop or retype; is gone before a row of its table is written (DEC-1968.2) | Exists, Is, creates, removes |
| `DropComputedColumn` | 2 (2, 4) | S: the indexes, uniques and checks over it gone (4922); a check by its text, lexed | Removes a computed column, frees its name and releases the columns and functions it reads (DEC-1174.1) | Exists, removes |
| `RenameColumn` | 3 | The target name free. P: no retained managed policy bound to the column. S: no check and no filtered index naming the column (DECISIONS 474) | The column's new name. P: other dependents' text follows the rename | Exists, rewrites |
| `Revoke` | 4 | The target exists (a revoke on an object the plan drops is not emitted) | Removes a permission | Exists, rewrites |
| `DropColumn` | 5 | Its indexes, constraints, inbound foreign keys and modules gone; planned policy drops first. P: generated columns reading it gone | Removes the column, frees its name | Absent, Exists, removes |
| `DropTable` | 6 | No inbound foreign key bound to this UID; dependent modules gone | Removes the table, frees its name | NotBoundTo, Exists, removes |
| `DetachPartition` | 6 | P: no row referencing the partition through a foreign key to its parent (pre-flight) | The partition leaves its parent with its rows, as an ordinary table under the declared names; frees its range (DEC-1544.1) | Exists, removes |
| `RebuildPartitionedTable` | order expansion only | Recorded source and reserved replacement lineage; no executable root until the complete route ships (DEC-2298.4) | Named unexpanded-root refusal, no fictitious schema resource | refuses |
| `CopyTableRows` | order effects only | Base source tree/columns and Final replacement tree/columns; reads source values, writes destination values | No name claims; copy completion is an expansion obligation | Holds, Is, Reads, rewrites |
| `BuildPartitionedIndex` | order effects only | Final parent/leaves and copied rows; explicit parent-owned clone/index/constraint recipe (DEC-2298.8) | Claims every explicit name; parent-owned release shared by slot edges/replay; private transactional lowering only | Exists, Holds, Is, Reads, creates |
| `AnalyzeTableTree` | order effects only | Final replacement parent and leaves, after their row writes | No schema identity or name transition; swap follows by expansion obligation | Holds, Is, Reads |
| `CreateTable` | 7; a partition (7, 2), or (9, 5) under a parent whose columns the plan changes | The name free, its types. Functions its defaults, checks and generated columns call: *content*. Beside a standing DEFAULT partition, P: no row referencing a row it moves, and no delete trigger or rule on the DEFAULT that the move fires (pre-flight) | The table, columns, key, uniques, checks, indexes; beside a standing DEFAULT partition, the DEFAULT's rows of its range move into it (DEC-1547.1) | Exists, creates, rewrites |
| `AttachPartition` | 7; (11, 1) when its parent's foreign key references a table the plan writes rows into or attaches a table to | The parent standing; the table already its parent's columns, order and parent's checks (refused otherwise). P: no row of the table outside its range, no trigger on it named as a parent's row trigger and not dropped first (DEC-1632.3), no column grant on it, none in the parent's DEFAULT partition inside it; as the DEFAULT, none of its rows another partition takes and no other DEFAULT kept (pre-flight, DEC-1639.1) | The table becomes a partition with its rows; its columns become its parent's, its matching keys and indexes clones, and the rest its own; takes its range (DEC-1545.1). In a staged plan a range or list attach's four statements carry their own temporary CHECK, claimed and released inside the one operation (DEC-1638.1); a HASH or DEFAULT attach stays one statement (DEC-1639.1) | Exists, creates, removes, rewrites |
| `AddColumn` | 8 | The table, the name free. A generated column's inputs; functions its default or expression calls: *content* | The column, backfilled; appends to the table's column order (DEC-2080.1) | Exists, creates, rewrites |
| `AlterColumnType` | 9 | What blocks a retype gone; planned policy drops first. A foreign key the plan drops over the column or into it, first. S: keys, indexes, checks, foreign keys, as the dialect's retype policy names them (DEC-1998.1). P: views and rules (`weave`), generated readers (refused). An old default dropped first | Converted values | Absent, Exists, creates, rewrites |
| `AlterColumnNullability` | 9 | Tightening: the values non-null. S: tightening, the indexes and uniques over it and filtered indexes naming it gone; relaxing, filtered indexes naming it gone (#1363, DEC-1998.1) | Accepts or refuses NULL | Exists, creates, removes |
| `AlterColumnDefault` | 9 | Functions the default calls: *content* | The default | Exists, Is, creates, removes |
| `SetPartitionDefault` | 9; (9, 4) after its parent's default or NOT NULL change on the column | Functions the default calls: *content*. Its parent's column, by the name the step spells: a release before the parent's rename of it, anything else after it (DEC-1968.2). P: after its parent's default set by the plan (`after_their_parents_defaults`); one split from a create beside a DEFAULT, right after the last function its text may call (`split_moving_defaults`, DEC-1547.1) | One partition's default: its own, or its parent's again (DEC-1581.1) | Exists, Is, creates |
| `SetPartitionNotNull` | 9; (9, 4) after its parent's default or NOT NULL change on the column; a DEFAULT's tightening (9, 6) in a plan that moves rows out of it | Tightening: the partition's values non-null | One partition's column accepts or refuses NULL (DEC-1581.1) | Exists, Reads |
| `AlterColumnExpression` | 9 | P: its inputs, a relaxation of its own column. Functions it calls: *content* | Recomputed stored values | Absent, Exists, creates, removes, rewrites |
| `AlterColumnNumbering` | 9; −3 giving up a source or an identity, +2 otherwise; (9, 0) retuning a kept identity on a column this plan narrows; (6, 1 + chain depth) giving up, renaming, or turning into an identity an owned sequence on a column it neither tightens nor retypes; (12, 3) putting a source on a table whose rows it updates or deletes and does not only insert | Putting an identity on: the column `NOT NULL`. Putting a source on: the column in its final type, and a plain default the baseline gives it removed first, by the change the differ splits out for it. Giving up an identity: the column still `NOT NULL`. P: no value too near the bound it counts toward to continue past; a kept source's bounds moved: its counter inside them (pre-flight) | The column's identity or owned sequence put on, taken off, retuned or converted; one put on continues past the column's values (DEC-1889.1); its bounds, cycle and cache set (DEC-1921.1) | Exists, Absent |
| `AddComputedColumn` | 9 (9, 3) | S: the columns it reads, in their final type. Functions it calls exist (one this plan creates is refused by name) | A computed column at the end of its table (DEC-1174.1); appends to the table's column order (DEC-2080.1) | Exists, creates, rewrites |
| `SetColumnDeprecated` | 10 | Nothing | Metadata only | Exists |
| `SetTablePersistence` | 9 (9, 10 + depth) | P: the foreign keys that stand: to logged, the tables it references logged first; to unlogged, the tables referencing it unlogged first (refused the other way round, measured) | Rewrites the table and its indexes under `AccessExclusiveLock` (DEC-1443.1) | Exists |
| `SetStorageParameters` | 10 | P: the table, under its final name. Nothing reads a storage parameter | The table's heap storage parameters; no rewrite (DEC-1441.1) | Exists |
| `SetIndexStorageParameters` | 10 | P: the index, which stands: one the plan adds or rebuilds takes its parameters in its `CREATE` instead | An index's storage parameters, in place; no rebuild (DEC-1442.1) | Exists |
| `InsertRow`, `UpdateRow` | 11 | The columns written, defaults the row takes (an update's `SET c = DEFAULT` too), the rows of the tables it references, table against table, never by value; an update's also leave before the rows they release (DEC-1968.2); an update's guards read the columns it leaves alone and its key, and an insert's postcondition every column in its `types`, those it leaves at NULL among them (#2079); an insert's also the NOT NULL gone of each baseline column that is NOT NULL with nothing to fill it and that the row leaves out (#2086); the checks, uniques, foreign keys and unique or filtered indexes the plan drops or replaces under their name gone (DEC-1998.1, DEC-1968.2) | Rows | Absent, Exists, Is, creates, rewrites |
| `DeleteRow` | 12 | Child rows moved away or cascaded: a child's rows leave before the rows of the tables it references, table against table (DEC-1968.2). The foreign keys the plan drops into its table, a rebuild's too, gone | Removes a row | Exists, Is, removes, rewrites |
| `SetPrimaryKey { to: Some }`, `AddUnique`, `AddIndex` | 13 | Columns, NOT NULL, unique data; a clustered index first. S: a computed column it names. A partition's `AddIndex`: the indexes its parent ends with (DEC-1688.1) | A key or index | Exists, Reads, creates, removes |
| `AddForeignKey` | 13 | Its columns, the referenced key, valid data: its own rows and the referenced table's, after the plan writes them | A foreign key | Exists, Is, Reads, creates |
| `SetReplicaIdentity` | 0 (0, 1) or 13, last | P: the key, unique constraint or unique index it names, over NOT NULL columns. At (0, 1) when that stands before the plan, before any drop of the old identity's index; after the additions otherwise | The table's replica identity (DEC-1444.1) | Exists, Is |
| `AddCheck` | 13 | Its columns, valid data. Functions it calls: *content* | A check | Exists, Reads, creates |
| `CreateModule`, `AlterModule` | 14 | What its definition names: other modules, by lexed name (DECISIONS 315); tables, columns and types | A module. P `AlterModule`: a drop and a create | Exists, creates, rewrites |
| `DropPolicy` | 0, before every module-drop rank | Its standing owner under the base name; before module drops, column drops and retypes; emitted transactionally | Removes one table-local policy and frees its name for replacement | Exists, removes |
| `CreatePolicy` | 14 (14, 2) | P: its standing or matching created table under the final name; all module creates/alters, structural changes, bootstrap inserts and policy drops first. Predicate bindings remain #595's named limitation | One table-local policy, with its own name namespace; a changed definition is drop plus create | Exists, creates |
| `SetRowSecurity` | 14 (14, 3) | P: its standing or matching created table and all planned policy drops/creates; modules and row writes, deletes included, first | Rewrites both independent RLS flags, carrying the complete before and after values | Exists, rewrites |
| `CreateRole` | 15 | The name free | A role | creates |
| `PublicExecution` | 14, at its routine's create rank | The routine exists | `PUBLIC`'s execute on a routine, settled in the statement after its `CREATE` (#687) | Is, rewrites |
| `Grant` | 16 | The role and the target exist | A permission | Exists, Is, rewrites |
| `SetDataMode` | 17 | The table's rows written | The table's data mode | Exists, Reads |
| `SetEnsureKeys` | 17 | The table's rows written | The table's managed ensure keys | Exists, Reads |

The three RLS kinds share phase rules in
`pbps_diff::order::row_security::edges`: policy drops precede module drops,
column drops and retypes, including in drop-only plans. Structural/table
changes, module creates/alters and their `PublicExecution` decisions precede
policy creation and flag changes; every policy create precedes every flag
change. Policy drops are setup suppliers too, so replacement creates and
flags follow them. Row writes, including deletes, finish before flags change.
The central CLI schedule and resolver graph use the same edges and
`Schedule::settle`, preserving classes 0 and 14 without another positional
reorder pass. These edges express phases, not predicate binding evidence.
Policy installs with different names on one table commute, as do flags on
separate tables.
A policy and check may share a name because their namespaces differ.

The PostgreSQL private emitter lowers the three physical rebuild parts, using
explicit parent-only index construction, a statement-atomic bounded cast copy,
and listed-tree ANALYZE. Logical roots, staged parts, certification, saved-plan
admission and CLI execution remain refused until the complete route ships.

## Pairs that interact

Interacting pairs fall into the categories below. Each category has one rule
that the classes satisfy by construction, stated at its head. A pair of
kinds in a category follows that rule unless its table says otherwise. The
tables list:
- every pair whose order something other than its classes decides (a rank,
  a pass, a refusal);
- every pair the classes get wrong;
- the pairs a reviewer is likely to ask about.

Policy creation interacts with module creation/alteration, table and column
construction, constraints and bootstrap inserts through the setup phase above.
Flags follow every policy drop/create and every row write. Standing policy
and flag transitions execute transactionally. Connected column
drop/retype/rename and routine rebuild under a policy the plan does not
explicitly remove or replace refuse by its catalog-proven name. A table
rename refuses only where it changes a retained policy's deparsed predicate:
ordinary local scalar predicates and schema-only moves keeping a whole-row
alias apply cleanly. Table/column renames under unmanaged policy owners also
apply cleanly and do not enter these rename refusals. Whole owner-table
drops release their policies before referenced table/column/routine teardown,
including sequence references reached through column ownership in `pg_depend`.
Renames and retypes preserve those sequences and need no such release edge;
cycles require explicit policy drops settled by the connected dependency checks.
Policies are never silently rebuilt or removed: only declared differences
produce explicit drops and creates.

A pair in no category is independent: neither side's effect meets the
other's requirement in the table above. Columns:
- **Kind:** *fixed*, *content*, *value* or *engine*, as defined under
  [Method](#method).
- **Order now:** what the mechanism does today.
- **Verdict:** ✓ where the order is right, with its evidence; ✗ where it is
  wrong, with its follow-up issue; ⧗ where it is left to the engine, with its
  record on #1350.

### Names changing hands

**Rule:** a change that frees a name runs in a lower class than any change that
takes it. Drops and renames away (classes 0 to 6) run before creates and adds
(7, 8, 13, 14, 15). The rule is by class, not by kind, so it holds across the
namespaces kinds share: on SQL Server tables and modules share one object
namespace, and on PostgreSQL views, tables and indexes share the relation
namespace. A rename *into* a name (classes 1 and 3) is held to the same rule
by class: `RenameRole` (1) takes a name `DropRole` (0) freed with no pass.
The exception is a rename whose freeing drop sits in a higher class than its
own: `DropColumn` (5) before `RenameColumn` (3), and before `RenameTable` (1)
a `DropTable` (6) or the drop of an index (P) or named constraint (S) holding
the name (2). `frees_a_renamed_column` and `rename_order` order those; the
rows below list each.

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| `DropTable` → `CreateTable` of its name | fixed | class 6 before 7 | ✓ |
| `RenameTable` away → `CreateTable` of the vacated name | fixed | class 1 before 7 | ✓ |
| `DropModule` (a view) → `CreateTable` of its name | fixed | class 0 before 7 | ✓ |
| `DropTable` → `CreateModule` of its name | fixed | class 6 before 14 | ✓ |
| `DropTable` or a rename away → `RenameTable` into its name | fixed | `rename_order` | ✓ DEC-536.1 |
| `DropColumn` → `RenameColumn` into its name | fixed | the drop moves to (2, 3), ahead of class 3 | ✓ DECISIONS 474 |
| `DropColumn` or `RenameColumn` away → `AddColumn` of its name | fixed | class 5 or 3 before 8 | ✓ |
| A column rename chain | fixed | `chain_depth` inside class 3; on a connected SQL Server plan, reordered by the collation's answer | ✓ DEC-1366.3 |
| A column rename cycle (a swap) | — | refused at diff time, `ColumnRenameCycle` | ✓ refused by design, DEC-541.1 |
| A table rename chain | fixed | `rename_order` | ✓ DEC-536.1 |
| A table rename cycle (a swap, a rotation, or a cycle through a cross-schema move's intermediate name) | — | left out of `rename_order`'s graph; refused at diff time, `TableRenameCycle` | ✓ refused by design, DEC-1966.1 |
| P: a detach vacating a clone's catalog name → another detach claiming it | engine-sourced | exact catalog releases are handover edges, settled with the effects; retained names and an unnamed key are not released | ✓ DEC-1558.1 |
| An owned sequence rename chain | fixed | (6, 1 + chain depth), `sequence_release_depths` | ✓ DEC-1889.1 |
| P: an identity given up or converted → a new relation claiming its sequence's catalog name | engine-sourced | the connected name handover is settled with the effects; a kept or retuned identity keeps its name | ✓ DEC-1986.1 |
| An owned sequence rename cycle (a swap or a rotation) | — | refused at diff time, `SequenceRenameCycle` | ✓ refused by design, DEC-1889.1 |
| A catalog-only holder (adopted or fallback default, unrecorded object, collation pair) → `RenameTable` into its name | engine-sourced | `object_order` on a connected SQL Server plan | ✓ DEC-1366.1 |
| A table rename moving a holder away (P) → a cross-schema rename carrying an index or owned sequence onto the held name | engine-sourced | on a connected PostgreSQL plan, `order_carried_moves` settles the plan over the name handovers, the catalog's occupants included, where only table renames pass one another; refused at `plan --db` with the remedy otherwise | ✓ DEC-1767.1, DEC-1957.2 |
| `RenameColumn` or another later class freeing a default name → `RenameTable` into it | — | refused at `plan --db` with the remedy | ✓ refused by design, DEC-1366.2 |
| Index (P) or named constraint (S) drop → `RenameTable` into its name | fixed | `rename_order` | ✓ DECISIONS 496, DEC-496.1 |
| Constraint or index drop → add of the same name | fixed | class 2 before 13; P: a constraint trigger's companion name, on its table and its partitions, is released by its module change before the constraint addition or partition creation/attachment taking it, by slot edges in `after_the_rebuilds`' settle | ✓ DECISIONS 168 (namespaces), DEC-2100.1, DEC-2100.2 |
| `DropModule` → `CreateModule` of its id | fixed | class 0 before 14 | ✓ |
| `DropRole` or `RenameRole` away → `CreateRole` of its name | fixed | class 0 or 1 before 15 | ✓ |
| `DropRole` → `RenameRole` into its name (S) | fixed | class 0 before 1 | ✓ |
| `DropRole` of a holder → `DropRole` of its member | fixed | once memberships are read, `order::role_drops` settles catalog edges within the original role slots, preserving the existing depth and peer order | ✓ DECISIONS 127, 139; DEC-1959.1 |
| Trigger and grants keyed by a table name that passes to another table | fixed | rebuilt as the occupant's | ✓ DEC-1118.1 |

### Names in force

Every change names its objects as they are named at its own position, so a
rename provides the name every later change uses.

**Rule:** a rename (class 1 or 3) runs after the module drops (class 0) that
name the old name. It runs before every change that names the new one.

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| `RenameTable` → `RenameColumn` and every later change naming the table | fixed | class 1 before 3 and the rest; `RenameColumn` carries the new table name | ✓ |
| `RenameTable` → class-2 drops naming the table | fixed | class 1 before 2; the drops use the new name | ✓ |
| Module drops (class 0) → renames | fixed | class 0 first; the drops use the old names | ✓ |
| S: a module drop the computed edges ordered (among the drops, or ahead of them with a pair) ↔ table renames | engine-sourced | on a connected plan whose renames `object_order` searches, the drop moves between what the edges order it after and before when the catalog holds an object under its name and a rename can claim it, and a table rename may move around it; any other keeps its place among the drops. A module's drop names the module, never a renamed table, and the walk frees its name where it runs | ✓ DEC-1461.1, DEC-1680.1 |
| `RenameColumn` → later changes naming the column (classes 4 to 17) | fixed | class 3 before them | ✓ |
| Constraint and index drops naming a column → its rename | fixed | class 2 before 3; the drops use the old name | ✓ DECISIONS 474 |
| `RenameRole` → `Revoke` and `Grant` naming the role | fixed | class 1 before 4 and 16. P: performed by hand, and grants follow the role's oid | ✓ ADR-0010 §3 |

Policy teardown names the base table: class-0 `DropPolicy` runs before its
class-1 rename. Its replacement `CreatePolicy` and `SetRowSecurity` use the
final table name; pairing uses the retained table UID, so a new table reusing
the old spelling is independent (DEC-1173.1, DEC-1173.2).

### Something created before what needs it

**Rule:** a change that creates an object runs in a lower class than every
change that needs it. Tables (7) and columns (8) come before alterations (9),
rows (11), constraints (13), modules (14) and grants (16). Roles (15) come
before grants (16). The exceptions, listed here, are the pairs inside one class
and the expression-bearing changes that need a function.

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| `CreateTable` → rows, keys, foreign keys, modules and grants on it | fixed | class 7 before 11, 13, 14, 16 | ✓ |
| `CreateTable` of a partitioned parent → `CreateTable` of its partition | fixed | (7, 2) after the rest of class 7 | ✓ DEC-1170.1 |
| `AlterColumnDefault`, `AlterColumnNullability`, or an `AlterColumnType` changing nullability, of a parent → its partitions' `SetPartitionDefault` and `SetPartitionNotNull` on that column | fixed | (9, 4) after the parent's class 9, and an effect edge in the resolver's schedule (#1610); the parent's change recurses over the partition's own (DEC-1687.1) | ✓ DEC-1687.1 |
| A parent's column change (`AddColumn`, `DropColumn`, `RenameColumn`, `AlterColumn*`) → `CreateTable` of a partition under it | fixed | (9, 5) after them: the partition is created with its own entries on the parent's columns as they end (DEC-1687.1) | ✓ DEC-1687.1 |
| `AttachPartition` → the partition's own `SetPartitionDefault`, `SetPartitionNotNull`, `SetTablePersistence`, `SetStorageParameters`, `AddIndex` and `AddCheck` | fixed | class 7 before 9, 10 and 13; each acts on the partition alone once attached. After an attach at (11, 1), its own alterations of classes 9 and 10 move to (11, 2) | ✓ DEC-1545.1 |
| A partitioned parent's `AddIndex` → a partition's own `AddIndex` | fixed | (13, 0) before the rest of class 13, an effect edge in the resolver's schedule (DEC-1968.2), first among the indexes the dependents' weave restores around a rebuilt function, and `before_its_partitions_indexes`, the last reordering of `plan --db`, which settles an edge from the parent's index to each partition index it could adopt, so a partition index the passes left before it sinks to right after it, with what rests on it, or refuses a cycle by name, carrying a change that creates a table, a column or a module among them: the parent's index built after a partition's own of its shape takes it as its clone (DEC-1688.1) | ✓ DEC-1688.1, DEC-1967.2 |
| `AddColumn` → rows, constraints, modules naming it | fixed | class 8 before 11, 13, 14 | ✓ |
| `AddColumn` (input) → generated `AddColumn` reading it | fixed | (9, 2) after class 8 | ✓ DEC-1168.1 |
| `AddColumn` or `AddComputedColumn` → another of the same table | fixed | their declared order inside the class, the computed ones after (9, 3); kept by the table's column order, written by each | ✓ DEC-2080.1 |
| `AlterColumnType` of an existing input → generated `AddColumn` reading it | fixed | (9, 2) after the in-place alterations: a standing generated reader blocks the retype | ✓ DEC-1168.1 |
| S: clustered `SetPrimaryKey`, `AddUnique` or `AddIndex` → the table's other added indexes | fixed (cost) | rank inside class 13. Built after them, the clustered layout would rebuild each one; correctness needs nothing here | ✓ DEC-1178.1 |
| `AddColumn` or `RenameColumn` (input) → `AlterColumnExpression` reading it | fixed | class 8 or 3 before 9 | ✓ DEC-1168.1 |
| S: `AddColumn`, `AlterColumnType` or `AlterColumnNullability` (input) → `AddComputedColumn` reading it | fixed | (9, 3) after class 8 and the class's alterations | ✓ DEC-1174.1 |
| S: `AddComputedColumn` → the index, unique or check over it | fixed | class 9 before 13 | ✓ DEC-1174.1 |
| S: function created → `AddComputedColumn` calling it | content, over-approximated | refused by name (`may_name`): the function is class 14 | ✓ DEC-1174.1 |
| Retype of a generated column → its `AlterColumnExpression` | fixed | rank −1 before 0: the new expression is computed in the final type | ✓ DEC-1168.1 (`a_generated_columns_nullability_relaxes_before_and_tightens_after_its_expression`) |
| Referenced key → `AddForeignKey` | fixed | rank inside class 13; P: a key delayed by a constraint-trigger name handover keeps its effects edge to the FK in the same settle | ✓ DEC-2100.3 |
| P: `SetPrimaryKey`, `AddUnique` or `AddIndex` of the identity's index, new or rebuilt → `SetReplicaIdentity` | fixed | rank 2, last in class 13. A rebuilt index is not the identity until it is set again, so the differ plans the identity wherever the plan re-adds its index. A key delayed by a constraint-trigger name handover keeps its effects edge to the identity in the same settle | ✓ DEC-1444.1, DEC-2100.3 |
| P: `SetReplicaIdentity` to a target that stands → `DropIndex`, `DropUnique` or `SetPrimaryKey { to: None }` of the old identity's index | fixed | (0, 1) before class 2. Dropped first, the old index leaves the table identifying no row until the identity is set | ✓ DEC-1444.1 |
| `CreateRole` → `Grant` to it | fixed | class 15 before 16 | ✓ |
| `CreateModule` or `AlterModule` → `Grant` on the routine | fixed | class 14 before 16. P: a rebuild's lost grants are restated after it | ✓ ADR-0010 §5 |
| `CreateModule` or `AlterModule` → `PublicExecution` on the routine | fixed | same class, same create rank and subject: the next statement, so an autocommit `--sql` script never leaves the routine executable by `PUBLIC` past its own `CREATE` | ✓ #687 |
| Structural/table changes, module creates/alters and `PublicExecution` → `CreatePolicy` or `SetRowSecurity` | fixed | shared `row_security::edges` setup phase, settled by the CLI schedule and resolver graph; bootstrap inserts finish before either kind | ✓ DEC-1172.2 |
| `DropTable` owning a policy → referenced `DropTable`, `DropColumn`, `AlterColumnType`, policy-changing table/column rename or routine removal | engine | exact `pg_policy`/`pg_depend` bindings, including relation-only references for table drops; catalog edges settle with structural, name-slot and routine constraints. Apply validates the saved order; an unchanged policy on a retained owner refuses unless the plan explicitly removes or replaces it | ✓ DEC-1172.1 |
| `CreateModule`, `AlterModule` or `PublicExecution` → `CreatePolicy` | fixed | shared setup edges; modules finish before policies at (14, 2) | ✓ DEC-1173.2 |
| `AddColumn` → `CreatePolicy` | fixed | class 8 before (14, 2), retained by shared setup edges | ✓ DEC-1173.2 |
| `DropPolicy` → `CreatePolicy` replacing the same owner UID and name | fixed | class 0 before (14, 2), retained by shared setup edges; full old/new payloads, no predicate analysis | ✓ DEC-1173.2 |
| `DropPolicy` or `CreatePolicy` → `SetRowSecurity` | fixed | shared setup phase: drops in class 0, creates at (14, 2), flags at (14, 3); both Created and Standing origins | ✓ DEC-1173.2 |
| Tightening → primary key or unique key over the column | fixed | class 9 before 13 | ✓ |
| Row writes → `SetDataMode` / `SetEnsureKeys` of their table | fixed | class 11 and 12 before 17 | ✓ |
| Module → module that names it | content, over-approximated | `creation_order_with`, lexed names (DECISIONS 315); on SQL Server a schema-bound function caller also requires the callee's `Final` definition, so its in-place rewrite orders the pair in the effects. Releasing a baseline schema binding instead precedes the old callee's rewrite through `BeforeRewrite`, settled by the shared schedule | ✓ DEC-2192.1, DEC-2228.1 |
| Function created or rebuilt → check, filtered index or set default calling it | content, over-approximated | `after_the_rebuilds`: a `Lexical` edge from the last function create, when its text names the function, settled | ✓ DEC-942.1, DEC-1364.1, DEC-1967.1 |
| Function created or rebuilt → `AlterColumnExpression` calling it | content, over-approximated | `after_the_rebuilds`, when its text names the function | ✓ DEC-1168.1, DEC-1364.1 |
| Function created or rebuilt → `AddColumn` whose default or generation expression calls it | content, over-approximated | `after_their_functions`: after the create its text names, and what may read the column, or a partition holding it, after it; a cycle is refused by name | ✓ DEC-1364.1 |
| `AddColumn` → a module created that reads it | content, over-approximated | class 8 before 14; reordered after a column that moves, when it names the column or its table | ✓ DEC-1364.1 |
| Function rebuilt → default a row of the plan takes | content | stays ahead of the rows | ⧗ #1030 |
| Function created → a new table's expression-bearing parts | content, over-approximated | `split_new_tables`, then `after_the_rebuilds`, when their text names the function | ✓ #1027, DEC-1364.1 |
| Function created → a new table's generated column calling it | content, over-approximated | shared creation parts keep the prefix before the function, then add the suffix in declaration order; a suffix partition key or a generation reading a later column instead uses SET EXPRESSION on connected PostgreSQL 17+, retaining the whole-table hold otherwise | ✓ DEC-1503.1 |

### Something removed before what it blocks

**Rule:** a change that removes a dependent runs in a lower class than the
drop, rename, alteration or row delete it would block. Module drops (0) and
constraint, index and key drops (2) come before renames (1, 3), column and
table drops (5, 6), alterations (9) and row deletes (12), and so does a table
drop before the deletes of rows it referenced. The exceptions, listed here, are the pairs inside
one class and the dependents a class cannot see.

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| `DropPolicy` → `DropModule`, `DropColumn` or `AlterColumnType` | fixed | class 0 before every module-drop rank and before classes 5/9; shared teardown edges survive settlement, including drop-only plans | ✓ DEC-1173.2 |
| Dependent module → its table, column or retyped column | fixed | class 0 first. P: rebuilt around a retype from the catalog | ✓ DECISIONS 311; `to_rebuild` |
| Catalog dependents of a dropped or rebuilt module | engine | `weave`, an expansion: added steps and `Catalog` edges, settled after each rewrite | ✓ DECISIONS 311, DEC-942.1, DEC-1958.2 |
| `DropModule` → `DropModule` of a module it names | fixed | `drop_rank`, deepest first inside class 0, from the base's lexed names | ✓ DECISIONS 311, 315 |
| Generated column's release (expression change, column or table drop) → drop of the function it calls | engine | a `Catalog` edge from each release, and from the drops of dependent modules, to the drop | ✓ DEC-1168.1, DEC-1958.2 |
| An identity given up → its column's nullability relaxed | fixed | rank −3 before −1. Measured on 16 and 18, `DROP NOT NULL` on an identity column is refused ("is an identity column") | ✓ DEC-1889.1 |
| Function drop moved after its release ← a column its own body reads, dropped or retyped earlier | content | the drop stays after the release | ⧗ #1350 |
| Foreign key → the key it references | fixed | rank inside class 2 | ✓ |
| Constraint, index or key drop → `DropColumn`, `RenameColumn` (S) | fixed | class 2 before 3 and 5 | ✓ DECISIONS 474 |
| Inbound foreign key → `DropTable` | fixed | class 2 before 6 | ✓ |
| `DropForeignKey` or `DropTable` of a child → `DeleteRow` of its parent | fixed | class 2 or 6 before 12; the pre-delete probe leaves the removed key or table out | ✓ DECISIONS 128 |
| `DropTable` of a partition → `CreateTable` of a partition over its range | fixed | class 6 before 7; the new range's pre-flight count leaves the dropped partition's rows out | ✓ DEC-1171.1 |
| `DropTable` → `DetachPartition` claiming a name the dropped table's index or key holds | fixed | (6, 1) before (6, 2) | ✓ DEC-1544.1 |
| `DetachPartition` → `CreateTable` of a partition over its range | fixed | class 6 before 7; the new range's pre-flight count leaves the detached partition's rows out | ✓ DEC-1544.1 |
| `DropTable` or `DetachPartition` of the DEFAULT partition → `CreateTable` of a range partition beside it | fixed | class 6 before 7; the DEFAULT is gone, and the partition is a plain create | ✓ DEC-1547.1 |
| `RenameTable` of a parent → `CreateTable` of a partition beside its DEFAULT | fixed | class 1 before 7; the move names the parent as renamed, its pre-flight as stored | ✓ DEC-1547.1 |
| `CreateTable` of a partition beside a DEFAULT → the DEFAULT's own tightening: `SetPartitionNotNull` to NOT NULL, `AddCheck`, `AddUnique`, a unique `AddIndex` | fixed | the moved rows may be what the tightening refuses. `SetPartitionNotNull` at (9, 6), after the create at (7, 2) or (9, 5); the others are of class 13. A create beside a DEFAULT is never held behind a function: its own default is split into a `SetPartitionDefault` after it (`split_moving_defaults` in `dependents.rs`). Each one's pre-flight probe is unchecked: it would count rows the move takes | ✓ DEC-1547.1 |
| `DropTable` or `DetachPartition` of a partition → `AttachPartition` over its range | fixed | class 6 before 7; the new range's pre-flight count leaves the leaving partition's rows out | ✓ DEC-1545.1 |
| `DropTable` or `DetachPartition` of a partition, the DEFAULT among them → `AttachPartition` as the DEFAULT | fixed | class 6 before 7; the pre-flight counts neither a leaving partition's range nor a leaving DEFAULT | ✓ DEC-1639.1 |
| A table's own `DropIndex` or `DropCheck` → its `AttachPartition` | fixed | class 2 before 7; the engine takes either order, an own index or check staying the table's across the attach | ✓ DEC-1545.1 |
| `SetPrimaryKey { to: None }` → relaxing a key column's nullability | fixed | class 2 before 9 | ✓ DECISIONS 269 |
| Generated column → its input's drop | fixed | (5, 0); (2, 2) beside a rename | ✓ DEC-1168.1 |
| S: index, unique or check over a computed column → its drop, and its re-add around an expression change | fixed | dropped in class 2 before (2, 4), re-added in 13 (`recreate_retyped_dependents`) | ✓ DEC-1174.1 |
| S: computed column dropped, alone or with its table → drop of a function it calls | catalog (`sys.sql_expression_dependencies`), connected | the function's drop moves after the column's or the table's drop, and what it is schema-bound to after it (3729 otherwise) | ✓ DEC-1431.1 |
| S: that function drop → the drop of a table it is schema-bound to, when the rename search reorders the plan | catalog, connected | `object_order` keeps the pairs `computed_order::drop_precedence` gives: the table drop waits for the drop of the function schema-bound to it | ✓ DEC-1461.1, DEC-1680.1 |
| S: drop of a module schema-bound to a computed column → that column's drop → the drop of a column it reads | catalog, connected | the computed column's drop moves after the module's, and the drop of a column it reads after it; the rename search keeps both as pairs | ✓ DEC-1680.1 |
| S: standing or re-added computed column → rename, drop, retype or nullability change of a column it reads; alter or drop of a function it calls | fixed, over-approximated | refused by name (`may_name`), with a two-plan remedy; a computed column the plan drops or changes is out of the way at (2, 4) | ✓ DEC-1174.1 |
| S: key, index, check or foreign key over a column → its retype or recollation | fixed | dropped in class 2, re-added in 13 (`retype_dependents`) | ✓ #1175, DECISIONS 515 |
| S: index key, `INCLUDE` column, filtered predicate or unique constraint over a column → tightening its nullability; filtered predicate → relaxing it | fixed | dropped in class 2, re-added in 13 (`nullability_dependents`), alone or inside a retype | ✓ DEC-1363.1 |
| P: generated column → retype or drop of its input | fixed | refused by name, with a two-plan remedy, unless the plan drops the generated column or changes its expression to one that does not name the input | ✓ DEC-1168.1 |
| P: expression change releasing an input → that input's retype or drop | engine, over-approximated | `release_generated_inputs` moves the retype or drop right after the release. The old reads are `pg_depend`'s; a new text naming the input keeps it refused (`may_read`) | ✓ DEC-1316.1 |
| P: a released input's drop ↔ a crossed `InsertRow` | effect and catalog | an omitted required input or disappearing default delays the insert and its transitive row/key consumers until after the release and drop. An insert consuming the old default stays before its drop; nullable inputs without a default and trigger timing retain their existing order | ✓ DEC-2174.1, DEC-2291.1, #2086 |
| P: retype → another generated column's expression change that starts reading it | engine, over-approximated | an edge in `release_generated_inputs`; with a drop instead, refused by name | ✓ DEC-1391.1 |
| Old default removed → retype → new default set | fixed | dependency ranks −2, −1, 0 inside class 9. S: the old default blocks the retype (5074). Both: a default written for the new type is invalid under the old one | ✓ (`a_default_written_for_the_new_type_is_set_after_the_type_is`) |
| `Revoke` on an object the plan drops | fixed | not emitted | ✓ |

### Values one change writes and a later one reads

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| Retype → probes of later keys, checks and deletes | value | the probe projects through the new type, or the probe is unchecked | ✓ DECISIONS 340, 410 |
| `AddColumn` backfill → probes | value | a literal is projected; an expression, identity or generated column is unchecked | ✓ DECISIONS 336, 339 |
| `DeleteRow` → `SetRowSecurity` | value | class 12 before (14, 3), retained by shared setup edges; row guards see the pre-change flags | ✓ DEC-1173.2 |
| Row writes → probes of keys, foreign keys and checks | value | the probe reads the rows after the plan's writes | ✓ DECISIONS 335 |
| Row writes setting a column, or deleting the rows that hold its NULLs → tightening it to NOT NULL | fixed, value | (12, 2), after the rows of its table; split from a retype; the probe reads the rows after the plan's writes | ✓ DEC-1367.1 |
| `AlterColumnExpression` → tightening, keys, checks, filtered indexes, deletes | value | the probe is unchecked; the order is tighten and keys after the recomputation | ✓ DEC-1168.1 |
| Default set → a row that takes it | value | the default goes first; it stays ahead of a rebuild | ✓ #1030 |
| Parent's default set → a partition's own default on that column | value | `order::parent_defaults`: expansion supplies a missing restoration; parent and compatibility constraints are settled, retaining the old bytes | ✓ DEC-1581.1, DEC-1959.4 |
| Nullability relaxed → expression recomputing a NULL | value | rank −1 before 0 | ✓ DEC-1168.1 |
| Nullability tightened → an identity put on the column | fixed | rank +1 before +2. Measured on 16 and 18, the engine refuses an identity on a nullable column ("must be declared NOT NULL"). Where the plan inserts rows into the table, both stay in class 9 ahead of the inserts, the tightening folded into a retype; where it only updates or deletes them, both follow the deletes, at (12, 2) and (12, 3); where it deletes and inserts both, the table's inserts follow the identity at (12, 4), and another table's rows referencing it are refused (#1367) | ✓ DEC-1889.1 |
| Retype → a numbering source put on, or an owned sequence kept, on the column | fixed | rank −1 before +2: a sequence is created or moved `AS` the final type, and the column's values are read in it | ✓ DEC-1889.1 |
| A kept identity retuned → a retype that narrows its column | fixed | (9, 0) before (9, 1). Measured on 16 and 18, the engine moves an identity's sequence with its column and refuses the retype while a bound set for the wider type stands ("MAXVALUE (100000) is out of range for sequence data type smallint"); bounds declared for the narrower type fit the wider. A widening keeps the retype first | ✓ DEC-1921.1 |
| A plain default dropped → a numbering source put on the column | fixed | rank −2 before +2. Measured on 16 and 18, the engine refuses an identity on a column with a default ("already has a default value"), and a `serial`'s default would replace it unseen | ✓ DEC-1889.1 |
| A numbering source given up → a plain default set in its place | fixed | rank −3 before 0: the block that gives up an owned sequence drops its default, which would take the new one with it, and the new default may need the retype at −1 | ✓ DEC-1889.1 |
| An owned sequence given up or renamed → a created table or added column whose sequence takes its name | fixed | class 6 before 7 and 8: the name is free before `CREATE SEQUENCE` claims it. Neither the drop nor the rename needs a class-9 change first | ✓ DEC-1889.1 |
| A numbering source put on → rows the plan writes into its table | fixed | class 9 before 11: a row that leaves the column out takes the new source. A key it numbers takes no declared rows (ADR-0013 §2) | ✓ DEC-1889.1 |
| Expression recomputed → nullability tightened | value | rank 0 before +1; split from a retype | ✓ DEC-1168.1 |

### Rows among themselves

**Rule:** rows follow the foreign keys between their tables. A referenced
table's rows go in first and come out last. Deletes run after every insert
and update (SPEC §4.6).

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| Parent row → child row (insert, update) | fixed | data rank inside class 11 | ✓ |
| Child `DeleteRow` → parent `DeleteRow` | fixed | the data rank, negated inside class 12 | ✓ |
| Row update moving a reference away → `DeleteRow` of the old parent | fixed | class 11 before 12 | ✓ |
| `UpdateRow` or `DeleteRow` of a table with a foreign key to a partitioned parent → `DropTable` or `DetachPartition` of a partition of it | fixed | an edge from each, settled by `after_the_rows_that_leave`, where the drop passes only rows, other tables' column changes and drops; otherwise class 6 before 11, refused by the pre-flight. P: the pre-flight counts each referencing row as it will be when the detach runs | ✓ DEC-1553.1 |
| `InsertRow` or `UpdateRow` into a table a partitioned parent's foreign key references, or `AttachPartition` under that table → `AttachPartition` under the parent, which validates the key over the rows it brings | fixed | (11, 1), at the parent's data rank: with attaches in the plan, the data order covers their parents too (`supply_order`) | ✓ DEC-1545.1 |
| `AttachPartition` → `InsertRow` or `UpdateRow` into a table referencing its parent, and `AddForeignKey` into the parent | fixed | class 7 before 11 and 13; at (11, 1), the referencing table ranks after every table the parent references. P: the parent's rows counted with the attached table's | ✓ DEC-1545.1 |
| `InsertRow` or `UpdateRow` taking a value another row holds under a UNIQUE elsewhere → `DeleteRow` of that row | content | the write first (class 11 before 12): it is refused inside the transaction, loudly. Deleting first could cascade a moving child away silently | ✓ by design, SPEC §4.6 |

## Content-dependent pairs

Every content-dependent pair is the same question: does this expression or
body name that function or column? The planner never answers it from the
text's meaning (DECISIONS 174). It answers with **a lexical
over-approximation**: the text is read through the dialect's lexer, and every
name it could refer to is taken as an edge. An edge too many costs nothing but
a later position; one too few puts a change before what it needs. The answer
is conservative, so it can never be wrong in the dangerous direction.

- `creation_order_with` reads each module's definition this way (DECISIONS
  315).
- `after_the_rebuilds` reads a check's, an index's, a default's and a
  generation expression's text for the functions the plan creates or rebuilds
  (DEC-1364.1). Only an addition whose text names one moves after the last
  create (DEC-1967.1); the rest keep the differ's place. A new column whose
  expression names one follows that create, and what may read the column
  follows the column: a module naming it, or naming its table. Where the
  function itself may read the column, or a row the plan writes needs it,
  no order performs both, and the plan is refused by name with a two-plan
  remedy. Over-approximated in both directions, a refusal can name a cycle the
  engine would not have, almost always in a plan the earlier positional rule
  also left to fail (DEC-1364.1 names the exception).

- `names_a_later_relation` reads the same texts' literals for a relation the
  plan creates after them, which PostgreSQL resolves as the expression is
  created: `'app.ix'::regclass`. It does not move anything; the plan is refused
  with a two-plan remedy (DEC-1576.1).

What is left to the engine is recorded on #1350, each with a two-plan remedy:
- an expression change still calling a function the plan rebuilds;
- a function dropped after its release whose own body reads a column dropped
  or retyped earlier;
- a default a plan row takes calling a rebuilt function (#1030).

New tables use creation parts that preserve column order. A suffix partition
key, a generation reading a later column, or a transitively called module
naming the table or its types instead keeps the whole-table hold without a known
PostgreSQL 17+ capability. Consumers use the complete shape unless a reference
is proven to resolve only the relation name; the reverse regclass pair still
uses prefix edges. Creation risks require each receiving part before every row
write into that table identity (DEC-1503.4). Selection walks the same lexical
and declared module dependency edges as settlement (DEC-1503.5).
The exhaustive structural classification also puts partition creates and
attachments after their parent's complete columns. Attachments introduce
standing rows for risk derivation, as do writes through current partition
ancestors and DEFAULT moves (DEC-1503.6).

## Value flow

Each value-flow pair has its own guard today:
- a retype is projected through a cast (DECISIONS 340, 410);
- a literal backfill is projected (DECISIONS 339);
- planned row writes are read as the rows after the plan (DECISIONS 335);
- a recomputed generated column is reported unchecked (DEC-1168.1).

They all follow one rule: **a probe that reads a value an earlier change of the
plan rewrites either projects that change, or is reported unchecked.** No
probe judges the old value. What the per-guard form lacks is a single place that
names, for each change kind, which values it rewrites. A new kind that rewrites
values, such as a SQL Server computed column (#1174), has to remember every
probe that might read them. Folding the guards into one "values this plan
rewrites before position *i*" table in `pbps-pg::preflight` and its SQL Server
counterpart would make that table the one row a new kind adds.

## Findings

| Finding | Kind | Disposition |
|---|---|---|
| S: tightening a column an index or a unique constraint covers is refused at apply | fixed, wrong | fixed, DEC-1363.1 |
| Tightening runs before the reference rows that fill its NULLs, and its probe ignores them | fixed, wrong | fixed, DEC-1367.1 |
| Content-dependent corners of expression-bearing changes around function creates, rebuilds and drops | content | #1350; the lexical over-approximation above closes the new-column corner (DEC-1364.1) |
| Value-flow guards are per-probe, not one rule | value | recorded above; a refactor when a new value-rewriting kind arrives (#1174) |

Every other interacting pair is ordered correctly. Its evidence is the
decision or the issue named in its row.

## Keeping this current

- A new `Change` variant adds a row to [the table](#what-each-change-requires),
  with its effects in `pbps_model::operation::effects` (an exhaustive match,
  so it does not compile without them) and in the table's last column.
  `every_change_kind_has_a_row_in_the_ordering_audit` lists the variants from
  the enum and refuses one this file does not name, and
  `the_requires_table_states_the_effects_of_each_kind_it_names` refuses a last
  column the code disagrees with.
- A new ordering rule states an effect on existing resources, or supplies a
  factual constraint to an existing order-owned route. Only a new obligation
  needs a typed expansion. Check scoped compatibility/search predicates and
  the existing base policy as well: effects are not a promise that these
  semantic review sites have vanished. Do not add a caller-owned reorder
  stage. Preserve completed-route authority and compiler-negative coverage
  when changing its boundary (DEC-1959.5, DEC-1959.6).
- A new interacting pair found in review goes into the matching table with its
  verdict. It is not fixed in isolation: decide whether it is fixed or
  content-dependent first.

The completed canonical result retains its planner's invariant type brand.
The sealed route-state/dialect pair permits re-scheduling only with that same
engine, including semantic fixture observation. Zero-SQL scope additions carry
the brand in a non-executable Continuing state until shared scheduling finishes;
scratch preparation also requires the input's original engine. These compile-time
restrictions add no pass, constraints, rank or runtime refusal.
`OrderedPlan<D>`, `AdmittedPlan<D>` and `SealedPlan<D>` implement only
`PlanView<D>`; `PlanDialect` fixes each real engine's input to its own view.
CLI dispatch preserves the concrete engine before certification, and
`LoweredPlan` retains that emitter's invariant brand. Semantic `dyn Dialect`
queries and test-only observations cannot erase or substitute this authority.
Singleton support is absent from ordinary and release builds (DEC-1959.5).

Bootstrap's prerequisite script guard is prepared from the engine's declared
schema/trust context with a certified lowered plan. Its immutable script keeps
that engine and the existing guard-before-DDL bytes; it adds no ordering stage
or executable operation. Bootstrap's per-statement prerequisite checks select
positions from the certified collection (DEC-1959.5, DEC-2181.1).

## Declared type pairs (DEC-1635.3)

| Pair | Decision | Mechanism |
|---|---|---|
| CreateType → CreateTable/AddColumn/AlterColumnType using it | final type must exist | class 7 (type subrank before tables) before 8/9; declared `Is(Type, Final)` effect |
| CreateType → CreateType domain over it | base first | domain-base depth rank within class 7, plus `Is(Type, Final)` |
| RenameType → typed column creation/retype and dependent type rename | final spelling first | class 1, domain-base depth rank and final type effect |
| Type creation/rename → defaults, checks, modules and grants naming it | final name first | classes 1/7 before defaults/checks/modules/grants (9/13/14/16) |
| DropTable/DropColumn/retype away → DropType | last column released first | classes 6/5/9 before 18; `Absent` table/column or final-column `Is` effects |
| DropModule/drop default/drop check → DropType | dependent gone first | classes 0/9/2 before 18 |
| DropType of a domain → DropType of its base | dependent domain gone first | reverse baseline domain-base depth rank within class 18, plus `Absent(Type)` effect |
| Drop/rename a type, table or view → claim its Types name | old holder releases first | existing slot handover edges, including cross-kind class inversions |
| RenameType chains | free destination first | existing slot edges; cycles/swaps refuse by name in the differ |
| Independent type operations and all remaining pairs | no dependency | stable class/rank key; no extra pass |

The shadow verifier uses the same type resources, final-incarnation requirements,
slot claims and releases; pending Types handovers join the existing
`after_the_rebuilds` slot settlement, which also carries their dependents.
The release verifier remains unchanged.
