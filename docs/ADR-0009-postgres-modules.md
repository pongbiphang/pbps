# ADR-0009: Modules on PostgreSQL — overloading, deparsing, and what `CREATE OR ALTER` was buying

- Status: proposed. Phase 5 design; nothing is built. This decides the model
  before the dialect is written.
- Date: 2026-09-04
- Related: docs/SPEC.md §8.2, §12, open question 9;
  [ADR-0002](ADR-0002-module-model.md), which this revisits;
  [ADR-0005](ADR-0005-roles-and-grants.md);
  [ADR-0010](ADR-0010-postgres-privileges.md)

## Why this is written before any PostgreSQL code

SPEC §12 states the test Phase 5 exists to run: *"If Phase 5 forces a large
change to `pbps-model`, the Phase 0 abstraction was drawn in the wrong place."*
A test is only worth having if it is run while failing it is still cheap.
ADR-0002 recorded the collision — "PostgreSQL identifies a function by name
**plus argument types**, so 'the name is the identity' needs revisiting" — and
deferred it. Deferring it again means writing an emitter against a model that
cannot hold the objects it emits.

The verdict, up front:

| ADR-0002 held that | On PostgreSQL |
|---|---|
| A module's name is its identity | **False for functions and procedures.** The key of `Schema::modules` has to change |
| Modules share one namespace with tables | **Half true.** Views do; functions and procedures do not |
| The stored definition comes back verbatim, so the round trip is near-exact | **False for views**, and false for functions written in standard syntax |
| `CREATE OR ALTER` avoids drop + create, and so preserves grants | **Not for sale.** PostgreSQL's replace can only *append* view columns |
| The schema-bound ordering problem is one deferred corner case | **It is the common case** |

One model change, one new read-back rule, and one bargain that lapses. The
bargain is affordable only because ADR-0005 has since shipped — see §3.

## What was measured, and against what

Everything marked **measured** below was run on 2026-09-04 against:

- **PostgreSQL 18.6** —
  `docker.io/library/postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280`
- **SQL Server 2025** —
  `mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1`,
  the digest `scripts/live-tests.sh` and CI already pin, for the contrast rows
  only.

Everything else is reasoning and says so. This separation is the house rule
(CLAUDE.md, "measure against a real engine before believing yourself"), and it
earned its keep here: two of the five verdicts above were **not** what the
author expected before running them, and one of those two — §3 — changes a
decision ADR-0002 made deliberately.

## 1. A function's name is not its identity

**Measured.** Three functions and one procedure, all called `app.f`, coexist:

| Declared as | The catalog's own spelling of the identity |
|---|---|
| `f(a int)` | `app.f(integer)` |
| `f(a text)` | `app.f(text)` |
| `f(a varchar, b "char")` | `app.f(character varying,"char")` |
| `PROCEDURE f(a date)` | `app.f(date)`, `prokind = 'p'` |

and every operation on one of them refuses a bare name:

```
=> DROP FUNCTION app.f;
ERROR:  function name "app.f" is not unique
HINT:  Specify the argument list to select the function unambiguously.

=> GRANT EXECUTE ON FUNCTION app.f TO all_reader;
ERROR:  function name "app.f" is not unique
```

Three consequences, each measured rather than deduced:

1. **The engine normalizes the signature, and will hand it back.**
   `pg_get_function_identity_arguments` returns `integer` for a declared `int`
   and `character varying` for a declared `varchar`. The user may write either
   spelling — `GRANT EXECUTE ON FUNCTION app.f(int)` and
   `... app.f(integer)` were both accepted — and the engine resolves them to
   one object. **This is the lever.** §8.2's rule ("the database is the
   normalizer, never a parser here") applies to signatures exactly as it
   applies to expressions.
2. **Procedures share the overload set; tables and views do not.** `app.f(date)`
   as a `PROCEDURE` coexists with the functions in `pg_proc`. Meanwhile
   `CREATE VIEW app.customer` over an existing table `app.customer` failed with
   `relation "customer" already exists`, and `CREATE FUNCTION app.customer(int)`
   **succeeded**. So ADR-0002's `ObjectName` — one type, one namespace, and
   `check_names` as its consequence — describes `pg_class` and not `pg_proc`.
