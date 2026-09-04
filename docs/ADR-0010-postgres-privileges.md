# ADR-0010: Privileges on PostgreSQL — the role is not the portable unit

- Status: proposed. Phase 5 design; nothing is built.
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

**The residual, stated rather than hidden.** `EXECUTE` to `PUBLIC` on a function
is a real exposure, and revoking it is ordinary hardening — measured, the revoke
leaves `{postgres=X/postgres}` and PUBLIC is refused. pbps cannot express either
that state or the intent to reach it, so a project that wants it does the revoke
outside pbps, and pbps will not undo it (it compares only managed roles) nor
report it. That is a gap, not a design: expressing "revoked from `PUBLIC`" needs
a grantee the model does not have, and it should get one before this engine's
privileges are called done.

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
| `Permission` gains `usage`, `create`, `truncate`, `trigger`, `maintain`; `alter` and `view-definition` become dialect-refused | An enum and a `validate_role` table |
| `GrantTarget::Object` must name a function by signature | The `ModuleId` of [ADR-0009](ADR-0009-postgres-modules.md) |
| Role *existence* becomes a dialect capability rather than a given | A trait method; SQL Server keeps today's answer |
| PostgreSQL catalog read-back, ACL expansion, unexpressible reporting | `pbps-postgres`, which does not exist yet |

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

- **Column-level grants stay deferred**, as in ADR-0005, and are reported as
  unexpressible. PostgreSQL estates use them more than SQL Server ones, so this
  deferral costs more here; it is still not Phase 5 work.
- **Row-level security, `SET ROLE` chaining, `pg_hba.conf`, `CONNECT` and
  database-level privileges are out of scope** — the first is a policy engine of
  its own, the rest are cluster or connection concerns.
- **Sequence, type, domain and foreign-data-wrapper grants are unmodelled** and
  reported, not folded.
- **What `PUBLIC` holds cannot be declared, and therefore cannot be revoked by
  pbps** (§5). Every function arrives executable by everyone, and hardening that
  is a step taken outside the tool which the tool will neither undo nor mention.
  This is the largest gap in this document and it should close before PostgreSQL
  privileges are called finished.
- **The `MAINTAIN` privilege is PostgreSQL 17+**; the dialect must gate it on the
  server version the way `plan --db` already gates on SQL Server's edition.
- **Everything here is proposed.** No PostgreSQL dialect exists, and every
  decision above is falsifiable by the live suite that has to accompany it.

## Placement

Phase 5, ahead of the emitter, for the same reason as ADR-0009: the parts that
touch `Permission`, `GrantTarget` and the ids file are format, and format is the
most expensive thing in this project to change late (SPEC §12).
