# The differ and statement ordering

How a difference becomes an ordered list of changes and statements. Part of the
[decision record](../DECISIONS.md), which says how to add an entry here.

<a id="decision-5"></a>

5. **IDENTITY changes are blocked** (cannot be done with ALTER).

<a id="decision-12"></a>

12. **`ALTER COLUMN` restates the whole definition**, and an omitted
    `NULL`/`NOT NULL` means `NULL` — so `AlterColumnType` carries nullability,
    `AlterColumnNullability` carries the type, and the differ folds a
    type+nullability change into one `AlterColumnType`.

<a id="decision-26"></a>

26. **ONLINE is edition-dependent, and only a connection knows the edition.**
    The emitter writes `WITH (ONLINE = ON)` where the statement takes one — a
    UNIQUE constraint is index-backed and does, a foreign key and a check are
    metadata only and the clause is a syntax error there — and `plan --db`
    reads `SERVERPROPERTY('Edition')` and refuses before writing a plan the
    server would reject. An offline plan says the hint is unverified.

<a id="decision-31"></a>

31. **Module changes bracket the table changes.** Drops sort first (a
    SCHEMABINDING view blocks a rename), creates last (they select columns that
    must exist), and within each group the order comes from an identifier scan
    over the definition text — of the **code only**, so a name in a comment or
    a literal invents no edge — with `depends_on:` as the escape hatch.
    *Known limitation*: an alter that **releases** a schema-bound dependency
    would have to run first, and one rank cannot serve both directions. Left
    unfixed on purpose; the reasoning and the trade are in ADR-0002 under
    "Known limitation".

<a id="decision-61"></a>

61. **An unnamed declared primary key matches any stored name.** A declaration
    that writes `primary_key: [id]` leaves the name to the engine, and the
    engine invents one that the recorded state then carries. Comparing names
    there restated `SetPrimaryKey` — a `constraint` risk — on every connected
    plan until somebody copied `PK__t__357D4CF8...` into the file, which is a
    demand nobody outside `pull` would meet. So an unnamed declaration compares
    columns only; a *named* declaration is compared in full, because renaming
    a constraint is a change the plan has to carry. The cost is that the
    differ cannot express "give this key a name" for a declaration that has
    none — which is the declaration saying it does not care.

<a id="decision-123"></a>

123. **The names a plan's remaining statements need free are compared with
    one another, not only with the catalog.** 119 asked the engine which
    existing principal holds each wanted name; two declared roles the
    database reads as one name — `Reader` and `reader` under a
    case-insensitive collation — held nothing in the catalog, passed, and
    the second `CREATE ROLE` failed after everything before it had run,
    committed under a staged apply. The wanted names go to the engine
    numbered, joined to themselves under `COLLATE DATABASE_DEFAULT` (now
    `CATALOG_DEFAULT`, DEC-1243.1) on
    `a.i < b.i`, and any pair refuses the plan by name before the catalog
    is asked. Declared tables and modules have the same latent shape —
    `dbo.Foo` beside `dbo.foo` — but it predates this phase and is not
    engine-checked anywhere yet; it is recorded here rather than fixed in a
    review round.

<a id="decision-208"></a>

208. **The differ compares the declarations against what was declared when
    each object was last written, and falls back to the read-back where
    nothing was recorded.** `Declared::overlay` lays the recorded texts over
    the read-back the connected plan (and a snapshot `--base`) compares
    against; a git baseline is declarations already. Where the record has no
    text for an object — an environment adopted with `baseline`, a version 6
    state, an object created by hand — the read-back stands, which is what
    the differ always compared. On an engine that stores what it was given
    that is the same answer as before this change; on one that respells, the
    object is restated once and the apply records what it declared, which is
    the "restated once" ADR-0009 §2.2 asks for, reached by a fallback rather
    than a rule of its own. And the read-back is not the engine that stores
    what it was given: **measured**, SQL Server reads a default `GETDATE()`
    back as `(getdate())`, a check `n > 0 AND label <> 'none'` as
    `([n]>(0) AND [label]<>'none')`, and a filtered index's predicate the
    same, so `plan --db` straight after a `bootstrap` restated the default,
    dropped and re-added the check and dropped and rebuilt the index —
    marked destructive — on every run. ADR-0013's Limits called that a
    reasoned worry, unmeasured on SQL Server; it was a shipped bug, and the
    live test pins the fix.

<a id="decision-236"></a>

236. **A unique index is gated and counted as the constraint it is, and a
    filtered one only over the rows its predicate keeps — or not at all.**
    SQL Server enforces a `UNIQUE` constraint *with* a unique index: the two
    are one object, and which YAML key the uniqueness was written under
    (`unique:` or `indexes: {…, unique: true}`) cannot decide whether the
    change faces the gate or is counted. It did: `AddIndex` had no risk class
    and no probe, so adding `unique: true` over a column holding duplicates
    planned as a no-risk change and failed at the engine mid-apply — where,
    under `--staged`, every earlier checkpoint has already committed.

    The filter is the part that is not obvious. A filtered index constrains
    only the rows its predicate keeps, so counting the others reports
    collisions the engine exempts and refuses a plan it would have accepted —
    the "invent a violation" direction, and the worse of the two. But the
    predicate is arbitrary SQL over the whole row, while the relation
    `rows_after` builds is a few projected columns unioned with rows that are
    not in the table yet. It can only be asked of the stored branch, and only
    where this plan leaves that branch alone: not where the plan writes a row
    (an update that touches no key column still moves a row in or out of the
    filtered set), not where it renames a column of the table (the text then
    names nothing, or — when a second column is renamed into that spelling —
    silently names the wrong one), not where it adds one (not there to be
    read), and not where it retypes one. That last is 152's trap on the other
    side of the query: there `UNION ALL` reconciled the branches by data-type
    precedence, here it is comparison precedence, and no conversion can help,
    because the predicate is the user's text over columns this projection
    never selects. Measured: `flag int` holding `1` twice under
    `WHERE [flag] = '01'` keeps both rows, so the probe counts a collision and
    refuses the plan; retype `flag` to `varchar` and the stored values read
    `'1'`, which the predicate excludes and the engine creates the index over
    nothing.

    Where any of those hold the probe disappears, which is the answer every
    other unspellable value gets here (165, 171). It disappears *silently*: a
    probe that is never built is not among the ones `apply` reports as
    unchecked, since that count is of probes that ran and could not answer.
    That is the standing behaviour of every `Ok(Vec::new())` in `preflight`
    and not a property of this change, so it is not fixed here — see issue
    #145. The gate is what carries the change either way: the risk class does
    not depend on whether a count could be taken, so the approval still asks a
    human. The alternative — counting unfiltered and
    calling it conservative — is not conservative at all in this direction; it
    refuses valid plans.

<a id="decision-237"></a>

237. **The constraint and index drops run before the column renames, all of
    them.** `order_key` renamed a column first and dropped the constraints
    around it after. For most of them that is right and #123 had just stopped
    them being restated at all — measured, `sp_rename` carries a primary key,
    a unique constraint, an index's key and `INCLUDE` columns and a child's
    `references_table` for free. Two kinds it cannot carry, and there the
    order was backwards: measured on SQL Server 2025, a rename of a column a
    **check constraint** names is refused with 15336, and one a **filtered
    index's predicate** names with 5074, and 4922 behind it. The plan a valid
    revision produced —
    rename, drop the check, add it back with the new spelling — could not run,
    and no declaration the user could write fixed it. The escape was two
    revisions.

    **The whole group moves, not the two kinds that need it.** Moving only
    `DropCheck` and the filtered `DropIndex` means asking which columns a
    check's expression names, and this tool deliberately never parses that
    expression (174). Moving all four costs nothing instead: measured, a
    rename is never blocked by a constraint that does not name the column, so
    `DropUnique`, `DropForeignKey` and an unfiltered `DropIndex` are as safe
    one class earlier as one class later. They still precede `DropColumn`,
    which is the other thing their old position was for, and
    `DropForeignKey`'s `dependency_rank` of -1 travels with them and still
    puts it ahead of the key it references.

    **They move ahead of the column renames, not ahead of the table rename.**
    A drop names its table, so a drop emitted before `sp_rename` on the table
    would name a table that no longer exists — the shape #118 fixed. Measured,
    neither a check nor a filtered index blocks a *table* rename, so there is
    nothing to gain by going further.

    **`Revoke` could not travel with them.** It shared the drops' class and
    has the opposite need: it names the object as the renames leave it. So it
    stayed where it was and took a class of its own, which is the negative
    case the ordering tests pin.

    The re-add stays in the constraint class far below, after the renames,
    which is where the new spelling can be written. Only the drop moves.

<a id="decision-262"></a>

262. **`online` builds an index concurrently only when it has no filter.**
    Measured, `CREATE INDEX CONCURRENTLY` cannot share a batch with anything at
    all — `cannot run inside a transaction block` — so it cannot carry the write
    `search_path` of 259, and a path set by a preceding statement is not there
    after a staged apply resumes on a new connection. An index *with* a filter
    is therefore built the ordinary way and the hint is dropped, which is the
    trait's own rule for a hint a dialect cannot honour on this statement: the
    destination is the same either way, and refusing would turn a performance
    hint into an outage. An index *without* one has no expression to bind and
    needs no path, so nothing is lost by leaving the scope off it.

    The concurrent statement says both things about itself —
    `Statement::non_transactional` and `Statement::own_batch` — so a plan
    carrying one is refused at plan time with the whole plan intact, rather than
    halfway through an apply. That also makes `own_batch`'s own comment false
    where it said PostgreSQL has no batch restriction; it has exactly this one.

    A unique constraint gets no concurrent path either, and for a different
    reason: the online spelling is `CREATE UNIQUE INDEX CONCURRENTLY` followed
    by `ADD CONSTRAINT … USING INDEX`, whose halves commit separately. One
    declared constraint arriving as two committed steps is a state the gate
    never approved.

<a id="decision-266"></a>

266. **A nullable primary key column is refused on PostgreSQL too, and for the
    opposite reason.** SQL Server refuses the table at `CREATE`, so its rule
    (`validate.rs`) only moves the failure earlier. **Measured, this engine
    accepts it** and sets `NOT NULL` itself:

    ```text
    CREATE TABLE t (id integer, CONSTRAINT pk PRIMARY KEY (id));  accepted
    the column afterwards:                                        attnotnull = t
    ALTER TABLE t ALTER COLUMN id DROP NOT NULL;
        -> ERROR 42P16: column "id" is in a primary key
    ```

    So the declaration and the database disagree from the moment the table
    exists, the pull reads `nullable: false`, every plan proposes the
    `DROP NOT NULL` that would put the declaration back, and the engine refuses
    that one for ever. A declaration an engine silently rewrites is worse than
    one it rejects, and the rule is more necessary here than on the engine it
    came from.

    Written from the measurement rather than inherited: `pbps-pg` had no
    key-column checks at all, which is PITFALLS' "the second implementation did
    not inherit the first one's scar" with the scar in the wrong shape as well
    as missing. The rest of `pbps-mssql`'s `key_columns` — a key naming a column
    the table does not have, naming one twice, naming none, or naming one whose
    type cannot be part of a key — is missing here too and is issue #175: those
    four fail loudly at the server, which is late but not silent, and this one
    does not fail at all.

<a id="decision-269"></a>

269. **A primary key that is only dropped is ordered with the constraint drops;
    one that is replaced is not.** `order_key` had every `SetPrimaryKey` in the
    addition class (13), below every column change, because one variant carries
    both directions. So a declaration that gives up a key and relaxes the
    column it held produced a plan whose first statement neither engine would
    perform:

    ```text
    ALTER TABLE t ALTER COLUMN id DROP NOT NULL;   -- 42P16 on PostgreSQL:
                                                   -- column "id" is in a primary key
    ALTER TABLE t DROP CONSTRAINT pk_t;            -- never reached
    ```

    **Measured on both engines**, which is what makes this the differ's problem
    and not a dialect's: SQL Server refuses the same shape with 5074, "the
    object 'pk_pkord' is dependent on column 'id'", and 4922 behind it. A
    valid, reviewed plan, refused.

    The drop now sits in class 2 with `DropIndex`, `DropUnique`,
    `DropForeignKey` and `DropCheck` — where a constraint drop belongs, and
    where it also lands ahead of `DropColumn` at 5, the other statement a
    standing key blocks. No new class and no renumbering: `dependency_rank`
    already keeps a foreign-key drop ahead of the key it references *inside*
    this class, which is the order the engine requires and the reason that rank
    was written (DECISIONS 237).

    **Conditioned on `to: None`, not on the variant**, because a key being
    replaced is no longer one change — see DECISIONS 270.