3. **`CREATE OR REPLACE` adds where a user means replace.** Declaring
   `h(a int)` and then editing it to `h(a bigint)` leaves **both**:

   ```
   => CREATE OR REPLACE FUNCTION app.h(a int)    ...;
   => CREATE OR REPLACE FUNCTION app.h(a bigint) ...;
   => SELECT p.oid::regprocedure FROM pg_proc p ...;
    app.h(integer)
    app.h(bigint)
   ```

   On SQL Server the same edit is one `CREATE OR ALTER` and one object. On
   PostgreSQL an emitter that renders the declaration and stops has silently
   left a second, still-callable function behind. This is the shape the tool
   exists to prevent, and it is only preventable if the **baseline** records
   the old signature — which it does exactly when the signature is the key.

### The decision

**The key of `Schema::modules` becomes a `ModuleId` carrying the qualified name
and, for the kinds that overload, the argument types as the dialect normalizes
them.** For SQL Server the argument list is always absent, so every existing
value, file and snapshot means what it meant before.

```yaml
# schema/app.f.function.yml
function: app.f(int, text)      # the identity; `int` is folded to `integer`
definition: |
  (a integer, b text) RETURNS integer AS $$ ... $$
```

Three things this deliberately does **not** do:

- **It does not parse the definition.** The signature is declared, not
  extracted. ADR-0002 kept the parameter list inside `definition` because
  modelling parameter syntax is parsing SQL, and that holds: modes, defaults,
  `VARIADIC` and parameter *names* stay in the text and out of the model. Only
  the argument **types**, which are what the engine keys on, are lifted out.
- **It does not put the signature in a field of `Module`.** Inviolable
  constraint 2 — containers hold names, elements do not — decides this. The
  signature is part of the name, so it belongs in the map key.
- **It does not fold the signature into a string.** `app.f(int, text)` and
  `app.f(integer,text)` are the same object, and constraint 1 says two
  semantically identical `Schema`s must be `==`. A string key makes them
  unequal; a key holding normalized types makes them equal, and normalization
  is already the dialect's job (`Dialect::normalize_type`).

**The cost is duplication, and it is real.** The argument types appear in the
key and again inside `definition`, and nothing offline can check that they
agree, because checking would be parsing. What catches a disagreement is the
engine: the declared signature is what the emitter writes into `DROP FUNCTION`
and `GRANT`, and the definition is what it writes into `CREATE`, so a
mismatch produces a `CREATE` the engine refuses or an object the next plan
reports as one to drop and one to add. Both are loud, both are inside the
plan's transaction, and neither is silent — which is the bar §7.5 sets for
everything the tool cannot check statically.

## 2. What PostgreSQL gives back is not what was written

ADR-0002 recorded this as luck: *"`sys.sql_modules` stores the definition
**verbatim** … so the round-trip is near-exact and the drift false-positive
risk is lower than the constraint case."* That luck does not travel.

**Measured.** A view created as

```sql
CREATE VIEW app.active_customer AS
  SELECT customer_id, full_name
  FROM app.customer
  WHERE legacy_code IS NULL;
```

comes back reindented, and — from the *same engine, at the same moment* — in
two different spellings:

| Source | What it returns |
|---|---|
| `pg_get_viewdef(oid, true)` | `… WHERE legacy_code IS NULL;` |
| `pg_views.definition` / `pg_get_viewdef(oid, false)` | `… WHERE (legacy_code IS NULL);` |

PostgreSQL does not store view text at all: it stores a rewritten parse tree
and *deparses* it on request. Functions split into two cases, also measured:

| Body written as | `prosrc` holds |
|---|---|
| A string literal (`AS $$ … $$`) | **Verbatim**, comments and odd spacing intact |
| Standard syntax (`BEGIN ATOMIC … END`, PG 14+) | **Empty** — the body is parsed, and `pg_get_functiondef` deparses `SELECT a + 1;` back as `SELECT (a + 1);` |

Three consequences:

1. **§8.2 still holds, and is what saves this.** "The database is the
   normalizer" was never a claim that the database is a faithful store; it is a
   rule about *where the comparison lives*. Drift compares the recorded state
   against the live one, and both sides come from the deparser, so drift is
   unaffected. What breaks is the *other* comparison — declaration against
   baseline — where a hand-written definition meets a deparsed one and can
   never converge. Left alone, every `plan --db` would restate every view,
   forever: not a wrong plan, but a plan that cries wolf daily, which ADR-0002
   names as the failure to avoid.
2. **So the dialect must say which side of the comparison a text came from.**
   The decision: after a successful apply, the read-back replaces the module's
   recorded definition — as it already does — and the differ compares a
   declared definition against a *recorded* one only after passing both through
   `normalize_definition`; where they still differ and the recorded side is
   deparsed output, the change is emitted, the read-back replaces it, and the
   **next** plan is clean. Convergence in one round, not never. This is the
   same "restate one definition" cost §8.2 already accepts, paid once per
   edited module rather than once per run.
