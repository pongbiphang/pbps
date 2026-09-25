# `doctor`

Which permissions `doctor` asks for, and on which securables. Part of the
[decision record](../DECISIONS.md), which says how to add an entry here.

<a id="decision-38"></a>

38. **`doctor` reimplements nothing and writes nothing.** It calls
    `validate_findings`, the same function `validate` runs — a readiness command
    that disagreed with `validate` would be worse than one that never looked.
    Permissions are *asked for* (`sys.fn_my_permissions`, `HAS_PERMS_BY_NAME`),
    never tried, and named one by one with what each is for **and at the
    securable where it is needed**: the four `CREATE`s at the database (they
    cannot be granted lower), `ALTER` / `VIEW DEFINITION` and the probes'
    `SELECT` per **managed** schema, and `INSERT` / `DELETE` plus the ledger's
    own `SELECT` on the ledger and lock *objects* (falling back to their schema
    only until those tables exist), plus `ALTER` on the ledger's schema **while
    the ledger does not yet exist** — `CREATE TABLE` at the database does not by
    itself let an account create a table in a schema. `SELECT` is listed twice on purpose: the
    probes read managed tables and the ledger read is two tables in `dbo`, and
    one entry made the wrong demand in both directions. The ledger's schema is
    **not** forced into the managed set — a project managing only `app` never
    touches a `dbo` table. Asking at database scope alone reports gaps a
    least-privilege account does not have, and the remedy it then invites is
    exactly the "make it db_owner" this list exists to avoid. A securable that
    does not exist yet is left unasked — that is every first deployment, and the
    create-time `ALTER` is required per ledger table still missing, not once for
    the pair. `REFERENCES` is on the list because a foreign key is authorized on
    the *referenced* table and `ALTER` does not imply it — and when that table is
    **outside the managed schemas**, `REFERENCES` and the probe's `SELECT` are
    asked on the object itself (`Needed::Referenced`), because nothing asked
    about the managed schemas can see it. That query deliberately omits the
    `OBJECT_ID` existence filter the ledger's uses: metadata visibility cannot
    tell absent from invisible, and here dropping the object would under-report
    instead of falling back. **`CONTROL` is
    deliberately absent**: a cross-schema rename needs it (`ALTER SCHEMA ...
    TRANSFER`), but `doctor` sees no plan, so demanding it would require
    near-ownership of every managed schema always — the claim is narrowed to
    "most changes" instead, and the real check belongs in the plan-aware
    pre-flight. A declared schema the database **lacks** is a readiness *error*
    (pbps never emits `CREATE SCHEMA`), which is a different question from
    leaving it unasked for permissions. `CONTROL` on the database is also **not
    a shortcut past the list**: the inputs are `HAS_PERMS_BY_NAME` answers,
    which already account for inheritance, so an owner comes back clean without
    one — while a `DENY` at a narrower securable beats an inherited `CONTROL`,
    still leaves `CONTROL` listed in `sys.fn_my_permissions`, and really does
    make the DDL fail. Returning early on that one signal called such an
    account ready.

<a id="decision-62"></a>

62. **`doctor` asks for the role permissions only of a project that declares a
    role.** *Widened by 68: "declares" became "has", and the securables
    include what the roles hold live.* `CREATE ROLE`, `ALTER ANY ROLE` and `CONTROL` on every granted
    securable are security-shaped, and the list refuses over-demand everywhere
    else — but an account short of them fails on the first `GRANT` of a
    project that does declare roles, after `doctor` said ready. So `REQUIRED`
    gained its first entries that depend on the declarations: `Needed::RoleAdmin`
    is switched off by `Held::roles_declared`, and `Needed::Granted` is asked
    per object and per schema the roles are granted on, the way foreign-key
    targets outside the managed schemas already are. `CONTROL` is what is
    asked for, because `HAS_PERMS_BY_NAME` cannot ask "held with grant option".

<a id="decision-69"></a>

69. **`doctor` asks about what the managed roles hold live, not only what the
    declarations grant.** A revision that removes a role's last grant, or the
    role, plans a `REVOKE` or a `DROP ROLE` whose securable the declarations
    no longer name — so `grant_targets` was `None` and `doctor` said ready to
    an apply that then failed on the `REVOKE`. The managed roles (declared, in
    the ids file, or held by the environment's recorded state — never the
    tombstones, which are permanent and would keep the requirements on for
    a drop applied years ago) are asked about, and the connected check
    reads their grants from the **recorded state** first
    — that is what the next plan revokes against, and the ledger is readable
    by any account that can deploy — and from `sys.database_permissions`
    second, for grants adopted by hand. The catalog alone was tried first and
    measured useless: metadata visibility hides a securable from an account
    with no permission on it, which is exactly the account `doctor` is
    checking, so the live query saw nothing precisely where the gap was.

