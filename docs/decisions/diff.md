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
    numbered, joined to themselves under `COLLATE DATABASE_DEFAULT` on
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
differ emits separately, move too. Those carry the doomed table's name, which a
rename into that name also carries once it has run, so they keep the doomed
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