3. **A deparser is a version-dependent function.** A PostgreSQL major upgrade
   can change how it renders, and every managed view would then read back
   differently on the same unchanged database — mass phantom drift, at the
   worst possible moment. This is **reasoning, not measured**: it needs two
   engine versions and belongs in the Phase 5 live suite as an explicit case.
   The mitigation is that drift is reported, never acted on, and `verify` is
   the command that reports it; nothing auto-applies.

**Also measured, and a nastier version of the same shape:** a rename performed
by pbps changes the deparsed text of every view that references the renamed
object. Renaming `app.customer` to `app.client` rewrote the view's own stored
definition; renaming a column produced `name AS full_name` inside it. So on
PostgreSQL a table rename silently edits objects that are not in the plan.
`apply` must re-read every managed module after a plan containing a rename, or
the closing state records definitions the database no longer has and the next
`verify` reports drift the tool itself caused. SQL Server has the mirror
problem (the definition goes *stale* rather than following), which ADR-0002
already handles by impact analysis; the fix here is different and has to be
built.

## 3. `CREATE OR ALTER` was buying grant preservation. PostgreSQL will not sell it

This is the finding the author did not expect.

ADR-0002 chose `CREATE OR ALTER` over drop + create for one stated reason: it
**preserves permissions**, "a known DACPAC pain point worth naming in
comparisons". PostgreSQL's `CREATE OR REPLACE VIEW` exists, but **measured**, it
can only append:

| The edit | Result |
|---|---|
| Append a column | `CREATE VIEW` — accepted |
| Remove or reorder a column | `ERROR: cannot drop columns from view` |
| Change an output column's type | `ERROR: cannot change data type of view column "a"` |
| Rename an output column | `ERROR: cannot change name of view column "id" to "key"` |

and for functions, `CREATE OR REPLACE` refuses a changed return type
(`ERROR: cannot change return type of existing function. HINT: Use DROP
FUNCTION app.h(integer) first`) — a change that lives inside `definition`,
where the tool has refused to look, so it cannot be predicted at plan time.

**Measured**, drop + create is exactly as expensive as ADR-0002 feared:

```
-- after CREATE OR REPLACE:      {postgres=arwdDxtm/postgres,all_reader=r/postgres}
-- after DROP VIEW + CREATE VIEW: (null)
```

### Why this is affordable now, and would not have been in Phase 3.5

ADR-0002 closed its "known limitation" section with: *"Revisit this with
ADR-0005, when permissions are something pbps can carry across a drop."* That
is now true. ADR-0005 shipped, and its implementation note 9 already decides
this exact case: *"A target this plan drops and creates again is granted from
nothing … every declared permission on it is a `GRANT` after the `CREATE`."*

So the decision is: **on PostgreSQL, a module change that `CREATE OR REPLACE`
cannot express is emitted as drop + create, and the declared grants on that
module are re-emitted after it**, by the machinery ADR-0005 built. The residual
loss is named rather than hidden:

- A grant to a **declared** role comes back. Measured behaviour, existing code
  path, and it is the case the tool is for.
- A grant to an **undeclared** principal does not. It is destroyed silently by
  the engine, and pbps cannot restore what it does not model. **This must be a
  refusal, not a warning**: a connected plan that is about to drop and create a
  module reads the object's ACL first and refuses if it carries a grant the
  declarations do not hold — the same shape as ADR-0005 note 4's "`plan --db`
  refuses to plan over a role it cannot describe", and for the same reason.
  Warning and proceeding would put "the application lost access" behind a line
  of output nobody reads at 3am.

That refusal is the honest cost of this ADR, and it is worth stating plainly:
on PostgreSQL, pbps will refuse to edit a view that somebody granted access on
by hand, until that grant is either declared or removed. That is friction. The
alternative is destroying it.

## 4. The dependency refusal is the common case, not the corner

ADR-0002 recorded, as a *known limitation*, that an `ALTER` releasing a
schema-bound dependency has to run before the table change, while every other
alter has to run after — and left it unfixed because only `WITH SCHEMABINDING`
functions could reach it.

**Measured**, PostgreSQL applies that refusal to every ordinary view:

```
=> ALTER TABLE app.customer ALTER COLUMN legacy_code TYPE varchar(20);
ERROR:  cannot alter type of a column used by a view or rule
DETAIL:  rule _RETURN on view app.active_customer depends on column "legacy_code"

=> ALTER TABLE app.customer DROP COLUMN legacy_code;
ERROR:  cannot drop column legacy_code of table app.customer because other
        objects depend on it
HINT:  Use DROP ... CASCADE to drop the dependent objects too.
```