<a id="decision-134"></a>

134. **A permission on a schema is probed before the plan runs.** `validate`
    accepts a schema target it cannot see inside — an external schema has no
    declared objects — and this tool never creates a schema, so a grant on
    one the database does not have is a statement the engine refuses. Under
    `apply --staged` every change before it has committed by then, including
    the `CREATE ROLE`. The probe counts one for a schema `SCHEMA_ID` cannot
    find, which lets the engine decide what one name is, here as everywhere
    else. A `REVOKE` names its securable the same way and is probed the same
    way. An *object* target needs no probe: it is declared, so it exists or
    this plan creates it.

<a id="decision-289"></a>

289. **`doctor` asks about ownership on this engine, because no privilege
    authorizes DDL.** Measured on 18.6, as a role holding
    `GRANT ALL PRIVILEGES ON own.t`:

    ```text
    has_table_privilege('own.t', 'SELECT,INSERT,UPDATE,DELETE,REFERENCES,TRIGGER')  ->  t
    ALTER TABLE own.t ADD COLUMN c int   ->  42501: must be owner of table t
    CREATE INDEX ix_t ON own.t (v)       ->  42501: must be owner of table t
    DROP TABLE own.t                     ->  42501: must be owner of table t
    ```

    Every privilege the engine has to give, held, and not one statement a plan
    is made of could run. The SQL Server list ported across would have asked
    `has_table_privilege` about a vocabulary that is real here, got `true` for
    all of it, and reported an environment ready that cannot alter a single
    table — the under-demand `pbps_mssql::doctor::Needed` exists to remove,
    arriving through the front door. The question is
    `pg_has_role(current_user, relowner, 'USAGE')`, which is the one the engine
    asks itself, and the remedy is `ALTER TABLE ... OWNER TO` or
    `GRANT <owner> TO <deployer>` rather than a `GRANT` on the table.

    The scopes are this engine's for the same reason: a schema takes `USAGE` and
    `CREATE` and nothing else, there is no database-scoped `CREATE TABLE`, and
    `CREATE` on the database is deliberately not demanded — the emitter never
    writes `CREATE SCHEMA`, so an absent managed schema is reported as absent
    (as on the other engine) rather than covered by a grant that also permits
    creating any schema at all.

<a id="decision-294"></a>

294. **`doctor` asks for `USAGE` on a schema wherever objects in it are used,
    not only where they are created.** `Needed::LedgerCreation` asked for
    `CREATE` on the ledger's schema while the ledger did not exist, and
    `Needed::Ledger` asked for the DML on the two tables once it did — and
    between them nobody asked whether the role could enter the schema at all.

    Measured on 18.6, with `USAGE` on `public` revoked from a role holding
    `SELECT`, `INSERT` and `DELETE` on both tables: `has_table_privilege`
    answers **`t`** — the question is asked by oid and never resolves the name —
    while every statement naming the ledger is `42501: permission denied for
    schema public`. This is 289's shape a second time, and from the same
    direction: a privilege question with a true answer, about something the
    engine decides elsewhere.

    Swept for, as CLAUDE.md asks, and found again one securable out: a foreign
    key into `shared.parent` needs `USAGE` on `shared`, and nothing asked about
    the *managed* schemas can see that. `Needed::ReferencedSchema` is that
    entry. The same sweep found the plain omission beside it — `Needed::
    Referenced` asked for `REFERENCES` and not for the `SELECT` the probe for
    that key performs, which the SQL Server list has carried since it was
    written.

<a id="decision-428"></a>

