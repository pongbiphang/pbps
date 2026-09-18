# ADR-0010: Privileges on PostgreSQL — the role is not the portable unit

- Status: accepted; the model changes (DECISIONS 210–211) and scoped PostgreSQL
  privilege support are implemented. Phase 5 step 6 (#81) built the engine
  paths; step 10 (#85) and its follow-ups connected the CLI checks. The Limits
  retain unmodelled grants and the boundary around cluster roles.
- Date: 2026-09-04
- Related: docs/SPEC.md §5, §8.1, §8.2, §12, open questions 7 and 9;
  [ADR-0005](ADR-0005-roles-and-grants.md), which this revisits;
  [ADR-0009](ADR-0009-postgres-modules.md)

ADR-0005 closed by naming its own PostgreSQL collision: *"PostgreSQL variance
(default privileges, schema and sequence grants) is Phase 5 touchstone material,
alongside function overloading from ADR-0002."* This is that material, measured.

It is worse than the note implies. Function overloading (ADR-0009) costs one
key type. This one costs a **premise**: ADR-0005's design rests on a line
between portable roles and environment-local principals, and PostgreSQL does not
draw that line anywhere.

## The verdict, up front

| ADR-0005 held that | On PostgreSQL |
|---|---|
| The portable unit is the **database role**; logins and users are server-level and each environment's own | **The distinction does not exist.** A role is cluster-level and `LOGIN` is an attribute of it |
| `schema::dbo` grants every object in a schema, **present and future** | **False.** The "future" half needs `ALTER DEFAULT PRIVILEGES`, which is keyed to a *principal* |
| A grant on a table lets the grantee read the table | **False** without `USAGE` on the schema. The grant is real, recorded, and inert |
| The closed set `select insert update delete references execute alter view-definition` | `alter` and `view-definition` **do not exist**; `usage`, `truncate`, `trigger`, `create`, `maintain` do |
| A role is refused a drop when it **owns** something | Broader: **holding a privilege** blocks the drop too |
| An object's grants are a set of rows | A **NULL** ACL means "the built-in default applies", which for functions grants `EXECUTE` to `PUBLIC` |

Two of these turn a role that `verify` calls clean into a role that cannot do
its job, and one turns it into a role that can do far more than declared. Those
are the two directions this tool exists to keep closed.

## What was measured, and against what

Everything marked **measured** was run on 2026-09-04 against:

- **PostgreSQL 18.6** —
  `docker.io/library/postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280`
- **SQL Server 2025** —
  `mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1`,
  the digest CI and `scripts/live-tests.sh` pin, for the contrast rows.

Anything not marked measured is reasoning and says so.

## 1. `USAGE` is the grant that makes the other grants real

**Measured.** A role with `SELECT` on a table, and nothing on its schema:

```
=> GRANT SELECT ON app.customer TO app_reader;
=> SELECT has_table_privilege('app_reader','app.customer','SELECT') AS has_select,
          has_schema_privilege('app_reader','app','USAGE')          AS has_usage;
 has_select | has_usage
------------+-----------
 t          | f

=> SET ROLE app_reader; SELECT count(*) FROM app.customer;
ERROR:  permission denied for schema app
```

The catalog says the permission is there. The engine says no. A `role:` file
that declares exactly what ADR-0005's own example declares —

```yaml
role: app_reader
grants:
  app.customer: [select]
```

— produces, on PostgreSQL, a role that introspects clean, drifts by nothing,
and cannot read the table. That is CLAUDE.md's rule about absent, empty and
unreadable being three different things, arriving as a permission model rather
than as a file read.

**Decision.** `usage` joins the permission set, and `Dialect::validate_role`
refuses — not warns — a PostgreSQL role that is granted on an object in a schema
it has no `usage` on, naming the line to add. The check has a home already:
ADR-0005 note 11 built `validate_role` to compare a permission against the kind
of its target, measured pair by pair against the engine's own table.

**Why not add the `USAGE` automatically.** It is one line of emitter code and it
is the wrong line. The tool's whole claim is that the reviewed plan is the
change: a `GRANT USAGE` that appears in the plan because pbps inferred it is a
permission nobody declared, nobody reviewed in the merge request, and nobody can
find in git afterwards when an auditor asks who widened access. Refusing costs
the user one line of YAML and keeps the audit trail true.

## 2. "Present and future" is not portable, because the future has an owner

ADR-0005 gives `GrantTarget::Schema` this documented meaning: *"Every object in
a schema, present and future."*

**Measured, SQL Server** — that is exactly right:

```
GRANT SELECT ON SCHEMA::dbo TO app_reader;   -- then CREATE TABLE dbo.later
existing table: yes
table created after the grant: yes           -- and a real SELECT on it succeeded
```

recorded as one standing row, `class_desc = SCHEMA`.

**Measured, PostgreSQL** — it is not:

```
GRANT SELECT ON ALL TABLES IN SCHEMA app TO all_reader;   -- then CREATE TABLE app.later
 existing_table | table_created_after
----------------+---------------------
 t              | f
```

`ON ALL TABLES` is a one-shot expansion over the objects that exist at that
moment. The only PostgreSQL construct carrying the *standing* meaning is
`ALTER DEFAULT PRIVILEGES`, and **measured**, it is keyed to whoever creates the
object:

```
ALTER DEFAULT PRIVILEGES FOR ROLE owner_a IN SCHEMA app GRANT SELECT ON TABLES TO all_reader;
 created_by_owner_a | created_by_owner_b
--------------------+--------------------
 t                  | f

SELECT defaclrole::regrole, defaclnamespace::regnamespace, defaclacl FROM pg_default_acl;
 owner_a | app | {all_reader=r/owner_a}
```

So rendering `schema::app: [select]` on PostgreSQL requires naming a principal
in the emitted SQL — and **which principal is environment-local**, which is the
single fact ADR-0005 was designed around ("Naive GRANT-as-code dies on one fact:
principals are environment-specific"). A declaration cannot supply it, and a
tool that guessed it would be writing a grant whose scope depends on which
deployment account happened to run the last migration.

**Decision.** On PostgreSQL, a `schema::` target carries only the permissions
PostgreSQL defines on a schema — `usage` and `create` — and a *table*
permission on a schema target is refused by `validate_role`, naming the
object-level grants that express it. `GrantTarget::Schema` therefore survives,
with its meaning intact on each engine, and the construct that cannot be
expressed is refused rather than approximated.

**Why not redefine `schema::` per dialect** as "every managed object in the
schema, re-expanded on every plan" — which is expressible, needs no principal,
and is a pure function of the declarations. Because it means something
*different*, and the same words in the same file would then grant a
newly-arrived unmanaged table on one engine and not on the other. If that
construct is wanted it deserves its own spelling and its own ADR; quietly
reusing this one would put a portability trap into the format, and the format is
the most expensive thing here to change late.

## 3. A role is a cluster object, and a user is a role

**Measured.** A role created while connected to `postgres` is visible from a
different database of the same cluster:

```
\connect other
SELECT rolname FROM pg_roles WHERE rolname IN ('app_reader','all_reader');
 all_reader
 app_reader
```

There is no database-scoped role in PostgreSQL, and no separate "user" object:
`CREATE USER` is `CREATE ROLE … LOGIN`. ADR-0005's table —

| | Managed by pbps | Stays environment-local |
|---|---|---|
| Database roles | Yes — existence and grants | |
| Logins, users | | Yes — server-level, per-environment |

— has no PostgreSQL reading. Its top row and its bottom row are the same kind of
object.

**Decision: on PostgreSQL, pbps does not create, rename or drop roles.** It
manages what a role is granted **inside the database it is connected to**, and a
declaration naming a role the cluster does not have is refused by `plan --db`
with the `CREATE ROLE` to run by hand — the same shape ADR-0005 already uses for
logins on SQL Server, applied one level out.

This is a *tightening* of ADR-0005 under ADR-0005's own criterion, not an
exception to it. The criterion is "does drop + add destroy state that lives only
in the environment and cannot be restored from the declarations?" On SQL Server
the answer was "yes — membership". On PostgreSQL the answer is "yes, and also
every privilege that role holds in **every other database of the cluster**,
which this connection cannot even see". A tool whose blast radius is one
database must not own an object whose blast radius is the cluster.

Two consequences worth stating:

- **Roles keep their `r_` uid, and it still earns its place.** Not for renames —
  there are none here — but as the managed-set marker: ADR-0005 note 3 makes "a
  role the ids file does not name is unmanaged" the rule that stops a plan
  revoking permissions the declarations were never allowed to name. That rule is
  load-bearing on PostgreSQL too.
- **`drop-role` changes meaning, and must say so.** On PostgreSQL it revokes
  every declared grant and removes the role from the managed set, then names the
  `DROP ROLE` for a human. It cannot mean "the role is gone", and a command that
  silently meant less than its name would be worse than one that explains
  itself.

## 4. What blocks a `DROP ROLE` is broader than ownership

ADR-0005 note 10 built a pre-drop check for SQL Server: a connected plan refuses
to drop a role that **owns** anything, listing the classes the catalog can
assign an owner to.

**Measured, PostgreSQL** — ownership is not the only blocker. A role that owns
nothing at all and merely *holds a grant* cannot be dropped:

```
=> DROP ROLE app_reader;
ERROR:  role "app_reader" cannot be dropped because some objects depend on it
DETAIL:  privileges for table app.customer
```

and a role that does own things is refused with every reason listed at once —
schema privileges, a default-privileges entry, and a table:

```
=> DROP ROLE owner_a;
ERROR:  role "owner_a" cannot be dropped because some objects depend on it
DETAIL:  privileges for schema app
owner of default privileges on new relations belonging to role owner_a in schema app
owner of table app.by_a
```

**Measured, SQL Server** — the contrast is total. A role holding an outstanding
`GRANT SELECT ON SCHEMA::dbo`, owning nothing, dropped without complaint:

```
role dropped while its GRANT on schema::dbo was outstanding
```

§3 puts role drops out of scope on PostgreSQL, so this does not become a new
gate. It becomes **advice that has to be right**: `pull` and `doctor` report why
a role cannot be dropped, and the remedy named is `REVOKE` / `REASSIGN OWNED` /
`DROP OWNED BY`, not the `ALTER AUTHORIZATION` that is SQL Server's answer. It is
recorded here because ADR-0005 note 10's list is engine-shaped, and a list like
that is exactly the kind of thing that gets copied to a second dialect unread.

## 5. NULL is not empty, and the default is not nothing

**Measured.** A freshly created function, and the `public` schema:

```
SELECT proacl FROM pg_proc WHERE proname='customer';
 (null)

SELECT nspname, nspacl FROM pg_namespace WHERE nspname IN ('public','app');
 public | {pg_database_owner=UC/pg_database_owner,=U/pg_database_owner}
 app    | {postgres=UC/postgres,owner_a=UC/postgres,...,all_reader=U/postgres}
```

A NULL ACL does not mean "nobody is granted anything". It means **the built-in
default applies**, and PostgreSQL's default for a function includes `EXECUTE` to
`PUBLIC`. In `public`'s ACL the entry `=U/pg_database_owner` — grantee omitted —
*is* `PUBLIC`, holding `USAGE`.

An introspector that reads NULL as an empty set reports a function as granted to
nobody while every role in the cluster can execute it, and `verify` calls it
clean. This is the third member of CLAUDE.md's set — absent, empty and
unreadable — and it is the one that reads as good news.

**Decision, in two halves, and the second half is a correction.**

**The reader never returns an empty set for a NULL ACL.** It expands it to the
engine's documented default for that object kind, or refuses to answer. "No row
in the catalog" and "nobody is granted anything" are different facts and only
one of them is good news.

**But the default is the zero point, not drift, and not a refusal.** A first
draft of this ADR routed `PUBLIC` down the path ADR-0005 notes 4 and 15 built
for `DENY` and `WITH GRANT OPTION` — unexpressible, and `plan --db` refuses
rather than plan over it. **Measured, that deadlocks the tool on its own
output:**

```
-- a function pbps has just created, granted by nobody:
proacl IS NULL       -> t
SET ROLE v_nobody;   -- holds only USAGE on the schema
SELECT v.fresh(7);   -> 7          -- PUBLIC executed it
```

Every function pbps creates arrives with `EXECUTE` to `PUBLIC`. Under the first
draft, the very next `plan --db` would refuse, and managing functions at all
would require an undocumented manual revoke after every create. A rule that
makes the tool's own successful apply unplannable is not a safe rule; it is a
broken one.

So the managed set is drawn where ADR-0005 note 3 already draws it, one axis
further: **only grants to *managed roles* are compared.** `PUBLIC` is not a role
the ids file can name, so what it holds is outside the managed set the way an
undeclared role's grants already are — reported by `pull` and carried by
`status` as context, never as drift and never as a gate. What stays
*unexpressible*, and keeps the ADR-0005 treatment, is a deviation the model can
name no part of: `WITH GRANT OPTION` (`a*r`), column-level grants in
`pg_attribute.attacl`, and privileges outside the closed set — all of them
**on a managed role**, which is what makes them that role's business.

**The residual, stated rather than hidden — and it is sharper than "pbps stays
out of the way".** `EXECUTE` to `PUBLIC` on a function is a real exposure, and
revoking it is ordinary hardening: measured, the revoke leaves
`{postgres=X/postgres}` and PUBLIC is refused. pbps can express neither that
state nor the intent to reach it.

A first draft of this paragraph concluded that pbps therefore "will not undo it".
**Measured, that is false**, because a revocation is not a row in the ACL — it is
the *absence* of the engine's default — and [ADR-0009](ADR-0009-postgres-modules.md)
§3 rebuilds a function by drop + create — on that engine, *every* module edit
does, because which edits a replace could express is not knowable without
parsing or executing DDL:

```
REVOKE EXECUTE ON FUNCTION w.f(int) FROM PUBLIC;  -> {postgres=X/postgres}
DROP FUNCTION w.f(int); CREATE FUNCTION w.f(a int) RETURNS bigint ...;
                                                  -> NULL
SET ROLE w_nobody; SELECT w.f(7);                 -> 7
```

An ordinary return-type edit silently reopens a function somebody deliberately
closed. Comparing only managed roles keeps pbps from *reporting* the difference;
it does not keep pbps from *destroying* it.

So the two documents close it from both sides. ADR-0009 §3 refuses a rebuild
whose ACL the declarations cannot reproduce **in either direction**, which is
what stopped the regression while the gap stood.

**Amendment (issue #318): the gap is closed, and the grantee exists.**
Expressing "revoked from `PUBLIC`" needed a grantee the model did not have, and
until it had one, hardening a managed function and managing it were mutually
exclusive. `Change::PublicExecution` is that grantee, in the narrowest
shape that expresses the act and nothing else: a `RoutineId`, which of the two
decisions the plan reached, and which act brought the routine into being — no
permission set and no `GrantTarget`, because `EXECUTE` is the only default
`PUBLIC` holds on a routine and a view or a trigger cannot be one. Deliberately
**not** a `Revoke` from a role spelled `PUBLIC` — a project may declare a role
of that name, and a string holding the word would be a sentinel every reader
had to know about. The opt-in decision is recorded even though the `CREATE`
would usually have left the default standing, so that a plan with no opinion
about a routine reads differently from one that deliberately left it open; and
it writes a `GRANT EXECUTE … TO PUBLIC` of its own rather than trusting that
`CREATE`, because a cluster's default privileges can revoke it (DECISIONS 517).

Three consequences follow, and the third is the one that reverses a paragraph
above.

- **A plan that creates a routine closes it.** Not only a `SECURITY DEFINER`
  one: which routines are definer routines is inside a body this tool never
  parses, and the textual scan that would answer it is fooled by the words
  appearing in a comment or a string. A declaration writing
  `public_execute: true` is what leaves the default standing, and that line
  goes through the merge request and into the plan's checksum.
- **The opt-in is an annotation, not state**, travelling with `strategy:` and
  `depends_on:` for the reason §5 gives above: what `PUBLIC` holds is never
  compared, so a field inside the module would make a declared routine stop
  matching the identical routine read back from the catalog.
- **A rebuild no longer refuses a hand revoke.** The ADR-0009 §3 guard accepts
  exactly one missing default — `PUBLIC`'s `EXECUTE`, on a routine whose plan
  carries the revoke that re-issues it after the `CREATE`. A different
  permission, or a different grantee, is still the refusal, and the `CREATE`
  still restores the default in between; what changed is that the plan now says
  what to do about it (DECISIONS 517).

That path existing already is the reassuring part of this ADR. The mechanism for
"the engine holds a permission this model cannot describe, and silence about it
would be a lie" was built once, for SQL Server, and it absorbs PostgreSQL's
version without a new idea.

**Measured, and in the same family:** `WITH GRANT OPTION` appears as a `*` in the
ACL (`all_reader=a*r/postgres`), and column-level grants live in a different
catalog entirely (`pg_attribute.attacl`), so a reader that only looks at
`relacl` sees neither. Both are already ADR-0005's "unexpressible" cases; only
the place to look is new.

## 6. The closed permission set is SQL Server's set

**Measured.** `GRANT ALTER ON app.customer TO all_reader;` →
`ERROR: unrecognized privilege type "alter"`.

PostgreSQL has no `ALTER` privilege — altering is the owner's right and is not
grantable — and no `VIEW DEFINITION`. It has `USAGE`, `TRUNCATE`, `TRIGGER`,
`CREATE`, `CONNECT`, `TEMPORARY`, `SET` and (17+) `MAINTAIN`, none of which the
model can name.

**Decision.** `Permission` becomes the **union** of what the supported engines
grant, and `Dialect::validate_role` refuses the ones its engine lacks — the
existing `DialectError::Unsupported` path, and the existing note-11 machinery.
Not a per-dialect enum: inviolable constraint 1 needs one model in which two
semantically identical `Schema`s compare equal, and a dialect-parameterized
permission type would put the dialect inside the model, which is the boundary
the architecture is drawn to protect.

## 7. Sequences, and one decision that closes two problems

**Measured.** A role with `INSERT` on two tables, one using an identity column
and one using `serial`:

```
=> INSERT INTO app.ident (v) VALUES ('identity column');   -- INSERT 0 1
=> INSERT INTO app.ser   (v) VALUES ('serial column');
ERROR:  permission denied for sequence ser_id_seq
```

A `serial` column's sequence is a separate grantable object; an identity
column's is not reachable that way. **Measured**, the sequence is created in the
same schema (`s_id_seq`) and dropped with its table.

ADR-0009 already refuses `serial` in declarations for an unrelated reason — it
does not round-trip, reading back as `integer` plus an owned sequence. That one
refusal closes this problem too: a managed table uses `GENERATED … AS IDENTITY`,
and no sequence grant is needed to write to it. For tables `pull` finds already
using `serial`, the sequence grant is reported as unexpressible (§5), not
folded into the role.

## What this changes in the model

| Change | Size |
|---|---|
| `Permission` includes `usage`, `create`, `truncate`, `trigger`, `maintain`; unsupported words are dialect-refused | An enum and a `validate_role` table |
| `GrantTarget::Object` names a function by signature | The `ModuleId` of [ADR-0009](ADR-0009-postgres-modules.md) |
| Role *existence* is a dialect capability rather than a given | A trait method; SQL Server keeps today's answer |
| PostgreSQL catalog read-back, ACL expansion, unexpressible reporting | Implemented in `pbps-pg`; step 6 and its amendment below record the measured scope |

`Role`, its `grants` map, the ids file's `roles` section, the `revoke` and
`grant-widen` risk classes, the drift comparison and the managed-set rule are
all unchanged. As with ADR-0009, the dialect-agnostic crates hold.

## Ruled out

- **Naming a principal in a declaration** (`ALTER DEFAULT PRIVILEGES FOR ROLE
  …`). It is the only faithful rendering of "present and future" on PostgreSQL,
  and it imports the one thing ADR-0005 exists to keep out of the files.
- **Granting `USAGE` implicitly** (§1). A permission nobody declared and nobody
  reviewed.
- **Redefining `schema::` per dialect** (§2). Same spelling, different scope,
  silently.
- **Managing role existence on PostgreSQL** (§3). The object is cluster-scoped;
  the tool is database-scoped.
- **A per-dialect `Permission` type** (§6). Puts the dialect inside the model.
- **Modelling `PUBLIC` as an ordinary role** (§5). Putting it in the ids file
  with an `r_` uid would make "granted to everybody" read as one more row in a
  list, and would have pbps creating and dropping a principal the engine owns.
  A dedicated grantee — something a `role:` file can name but the ids file
  cannot mint — is the shape that would work, and it is a follow-up rather than
  a decision taken here.
- **Refusing to plan over the engine's own default** (§5). It was the first
  draft's rule, and measured, it makes a function unplannable the moment pbps
  creates one.

## Limits

- **Column-level grants stay deferred**, as in ADR-0005. The carried-state
  reader's refusal is measured for a view whose column grant lives in `attacl`,
  not its object ACL, by
  [`a_module_carrying_what_a_rebuild_would_destroy_refuses_and_names_it`](../crates/pbps-pg/tests/live.rs).
  This protects that rebuild; it does not make column grants declarable.
- **Row-level security, `SET ROLE` chaining, `pg_hba.conf`, `CONNECT` and
  database-level privileges remain outside the declaration model** — the first
  is a policy engine of its own, the rest are cluster or connection concerns.
  A diagnostic or preflight check is not support for managing those policies.
- **Sequence, type, domain and foreign-data-wrapper grants remain unmodelled.**
  [`a_grant_on_something_the_declarations_cannot_name_is_reported_not_lost`](../crates/pbps-pg/tests/live.rs)
  measures reporting for sequence, type and procedural-language grants, plus
  grants on materialized views and partitioned tables. It does not measure a
  foreign-data-wrapper grant or close the declaration gap.
- **`PUBLIC` still cannot be a declared grantee** (§5), but it is no longer
  invisible to the reader.
  [`a_null_acl_is_the_engines_default_and_is_reported_rather_than_compared`](../crates/pbps-pg/tests/live.rs)
  measures default execution and reporting of both that access and its later
  revocation. [`routine_rebuilds_do_not_restore_revoked_public_execute`](../crates/pbps-cli/tests/flow_pg.rs)
  requires planning and apply to refuse a rebuild after revocation, preserving
  the closed function. Declaring the hardening remains unsupported.
- **The `MAINTAIN` version gate is now measured on both sides of PostgreSQL 17.**
  [`maintain_is_taken_at_seventeen_and_up_and_refused_below_it`](../crates/pbps-pg/tests/live.rs)
  compares the gate with real grants on pinned PostgreSQL 18.6 and 16.15.
  [`permission_versions_are_checked_before_planning_bootstrap_apply_and_resume`](../crates/pbps-cli/tests/flow_pg.rs)
  covers the CLI gate. This answers the version-gating requirement, not the
  unmodelled privilege classes above.

## Placement

The model preparation (DECISIONS 210–211) landed before the PostgreSQL
emitter, for the same reason as ADR-0009: format is the most expensive thing
to change late (SPEC §12). Phase 5 step 6 (#81) built the PostgreSQL role/ACL
paths, and step 10 (#85) plus the doctor and permission-version follow-ups
(#304, #321) connected their CLI checks. The original measurements and the
later implementation amendment remain separate evidence.

## Amendment — what landing §3 and §6 changed

- **SQL Server refuses the five words in three places, from one table**
  (DECISIONS 210). §6 named `validate_role`; landing it, the emitter and the
  catalog read-back turned out to be the same shape — each had read "parses as
  a `Permission`" as "is this engine's", which the union broke. Measured while
  landing: on SQL Server each of the five is a parse error on `GRANT` (Msg 102,
  before the securable is looked at), and `sys.fn_builtin_permissions` names
  none of them in any class, so the read-back cannot meet one today; it filters
  anyway, because the day the catalog grows a word the model spells is the day
  a `pull` writes a role `validate` refuses.
- **`maintain` is not gated on the server version by the model.** The Limits
  entry stands: the model has no server version, so the PostgreSQL dialect's
  `validate_role` refuses `maintain` below 17 the way `plan --db` gates on SQL
  Server's edition — a connected check, not a format one.
- **The editor schema lists the union** (schema version 7), not one engine's
  words: which word a project's engine lacks is `validate`'s finding.
- **`manages_roles` initially had no reader** (DECISIONS 211). The preparation
  kept SQL Server's `true` answer and role-existence behavior. The PostgreSQL
  consumer subsequently landed with step 6: a missing role is refused with
  the `CREATE ROLE` a human runs, and dropping a declaration revokes its managed
  grants without dropping the cluster role. Step 10 connected the CLI paths.

## Amendment — what landing §1, §2, §4, §5 and §7 changed

The PostgreSQL half is built (issue #81). Everything above reproduced when it
was re-measured against 18.6, and three things were sharper than this document
had them.

- **The zero point is wider than `PUBLIC`** (DECISIONS 371). §5 argued that the
  engine's default must not be routed down the unexpressible path, because
  every function pbps creates arrives with `EXECUTE` to `PUBLIC` and the next
  plan would refuse. The same argument settles the **owner**, whom §5 does not
  name: every table pbps creates arrives owned by the deploying account with
  the owner's whole set, so comparing that set would have the plan after a
  successful apply revoke what the apply produced. The reader therefore expands
  a NULL ACL with the engine's own `acldefault` and then draws the line at who
  put an entry there — `acldefault`'s and the owner's are the zero point,
  `PUBLIC`'s is context, and the rest is a grant.
- **`manages_roles` has its reader** (DECISIONS 370), and the reading is in the
  **differ**, not only in the CLI. On a dialect that answers `false` no
  `CreateRole`, `DropRole` or `RenameRole` is built at all: a declared role is
  granted rather than created, because refusing it would refuse the only way a
  role ever comes under management here; a dropped role has its grants revoked
  and is left standing; a rename emits nothing, because an ACL entry holds the
  role's oid and every grant followed it. That last elision has a precondition
  §3 does not state and DECISIONS 377 does: the evidence of a rename is the
  **old** name's absence, not the new name's presence. With both names in the
  cluster they are two principals, and an empty plan would leave the old one
  holding everything and record the new one as holding it. The emitter keeps all five arms, and
  the three it refuses name the statement a human runs.
- **A name may be in two namespaces at once** (DECISIONS 379). §6's
  permission-vs-kind table is what makes a bare object target sound — a set
  with `execute` in it is a routine's — and landing it showed the other half:
  relations and routines are separate catalogs here, so `co.f` may be a table
  *and* a function, measured on 18.6. The offline check reads the namespace off
  the permission set exactly as the emitter does; reading the relation first
  refused a `GRANT EXECUTE` the engine runs.
- **A routine grant has one spelling here, and it is the signature**
  (DECISIONS 381). The bare name the engine accepts where nothing overloads is
  not a spelling the catalog can give back — `pg_proc` holds the arguments and
  nothing remembers the statement — so a declaration using it would differ from
  the database on every plan. Refused offline, the mirror of the signature
  refusal on the engine where nothing overloads.
- **§1 has one exception, and it is the schema everything lands in by
  default** (DECISIONS 383). `initdb` grants `USAGE` on `public` to PUBLIC in
  every database, measured — and PUBLIC is not a role a project can declare
  (§5), so no `schema::public: [usage]` line could appear in a pull. Requiring
  one refused every project whose tables live where PostgreSQL puts them.
- **`maintain` needs two servers to test at all** (DECISIONS 374). The
  amendment above set the rule and this is what obeying it costs: the live
  suite and the `live-pg` CI job now start a pinned PostgreSQL 16 beside the
  pinned 18, because "refuses the word" and "takes the word" are two different
  servers.

**The gap §5 named is still open.** Expressing "revoked from `PUBLIC`" needs a
grantee the model does not have. The pull now *reports* both halves — the
routines `PUBLIC` can execute and the routines it no longer can, the second
being the absence of a row rather than a row — so the state is visible; it is
still not declarable, and ADR-0009 §3's rebuild refusal is still what stops a
module edit from undoing it.