No `SCHEMABINDING` was asked for; PostgreSQL tracks the dependency because it
stored a parse tree (§2). So the corner ADR-0002 declined to fix is, on this
engine, the ordinary case of "retype a column a view selects".

Two things follow, and neither is a new mechanism:

- **The ordering rank must be per-change, not per-family.** A module the plan
  both edits and whose dependency the plan changes has to be dropped before the
  table changes and created after them. ADR-0002's brackets ("drops first,
  creates and alters last") become three ranks, with the middle one populated
  by exactly the modules whose dependencies this plan touches. On PostgreSQL
  that set is computable without parsing, from `pg_depend`, at `plan --db`
  time — which is where ADR-0002's impact machinery already asks its questions.
- **`DROP ... CASCADE` is refused outright.** It is the shortest path and it
  destroys objects nobody reviewed. The guardrail is SPEC 14.3's, and this is a
  new instance of it: the plan names every object it drops, or it does not drop.

An offline `plan` cannot see `pg_depend`, so it emits the ADR-0002 ordering and
may produce a plan the engine refuses. That is acceptable and already the
documented shape: offline plans are previews (§9.1), and the refusal is loud,
transactional and leaves nothing behind.

## What this changes in `pbps-model`

The test SPEC §12 set was "does Phase 5 force a large change". The answer:

| Change | Size |
|---|---|
| `Schema::modules` keyed by `ModuleId` instead of `ObjectName` | One key type; every dialect-agnostic user of it goes through the map |
| `GrantTarget::Object` must be able to name a function by signature (see [ADR-0010](ADR-0010-postgres-privileges.md)) | The same `ModuleId` |
| `check_names`' one-namespace rule becomes a dialect question | A trait method; MSSQL keeps today's answer |
| A dialect hook for "which module kinds overload" | Data, not behaviour |

Everything else — `Module`, `ModuleKind`, `ModuleDeps`, the ids file (modules
still carry no identity), the differ, `docs`, the policy engine — is unchanged.
**The abstraction passes its own test.** It is worth recording that the test
was run and what it cost, because "we checked" is not the same claim as "it
held", and only one of them is evidence.

## Ruled out

- **Parsing the parameter list out of `definition`.** It is the only way to
  remove the duplication in §1, and it is parsing SQL, in two dialects, for
  syntax that includes defaults and parameter modes. §8.2 forbids it and the
  cost proves the rule.
- **Moving parameters into structured model fields.** Same objection, plus it
  puts dialect-specific syntax into a dialect-agnostic crate, which is the one
  thing the architecture is drawn to prevent.
- **Refusing to manage overloaded functions** (report them as unmanaged, as
  `WITH ENCRYPTION` modules are). Cheap, and it fails the adoption argument
  ADR-0002 was written to satisfy: a tool that hands back half the routines of
  a real PostgreSQL estate forces the second tool it was meant to replace.
  Kept as the fallback if the §1 duplication proves unworkable in practice.
- **Keying modules by a signature string.** Breaks inviolable constraint 1 the
  moment a user writes `int` where the last commit wrote `integer`.
- **`DROP … CASCADE`** (§4), and **auto-applying a deparser-driven restatement**
  (§2): both trade a review for a shorter path.

## Limits

- **Materialized views, aggregates, operators, domains, casts and extensions
  are out of scope**, as their SQL Server analogues are. They matter for `pull`,
  which must inventory them as unmanaged rather than ignore them.
- **`serial` is not a type** — measured: a `serial` column reads back as
  `integer` plus an owned sequence. It cannot round-trip and the loader must
  refuse it, pointing at `GENERATED … AS IDENTITY`. Recorded here because it is
  discovered through the same read-back path; it belongs to the type catalogue,
  not to modules.
- **The deparser-version hazard (§2.3) is unmeasured** and needs two engine
  versions in the live suite.
- **Everything here is proposed.** No PostgreSQL dialect exists; this document
  is a set of decisions taken in advance so that the emitter has something to be
  written against, and each is falsifiable by the live suite that must come with
  it.

## Placement

Phase 5, ahead of the emitter. The `ModuleId` key change is a `pbps-model`
change and therefore the most expensive kind to take late (SPEC §12, Phase 0's
lesson) — it should land with, or before, the first PostgreSQL code, and it
costs SQL Server nothing because the argument list is absent there.
