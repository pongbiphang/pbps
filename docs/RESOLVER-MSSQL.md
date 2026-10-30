# SQL Server binding design (#619)

The measured design for the SQL Server resolver: what SQL Server binds when an
object is created, how it resolves names, what keeps a catalog read
consistent, how scratch is told from the target and proven empty, and what
the externals file is compared on. It enables nothing: SQL Server resolution
stays refused by name until [#620](https://github.com/pongbiphang/pbps/issues/620)
implements this design, and nothing here is inferred from PostgreSQL
([ADR-0016](ADR-0016-engine-assisted-planning.md#delivery-and-supersession),
item 3).

The reasons are [DEC-619.1](decisions/resolver.md#dec-619-1) to
[DEC-619.4](decisions/resolver.md#dec-619-4). Every claim below is pinned by a
live test in `crates/pbps-mssql/tests/live/binding619.rs`, named in brackets,
on the pinned SQL Server 2025 (17.0) build, Developer and Express. Hosted
families (Azure SQL Database, Managed Instance) have no Docker mapping and
stay refused by name, unmeasured (DECISIONS 521).

## 1. What binds at creation, and where it is recorded

SQL Server binds very little at creation. Most references in module bodies
are resolved again each time the module runs, so for them the question is
what they resolve to now, not what they resolved to when they were created.

| Surface | Bound | Recorded |
|---|---|---|
| A one-part name in a view, function, procedure or trigger body that is not schema-bound | When it runs: the module's schema, then `dbo` | `sys.sql_expression_dependencies`, `referenced_id` resolved when the catalog is read [`the_dependency_catalog_resolves_a_one_part_name_when_it_is_read`] |
| A one-part procedure name in an `EXEC` in such a body | When it runs: `sys`, then the default schema of the principal the module executes as, then `dbo`; never the module's schema [`a_procedure_call_in_a_module_resolves_in_sys_then_the_executing_users_schema`] | The same |
| A two-part name in such a body | The named object, by name | The same, by name |
| A `WITH SCHEMABINDING` view or function's references (two-part names required) | At creation, by id; the engine refuses to alter or drop the target | `is_schema_bound_reference = 1` with `referenced_id` [`schema_bound_references_are_held_by_the_engine_and_plain_ones_are_not`] |
| A function a computed column, DEFAULT or CHECK calls (two-part names required) | At creation, by id; schema-bound | The same |
| A type in a column, parameter or return | At creation, by type id | `sys.columns`, `sys.parameters` [`synonyms_and_other_databases_are_named_and_built_ins_are_never_shadowed`] |
| A synonym | Its name, at creation; its target, when it runs | The dependency names the synonym; the target is `sys.synonyms.base_object_name` |
| A filtered index's predicate | Its own table's columns only | No dependency row |
| A procedure referencing an object that does not exist yet | When it runs (deferred name resolution) | `referenced_id` NULL |
| A three-part or four-part name | Another database or server, by name | `referenced_database_name`, `referenced_id` NULL; refused at creation when the object is missing |
| A built-in function's one-part name | Always the built-in: a user function of the same name is never taken | No row |
| `QUOTED_IDENTIFIER`, `ANSI_NULLS` | At creation, fixed for the module | `sys.sql_modules.uses_quoted_identifier`, `uses_ansi_nulls` [`a_modules_set_options_are_fixed_when_it_is_created`] |

`is_ambiguous = 1` marks a two-part call such as `dbo.f(c)` in a body that
is not schema-bound, which could also be a method of a column named `dbo`.
pbps manages no CLR-typed column, so the reference is read as the function.

## 2. Name resolution

- **Inside a module** a one-part object name looks in the module's own
  schema, then in `dbo`. Neither the caller's default schema nor `EXECUTE AS`
  changes it
  [`a_modules_one_part_names_resolve_in_its_schema_and_a_statements_in_the_users`].
- **A procedure call is the exception**
  [`a_procedure_call_in_a_module_resolves_in_sys_then_the_executing_users_schema`].
  A one-part name in an `EXEC` inside a module looks in this order:
  - first in `sys`, so a system procedure is never captured, even by a
    same-name procedure in the module's schema or `dbo`;
  - then in the default schema of the principal the module executes as:
    the caller, or the module's `EXECUTE AS` principal;
  - then in `dbo`.

  It never looks in the module's schema. `app.calls` running `EXEC q` runs
  `dbo.q` for a caller whose default schema is `dbo`, though `app.q` exists,
  and `other.q` for a caller whose default schema is `other`.
- **Outside a module**, in the statement that creates a table or a module, a
  one-part name looks in the executing user's default schema, then in `dbo`.
  A column typed `amount` lands on `a.amount` for a deployer whose default
  schema is `a`. Scratch therefore reproduces the deployer's default schema;
  #611's authorization context reads it.
- **Every kind of object occupies a name.** A scalar function arriving
  under a table's name breaks a view that reads the table, rather than being
  passed over for `dbo`'s table.
- **The motivating pair**
  [`an_arrival_in_a_modules_own_schema_rebinds_its_one_part_names_and_no_other_arrival_does`]:
  - A table arriving in schema `other`, off the path, moves nothing.
  - A function arriving in the module's schema under a name that only
    two-part callers use moves nothing.
  - A table arriving in the module's own schema rebinds a one-part view and
    a procedure at once, with no DDL on either. The two-part view, the
    schema-bound view and the computed column stay where they were.
  - The compatibility level moves no recorded binding.

**Consequence for the resolver.** A one-part reference in a body that is not
schema-bound is **runtime-bound**. Rebuilding the module cannot pin it, so
there is no rebuild decision to make for it. Its candidates enter the
manifest:

- **An object reference:** the same name in the module's schema and in
  `dbo`, of any object type.
- **A procedure call:** none if `sys` holds a procedure of that name.
  Otherwise the name in `dbo` and in the default schema of each principal
  the module can execute as.
  - A module with a fixed `EXECUTE AS` has that one principal.
  - Any other module has every database principal, since anyone may call it.
  - The principals' default schemas enter the manifest with them, because
    `ALTER USER … WITH DEFAULT_SCHEMA` moves the call.

The resolver reports what each plan-created object captures:

- an object the plan creates that captures a **managed** module's reference
  is reported as that module's binding change, with no statement;
- one that captures an **unmanaged** module's reference refuses, as the
  unmanaged-dependent gate does on PostgreSQL.

Creation-bound references (schema-bound, computed columns, defaults, CHECKs
and types) are bound by id and compared like PostgreSQL's.

## 3. Catalog consistency

Catalog metadata is not versioned. No isolation level, hint or statement
shape reads it at one instant:

- Under `SERIALIZABLE` with `HOLDLOCK`, the reads hold only a shared database
  lock. Another session's `CREATE`, `ALTER VIEW`, `GRANT`, role-membership
  change and `DROP` all commit at once, and the next read sees them.
- Under `SNAPSHOT`, the catalog views see the new object, and touching it
  dooms the transaction (error 3961)
  [`no_isolation_level_or_hint_holds_the_catalog_against_another_session`].
- Two scans of `sys.objects` in one `SELECT` see a table another session
  commits between them [`one_statement_does_not_read_the_catalog_at_one_instant`].
- `modify_date` moves when an object is renamed and renamed back, but not when
  a grant and its revoke restore a permission
  [`modify_date_moves_on_a_restored_definition_but_not_on_a_restored_grant`].

Two candidates that do hold the catalog were measured and rejected
([DEC-619.1](decisions/resolver.md#dec-619-1)):

- **A schema lock.** A same-owner `ALTER AUTHORIZATION` holds the schema
  against change and blocks every read of it too. It needs ownership, which
  neither a deployer with `ALTER` on the schema nor `db_ddladmin` holds, and it
  misses database-scope changes such as role membership
  [`a_schema_lock_needs_ownership_a_deployer_lacks_and_misses_database_scope_changes`].
- **A DDL witness trigger installed on the target.** It misses `ALTER
  DATABASE` options and view refreshes. It also makes every user's DDL fail
  once that user may no longer write its row
  [`a_ddl_witness_trigger_misses_some_changes_and_fails_other_users_ddl`].

**The design: plain catalog reads, compared at the opening and at the closing
read.** Each capture reads its components one statement at a time. The
apply's closing capture runs after the last statement and before the commit.
A mismatch at either point refuses: before the first statement nothing runs,
and after it the transaction rolls back.

A commit between two component reads does one of three things
[`a_commit_between_component_reads_reaches_a_later_read_or_follows_the_earlier_one`]:

- it reaches a later component, which then differs from the seal and refuses;
- it touches only components already read, which is a change after their last
  observation — the class SPEC §7.6 already excludes, now measured from each
  component's last read rather than the capture's;
- it is changed and restored between the reads, which leaves nothing lasting.

A lasting effect that a transient object leaves on the apply's own statements
is itself read by the closing capture: a column typed by a type that arrived
for a moment keeps that type.

## 4. Capture inside the apply transaction

[`the_apply_transaction_reads_its_own_ddl_and_does_not_lock_out_another_writer`]

- The closing capture, in the apply's own transaction, sees the transaction's
  uncommitted DDL and the bindings it records.
- That uncommitted DDL holds another session's catalog scan of the same schema
  until the commit, but not that session's DDL. The apply detects another
  writer; it does not lock one out.
- The deployment lock (`dbo.__pbps_lock`, a committed row) keeps out only
  other pbps applies (SPEC §8.1).
- Reading `sys.sql_expression_dependencies` needs two permissions.
  Without `SELECT` on the view, which only `db_owner` holds by default, the
  read is refused (229). With `SELECT` but without `VIEW DEFINITION` on the
  database, it reads empty, which cannot be told from no references
  (measured on #2141, [DEC-2141.1](decisions/resolver.md#dec-2141-1)). The
  capture requires both and refuses without either.

## 5. Reconstruction

- **The deployer.** Scratch reproduces #611's database context, without
  `ALTER LOGIN` (DEC-2267.1). An ordinary deployer is a second user without
  a login that the sandbox (§6) creates. The user carries:
  - its default schema, which statement-level names need (§2);
  - its role memberships;
  - the database and schema GRANTs and DENYs the target grants it;
  - object and column permissions on staged objects, including role and
    public rows. A column REVOKE remains an exception, never a DENY.

  A target whose captured effective user is `dbo` enters scratch's own
  `dbo` from the sandbox, also `WITH NO REVERT`. A db_owner member is not
  equivalent: object DENY stops that member and does not stop dbo. Read
  `is_trustworthy_on` before and after entry and require 0; otherwise an
  entered dbo can gain the database owner's server reach. This amends
  DEC-619.3's literal second-loginless-user requirement.

  Restore the captured effective language, DATEFIRST, DATEFORMAT and SET
  options on the compile connection and verify its reported values. Owner
  partitions include every held schema, staged effective owners and deployer
  and dbo anchors. Resolve explicit owner, parent owner, then schema owner;
  compare equality relationships across subjects, never owner names or IDs.
  Trusted setup assigns ownership before object ACL replay because changing
  authorization clears those ACLs. Contradictory file ownership refuses
  without repair. Settlement re-reads database and staged-object authority.

  The declared objects compile as this user, entered from the sandbox
  `WITH NO REVERT`. A statement the deployer may not run then refuses on
  scratch as on the target: `CREATE VIEW` without the permission (262), or
  a table in a schema it cannot alter (2760). An ordinary deployer without
  impersonation authority cannot return to the sandbox (15517); neither
  entry can REVERT (15196)
  [`the_reproduced_deployer_compiles_inside_the_sandbox_with_only_its_own_rights`].
- **Order and transport (DEC-2374.1, DEC-2374.2, DEC-2374.5).** The emitter's
  statements compile in the scratch route's order, not the ordinary plan's. A
  new table whose computed column, default or check calls a planned function
  is split by `Mssql::generated_table_parts`: a computed column naming a
  declared function fails in the `CREATE` with 4121, and a default with 342.
  The columns before the first such computed column stay in the `CREATE`,
  every later one is added in declared order (positions are compared as
  reported and the engine appends), and what names a late column is its own
  part. `generated::constraints` orders them: the function before each part
  that calls it, the late columns after the bare table and in order, a module
  after the columns it reads. Table, then function, then `ALTER TABLE .. ADD
  c AS f(..)` compiles even where `f` is schema-bound and reads the table. A
  declaration whose order cannot both compile and keep the declared positions
  refuses by name, with the remedy. Each `Statement` is its own batch, in
  autocommit, on the deployer's connection: a failed statement leaves nothing
  behind and the connection usable, so no transaction frames the run. A
  failure naming a deadlock, a lock or a resource limit answers nothing about
  the declaration and is a read failure, not a verdict.
- **Fingerprints.** A module's fingerprint carries `uses_ansi_nulls`,
  `uses_quoted_identifier` and `is_schema_bound`. The database collation and
  compatibility level are compared as reported facts (`mssql-reported-scope-v2`).
- **Teardown and rebuild order.** A schema-bound reference blocks the
  following:
  - altering a column it reads (errors 5074 and 4922);
  - dropping its target (3729);
  - dropping or altering a function a computed column, DEFAULT or CHECK
    calls (3729).

  A schema-bound view cannot call a function that is not schema-bound (4513).
  A plain view blocks nothing. Per #1776's new-engine contract (S7,
  [#1959](https://github.com/pongbiphang/pbps/issues/1959)), these are written
  as SQL Server `engine_effects` and `Catalog` edges that `settle` orders: the
  dependent's drop before the target's change, and its re-creation after. No
  SQL Server reorder pass is written.

## 6. Separation

- **No reported identifier proves two instances are different.** `master`'s
  `family_guid` differs between two installations of one image but is kept by
  a copy of an installation, together with `@@SERVERNAME`. Measured with a
  `podman commit` clone on #619; the PR records it.
  - The target identity the evidence seals is `master`'s `family_guid`. Like
    PostgreSQL's `system_identifier`, it names an installation's lineage, so a
    copy passes the target-binding check. The opening manifest check then
    compares the copy's state.
- **Scratch is never the target, by a live marker.** Before any other scratch
  write, the scratch session creates a global temporary table under a fresh
  name, `##pbps_<nonce>`, and the target session looks for it.
  - Every login on the same instance sees it, without privilege. Another
    instance does not. It ends with the session that made it
    [`a_global_temporary_table_marks_one_instance_to_every_login_on_it`].
  - Seen means the same instance. Then the same-instance rules below apply,
    and scratch may never be the target database itself.
- **A sandbox, not a confined login**
  ([DEC-619.3](decisions/resolver.md#dec-619-3)). Every statement pbps runs
  on scratch for the plan runs inside a database execution context:
  - the baseline, the externals file and the deployer's reproduction run as
    the sandbox user, `db_owner` of the scratch database only;
  - the declared objects run as the reproduced deployer (§5), an ordinary
    loginless user or scratch's own dbo, entered from the sandbox
    `WITH NO REVERT` (DEC-2267.1).
  - supplied-scratch release retains a separate bounded dbo connection only
    for visibility repair (DEC-2314.1). It records the pre-file visibility
    inputs, restores them under the same TRUSTWORTHY-OFF and token checks,
    then irreversibly re-enters the original sandbox for ordinary cleanup.
    Repair never executes through the provisioning login.

  The ordinary deployer is loginless and carries its captured database
  authority. The dbo route
  requires TRUSTWORTHY OFF and passes the same escape probes; this replaces
  the earlier statement that both execution identities are loginless:
  - **How it is entered.** The supplied account creates the user after the
    empty check, and impersonates it with `EXECUTE AS USER … WITH NO
    REVERT` before the first such statement. The session cannot leave it.
    A fresh session of the supplied account drops the user in the cleanup.
  - **Measured from an `sa` session**
    [`a_scratch_sandbox_holds_no_server_reach_and_enters_only_where_guest_admits_it`].
    In the sandbox:
    - `CONTROL SERVER` and `sysadmin` read 0;
    - `CREATE LOGIN`, `CREATE DATABASE`, `sp_configure`, and setting the
      scratch `TRUSTWORTHY` or `DB_CHAINING` are refused (15247, 262);
    - `EXECUTE AS LOGIN` and `REVERT` are refused (15406, 15196);
    - a database whose `guest` is disabled is refused (916);
    - every kind of DDL a run needs works: schemas, types, tables, views,
      functions, roles, loginless users, grants and temporary tables.
  - **The supplied account is no longer the boundary.** It must be
    `db_owner` of its scratch database, to create and impersonate the
    sandbox. Its server permissions, roles, ownership and mappings stay
    outside the sandbox, whatever they are.
  - **What still reaches out**, read before the first plan statement. Each is a
    property of the instance or of the scratch database, not of an account:
    - **Instance-wide ownership chaining must be off.** The sandbox reads
      `sys.configurations.value_in_use` for `cross db ownership chaining`.
      This option overrides the scratch database's own `DB_CHAINING` flag
      and allows same-owner chains into other databases, including `msdb`.
      An enabled or unreadable setting refuses by name. Disable it with
      `EXEC sp_configure 'cross db ownership chaining', 0; RECONFIGURE;`
      or use a scratch instance with it disabled
      [`instance_wide_ownership_chaining_refuses_the_scratch_sandbox_and_cleans_setup`].
    - **A database whose `guest` admits the sandbox.** The sandbox entered,
      and wrote, a target with `guest` enabled.
      - The supplied session lists every database in `sys.databases`. The
        sandbox lists only `master`, `tempdb` and its own, so it cannot
        list them itself.
      - The sandbox then reads `HAS_DBACCESS` for each. Only the scratch
        database, `master`, `tempdb` and `msdb` may admit it. This is
        #1667's "no access to another database".
      - The list counts only while the supplied session's
        `HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW ANY DATABASE')` reads 1. Under a
        `DENY VIEW ANY DATABASE` the query still succeeds but omits every
        other database, a guest-enabled target included, and `DB_NAME`
        hides the same ones. A list that cannot be shown whole refuses:
        hidden is not empty [`a_database_list_without_view_any_database_hides_the_target_and_says_so`].
    - **The scratch database's options, compared whole.** Its
      `sys.databases` row is compared with a fresh database's, every column
      by default [`a_scratch_databases_options_are_compared_whole_with_a_fresh_databases`].
      - Identity is skipped: `name`, `database_id`, `create_date`,
        `physical_database_name`, `service_broker_guid` and `owner_sid`.
        `log_reuse_wait` and `log_reuse_wait_desc` are statuses too: a
        FULL-recovery database waiting for `LOG_BACKUP` remains usable.
      - The collation, compatibility level, containment and ANSI settings
        are compared with the target instead, by §9's reported-facts rule.
        The fresh-row exclusions and that rule's database facts must name
        exactly the same set (DEC-2236.1).
      - In the run-owned layout the administrator creates a reference
        database beside scratch, reads its row, and immediately drops it,
        including after a failed read. It inherits that instance's `model`.
      - A supplied account need not be able to create a database. Its
        reference is `model`'s row, with the creation-decided bits replaced
        by measured values: broker, full-text and data retention start
        enabled; mixed-page allocation, read-only, clean shutdown, trust,
        chaining, broker priority, accelerated recovery and optimized
        locking start disabled. Every other column is compared by default.
      - A supplied scratch refuses when `model` is `READ_ONLY`, naming
        `ALTER DATABASE model SET READ_WRITE` as the remedy. On that model,
        snapshot isolation reports ON even when its persisted setting is
        OFF; two identical reported rows can produce different newborn
        rows. A constant cannot recover the hidden setting. The live test
        enumerates every database column and restores and verifies `model`
        after every option measurement.
      - The sandbox cannot change an option. An option set before it
        arrives is caught without a rule naming it. A `TRUSTWORTHY` scratch
        owned by an administrator lends an `EXECUTE AS OWNER` module the
        owner's `CONTROL SERVER`. `LEDGER = ON` turns every later table
        into a ledger table, which a drop only renames, so cleanup could
        not restore the inventory.
    - **What `guest` and `public` hold in `master`, `tempdb` and `msdb`.**
      The sandbox enters them as `guest`. Its token there must be `guest`
      and `public` only for the sandbox and ordinary deployer. Only after
      validated dbo entry, `dbo` (SQL USER) and `db_owner` (ROLE) may also
      appear with usage `DENY ONLY`; any other entry or usage refuses.
      The entered dbo's `sys.login_token` must contain the scratch owner's
      login and its server roles only, all `DENY ONLY`. The guest/public
      permission, membership and ownership rules stay unchanged; deny-only
      identities do not become grant-capable ownership subjects.
      Fixed rules apply on every build, without a
      permission-list snapshot (DEC-2236.1):
      - At database scope, exactly three GRANTs qualify: `CONNECT` to
        `guest`, and `VIEW ANY COLUMN MASTER KEY DEFINITION` and
        `VIEW ANY COLUMN ENCRYPTION KEY DEFINITION` to `public`.
      - An object permission must resolve as ms-shipped through
        `OBJECTPROPERTYEX(major_id, 'IsMSShipped')`, evaluated inside that
        database. Hidden system views can be absent from `sys.all_objects`.
        NULL refuses as unresolvable; `OBJECT_SCHEMA_NAME` and `OBJECT_NAME`
        name the object under the same rule.
      - A GRANT must be object-level (`minor_id = 0`), to `public`, and
        SELECT or EXECUTE. A DENY on an ms-shipped object qualifies because
        it only narrows reach. If it prevents a required catalog read, that
        read refuses by database and catalog name. No other permission
        class or permission on a non-ms-shipped object qualifies: an
        administrator's `EXECUTE AS OWNER` procedure is one such path.
      - `guest` must belong to no role and own only the `guest` schema;
        `public` owns nothing. Violations name the database, grantee,
        permission and affected object or role.
      - **Accepted residual.** An administrator's added GRANT to `public`
        on an ms-shipped object cannot be distinguished from a shipped
        grant. Measured `sys.xp_cmdshell` and `sys.sp_helptext` EXECUTE
        grants differ only in `major_id` in `sys.database_permissions`.
        Every login already holds that shared reach, the supplied account
        included, so the sandbox gains nothing from it. The check accepts
        it rather than pinning a build's system-object list.
  - **Why not a confined login.** The design first checked the supplied
    login's own reach against an allowlist, in PostgreSQL's form (#2085).
    Review kept finding reaches a SQL Server login carries that no read of
    its token, server permissions and grants shows. Each was measured, and
    the sandbox carries none of them
    [`a_login_reads_its_own_server_reach_without_privilege`,
    `a_login_reads_every_database_it_can_enter`,
    `a_login_reaches_the_server_through_a_role_it_owns_or_a_trustworthy_database`]:
    - `IMPERSONATE` on an administrator's login;
    - a user in another database, including a `db_owner` user in `msdb`;
    - a server role the login owns, which it may add itself to.

    A login's reach has no closed list on SQL Server. Other known routes
    include certificate-signed modules, linked servers and Agent proxies.
  - **PostgreSQL keeps its confined account.** There the difference
    measured on SQL Server does not arise, and no sandbox exists:
    - a connection enters one database, with no `USE` and no three-part
      name;
    - its cluster-wide authority is the closed set `REACH` reads;
    - `SET ROLE` is undone by `RESET ROLE`.
- **The target's instance shares `tempdb`.** A global temporary table
  carries no permission check, and every session on the instance shares the
  `##` names. On the target's instance the sandbox can do the following to
  a `##` table another session is still using
  [`a_sandbox_on_the_targets_instance_reaches_another_sessions_global_temporary_table`]:
  - read it;
  - write to it;
  - drop it;
  - take a `##` name before the session that expects it.

  Only `#` tables are private. Creating a temporary table needs no
  permission, so nothing can deny it, and an externals file's dynamic SQL
  defeats any parse. On the target's instance, then, what runs is limited
  instead:
  - **Without an externals file, pbps renders the externals.** It renders
    them from the target's catalog, as shapes carrying §8's compared
    properties, with every identifier quoted. The run then sends only DDL
    that pbps renders: no DML, no `EXEC` of a module and no user SQL.
  - **Such DDL runs no body that reaches a `##` table**
    [`rendered_ddl_on_an_empty_scratch_runs_no_body_that_reaches_a_global_temporary_table`].
    - A function or view cannot name a temporary table (2772, 4508), and a
      function cannot run dynamic SQL (443).
    - A trigger or procedure body runs on DML or `EXEC`, never on DDL
      against its table.
    - A run on the target's instance that would need any other statement
      refuses by name.
  - **An externals file on the target's instance refuses** unless the
    entry sets `same_instance: allow`. The default is `refuse`. As with
    `outside_writes`, the choice is the user's: the evidence seals it and
    `explain` shows it, because under `allow` the file's SQL shares
    `tempdb` with production.
  - **Another instance needs no server set aside.**
    - The run-owned layout on a Docker container is the default
      (DEC-1674.1).
    - On Windows, a per-user LocalDB instance is another instance with its
      own `tempdb`. It is Express, so an edition difference is a recorded
      limitation (§9).
      - The operator provides and starts the instance and names it in the
        scratch variable, `Server=(localdb)\<name>;Integrated Security=SSPI;Encrypt=DANGER_PLAINTEXT`.
        A named instance only: the default, shared and versioned forms are
        refused. LocalDB has no TLS on its pipe; an omitted `Encrypt` means
        required and `Encrypt=false` still encrypts the login, so the operator
        writes the driver's no-TLS spelling explicitly (DEC-2271.2).
      - The engine finds its pipe once per run by asking `SqlLocalDB.exe`
        (a fixed path, no shell, bounded output and time) and opens that
        endpoint for every connection of the run. No pipe, two pipes or a
        pipe of another shape is a named failure, never an empty result.
      - A vanished pipe or a restart fails the next connection, and a
        restarted instance has lost the marker: the run is invalid and
        never continues or cleans up on the restarted instance. A new
        invocation discovers again (DEC-2271.2).
      - The Linux-target-with-Windows-scratch pair stays a named platform
        mismatch (§9).
      - Measured support is what the `resolver-localdb` job has run; see
        STATUS. Nothing in this section was measured on Linux.
- **The layout.** The run-owned layout creates the run's login and
  database, so it needs `ALTER ANY LOGIN` and `CREATE ANY DATABASE`. It runs
  on another instance. On the target's instance only the supplied layout
  runs: an account that is `db_owner` of its own pre-created database, its
  statements in the sandbox, under the `tempdb` rule above.

## 7. An empty scratch database

The empty check covers two closed sets ([DEC-2263.1](decisions/resolver.md#dec-2263-1)):

1. **What rehearsal observes:** every user-created securable and non-securable
   in the catalog sweep; name resolution (schemas, principals, default schemas,
   synonyms); ownership and permission chains (owners, grants, memberships,
   signatures); and compilation options (the whole `sys.databases` comparison
   in §6 and database-scoped configurations).
2. **What separation relies on:** the sandbox's token and grants, signatures
   and `EXECUTE AS` paths, and objects that could carry it outside scratch
   if rehearsal sends on them, including broker routes and remote bindings.

State outside both sets is an accepted residual by rule, listed below; it adds
no read, permission or build-keyed baseline. Inside them, a built-in excluded
by name or ID must exist. Its alterable rehearsal-relevant columns must match
a fixed definition shared by every build. Cleanup restores only a fixed,
known definition; otherwise it refuses by name and never recreates a missing
built-in. This supersedes the matching empty-check and residual rules in
DEC-2236.1.

A database born from an unmodified `model` on the pinned build holds the
fresh-install inventory
[`a_database_born_from_model_holds_only_the_fresh_install_inventory`]:

- no user object;
- the 13 schemas: `dbo`, `guest`, `INFORMATION_SCHEMA`, `sys` and the nine
  fixed-role schemas;
- the 14 principals: `public`, `dbo`, `guest`, `INFORMATION_SCHEMA`, `sys`
  and the nine fixed roles;
- one role membership, `db_owner` ← `dbo`;
- permissions: `public`'s 2 database grants and `dbo`'s one database grant;
- no user type, assembly, synonym, XML schema collection, full-text catalog
  or database DDL trigger, and no database-scoped configuration away from its
  default.

The check requires the fixed schema, principal, membership and database-grant
lists above, the system broker message types, contracts, services and queues,
the `sys` XML schema collection and system assembly, `R` and `Python`, the
primary filegroup and files, and `AutoCreatedLocal`.
That route's address is `LOCAL`, its owner is `dbo`, and its service, broker,
lifetime and mirror fields are NULL. Cleanup resets its address and owner;
other changed destination fields refuse. `guest`'s default schema is `guest`
and `ALLOW_ENCRYPTED_VALUE_MODIFICATIONS` is OFF. `public` and every fixed role
are owned by `dbo`; changed owners are restored before dropping user principals.
The system assembly and languages are owned by `sys`. An altered owner names
the object and owner and refuses with "this database cannot be restored to a
fresh state; supply a new scratch database": assigning ownership back to `sys`
fails even as the loginless `db_owner` (15151). Cleanup issues no ALTER or DROP
for them. A missing fixed grant also refuses: re-granting would recreate missing
built-in state rather than reset a present object's column.

SELECT on an ms-shipped catalog view is the
measured exception: a fresh database's 232 system-object grants are all
such SELECTs, including hidden views `sys.all_objects` does not list.
They cannot be schema-bound, creation does not execute their reads, and
the loginless `db_owner` sandbox can grant, deny and revoke them itself.
Every other permission on a system object must be absent. A denied catalog
read refuses by catalog name; it never certifies emptiness (DEC-2236.1).

On supplied scratch, acquisition records these catalog SELECT ACLs before
untrusted execution, including accepted residual grants/denies and their
grantors/options. Release restores that visibility baseline before its
ordinary inventory (DEC-2314.1), so a file-made public DENY cannot survive
a successful cleanup and blind the next run. This does not turn a hidden
post-file read into an empty inventory or recreate unrelated missing built-ins.
A changed recovery identity, lost session, TRUSTWORTHY ON or failed repair
retains the existing named cleanup failure.

**How the inventory is read.** Securable classes and an explicit sweep of
non-securable catalogs supply the inventory. A
database holds things `sys.objects` never lists: a master key, a
certificate, an asymmetric key, and a partition function and scheme each
live only in their own catalog [`a_database_holds_securables_no_object_catalog_lists`].

- **Securable classes.** The read takes one catalog per database-scoped
  securable class the engine reports in `sys.fn_builtin_permissions`: the
  `DATABASE` class and every class whose parent is `DATABASE` or `SCHEMA`.
  On the pinned build there are 23, from `APPLICATION ROLE` to
  `XML SCHEMA COLLECTION`. `CREDENTIAL` is not among them: its parent is
  `SERVER`, and only `DATABASE SCOPED CREDENTIAL` belongs to the database. Each catalog is compared with its fresh-install
  rows, and the system broker objects a new database carries are among
  them. A class with no known catalog refuses by name, so a class a later
  build adds is refused until it is read.
- **Objects that are no securable.** The inventory was swept against
  `sys.all_views` on the pinned 17.0.4075.5 build. Beyond the securable
  classes it reads:
  - partition functions and schemes, plan guides and event notifications;
  - database audit specifications, filegroups and files;
  - Always Encrypted master keys, encryption keys and their values;
  - extended properties, every class;
  - broker priorities and conversation endpoints, plus conversation groups
    and queued transmissions without an endpoint;
  - external data sources, file formats and libraries;
  - database and table change tracking;
  - FILESTREAM non-transacted access and directory settings;
  - automatic tuning mode and explicitly selected tuning options;
  - user Query Store hints, forced plans and per-query tuning settings;
  - ledger digest locations, including historical locations, which refuse
    by name because there is no safe cleanup rule.

  A populated ledger digest location could not be tested because it needs
  external storage; the guard refuses any returned row.

  Fresh FILESTREAM settings are OFF with no directory. Fresh automatic
  tuning uses AUTO mode and DEFAULT options; its runtime actual state is
  not a baseline. Database-ID catalogs are filtered to the current database.
  Query Store execution statistics alone are runtime history; recorded
  hints, forced plans and tuning overrides are persistent intent.

  Parent cleanup was measured for search properties, sensitivity labels,
  security predicates, selective XML paths and namespaces, JSON and spatial
  indexes, columnstore metadata, table change tracking, ordinary columns,
  constraints, statistics, parameters and dependencies. Dropping their
  inventoried parent removes the corresponding user rows. External library
  files disappear with their library. Full-text catalogs can be created and
  dropped, but stoplist and index creation fail on this image (7609), so
  empty full-text child catalogs are not evidence of parent cleanup. Ledger deletion retains named, soft-deleted table/view
  objects and history; it must refuse rather than report a fresh database.

  There is no engine-provided enumeration of non-securable kinds. The
  explicit list remains necessary even though every securable class is
  checked against the engine's list.
- **Accepted residual: physical allocation.** Primary data/log file growth,
  size and maximum size, and `PRIMARY`'s `AUTOGROW_ALL_FILES`, can change as
  the loginless `db_owner`. They govern allocation, not name resolution,
  ownership, compilation or separation. File growth/size defaults vary by
  version and provisioning. Their changes pass without a read or baseline
  for those columns; the primary files/filegroup must still exist and remain
  usable. User-added files and filegroups remain inventoried.
- **Accepted residual: database encryption key with encryption OFF.** The
  whole `sys.databases` row is unchanged; reading the key needs server-level
  `VIEW SERVER SECURITY STATE` (15562 as the loginless `db_owner`). No object
  creation, binding or compilation depends on it. Encryption ON changes
  `is_encrypted`, so the whole-row comparison refuses. pbps never creates
  a database encryption key, so cleanup cannot leave one behind; run-owned
  and Docker databases start new.
- **Accepted residual: granular Query Store options.** `db_owner` can change
  `QUERY_CAPTURE_MODE`, `MAX_STORAGE_SIZE_MB` and similar options without
  changing `sys.databases`. Fresh defaults vary by version with no
  `is_default`, so a baseline would be build-keyed. They govern plan
  collection, never object creation, binding or compilation. Hints, forced
  plans and per-query tuning that affect execution remain inventoried and
  cleaned. These are instances of the two-set rule, not per-build exceptions.

#723's check compares the newborn database with this inventory, read so,
before sandbox setup or plan DDL. The live marker necessarily precedes it.
A table or user added to `model` appears in the next
`CREATE DATABASE`, measured on #619.

The check reads as the database's `db_owner`. Metadata visibility hides what
a database holds from any other account, which reads no user object where one
exists [`an_account_that_is_not_db_owner_cannot_see_what_its_database_holds`].
So the supplied layout's account must be `db_owner` of its database, and a
check that cannot read as one refuses.

An initially dirty supplied database refuses without dropping its contents,
as PostgreSQL's does. After acquisition, the supplied layout is emptied
after a run, including a refusal, as `db_owner`.
SQL Server has no `DROP OWNED`. So cleanup drops exactly what the
inventory read finds beyond the fresh-install inventory, the same read #723's
check makes, rather than a list of kinds. A kind no rule here knows to drop
refuses by name, rather than being left behind:

- **Order.** Extended properties precede grants and memberships. Cleanup
  then works in reverse dependency order: schema-bound
  dependents, then modules, tables, types, synonyms and other
  schema-contained objects, then schemas, then database roles and users.
  Conversations end before services and priorities are dropped. External
  tables use `DROP EXTERNAL TABLE` before data sources, file formats and
  credentials. Security policies precede tables and functions. Table change
  tracking is disabled before primary keys are dropped (3735), and database
  tracking is disabled after the tables. Automatic tuning returns to AUTO
  before options return to DEFAULT (15715). Query Store hints and forced
  plans are cleared before their query is removed (12456).
  Partition schemes precede their functions, and encryption keys precede
  their master keys. A role owned by another role or user goes before its
  owner; that ownership chain is a dependency too (15421).
  Catalog-ID edges also order references within a rank: symmetric-key
  encryptors, assembly references and table types using alias types.
  Certificate/key-mapped users precede their matching key by SID (15559);
  signatures and counter-signatures precede their signer. Keys precede
  their owners and master-key protectors, and external models precede
  their credentials. Bound types or columns precede standalone defaults
  and rules; standalone defaults use `DROP DEFAULT`. A dependency cycle
  refuses with the retained names. Cleanup does not acquire passwords or
  alter encryption to break a cycle.
- **Roles.** A role sheds its members first. Its schemas go before it,
  since `DROP ROLE` refuses a role that has a member (15144) or owns a
  schema (15138)
  [`a_role_drops_only_after_its_members_and_the_schemas_it_owns`].
- **The sandbox user** is dropped last, by a fresh session of the supplied
  account. Other cleanup statements stay in the irreversible sandbox:
  database DDL triggers are user code too. The final privileged drop checks
  that the complete inventory contains no other remaining object first.
- **Afterwards.** A database that does not read as the inventory refuses
  the next run.

## 8. The externals file on SQL Server

#1673's rule (DEC-1673.1), in PostgreSQL's form (#2085,
[DEC-619.4](decisions/resolver.md#dec-619-4)), as `pbps-pg`'s
`capture/staged.rs` does it. Every object the externals file creates is
compared with the target on **every property the engine reports for it and
its children**, minus a short skip list. A property or class no rule names is
compared, not passed.
Names resolve under the engine's database collation, but stored spelling remains compared: a resolved spelling difference refuses by both names, while a genuinely absent object refuses as absent.

- **What is read:**
  - the object: `sys.objects`, `sys.sql_modules`' flags and the
    `OBJECTPROPERTYEX` properties in the named SQL Server 17 manifest
    `capture::OBJECT_PROPERTIES`. Every manifest name has an explicit
    disposition. The API cannot enumerate future property names; qualifying
    another engine version requires checking this manifest against its
    documented properties. NULL is retained as a value for non-applicability;
    missing rows, unreadable metadata and failed queries refuse;
  - its columns: a table's, a view's and a table-valued function's result
    columns, from `sys.columns`;
  - its parameters (`sys.parameters`);
  - its primary key, unique constraints and unique indexes, with their key
    columns (`sys.indexes`, `sys.index_columns`);
  - a type's facets (`sys.types`);
  - a synonym's `base_object_name`.
- **What is skipped, and why**, as PostgreSQL's `projected` skips a body and
  defaults:
  - a module's definition, since a view may be staged as a shape view of
    typed NULLs;
  - column defaults and CHECK bodies;
  - noncascading foreign keys, non-unique indexes and ordinary triggers;
    their redundant Has/Count/ID properties are named individually in the
    manifest, including summaries that mix retained and skipped children.
    Unique keys and relevant cascade/trigger prerequisites are compared
    separately; a prefix never selects properties to skip;
  - module text in both normalized properties and `sys.sql_modules`, and
    default references in column/type rows as well as the skipped children;
  - `Cardinality`, `TableIsLockedOnBulkLoad` and `sys.tables.lock_on_bulk_load`;
    and the progress counters `TableFullTextMergeStatus`,
    `TableFulltextDocsProcessed`, `TableFulltextFailCount`,
    `TableFulltextItemCount`, `TableFulltextPendingChanges` and
    `TableFulltextPopulateStatus`. Fulltext configuration remains compared;
    no build-specific expected-value table supplies a baseline;
  - ids, dates and owner names, which differ on scratch by construction,
    as PostgreSQL drops role names.
- **Owner equality is compared.** Unlike PostgreSQL, SQL Server reads
  ownership at creation, though only as an equality: a `PERSISTED` computed
  column refuses a function whose effective owner is not the table's
  (4988), whoever owns either [`a_persisted_column_needs_its_functions_owner_to_be_the_tables`].
  - The comparison covers every schema scratch holds, managed and external.
    Two of them must share an effective owner on scratch exactly when they
    do on the target, and an object with its own owner counts as its own
    entry.
  - Scratch reproduces the target's grouping: each schema is created
    `AUTHORIZATION` a loginless user standing for its target owner, one
    user per distinct owner.
  - A grouping scratch does not reproduce refuses by name. A scratch where
    everything has one owner would accept what the target refuses.

Why the default must be "compared": each measured property below is one a
creation in another object reads, and a hand-written list of what to compare
missed each in turn.

| Property | What reads it |
|---|---|
| A view's or function's `is_schema_bound` | A schema-bound caller refuses one that is not (4513) and may not name a synonym (2788) [`schema_bound_references_are_held_by_the_engine_and_plain_ones_are_not`] |
| A table-valued function's result columns | A view over the function binds them, and breaks (207) when they change [`a_table_valued_functions_result_columns_are_what_a_view_over_it_binds`] |
| A table's primary key, unique constraints and unique indexes | A foreign key binds one by `key_index_id`, and is refused without one (1776), as #1673 compares them on PostgreSQL [`a_foreign_key_binds_a_candidate_key_and_a_persisted_column_a_deterministic_function`] |
| A function's `IsDeterministic`, `IsPrecise` | A `PERSISTED` computed column refuses a function that is not deterministic (4936); no signature shows it |
| A column's type, a user type by `schema.name` | A schema-bound view and a computed column read it at creation |
| Whether two objects share an effective owner | A `PERSISTED` computed column refuses a function owned by another principal than the table (4988) [`a_persisted_column_needs_its_functions_owner_to_be_the_tables`] |

**Cascade legality remains the engine's decision.** For each managed DELETE
or UPDATE CASCADE edge, traverse external CASCADE edges in both directions to
include the connected closure, including external-to-external paths. Preserve
column mappings, keys, exact actions and effective-owner equality. Include the
INSTEAD OF event metadata on a referencing table that blocks the matching
cascade (1787); a trigger on the parent or for another action is not a barrier.
Noncascading edges and disconnected action paths add no barrier. The staged
comparison refuses a missing or different prerequisite before accepting shapes;
SQL Server itself decides multiple-path legality (1785).

A three-part or four-part reference refuses by name: the externals file stages
objects in one database and cannot stand in for another. The single named
multipart check reads `sys.sql_expression_dependencies` before interpreting
referenced IDs, including qualifiers naming the current database and deferred
missing user objects. For synonyms it resolves `sys.synonyms.base_object_name`
with `PARSENAME`; a database or server component refuses even without a
dependency row. No source parser is used (DEC-2268.1).

**Accepted residual:** SQL Server does not record references to system objects
in another database's `sys` schema, such as `master.sys.objects`. Build equality
makes those system objects identical on the qualified instances. User objects,
including `msdb.dbo.sysjobs`, are recorded and refused. Dynamic SQL is not bound
at creation and remains outside this static dependency check.

`capture::Staged` has private construction: success accounts for every supplied
post-file inventory root and accepts the corresponding target shapes. Its
`changed_since` check compares those accepted shapes against the final target
capture, using its exported scope. Retained OBJECTPROPERTYEX facts belong to
the canonical `mssql-capture-v4` shape and its sealed prerequisites, so later
rechecks do not lose facts the staged comparison used. The CLI prepares and accepts externals, compiles the declarations as the
reproduced deployer (§5) and reads the compiled scratch and the target, then
stops at the named boundary: planning from evidence (#2143). No planning or
binding certificate is fabricated.

System-named keys, CHECKs, DEFAULTs, foreign keys and key-backed indexes use
canonical declaration identities (DEC-2375.1). Private CHECK definitions stay
inside capture; their public identities are domain-separated HMAC tokens under
the environment key fixed before the read. Diagnostics name the table, kind
and an eight-character token prefix, and suggest naming the constraint. Two
identical unnamed declarations on one table refuse rather than merge.

**File execution and rendering (DEC-2269.1).** Preparation inventories the
bounded scratch after generated database authorization setup, executes the
file on a separate connection entered into the same sandbox WITH NO REVERT,
inventories again, renders missing required externals, and inventories a third
time. The inspection session retains its owner context independently of file
DENYs and session settings. Every created root, including an unreferenced
schema's object, enters the complete staged comparison. Children are carried
only when their parent was created; every other inventory class is either
compared or refused by name. Only exact generated setup identities are excluded.

Standalone GO lines are split outside single/double quotes, bracketed
identifiers, line comments and nested block comments. Trailing comments remain
comments in the adjacent batch, including multiline comments. GO repetition
and sqlcmd directives/variables refuse before execution. No shell client runs.
Partial commits, batch failures, comparison refusals and cancellation all use
the recorded owned cleanup; a wrapping transaction cannot contain dynamic EXEC
that commits it. File sessions close before cleanup acquires a fresh bounded
connection.

The pure renderer uses catalog structure with quoted identifiers. Its bounded
templates cover alias types, ordinary table columns (including identity),
ordinary unique B-tree keys/indexes, nullable typed views, sequences, scalar
routines whose semantic flags the constant template reproduces, and external
CASCADE constraints. Complete comparison rejects every unreproduced setting
or future scalar property. A non-nullable view, computed column, filtered
unique index, unsupported routine property or other unqualified class refuses
by name; the operator may supply a matching file. Original module bodies,
assemblies and cross-database text are never replayed. File ownership is not
repaired. Rendered objects receive their mapped ownership before ACL replay,
and the entered compile session verifies the resulting authorization.

Supplied cleanup carries class-6 ACLs with the exact type it will drop
(DEC-2269.2). Independently revoking a grant-option chain can invalidate a
later row's recorded grantor and report 15151; DROP TYPE removes the ACLs
after dependent objects have been removed. An unreadable type identity or a
permission without a recorded type drop still refuses. The final inventory
read must be empty before the sandbox user is removed.

An external or cascade-closure object which needs a managed object becomes a
native reconstruction step (DEC-2376.1). The shared scratch blueprint places
it immediately after its last managed prerequisite, and its renderer template
runs on the setup connection. It is then compared and merged into `Staged`;
a prerequisite that reconstruction never creates refuses by both names.
The legal chain `a -> managed -> x` cannot run earlier (measured error 1767),
and the file may not invent managed objects during setup.

A complete `-- pbps:deferred` marker line outside strings and comments splits
the externals file. At most one marker is permitted. The prefix runs during
setup; the suffix runs on the setup connection after every managed step,
then is inventoried and compared. For example:

```sql
CREATE TABLE dbo.a(i int CONSTRAINT a_pk PRIMARY KEY);
CREATE TABLE dbo.x(i int CONSTRAINT x_pk PRIMARY KEY);
-- pbps:deferred
ALTER TABLE dbo.x ADD CONSTRAINT x_managed
    FOREIGN KEY(i) REFERENCES dbo.managed(i) ON DELETE CASCADE;
```

The managed declarations supply `dbo.managed`; the deferred file creates only
its externals. Deferred statements do not interleave with managed steps.
A failure names the numbered deferred statement, and partially created objects
remain in the cleanup inventory. Same-instance file policy remains #2270's;
preparation still does not publish evidence before #2143.

The target capture compares objects by the same rule: every column each catalog view reports, minus a reasoned skip list ([DEC-2153.1](decisions/resolver.md#dec-2153-1)).

The SQL Server server entry accepts `same_instance: refuse | allow`, defaulting
omission to `refuse`. PostgreSQL rejects an explicit setting; Docker rejects
the field. A target-database scratch always refuses. A same-instance supplied
scratch without a file uses only catalog-rendered externals; with a file,
including an empty file, it requires `allow`. All containment and complete
staged comparisons still apply. Another instance uses the ordinary gates;
`outside_writes` is independent.

The shared runtime seals `same_instance: refused | allowed`; explain shows
both values in human and JSON output. Legacy PostgreSQL omission reads as
`NotApplicable` and reserializes unchanged. The first SQL Server evidence
reader (#2143) must require an applicable value and accept only
`mssql-reported-scope-v2`: missing provenance is never consent (DEC-2270.1).

## 9. Compatibility on reported facts

`mssql-reported-scope-v2` retains #611 and #1636's non-assembly facts:

- product version, level and family;
- server and database collation, compatibility level, containment and the
  ANSI settings;
- an edition difference is a recorded limitation;
- a hosted family refuses by name.

The rule stops comparing user CLR assemblies ([DEC-619.4](decisions/resolver.md#dec-619-4)).
A CLR function, procedure, aggregate, type or trigger that a managed object
reaches is an external object. The externals file cannot stage it without
the assembly's binary, so it refuses by name as an unstaged external object
(#1517). An assembly nothing reaches binds nothing. #721 (an assembly's
identity covers only its manifest file) and #718 (an assembly the deployer
cannot see reads as absent) are closed with that change in #2270.

A LocalDB scratch reports `host_platform` Windows and is compared like any
other instance: a Linux target paired with it is a named platform mismatch,
and supporting that pair, or a major other than 17, is a separate rule
decision (#2271, DEC-2271.2).

#719's telemetry, long-running runtime and Express questions remain
prerequisites before the complete SQL Server path is advertised.

## ADR-0016 cases on SQL Server

Cases 12, 13, 19, 20 and 21 are measured-profile only and are not mapped.
Fixture names are the tests #620 adds.

| Case | SQL Server | Owner |
|---|---|---|
| 1 | The rebuild half does not apply: a rebuild cannot pin a runtime-bound name (§2). Becomes `mssql_an_object_the_plan_creates_that_captures_a_reference_is_reported` (managed module: reported; unmanaged: refused). | #620 (a), (c) |
| 2 | `mssql_each_surface_gets_the_engines_answer`: module-schema-then-`dbo` candidates, qualified and schema-bound references, computed columns, defaults, CHECKs, types, synonyms; no overloads in T-SQL. | #620 (a) |
| 3 | `mssql_a_schema_bound_binding_is_the_targets_observed_one` for id-bound references. Runtime-bound names have no history to differ (§1). Cross-database references refuse by name. | #620 (a) |
| 4 | `mssql_unreadable_dependency_metadata_never_verifies`: no `VIEW DEFINITION`, a hidden object, a procedure's deferred name. | #620 (a) |
| 5 | #611's rule; #620's compatibility tests. | #620 (b) |
| 6 | Shared; #620's "ordinary, offline and no-pull behaviour is unchanged". | #620 |
| 7 | Shared evidence; the SQL Server evidence reader. | #620 (c) |
| 8 | `mssql_a_resolved_plan_applies_without_docker` and its wrong-binding rollback. | #620 (d) |
| 9 | DEC-619.1's form: `a_commit_between_component_reads_reaches_a_later_read_or_follows_the_earlier_one` (here) and #620's two-session capture fixture. | #619, #620 (a) |
| 10 | #621's plaintext trace test. | #621 |
| 11 | The separation refusals of §6 and §7. | #620 (b) |
| 14 | A reconnect on scratch invalidates the run; compatibility inputs only. | #620 (c) |
| 15 | `mssql_an_external_property_change_invalidates_the_plan`: a synonym retargeted, a function made schema-bound, a type's precision. | #620 (d) |
| 16 | `mssql_scratch_reproduces_the_deployers_default_schema` (§2). | #620 (b) |
| 17, 18 | Engine-neutral confidential-artifact handling; no SQL Server fixture. | #621 |
| 22 | `mssql_a_resolved_plan_rolls_back_when_its_closing_check_finds_another_writer`. | #620 (d) |
| 23 | The compatibility comparison on reported facts (§9). | #620 (b) |
| 24 | `mssql_schema_bound_dependents_are_torn_down_and_rebuilt_around_their_target` (§5), through `settle`. | #620 (a), (c) |

## #620's four sub-issues

- **(a) Capture and ownership.**
  - The candidate reads of §2:
    - for an object reference, the module's schema and `dbo`, every object
      type;
    - for a procedure call, the executing principals' default schemas and
      `dbo`, unless `sys` holds the name.
  - The bindings of §1, with `VIEW DEFINITION` required.
  - Ownership from the start, sealed under the environment key.
  - Plain component reads, compared (DEC-619.1).
  - The schema-bound `engine_effects` of §5.
- **(b) Separation, reconstruction and assessment.**
  - The marker, the sandbox and the three reads before the first write,
    and the layout rule of §6.
  - On the target's instance:
    - the rendered externals, with only rendered DDL sent;
    - the `same_instance` switch, sealed and shown by `explain`.
  - #723's inventory check, run as `db_owner` (§7), and emptying the
    supplied layout.
  - The externals shapes of §8.
  - The deployer context, including the default schema.
  - The CLR change of §9.
- **(c) Planning and evidence.** The SQL Server arm of
  `engine::validate_plan_analysis` and its evidence reader. Report what a
  plan-created object captures (§2). `master`'s `family_guid` is sealed as the
  target identity.
- **(d) Apply checks.** #616's checks in the apply transaction, with the
  closing capture after the last statement. Lift `resolver_apply_limitation`
  for SQL Server once (a)–(c) are in.