428. **PostgreSQL doctor distinguishes data privileges from grant authority.**
    Ownership permits DDL and carries grant options, but an owner can revoke
    its own INSERT and still own the table. The shared doctor demand therefore
    carries the data columns and exact grant targets with their permissions;
    PostgreSQL checks effective DML privileges separately from each required
    `WITH GRANT OPTION`. Column grants cover INSERT and UPDATE when every
    emitted column is covered. Ensure, empty exact, and key-only declarations
    ask only for the writes they can produce, plus SELECT for readback.
    SQL Server retains its existing object-level CONTROL demand.

    Declared and recorded grants, and current ACLs of managed roles, all supply
    grant demands: removing a declaration still requires authority to REVOKE.
    Schema, relation, and routine namespaces remain distinct, and signatures
    distinguish routine overloads. Queries use catalog OIDs and bound names so
    an inaccessible schema is reported instead of making name resolution fail.
    Absent securables have no privilege gap; their absence has a separate remedy.
    The live doctor tests pin revoked owner DML, column grants, an empty exact
    block, same-named view/routine overloads, inherited grant options, and catalog
    grants absent from declarations. The CLI regression pins the complete
    declaration-to-JSON path and the named remedies (issues #304 and #317).

<a id="decision-439"></a>

439. **`doctor`'s object-scope permission questions are resolved against the
    environment's own recorded name, not the declared one.** `managed_schemas`,
    `referenced_tables`, `grant_targets` and `data_tables`
    (`crates/pbps-cli/src/doctor.rs`) build every object name `doctor` asks
    about from the declarations. Until `apply` reaches a given environment, a
    table this plan renames still carries its *old* name there, and SQL
    Server keeps a `GRANT` or a `DENY` with the object through `sp_rename`
    (measured on the pinned image, matching `deploy.rs`'s own reliance on
    that fact) — keyed by `object_id`, not by name. Asking `HAS_PERMS_BY_NAME`
    under the declared name therefore found nothing there: `Needed::DataInsert`
    /`DataUpdate`/`DataDelete` and `Needed::Granted` silently fell back to the
    schema, reporting a gap a careful DBA's object-level grant did not have,
    and missing an object-level `DENY` that really blocked the deployment
    (#133).

    The fix resolves per uid, not per file. The **project's ids file** is the
    identity of the *declared* world and says nothing about how far any one
    environment has got — two environments routinely sit at different
    points — so it is the wrong side to resolve *against*. The
    **environment's own recorded `StateSnapshot::ids`**, read from the same
    `state::latest` call `pbps_mssql::doctor::permissions` already makes for
    the role question, is the right one: declared name -> uid comes from the
    project's ids file, uid -> this environment's current name comes from its
    own recorded ids. `IdsFile::resolved_in` (`pbps-model`) does the lookup
    and is shared with `deploy.rs`'s own resume-time name resolution
    (formerly a private `live_name`), which already depended on exactly the
    same two-map shape.

    Three cases fall through to the name asked with, deliberately, rather
    than reading as "holds nothing": no uid for the declared name (an object
    added since the last `plan` — nothing has been applied for it anywhere),
    a uid the project has but the environment's recorded ids does not (this
    environment has never had the object), and an unreadable ledger (already
    a gap of its own, reported by the ledger permission rows; the empty
    `IdsFile` this falls back to has no uid for anything, so it takes the
    same path as "never had it" rather than this resolution inventing a
    distinction it cannot tell apart). `referenced` is left unresolved: those
    tables lie outside the managed schemas, and pbps never renames an object
    it does not manage. This PR is scoped to `pbps-mssql`, the dialect the
    issue measured; the same shape may recur in `pbps-pg`'s doctor, which is
    tracked separately rather than fixed here unmeasured.

    A live test grants `INSERT` on a table's current name alone (nothing
    wider) against declarations naming its rename destination with the
    project's ids file already pointing there, and requires the account read
    ready rather than gapped; the converse grants the whole schema with a
    `DENY` on the current name and requires the gap land on that object, the
    `DENY` really refusing the statement, and the refusal surviving the
    `sp_rename` itself. A unit test on `IdsFile::resolved_in` pins the
    resolution preferring the environment's recorded name over the declared
    one and falling back to the declared name when it cannot resolve, for
    each of the three reasons above.

<a id="decision-440"></a>