<a id="decision-270"></a>

270. **A replaced primary key is planned as two changes, its drop and its
    add.** DECISIONS 269 put a key's drop with the constraint drops by keying
    the class on `to: None`, and left the replacement where it was. Review
    found the half that leaves open, and it is the same defect: a declaration
    turning `PRIMARY KEY (id)` into `PRIMARY KEY (other)` while relaxing `id`
    still ran `DROP NOT NULL` against a column `pk_t` held — `42P16` on
    PostgreSQL, 5074 with 4922 behind it on SQL Server.

    One change cannot be ordered correctly here, and no class can rescue it.
    The drop must precede every column change a standing key blocks; the add
    must follow every column its new shape may name, including one this same
    plan adds at class 8. Opposite ends, so: two changes.

    Nothing else moves. The model is unchanged, and both emitters already
    emitted the two statements independently — `if let Some(pk) = from` then
    `if let Some(pk) = to` — so the SQL is the SQL it was and only the
    positions change. `from: None` on the add half is accurate where it runs,
    because the drop half has already taken the key away.

    **The risk classes follow, and that is the intended consequence rather
    than a side effect.** A replacement used to answer `Constraint` alone;
    now its drop answers `Destructive` and its add `Constraint`. That is what
    the database does — the old key and its index are gone — and a gate that
    was told only about the constraint was told half of it. A policy that
    denies `Destructive` will now stop a key replacement, which is the
    conversation that should have been happening.

    The plan a reviewer reads changes shape with it: one line becomes two, in
    different places, each with its own risk. That is more to read and it is
    the truth about what runs; the alternative is one line that hides a drop
    among the additions.

<a id="decision-271"></a>

271. **A column with a default and a new type is three phases, one rank each.**
    Both change kinds are ordering class 9, so the tiebreaker decided which ran
    first, and the tiebreaker is the change's `Debug` rendering — which puts
    `AlterColumnDefault` ahead of `AlterColumnType` by the alphabet and nothing
    else. Each end of that is refused, by a different engine, and both are
    measured:

    ```text
    the default first, PostgreSQL:
      ALTER TABLE t ALTER COLUMN n SET DEFAULT 'abc';
          -> ERROR: invalid input syntax for type integer: "abc"
    the type first, SQL Server:
      ALTER TABLE t ALTER COLUMN n bigint NULL;
          -> Msg 5074: the object 'df_dn2' is dependent on column 'n'
    ```

    So neither order works and the answer is three phases: **drop the default
    the old type gave meaning to, change the type, install the default written
    for the new one.** `dependency_rank` answers `-2` for
    `AlterColumnDefault { to: None }`, `-1` for `AlterColumnType` and `0` for
    `AlterColumnDefault { to: Some(_) }` — the same instrument, and for the same
    reason, as the foreign key's rank: the dependency is a layering rather than
    a graph, so a constant says it exactly. No new class and no renumbering.

    A *replaced* default is two changes when the column is retyped, because the
    type change has to run between the halves — the third instance of "one
    variant carrying both directions cannot be ordered by direction"
    (DECISIONS 270, PITFALLS). **Only** when the type moves: a default replaced
    on a column that keeps its type needs no drop, since `SET DEFAULT` replaces
    on PostgreSQL and the SQL Server emitter already drops and adds inside its
    one statement, and splitting it would put two lines at opposite ends of a
    plan where one says it better (SPEC §14.1).

    The nullability needs no rank of its own: `Change::AlterColumnType` carries
    both ends, so the differ never emits a nullability change beside a type
    change on one column — and measured, SQL Server accepts the nullability
    form of `ALTER COLUMN` with a default standing, so the dependency is the
    type's alone.

    **The second measurement arrived by breaking it.** The rank went in with
    only PostgreSQL's end measured, `AlterColumnType` at `-1`, and that reversed
    an order SQL Server had been relying on by accident: with the default's drop
    sorted first by the alphabet, a retyped column's constraint had always
    happened to be gone. Nothing in the suite covered it. The regression test is
    `a_retyped_column_gives_up_its_old_default_before_the_type_moves`, on the
    engine that refuses it.

    The tiebreaker's own comment said this would happen: "anything with a real
    order between them belongs in separate classes; this tiebreaker cannot
    express it." A rank is the third way, and it is the one that costs no
    renumbering.

    What this does not reach is a type change on a column whose default the
    plan does not touch: there is no `AlterColumnDefault` to order, and SQL
    Server refuses that statement too. It is that engine's emitter to fix —
    issue #180 — because emitting a drop-and-re-add for an unchanged default
    would put two lines in every plan that widens a defaulted column, on both
    engines, for one engine's constraint model.

<a id="decision-272"></a>

