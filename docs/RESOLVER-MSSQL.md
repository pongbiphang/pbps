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

- **Inside a module** a one-part name looks in the module's own schema, then
  in `dbo`. Neither the caller's default schema nor `EXECUTE AS` changes it
  [`a_modules_one_part_names_resolve_in_its_schema_and_a_statements_in_the_users`].
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
there is no rebuild decision to make for it. Its candidates are the same name
in the module's schema and in `dbo`, of any object type. Both enter the
manifest, and the resolver reports what each plan-created object captures:

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
- Reading `sys.sql_expression_dependencies` needs `VIEW DEFINITION` on the
  database: without it the view reads empty, which cannot be told from no
  references. The capture requires the permission and refuses without it.

## 5. Reconstruction

- **The deployer.** Scratch reproduces it with #611's context, through
  `EXECUTE AS USER`: its default schema, which statement-level names need
  (§2), and its role memberships.
- **Fingerprints.** A module's fingerprint carries `uses_ansi_nulls`,
  `uses_quoted_identifier` and `is_schema_bound`. The database collation and
  compatibility level are compared as reported facts (`mssql-reported-scope-v1`).
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
- **A confined account.** A login reads its own server reach without
  privilege, from `fn_my_permissions(NULL, 'SERVER')`, `sys.login_token`
  and its own rows in `sys.server_permissions`
  [`a_login_reads_its_own_server_reach_without_privilege`]:
  - Confined means server permissions of `CONNECT SQL` and
    `VIEW ANY DATABASE` only, no server role but `public`, and no grant to
    any principal in its token on a server-scoped securable beyond
    `public`'s defaults (those two and `CONNECT` on the endpoints).
  - The third read is needed: a login granted `IMPERSONATE` on an
    administrator's login reads exactly as a confined one in the first two,
    and becomes `sysadmin` by `EXECUTE AS LOGIN`. Its grant row shows it.
  - `securityadmin` adds `ALTER ANY LOGIN` and `CREATE LOGIN`; `dbcreator`
    adds `CREATE ANY DATABASE`; `sysadmin` holds `CONTROL SERVER`.
  - **And it enters no other database.** Confined means that `HAS_DBACCESS`
    reads 1 only for its own scratch database and `master`, `tempdb` and
    `msdb`, over every database `VIEW ANY DATABASE` lists. A login mapped
    into the target, as its owner or any user, reads 1 there, and so does
    every login on a target with `guest` enabled. Neither shows in a
    server-level read, and either would let a scratch statement reach the
    target by `USE` or a three-part name. This is #1667's SQL Server half:
    no server-level permission and no access to another database
    [`a_login_reads_every_database_it_can_enter`].
  - **In those three it is `guest`.** Access to `master`, `tempdb` and
    `msdb` is the same 1 whether the login enters as `guest` or as a user
    mapped there as `db_owner`. So the login also has no principal of its
    own in any of the three, which it reads from their
    `sys.database_principals` by its own SID.
- **The layout.** The run-owned layout creates the run's login and database,
  so it needs `ALTER ANY LOGIN` and `CREATE ANY DATABASE`, which no confined
  account holds. On the target's instance only the supplied layout runs: a
  confined account in its own pre-created database. The run-owned layout runs
  on another instance.

## 7. An empty scratch database

A database born from an unmodified `model` on the pinned build holds the
fresh-install inventory
[`a_database_born_from_model_holds_only_the_fresh_install_inventory`]:

- no user object;
- the 13 schemas: `dbo`, `guest`, `INFORMATION_SCHEMA`, `sys` and the nine
  fixed-role schemas;
- the 14 principals: `public`, `dbo`, `guest`, `INFORMATION_SCHEMA`, `sys`
  and the nine fixed roles;
- one role membership, `db_owner` ← `dbo`;
- permissions: `public`'s 2 database grants and 232 grants on system objects,
  and `dbo`'s one database grant;
- no user type, assembly, synonym, XML schema collection, full-text catalog
  or database DDL trigger, and no database-scoped configuration away from its
  default.

#723's check compares the newborn database with this inventory, before any
scratch write. A table or user added to `model` appears in the next
`CREATE DATABASE`, measured on #619.

The check reads as the database's `db_owner`. Metadata visibility hides what
a database holds from any other account, which reads no user object where one
exists [`an_account_that_is_not_db_owner_cannot_see_what_its_database_holds`].
So the supplied layout's account must be `db_owner` of its database, and a
check that cannot read as one refuses.

The supplied layout is emptied after a run, and before one, as `db_owner`.
SQL Server has no `DROP OWNED`, so pbps drops what the run created in reverse
dependency order: schema-bound dependents first, then modules, tables, types,
synonyms, schemas and users. A database that does not read as the inventory
afterwards refuses the next run.

## 8. The externals file on SQL Server

#1673's rule (DEC-1673.1), with SQL Server's shapes
([DEC-619.4](decisions/resolver.md#dec-619-4)). Every object the externals
file creates is compared with the target on what a creation-time binding can
read:

| Class | Compared |
|---|---|
| Any object | Its schema, name and object type: every kind occupies the name (§2) |
| Table, view | Columns: name, order, type (a user type by `schema.name`), length, precision, scale, nullability, collation, computed or not; a view's `is_schema_bound`, which a schema-bound caller requires (4513); a table's primary key, unique constraints and unique indexes (key columns and order), which a foreign key binds by `key_index_id` and is refused without (1776), as #1673 compares them on PostgreSQL |
| Function | Parameters and return type as above, kind (scalar, inline, multi-statement), `is_schema_bound`, which a schema-bound caller requires (4513), the engine's `IsDeterministic` and `IsPrecise`, which a `PERSISTED` computed column calling it requires (4936) [`a_foreign_key_binds_a_candidate_key_and_a_persisted_column_a_deterministic_function`], and a table-valued function's result columns as a table's, which a view over it binds [`a_table_valued_functions_result_columns_are_what_a_view_over_it_binds`] |
| Procedure | Parameters as above |
| User type | Base type, length, precision, scale, nullability |
| Synonym | `base_object_name`; a schema-bound caller may not name one (2788) |
| Schema | Name |

Not compared: module bodies and view queries (a view may be staged as a shape
view of typed NULLs), non-unique indexes, CHECKs, defaults, foreign keys and
triggers.

A three-part or four-part reference refuses by name: the externals file stages
objects in one database and cannot stand in for another.

## 9. Compatibility on reported facts

`mssql-reported-scope-v1` stays as #611 and #1636 left it:

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
cannot see reads as absent) are closed with that change in #620 (b).

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
  - The candidate reads of §2: the module's schema and `dbo`, every object
    type.
  - The bindings of §1, with `VIEW DEFINITION` required.
  - Ownership from the start, sealed under the environment key.
  - Plain component reads, compared (DEC-619.1).
  - The schema-bound `engine_effects` of §5.
- **(b) Separation, reconstruction and assessment.**
  - The marker, the confined-account reads (server and database reach) and
    the layout rule of §6.
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