440. **`pbps_pg::doctor`'s `Needed::Referenced` falls back to the *declared*
    columns a key names, not the target's whole catalog.** 294 asked
    PostgreSQL for `REFERENCES` and `SELECT` on the whole of a foreign key's
    referenced table, outside the managed schemas. PostgreSQL grants both of
    those **per column**, and a key needs them only on the columns it names.
    Measured on 18.6, with `GRANT SELECT (id), REFERENCES (id) ON
    shared.parent` and nothing wider:

    ```text
    has_table_privilege ('shared.parent',      'REFERENCES') -> f    has_table_privilege(..., 'SELECT') -> f
    has_column_privilege('shared.parent','id', 'REFERENCES') -> t    has_column_privilege(...,  'SELECT') -> t
    CREATE TABLE app.child (id integer PRIMARY KEY, pid integer REFERENCES shared.parent(id))  -> CREATE TABLE
    ```

    So a role granted exactly what the key uses deployed, and `doctor`
    reported two gaps against it and exited 2 — the over-demand `Needed`
    exists to remove, one securable further out, and in the direction that
    pushes an operator towards a wider grant on somebody else's table (issue
    #215).

    The fallback asks after the *declared* columns — `references_columns` on
    each foreign key, unioned per target across every key that points there
    (`pbps-cli::doctor::referenced_targets`) — not the catalog's full column
    list. `pbps_mssql::doctor::Columns::Declared` is the same choice for
    `Needed::DataUpdate`, and for the same reason: `doctor` never sees a plan,
    so the catalog's list would include columns the key never names, and an
    account granted exactly the key's own columns would still see a gap
    against a column no statement will ever touch. The one case this fallback
    does not reach is `Needed::ManagedTable`'s `SELECT` on a table this
    project *does* manage: `doctor` has no declared column list for it either
    — the pre-flight probes it is asked for run against a *plan*, which
    `doctor` never sees — so covering it would mean reading the catalog's
    whole column list the way `pbps_mssql::doctor::Columns::Catalog` does for
    its own `Needed::Referenced`, and that plumbing is not built here. Left as
    a known gap rather than invented unmeasured (tracked in #215's own
    discussion, step 9, #84).

    A column the fallback asks about is looked up by **attribute number**, not
    by name: `has_column_privilege(oid, name, priv)` raises `column "x" of
    relation "y" does not exist` for a name the table does not actually carry,
    and a referenced table's declared columns are not validated against
    reality the way a managed table's are. Joined through `pg_attribute` and
    called as `has_column_privilege(oid, attnum, priv)` instead (measured on
    18.6): a `LEFT JOIN` miss leaves the attribute `NULL`, and the attnum
    overload answers `NULL` rather than raising for `NULL` in either
    argument — read as "not held", the same rule this module already applies
    to every other NULL the server returns, rather than failing the whole
    readiness read over one stale reference.

    The rescue is "every named column, or the object-scope gap stands": each
    column a key names must confirm the permission before the object-scope
    report is dropped, so a grant covering only part of a composite key still
    reports the gap. Unit tests pin a `TableRights` holding no table privilege
    but covering every named column at no gap, and one column short of that
    at the gap standing; a live test reproduces the measurement above as the
    least-privilege role itself, asserts `doctor` reports nothing, and then
    creates the key as that role — the check and the engine pinned to the
    same answer.

<a id="decision-466"></a>

466. **SQL Server foreign-key readiness uses the declared target-column union
     (issue #195).** The shared doctor request already carries this union for
     PostgreSQL (440). SQL Server now consumes the same map rather than asking
     `sys.columns` for every column of each external target. `REFERENCES` covers
     the emitted key and `SELECT` its preflight probe; neither names an
     unrelated target column. Targets within managed schemas retain their
     existing classification, and managed-table, ledger, role and row-DML
     permission demands retain their respective column policies.

     An object-level grant remains sufficient. Otherwise each named column
     must answer 1 for the requested permission: 0 or NULL is a gap, and an
     empty subset supplies no column-level evidence. Target and column names
     remain bound values. The statement packer counts one parameter per named
     column as well as the target's two name parts, so unions across many keys
     remain within the existing RPC budget without dropping a target.

     Measured on SQL Server, a least-privilege account granted only the two
     referenced columns can execute both emitted foreign keys and their
     preflight probes while an unrelated column remains unreadable. Revoking
     one required REFERENCES or SELECT grant produces the corresponding gap
     and makes the real statement fail. Live cases also cover unknown and
     hostile column names, absent targets and empty subsets. The CLI regression
     checks both members of the union and repeated references to one column;
     the packing test covers a union too wide for one statement.

<a id="decision-467"></a>

467. **SQL Server doctor asks managed probe reads on each table (issue #194).**
     Measured on SQL Server 17.0.4075.5, complete column-level `SELECT` grants
     answer zero at both schema and object scope while the probe's column
     reads succeed. The schema question therefore overstates the required
     grant. The opposite scope mismatch matters too: an object-level `DENY`
     defeats a schema grant and the read fails despite a positive schema
     answer. Readiness now uses the existing object-or-all-catalog-columns
     permission question for the managed tables already carried by `Ask`.

     The shared input and PostgreSQL behavior do not change. SQL Server uses
     the supplied table names and resolves them against the environment's
     recorded identities before querying; recorded tables awaiting removal
     are also asked about under their existing names. A new declaration reusing
     a departing table's name retains its own creation-schema fallback, so it
     cannot inherit the old identity's object grant. Existing tables use their
     object answer even when a schema grant would give a different answer.
     Absent tables use the schema's answer, deduplicated per securable; absent
     schemas remain separately diagnosed. No catalog table outside the managed
     declarations or recorded state is added merely for sharing a schema.

     `doctor` sees declarations, not a plan. This checks every catalog column
     of a managed table, as the existing object-scope reader does, rather than
     inventing a plan-specific subset. Foreign-key targets retain the declared-column union
     from decision 466. The CLI live regression checks complete and
     incomplete column grants, object grants and denials, the absent-table
     schema fallback, and actual permitted/refused reads under the deployment
     login. Unit cases preserve distinct rename/reuse demands and recorded
     tables without turning an absent schema into an invented grant target.

<a id="decision-483"></a>

483. **Doctor distinguishes grant options from adopted ACL revocation authority.**
     The PostgreSQL diagnostic retains each catalog ACL's original grantor
     (#329). An unrelated grant option authorizes a new GRANT but cannot revoke
     that ACL. Measured on PostgreSQL 18 and 16, even the owner/superuser's
     REVOKE leaves an independent grantor's entry intact. An inherited original
     grantor works when no competing direct option selects the deploying role.

     Each possible removed privilege needs the same original grantor selected
     by the connected role. Owner/superuser selection, direct grant options and
     a unique effective inherited grantor are recognized. Competing inherited
     choices are reported with the original grantor named and explicit role
     selection advised: PostgreSQL does not promise which inherited path wins.
     Multiple original grantors on one grantee/target remain a gap because one
     emitted REVOKE cannot combine their authority. This is a read-only readiness
     diagnosis (SPEC 14.1), not a grantor addition to the declaration or plan.

     Adopted object ACLs use the differ's managed boundary: declared and ids-file
     tables, declared modules and the environment's recorded identities (#332).
     Exact routine signatures stay separate from relations; schema ACLs remain
     declarable outside that object scope. Declared/recorded explicit grants
     still demand their own authority. An absent external schema grant target
     joins the existing schema-absence diagnosis before any privilege gap (#333).

     Routine lookups and adopted signatures use introspection's canonical
     settings (#330). The existing transaction/savepoint scope restores the
     caller's search path on success and error. A visible user-defined type
     therefore cannot hide its routine's grant demand by losing qualification.
     Live controls assert ACL effects, overload and scope negatives, recorded
     targets and schema-presence transitions; CLI JSON retains the quoted
     CREATE SCHEMA remedy.

<a id="decision-484"></a>

484. **Doctor resolves data permissions by identity and predicts new-table ACLs.**
     The connected demand keeps INSERT's union of explicitly supplied cells
     separate from UPDATE and full data readback (#331). A missing cell uses
     its default or NULL without demanding INSERT on that column. Explicit
     NULL is still a supplied cell; mixed rows demand the union. Ensure/exact
     and key-only statement rules remain unchanged.

     PostgreSQL receives the same project ids already supplied to SQL Server.
     Table and column uids resolve against this environment's recorded mapping
     before permission queries (#334, #335, #382). An unrelated object at the
     declared destination cannot supply the old identity's ACL. A new identity
     reusing a recorded name cannot borrow the departing object's rights.
     Diagnostics name the current securable so a grant can be applied before
     the rename. The same table resolution covers managed ownership/SELECT,
     explicit object grants and adopted ACL scope; recorded grant demands
     already name current objects and are retained unchanged.

     Missing recorded objects report unknown authority rather than falling
     through to new-table defaults. Unreadable recorded identities propagate
     an error; no ledger and an empty ledger retain their existing diagnoses.
     No identity intent is inferred, and neither saved formats nor execution
     rules change. This remains SPEC 14.1's read-only readiness report.

     Truly new data tables use the current deployment role's global default
     table ACL, or PostgreSQL's built-in ACL when no global entry exists, plus
     that role's schema-specific additions (#336). A membership's own default
     ACL does not apply to objects this role creates; ACL grants to inherited
     roles and PUBLIC still supply effective ordinary rights. Ownership keeps
     grant options but does not restore self-revoked ordinary DML privileges.
     PostgreSQL 18 and 16 measurements and emitted INSERT controls pin the
     revoked-global, additive-schema, inherited-recipient and PUBLIC cases.

     Separate live regressions pin each issue, including same-name decoys,
     missing recorded objects and names freed for new identities. CLI JSON
     tests mint pending table/column ids through plan and verify both covered
     column grants and a revoked UPDATE. Managed-table catalog-wide SELECT
     coverage (#392) remains a separate demand from declared data readback.

<a id="decision-505"></a>

505. **`doctor` asks for the delete count's policy-catalog read, at the
     database and on each securable an effective metadata `DENY` sits on;
     `VIEW SECURITY DEFINITION` is not what decides it (issues #522, #524).**
     468 made SQL Server's shared preflight/execution count prove the row-level
     security policy catalog readable *before* it discovers referencing keys,
     because a parent-only deployer can see neither the foreign key nor the
     filtered child its `DELETE` would still cascade into. `doctor` asked
     `VIEW DEFINITION` only on the **managed** schemas (SPEC §9.5,
     `Needed::Managed`), and a security policy can live in a schema the project
     neither manages nor declares. So an account holding everything `doctor`
     listed passed readiness and met `Cannot count referencing rows: inspecting
     row-level security policies requires database VIEW DEFINITION.` at its
     first `mode: exact` delete. The count fails safely — this was a readiness
     diagnostic gap, not an accepted destructive plan — which is exactly what
     the command exists to remove.

     **Two requirements for one proof**, because the two halves are missing in
     two different places and a `GRANT` fixes only the first.
     `Needed::DeleteCatalog` is the database-wide `VIEW DEFINITION` the count
     asks for outright. `Needed::DeleteCatalogDenied` is the count's own second
     check: an **effective** object or schema metadata `DENY` reaching this
     principal directly or through a role. A database grant loses to one (460),
     so reporting the database permission alone would send an operator to grant
     something they already hold. Each is reported on the securable that
     carries it, which is both the truth and the remedy.

     **Demanded only of a declaration that removes a row**, for the reason
     `Needed::RoleAdmin` gives: database-wide `VIEW DEFINITION` is a broad ask,
     whether the project needs it is visible in the declarations `doctor`
     already reads, and `ensure` never emits a `DELETE`. The
     `sys.database_permissions` scan behind the second half is asked under the
     same condition, so a project that removes no rows is neither asked nor
     told. The live test holds both declarations against the same account and
     the same server to pin the difference.

     **A denied securable often cannot be named, and an invented name is worse
     than none.** Measured on SQL Server 2022 (16.0.4295.3) and 2025
     (17.0.4075.5) alike: an object carrying an effective `DENY VIEW
     DEFINITION` answers NULL to both `OBJECT_SCHEMA_NAME` and `OBJECT_NAME` —
     the denial being reported is itself what removes the metadata visibility —
     and a `SELECT` grant on its schema does not bring the name back. A denied
     *schema* is still named. `Gap::securable` offers its output as the thing a
     statement names, so the unnameable case travels as
     `Securable::Unreadable { class, id }` and prints
     `OBJECT::<unnameable: id 1221579390>`: deliberately not bracket-quoted,
     because it is not a name, and one query away from the name for whoever
     holds the `DENY`. `introspect::Securable::Unreadable` records the same
     finding for `pull`; absent, empty and unreadable are three different
     things.

     `sys.database_permissions.class` is a `tinyint`, and the driver hands that
     back as a `u8`: reading it as an `i32` fails outright. The widening happens
     in the engine, where the column is named. Only the live test could find
     that — the unit tests build `Held` by hand.

     **`VIEW SECURITY DEFINITION` was the premise of #524 and the engine does
     not support it.** A review read the permission's name and concluded that
     an effective denial of it could leave database `VIEW DEFINITION` granted
     while hiding a policy, so that the count would accept a filtered zero. Run
     on both engines above, with an enabled FILTER policy in a separate schema,
     child `SELECT` and database `VIEW DEFINITION`:

     | Deployment permissions | db `VIEW DEFINITION` | db `VIEW SECURITY DEFINITION` | visible FK / predicate | probe and guard |
     | --- | --- | --- | --- | --- |
     | baseline | 1 | 1 | 1 / 1 | both refuse the active policy |
     | direct database `DENY` | 1 | 0 | 1 / 1 | both refuse the active policy |
     | role-inherited database `DENY` | 1 | 0 | 1 / 1 | both refuse the active policy |

     With the policy disabled and the child detached, all three count a real
     zero and the guarded delete runs. Refusing on the denial alone would reject
     those three — a valid plan refused, the one direction this count may not
     fail in. `sys.fn_builtin_permissions(DEFAULT)` reports the permission at
     **DATABASE** scope only, covered by `VIEW DEFINITION`, and denying it on
     the policy object or its schema is syntax error 102. So there is no
     production change for #524: the fixture is asserted permanently instead,
     the scope list and the 102 included, so that an engine which grows the
     scope fails that test rather than passing it silently.

     **No permanent SQL Server 2022 container in CI.** The two engines answered
     every cell of that table identically, and both were measured here. The
     `pbps-test-pg16` precedent exists because the two PostgreSQL servers
     *differ* — pinning a second engine is worth its cost when it disagrees,
     not merely when it is older.

<a id="decision-509"></a>

509. **A foreign key's target is somebody else's table when the declarations do
     not hold it, whatever schema it sits in.**

     `doctor`'s foreign-key question used to classify a target by the *managed
     schemas*: a target inside one was left out, because the managed `SELECT`
     was asked at schema scope and covered it, and asking again would have
     reported one gap at two securables. Both dialects have since narrowed that
     read to the tables — `Needed::ManagedTable` on SQL Server, `SELECT` on each
     managed table on PostgreSQL — and an undeclared parent inside a managed
     schema fell out of both lists.

     Classified by **declared table membership** instead (issues #510, #315).
     The two issues are one change to one function, `pbps-cli`'s
     `referenced_targets`, and could not be made separately. Measured on both
     pinned engines: the DDL half stays covered on SQL Server by `REFERENCES ON
     SCHEMA::app` and is covered by nothing on PostgreSQL, where a schema grant
     is only `USAGE` and `CREATE` — while nothing covers the `SELECT` the
     foreign key's own pre-flight probe makes. The login passed `doctor` with no
     gaps and its probe failed, with error 229 and with `42501`.

     Asking a target inside a managed schema at object scope does not report the
     covered half twice. `HAS_PERMS_BY_NAME` accounts for inheritance, so a
     `REFERENCES` grant on the schema answers 1 for a table under it; PostgreSQL
     has no schema-scoped `REFERENCES` to inherit from, so the object-scope
     question is the only one there is. A target the declarations *do* hold
     stays out either way: it is a managed table, asked about as one.

<a id="decision-510"></a>

510. **`doctor` asks for the read that closes an apply, over the columns the
     declaration names.**

     `SELECT` on a managed table is asked over the *catalog's* columns, which is
     the right list for the pre-flight probes and the wrong one for the row
     read-back: `apply` reads the managed rows back before it records and
     commits, and `rows::query` projects the key and the writable cells the
     **declaration** names. The two lists differ in exactly the case that
     matters — a plan that adds a column reads it back in the same run that
     creates it.

     So a `SELECT` requirement of its own, on each table that declares rows
     (issue #516). Measured on 17.0.4075.5 with `app.t(code, label)` and a
     declaration adding `extra`: a login granted `SELECT` on `code` and `label`
     answers 1 on each, 0 on `extra` — a column the catalog does not hold yet
     answers 0 rather than NULL — runs the `ALTER` itself, and then cannot read
     what it added (error 230). An object-level grantee answers 1 at object
     scope, which the question takes before it reaches any column, and its read
     runs. The remedy the report prints is therefore the grant that actually
     covers a column added later, which is the same reasoning `Columns::Declared`
     already carried for `UPDATE`.

     Asked as a second statement rather than by widening the DML question's
     column list: the read covers the key and the `UPDATE` must not: demanding
     `UPDATE` on a primary key would refuse every column-level grant a careful
     DBA would write. One column list per object is all one statement can bind.

<a id="decision-512"></a>

512. **A table moving between schemas is asked about at two securables, because
     the move drops the permissions on it.**

     Current-name resolution asks about the object this environment still has
     (439), which is right for every statement that runs *before* the change and
     silent about the ones after it. For a move between schemas that silence is
     wrong: `ALTER SCHEMA <dest> TRANSFER <source>.<name>` **drops every
     permission on the object it moves**. Measured on 17.0.4075.5 — a login
     holding `SELECT` on `app.old_name` and `ALTER` on both schemas ran the
     transfer, `HAS_PERMS_BY_NAME` answered 1 before it and 0 after, and the next
     read failed with error 229.

     So the destination is demanded as well (issue #517). The destination object
     does not exist yet, so the only securable a grant for it can sit on is the
     destination *schema*, and `GRANT SELECT ON SCHEMA::dest` was measured to
     carry the post-transfer read. The source answer is kept too: the probes read
     the table where it is now. An ordinary rename is unaffected — its schema
     does not change, so the second question is not asked.

     **Demanded of the tables that are really read there, and of no others.**
     The first cut asked it of every managed table that moves, and that refused
     a deployment which would have run: the probes' `SELECT` is spent *before*
     the transfer — `preflight` runs before `execute_statements` — and the only
     read that happens afterwards is the row read-back, which `apply_under_lock`
     scopes to the plan's data tables. A moving table with no `data:` block is
     therefore never read at its destination, and asking for it there was the
     over-demand this list exists to avoid, arrived at while fixing an
     under-demand. The question lives in the data path alone.

     **Every demand the table carries, not only the read.** The transfer drops
     *all* of the object's permissions, so a report that asked the destination
     for `SELECT` alone would print a remedy that makes `doctor` go green on an
     environment where the first row still fails — a misleading all-clear, which
     is worse than the silence it replaced. Measured on 17.0.4075.5: a login
     holding `SELECT, INSERT, UPDATE, DELETE` on `app.old_name` and `SELECT` on
     `SCHEMA::dest` ran the transfer, read the table at its new name, and was
     refused its `INSERT` with error 229 — `HAS_PERMS_BY_NAME` answering 1 for
     the destination's `SELECT` and 0 for its `INSERT`. So the data
     requirements ask at the destination too, and `SELECT` is named there twice,
     by the probes and by the read-back, which is the "one permission, two
     reasons" shape the ledger's own `SELECT` already has.

     What this does **not** demand is the `CONTROL` on the source object that the
     transfer statement itself wants on top of `ALTER` on the destination. That
     over-demand is a separate question (#352), and this entry deliberately
     leaves it where it was.

<a id="decision-513"></a>

513. **A key the declarations no longer name carries no demand, because the plan
     drops it before anything reads it.**

     The delete-count child that moves between schemas is demanded at its
     destination (511, 512), and the discovery that finds it reads the catalog —
     which still holds the key the next apply is about to take away. A managed
     child that both moves and drops its key into the parent was therefore asked
     for a `SELECT` on the destination schema that nothing would ever spend:
     `DropForeignKey` is `order_key` 2 against `DeleteRow`'s 12, the guard
     (`preflight::still_referenced`) discovers its children from the catalog
     *inside* the delete's transaction, and by then the constraint is gone. The
     probe that runs before the statements does still see it, and leaves it out
     for itself (128) — but the probe reads the child at its *source* name, from
     before the transfer, so the destination is nobody's demand.

     `doctor` never looks at a plan (417), and does not have to here: a key the
     catalog holds that the declarations do not name is a `DropForeignKey` the
     next apply will write, and the declarations are already in hand. So the
     declared keys travel with the rest of the ask (`Ask::declared_keys`), and
     the destination is demanded only for a child whose declaration still names
     a key into a table this project can delete a row from.

     **This narrows one case and not its neighbour**, deliberately. A child the
     plan drops outright (`DropTable`, `order_key` 6) is also left out of the
     guard's read, and `doctor` still demands the count's `SELECT` on it — but
     that child is *undeclared* by construction, since a dropped table is one the
     declarations no longer hold, and nothing in a declarations-only reading
     distinguishes it from somebody else's table that will still be there. The
     demand is on an object that exists, at the securable it exists on, which is
     an over-demand this reading cannot see; the destination demand was one it
     could.

<a id="dec-384-1"></a>

**DEC-384.1. SQL Server `doctor` asks the database's collation which names are
one securable, instead of folding them in Rust (#384, #673).** Three
comparisons decided it in Rust:

- `resolve_for_query`'s claim check folded with `to_lowercase`. That covers
  case only, so on an accent-, width- or kana-insensitive database a reused
  `app.cafe` beside a recorded `app.café` passed as a different table, and was
  asked about under the departing identity's object.
- The foreign-key targets the CLI reports as undeclared were compared exactly.
- The delete count's catalog children were checked against the managed names
  exactly.

The last two asked a managed table a second time under another spelling.

All three now go through `catalog::matching_table_names`, which compares under
`DATABASE_DEFAULT` (DECISIONS 119, 142), in `pbps-mssql::doctor`. That is where
the engine is known. The CLI's `referenced_targets` stays exact, because it
reads the declarations offline and serves PostgreSQL, where the exact
comparison is right. On a case-sensitive database the engine keeps the
spellings apart, and so do these checks. The cost is up to five round trips
per environment, each skipped when its lists are empty.
