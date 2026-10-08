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

*Amended by [DEC-1461.1](#dec-1461-1) and [DEC-1680.1](#dec-1680-1): a
module drop the computed edges placed among the drops moves only between what
those edges order it after and before.*
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

*Amended by [DEC-1460.1](#dec-1460-1): on a connected plan the catalog's
edges, not this entry's text screen, judge a computed column that stands
throughout.*

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

*Amended by [DEC-1462.1](#dec-1462-1): the edges are read as complete only
once no referrer of what the plan changes is hidden from the login.*

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

<a id="dec-1442-1"></a>

**DEC-1442.1. A PostgreSQL index, primary key and unique constraint declare
their index's storage parameters, from a per-method list, and a change to
them alone is made in place (#1442).**

Measured on 16.15 and 18.6, alike:

- A B-tree index takes `fillfactor` and `deduplicate_items`; a GIN index
  `fastupdate` and `gin_pending_list_limit`; each refuses the other's. A key
  and a unique constraint take theirs in the constraint clause (`… PRIMARY
  KEY (id) WITH (fillfactor = 70)`).
- `reloptions` keeps the spelling given, parsed by the engine's rules
  (DEC-1441.1).
- **Every one changes in place.** `ALTER INDEX … SET (…), RESET (…)` leaves
  `relfilenode` unchanged, on a key's and a unique constraint's index too. No
  supported parameter needs a rebuild.
- The lock is on the index alone: `ShareUpdateExclusiveLock` for a B-tree's,
  `AccessExclusiveLock` for a GIN index's, which holds every write to the
  table, since each writes the index.

**Model.** `storage_parameters` on `Index`, `PrimaryKey` and
`UniqueConstraint`, canonical as a table's are, by the index's method
(`storage::canonical_index`); a key's and a unique constraint's index is a
B-tree. YAML: in an index's block; in a key's mapping form, beside an
optional `name:`; and in a unique constraint's mapping form, `{columns: […],
storage_parameters: {…}}`, its column list staying the plain form. Those two
forms are read by hand rather than as an untagged enum, which would read a
column named `n`, `y` or `on` as a boolean.

**Plan.** A part is compared without its parameters to decide a rebuild; a
difference in them alone is one `SetIndexStorageParameters`, `ALTER INDEX …
SET (…), RESET (…)`, in class 10, with an estimate that reads nothing and
names the method's lock. The key's index is found by a `DO` block when it
runs, as its drop is, since a declared key need not be named. A part rebuilt
for its definition carries its declared parameters in its `CREATE`, so none
is lost on recreate, and a parameter change for a part the plan rebuilds is
dropped, whichever pass rebuilt it: the differ's, or a rebuild woven around a
function (#1483 review).

**Read.** From each index's `reloptions`, the key's and a unique
constraint's through `conindid`. A name outside its method's list, or a value
this reader cannot spell, is a named limitation. An index of a method the
model does not hold (GiST's `buffering`) is one already.

**Drift.** A planned change excuses its part, as an index change does, and
the whole part is held once the run is whole to the before-read with only
the parameter change applied: dropped, or recreated under its name with
another definition, after the plan's `ALTER INDEX`, it is movement. Mid-run
it is held to either side of that statement, so no staged checkpoint records
another definition for the next read to compare against (#1483 review). An
index, key or unique constraint the plan adds, alone or in a `CREATE`, is
held to its declared parameters too, as to the rest of its definition.

Tests: `an_index_takes_its_own_methods_parameters`,
`an_index_parameter_must_be_its_methods` (`crates/pbps-model`);
`index_storage_parameters_round_trip_in_every_form`,
`a_boolean_looking_column_is_a_column_in_every_key_form` (`crates/pbps-load`);
`index_parameters_change_in_place_and_ride_a_rebuild` (`crates/pbps-diff`);
`index_storage_parameters_are_in_create_and_alter_index`,
`index_storage_parameters_are_read_by_the_indexs_method`,
`an_index_parameter_change_takes_its_methods_lock` (`crates/pbps-pg`);
`storage_parameters_are_refused_on_sql_server` (`crates/pbps-mssql`);
`index_storage_parameters_the_plan_sets_are_held_at_the_close`
(`crates/pbps-cli`); the live
`index_storage_parameters_round_trip_and_change_in_place` (on 16 and 18) and
`index_storage_parameters_change_in_place_through_the_cli`.

<a id="dec-1443-1"></a>

**DEC-1443.1. A PostgreSQL table declares `unlogged: true`, and a switch
between permanent and unlogged is one risk-classified change ordered by the
foreign keys between the tables it switches (#1443).**

Measured on 16.15 and 18.6, alike:

- `CREATE UNLOGGED TABLE` makes the table, its indexes and the sequence behind
  an identity or `serial` column unlogged; a switch carries the sequence too.
- `ALTER TABLE … SET LOGGED` and `SET UNLOGGED` rewrite the table and its
  indexes (a new `relfilenode`) under `AccessExclusiveLock`, either way.
- A permanent table may not reference an unlogged one; an unlogged one may
  reference a permanent one, and a self-reference is fine. So, switched
  together, the referencing table goes unlogged first and the referenced one
  logged first; the other way round is refused (`could not change table "pa"
  to unlogged because it references logged table "ch"`).

**Model.** `Table::unlogged`, `false` for a permanent table. A permanent table
declaring a foreign key to an unlogged one is refused by name
(`DiffError::PermanentReferencesUnlogged`). SQL Server refuses the key.

**Plan.** A created table is `CREATE UNLOGGED TABLE`. A switch is one
`SetTablePersistence`, `ALTER TABLE … SET LOGGED/UNLOGGED`, in class 9: after
the foreign-key drops of class 2 and before the adds of 13, so only the keys
that stand are checked. Inside the class it is ordered by each table's depth
in the declared foreign-key graph, ascending to logged and descending to
unlogged. Tables that reference each other in a cycle have no such order,
since whichever switches first breaks a key of the other, so the keys inside
the cycle are dropped before the switches and added back after them; a valid
declaration gives a cycle one persistence, so its tables switch together
(#1488 review). Its estimate is a rewrite that reads every row under
`AccessExclusiveLock`.

**Risk.** To unlogged is `destructive`: from then on a crash or an unclean
shutdown empties the table, and a standby never has its rows. To logged
carries none.

**Read.** `relpersistence = 'u'` is held; a temporary table stays a
limitation, and the ledger's own tables stay refused when unlogged (#836).

**Drift.** A touched table's persistence is held to the before-read unless the
plan switches it, and then to the plan's value once the run is whole, by uid;
a created table to its `CREATE`.

Tests: `an_unlogged_table_round_trips_and_permanence_writes_nothing`
(`crates/pbps-load`); `persistence_switches_follow_the_foreign_keys`
(`crates/pbps-diff`); `persistence_is_created_and_switched`,
`a_persistence_switch_is_a_rewrite_under_access_exclusive`,
`an_unlogged_table_is_read_as_unlogged` (`crates/pbps-pg`);
`storage_parameters_are_refused_on_sql_server` (`crates/pbps-mssql`);
`a_persistence_switch_by_someone_else_is_movement` (`crates/pbps-cli`); the
live `unlogged_tables_round_trip_and_switch_in_foreign_key_order` (on 16 and
18) and `an_unlogged_table_switches_through_the_cli_behind_its_risk`.

<a id="dec-1176-1"></a>

**DEC-1176.1. A SQL Server system-versioned table and its history are one
table of the model, declared under `system_time:`. Only the history layout
the engine builds is read, and until #1177 every change to such a table is
refused (#1176).**

Measured on 17.0.4075.5, Developer and Express editions:

- `CREATE TABLE … WITH (SYSTEM_VERSIONING = ON (HISTORY_TABLE = h))` builds
  `h` with one clustered, non-unique index `ix_<h>` on `(end, start)`. It is
  compressed PAGE on Developer and NONE on Express, which does take PAGE when
  asked.
- Without `HISTORY_TABLE`, the engine names the history
  `MSSQL_TemporalHistoryFor_<object_id>`, a name a recreated table does not
  get.
- `SET (SYSTEM_VERSIONING = OFF)` keeps the period, and the history becomes an
  ordinary table.
- Given an existing table whose columns match, `HISTORY_TABLE` adopts it as
  the history, with its rows and its layout, instead of refusing.
- A history table takes a default constraint and further indexes. It refuses
  a CHECK constraint (13564) and a trigger (13569).
- A period column must be `datetime2` (13501). It may have a default.
- A system-versioned table must have a primary key (13553); a period alone
  needs none.

**The engine's temporal limitations, audited once against 17.0** (#1501
review):

| Limitation | On 17.0 | pbps |
|---|---|---|
| No primary key on a versioned table | refused (13553) | validation refuses it |
| Period columns of two precisions (an omitted one is 7) | refused (13513) | validation refuses it |
| A history name over 124 characters | its `ix_` index name is cut at 127 | validation refuses it |
| An INSTEAD OF trigger on a versioned table | refused (13569) | validation refuses it |
| A foreign key to a history table (which takes no key: 13558, 13583, 13741) | refused (13565) | validation refuses it (#1513); one to a spelling the database's collation reads as the history is refused when connected (#1625, DEC-1243.1) |
| An AFTER trigger on it, or either kind with a period alone | accepted | held |
| A cascading foreign key from or to a versioned table | accepted | held; a key from one is refused when connected to 2016, which refuses it (#1502) |
| A finite history retention | accepted (2017 added it) | held; refused when connected to 2016, which has none (#1502) |
| A computed, identity, `xml`, `(max)` or `ntext` column | accepted | held as for any table |
| A sparse or FILESTREAM column | refused (11418) | outside the model |
| A history table's constraints, triggers or own layout | see above | the pair is left out |
| TRUNCATE, DROP, and changes to the period or the history | refused or rewrites history | every change refused until #1177 |
- History is kept for at most 1000 years in any unit (365242 days, 52177
  weeks, 12000 months, 1000 years; 13749 beyond, found by bisection), so
  validation refuses a longer retention.

**The model.** Leon chose all three design points on the issue.

- `Table::system_time` holds `start` and `end`, each naming a column in
  `columns:`, which keeps its type, nullability and place. `hidden` covers both
  period columns. `versioning: {history, retention}` is absent for a period
  alone, which is a state the engine has, so a pair read with versioning off
  shows as a difference rather than as a missing table.
- The history has no uid. It is a property of its table. Its name is always
  written out, the engine's own choice included (with an onboarding notice),
  so a rebuild keeps it.
- No row writes a period column, so `data:` on such a table is refused.
- PostgreSQL refuses `system_time`.

**The reader holds the engine's history layout only.** It compares the same
fields it reads for an ordinary index:

- exactly one index, named `ix_<history>`;
- clustered, non-unique, unfiltered, enabled and unpartitioned;
- keys `(end ASC, start ASC)` and nothing included;
- no key, check or foreign-key constraint, and no default, identity or
  computed column;
- columns that mirror the table's, in order.

Anything else leaves the pair out, both tables named. Compression is not held,
and PAGE and NONE both count as the default. The edition decides between them,
and an ordinary table's compression is not read either. ROW, columnstore, or
a mix across partitions stays a limitation.

**The reader fails closed.** Each of these is a limitation, never an ordinary
table or a default:

- a history it cannot see;
- a retention it cannot read;
- a `generated_always_type` other than a period's 1 and 2;
- a hidden column outside the period;
- one period column hidden and the other not.

A module bound to the history stays out with it, and a grant on the history
is reported as one on an object outside the model.

**The history's name** takes a place in the schema's namespace although no
declaration lists it:

- validation refuses another history or any object the declaration puts in
  that namespace: a table, a module, or a named constraint. A generated
  default's names are not reserved, because `CREATE TABLE` takes only the first
  and only a later rename the fallback. A default already at either is the
  connected walk's, and the plan's own creation fails safely in its
  transaction;
- `plan --db` reads it and refuses an occupant, through the `sys.objects`
  walk;
- `bootstrap` refuses an occupant before anything runs, since the engine
  would adopt a matching one;
- `apply` asks the same under the lock, before anything runs, for a table made
  there since the plan was computed. A staged run asks when it starts, not on
  a resume, whose own earlier statement created the history;
- validation refuses the ledger's own table names for it, as for a table;
- `doctor` asks for its schema, and bootstrap holds that schema's spelling to
  the database's, as it does a table's.

Every place that lists what a declaration puts in the database was audited
for the history once (#1501 review). The places above are the ones that need
it. The ids file, the managed scope and declared grants do not: the history
has no identity, and pbps declares no grant on it.

**Creation never adopts.** `HISTORY_TABLE = <name>` would take an existing
table that matches. No check before the statement can close the window in
which another session makes one, and the creation timestamps cannot tell the
two apart: in a tight loop, 169 of 200 fresh histories had their table's
`create_date`, which is a `datetime`. So pbps does not use `HISTORY_TABLE`.
It creates the table with versioning on and lets the engine name the history
`MSSQL_TemporalHistoryFor_<object_id>`, a name that belongs to the new table
and that nothing can hold in the table's own schema. In the same batch it then
renames the history and its `ix_` index to the declared names. A history
declared in another schema could meet an object of exactly the engine's name
there, so it is first renamed to a name made from a fresh GUID, then moved,
then renamed (#1512). The engine accepts the renames and the transfer while
versioning is on, in one transaction. A
declared name that is taken fails the rename (15335), and the plan rolls back
with it. The `CREATE` and the renames are one transaction of their own, which
nests in a transactional apply, so a staged apply, whose statements commit one
by one, cannot leave the table behind without its checkpoint. The checks before the statements still give a taken name a refusal
that names it. The engine cuts the index name at 127 characters, so a history
name longer than 124 characters is refused.

**Changes.** Creating the table is one statement, so the engine builds the
history. Every other change to a table with `system_time` on either side is
refused by name until #1177, which admits the first one (DEC-1177.1). This covers:

- a difference in `system_time` itself, which no change carries and which
  would otherwise plan nothing;
- dropping the table, which the engine refuses (13552);
- renaming it;
- every column, constraint and index change. Some succeed with history side
  effects: DROP COLUMN deletes the column's history, and ADD NOT NULL with a
  default writes into every history row.

A grant or a trigger on the table, and a change to another table referencing
it, are not changes to the pair.

<a id="dec-1177-1"></a>

**DEC-1177.1. The one change admitted to a table with `system_time` is adding
a nullable column; every other change is still refused, and the refusal names
each one (#1177).**

Measured on 17.0.4075.5, on a populated versioned table:

- `ALTER TABLE … ADD <column> NULL`, with or without a default, adds the column
  to the history as well, in place. Both sides are nullable, and every
  existing row of both reads NULL.
- The history gains no default constraint, versioning stays on, and the
  retention is unchanged.
- Every history row keeps its values and its period. A schema-bound view over
  the table is undisturbed.
- No toggle of versioning is involved, so none of the OFF/ON choreography the
  feasibility review measured is admitted.
- The `ADD` needs ALTER on the table's schema and on the history's, not CONTROL
  on either table: a login holding no CONTROL adds the column to both, and one
  without ALTER on the history's schema is refused (1088). `doctor` asks for
  ALTER on the history's schema, as a managed one (DEC-1176.1).

That is the whole of what is admitted: an `AddColumn` whose column is nullable,
on a table that keeps its `system_time`. A period column is declared through
`system_time`, whose difference stays refused. Adding a NOT NULL column stays
refused, because with a default it writes that default into every history row.
So does every other kind of change, each named in the refusal by its kind.

What makes the admitted change safe is not new. The apply refuses a saved plan
whose recorded pair has moved since it was computed, before any DDL. The
closing read holds the table's `system_time` to the before-read, and reads the
pair only while the history still mirrors the table, so a column the engine
did not add to the history leaves the pair a limitation that refuses the
recording. The estimate stays unmeasured for a temporal table, as it is.

<a id="dec-687-1"></a>

**DEC-687.1. A routine's `PUBLIC` decision sorts in the module class at that
routine's own rank, so it is the statement after its `CREATE`.**
`Change::PublicExecution` used to share class 16 with the grants, two classes
after the `CreateModule` (14) it belongs to. Inside a transactional apply,
nobody outside sees that gap. In the script `bootstrap --sql` and `plan --sql`
render, each statement is its own autocommit (DECISIONS 259), so a routine
created at 14 held the engine's default `EXECUTE` for `PUBLIC` until the
revoke at 16 ran, after every remaining module create and every role create.
For a `SECURITY DEFINER` routine, that is the exposure #318 closed, reopened
for the length of the script (#687).

The issue proposed a class of its own after the modules. That would still
leave a later routine's `CREATE` between a routine and its revoke, and it
would renumber every ordinal below it together with the prose that quotes
them. Instead, `order_key` puts `PublicExecution` in class 14, and
`dependency_rank` gives it the create rank of the routine it settles. Its
subject is already the routine's name, so the sort's subject tiebreak places
it next to that routine's `CREATE` or `ALTER`, and the change's rendering puts
it after. No other class moves. Wrapping the script in a transaction stays
refused for DECISIONS 259's reasons, as does refusing `--sql` for routines.
The connected planner's PostgreSQL passes reorder modules after the sort.
`weave` puts dependents around a drop, and `after_the_rebuilds` and
`after_their_functions` place additions after the functions they call. Those
passes chain the changes they do not move in plan order. A decision sitting
between two creates was such a change, and it tied one routine's create to
another routine's revoke, a cycle the passes then refused as a plan no order
performs. `account_for_module_dependents` therefore sets every decision aside
(`take_public_execution`) before the passes and puts each back after its
routine's last `CREATE` or `ALTER` (`settle_public_execution`) once the order
is final. A decision depends on nothing but its routine.

The unit test `each_routine_is_closed_to_public_in_the_statement_after_its_create`
creates three routines, one of which calls another, and requires each decision
to follow its own `CREATE` directly. Putting the decision back at class 16
fails it, and so does ranking it at 0. The pass-level behaviour is pinned by
`public_decisions_return_beside_their_routines_final_create`, and the live
`flow_pg` fixtures with functions created around new columns fail without it.

<a id="dec-1170-1"></a>

**DEC-1170.1. A PostgreSQL RANGE-partitioned table declares its key and its
partitions in its own file; each partition is a table of the model holding
only its parent and bound. A tree is read whole or not at all, and until
#1171 it is created whole and never changed (#1170).**

Measured on 18.6 and 16.15:

- `pg_get_partkeydef` prints `RANGE (ts)`, and prints an expression, an
  operator class or a collation when the key has one.
- `pg_get_expr(relpartbound)` prints `DEFAULT` or `FOR VALUES FROM (…) TO
  (…)`. Each datum is `MINVALUE`, `MAXVALUE`, a quoted literal, or an unquoted
  number, never with a type. Unquoted, a value is its key type's `::text` under
  the session pbps pins (`TimeZone` UTC, ISO dates), measured for `numeric`,
  `real`, `timestamp`, `timestamptz`, `interval`, `varchar`, `uuid` and `bytea`.
  A `timestamptz` bound prints in the session's time zone, which is why the
  pin matters.
- A bound value is coerced with the key column's typmod, and a value that
  does not fit is an error, as `pg_input_is_valid` reports.
- A partition's key and foreign-key constraints are clones (`conparentid` set),
  and so are the foreign keys a table *referencing* the parent gets to each
  partition (`r_fk_1` on 18, `r_p_id_p_ts_fkey1` on 16). A partition's CHECK
  and, on 18, NOT NULL rows are inherited and not local. Its indexes are
  attached in `pg_inherits`.
- A table `ATTACH`ed as a partition ends with every column and constraint
  inherited, as one created `PARTITION OF` does, but keeps its own column order
  and none of the parent's defaults.

**The model.** Leon chose each point on the issue.

- `Table::partition_by` holds the key's columns. Only RANGE over plain columns
  is held; an expression key, a key operator class or collation, LIST and HASH
  stay limitations.
- A partition is an ordinary entry in the schema's tables with
  `partition_of: {parent, bound}` and nothing else. It therefore has a uid of
  its own, which #1171 needs to follow a detach or a drop, and occupancy,
  scope and drift treat it as the table it is. Validation refuses a partition
  that declares anything, a grant included (#1532 relaxes that; its own checks
  and indexes since DEC-1577.1, its grants since DEC-1579.1, its persistence
  and storage parameters since DEC-1580.1).
- A bound value is the engine's own text, unquoted. The connected spelling
  check asks the engine for each declared value's reading as its key column's
  type, under the pinned session, and refuses a different one with the
  engine's spelling. A value is never trimmed or compared as SQL.
- Partitions are written in the parent's file under `partitions:`, in name
  order, each value double-quoted: bare, YAML would read `0x1F` as 31 and a
  text value `MINVALUE` as the unbounded end. The reader leaves a tree out
  rather than hold a text bound spelled like an unbounded end.

**The reader holds a tree only if rebuilding it from the declaration gives it
back.** The parent is RANGE with a key that is its quoted column names and
nothing else, has no access method, storage parameters, row security, rules,
triggers or replica identity, and is permanent. Each partition is attached
with no detach pending, an ordinary permanent heap table with none of those
either and no grant on it or a column. Its columns are inherited and equal to
its parent's in order, type, collation, NOT NULL, identity, generation and
default. Every constraint is a clone or inherited, and every index attached.
Anything else in any table of the tree, nested partitioning and a foreign
table among them, leaves the whole tree out, every table named. So does a
bound the reader cannot parse, and a parent left out for another reason takes
its partitions with it. Clones are dropped by catalog parentage, never by name.

**The plan.** A bootstrap creates the parent with `PARTITION BY RANGE (…)` and
without `USING heap`, which 16 refuses on a partitioned table. Each partition
follows as `CREATE TABLE … PARTITION OF … FOR VALUES … USING heap`, ordered
after every parent, and the engine gives it the parent's keys and indexes.
Every other change to a table that is partitioned or a partition on either
side is refused by name, as `PartitionedTableChange`. That includes a
difference in the key or a bound, which no change carries, and a partition
created under a parent that already stands; both are #1171's to qualify. A
change to another table, a foreign key to the parent included, is not refused.
A partition the database has under a managed parent and the declarations do
not is reported by the scope as something the declarations cannot express,
with `pbps pull` and `pbps baseline` as the remedy: the parent routes rows
into it, so it is not somebody else's table.

`data:` on a partitioned table or a partition is refused in this slice, and so
are `partition_by` and `partitions` on SQL Server.

Pinned by the live `range_partition_trees_round_trip_whole_or_not_at_all` (on
16 and 18), which fails without the clone filter, the column comparison, the
ACL check or the bound probe; the CLI
`a_partition_tree_round_trips_through_the_cli`, which fails without the
undeclared-partition refusal; and the unit tests
`a_partition_tree_is_created_whole_and_otherwise_refused`,
`partitions_load_as_tables_and_render_back_in_the_parent_file` and
`a_deparsed_bound_comes_apart_and_nothing_else_does`.

*Amended by [DEC-1545.1](#dec-1545-1): the reader skips a column the
partition dropped before it was attached.*

<a id="dec-1171-1"></a>

**DEC-1171.1. A standing partition tree gains a partition or loses one with
drop intent, which detaches it first. The apply's pre-flight counts the rows
that would make the engine refuse either, and every other change to a tree
stays refused by name (#1171).**

Measured on 18.6 and 16.15, identical:

- `CREATE TABLE … PARTITION OF` under a standing parent takes ACCESS EXCLUSIVE
  on it, and on its DEFAULT partition, which it scans. Rows of the DEFAULT
  partition inside the new range make the engine refuse the `CREATE` (23514).
- A partition that a foreign key to its parent reaches cannot be dropped
  (`cannot drop table … because other objects depend on it`): the key keeps a
  clone on each referencing table, naming that partition. `DETACH PARTITION`
  removes the clones, and refuses while referencing rows remain (23503).
- A detached table keeps engine-chosen names (`p1_pkey`, `p1_v_idx`) and its
  parent's constraint names, so detaching and keeping the rows is #1544's to
  design, not this slice's.

**The slice.** Leon chose it on the issue (2026-10-05).

- A partition created under a parent that already stands is a `CreateTable`
  as #1170 has it. The differ no longer refuses it. Its estimate names the
  parent's ACCESS EXCLUSIVE lock and the DEFAULT partition's scan, and is
  left out when the parent is created by the same plan.
- A dropped partition is a `DropTable` with `detach_from`, its parent, emitted
  as `ALTER TABLE parent DETACH PARTITION p; DROP TABLE p;` in one statement,
  so that no apply, staged or not, stops between the two. It needs drop intent
  like any table and is `destructive`. SQL Server refuses a `detach_from`, as
  its model holds no partition. Plan version 26 carries the field.
- The parent's own changes (#1546), detaching with the rows kept (#1544),
  attaching an existing table (#1545) and moving DEFAULT-partition rows into a
  new range (#1547) stay refused by name, each with its own issue, all ahead of
  #1172.

**The pre-flight, not the plan, reads the rows** (SPEC 7.2: a plan reads no
data). Both counts are one `SELECT` through `query_to_xml`, built from the
catalog at run time, like the delete probe (DECISIONS 325):

- For a range added under a standing parent, the parent's rows inside the
  range. A range cannot overlap another partition's, so every such row is in
  the DEFAULT partition. The predicate compares the key's leading columns up to
  the first `MINVALUE` or `MAXVALUE` (inclusive below for `MINVALUE`,
  exclusive for `MAXVALUE`, the reverse above), and a row with a NULL key
  column, which no range takes, is not counted. Rows of partitions the same
  plan drops are not counted either, which a range split in one plan needs.
- For a dropped partition, the rows of every table with a foreign key to the
  parent that match a row of the partition, a partitioned referencing table
  with its partitions.

The engine's own refusal inside the transaction stays the backstop.

**`drop_blockers` knows what the detach removes.** It read the referencing
table's clone as a standing dependent, and the clone's internal owner, the
foreign key itself, as one too. Every drop of a partition such a key reaches
was refused, with no row referencing it. The clones are now removals at the
drop's own position that promote nothing, the way a replaced generation
expression promotes nothing (DEC-1316.1).

**A saved plan's replay** needs no new check. A partition added by hand under
a managed parent after planning is reported as inexpressible (DEC-1170.1), and
a partition dropped or rebound by hand moves the baseline checksum. Either is
refused before the first statement.

Pinned by the live `range_partitions_are_added_and_dropped_on_populated_trees`
(on 16 and 18), which fails without the NULL-key guard, the dropped-partition
exclusion or the `drop_blockers` removal; the CLI
`partitions_are_added_and_dropped_through_the_cli`; and the unit tests
`a_partition_tree_is_created_then_gains_and_loses_partitions_only`,
`a_partition_is_detached_and_dropped_in_one_statement`,
`a_range_end_compares_the_columns_before_its_first_unbounded_end` and
`a_partition_change_under_a_standing_parent_is_probed`.

*Amended by [DEC-1545.1](#dec-1545-1): an ordinary table declared in its
parent's `partitions:` is attached, with its rows.*

<a id="dec-1544-1"></a>

**DEC-1544.1. A partition declared as an ordinary table of its parent's shape
is detached and kept, as a `DetachPartition` change that renames what it keeps
to the declared names in the same batch. Any other shape is refused (#1544).**

Measured on 18.6 and 16.15, identical:

- While attached, a partition's clone of its parent's key, unique constraint,
  foreign key or index can be renamed. An inherited CHECK refuses it (`cannot
  rename inherited constraint`).
- `DETACH PARTITION` ends every link to the parent: `conparentid` is 0 and no
  index is left in `pg_inherits`. Columns turn local with the parent's
  defaults. A CHECK keeps its parent's name, and on 18 so do the NOT NULL rows.
- The detach is refused while rows reference the partition (23503), which
  #1171's pre-flight already counts.
- A unique constraint's or a key's index shares the schema's relation
  namespace, so it cannot keep its parent's name in the same schema. A foreign
  key's or a check's is the table's own and can.

**The declaration.** Leon chose it on the issue (2026-10-05): the detached
table is pbps's, and its names are the declaration's, not the engine's. The
partition leaves `partitions:` for a file of its own, under the same name and
so the same uid. The alternatives were to make the declaration copy the
engine's names, or to leave the table unmanaged.

**The differ.** A table that was a partition and is declared as an ordinary,
unpartitioned table plans one `DetachPartition` and nothing else. Its base
holds no columns of its own, so a column-by-column diff would read every one
as new. The declaration must be its parent's shape:

- the columns, in order, their types compared in the dialect's spelling as
  any column's are, so `int` declared is the `integer` read back;
- a key on the same columns;
- every unique constraint, foreign key, check and index matched one to one
  with the parent's by definition, names aside;
- every other field equal: settings, `data:`. A description, the table's or a
  column's, is prose `diff` does not compare and a connected base never holds,
  so it is free.

The parent is read through the plan's renames first, as `diff_constraints`
reads any table: a foreign key to a table renamed in the same plan is declared
under the new name. Anything else is `DetachedShape`, named, and is a second
change to make in a later revision. A rename at the same time stays #1170's refusal.

**The change** carries:

- each of the parent's objects with the name its clone takes. A key left
  unnamed is `None` and keeps the name its clone has, since an unnamed key
  matches any name. Renaming it to `<table>_pkey` would collide whenever
  another relation holds that name, which is why the engine chose another;
- the declared shape. The apply holds the read-back to that shape, as it holds
  a created table to its `CREATE`, and does not compare it with the partition's
  empty base. The declared-expression record takes its text from the shape and
  its bindings from the parent, re-keyed to the declared names.

It sorts in class 6, so it frees its range before a partition is created over
it, and after every table drop of that class, since a dropped table's index or
key may hold a name the detach claims; a drop claims none. A name an engine
gave another partition's clone, which only the catalog holds, is not ordered
for (#1558). It is `destructive`: no row is deleted, but every row of the partition
leaves its parent, and a query on the parent stops returning them. Plan
version 27 carries it, and SQL Server refuses it.

**The statement** is one `DO` block:

0. It locks the parent (`LOCK TABLE ONLY`, `ACCESS EXCLUSIVE`) before anything
   else. A rename locks the partition, and a reader holding the parent while
   reaching for the partition would otherwise deadlock with the batch; measured
   on 18, the engine aborts one of the two. Pinned by the live
   `a_detach_takes_the_parent_before_the_partition`.
1. It finds each clone through its parent object, as `drop_primary_key` finds
   an engine-chosen name: `conparentid` for a key, a unique constraint or a
   foreign key, and `pg_inherits` for an index. Each one whose name changes is
   renamed with `EXECUTE format(... %I ...)`, the table's name escaped for
   `format`, to a temporary name. A clone not found raises, rolling the batch
   back.
2. The detach.
3. Each check whose name changes is renamed from its parent's name to a
   temporary name.
4. Every temporary name is renamed to its declared one.

The temporary names exist because a declaration may exchange two names, or give
a clone a name a check still holds; renaming straight across would collide
with a name not yet vacated. How they are chosen, and what happens to a
declared name something else holds, is DEC-1565.1, which superseded this
entry's static prefix.

The block runs on the table's schema's path, where a user who may create there
can add an operator. Measured on 16 and 18, an operator whose arguments match
exactly (`text || oid`, `oid = regclass`) is chosen over a built-in that needs
a cast, `pg_catalog` searched first or not; DECISIONS 276 protects only an
identical signature. So the block uses no such operator: every class is
compared as an `oid`, and every name is built by `pg_catalog.format`. Pinned by
the live `a_detach_calls_no_operator_a_schema_user_could_add`, whose operators
the previous block called 71 times.

Pinned by the live `a_partition_is_detached_and_kept_under_its_declared_names`
(on 16 and 18), which fails without the renames, with renames made straight
across (an exchange of names), or with an unnamed key renamed to its default
name while a sequence holds it; the CLI
`a_partition_is_detached_and_kept_through_the_cli`, which fails without the
apply holding the table to its declared shape; and the unit tests
`a_partition_declared_as_its_parents_shape_is_detached`, which fails without
the shape check, without the type normalization, or without the renames, and `a_detach_renames_what_it_keeps_around_one_batch`.

<a id="dec-1565-1"></a>

**DEC-1565.1. A detach chooses its temporary names from the live catalog, and
refuses by name a declared name something outside the batch holds (#1565;
supersedes DEC-1544.1's temporaries).**

Review of #1556 found one edge case after another in DEC-1544.1's name moves:

- an exchange of names;
- a hand-off between two detaches (#1558);
- a name held by a table the plan drops;
- a declared name spelling a temporary one;
- a temporary prefix grown past the 63-byte identifier limit, where the
  engine truncated every temporary to one name.

Each was a guess made at plan time about a namespace that exists only at apply
time, so the choices moved into the block, under the parent's lock it already
takes first:

- **Temporaries.** The `k`th name that moves takes the first `pbps_t<k>_<n>`
  (`n` from 0) that no relation in the schema, no constraint on the table and
  no declared name holds. Short and counted, it cannot reach the limit, and
  nothing at plan time has to be avoided.
- **Claims.** Once every moving name sits on its temporary, each declared name
  is checked where the engine would check it. An index's name is checked
  among the schema's relations, a foreign key's or a check's among the
  table's constraints, and a key's or unique constraint's in both. A holder
  left is outside the batch: the block raises `cannot give <table> the name
  <name>: <holder> already holds it`, naming the holder in full, with a hint
  to free the name in a plan of its own first, and the transaction rolls
  back before any declared name is given.

A table dropped in the same plan still frees its names first: detaches sort
after the drops of their class (DEC-1544.1). A clone of another partition
detached in the same plan cannot be ordered for from the declarations, since
its name is the engine's. That case is now this named refusal with a two-plan
remedy, which is what #1558 asked of the plan short of ordering it.

Every operator in the block keeps to DEC-1544.1's rule: classes are compared as
`oid`, names are built by `pg_catalog.format`, and the rest are built-ins'
exact signatures.

Pinned by the live `a_detach_takes_free_temporaries_and_refuses_a_held_name_by_name`
(on 16 and 18). Its first detach declares the names the first temporaries
would take, has a sequence squatting on one, and uses two names at the
identifier limit, one spelled like #1544's prefix; it fails against #1544's
block. Its second refuses a name held by a sequence and one held by another
partition's index clone, each naming its holder and changing nothing; it fails
without the claim check. Also pinned by the unit
`a_detach_renames_what_it_keeps_around_one_batch`.

<a id="dec-1577-1"></a>

**DEC-1577.1. A partition declares its own checks and indexes under its entry
in the parent's file, and the reader reads them as its own by catalog
parentage; the parent's clones stay the parent's (#1577).**

Leon chose the slice on #1532: read back and create, with the declaration in
the parent's file. Changing them on a standing partition stays refused by name
until #1581.

Measured on 16.15 and 18.6:

- An own CHECK added to a partition is `conislocal` with `coninhcount = 0` and
  `conparentid = 0`. The clone of the parent's is not local and has
  `coninhcount = 1`. One written `CREATE TABLE … PARTITION OF … (CONSTRAINT ck
  CHECK (…))` under a parent check's name, with its expression, is merged into
  the inherited one with a notice and ends `conislocal = f`. One added with
  `ALTER TABLE … ADD CONSTRAINT` under that name is refused, `constraint "ck"
  for relation "p1" already exists`.
- An own index has no `pg_inherits` row; a clone of the parent's has one. A
  unique own index is allowed.
- Clones keep the parent's names for a CHECK and a foreign key (and, on 18, a
  NOT NULL row). The engine chooses names for the clones of an index and of a
  key (`p1_v_idx`, `p1_pkey`). An own index under a name a clone took is
  `relation "p1_v_idx" already exists`.

**The shape.** A `partitions:` entry stays `{from, to}` or `default` while the
partition has nothing of its own. Once it has, the entry is a block: `from:` and
`to:`, or `default: true`, then `checks:` and `indexes:` written as a table's.
The schema states the bound as one alternative or the other, as the loader
takes it, so an editor does not bless `{}`.

**What is read.** The tree predicate admits a CHECK that is local and
inherited from nowhere, and any index; an own key, unique constraint, foreign
key, exclusion constraint or NOT NULL row still leaves the tree out. The
partition then takes its CHECK rows that are not inherited, and its indexes
that are neither attached nor behind a constraint. A clone is never told apart
by name.

**What is created.** After `CREATE TABLE … PARTITION OF`, each own check by
`ALTER TABLE … ADD CONSTRAINT` and each own index by `CREATE INDEX`, as for a
new table. Not in the `CREATE`: a check written there under a parent check's
name would be merged without a word, where the `ALTER` refuses it.

**What is validated.** A partition declares no columns, so its own items are
held to its parent's by `Dialect::validate_partition`, with the rules a table's
are held to. A check named as one of the parent's checks or foreign keys is
refused offline, since its clone keeps that name. The names the engine chooses
for the other clones are the engine's to choose and are not guessed. An own
name one of them took fails the apply on the engine's `already exists`, and the
transaction with it; the remedy is another name. A pull whose partition fails
validation leaves the whole tree out, since a tree is read whole or not at all.

**Plan version 28.** An older build reads a created partition's `checks` and
`indexes`, which `Table` always had, and emits the partition without them, so
a saved plan carrying them is version 28 and refused by an older build.

**A detach keeps them.** DEC-1544.1's shape check matches the partition's own
checks and indexes by name and definition before the parent's by definition.
An own one declared under another name, or changed, is a change the detach
does not make, and is refused by name.

Pinned on 16 and 18 by the live `range_partition_trees_round_trip_whole_or_not_at_all`
(own check and index, a unique own index on the DEFAULT partition, rebuilt from
the declaration; an own unique constraint still leaves its tree out; it fails
without the predicate change), `a_partition_is_detached_and_kept_under_its_declared_names`
and the CLI's `a_partition_tree_round_trips_through_the_cli` (which fails
without the validation wiring). Also by the units
`a_partition_reads_its_own_checks_and_indexes_and_no_clone`,
`a_partitions_own_checks_and_indexes_answer_to_its_parents_columns`,
`a_partitions_own_checks_and_indexes_are_its_own_through_a_detach`,
`a_partition_whose_own_index_is_refused_takes_its_tree_out`,
`a_partitions_own_checks_and_indexes_round_trip_under_its_entry`,
`a_partition_entry_is_one_bound_and_only_what_a_partition_owns` and
`declaration_schema_and_loader_agree_on_a_partition_entry`.

*Amended by [DEC-1581.1](#dec-1581-1): changing them on a standing partition
plans and applies.*

<a id="dec-1578-1"></a>

**DEC-1578.1. A partition declares its own column defaults and NOT NULLs under
its entry's `columns:`, held on `PartitionOf` and never as columns of its own;
the reader reads one only where it is not the parent's (#1578).**

The second of #1532's four slices: read back and create, the declaration in
the parent's file. Changing one on a standing partition stays refused by name
until #1581.

Measured on 16.15 and 18.6:

- The engine copies the parent's defaults into the partition's own
  `pg_attrdef` as it creates it, with the parent's text. A default of the
  partition's own is therefore one whose text is not the parent's.
- A NOT NULL of the partition's own, where the parent's column is nullable, is
  `attnotnull`. On 18 it is also a constraint row, local and inherited from
  nowhere, named `<partition>_<column>_not_null` by the engine whether it was
  written in `PARTITION OF (…)` or by `ALTER` after. The model holds no NOT
  NULL constraint name, for a table or a partition.
- A partition can `DROP DEFAULT` a default its parent's column has.
- A default on a partition's generated column is refused on both; on an
  identity column it is refused on 18. On 16 the partition's column does not
  carry the identity at all, which DEC-1170.1's comparison already leaves out.
- A row written through the parent takes the parent's defaults, not the
  partition's: a partition's own default is what a row written to it directly
  gets.
- The parent's `ALTER COLUMN … SET DEFAULT` reaches every partition and
  overwrites a partition's own default; with `ONLY` it does not. Every
  partition's default, its own or its copy of the parent's, depends on the
  functions it calls.

**The shape.** `PartitionOf::columns` maps a column to `PartitionColumn {
default, not_null }`, each an override, so a table that is not a partition
cannot hold one and a partition still declares no column, type included. In
the file, a block entry's `columns:` holds `{default: …}`, `{nullable:
false}`, or both, after the bound. `nullable: true` is refused, since a
partition cannot drop its parent's NOT NULL, and so is an entry with neither.

**What is read.** The tree predicate stops comparing NOT NULL and plain
defaults in its column list, keeping the generation expression, and instead
refuses a partition column whose parent's is NOT NULL and its own is not, or
whose parent's has a default and its own has none: a dropped default still
leaves the tree out, named. On 18 it admits a validated NOT NULL row of the
partition's own. The partition then reads a default whose text differs from
the parent's, and a NOT NULL the parent's column lacks, a generated column's
included: measured on 16 and 18, `SET NOT NULL` on a partition's generated
column is its own, and on 18 a local row as above. A default of its own that
uses a sequence names the sequence as a column's does, since the declaration
holds the default and not the sequence.

**What is created.** One `ALTER TABLE … ALTER COLUMN … SET DEFAULT …, ALTER
COLUMN … SET NOT NULL` after `CREATE TABLE … PARTITION OF`, before the
partition's own checks and indexes. Measured, it gives what the same words in
the `CREATE` give, the NOT NULL row on 18 under the same engine name.

**Ordering.** That statement is part of the partition's `CreateTable`, which
sorts ahead of the modules. In the ordinary plan, a default of its own that
calls a function the plan creates therefore holds the whole partition after
that function, as a generated column holds its table (DEC-1364.1). It is not
split out into an `AlterColumnDefault` as a table column's default is: the
partition has no column, and no column uid, of its own. A new partition whose
own default names a held new table follows it too. Scratch reconstruction
sets every table's defaults in its expressions phase, after the modules; a
partition's own default is set there too, after its parent's, which would
otherwise overwrite it. Bootstrap does not run the ordering passes for any
table (#1585).

**A parent's default after its partitions.** Two places would set a parent's
default after a partition with its own on that column exists, or take a
partition's default off around a function rebuild. Each needs a change that
sets one partition's default, which #1581 brings, and both are #1588. Until
then each is refused by name: a new parent's default split out after a new
function (DEC-942.1) while a new partition overrides that column, with the
two-plan remedy; and a function rebuild under any partition's default, its
own or its parent's copy, which `weave` now names as a partition's default
rather than as one the project does not declare. The copy's refusal predates
this entry (#1170).

**What is validated.** `validate_partition` refuses an override of a column
the parent lacks, a NOT NULL the parent's column already has, the parent's own
default again, a default on a generated or identity column, and an empty
default. It also holds the default as a column's is, against the parent's
column type: a NULL the engine erases (measured on 16 and 18, a NULL of the
column's type leaves the partition no default at all, even its copy of the
parent's) and a setting-sensitive bare literal. The comparison with the parent's default is of declared text: an
override the engine spells as the parent's reads back as none, and is then a
change the plan refuses by name.

**The declared record.** A partition's own default is recorded under the
partition and the column in `DeclaredExpressions::defaults`, as a column's
is, and overlaid on the read-back, so the engine's spelling of it plans
nothing. No new field, so the record's shape is unchanged.

**A detach keeps them.** DEC-1544.1's shape check compares the detached
table's columns with the parent's as the partition holds them: its own default
and NOT NULL applied.

**The resolver.** A partition's own default is a `pg_attrdef` binding
surface, as a column's default is. `Table::expression_columns` lists both, and
every inventory of binding surfaces reads it: the assessment's, the
qualified catalog's required set, the ordering graph's, the evidence's, and
the owner transitions'. Read from `columns` alone, a partition, which has
none, held nothing, and a plan whose only bound expression was a partition's
own default was assessed as needing no resolver. Capturing that row on
scratch is #1587. Until then a resolver run that needs it is refused, since
the qualified catalog has no record of the surface.

**Verification.** A created partition's own defaults and NOT NULLs are
verified as a column's are: NOT NULL exactly, a default by presence.

**Versions.** Plan version 29 and state version 21: `PartitionOf` gains a
field an older build refuses to read, so each says so by its version rather
than failing to parse.

Pinned on 16 and 18 by the live `range_partition_trees_round_trip_whole_or_not_at_all`
(own default and NOT NULL read and rebuilt, the direct insert taking them; a
dropped default leaves its tree out; the other partitions declare none),
`a_partition_is_detached_and_kept_under_its_declared_names`, and the CLI's
`a_partition_tree_round_trips_through_the_cli`. Also by the units
`a_partition_reads_its_own_defaults_and_not_nulls_and_not_its_parents`,
`a_partitions_own_column_overrides_answer_to_its_parents_columns`,
`a_partitions_own_column_overrides_are_its_own_through_a_detach`,
`a_partitions_own_default_is_recorded_and_overlaid`,
`a_partitions_own_column_defaults_and_not_nulls_round_trip_under_its_entry`,
`declaration_schema_and_loader_agree_on_a_partition_entry`,
`a_partitions_own_default_from_a_sequence_names_it`,
`a_new_partitions_own_default_calling_a_new_function_follows_it_whole`,
`a_held_new_table_leaves_unrelated_new_tables_free`,
`a_partitions_own_default_is_set_after_its_parents_and_its_functions`,
`a_new_tables_expressions_are_split_out_to_follow_a_rebuilt_function`,
`a_dependent_the_plan_cannot_account_for_refuses_it_by_name` and
`a_created_partitions_own_defaults_and_not_nulls_answer_for_themselves`; the
CLI's `partitions_are_added_and_dropped_through_the_cli` applies a partition
whose own default calls a function the same plan creates.

*Amended by [DEC-1581.1](#dec-1581-1): a partition's own default and NOT NULL
change on a standing partition, and a parent's default set by a plan, around a
rebuild or split out of a new table, is followed by each partition's own
instead of refused (#1588).*

*Amended by [DEC-1609.1](#dec-1609-1): a partition's own default the engine
would store with its parent's text is refused at `plan --db`, before the
plan, instead of by the apply's closing check (#1609).*

<a id="dec-1579-1"></a>

**DEC-1579.1. A grant on a partition, and one on a partitioned parent, is that
table's own, declared and read as any table's (#1579).**

The third of #1532's four slices. Nothing new is declared: a role's file names
a partition as it names any table, `app.ev_2025: [select]`, and the partition
is a declared table since DEC-1170.1.

Measured on 16.15 and 18.6:

- A grant on the parent lands in the parent's `relacl` alone, whether made
  before or after its partitions exist, and reaches the partitions only through
  the parent: `SELECT` on a partition directly is refused to a role granted
  only on the parent.
- A grant on a partition lands in the partition's own `relacl`, and a column
  grant in its `attacl`, as on any table.
- A table `ATTACH`ed as a partition keeps its own `relacl`.

**What changed.**

- The tree predicate no longer leaves a tree out for a partition's `relacl`.
  A column grant still does: no table holds one (SPEC §4.7 reports them and
  leaves them alone), and for a partition that is the tree's named
  limitation, as before.
- `check_partitions` no longer refuses a role granted on a partition.
- The reader takes a grant on a partitioned table (`relkind` `p`) as a grant
  on that table. Since DEC-1170.1 the model declares one, yet its grant was
  still reported as on a kind the model does not declare, and so was never
  held. A partitioned table the pull does not hold has its grant reported as
  on an object the pull did not record, as an ordinary table's is.
- `doctor`'s two grant questions, the deployer's authority over a recorded
  grant and over a target's existing ACL, ask of a partitioned table too.
  Asking of tables and views alone, they found a recorded grant on a parent
  absent and refused readiness after a clean bootstrap.

Everything else is the grant model's for any table: a grant on a new
partition is planned after its `CREATE`, granting and revoking on a standing
partition plan as on a table (a revoke behind `--allow revoke`), and a dropped
partition takes its grants with it. PUBLIC's grant on a partition is reported
as on a table and never compared (ADR-0010 §5).

Pinned on 16 and 18 by the live `range_partition_trees_round_trip_whole_or_not_at_all`
(a tree with a PUBLIC grant on a partition is held and the grant reported; one
with a column grant is left out), and on 18 by the CLI's
`a_partition_tree_round_trips_through_the_cli` (pull, bootstrap, verify and an
empty replan with a grant on the parent and one on a partition, the parent's
not reaching the partition; `doctor` clean after it; a grant and a revoke
on the standing partition)
and `partitions_are_added_and_dropped_through_the_cli` (a granted partition
dropped). Also by the units `a_partition_needs_its_parent_and_its_key`'s
grant case and `a_grant_on_a_relation_kind_this_model_does_not_declare_is_reported`.

<a id="dec-1580-1"></a>

**DEC-1580.1. A partition's persistence and storage parameters are its own,
declared under its entry as a table's and created in its `CREATE TABLE … PARTITION OF`
(#1580).**

The last of #1532's four slices: read back and create. Changing either on a
standing partition stays refused by name until #1581.

Measured on 16.15 and 18.6:

- A partition is `UNLOGGED`, and carries heap storage parameters, `toast.*`
  among them, of its own, set in `CREATE … PARTITION OF … USING heap WITH (…)`
  or by `ALTER TABLE` after; the two read the same.
- The parent gives it neither. A partitioned table refuses storage
  parameters, and on 18 refuses `UNLOGGED`; 16 takes an `UNLOGGED` parent and
  creates its partitions permanent all the same.
- A table `ATTACH`ed as a partition keeps both.
- A permanent table's foreign key to a partitioned parent with an `UNLOGGED`
  partition is accepted, whichever is made first, though the same key naming
  the `UNLOGGED` table itself is refused.

**The shape.** A `partitions:` entry takes `unlogged: true` and
`storage_parameters:`, written and spelled as a table's, after its checks and
indexes; with either the entry is a block. `toast.*` is refused as on a table:
a table without a TOAST relation drops it, so it could never read back.

**What is read.** The tree predicate admits a partition that is `UNLOGGED` or
has storage parameters, and reads both as the partition's, through the
table's reader. The parent must still be permanent with none, so 16's
`UNLOGGED` parent still leaves its tree out. A partition's TOAST parameters
still leave the tree out, named, as they leave a table out.

**What is created and checked.** Both go in the partition's `CREATE`, as a
table's do, so the post-apply check holds them from the first read that finds
it, as a table's. `validate_partition` holds the parameters to the table's
spelling rules.

**A permanent key through the parent is refused.** The engine's acceptance
above is a hole in its own rule: a crash empties the `UNLOGGED` partition and
leaves the referencing rows pointing at nothing. The differ therefore extends
its refusal of a permanent table's key to an unlogged one (#1443) to a key
whose target is a partitioned table with a declared `UNLOGGED` partition, as
`PermanentReferencesUnloggedPartition`, naming the partition, whether the key
or the partition comes first. A database already in that state pulls into
declarations the plan refuses by name; the remedy is the one the message
gives. A referencing table pbps does not manage is not seen (#1595).

**A detach keeps them.** DEC-1544.1's shape check compares the detached table
with its parent's settings overlaid by the partition's own persistence and
storage parameters. Declared otherwise, it is refused by name, since the
detach does not change them.

**Plan version 30, schema set 34.** An older build reads a created
partition's `unlogged` and `storage_parameters`, which `Table` always had, and
emits the partition permanent and without them. The state needs no new
version: `Table` already carries both.

Pinned on 16 and 18 by the live `range_partition_trees_round_trip_whole_or_not_at_all`
(an `UNLOGGED` partition with two parameters read and rebuilt; a TOAST
parameter leaves its tree out), and on 18 by the CLI's
`a_partition_tree_round_trips_through_the_cli` (pulled under the entry,
bootstrapped as declared, the other partition permanent with none, a change
to either refused by name). Also by the units
`a_partition_reads_its_own_persistence_and_storage_parameters`,
`a_partitions_own_persistence_and_storage_round_trip_under_its_entry`,
`a_partition_tree_is_created_parent_then_partition`,
`a_partitions_own_checks_and_indexes_answer_to_its_parents_columns`,
`a_partitions_own_persistence_and_storage_are_its_own_through_a_detach` and
`a_created_partitions_persistence_and_storage_answer_for_themselves`.

*Amended by [DEC-1581.1](#dec-1581-1): changing either on a standing
partition plans and applies.*

*Amended by [DEC-1595.1](#dec-1595-1): a referencing table pbps does not
manage is read from the catalog, and the partition is refused by name.*

<a id="dec-1581-1"></a>

**DEC-1581.1. A standing partition's own indexes, checks, storage parameters
and persistence change through the table's change kinds, and its own defaults
and NOT NULLs through two kinds keyed by the partition and the column; a
parent's default the plan sets is followed by every partition's own (#1581,
#1588).**

The follow-up to #1532's four slices, which read and created a partition's own
properties (DEC-1577.1 to DEC-1580.1). This one changes them on a partition
that stands. Grants on a partition already plan as any table's (DEC-1579.1).

Measured on 16.15 and 18.6, each against a partition holding rows:

- `SET UNLOGGED` and `SET LOGGED`, `SET (…)` and `RESET (…)`, `CREATE INDEX … ON`
  the partition and `DROP INDEX`, and `ADD CONSTRAINT … CHECK` and `DROP
  CONSTRAINT` all act on the partition alone.
- `ALTER COLUMN … SET DEFAULT` with the parent's text puts the parent's copy
  back after a default of the partition's own, and reads back as the parent's.
  `DROP DEFAULT` leaves the partition's column with none.
- `SET NOT NULL` and `DROP NOT NULL` work on a column its parent leaves
  nullable. On 18 `SET NOT NULL` adds the local row `<partition>_<column>_not_null`.
  `DROP NOT NULL` on a column its parent holds NOT NULL is refused, and
  `SET NOT NULL` over a NULL the partition holds fails as a table's does.
- On the parent, `ALTER COLUMN … DROP DEFAULT` takes every partition's default
  away, a partition's own included, even one that calls nothing the plan
  rebuilds. `SET DEFAULT` overwrites every partition's own. With `ONLY`, both
  leave the partitions alone.

**The table's kinds.** `AddIndex`, `DropIndex`, `AddCheck`, `DropCheck`,
`SetStorageParameters` and `SetTablePersistence` act on the partition alone, so
`refuse_partition_changes` admits them on a table that is a partition on both
sides and not itself partitioned. A clone of the parent's index or check is
never one: the reader holds only the partition's own (DEC-1577.1), and a
clone is the parent's to change (#1546). Everything else on a partition stays
refused by name, its renames, its own index's storage parameters and its
replica identity among them, and so does every change to a partitioned parent.
Switching a partition to `UNLOGGED` under a parent a declared permanent table
references is refused, as creating one is (DEC-1580.1).

**Two kinds of their own.** A partition declares no column, so it has no
column uid, and `AlterColumnDefault` and `AlterColumnNullability` cannot name
one. `SetPartitionDefault` and `SetPartitionNotNull` are keyed by the
partition's table uid and the column's name.

- `SetPartitionDefault { from, to, fallback }`: `from` and `to` are the
  partition's own default as declared, and `fallback` is its parent's
  declared default.
  - Without one of its own, the partition takes its parent's back: `SET
    DEFAULT <fallback>`, measured to read back as the parent's.
  - `DROP DEFAULT` is emitted only with neither. Otherwise a row written
    straight to the partition would lose the default its parent gives.
  - It carries its `parent`, because each text is resolved under the schema
    it was written for. Its own default is set under the partition's write
    path, as its `CREATE` sets it. Its parent's is set under the parent's
    write path, which a partition in another schema does not share: an
    unqualified `f()` is the parent schema's function, not the partition
    schema's (#1607 review).
  - It sorts with `AlterColumnDefault`, and `after_the_rebuilds` moves one
    whose text calls a function the plan creates.
  - `Declared::advance` records `to` as a column's default is recorded.
- `SetPartitionNotNull`: tightening carries `not_null` and a pre-flight NULL
  count on the partition. Relaxing carries nothing. It sorts by the same
  ranks as `AlterColumnNullability`.

**Around a rebuild (#1588).** Every partition holds a default row on a column
whose parent has a default: the copy the engine gave it, or its own. Each such
row that calls a function the plan rebuilds depends on it, and `weave` now
holds it as managed whenever the partition is declared:

- it is released with `SetPartitionDefault { to: None, fallback: None }`
  before the drop;
- it is restored after the create with the partition's own default, or its
  parent's.

The parent's release, an ordinary `DROP DEFAULT`, takes every partition's row
with it, and its restoration, an ordinary `SET DEFAULT`, overwrites every
partition's own. So `after_their_parents_defaults` runs last on a connected
plan and in bootstrap, after any parent's default the plan sets. It sets
every declared partition's own default on that column again, after the
parent's:

- one the plan already sets earlier is moved there;
- one it does not set is added, including a partition whose own default calls
  nothing the plan rebuilds;
- a partition created after the parent's default needs neither, since its
  `CREATE` sets its own.

The same pass replaces the refusal in `split_new_tables`. A new parent's
default split out after a new function no longer refuses a new partition
that overrides that column. That partition's own is set again after it.

**Held at the close.** A staged apply's closing read holds a standing
partition's own defaults (by presence) and NOT NULLs, as it holds a created
partition's:
- once the run is whole, to the before-read with the plan's settings applied
  in order;
- mid-run, a field the plan sets may hold either value: a column's default
  where the plan sets it on the partition or on the parent, and its NOT NULL
  where the plan sets that. Every other field is held to the before-read,
  including the other field of a column the plan touches.

Another session's change to them is movement (#1607 review).

**Why not `ONLY`.** `ALTER TABLE ONLY parent` would leave every partition
alone, which is right around a rebuild. It is wrong for a split default: the
partitions created before it would get no copy at all. It also spells the
parent's own change, which is #1546's to decide. Setting each partition's own
again after the parent's works for both.

**Plan version 31.** An older build cannot read the two new kinds. The state
needs no new version.

Pinned on 16 and 18 by the CLI's
`a_standing_partitions_own_properties_change_through_the_cli`:
- each kind applied to a populated partition, verified and replanned empty;
- the default dropped back to the parent's, which a row written to the
  partition then takes;
- negatives: a NOT NULL over the partition's NULLs is refused before the first
  statement, and a parent's index stays refused by name.

Also by `a_partitions_default_taken_back_resolves_under_its_parents_schema`: a
partition in another schema takes back its parent's unqualified `f()` as the
parent schema's function.

Also pinned by `a_function_rebuilt_under_partition_defaults_keeps_each_partitions_own`:
- a parent's default calling `app.f` with an inheriting partition, an
  overriding one, and one whose own default calls only `app.g`;
- `app.f` rebuilt: applied, verified, replanned empty, each partition keeping
  its default;
- the same tree created by one plan and by a bootstrap with `app.f` new;
- `app.g` dropped for good under a partition's own default is refused by name.

Also by the units:
- `a_partitions_own_column_overrides_are_its_own_through_a_detach`;
- `a_partitions_own_checks_and_indexes_are_its_own_through_a_detach`;
- `a_partitions_own_persistence_and_storage_are_its_own_through_a_detach`;
- `a_partitions_default_is_released_and_set_again_after_its_parents`;
- `a_partitions_own_columns_set_by_someone_else_are_movement`;
- the partitioned case of the new-table split test in `dependents.rs`.

<a id="dec-1595-1"></a>

**DEC-1595.1. An `UNLOGGED` partition a plan creates under a parent that a
permanent table outside the declarations references is refused by name, from
a catalog read at `plan --db` and again before the apply's first statement
(#1595).**

DEC-1580.1 refuses a declared permanent table's key to a parent with an
`UNLOGGED` partition. A table pbps does not manage is not in the declarations,
so the differ cannot see its key. Measured on 16 and 18, with a permanent
table holding a key to the partitioned parent:
- `CREATE UNLOGGED TABLE … PARTITION OF` is accepted, and the partition gets
  a copy of that key;
- `ALTER TABLE <partition> SET UNLOGGED` on a standing partition is refused by
  the engine itself ("could not change table … to unlogged because it
  references logged table …"), so only a created partition needs the read.

**The read.** For the parent of each partition the plan creates `UNLOGGED`,
`pg_constraint` gives every foreign key whose target is that parent and whose
table holds rows (`relkind = 'r'`) and is permanent. A partitioned
referencing table holds no rows. Each of its leaf partitions carries a copy of
the key, so a permanent leaf is named and an unlogged one, which a crash
empties too, is not. A leaf attached with a key of its own keeps that key's
name (measured on 16 and 18), so each copy is followed up `conparentid` to
the key its partitioned table declares, and named through it.

**What is not a referencer.** A key the plan takes away: a table it drops or
makes unlogged, or a key it drops, matched against the leaf's copy or the key
its table declares. Each is compared under the catalog's name: a persistence
switch through its table's rename by uid, a dropped key under the name the
rename leaves, and a dropped table as it stands, since a rename may take the
name it frees. In a transactional plan the removal may sit anywhere, since
both commit or neither does. In a staged plan only one that runs before the
partition's `CREATE` counts: a staged apply commits between statements, and
the persistence switch sorts after table creation, so there making the
referencing table unlogged takes a plan of its own first. A declared
permanent referencer is refused before planning (DEC-1580.1) and is not read
again.

**Why not a probe.** A preflight `Probe` returns a count, and the message has
to name the referencing table and key. A probe that cannot run is also
reported as "the engine will enforce it during the apply", which is not true
here: the engine takes the partition. So this is a catalog read like the
relation-name occupants (#951), and a failed read is an error, never "no
referencers".

**Why again at the apply.** Nothing binds a saved plan to the catalog it was
planned against. A key added after the plan meets the partition all the same.
The read runs only on a fresh start, as the other preflight reads do.

Pinned on 16 and 18 by the CLI's
`an_unlogged_partition_under_an_undeclared_permanent_key_is_refused`:
- an undeclared permanent table's key refuses the plan, naming the table and
  the key, and a partitioned one's leaf copy is named through the key its
  table declares;
- a plan made while only an unlogged table held a key is refused at the apply
  once a permanent key arrives, and nothing ran;
- negatives: the unlogged referencing table is not named, and a permanent
  partition plans, applies and verifies.

Also by the unit `an_unlogged_partition_under_a_permanent_key_is_refused_by_name`.

<a id="dec-1462-1"></a>

**DEC-1462.1. A connected SQL Server plan proves that no referrer of what it
changes is hidden before it reads the catalog's expression edges as complete
(#1462; amends DEC-1431.1).**

**Context.** DEC-1431.1 reads `sys.sql_expression_dependencies` and refuses
or moves a change by the edges it finds. An empty read meant "no edge" whether
there was none or this login could not see the referrer, so an alter of a
function that a hidden computed column calls planned as if nothing called it,
and the engine refused it at apply (3729).

**Measured** on 17.0 with a login that is not `db_owner`:

| The login holds | `sys.sql_expression_dependencies` | the referencing `sys.objects` row |
|---|---|---|
| no `SELECT` on the view (`db_ddladmin` included) | Msg 229 | — |
| `SELECT` on it, no database `VIEW DEFINITION` | no row at all, no error | absent |
| both, schema `DENY VIEW DEFINITION` on the referrer's schema | the edge's row | absent |
| both, object `DENY VIEW DEFINITION` or `DENY CONTROL` on the referrer | the edge's row | absent |
| `db_owner`, with the same schema `DENY` | the edge's row | present: the owner's override |

**Decision.** The edges are read only for a plan with a change they decide:
a function's alter or drop, a computed column's drop, a module's drop beside
another drop `release` may order, or a column's rename, drop, retype or
nullability change on a table with a computed column. `sys.computed_columns`
answers the last for a login holding `VIEW DEFINITION` on the table's schema,
and a table it cannot see counts as having one. A plan that only adds a
computed column, drops a table or one view, or changes a column of a table
with no computed column reads none, and needs no grant for them. Before the
edges are read, `prove_referrers_visible` asks for database
`VIEW DEFINITION`, the grant without which the view answers nothing.
It then looks for an edge whose referencing object `sys.objects` does not
show, onto what a hidden referrer could block: a function the plan alters or
drops, which a computed column anywhere may call, and a computed column the
plan drops, matched by that column, which a schema-bound module may read.
Only a schema-bound reference counts (`is_schema_bound_reference`, which the
row keeps when its referrer is hidden): a computed column's call and a
`WITH SCHEMABINDING` module's read block the change, a plain procedure or view
does not. Either refuses the plan by name: "a referrer of `dbo.f` is hidden from this
login". A column change or a computed column added is judged by the computed
columns of its own table, which are visible with it, so a hidden view over
another column of that table refuses nothing. A `db_owner` member sees through
the `DENY` and passes, as the engine lets it.

**Why not a permission check alone.** DEC-1192.1's schema-level proof answers
for the schemas a plan creates names in; a referrer can be in any schema. A
database grant does not override a schema or object `DENY` (DECISIONS 460),
and the denial's own row leaves which object it hides unsaid. The engine's
own edge row, whose referencing object this login cannot see, names exactly
the object that has a hidden referrer.

Pinned by the live `a_hidden_referrer_refuses_the_plan_rather_than_reading_as_no_edge`
(`crates/pbps-cli/tests/flow.rs`): the login denied `VIEW DEFINITION` on the
referrer's schema is refused by name; as `sa`, and as the login once the
`DENY` is revoked, the edge itself refuses the alter. A hidden schema-bound
view over `dbo.k.c` refuses that column's drop by name, and a computed column
added beside it plans. A hidden plain view over `dbo.k.c`, or a hidden
procedure calling `dbo.h`, refuses neither the drop nor the alter. With
managed-schema `VIEW DEFINITION` only, the alter and a nullability change on a
table with a computed column are refused for the database grant, and adding a
computed column, the same change on a table with none, and dropping one view
plan.

<a id="dec-1460-1"></a>

**DEC-1460.1. A connected SQL Server plan leaves a standing computed column to
the catalog's edges, not the differ's text screen (#1460; amends DEC-1174.1).**

**Context.** DEC-1174.1's screen refuses a change to a column a standing
computed column may read, or to a function it may call, by a text scan that
folds case. That over-approximation is safe offline, where a plan is never
applied (SPEC §7.3). Since DEC-1431.1 a connected plan also reads the
engine's own edges, matched under the database's collation. The screen ran
first, so in a `Latin1_General_100_CS_AS` database a retype of `A`, beside a
computed column reading `a`, was refused although SQL Server accepts it.

**Decision.** The differ takes a `Screen`. Offline paths keep `Screen::Text`.
A connected SQL Server plan diffs with `Screen::Catalog`, which skips the
screen for a computed column that stands throughout the plan, for both its
inputs and the functions it calls. `order_computed_by_edges` then judges
those columns by the edges. Since DEC-1462.1, every change the screen skips
there is one the pass reads the edges for, or refuses without the grant that
makes them complete.

**Why not for an added column.** A computed column the plan adds, new or
again, has no edge for its new text until the plan stores it, so the screen
still judges what it calls (#1459).

*Amended by [DEC-1459.1](#dec-1459-1): a connected plan also compares the
names it calls with the functions the plan changes, under the collation.* PostgreSQL has no computed columns and
keeps `Screen::Text`.

**Why a parameter, not a hint.** `Hints` come from the declarations and never
take part in a comparison. Whether a catalog will be read is the caller's
fact, so the caller passes it.

Pinned by the live `a_connected_plan_does_not_fold_case_for_a_standing_computed_column`
(`crates/pbps-cli/tests/flow.rs`): in a case-sensitive database, retyping `A`
is refused by an offline `plan` and plans and applies connected, and retyping
`a` is refused by the edge. Also by the unit
`a_connected_plan_leaves_a_standing_computed_column_to_the_catalog`: the
catalog screen lets a standing column's input retype and its function's alter
through, the text screen refuses both, and a re-added column's call is refused
under either.

<a id="dec-1609-1"></a>

**DEC-1609.1. A partition's own default whose stored text would be its
parent's is refused at `plan --db`, from a deparse in a temporary table in a
transaction of its own that is always rolled back (#1609).**

The reader takes a partition's default as the parent's when its
`pg_get_expr` text equals the parent's (DEC-1578.1). A declared own default
written differently from the parent's but stored the same, `(1)` beside `1`,
or `('x'::text)` beside `'x'`, applies, then reads back as no default of its
own, and the apply's closing check refuses it after every statement ran.

Measured on 16.15 and 18.6:
- `(1)` and `1` are both stored as `1`; `'other'` on a text column is stored
  as `'other'::text`. Only the engine knows the stored text.
- A `READ ONLY` transaction refuses `CREATE TEMP TABLE`, so the deparse cannot
  share the spelling reads' transaction and gets its own, rolled back whatever
  happens.
- Without the `TEMP` privilege on the database `CREATE TEMP TABLE` is refused.
- An event trigger on `ddl_command_end` fires on `CREATE TEMP TABLE` and on
  each `ALTER`; its writes roll back with the transaction, but not all it
  does: one calling `nextval` left the sequence three calls on after the
  probe rolled back (#1669).

**The deparse.** Only a candidate is asked about: a partition's own declared
default on a column whose parent declares one, with a declared text that
differs. None, and no DDL runs. Otherwise one temporary table holds a column
pair per candidate, typed as the column is, and each pair gets the parent's
text under the parent's schema path and the partition's under its own, which
is the path the emitter writes each under, built by the emitter's own
`write_path` with `pg_temp` last, so an unqualified name resolves as it will
at the apply: without `pg_temp` on it the engine searches the temporary schema
first, and a name the probe's own table shares would reach that table
(#1659). The transaction first takes every parser setting the
apply pins (`session_pins!`), not only the deparse's: measured on 18, a
database's `transform_null_equals = on` stores `(false = NULL)` as `(false IS
NULL)`, which the apply, pinned off, stores as `(false = NULL::boolean)`. Equal `pg_get_expr` texts are refused, naming the
partition, the column, both texts and the remedy: drop the partition's own
default, it is the parent's.

**One statement, never a batch.** The declared text is the tail of an
`ALTER TABLE` sent through the extended protocol, which the engine refuses
when it holds a second command. Through a batch, a text such as `1; COMMIT;
CREATE TABLE …; COMMIT; BEGIN; SAVEPOINT …` ended the probe's transaction and
committed SQL of its own (measured on 18), while a connected `plan` is
read-only (SPEC §9.8). Such a text is unasked, with its warning.

**Why not `EXPLAIN` or `SELECT`.** The planner folds constants, so the text it
shows is not what `pg_attrdef` stores. Only storing the default gives the
stored text.

**No DDL under an event trigger.** Before storing anything the probe reads
`pg_event_trigger` for one on `ddl_command_start`, `ddl_command_end` or
`sql_drop` that is not disabled; with one, or with that read failing, it runs
no DDL and every candidate is unasked, naming the trigger (#1669).

**Unasked is not clean.** A candidate whose type cannot be named, an enabled
DDL event trigger, or a deparse that fails (no `TEMP`, an expression that does
not resolve yet because the plan creates what it names), is not a finding. It
is a warning that it was not checked before the plan, and the apply's
closing check still refuses a wrong recording. A connected plan reports it
where its other warnings go: on stderr in human output, and in `--format json`
as the warning finding `plan.partition-default-unasked` inside the one
envelope, with nothing beside it (SPEC §9.8; #1660). Pinned on 16 and 18 by
the CLI's `an_unasked_partition_default_is_a_warning_in_the_json_plan`.

Pinned on 16 and 18 by the CLI's
`a_partitions_own_default_stored_as_its_parents_is_refused_before_the_plan`:
- an own `(1)` beside the parent's `1`, and a new partition's `('x'::text)`
  beside `'x'`, refuse `plan --db` naming both and the remedy, write no plan
  file, and leave no temporary table behind;
- negatives: own defaults the engine stores differently (`2`, `'other'`, `'y'`)
  plan, apply and verify, and a row inserted into the partition takes its own;
  so does an own `(false = NULL)` beside the parent's `(false IS NULL)` under a
  database that sets `transform_null_equals = on`.
- a declared text that ends the transaction and creates a table commits
  nothing, and is warned about as unasked.
- an own `('pbps_1609'::regclass)::oid::bigint` beside the parent's
  `app.pbps_1609` reaches the schema's table, not the probe's, and is refused
  as the parent's (#1659).

Also by the CLI's `a_ddl_event_trigger_leaves_the_partition_default_probe_unrun`,
on 16 and 18: with an enabled `ddl_command_end` trigger calling `nextval`, the
plan leaves the sequence where it was and warns naming the trigger; disabled,
the pair is asked and refused as the parent's (#1669).

<a id="dec-1459-1"></a>

**DEC-1459.1. A connected SQL Server plan compares what an added computed
column calls with the functions it changes under the database's collation
(#1459; amends DEC-1460.1).**

**Context.** A computed column the plan adds, new or again, has no catalog
edge for its new expression, so DEC-1460.1 leaves it to the differ's text
screen. That screen folds case and nothing else. Under
`SQL_Latin1_General_CP1_CI_AI` a column re-declared as `([dbo].[cafe]([a]))`
calls `dbo.café`; with `café` altered in the same plan, the screen saw no
call, the column was re-added before the alter, and SQL Server refused the
alter inside the apply (3729).

**Decision.** `refuse_added_computed_calls` reads, from each added computed
column's expression, the two-part names it calls in code, a pair an opening
parenthesis follows (`Dialect::qualified_calls`), and asks the engine which of them name a
function the plan creates, alters or drops under the catalog collation, as
`column_names_alike` does for DEC-1431.1. A match refuses the plan by name:
"computed column dbo.t.c calls `dbo.café` as `dbo.cafe`". It reads no
`sys.sql_expression_dependencies` and needs no grant beyond the connection.

**Why not compile the expression.** Binding it by the engine, by adding the
column inside the planning transaction or on a scratch database, would be
exact, but `plan --db` would then take a schema lock on a live table or need
`CREATE DATABASE`. A computed column calls a function only by a two-part
name, so the names it calls and the collation that compares them decide
which function it calls. The maintainer chose this (2026-10-08).

**A column's method.** `geo.STAsText()` over a spatial column has a call's
shape too. Measured on 17.0 under `CI_AI`, where `[géo]` and `geo` are one
schema: while a function `géo.STAsText` exists the engine rejects the call as
ambiguous (Msg 327), and a function created after the column leaves the
table's computed columns unloadable (Msg 474). One dropped before the add is
gone, and the add binds the method. So a call whose first part is a column of
the table, as declared, is not refused for a function the plan drops, and
still is for one it alters or creates.

Only a column of a type with methods is such a receiver (#1677): `geography`,
`geometry`, `hierarchyid` and `xml`, all there are, since the model's type
catalogue has no user-defined CLR type. On an `int` column `xe`, the call
`[xe].[f]([a])` beside a dropped `[xé].[f]` is the function's, and is
refused; before #1677 any declared column exempted it, and the add failed
inside the apply. Pinned by the live
`a_call_on_a_column_without_methods_is_a_call_to_the_function`.

Pinned by the live `a_readded_computed_columns_calls_are_compared_under_the_collation`
(`crates/pbps-cli/tests/flow.rs`): under `CI_AI`, re-declaring the column as
`[dbo].[cafe]` while `café` is altered is refused at `plan --db`, and
re-declaring it to call `dbo.g` plans; adding `([geo].[STAsText]())` over a
geography column while dropping `[géo].[STAsText]` plans, applies and
verifies. Also by the units
`an_added_computed_column_is_refused_by_the_collations_reading_of_its_calls`
(another schema, a spelling the collation keeps apart and a view's drop
refuse nothing) and `qualified_calls_are_read_from_code_as_written` (no name
from a literal or a comment, across a call, or without a parenthesis, so not
a spatial column's property).

<a id="dec-1461-1"></a>

**DEC-1461.1. The rename search keeps a module drop the computed edges placed
among the drops in its place among them (#1461; amends DEC-1366.1).**

*Amended by [DEC-1680.1](#dec-1680-1): the search now has the edges' own
constraints, and the place is kept only for a module drop no edge orders.*

**Context.** DEC-1431.1's pass moves a function's drop to after the last
removal of a computed column that calls it, and so between two table drops
when the function is schema-bound to the second: `DROP TABLE dbo.u`,
`DROP FUNCTION dbo.g`, `DROP TABLE dbo.lookup`. Where a name only the catalog
holds made DEC-1366.1's search reorder the renames, the search put every
rename and drop of its region first and every other change after them, and a
module drop is neither. `dbo.g` then dropped after `dbo.lookup`, which SQL
Server refuses while `dbo.g` is bound to it.

**Decision.** A module drop placed after the region's first rename or drop is
one of the region's drops. The drops keep their order among themselves in the
search, so the function stays between the two tables, and a rename may still
move around it: a module drop claims no name, and the walk sees the name it
frees. A module drop the edges left
alone runs in class 0, before the first rename or drop, and keeps its place
outside the region.

**Why not the edges as constraints.** Handing the search the edges would
order the drops again, which the differ and DEC-1431.1 have already done, and
needs them carried from that pass into this one. The search's own rule, that
drops keep their order, places the module drop correctly once it is counted
among them.

**Limit.** That rule is stricter than the edges: the module drop is held
among *all* the region's drops, where the edges only need it after its
release and before a table it is bound to. Which name a rename's default
takes depends on what is free when it runs (DEC-981.1), so a function that
holds a default's generated name can need to drop later than its fixed place,
after the rename has taken the fallback. Such a plan is refused at
`plan --db` with DEC-1366.1's remedy, splitting it, and nothing runs. Giving
the search the edges' own constraints is #1680.

Pinned by the live `computed_function_drops_keep_their_place_when_the_renames_are_searched`
(`crates/pbps-cli/tests/flow.rs`): with a default adopted as `s1.c` forcing
the search, `dbo.g` drops between `dbo.u` and `dbo.lookup`, and the plan
applies and verifies. Also by the unit
`a_module_drop_among_the_drops_keeps_its_place_in_the_search`, whose negative
keeps a module drop ahead of the region out of it.

<a id="dec-1545-1"></a>

**DEC-1545.1. An ordinary managed table declared in its parent's `partitions:`
under its own name is attached and keeps its rows, provided it already has
its parent's columns; what it keeps of its own is brought to the declaration
in the same plan, and its column uids leave the ids file (#1545).**

**The declaration.** The maintainer chose it on the issue (2026-10-08). The
table's entry moves from its own file into its parent's `partitions:`, under
the same name and so the same table uid. This mirrors DEC-1544.1's detach.
No identity intent is added: the name does not change, so the declaration's
move, which git records, is the intent. Renaming and attaching at once stays
refused.

Measured on 16.15 and 18.6:

- `ATTACH PARTITION` makes the table's columns inherited (`attislocal` false).
  A table in its parent's column order then reads back as a partition the
  model holds.
- The engine refuses a column the parent lacks, another type or collation, a
  nullable column where the parent's is NOT NULL, a generated column where
  the parent's is plain or the reverse, an identity column on 18, and a
  parent's CHECK the table does not hold under the same name.
- A table in another column order keeps that order, which the reader does not
  hold.
- An index, a primary key, a unique constraint or a foreign key that matches
  one of the parent's is adopted as its clone, under the table's own name and
  whatever its storage parameters; an index whatever its sort order (`DESC`,
  `NULLS FIRST`), though not under another collation, operator class,
  `INCLUDE` list or method. Of several matches, the first by oid is adopted. A foreign key with other referential
  actions is not adopted. One the table lacks is built, under a name the
  engine chooses. A foreign key of
  the table's own stays its own, which no partition holds yet. A parent's
  CHECK on the table becomes the inherited copy, and the table's other checks
  stay its own.
- The table keeps its own defaults and is given none of its parent's.
- A row outside the range, or with a NULL in a key column, fails the attach
  (`partition constraint … is violated by some row`). So does a duplicate
  under the parent's key, a row the parent's foreign key does not find, and a
  row of the parent's DEFAULT partition inside the range.
- A column the table dropped before the attach stays `attislocal`.

**The shape**, refused by name (`AttachedShape`) as a change for an earlier
plan. The table must hold:

- its parent's columns, by name and in order, with the same types (in the
  dialect's spelling), collations, identities and generations; a description
  and a deprecation are annotations the catalog does not hold, and are not
  compared. A generated column is refused when the table is in another schema
  than its parent: the engine does not compare generation expressions as it
  attaches and keeps the table's, so the same text calling another schema's
  function would compute the column differently in each partition (measured
  on 18). The remedy is to move the table to its parent's schema first;
- NOT NULL wherever its parent's columns are;
- no primary key, or one on the parent's key columns;
- no unique constraint or foreign key that is not one of the parent's,
  matched one to one by definition, storage parameters aside as the engine
  matches them;
- each of the parent's checks under the same name. A check and a generation
  expression are compared as the engine compares them, by what they parse to.
  A connected plan reads that from the engine's own spelling of both standing
  objects (`diff_read_back`, the read-back before the recorded texts are put
  in its place): measured on 16 and 18, `n>0` and `n > 0` both read
  `CHECK ((n > 0))`, and the attach accepts them. Offline there is no such
  spelling, so they are matched by name and kind alone. An offline plan is
  never applied, and a refusal there would also keep it from writing the
  identities the connected plan needs;
- no replica identity, no `data:` and no other setting a partition does not
  hold;
- no trigger on it, and no foreign key of another table referencing it. A
  declared trigger is refused as the plan is made. An unmanaged one is not
  in the declarations under `unmanaged: ignore`, and the engine attaches
  over it, but the reader then refuses the whole tree, so a staged attach
  would commit before its checkpoint failed. The pre-flight therefore counts
  every non-internal trigger on the table from the catalog, before the first
  statement, as the reader's purity does (#1642 review).
- no column grant on it. A column-level grant is never declared (SPEC §5),
  so no plan removes one, and the engine keeps it as it attaches, measured on
  16 and 18, while the reader refuses a partition column that holds one. For
  the reason the triggers are counted, the pre-flight counts every column of
  the table with an ACL, whoever holds it (#1642 review). A revoke leaves the
  ACL NULL again.

The parent must be a partitioned table before the plan. A DEFAULT bound is
refused: attaching as the DEFAULT partition is #1639.

**What it keeps of its own** is compared with the declaration as a standing
partition's is (DEC-1581.1). The differ reads the table as the engine leaves
it after the attach:

- the indexes the parent's do not match, storage parameters aside, and the
  checks that are not the parent's;
- its persistence and storage parameters;
- a default whose text is not its parent's, or any default when the table is
  in another schema than its parent, where the same unqualified text may name
  another schema's object;
- a NOT NULL its parent's column does not have.

The table's kinds and `SetPartitionDefault`/`SetPartitionNotNull` then bring it
to the declaration: a drop in class 2 before the attach, everything else
after it. A column with no default where its parent's has one has nothing the
model can hold, so it takes its parent's back (`SetPartitionDefault` with the
fallback) unless it declares its own.

**Which index the engine adopts** is left to no guess. A plan does not know
which of several matches comes first by oid, and an adopted index keeps its
name as the clone. So for each of the parent's indexes, at most one of the
table's matching indexes is left: one whose name the declaration does not
give an index of its own. Every other match is dropped before the attach, and
one the declaration keeps is added after it. With none left, the engine
builds the clone under a name of its own choosing (`t_n_idx`, measured on
18). A declared name that the engine's choice then takes is #1558's class.
An index with an expression or a filter is never left for the engine to
choose, in any schema. The engine matches what each is bound to and how it
parses, which the text does not say: `n+1` and `n + 1` are one index to it,
and in another schema the same text may call another function. So every such
index is dropped before the attach, and one the declaration keeps is added
after.

**A key or unique constraint is not left to compete** with a parent's plain
index either. A parent's plain index adopts a matching index whether or not
a constraint stands on it, while a parent's key or unique constraint adopts
only one that has one. Measured on 16 and 18, a parent's `UNIQUE INDEX (a)`
made before its `UNIQUE (a)` takes the index of the table's `UNIQUE (a)`. The
constraint stays the table's own, and the parent's is built a clone beside
it, a tree the reader refuses. Which comes first is the oid order no plan
knows. So the table's key or unique constraint whose index a parent's plain
unique index could take is dropped before the attach, and the engine builds
both clones; `refuse_partition_changes` admits those two drops on a table
the plan attaches, and only there.

**A function rebuilt in the same plan** reaches the attached table's checks.
Measured on 16 and 18, a check the table holds as its parent's becomes the
parent's inherited copy once attached (`conislocal` false), which the engine
refuses to drop on its own and the parent's drop takes with it. So the
module's dependents leave it out (`its_parents_once_attached`): the parent's
own check, declared, is removed and restored around the rebuild, and both
reach the partition as they recurse. Only when the module's drop comes
before the attach is the table's copy removed by itself, and it is never
put back. A check the partition declaration keeps as its own is no copy:
it stays the table's across the attach and is woven as any declared check.

**The attach validates the parent's foreign keys** over the rows it brings,
as `ADD FOREIGN KEY` does. So the parents a plan attaches to are ordered
with the tables that receive rows (`supply_order`), each after every table
it references. An attach whose parent references a table the plan writes
rows into, or attaches a table to, runs among the row changes at (11, 1), at
its parent's rank. That puts it after what fills the tables its parent
references, and before the rows of a table referencing the parent. Its own alterations of classes 9 and 10 follow it to (11, 2). A
staged plan never holds both, being one logical change. Pre-flight reads a
parent's rows, once a table is attached to it in the plan, as its own and
that table's together, so a foreign key into the parent is not refused for
child rows only the attached rows satisfy.
`refuse_partition_changes` admits these changes on a table the plan
attaches.

**Identity.** A table that holds column uids and is declared as a partition
is the one being attached. Its column uids leave the ids file, and the file's
diff in git records it. This is not a drop: no row or column goes, so there is
no tombstone and no drop intent. The parent's uids are untouched, so none is
duplicated. A plan the differ refuses writes no ids file, so a refused attach
loses no uid. A later detach gives the columns uids again (DEC-1544.1).

**The change.** `AttachPartition { uid, table, parent, bound, shape }`, plan
version 32. It sorts in class 7 with a created partition's `CREATE`, after
the detaches and drops of class 6 that free a range. Its risk is
`constraint`: no row leaves, but the engine checks every row against the
bound and builds or checks the parent's keys and foreign keys over them. SQL
Server refuses it.

**The statement** is one `ALTER TABLE parent ATTACH PARTITION t FOR VALUES …`,
pbps's alone and every name qualified, so it runs on the empty path
(DEC-1564.1). The estimate says it reads every row and holds the table
exclusively. Avoiding the scan with a CHECK that implies the bound was left to
#1638 by the maintainer's choice.

**The pre-flight** asks two counts, before any statement:

- the table's rows outside the range or with a NULL key column, counted
  against the parent's key;
- the DEFAULT partition's rows inside the range, the probe a created
  partition already asks (DEC-1171.1).

A duplicate under the parent's key and a row the parent's foreign key does
not find are not counted. The engine refuses the attach on either, a
transactional apply changes nothing, and the `constraint` risk names the
hazard before approval.

**The apply's hold.** Once the table reads back as a partition, it is held to
the declared shape, as a created table is, and to the parent and range the
plan declares, which another session could change by detaching and attaching
it again. A staged read taken while statements remain, before the attach has
run, compares it as the ordinary table it was; once every statement has run,
an ordinary table there is movement. An attach the plan
leaves `UNLOGGED` is a new unlogged partition under the parent's referencing
keys, refused as a created one is (DEC-1595.1).

**The reader** skips a dropped column when it asks whether a partition's
columns are inherited. Dropping a column is the remedy the shape refusal
names, and the attach otherwise leaves a tree the model would not read
(amends DEC-1170.1).

Pinned on 16 and 18 by the CLI's
`a_table_is_attached_as_a_partition_through_the_cli`:

- an extra column is refused by name and the ids file is left unchanged;
- after the column is dropped, the attach is refused without its risk;
- the pre-flight refuses a row outside the range and a DEFAULT-partition row
  inside it, and nothing changes;
- once those rows are gone, the attach applies, `verify` is clean and the
  next plan is empty;
- afterwards the rows answer through the parent, the parent's default is back,
  the matching index is a clone, the dropped own index is gone, the own check
  stays, and the table's column uids are gone;
- a table already its parent's shape is attached in a staged apply.

Also by the live `an_attach_counts_the_rows_its_range_does_not_take`, which
pins a two-column key, NULL keys and `MINVALUE`/`MAXVALUE` against the
engine's own refusal, and by these unit tests:

- `an_ordinary_table_declared_as_a_partition_is_attached`;
- `an_attached_table_is_held_to_its_shape_once_it_is_a_partition`;
- `an_attached_table_keeps_the_records_of_what_stays_its_own`;
- `a_table_is_attached_in_one_unscoped_statement`;
- `a_partition_change_under_a_standing_parent_is_probed`;
- `an_unlogged_partition_under_a_permanent_key_is_refused_by_name`.

<a id="dec-1680-1"></a>

**DEC-1680.1. The rename search moves a module drop the computed edges ordered
between what it waits for and what waits for it, by the pairs DEC-1431.1's
pass hands it (#1680; amends DEC-1461.1 and DEC-1366.1).**

**Context.** DEC-1461.1 held such a module drop in its place among *all* the
region's drops, which is stricter than its edges. A function holding a
default's generated name can need to drop after a rename into that table's
name: run first, it frees the name, the rename's default takes it
(DEC-981.1), and a check claiming it collides. Its fixed place between two
table drops ruled out that order, and the plan was refused at `plan --db`.

**Decision.**
- `computed_order::drop_precedence` gives, after DEC-1431.1's pass, each pair
  of a change and a drop that waits for it. It uses `release`'s own rule,
  shared through one function: a function's drop waits for each removal of a
  computed column that calls it; a module's or table's drop waits for each
  drop of something schema-bound to it.
- Two waits are added to that rule, because a module drop can now move later.
  A computed column's drop waits for the drop of each module schema-bound to
  it: SQL Server refuses it while such a module stands. The class order runs
  a module drop (class 0) first, but DEC-1431.1's pass, and now the search,
  can move it later. A column's drop waits for the drop
  of each computed column of its table that reads it, which a computed drop
  moved after such a module could otherwise pass, out of the class order
  (2 before 5) that kept it first. `release` moves them as it moves the
  others.
- `order_computed_by_edges` returns the pairs. The plan pipeline passes them
  to the rename search. Nothing reorders the plan between the two passes.
- In the search, a module drop in the region moves like a rename only when
  all three hold:
  - a pair names it;
  - the catalog read found an object under its name, so its drop frees one;
  - a table rename can claim that name under the collation: its target, a
    transfer's stop, a moved default's generated or fallback name, or a
    carried child's name, which is the read's child of the renamed table
    itself, as `carried_destinations` reads it
    (`NameFacts::module_drop_matters`).

  The renames are the only claims among the drops. A claim after them runs
  after every module drop in any order, so no other place can differ.
  A module drop a pair ties to a movable one, directly or through other
  module drops, moves too. A function the movable one is bound to drops after
  it, and held fixed it would pin the movable one in front of it.
  A module drop ahead of the renames and drops (class 0) joins them by the
  same rule when a pair names it. One that must only precede a table it is
  bound to is not moved by DEC-1431.1's pass, yet a rename can need to run
  before it. Every other change ahead of them still runs first.
- A module drop that moves only through a pair, a follower, is never a
  choice of its own. It runs as soon as everything it waits for has run. It
  frees no name a rename claims, so no other place for it can clear what that
  place does not.
- This closes the class the review rounds kept finding: drops are free to
  commute in the walk, since a drop only frees names. So the only drops worth
  branching over are those a rename can tell apart. The search still branches
  over the renames, those module drops and the fixed drops' order against
  them, as DEC-1366.1 does, under its trial bound and its two-plan remedy. Made
  movable without this, a table's drop releasing a dozen functions
  multiplied the orders past the search's bound. That happened with names
  nothing held, and again with names only added checks claimed. A claim the
  list misses leaves the drop in DEC-1461.1's place. Every candidate, renames, these module drops and the region's next
  fixed drop alike, is tried only once everything it waits for in the region
  has run. A pair whose earlier change ran before the region is met.
- Any other module drop keeps DEC-1461.1's place among the drops. So does
  every drop when no edge was read.

**Why the pairs and not the edges.** The search orders positions in one plan;
the pairs say exactly which positions must precede which, by change, and the
catalog names and collation they were decided under stay in DEC-1431.1's pass.

Pinned by:
- `a_module_drop_moves_between_what_its_edges_order_it_after_and_before`
  (`crates/pbps-cli/src/object_order.rs`). It runs #1680's example, and has
  two negatives: without the pairs the plan is refused as before, and with
  the function schema-bound to the table dropped before the rename it is
  refused rather than moved past that drop.
- `a_module_drop_among_the_drops_keeps_its_place_in_the_search`, now also
  with its pairs.
- `a_module_drop_that_frees_no_name_keeps_its_place` and
  `a_module_drop_whose_name_no_rename_claims_keeps_its_place`: a table's drop
  releases eleven function drops. Their names are held by nothing in the
  first test and claimed only by added checks in the second. In both, the
  rename is still free to run first; made movable, the drops exhausted the
  search. The example test's negative keeps a function the catalog does not
  hold out of the movable set, and `a_transfer_claims_only_its_own_tables_children`
  counts only the renamed table's children as a transfer's claims.
- `a_module_drop_tied_to_a_movable_one_moves_with_it`: the example's function
  is bound to another dropped function, which moves with it to after the
  rename.
- `a_follower_module_drop_is_placed_not_searched`: a function the rename
  must run before, bound to eleven functions the plan drops. The followers are
  placed after it without branching; searched as choices, they exhausted the
  bound.
- `a_leading_module_drop_a_pair_names_moves_too`: a class-0 function that
  must only precede the table it is bound to moves after the rename, while a
  class-0 drop no pair names stays first. Without the pair the plan is
  refused.
- `what_a_released_function_is_bound_to_follows_it`
  (`crates/pbps-cli/src/computed_order.rs`), on the pairs themselves.
- `a_computed_drop_follows_the_module_bound_to_it_and_its_inputs_follow_it`
  (the same file). It orders a table's drop, then the function it releases,
  then a computed column the function is bound to, then the column that
  computed column reads, and checks their pairs. Its negative: unbound, both
  drops keep their place. The search test above also refuses to move the
  function past a computed drop bound to it.

<a id="dec-1663-1"></a>

**DEC-1663.1. The partition-default probe runs only on a catalog no
untrusted role can change, and tells a failure by its SQLSTATE, never the
engine's words (#1663, #1707; amends DEC-1609.1).** Storing a declared
default parses it as the deployer. Measured on 16 and 18, the parse runs
neither a domain's CHECK for a literal of the domain itself nor a user
cast's function. It does run a domain's CHECK when it reads a literal as a
composite, array or range over that domain: `record_in` and `array_in` call
the element's input function, which for a domain is `domain_in`. A role
with `CREATE` in any schema can own such a domain, and a check that calls
its PL/pgSQL function ran with the deployer's privileges and raised a table
only the deployer could read. The rollback took back no read, and the error
text carried it to the output.

The issue offered two fixes: run the probes as a role with no data access,
or refuse types whose functions an untrusted role owns. The first needs a
role pbps would provision in every target, with `USAGE` on whatever schemas
the declared texts name. The second, as written, reads the wrong functions.
Measured, a non-superuser cannot create a base type or a shell type, so
neither a range's `canonical` nor a type modifier function. Every input
function the parse reaches is a superuser's; what another role can write is
a domain's CHECK.

A check present when the probe looks is not the only one it can meet.
Reading the dangerous kinds of object failed five ways in review (#1706):
between the read and the store, measured on 16 and 18, the owner of a
domain with no check can add a `NOT VALID` one; the owner of a composite or
a table can give it a column of a new domain; a role that can create in a
schema, or create a schema in the database, can create the type a declared
text names; a role holding `ADMIN` on another can grant it with `SET` to
one of its own; and a member that only inherits the deployer was taken for
it, though `current_user` tells them apart under a row security policy.
Catalog lookups see what other sessions commit mid-transaction, and nothing
locks a type against `CREATE` or `ALTER`, so no read of the present state
holds until the store. The rule is instead that the catalog is closed
(#1707): a role is trusted when it is a superuser or can `SET ROLE` to the
current role or to a superuser, and from every other role the probe walks
`pg_auth_members` along memberships granted with `SET`, `INHERIT` or
`ADMIN`. When anything a walk reaches is the current role or a superuser,
owns any object in this database (a `pg_shdepend` owner row, whatever the
object's kind), or holds `CREATE` on the database or on a schema, nothing is
stored and each pair stays unasked with a warning naming the untrusted
roles, as under an event trigger (#1669); the apply's closing check still
refuses a wrong recording. Predefined roles act only through their members,
so `pg_database_owner` counts only under another database owner. The walk
is wider than any one role can use, and it counts a concurrent session's
temporary objects; both only leave the probe unrun more often, never run it
where it should not. Under the single-deployer assumption (SPEC §7.6), a
database pbps manages alone keeps the probe. A function a trusted one calls
by name inside its body is not seen; that body is its trusted owner's to
write, and the apply would run it all the same.

Another session's temporary schema is outside that rule: any role with
`TEMP` on the database, `PUBLIC`'s by default, creates objects there, and
the role can empty it before the read and fill it after. Counting `TEMP`
would leave the probe unrun in nearly every database with a second role.
Measured on 16 and 18 instead, those schemas belong to the bootstrap
superuser with no grant, so a non-superuser is refused `USAGE` on one, and
no `search_path` searches another session's. Only a superuser reaches one,
and only by naming it. So a pair whose declared texts or column type
contain `pg_temp`, `pg_toast_temp` or a `U&` escape, in any case, stays
unasked with a warning (#1706). The text is read, not the parse, since the
parse is what runs the check. An identifier cannot be split by a comment or
quoting, and `U&` can spell one without its letters. A literal that only
mentions the name costs a warning, never a wrong answer.

A failure inside the probe is reported as "the engine refused it (SQLSTATE
22P02)", never the server's text. That text is whatever the code the parse
reached chose to raise, with what it read. The row and bound spelling reads
(DECISIONS 101, 106; DEC-1170.1) cast only to the types pbps spells, under
an empty `search_path`. They run built-in input functions only and keep
their text.

Pinned on 16 and 18 by the CLI's
`an_untrusted_domain_check_leaves_the_partition_default_probe_unrun`:

- a role's domain whose check counts its runs and raises a deployer-only
  table, reached through a composite literal in a declared default, leaves
  the pair unasked with the role named; the check never runs and the
  table's contents appear nowhere in the output;
- each alone leaves the pair unasked with the role named (#1707): a domain
  with no check, a composite and a table it owns in a schema it can no
  longer create in; `CREATE` on the database; `ADMIN` on the deploying role
  without `SET` or `INHERIT`; `INHERIT` on it without `SET`;
- without it, and beside a domain the deployer owns that calls only
  built-ins, the pair is asked and refused as the parent's;
- a default naming a composite over a domain in another session's
  temporary schema, held by the deployer so the catalog is closed, leaves
  the pair unasked, and that check never runs (#1706);
- a default the engine refuses is told as `SQLSTATE 22P02`, without the
  server's words.

The text check's spellings are pinned by
`a_text_that_could_name_a_temporary_schema_is_told_by_any_spelling` and
`a_text_naming_no_temporary_schema_is_still_asked`.

<a id="dec-1687-1"></a>

**DEC-1687.1. A standing range-partitioned parent's columns change as a
table's do, the engine recursing each change into every partition, and each
partition's own default and NOT NULL follow the parent's change on their
column (#1687).**

The first of #1546's four slices (leon, 2026-10-08): columns here, then
indexes (#1688), keys, checks and foreign keys (#1689), and renaming the parent
(#1690). Until DEC-1170.1 every change to a partitioned parent was refused.

Measured on 16.15 and 18.6, on a populated tree:

- `ADD COLUMN` (with a default, NOT NULL, or generated), `DROP COLUMN`,
  `ALTER COLUMN … TYPE` and `RENAME COLUMN` on the parent recurse into every
  partition, under `AccessExclusiveLock` on each. No partition can add, drop,
  retype or rename an inherited column alone.
- A partition-key column cannot be dropped or retyped. It can be renamed, and
  the key follows it (`RANGE (ts)` becomes `RANGE (at)`).
- `SET DEFAULT` and `DROP DEFAULT` on the parent recurse and **overwrite a
  partition's own default** (DEC-1578.1). `ONLY` leaves the partitions alone,
  but then the partitions created earlier lack the parent's default, which the
  reader's tree purity refuses.
- `SET NOT NULL` recurses. `DROP NOT NULL` recurses too, and a partition's own
  NOT NULL that predates the parent's **survives on 18 but not on 16**. A NOT
  NULL a table brought to its attach does not survive it on either version.
- A partition's own default follows a column rename.

**What is planned.** On a parent that stands before and after the plan, the
following are admitted with no new change kind: `AddColumn`, `DropColumn`,
`RenameColumn`, `AlterColumnType`, `AlterColumnNullability`,
`AlterColumnDefault` and `SetColumnDeprecated`. These stay refused by name:
- a key column's drop or retype;
- an identity column added, whose partitions cannot be read back yet (#1681);
- a generation expression changed in place.

A key column renamed is not a change of the partitioning.

**The partitions' own** (`diff_partition_columns`), compared through the
parent's renames and drops in the same plan:
- **A rename** carries a partition's own entry with it, and plans nothing.
- **A drop** takes the partition's own entries on that column, and plans
  nothing.
- **A default the parent sets or drops** is followed by each declared own
  default on that column, set again. The connected pass
  `after_their_parents_defaults` (#1588) already did this for a `SET`, and
  `DROP DEFAULT` is the case it does not cover.
- **A NOT NULL the parent drops** is followed by every partition brought to
  its declaration: its own NOT NULL set, or dropped where 18 would keep one
  from before.
- **A NOT NULL the parent sets** holds every partition, so a partition's own,
  which validation then forbids the declaration to keep, is left to it rather
  than dropped after.

These partition changes sort at (9, 4), after their parent's change of class 9.

**Why not drop the partition's own NOT NULL before the parent's tightening.**
It would leave no local NOT NULL behind on 18, but it orders a partition
change before its parent's in a class that otherwise follows the parent. And
the leftover is harmless until the parent drops its NOT NULL again, which the
rule above already handles.

**The apply guard.** The undo of a plan's column renames (`Renames::apply`)
renames a parent's key and a partition's own column entries too, so both
reads compare under one set of names. A partition of a parent whose column the
plan drops or makes NOT NULL is held field by field, to what the parent's
change leaves it. Otherwise it would be compared whole and called moved.

**The cost estimate** names the recursion. On a partitioned table it says the
statement recurses into its partitions, how many, and the lock it takes on
each. Rows and rewrite stay unknown, since ADR-0012 did not measure
partitioned tables.

Pinned by:
- `a_partitioned_parents_columns_change_and_its_partitions_keep_their_own`;
- `a_partitions_own_column_overrides_are_its_own_through_a_detach`;
- `a_partition_key_and_a_partitions_own_columns_follow_the_parents_renames`;
- `a_partitions_own_columns_set_by_someone_else_are_movement`;
- the CLI's `a_partitioned_parents_columns_change_through_the_cli`, on 18 and
  on 16.