272. **`CREATE TABLE` names its access method, and does so in the statement.**
    The reader accepts a table only when `relam` is heap (`catalog.rs`), which
    is deliberate — everything below the model, from column storage to the way
    a page is read, is heap's. An unqualified `CREATE TABLE` takes its method
    from `default_table_access_method`, so under a role whose setting names
    another installed method the table is created *successfully* and then reads
    back as an unsupported object: absent from the pulled schema, planned again
    as a `CREATE` the engine refuses for already existing, and the deployment
    cannot converge. The apply reported success and the recording says the
    table is as declared.

    **In the statement, not in the transaction framing** beside the session
    pins of DECISIONS 267, and the difference is the point: those settings
    cannot be said in the statement they affect — there is no way to spell
    `DateStyle` inside a check constraint — while this one can. A clause cannot
    be answered differently by a session, on any path, including the rendered
    `--sql` script an operator runs through `psql` with no framing around it
    (issue #174). Where both are available, the one that cannot be defeated is
    the one to write.

    `default_tablespace` is the same kind of setting and is deliberately left
    alone: the reader does not filter on it, nothing in the model speaks about
    where a table's storage lives, and a plan that neither declares nor records
    it has nothing to be wrong about. There is no equivalent for indexes —
    measured, `default_table_access_method` is the only such setting on 18.6,
    and an index's method comes from its own `USING` with `btree` as the
    grammar's default rather than a session's.

    **The divergence itself is not measured, and that is stated rather than
    hidden.** The pinned image ships exactly one table access method, so there
    is no second one to create a divergent table with. Both halves are
    measured — the setting exists and is validated against the installed
    methods, and `USING heap` fixes `relam` — and the reader's rule is code.

<a id="decision-295"></a>

295. **A referenced foreign-key target may be a partitioned table; a managed
    table may not.** `doctor`'s table question filtered `relkind = 'r'` for
    every list it asks about, taking the rule from the pull — where it is right,
    because an index and a sequence are rows in `pg_class` too and a partitioned
    table is one this model cannot hold at all.

    A target of somebody else's is not this project's table, so what the model
    can hold says nothing about it. Measured on 18.6:

    ```text
    shared.parent PARTITION BY RANGE (id)             ->  relkind = 'p'
    CREATE TABLE app.child (..., pid integer REFERENCES shared.parent(id))
      as a role with no grant on the parent            ->  42501: permission denied for table parent
      once REFERENCES is granted on it                 ->  CREATE TABLE
    ```

    Read at `r` alone, the target came back *absent* — and an absent securable
    is asked for nothing, by design (there is nothing to grant on) — so the
    check demanded neither the `REFERENCES` the key needs nor the `SELECT` its
    probe performs, and reported an environment ready that the very next
    statement refuses. The kinds are the caller's now: `r` for what this project
    manages and for the ledger, `r` and `p` for what a declared key points at,
    which are the two kinds this engine lets a key reference.

<a id="decision-460"></a>

460. **Replacing a referenced key carries its foreign keys through the typed
     plan (issue #177).** `pbps-diff` now adds `DropForeignKey` and
     `AddForeignKey` for retained managed foreign keys whose referenced
     primary/unique constraint or standalone unique index is dropped. Existing ordering places those changes
     before the key removal and after the replacement; risk classification
     runs afterward, so recreation carries `constraint` and FK removal keeps
     its existing ungated classification. Explicit FK changes are not
     duplicated. Table/column identity is brought forward before matching,
     including self-references and composite keys. Ordinary unchanged keys
     produce no extra work. Constraint names still mean drop plus add; no
     engine-specific rename change or saved-plan field is introduced.

     Measured on PostgreSQL 18.6 and SQL Server 2025, an FK binds a particular
     backing index. Creating another UNIQUE over the same columns does not
     let the old key drop: PostgreSQL reports `2BP01`, SQL Server `3727`.
     Dropping the FK first, dropping the old key, then recreating the FK
     succeeds and binds it to the remaining key. PostgreSQL permits a
     permutation of referenced composite columns; SQL Server rejects that
     declaration with `1776`. The differ therefore matches column sets
     conservatively and exposes all affected managed FK recreations for
     review. It does not claim to know which of several equivalent keys a
     standing FK uses. The connected dependency check answers that question
     from the catalog, so an external FK bound to a different standing key
     does not block the change.

     PostgreSQL extends the existing object-address DROP dependency graph to
     primary/unique-key and index roots. SQL Server reads
     `sys.foreign_keys.key_index_id` against `sys.indexes.index_id`, resolving
     constraint-backed keys through `sys.key_constraints`. Both count only earlier typed
     removals and reverse only preceding table renames when querying the
     original catalog. A missing existing key is a failed read, not a report
     of no dependencies. An FK outside the plan is named and refused during
     connected planning and checked again before apply; nothing is added to
     the approved plan at execution time. SQL Server's table/column DROP
     reader remains separately unavailable.

     SQL Server's new key check requires database `VIEW DEFINITION`:
     measured, ALTER plus VIEW DEFINITION on the parent alone returns zero
     `sys.foreign_keys` rows for an external child; the database permission
     exposes the dependency. An explicit child-table DENY still hides it while
     the database-level permission check answers true; that denial's own row
     remains readable. The guard therefore also refuses effective object/schema
     metadata denials, including those inherited through a database role,
     while respecting owner overrides. The check refuses insufficient visibility
     instead of treating that zero as proof of absence. This requirement is asked only
     for an existing unique key drop. SQL Server first reads the index kind
     with the parent's metadata rights; an ordinary nonunique or filtered
     unique index drop does not acquire a database-level permission
     prerequisite. Measured on both engines: a filtered/partial unique index
     cannot back an FK, so the pure differ excludes it from managed-FK
     recreation too. The SQL Server regression replaces a filtered index
     with only parent-level grants, while an unfiltered unique index still
     requires complete dependency visibility.

     Live CLI tests on both engines cover primary and unique key replacement,
     the explicit four-change saved plan, orphan refusal after recreation,
     an empty next plan, and external dependencies present during planning or
     arriving after approval. Engine tests distinguish two keys over the same
     columns, earlier/later FK removal, renamed tables, missing keys and SQL
     Server metadata visibility. Counterfactuals restore the old planner and
     each connected guard independently and observe their corresponding new
     regression fail. The existing staged restriction remains: this is a
     multi-change transactional plan, and `--staged` refuses it before writing
     an artifact.

     The first review found the same dependency on a standalone unique index:
     its `DropIndex`/`AddIndex` changes had been omitted from all three key
     scans. The differ now includes unique baseline indexes, and both connected
     guards cover their actual index identities. The shared regressions run
     all three key kinds, with a nonunique-index negative control. Restoring
     each reviewed scan independently makes its new regression fail.

     SQL Server FK enforcement flags also stay outside the declaration: a
     disabled, untrusted (`WITH NOCHECK`) or `NOT FOR REPLICATION` FK cannot
     be recreated as an ordinary enabled, trusted FK without changing its
     semantics. The catalog carries all three flags into the pure assembler,
     which omits the FK with one named relation limitation per constraint.
     Existing managed-set drift checks then refuse connected planning and
     apply before writing; pull reports the limitation. No model or saved-plan
     fields are added. Composite-FK unit tests retain the ordinary control,
     and live CLI tests cover clean and orphaned disabled/untrusted keys,
     replication semantics, changes after approval, unchanged engine flags on
     refusal, and applying the same plan after an explicit operator repair.

<a id="decision-461"></a>

461. **SQL Server retypes preserve defaults and explicitly rebuild managed dependents (issue #180).**
     Measured on SQL Server 17.0.4075.5, changing `int` to `bigint` is refused
     while a default, CHECK, primary/unique key, ordinary/filtered index or FK
     binds the column. Modifier-only changes preserve CHECKs; bounded
     `varchar`, `nvarchar` and `varbinary` widenings also preserve ordinary
     indexes and keys. FKs and filtered predicates still block those widenings;
     fixed-width, numeric modifier and `max` transitions do not share the index
     exception. The dialect reports these distinctions through the pure
     `retype_dependents` hook. The differ adds ordinary drop/add changes before
     sorting and classification, retaining destructive/constraint approval and
     probe behavior. Column lists select exact dependents; CHECK/filter text
     stays opaque, so these expressions are conservatively selected at table
     scope. Identity alignment handles table/column renames, and explicit
     replacements/removals are not duplicated. Referenced-key expansion also
     covers FKs whose backing unique index includes a retyped non-key column.
     PostgreSQL requests no such maintenance and keeps its existing plan shape.

     An unchanged default remains part of its column, without extra shared
     `AlterColumnDefault` changes or saved-plan fields. The SQL Server emitter
     saves the catalog's actual constraint name and definition, drops it,
     changes the type, then restores the same default in one isolated batch.
     A named default with an unreadable definition refuses; an absent default
     remains absent. Dynamic identifiers and literals retain their quoting.
     A default explicitly changed by this plan follows decision 271's existing
     drop/type/add ordering. The transaction restores the default if conversion
     fails. Cross-table dependencies are never discovered as extra runtime DDL;
     the saved plan carries their ordinary risks and connected guards.

     Rebuilding may not replace unmodelled write behavior with default behavior.
     The catalog therefore inventories disabled/untrusted/NOT FOR REPLICATION
     CHECKs and disabled/IGNORE_DUP_KEY unique indexes or key constraints as
     limitations; connected commands already refuse a managed limitation before
     writes. The names stay in the diagnostic, not as ordinary enforcing model
     objects. Ordinary disabled nonunique indexes are access-path differences
     and retain the existing storage-policy boundary of issue #171.

<a id="decision-463"></a>

463. **A failed concurrent index build recovers its own invalid artifact (issue #186).**
     Measured on PostgreSQL 18.6, a duplicate-key failure during `CREATE UNIQUE
     INDEX CONCURRENTLY` leaves an index with `indisvalid = false`. Dropping it
     concurrently lets the next attempt reach the duplicate-key failure again,
     instead of colliding with the old index name. A failed statement therefore
     does not mean that this non-transactional operation left nothing behind.

     The emitter attaches the table and index identity to the concurrent build's
     `Statement`, beside its transaction and batch markers. This is derived
     execution metadata, not SQL parsing, a model field or a saved-plan change.
     PostgreSQL's connected staged executor captures the table OID and whether
     the index name already exists before executing. On failure it removes only
     a newly present invalid index on that same table, using the emitter's
     quoted `DROP INDEX CONCURRENTLY`. Pre-existing names, valid indexes and
     objects on a different table are preserved. This retains SPEC 7.6's
     single-deployer assumption; it does not lock out concurrent external DDL
     that replaces an object during the recovery window.

     Recovery finishes before the CLI writes its existing failed-attempt audit.
     Its outcome names the artifact and wraps the original driver error, so
     ledger redaction retains the SQLSTATE and our own cleanup account while
     excluding driver text (455/456). If inspection or cleanup fails, the error
     says the artifact may remain and requires inspection before retrying;
     cleanup never replaces the original failure or claims success. The
     ledger's existing source-first ordering preserves this outcome even when
     the emitted SQL exceeds the reason column's width.

     A failure of the first non-transactional statement leaves no checkpoint.
     Its message directs a corrected fresh staged attempt, without `--resume`;
     existing resume validation still refuses that failed ordinary state.
     Checkpointed failures retain their resume path. Transactional statements
     and SQL Server's staged execution retain their existing behavior.

     The live CLI regression injects duplicate rows at DDL start, after the
     preflight, without a scheduling race. It verifies repeated failure and
     cleanup, the failed ledger row, no-checkpoint resume refusal, cleanup
     denial without losing the original SQLSTATE or recording driver text,
     and eventual successful application. A long composite index pins durable
     outcome ordering. Engine cases preserve pre-existing valid and invalid
     indexes, another table's index and a same-named table, exercise hostile
     quoted names, and retain successful builds and transactional controls.
     Reverting the recovery dispatch, no-checkpoint guidance and pre-existing
     object guard independently makes the respective live regression fail;
     restoring them passes both suites.

<a id="decision-474"></a>

474. **The column drop that frees a name sorts before the rename that claims
     it.** A deploy can skip a revision: `note` dropped in one, `label` renamed
     into the name it gave up in the next, and the plan is the difference
     between the deployed baseline and the last of them. `resolve` accepts both
     revisions and the differ accepts the diff — correctly, because the
     occupant is going — and then `order_key` handed the engine the rename
     first, at class 3, with the drop behind it at class 5. **Measured** on
     both pinned images, with the doomed column still in place:

     ```text
     EXEC sp_rename N'[dbo].[s].[label]', N'note', 'COLUMN'
       ->  Msg 15335: The new name 'note' is already in use as a COLUMN name
           and would cause a duplicate that is not permitted
     ALTER TABLE s RENAME COLUMN label TO note
       ->  ERROR: column "note" of relation "s" already exists
     ```

     Both take the two statements in the other order. So this was a valid,
     reviewed plan neither engine would perform, with no declaration the user
     could write to fix it — the shape 174 records for a check constraint
     blocking `sp_rename`, one namespace down.

     **The drop moves, not the class.** `order_key`'s own doc says why a new
     ordinal is expensive: the numbers are quoted in prose that justifies
     behaviour, and inserting one means renumbering all of it in the same
     commit. The sort key carries a rank *inside* the class instead, so the
     freeing drop sits between class 2 and class 3 without moving anything
     else: after the constraint and index drops, because a column a check or
     an index names cannot be dropped while they stand, and before the rename
     that is waiting for its name. The existing reorder of a freeing index
     drop ahead of a table rename (176, 467) becomes the first rank of class 1
     under the same scheme, unchanged in effect.

     **The drops that move are a table's, not a name's**, and two review
     rounds are why. The first version asked `Dialect::fold_ident` whether the
     dropped name was the one the rename claims. On SQL Server that is the
     identity function, and correctly so: what makes two spellings one column
     name there is the *database's* collation, which a plan computed offline
     does not have (SPEC 7.3). **Measured** on the pinned image, each in a
     database of the named collation:

     ```text
     SQL_Latin1_General_CP1_CI_AS   note vs Note  ->  one name, Msg 15335
     SQL_Latin1_General_CP1_CI_AI   café vs cafe  ->  one name, Msg 15335
     Latin1_General_CS_AS           note vs Note  ->  two names, both kept
     ```

     Lowercasing on top of the fold answered the first line and the review
     produced the second; width and kana sensitivity are two more flags on the
     same collation name, and a binary collation is another answer again. Each
     fold that comes up short refuses a valid plan at the engine, which is the
     defect this entry exists to fix, one collation further out. So the
     question asked is the one that is true under every collation: **a rename
     can only collide with a column of its own table**, so on a table this plan
     renames a column of, every column drop runs first.

     The over-match is free, which is what makes the wide answer the right one
     rather than the lazy one. Nothing between the drop's old class and its new
     one can notice it: a `Revoke` names no column, and a column this plan
     drops is never a rename's source, since the two come from different uids
     and no two baseline columns share a name. A drop on another table does not
     move at all. A rename into a
     name nothing in the plan gives up never reaches this sort at all —
     `resolve` refuses it as an occupied target, which is what keeps the
     reorder from reading as an implicit drop.

     **And the closing guard has to know the name belongs to two columns.**
     Found in review, and it is the other half of the same fact: with the
     ordering fixed the engine performs the plan, and then
     `refuse_unplanned_movement` refused it. That comparison is keyed by name;
     the baseline holds both spellings, so the rename rewind is skipped (the
     ambiguity rule of 189), and the entry the baseline has under `note` is the
     doomed column while the entry the read-back has is the survivor. Comparing
     them reads the rename as a retype of a column nobody touched — invisible
     while the two happen to share a definition, and `note int` against `label
     nvarchar(50)` is a difference `ColumnField::Whole` does not excuse. A
     valid plan, refused at its own checkpoint after the engine had already
     performed it. The `was` side now falls through to the rename's source
     whenever this plan drops the occupant of the name, which is the branch
     that already existed for the ordinary rename. The doomed column is not
     left uncompared: its absence is what `columns_after` holds the plan to,
     and in `order_key` order the rename's `Present` is the net promise about
     the name (280) — which is the ordering above, once more, doing the work.

     **And the fall-through only across the rename itself** — the earlier read
     holding the source and the later one not. Two more review rounds, one for
     each half, and both are the same mistake: a fix for a false refusal that
     opens a false acceptance is the worse trade, and neither half was visible
     from the read the fix was written for.

     Without the first half, a staged run's closing check — which compares two
     checkpoints, by then both holding the survivor and neither holding the
     source — fell through to nothing, and the rename's own `Whole` excused the
     resulting `None` as a one-sided column. A concurrent retype between the
     last checkpoint and the closing read would have been recorded as this
     plan's result. Without the second, a checkpoint spanning the *drop* alone
     did the same when another session put the name back before the read: the
     source is still there, so the rename has not run, and what stands under
     the claimed name is a replacement rather than the survivor. SPEC 7.6
     promises to catch both.

     **The sweep this closes, and the one it does not.** Every other pair
     where one change gives up a name another claims already ran in the right
     order: a column drop before an add (5 before 8), an index or constraint
     drop before its re-add (2 before 13), a role drop before a role rename (0
     before 1), a module drop before a module create (0 before 14), a table
     drop before a table create (6 before 7). One pair is wrong the same way
     and is **not** fixed here: `DropTable` at 6 against `RenameTable` at 1,
     where a table dropped in one revision gives its name to a table renamed
     in the next. Measured, both engines refuse that too (`Msg 15335`,
     `42P07`), but the drop cannot take the same trip: it has to run after the
     `DropForeignKey` of every child still referencing it, and those are class
     2, already behind the rename — so freeing the name means moving the
     foreign-key drops as well, which is the reordering 467 records as unsafe
     while a pinned one is present. Filed as issue #536 rather than widened into
     this one.

<a id="decision-496"></a>

496. **Table renames and the drops that release their names form specific
    dependencies, and a moved drop carries its execution address.** On PostgreSQL,
    a table rename may claim the schema-wide name of another table's index or
    candidate key, including the intermediate name of a cross-schema transfer.
    A flat promotion class cannot put a constraint drop after its owner's rename
    and before a different claimant's rename. Replace that promotion with a
    deterministic graph over the plan's table renames, freeing index/key drops,
    and only the foreign-key drops whose baseline references match those keys.
    Compare referenced column sets conservatively: the offline differ cannot
    identify which of several equivalent candidate indexes backs a foreign key.
    An unrelated foreign-key drop remains after its owner's rename.

    Preserve the preferred owner-rename-before-drop edge whenever it is acyclic.
    If the drop is itself a prerequisite of that rename, use the original table
    and schema in the typed drop instead. This lets a table's own index release
    its cross-schema destination, or its foreign key precede the key that releases
    its rename target. The initial dependency graph is FK -> key -> rename;
    checking each preferred owner edge before adding it keeps the graph acyclic,
    including mutually blocking owner preferences. Stable original sort positions
    break ties. This group stays after module drops and before other table work;
    column, data, authorization and module ordering retain their existing classes.
    Dialects with table-scoped index names retain ordinary ordering.

    No emitter may silently rename an address or reorder the reviewed changes.
    The existing typed changes, saved order and checksum carry the choice, without
    new model fields or a plan-format change. Declared expression/binding records
    consume that same order, removing an early dropped filter before rekeying its
    table. The apply movement check follows a part's subsequent table renames for
    shape exclusions and net postconditions; an early drop and a later re-add must
    answer together under the final table name. The dropped object reappearing,
    a wrong re-add definition, and untouched-part movement still refuse the plan.

    Measured on PostgreSQL 16 and 18: the original order fails with 42P07 for the
    pinned foreign-key, cross-schema self-index and two-owner constraint cases;
    the typed dependency order executes and preserves rows. Unit, generated-SQL,
    connected blocker, saved-plan/apply and recorded-state tests cover unrelated
    foreign keys, same/cross-schema names, unique and primary keys, filtered-index
    retention, and absence/definition negatives. Historical table-name reuse and
    column rename chains remain separate scopes (#536 and #541); this graph adds
    no implicit drop or rename intent and no connected I/O to the differ.

<a id="dec-536-1"></a>

**DEC-536.1. A dropped table releases its name in the DECISIONS 496 graph, with
the foreign-key drops it needs.** Two revisions deployed together can drop
`app.target` and then rename `app.old` into its name. `DropTable` ran in its own
class after every rename, so the plan was one neither engine performs: measured,
`sp_rename` into the doomed name is Msg 15335 on SQL Server 17.0.4075.5 and
`ALTER TABLE … RENAME` is `42P07` on PostgreSQL 18.6. A dropped table releases
its own name on every dialect, so the graph no longer returns early when the
dialect shares neither indexes nor constraints with tables. The table also
releases the names it carries, by the dialect's two answers. A drop whose name
nothing claims keeps its class.

The drop cannot run while another table's key names it, so every foreign-key
drop that references it moves ahead with it. Its own key drops, which the
differ emits separately, move too, as do the keys another dropped table has on
it. A dropped table's own keys carry its baseline name, which for the doomed
table a rename into that name also carries once it has run, so they keep that
address and are never rekeyed to the rename's source. Where both tables have a
key of one name (PostgreSQL scopes constraint names to the table), the two
changes are identical, and the first is taken as the doomed table's. Which key
references the doomed table is read from the baseline, where a table renamed
into its name is still under its source.

The apply movement check keys both reads by table name, and that name now
belongs to two tables across one plan, as a column name did in DECISIONS 474.
A read that still holds the rename's source holds the doomed table under the
name; its entry is skipped, and the renamed table's own entry compares it.
Holding both spellings no longer stops the rename being undone when this plan
drops the occupant: that is the plan's own order, and refusing it reported an
untouched child's key, which follows the rename, as moved. A single revision
that renames into a surviving table's name is still refused by `resolve`.
`DropModule` needs no change: it sorts before every table rename already.

A key that reads the same by name in the first and last revision can still
point at two tables: it referenced the doomed table, went with it, and a later
revision re-added it under the same name to the table renamed into that name.
Compared by name alone nothing changed, yet the standing key blocks the drop
(Msg 3726 on SQL Server) and the engine would never bind it to the new
occupant. The differ compares the referenced table's uid on the two sides, read
from the baseline as spelled there, and plans a visible drop and re-add when
they differ.

A rename releases its source name as it runs, so a rename into that name runs
after it. Skipped revisions that rename `z` into `a` and then `y` into `z` form
a chain the alphabet otherwise ordered (`y -> z` first, Msg 15335). A move to
another schema also releases the intermediate name it passes through, its
source name in the destination schema: `s1.z -> s2.a` holds `s2.z` for a
moment, so `s1.y -> s2.z` waits for it (Msg 15530, `42P07`). A cycle —
two tables trading names through a third — has no order an engine takes
without a temporary name; its edge is left out, and the engine refuses it as
before. The apply movement check likewise undoes a rename whose target this
plan renames away, as it does one whose occupant it drops: an untouched
child's key follows `y` to `z` and is not movement. Each read undoes only the
renames that had run when it was taken — a rename has run when its source name
is gone, or holds the next link, whose rename has run. One map for both reads
rewrote the earlier read's `z`, still the original table, to `y`, and a key
that followed the chain's head from `z` to `a` read as moved.

<a id="dec-541-1"></a>

**DEC-541.1. A column rename into a name another rename vacates runs after
it, a cycle is refused, and the closing check follows each column by the
plan's own changes.** Skipped revisions that rename `b` to `c` and then `a`
to `b` leave a chain. Both renames sat in one class, and their minted uids
decided the order, so `a -> b` ran while `b` still stood, which is `Msg 15335`
on SQL Server and `column "b" of relation "t" already exists` on PostgreSQL.
A rename now ranks by how many links of its table's chain it waits on, so the
far end, whose target is free, runs first. `resolve` accepts every step of a
swap through a third name (`a -> c`, `b -> a`, `c -> b`), which nets to a
cycle; no order of it runs, so the differ reports it instead of emitting one.
The deployer chooses a temporary name or separate deployments.

The closing movement check compared the two reads by column name. When a name
changes hands within the plan, because one rename gives it up and another
rename or a new column takes it, both reads hold that name for two different
columns, and no condition on names can tell them apart: excusing the pair
would accept concurrent drift under that name. Such a plan makes at least two
changes, and `--staged` applies one (ADR-0003), so it is transactional. The
earlier read is therefore from before its first statement and the later one
from after its last, and the plan's column changes, in its order, say which
baseline column each final name holds. The earlier read's columns are re-keyed
to those names, and the later read's constraint columns are undone to the
baseline names, before the usual comparison. Tables where no name changes
hands keep the one-hop rule of DECISIONS 189 and 474 unchanged.

<a id="dec-1178-1"></a>

**DEC-1178.1. A SQL Server table's clustered index is one table-level
selector, absent means the key, and the emitter always spells the key's
layout.** `clustered:` is `heap`, `{unique: <name>}` or `{index: <name>}`;
absent is the engine's own default, a clustered primary key or a heap without
one. That default is what every table pbps created before this had, which is
why a state or plan without the field reads as it rather than being refused.
A flag on each key and index was the other shape. It lets two objects both say
clustered, which the engine refuses (1902), and a validator would have to catch
what the selector cannot express at all. It also has no good default: a
`clustered: false` default on the key re-plans every existing key as a
rebuild, and a `true` default has to be switched off by hand whenever another
object is clustered. The selector names the object by its declared name and
by kind, since a UNIQUE constraint and an index may share a name in the
declarations. Constraints and indexes are never renamed in place, so the
selector follows a rename by naming the new object.

A move of the clustered index rebuilds the object that gives it up and the
one that takes it, each as a drop and an add; the engine has no in-place form
(`DROP_EXISTING` refuses clustered to nonclustered, 1925, and the reverse
while a foreign key references the key, 1930). A key rebuilt this way takes
its foreign keys through the rebuild, as any replaced key does. The clustered
object is added ahead of the rest of the addition class, because building it
rebuilds every nonclustered index already on the table.

The emitter writes `CLUSTERED` or `NONCLUSTERED` on every primary key it
creates, although the clustered case is the engine's default. Measured on
17.0, a bare `PRIMARY KEY` added beside a clustered index nobody declared is
silently nonclustered, and the recording then says clustered; spelled
`CLUSTERED`, the same statement is refused (1902). A UNIQUE constraint's and
an index's default does not depend on the table, so only `CLUSTERED` is ever
spelled on them. Drops stay offline: `ONLINE = ON` on a drop is accepted only
for a clustered index (3745 otherwise), and the drop change does not carry the
layout of what it drops. This replaces ADR-0003's reason ("this emitter writes
no `CLUSTERED`"), which no longer holds.

`pull` declares a clustered rowstore index or key (narrowing DECISIONS 14 and
#1186's limitation). It still leaves out a key backed by any other index
kind, a disabled clustered index, and a nonclustered key whose table's
clustered index it left out (a clustered columnstore included): without that
index the key's layout is one no declaration can write. It also leaves out
each of these newly adopted layouts on a table whose rows sit on a partition
scheme, since the declarations hold no data space and bootstrap would build
the table unpartitioned; the shapes adopted before this, on partitioned rows,
are #1227. PostgreSQL refuses the field, since `CLUSTER` is a
one-time reorder rather than a layout. The operational estimate calls a
clustered build, and the drop of an object the catalog confirms is clustered,
a rewrite of every row under Sch-M (measured on 17.0: new partition ids for
the table and each nonclustered index); every other index change stays
unmeasured.

<a id="dec-1243-1"></a>

**DEC-1243.1. A SQL Server identifier is compared under `CATALOG_DEFAULT`,
not `DATABASE_DEFAULT` (#1215, #1243; amends DECISIONS 119, 123 and DEC-384.1).**
Those entries asked the engine to compare names under the database's default
collation. That is the collation a database stores its data under. It is not
necessarily the one it names things under. A partially contained database
names its objects, columns and principals under a fixed catalog collation of
its own. Measured on 17.0, with `CONTAINMENT = PARTIAL COLLATE
Latin1_General_100_CI_AI`: a role, a table, a column and a constraint named
`cafe` can each stand beside one named `café`. `DATABASE_DEFAULT` reads each
pair as one name, so a check under it refuses a valid plan or pairs the wrong
objects. On a database that is not contained, the two collations are the same,
so the rule changes nothing there.

The rule applies to every catalog comparison of an identifier in
`pbps-mssql::catalog`:
- `principals_holding`;
- `matching_table_names`;
- `tables_reusing_a_column_name`;
- `names_alike`;
- `object_names_alike`;
- `object_name_occupants`.

It does not apply to two other kinds of `COLLATE` clause:
- a comparison of data values, as in `preflight.rs`, which stays under the
  database default;
- a clause that only gives a `UNION` column one collation without comparing
  anything, as in `owned_query`.

Pinned by `a_partially_contained_target_needs_a_server_that_allows_contained_databases`
(`crates/pbps-mssql/tests/support/resolver.rs`). It is the only test allowed
to switch `contained database authentication`. It asks all six functions about
`cafe` and `café` on a `PARTIAL` and a `NONE` database.

<a id="dec-1175-1"></a>

**DEC-1175.1. A column declares an explicit collation or none, and none is
the default of whichever database the column is created in.** `collation:`
names one; absent is the database default. The catalog cannot tell an
inherited default from the same name spelled out (DECISIONS 443), so the
reader declares a collation only where it differs from the connected
database's default. A connected plan compares the declarations with a
collation equal to the live default taken out (`Schema::without_collation`).
Compared as written, an unchanged column was altered to the collation it
already has on every plan, and a created one read back without it failed the
apply's postcondition. An offline preview cannot do that, and shows such a
column as a change; the connected plan is the one that is applied. The plan
records the live default (`PlanBaseline::database_collation`), and `apply`
refuses before its first statement if the database's default has changed
since — a staged resume before the statements it has left. The baseline checksum only sees that when a managed character column
sits under the default. A column the plan adds without a collation takes the
default in force when it runs, and reads back as none either way. (Measured
on 17.0: the ledger's own `CHECK` on `__pbps_lock` makes the engine refuse
`ALTER DATABASE ... COLLATE` (5075) until that check is removed, so this is
rare, but it is not impossible.) A name is compared without ASCII case
(`Collation`), as the engine resolves it.

The emitter spells `COLLATE` on every statement that writes a column
definition, `ALTER COLUMN` included, whether or not the collation changes.
Measured on 17.0, an `ALTER COLUMN` without it moved a `Latin1_General_CS_AS`
column to the database default with no error. So `AlterColumnType` and
`AlterColumnNullability` carry the collation, and the apply guard holds the
read-back to it. A collation change is an `AlterColumnType` with the type
restated. It is the same statement, and it is blocked by the same dependents:
measured, an index (key, INCLUDE or filter), a PRIMARY KEY or UNIQUE, a
foreign key on either side and a CHECK each refuse it (5074); a DEFAULT does
not. The dialect's `recollate_dependents` makes the differ take all of those
down and put them back, with a key's foreign keys.

A non-Unicode column's collation change is `narrowing`. Its bytes are in the
collation's code page, and measured, `ééééé` in `varchar(5)` kept `éé` under a
UTF-8 collation. Which code page a collation uses is not known offline. The
probes convert under the target collation instead of `DATABASE_DEFAULT`, and
count a rebuilt key's duplicates under the collation it will have. A Unicode
column's collation change takes no class of its own; the keys rebuilt around
it carry theirs. A foreign key's two sides must have the same collation
(1757). Offline, only two named collations that differ can be refused: an
absent one facing a named one is valid exactly when the named one is the
target's default, so `plan --db` asks again once it knows it. A name the server lacks is refused before the first statement (448).
The operational estimate reports a collation change as unmeasured.
PostgreSQL refuses the field.

<a id="dec-1169-1"></a>

**DEC-1169.1. An index names its access method and, per key, a non-default
operator class. The first slice holds GIN over `jsonb` only.** `method:` is
`btree`, which is the default and is written only when it differs, or `gin`.
A key is `column [opclass] [asc|desc]`, in PostgreSQL's own order. An absent
class is the method's default for the column's type. Absent-means-default
keeps every index declared, recorded or planned before this reading as the
index it always was.

PostgreSQL accepts GIN over `jsonb` columns under `jsonb_ops` (spelled by
leaving the class out) or `jsonb_path_ops`, and a B-tree only under default
classes. Spelling `jsonb_ops` is refused, as is `unique`, `include` or `desc`
on GIN. Measured on 18.6, the engine refuses the last three itself. Anything
else is refused before apply, because the reader would report it as a
limitation after apply. SQL Server builds every index as a B-tree and has no
classes, so it refuses `method: gin` and any class. `method: btree` is only
the default spelled out, and `fmt` writes it back out of the file.

The reader takes each key's class from the catalog as `schema.name`, or empty
for the resolved default. It accepts only `pg_catalog.jsonb_path_ops`. A class
of that name in another schema is another class, and is left out and named.
The emitter writes the class qualified (`"pg_catalog"."jsonb_path_ops"`) so
that the `search_path` cannot stand another in its place. A collation that is
not the column's own is still left out and named on its own account.

An index this slice now reads was never in a recorded state, because
`plan --db`, `baseline` and apply's closing read refuse a managed set that
carries a limitation. A declaration pulled before this slice lacks the index,
so a plan from it proposes a `DropIndex`. That drop is Destructive and waits
at the risk gate. The remedy is to pull again.

State version 10 carries the two fields and still reads 6 to 9, where their
absence is true of every recorded index. Plan version 14 turns 13 away. The
declaration schema is published as set 20.

Pinned by `a_gin_index_over_jsonb_round_trips_and_changes_as_a_typed_plan`
(`crates/pbps-pg/tests/live.rs`), which reads hand-written DDL, rebuilds it,
replaces and drops an index, and refuses to take a B-tree for the GIN index.

<a id="dec-1169-2"></a>

**DEC-1169.2. An index key is a column or an expression, and which one is
what its declaration says, never what its text looks like.** The model's key
is `IndexKey::Column` or `IndexKey::Expression`. It is an enum and not an
optional text beside the name, so a key cannot be both or neither, and a
reader that wants a column must say what it does with an expression. A
column key serializes exactly as every key did before (`name`), so older
states and plans read unchanged and a column key's fingerprint does not move.

YAML keeps `columns: [a, b desc]` for an index of columns and adds `keys:`,
one mapping each (`{column: id}` or `{expression: "lower(email)"}`, with
optional `opclass:` and `order:`), for an index with any expression. An
index names its keys under one list, not both. `columns:` cannot also hold
mappings: an entry read without a declared type makes a plain `n` or `on` a
YAML boolean, and a column may be named either. Parentheses decide nothing,
because a quoted identifier may contain them.

PostgreSQL holds expression keys on a B-tree only, under the default class
and the database's default collation. Neither an expression's own collation
nor its class's default is in the catalog by name, so anything else is left
out and named. Measured on 18.6, `lower(email) COLLATE "C"` shows
`indcollation` 950 against 100. The reader takes each expression's text by
position (`pg_get_indexdef(oid, k, true)`), and its type from the index
relation's own attribute. It never guesses a column from an expression.

The declared text is recorded, advanced and overlaid as a filter's is
(`DeclaredExpressions::keys`, one entry per key), and compared by presence
(SPEC §7.6). It is overlaid only where the read-back has the same shape, an
expression where one was declared and a column where one was. An expression
binds names as a filter does, so an index with one is never built
`CONCURRENTLY`. It is observed wherever a filtered index is, and it is split
out of a new table to be built after the modules it may call
(`Index::holds_expression`).

A declared expression index is now a table part, dropped and restored around
the rebuild of a function it calls. It was `Unrepresentable` until an index
key could hold it. One the model still cannot hold is never declared, and is
refused as unmanaged. A unique expression index is accepted and its collision
probe is unchecked, since no probe here evaluates the expression over the
planned rows. SQL Server refuses expression keys. A renamed column is not
rewritten inside an expression, as it is not inside a filter: the engine
renames what it stores, and the declaration spells the new name.

State version 11 carries expression keys and their declared texts, and still
reads 6 to 10: an older reader recorded every key as the column it was.
Plan version 15 turns 14 away. The declaration schema is published as set
21.

Pinned by `a_btree_expression_index_round_trips_and_changes_as_a_typed_plan`
(`crates/pbps-pg/tests/live.rs`) and
`a_declared_expression_index_is_dropped_and_restored_around_a_function_rebuild`
(`crates/pbps-cli/tests/flow_pg.rs`).

<a id="dec-1118-1"></a>

**DEC-1118.1. A table name that passes to another uid rebuilds what is keyed
by the name: the trigger is dropped and created, and the grants are compared
as the occupant's.** Across skipped revisions a table can be dropped, or
renamed away, while another is renamed or created into its name. The ordering
of that plan is DEC-536.1's. What is keyed by the table's *name* rather than
its uid compared equal across the two tables and emitted nothing:

- A trigger's id names its table. The doomed table's trigger went with its
  `DROP TABLE`, and the one declared on the new occupant was never created. On
  SQL Server the closing check then refused the plan (`dbo.target.audit is
  gone`). On PostgreSQL a synthesized rebuild dropped it after the table and
  was refused (`trigger "audit" for table "target" does not exist`).
- The grants under the name read as a dropped object's, whose grants nothing
  revokes. The occupant kept a permission it held under its old name and no
  longer declares.

`names_changing_hands` is the set of names whose baseline and declared uids
differ. A trigger on such a name is planned as a drop and a create, as a
trigger that changes kind is: the drop runs with the module drops, before the
table changes, and the create runs after them. For grants, a baseline grant
on a table this plan drops is left out (it goes with the table), and the name
is compared as the occupant's: its own grants arrive through the rename, the
declared ones are granted, and the rest are revoked. A name replaced by a new
table keeps its previous answer, grant everything and revoke nothing, because
the new table holds no grants of its own.

<a id="dec-1168-1"></a>

**DEC-1168.1. A column may be a stored generated column, carried as its
expression and its kind, and its expression changes in place on PostgreSQL
17 and later.** `generated: {expression, stored}` is a generation kind and
not a flag. `stored` is required, because PostgreSQL 18 reads a generation
expression with no kind as `VIRTUAL`, and 16 and 17 read it as a syntax
error. This model holds `stored: true` only. A virtual column computes on
read, and a declaration of it as stored would turn it into data. The reader
tells the two apart by `attgenerated` (`s` or `v`) rather than treating any
generation as one, and the emitter always spells `STORED`. A generated
column is never also a default. The engine refuses both together, and the
reader takes the expression out of `pg_attrdef`, where defaults live,
without also reading it back as one.

Measured on 16.15, 17.11 and 18.6:
- `SET EXPRESSION` recomputes every row, rewriting the table under `ACCESS
  EXCLUSIVE` on 17 and 18, and is a syntax error on 16. The supported change
  is therefore `AlterColumnExpression` on 17 and later. On 16 it is refused
  by name through a connected check (`generation_support`), never emitted as
  a drop and re-add that would take the column's dependents and its place.
- A change between generated and ordinary, or between kinds, has no in-place
  form and is refused by the differ (`GenerationChangeUnsupported`).
- The engine refuses to retype a column a generated column reads, and drops
  one only with `CASCADE`. The same connected check reads that dependence
  from `pg_depend`, from the expression's `pg_attrdef` row, and refuses such
  a retype or drop before anything runs, unless the plan also drops the
  generated column, or changes its expression to one that does not name the
  column (DEC-1316.1). It does not parse the expression.
- The engine also refuses a default beside a generation expression, a
  reference to another generated column, a non-immutable expression, and
  `NOT NULL` over null inputs at `ADD`. The first is refused at validation.
  The rest are the engine's, inside the transaction.

A generated column is dropped before the ordinary columns of its class,
keyed by uid so a table rename in the same plan does not hide it. It is added
after every column addition and every in-place column alteration (type,
nullability, default, expression). The engine refuses an expression over a
column not yet added, refuses to drop a column a generated column still
reads, and refuses to retype one, measured on 17.11. A generated column never reads another, so one layer each way is the
whole order. That holds in the class a column rename brings all of its
table's drops into. A recomputed value is checked against the column's
nullability as it stands, so relaxing it runs before the expression change
and tightening it after, measured on 17.11. A retype of the recomputed column
runs first, so the values are computed in the final type, and leaves the
tightening it would carry to that later step. Its expression binds the functions it calls as a default's
does. An `AlterColumnExpression` follows a function the plan creates or
rebuilds (DEC-942.1), and what validates the recomputed column moves with
its expression: a tightening, and a key over it on either side of a foreign
key. A column added with a generation expression does not move,
in an existing table or a new one, as a column added with a default does
not: a function the plan creates may read it, and measured on 17.11 the
engine resolves a SQL body's columns at `CREATE FUNCTION`, `BEGIN ATOMIC` or
not. One whose expression calls a function the same plan creates or rebuilds
is refused by the engine, and the apply rolls back.

A generated column is the engine's to fill, like a non-key identity
(`Column::engine_assigned`). It leaves `row_columns` and the row
read-back, and a row that sets one is refused. A `NOT NULL` generated add
is not counted as every stored row. Evaluating its expression before
approval would run the operator's code, so the probe reports it unchecked.
For the same reason, a probe that would read a column the plan recomputes is
reported unchecked rather than run over the old expression's values: its
tightening, a key over it, and any check or filtered index of its table. A
reference row's delete is counted against keys the catalog names at run
time, so any recomputation in the plan leaves it unchecked too.

Its declared text is recorded, advanced and overlaid as a default's is
(`DeclaredExpressions::generated`), and compared by presence (SPEC §7.6),
since the engine respells `a * 3` as `(a * 3)`. Across an apply, two
read-backs are compared outright: a planned expression change excuses a
new expression and nothing else, and a column that stops being generated or
changes kind is movement (SPEC §7.6). A column the expression
reads keeps its name in the declared text across a rename, as a filter
does. The engine rewrites what it stores, and the declaration spells the new
name. A function a generated column calls cannot be dropped or rebuilt
around it. No statement takes its expression off and puts it back:
`DROP EXPRESSION` leaves an ordinary column, and `SET EXPRESSION` is refused
on one. Such a plan is refused, naming the column, unless it also changes that
expression or drops the column or its table. The catalog's edge then has a
release, and the function's drop moves after it, and after the drops of the
modules that depend on it, which may have moved after their own releases. The release keeps the place
the differ gave it, after the additions, renames, retypes and relaxations its
new expression may need. What the moved drop leaves to the engine is a
function whose own body reads a column the plan drops or retypes before the
release. If the new expression calls the
function again, the drop is refused inside the transaction and the apply
rolls back. Telling the two apart would mean parsing the expression. A changed expression faces the gate as `narrowing` (SPEC §7.2): the
engine recomputes every stored row, and a `NOT NULL`, check or unique index
over the column can refuse what it computes.

State version 12 carries the field and still reads 6 to 11. An older
reader reported every generated column as a limitation, which no recorder
accepts. Plan version 16 turns 15 away. The declaration schema is published
as set 22. SQL Server refuses `generated:`: its computed columns are
`computed:` (DEC-1174.1).

Pinned by `a_stored_generated_column_round_trips_and_changes_its_expression`
(`crates/pbps-pg/tests/live.rs`),
`a_generated_columns_expression_changes_through_the_cli_and_its_inputs_are_held`
and `a_generated_columns_expression_change_is_refused_by_name_before_postgres_17`
(`crates/pbps-cli/tests/flow_pg.rs`).

*Amended by [DEC-1364.1](modules.md#dec-1364-1): a column added with a
generation expression that names a function the plan creates or rebuilds
follows that create, with what may read the column after it, and an
expression change moves only when its text names one.*

<a id="dec-981-2"></a>

**DEC-981.2. The SQL Server namespace walk claims a table rename's own target
when the rename runs, under the database's answer on which names are one.**
The walk that refuses a created name held by another object (#1077, #1215)
deliberately left a rename's target unclaimed, as an ordering question. The
plan's order already puts the drops that free a target first (DECISIONS 496,
DEC-536.1), so whatever still holds it when the rename runs is a collision
`sp_rename` refuses:

- two renames into names alike under the catalog collation (`dbo.Ck_Name` and
  `dbo.ck_name` on a case-insensitive database), which passed connected
  planning and failed at apply with Msg 15335;
- or an object the project does not record, such as a sequence.

The catalog read includes each rename target, so an object no other change
names is still seen. The target is checked after the table's constraints have
moved with it, since a transfer to another schema runs before `sp_rename`: a
carried check `New` holds `new` in the target schema by then. The table's own entry is not a holder, so a rename into a
case variant of its own name passes. The alike pairs are the database's (`object_names_alike`,
DEC-1243.1). A case-sensitive database reads the pair as two names, and the
plan applies there.

<a id="dec-981-3"></a>

**DEC-981.3. Ordering edges are also added for names that differ only in
case, and only where they close no cycle.** Which spellings are one name is the
target database's to say, and a plan is ordered offline (SPEC 7.3). Under a
case-insensitive collation, `DROP TABLE dbo.Target` must precede a rename to
`dbo.target`, and the column rename `b -> c` must precede `a -> B`. With exact
spellings alone, the rename could run first (Msg 15335).

The released-name graph (`rename_order`) and the column chain depth
(DEC-541.1) therefore also link names equal after lower-casing each character.
In the graph this applies on every path that releases a name: a dropped table,
a dropped constraint or index, and a rename vacating its source, as in the
table chain `b -> c` followed by `a -> B`. A move to another schema also
releases, in the source schema, the names of the indexes or constraints it
carries away, the engine's own generated default names among them
(`Dialect::generated_constraint_names`); a rename into one of them runs after
the move, under the exact spelling as under a folded one. A generated name is
one of several alternatives the table may hold, so its edges are weaker
still: added after the folded ones, and only where they close no cycle, so an
alternative the table does not hold never displaces an exact or a folded
edge. Any table rename, within its schema too, releases the generated names
of its old table name and claims those of the new one, as the SQL Server
emitter renames them. Between two generated alternatives the offline order
cannot tell which one the table holds; that is the catalog's to say, and a
connected plan asks it (#1361, DEC-1366.1).
The rule is:

- An edge found only this way is added after every exact edge, and only if it
  closes no cycle.
- A cycle closed through it is never reported: `a -> B` beside `b -> A` is a
  valid pair on a case-sensitive database.
- So a fold that is wrong for the target database costs an order the plan did
  not need, and never refuses a plan. Refusing is left to the connected check,
  which asks the database (DEC-981.2).

Accent-insensitive collations are not folded offline, since the fold is case
only. Where they make two names one, the connected check still asks the
database.

<a id="dec-1316-1"></a>

**DEC-1316.1. An expression change that stops reading a column releases it in
the same plan: the column's retype or drop runs after the change (#1316).**
A plan that moves a generated column from reading `a` to reading `b`, and
retypes or drops `a`, was refused (DEC-1168.1). That took two plans: the
expression change first, then the retype or drop. It is one plan now.

- **Which columns the old expression read** is the catalog's answer: the
  `pg_depend` edges of the column's `pg_attrdef` row (`dependences`). It is
  exact.
- **Whether the new expression still reads one** is a question of text, which
  the planner does not parse (DECISIONS 174). It is answered by
  over-approximation, with the identifier scan rename impact already uses
  (`may_read`, DECISIONS 477). A new expression that names the column, even in
  a function or field of the same name, may still read it, and the retype or
  drop stays refused. A false yes costs a second plan; a false no would cost
  an apply that rolls back. Only the first is allowed. The text speaks the
  plan's names, so it is scanned for the input's catalog name and for the name
  the plan gives it after a rename.

A retype names its column as declared, after the plan's renames. A drop names
it by the catalog's own name, so only its table is reversed; a rename into the
freed name belongs to another column.

On a connected plan, `release_generated_inputs` moves the retype or drop of a
released column to right after the expression change that releases it. How it
finds that order is DEC-1391.1's: edges that do not depend on the order,
where this entry first described a pass of positional moves. A
retype takes along the default written for its new type, matched by uid. The
old type may refuse that default: measured on 18.6, `SET DEFAULT 'abc'` on an
`integer` is `invalid input syntax`. It also takes along another generated
column's expression change whose new text may read the column, found by the
same scan. Once that column reads it, the engine refuses the retype, and the
catalog knows only the old readers.

Those two rules can cycle. A generated column may release one retyped input
and start reading another while a second column does the reverse. Each must
then follow one retype and precede the other, and no order runs: a
cycle among the edges of DEC-1391.1. It refuses there, and names the
remedy: a plan of its own that first changes
those columns to expressions reading neither input. It
runs after the module passes, which may move that change past a function's
create, and before the checks that read the order (`drop_blockers`,
`generation_support`). Both checks then see the release first:
- `generation_support` exempts a retype or drop that a release precedes;
- `drop_blockers` counts `SET EXPRESSION` as replacing the `pg_attrdef` row, so
  the old row's edges go with it. Its internal owner, the column, stays. A
  replaced row does not ask for its owner to be dropped, which a removed one
  would. Nor does it reach through the owner to what depends on it: a view over
  the generated column still stands, and is no blocker.

A change between the drop and its release that takes the dropped column's
name, an `AddColumn` or a `RenameColumn` into it, needs the name free, and the
release needs the column still there. The plan is refused by name, with the
two-plan remedy. So is a retype that would move past anything the differ puts
after the column alterations: rows, keys and constraints, modules, roles and
grants. That happens only when its release follows a function the plan
creates. The release then sits after those changes (`after_the_rebuilds`), and
any of them may need the new type: a row may write a value only that type
accepts, and a function body may be checked against it. The retype cannot both
follow its release and precede them. A drop of another input, moved after its
own release, needs nothing of this column's type and does not hold it back.

Pinned by `a_released_input_is_retyped_or_dropped_after_its_release`
(`crates/pbps-cli/src/engine.rs`), `a_replaced_internal_member_keeps_its_owner`
(`crates/pbps-pg/src/drop_impact.rs`), and the live
`an_expression_change_releases_its_old_input_in_the_same_plan`
(`crates/pbps-cli/tests/flow_pg.rs`).

<a id="dec-1363-1"></a>

**DEC-1363.1. A nullability change on SQL Server takes down and puts back what
blocks it, as a type change does (#1363).** Measured on 17.0:

- Tightening to NOT NULL is refused (5074, 4922) by an index over the column,
  whether as a key, an `INCLUDE` column or in a filter's predicate, and by a
  UNIQUE constraint on it.
- Relaxing to NULL is refused by a filtered index whose predicate names the
  column, and by nothing else.
- A CHECK, either side of a foreign key and a DEFAULT block neither direction.

`Dialect::nullability_dependents` answers per direction, and
`recreate_retyped_dependents` turns the answer into the same visible drop and
add pairs a retype gets (DECISIONS 461). These are classes 2 and 13.

A nullability change folded into an `AlterColumnType` adds its dependents to
the type change's own. They are not already covered: a `varchar` widening
keeps its index alone, and is refused once it also tightens.

A filter is opaque text, so every filtered index of the table is rebuilt, as
for a type change (DEC-1169.2).

The issue assumed relaxing needs nothing. The filtered index is the one
exception the live engine showed.

Pinned by `a_nullability_change_rebuilds_what_the_dialect_says_blocks_it`
(`crates/pbps-diff/src/schema_diff.rs`), and by
`tightening_a_column_rebuilds_what_indexes_it_through_the_cli`
(`crates/pbps-cli/tests/flow.rs`) and
`tightening_nullability_is_refused_by_what_indexes_the_column`
(`crates/pbps-mssql/tests/live.rs`).

<a id="dec-1367-1"></a>

**DEC-1367.1. A tightening runs after the plan's rows on its table, and counts
the NULLs they leave (#1367).** A revision that makes a column NOT NULL may
also be what gives its NULLs a value: an `ensure` row that sets it, or an
`exact` table that deletes the row that held it. A tightening in class 9 ran
before those rows (classes 11 and 12) and met the old NULLs. Its probe counted
the NULLs stored now, so the plan was refused on both engines.

- **Order.** A tightening of a table whose rows the plan writes or deletes
  sorts at `(12, 2)`: after the deletes, and before the additions of class 13.
  A primary key is among those additions, and SQL Server refuses one over a
  nullable column. The direction is fixed. No row change needs the column NOT
  NULL first, and a declared row that leaves the column unset is refused at
  validation. A table without row changes keeps its tightening in class 9, so
  no other plan's order moves.
- **Split.** A tightening folded into a retype comes out of it on such a
  table. The retype keeps the column nullable, and an `AlterColumnNullability`
  follows the rows. This is the split DEC-1168.1 makes for a recomputed
  column. The column then carries two changes, and the dependents both bring
  down are merged (DEC-1363.1).
- **Probe.** The NOT NULL probe on such a table reads `rows_after`, the
  relation the key probes already read (DECISIONS 335). That relation is the
  stored rows, minus the plan's deletes, with its updates and inserts applied.
  A NULL row the plan leaves alone is still counted. A value no probe can
  spell leaves the change unchecked, never passed.

Pinned by `a_tightening_runs_after_the_rows_of_its_table` and
`a_split_tightening_keeps_the_dependents_of_its_retype`
(`crates/pbps-diff/src/schema_diff.rs`), and by the live
`a_tightening_follows_the_rows_that_fill_or_remove_its_nulls`
(`crates/pbps-cli/tests/flow_pg.rs`) and
`a_tightening_follows_the_rows_that_fill_or_remove_its_nulls_through_the_cli`
(`crates/pbps-cli/tests/flow.rs`).

<a id="dec-1366-1"></a>

**DEC-1366.1. A connected SQL Server plan orders its table renames from the
catalog: where the namespace walk refuses the differ's order, the other orders
of the renames are walked, and the first one it clears is the plan (#1366).**
The differ orders renames and the drops that free their names from the
declarations alone (DEC-536.1, DEC-981.3). Some of what decides that order is
only in the target:

- a default adopted under a hand-chosen name (#1361);
- which of its generated names a default holds, the generated one or the
  fallback (DEC-981.1);
- an object the project does not record, at a target or at the name a
  transfer passes through (#1362);
- which spellings the collation reads as one name, which a case fold only
  guesses: on `Turkish_100_CI_AS`, `A` and `a` are one name and `I` and `i`
  are two.

Each guess the offline graph gained to cover one of these surfaced the next
(#1346). The namespace walk already simulates all of them from the catalog
read (DEC-981.2), but on its own it could only refuse. So connected planning
asks it for an order rather than a verdict:

- **Only table renames move**, among the drops of classes 2 and 6 and among
  one another. The drops keep their order, which already puts a foreign key's
  drop before the key or table it references. A drop claims no name, so no
  order a rename needs is one the drops would have to give up. Every other
  change keeps its place, after the renames and drops; so does every class
  DECISIONS 496 keeps (DEC-1366.2).
- **A drop on a renamed table is readdressed** to the name its table has where
  the drop now runs. The address is part of the typed, checksummed change
  (DECISIONS 496).
- **The search is depth first**, with the plan's own order tried first, so a
  plan the walk clears is left exactly as it was. A namespace state reached
  twice is walked once. A rename that can run is not always the one to run:
  a default it moves goes to its generated name when that is free and to its
  fallback otherwise, so running it early can park the default at a name a
  later rename claims. Only a whole order the walk clears is kept. After
  20,000 trials the plan is refused, saying another order may exist; the
  plans pbps meets need tens.
- **The read covers every order's claims**: the targets, each transfer's
  intermediate name, and the names a transfer carries each child to, which a
  second read asks once the first has found the children.
- **It runs last** among the passes that reorder a connected plan
  (`order_created_object_names`, after the module and generated-input passes),
  so the order it settles is the one the later checks read and the one saved.
  `apply` replays it.

An offline plan keeps the differ's order. It is a preview that is never
applied (SPEC 7.3), and its guesses stay, since without a catalog they are the
best order available. PostgreSQL is unchanged: identifiers compare exactly
there, and the names an index or key holds are declared. A column rename chain
within a table is ordered from the database's answer as well (DEC-1366.3).

Pinned by the unit tests in `crates/pbps-cli/src/object_order.rs`, and the
live `renames_only_the_catalog_can_order_apply_or_are_refused_at_plan`
(`crates/pbps-cli/tests/flow.rs`).

<a id="dec-1366-2"></a>

**DEC-1366.2. A name that only a change of a later class frees is refused at
`plan --db`, naming that change and the two-deployment remedy.** A column
rename (class 3) moves its generated default to the new column's name, so it
frees the old default name; a table rename (class 1) into that name needs the
column rename to run first. The classes DECISIONS 496 keeps do not allow that:
the column rename names its table by the name the table rename gives it.
Moving one change across the class line for this case would need its own
reasoning about every other change of both classes. The case is rare enough
not to justify that, and the plan has a direct remedy.

So the connected walk names the later change that frees a held name. Before
this, the walk said only that the name was held, and `sp_rename` would have
failed at apply otherwise. The remedy is to deploy the change that frees the
name in a plan of its own, then the rest. Two renames that each wait for the
other, such as a swap, get the same message: no order runs either first.

Pinned by `a_name_only_a_later_class_frees_is_refused_with_its_remedy` and
`a_swap_is_refused_as_two_changes_waiting_on_each_other`
(`crates/pbps-cli/src/object_order.rs`), and the live
`renames_only_the_catalog_can_order_apply_or_are_refused_at_plan`.

<a id="dec-1391-1"></a>

**DEC-1391.1. The release order of generated inputs comes from edges that do
not depend on the order, and a new reader of a dropped input is refused by
name.** DEC-1316.1 first ordered a released input's retype or drop by moving
it: to right after its release, taking along the changes found between the
two. Review found each move meeting another rule: the default for the new
type, a new reader of a retyped input, the same for a dropped one (#1391), and
two columns trading inputs, which looped until a repeat check stopped it.
Each was a fact about which generated column reads which column, decided by
where a change happened to sort (#1366).

These facts do not change with the order, so they are edges:

- each live reader's release, from `pg_depend`, comes before its input's
  retype or drop;
- a retype comes before the default written for its new type, matched by uid;
- a retype comes before any other generated column's expression change whose
  new text may read the column (`may_read`, over-approximated as DEC-1316.1
  says). The engine refuses a retype once that column reads it.

A change runs where the differ put it unless an edge holds it back. Then it
runs right after the last change it waits for, so nothing moves earlier than
the differ put it. The text speaks the plan's names, so where the plan
renames an input and gives its catalog name to another column, that name in a
new text reads the other column: it neither keeps the input unreleased nor
makes a new reader of it (review of #1424). A change held back is checked
against what it passes, as
before: a column's name taken before its drop runs, and a row, key or module
that may need the new type before its retype runs. Changes left waiting on
one another form a cycle, and the plan is refused naming them. The repeat
check is gone with the moves.

A dropped column that another generated column's new text may read has no
order. Once that column reads it, the drop is refused. Before that, its
expression names a column that is gone. So the plan is refused naming the
reader, with a two-plan remedy, unless the plan gives the name to a new
column, which the text then reads. A generated column the plan adds is such a reader
too (#1425): it reads its input from the moment it exists, and the differ
drops the input first. Only a drop looks at it, since the differ adds a
generated column after every column alteration, a retype among them
(DEC-1168.1). Before this, a reader the differ sorted
ahead of the release was passed over, and `DROP COLUMN` failed at apply. A new
reader sorted ahead of the retype it must follow is now held back too, where
the moves only looked between the retype and its release.

Pinned by `a_released_input_is_retyped_or_dropped_after_its_release`
(`crates/pbps-cli/src/engine.rs`) and the live
`a_new_reader_of_a_dropped_input_is_refused_before_anything_runs` and
`an_expression_change_releases_its_old_input_in_the_same_plan`
(`crates/pbps-cli/tests/flow_pg.rs`).

<a id="dec-1366-3"></a>

**DEC-1366.3. A connected SQL Server plan orders each table's column renames by
which names the database's collation reads as one.** The differ links a column
rename chain through names equal after lower-casing (DEC-981.3, DEC-541.1), and
keeps a folded edge only where it closes no cycle. A Rust case fold is not the
target's collation. On `Turkish_100_CI_AS`, `A` and `a` are one name and `I`
and `i` are two. So for `I -> a` beside `A -> i`, the fold links both pairs,
and the edge it keeps may be the false `I`/`i` one. `sp_rename` then fails
mid-apply with Msg 15335. Nothing connected checked column names before this.

So before the `sys.objects` walk, `plan --db` asks the database which of the
renamed columns' names are one, for each table that renames two or more of its
columns (`column_names_alike`, under `CATALOG_DEFAULT`, as
`tables_reusing_a_column_name` compares). A rename into a name then runs after
the rename that vacates that name, under the collation as under the exact
spelling. The renames keep the positions the differ gave them and only trade
them, so an order the differ already got right stays exactly as it was. The
walk runs after them, since a column rename moves a generated default
(DEC-981.1).

Renames that wait on one another under the collation have no order on that
database. `a -> B` beside `b -> A` is the case-insensitive swap. The plan is
refused naming them, with the remedy: rename one through a name nothing uses,
in a plan of its own first. On a case-sensitive database the same pair is four
names and applies. An offline plan keeps the fold, which is a preview's best
guess (SPEC 7.3).

Pinned by `the_databases_alike_column_names_order_a_rename_chain` and
`column_renames_trading_names_under_the_collation_are_refused`
(`crates/pbps-cli/src/object_order.rs`), and the Turkish column rounds of the
live `renames_only_the_catalog_can_order_apply_or_are_refused_at_plan`.

<a id="dec-1174-1"></a>

**DEC-1174.1. A SQL Server computed column is a part of its table, not a
column: declared under `computed:`, changed by a drop and an add, and
refused by name where a change would need it out of the way (#1174).**

Measured on 17.0.4075.5:

- The engine infers a computed column's type and nullability:
  `CONCAT(b,'-',a)` reads back as `varchar(23)` NOT NULL.
- `ALTER COLUMN … AS` is a syntax error (156). `ADD|DROP PERSISTED` alone
  changes in place.
- A computed column cannot be written (271).
- While it stands, the engine refuses each of these:

  | Change | Error |
  |---|---|
  | dropping it while an index or check is over it | 4922 |
  | renaming a column it reads | 15336 |
  | dropping, retyping, recollating or changing the nullability of a column it reads | 4922 |
  | `ALTER FUNCTION` on a function it calls, even without SCHEMABINDING | 3729 |

**The model.** `Table::computed` maps a name to `{expression, persisted,
not_null}`. It is beside `columns`, so neither can hold what only the other
has:

- A computed column has no type to declare, which would be a second source
  of truth for what the engine infers.
- It is never one of `row_columns`, so no reference row can write it.

PostgreSQL's generated columns (DEC-1168.1) declare their type and keep
`generated:`; PostgreSQL refuses `computed:`. Leon chose this shape over an
optional `type` on `Column` and over a required one compared with the
read-back.

**NOT NULL.** `not_null` is held only where it is declared. The engine
reports `is_nullable = 0` for a declared `PERSISTED NOT NULL` and for an
expression that is never NULL alike, so a read-back holds `not_null` wherever
a persisted column is not nullable. A declaration matches a read-back when
the expression and persistence are equal and the read-back is NOT NULL
wherever the declaration says so (`ComputedColumn::declares`). Re-declaring
NOT NULL on a never-NULL expression is harmless, and dropping the flag from
a declaration does not remove a constraint the engine holds.

**Changes.** `AddComputedColumn` and `DropComputedColumn` are the only
changes. An expression or persistence change is both, so the column moves to
the end of its table, which schema equality already ignores. With no uid, a
renamed computed column is the same pair under two names. In the ordering,
both act as a part does, an index's place in the drift check included:

- The drop is at (2, 4), after the index, unique and check drops of class 2
  and before every rename, drop or retype of what it reads.
- The add is at (9, 3), after the columns it reads reach their final type and
  before the class-13 additions over it.
- The indexes, uniques, checks and filtered indexes over a computed column
  that is dropped and re-added are rebuilt around it, through
  `recreate_retyped_dependents`, as around a retype (DECISIONS 461). So are
  those over a name that passes between an ordinary and a computed column in
  either direction: a column drop and a computed add, or a computed drop and
  a column add.
- In the connected scheduler of DEC-1366.1, its drop is a drop on a table, as
  an index's is. It is addressed by the table's name at the point where it
  runs.
- A module drop of a function a computed column calls, where the plan
  removes that computed column, runs after the removal. The connected plan
  moves it by the catalog's own edges (DEC-1431.1); the differ moves nothing
  for it.

The drop is `destructive`, as an index drop is: the values are derived, but
the object is gone. The add carries no risk. A persisted add is unchecked in
the pre-flight, and a key probe over a computed column the plan adds is
unchecked, not counted over the values the old expression stored.

**Refused by name, offline** (`DiffError::ComputedInputChanged`,
`ComputedFunctionChanged`). A standing computed column is one that is there
before the plan and that the plan neither drops nor adds. A change is both.
One the plan adds, new or again, comes at (9, 3), after every input change.

- A plan is refused that renames, drops, retypes or changes the nullability
  of a column a standing computed column may read (#1420).
- So is a plan that alters or drops a module a computed column may call,
  where the column stands or is added again in the same plan: either way it
  calls the module when the module changes (#1421). Only one the plan drops
  for good is out of the way first, with the module's drop moved after it.
- A plan that drops a computed column under a schema-bound module that
  reads it (4922) is refused by the connected plan, by
  `sys.sql_modules.is_schema_bound` (DEC-1431.1). The word in a module's
  text is no proof of the clause (#1439).
- So is a computed column that may call a module the same plan creates,
  whether by `ADD` or inside a `CREATE TABLE` (#1421). The module is created
  in class 14, after the table (7) and the column (9).

"May" is `Dialect::may_name`, the over-approximation of DEC-1316.1 applied to
code only and ignoring case: under a case-insensitive collation `A2` reads
`a2`, and the engine stores `[a2]`. Only a function module is matched against
an expression, for a move and for a refusal alike: a computed column calls
nothing else, so a view or procedure that shares a name the expression uses is
never what it calls. It is matched by its schema-qualified name
(`Dialect::may_name_qualified`), because SQL Server calls a scalar function
only by a two-part name, so `x.f` is not taken for `dbo.f`. Ordering by text
is the class DEC-1431.1 replaces with the catalog's own edges on a connected
plan; the offline refusals stay as a screen. It looks for a delimited name's escaped
spelling too: `a]b` is stored as `[a]]b]`. Its case fold can only make more
names equal. `İ` folds to `i` without the combining dot, and `ı` to `i`,
because a Turkish collation binds `[i]` to `İ`. Accent-, width- and
kana-insensitive equivalences are a collation's, which no textual fold
closes. The connected plan answers them with the catalog's own edges
(DEC-1431.1, #1426). A key over a computed column is
refused by validation (#1419).

**Drift.** The drift check compares computed columns as named parts, and holds a
plan's add to its persistence and to a declared NOT NULL. That includes the
computed columns inside a `CREATE TABLE`, which no separate change carries. One
another session adds or removes after the create is movement (SPEC §7.6).

**Pull.** `sys.computed_columns` gives the definition, unwrapped as a check's
is, and `is_persisted`. A definition the reader may not see stays a
limitation by name, never an ordinary column. Expressions are tracked in
`DeclaredExpressions::computed`, so the engine's respelling is no change.

Pinned by:

- `a_computed_column_change_is_a_drop_and_an_add_around_its_index` and
  `a_standing_computed_column_refuses_a_change_to_what_it_reads`
  (`crates/pbps-diff/src/schema_diff.rs`);
- `a_computed_column_is_read_into_its_own_section`,
  `a_computed_column_is_validated_as_its_own_kind` and
  `a_computed_column_is_created_added_and_dropped_as_declared`
  (`crates/pbps-mssql`);
- `may_name_reads_code_not_literals_and_ignores_case` (`crates/pbps-dialect`);
- the live `computed_columns_round_trip_and_change_through_the_cli`
  (`crates/pbps-cli/tests/flow.rs`).

<a id="dec-1431-1"></a>

**DEC-1431.1. A connected SQL Server plan orders and refuses computed columns
by the catalog's own expression edges, not by text (#1431; absorbs #1426,
#1432, #1437, #1439).**

**Context.** DEC-1174.1 decided offline, by text, which column a computed
column reads and which function it calls. A text match answers by spelling,
and #1423's review found it wrong in one shape after another: `dbo.f` for
`x.f`, a view sharing a column's name, `[cafe]` binding `café` under an
accent-insensitive collation, `f]x` stored as `[f]]x]`, and the word
`schemabinding` taken for the clause. Each was patched, and each patch had a
next case.

**Decision.** A connected SQL Server plan reads
`sys.sql_expression_dependencies` for the tables and modules it touches
(`pbps_mssql::catalog::expression_edges`). Each edge is the engine's own
binding, by object id: computed column to column, computed column to
function, and module to what it is bound to, with
`sys.sql_modules.is_schema_bound`. From those edges, in
`order_computed_by_edges`, after `release_generated_inputs` and before the
rename walk of DEC-1366.1:

- A function's drop moves to right after the last change that removes a
  computed column calling it: the column's drop, or its table's (3729
  otherwise). A drop of something a moved drop is schema-bound to, a
  function or a table, moves after it (#1432).
- Refused by name: a rename, drop, retype or nullability change of a column a
  standing computed column reads (15336, 4922); an alter or drop of a
  function a standing computed column calls (3729); a computed column's drop
  under a schema-bound module the plan leaves standing (4922). A column the
  plan drops and adds again is not standing: its edge is the old
  expression's, which is gone before the function changes. What the new
  expression calls has no edge yet, and the differ's screen reads it.

The pass matches the plan's names to the edges' under the catalog's own
collation (`column_names_alike`), not a case fold: a case-sensitive database
keeps `A` and `a` apart, and an accent-insensitive one joins `cafe` and
`café`.

The differ keeps its over-approximating refusals as an offline screen, where
a false yes costs a second plan. It no longer moves a function's drop, and it
no longer refuses on the word `schemabinding`. An offline plan is never
applied (SPEC §7.3), so its order is not the one that runs.

**Why not keep patching the text match.** The cases are as many as a
collation's equivalences and the forms a name can take. The engine already
resolved each name when it stored the text, so asking it closes the class.

**Why no second check at apply.** The edges follow from tables, computed
columns and module text, which the plan's drift check already holds the
database to (SPEC §7.6). A database where they changed is drift.

**Measured** on SQL Server 17.0: every edge from a computed column to a column
or function names the column exactly (`café`, not `[cafe]`) and the function
with its schema, with `is_schema_bound_reference = 1`; a module's edges name
tables, functions and columns it reads.

**Tests.** `a_function_drop_follows_the_removal_of_what_calls_it`,
`what_a_released_function_is_bound_to_follows_it`,
`the_edges_refuse_what_the_engine_would` (`crates/pbps-cli`); the live
`computed_function_drops_follow_the_catalogs_edges` and
`the_catalogs_edges_refuse_what_the_engine_would`
(`crates/pbps-cli/tests/flow.rs`).

<a id="dec-1444-1"></a>

**DEC-1444.1. A PostgreSQL table declares its replica identity under
`replica_identity:`, and a plan sets it where it changes and wherever the
plan re-adds the index it names (#1444).**

Measured on 16.15 and 18.6, alike:

- `pg_class.relreplident` is `d`, `f`, `n` or `i`; for `i`,
  `pg_index.indisreplident` marks the index, which may be a primary key's.
- `USING INDEX` refuses a partial index and one over a nullable column.
- `DROP INDEX` of the identity's index succeeds, and leaves `relreplident =
  'i'` with no index marked: the table identifies no row. A rebuilt index
  (drop and create) is therefore not the identity until it is set again. A
  retype that rebuilds the index in place keeps it.

**Model.** `Table::replica_identity`, `None` for the default: `full`,
`nothing`, `primary_key`, `{unique: <name>}` or `{index: <name>}`. One
selector naming the owner of the index by kind, as `clustered` does
(DEC-1178.1). `primary_key` is held apart from the default because the
catalog holds it apart, although it identifies the same rows while the key
stands. Validation refuses an identity on nothing, on a non-unique, partial or
expression index, and on nullable columns. SQL Server refuses the key.

**Plan.** `SetReplicaIdentity { uid, table, to }`, planned where the declared
identity differs from the database's, and wherever the plan adds the object
whose index it names, new or rebuilt by any pass of the differ. Two places:

- First, at (0, 1) and under the table's old name, where the table can take
  the target as it stands: every target the plan does not add, when the
  columns it is over are NOT NULL already. That is before the drops of class
  2, so an identity moved off an index the plan drops never leaves the table
  identifying no row.
- Otherwise last in class 13, after the index it names and after the NOT NULL
  of class 9. Where the plan drops the old identity's index too — a rebuild,
  or a move to a new index — `FULL` is set first, at (0, 1): every table can
  take it, so no read between the drop and the final setting finds the table
  identifying no row. The reader leaves such a table out as unreadable, so a
  staged checkpoint there could not be resumed (#1467 review).

A created table sets it as its `CREATE`'s last statement, after its indexes;
where the resolver splits the indexes out of the `CREATE`, in the plan and in
its scratch reconstruction, an identity on one of them is split out after
them, and the created table is held to that split setting (#1467 review). The resolver's rule ordering an index
against its table's other changes would put the identity before every add
and after every drop, so the identity is exempt from it and keeps the
differ's order.

The primary key's index is found by a `DO` block when the statement runs, as
an unnamed key's drop is: a pulled key carries no name, and the server names
an unnamed one `_pkey` or `_pkey1`.

**No risk class.** The identity decides what logical replication carries to
name an old row, which is neither a row nor an access. `nothing` on a
published table makes later replicated `UPDATE`s and `DELETE`s fail, which is
the publication's to manage, and pbps does not manage publications.

**Read.** A table is held whatever its identity, except one whose `USING
INDEX` names no index: that is not `nothing`, which someone chose, and no
declaration spells it, so the table is left out and named. An identity on an
index the pull leaves out is named too, and never read as the default. The
index's owner is a primary key or unique constraint only: a foreign key's
`conindid` is the index it references, which on a self-referencing table is
the table's own key (#1467 review).

**Drift.** A touched table's identity that changes across an apply is
movement unless the plan sets it. Where it does, any value mid-run is the plan
in progress, and once the run is whole it must be the last value the plan
sets: another session's after the plan's is movement (#1467 review). That
value is found by the table's uid, not by a name a setting ran under, which
two tables share in a handoff (`a` to `b` while `b` goes to `c`). A created
table is held to its declared identity once it shows one or the run is whole.

Tests:

- `a_replica_identity_must_name_an_index_postgres_takes` (`crates/pbps-model`);
- `every_replica_identity_round_trips`, `an_unknown_replica_identity_is_rejected`
  (`crates/pbps-load`);
- `a_replica_identity_is_set_before_its_old_index_goes_and_after_its_new_one_comes`,
  `splitting_table_creation_sets_an_index_identity_after_its_index`,
  `a_replica_identity_keeps_its_order_against_its_tables_indexes`
  (`crates/pbps-diff`);
- `every_replica_identity_spells_its_statement`,
  `a_replica_identity_is_read_as_the_object_owning_its_index` (`crates/pbps-pg`);
- `a_replica_identity_is_refused_on_sql_server` (`crates/pbps-mssql`);
- `a_replica_identity_moved_by_someone_else_is_movement` (`crates/pbps-cli`);
- the live `every_replica_identity_round_trips_and_moves_as_a_typed_plan`
  (`crates/pbps-pg/tests/live.rs`, on 16 and 18) and
  `a_replica_identity_moves_to_a_new_index_through_the_cli`
  (`crates/pbps-cli/tests/flow_pg.rs`).

<a id="dec-1441-1"></a>

**DEC-1441.1. A PostgreSQL table declares its heap storage parameters under
`storage_parameters:`, from a closed list, each held in one canonical
spelling (#1441).**

Measured on 16.15 and 18.6, alike:

- `pg_class.reloptions` keeps the spelling it was given, and the engine
  parses it with C's rules: booleans by `parse_bool` (`of` is false, `o` is
  refused), integers by `strtol(…, 0)` (`070` is octal 56, `0x14` is 20,
  `08` is refused) and, where that stops at `.` or `e`, again as a real
  rounded half to even (`70.5` and `7e1` are 70), reals by `strtod`, which
  refuses an underflow (`1e-320`) rather than reading 0, and
  `vacuum_index_cleanup` takes `auto` or a whole boolean word in any case.
- `user_catalog_table`, set or reset, takes `AccessExclusiveLock`; every
  other listed parameter takes `ShareUpdateExclusiveLock`.
- Out-of-range values and unknown names are refused by the engine.
- `toast.*` parameters live on the TOAST relation's `reloptions`, without
  the prefix, and a table with no TOAST relation discards them silently.

**Model.** `Table::storage_parameters`, a map from a closed list of heap
parameters (`pbps_model::storage::TABLE_PARAMETERS`) to each value's
canonical spelling: `true`/`false`, a decimal integer, the shortest decimal
of a real, `auto`/`on`/`off`. Both sides are put in it by the engine's own
rules (`storage::canonical`): the loader for the declaration, the reader for
`reloptions`. So a respelling is no change, and text is never compared. Two
parameters are 18's; a 16 server refuses them when the plan runs, inside its
transaction. SQL Server refuses the key.

**Plan.** A created table takes them in its `CREATE` (`WITH (…)`). A change
is one `SetStorageParameters`, one `ALTER TABLE … SET (…), RESET (…)`, with
only what differs; a parameter left alone is not restated. One statement, so
a staged read never finds half of it. Class 10, with the metadata: no
statement reads one, and it runs under the table's final name. No risk
class: none rewrites the table. Its cost estimate rebuilds and reads
nothing, and names the strongest lock among its parameters (#1477 review).

**Read.** What the model cannot declare is named, never dropped: a name
outside the list, a value the engine's rules read but this reader cannot
spell (a hexadecimal real), and any `toast.*` parameter. Each is a
limitation of its table, which keeps the table out of every command, so no
plan overwrites what it did not read.

**Why not `toast.*`.** A declared one on a table without a TOAST relation
would be discarded by the engine and planned again by every run. Holding it
needs a check that the table has one, which is follow-up scope.

**Drift.** A touched table's parameters that change across an apply are
movement unless the plan sets them; where it does, the before-read with the
plan's `set` and `reset` applied is held once the run is whole, by the
table's uid, as the replica identity is (DEC-1444.1). A created table is held
to its `CREATE` from the first read that finds it.

Tests:

- `every_spelling_the_engine_reads_alike_is_one_value`,
  `a_name_or_value_the_engine_would_not_take_is_refused`,
  `a_storage_parameter_must_be_listed_and_canonical` (`crates/pbps-model`);
- `storage_parameters_read_back_canonical_and_round_trip` (`crates/pbps-load`);
- `storage_parameters_set_what_differs_and_reset_what_goes` (`crates/pbps-diff`);
- `storage_parameters_are_one_statement_and_part_of_create`,
  `storage_parameters_are_read_canonical_and_the_rest_is_named` (`crates/pbps-pg`);
- `storage_parameters_are_refused_on_sql_server` (`crates/pbps-mssql`);
- `storage_parameters_moved_by_someone_else_are_movement` (`crates/pbps-cli`);
- the live `storage_parameters_round_trip_and_change_as_a_typed_plan`
  (`crates/pbps-pg/tests/live.rs`, on 16 and 18) and
  `storage_parameters_change_through_the_cli`
  (`crates/pbps-cli/tests/flow_pg.rs`).
