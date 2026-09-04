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

Four model changes — a map key and **three** fields in the state snapshot: the
declared module text (§2.2), the write path a parsed expression-bearing object
was created under ([ADR-0013](ADR-0013-postgres-reference-data.md) §3) and the
declared column default (ADR-0013 §4) — and one bargain that lapses. The bargain is affordable only because ADR-0005 has since
shipped (§3). Both model changes are small; what is not small is that the
signature is normalized by *routine* rules rather than column rules (§1), and
that the state has to keep what was declared as well as what came back (§2),
because on this engine those are permanently different texts.

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

**The key of `Schema::modules` becomes a `ModuleId`, and what identifies an
object depends on its kind** — which is itself a finding, because a first
version of this decision said "the qualified name plus, where they overload, the
argument types" and that is right for two kinds out of four:

| Kind | Identified by | Because |
|---|---|---|
| view | `schema.name` | one `pg_class` namespace with tables |
| function, procedure | `schema.name` + normalized argument types | they overload (§1) |
| **trigger** | **its table** + name | **measured** below |

**Measured**, a trigger's name is scoped to the table it is on, not to the
schema:

```
=> CREATE TRIGGER audit AFTER INSERT ON tg.orders ...;
=> CREATE TRIGGER audit AFTER INSERT ON tg.customers ...;
   both accepted: customers.audit, orders.audit

=> DROP TRIGGER audit;              syntax error at end of input
=> DROP TRIGGER audit ON tg.orders; accepted
```

So two valid declarations map to one `app.audit` key under a name-only identity,
and every statement the emitter writes for a trigger needs the table anyway.
`Module::on` already holds it — ADR-0002 put it in the model rather than in an
annotation precisely because "moving a trigger to another table is a different
object" — so this is the identity catching up with a field that was already
there for the right reason.

For SQL Server nothing moves: the argument list is always absent, and a trigger
name is unique per schema, so every existing value, file and snapshot means what
it meant before.

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
  unequal; a key holding normalized types makes them equal.

### The normalization is *routine* normalization, not column normalization

The obvious move is to reuse `Dialect::normalize_type`, which already folds
`int` to `integer`. **Measured, that is wrong**, and wrong in the direction that
produces two declarations for one object:

```
=> CREATE FUNCTION v.f(a varchar(10)) RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql;
=> CREATE FUNCTION v.f(a varchar(20)) RETURNS int AS $$ SELECT 2; $$ LANGUAGE sql;
ERROR:  function "f" already exists with same argument types

=> SELECT pg_get_function_identity_arguments(oid) ...;
 a character varying          -- the length is gone
```

and the same for precision: `h(numeric(10,2))` and `h(numeric(12,4))` are one
function, identified as `numeric`. **PostgreSQL discards type modifiers when it
identifies a routine.** A column's `varchar(10)` and `varchar(20)` are different
types and must stay different — that is what `normalize_type` is for — so the
two normalizations answer different questions and cannot be the same function.

**Decision.** The dialect gains a second, narrower hook — "normalize this type
*for routine identity*" — which on PostgreSQL strips the modifiers
`normalize_type` deliberately keeps, and on SQL Server is never called because
nothing there overloads. Reusing the column-oriented one would key
`f(varchar(10))` and `f(varchar(20))` as two modules over one engine object, and
every drop and grant the plan emitted would name a signature the engine resolves
to something else.

This is the same lesson as ADR-0011 Amendment 2 arriving one layer down: a
normalizer is only neutral with respect to the question it was written for.

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
2. **So the recorded state has to keep the declaration, not only the read-back.**
   A first draft of this ADR said the read-back replaces the recorded definition
   and the *next* plan is clean — convergence in one round. **That is wrong, and
   the measurement that disproves it is one already in this document**: a
   `BEGIN ATOMIC` body written as `SELECT a + 1` comes back as `SELECT (a + 1)`,
   with its comment gone. That difference is not layout, so
   `normalize_definition` cannot close it, and replacing the baseline with
   another deparsed copy never makes it equal to the unchanged hand-written
   declaration. `plan --db` would emit the same `CREATE OR REPLACE` on every
   run, for ever — the cry-wolf loop this section claimed to avoid.

   The decision: the state snapshot records, for each managed module, **both**
   texts — the definition **as declared** when it was applied, and what the
   engine returned. Then each comparison stays inside one space, which is what
   §8.2 asks for:

   | Question | Compares |
   |---|---|
   | has the *declaration* changed? (the differ, `plan --db` included) | declared-now against declared-at-last-apply |
   | has the *environment* changed? (drift, `verify`) | the recorded read-back against the live read-back |

   Neither comparison ever puts a hand-written text beside a deparsed one, so
   there is nothing to converge. The cost is one field per module in the state
   snapshot, and it is the honest price of an engine that does not store what it
   was given.

   **And `baseline` has nothing to put in the declared slot.** `cmd_baseline`
   builds its snapshot from `managed_state` — the live catalog, scoped by the
   declarations — because its whole purpose is to take the database as it stands
   (§9.2). No declaration was applied, so neither obvious filling works: the
   live deparsed text restores the endless restatement this section just fixed,
   and the current YAML makes a module somebody edited by hand in the database
   compare **unchanged for ever**, while the database no longer matches it.

   So the slot is **left empty**, and empty means "never applied through this
   tool" rather than "equal to nothing". A module whose declared slot is empty
   is restated once by the next plan, and both slots are filled properly by that
   apply.

   **On PostgreSQL that restatement is a rebuild**, because §3 emits every
   module change as drop + create — so adopting a database rebuilds every
   managed module once, carrying the full restore obligation and refusing
   wherever it cannot carry something. A first version of this paragraph called
   the restatement "cheap, idempotent and grant-preserving", which described a
   `CREATE OR REPLACE` path §3 has since removed; two parts of one document
   should not disagree about what the tool does.

   Stated plainly, that is the cost of adoption on this engine, and it lands
   where an estate is most likely to have hand-made grants and owners. It is
   also the moment to discover them: adoption is exactly when a project finds
   out what the database is carrying that the declarations do not say, and a
   refusal that names the object and the grant is a better first day than a
   silent one.

   That is also the answer to the second failure above rather than an accident:
   after a `baseline`, a hand-edited module is **not** silently blessed. It is
   restated to match the declarations, which is what declaring it meant.
   `baseline` resets the drift comparison; it does not redefine the desired
   state. The same rule covers `snapshot`, and `bootstrap` needs no exception
   because it creates every module from the declarations it holds.
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
A first version of this paragraph concluded that `apply` should therefore
re-read every managed module after a rename, so the closing state matches what
the database holds. **That prevents drift by blessing a divergence**, which is
worse: the environment now says one thing, the declarations say another, and
after the fix in §2.2 nothing compares them. **Measured:**

```
after the rename the catalog says:  SELECT id, full_name FROM m.clnt;
recreating from the unchanged declaration text:
    refused: relation "m.cust" does not exist
```

The declarations are supposed to be the authority. `bootstrap` reads them — it
is the disaster-recovery path — and it now fails, *because* the tool decided the
engine's rewrite was the truth.

**Decision: a rename is a table change like any other, so §4 applies to it.**
The dependent managed modules are rebuilt from their **declarations**, in the
same plan. If a declaration still names the old table the `CREATE` fails,
loudly, inside the plan's transaction, with nothing left behind — the correct
outcome, because that declaration is wrong and only the user can fix it. If it
was updated, the environment and the declarations agree again, which is what a
rename in this tool is supposed to mean.

Re-reading is still needed for the modules that are *not* rebuilt, so the
recorded read-back matches the live one; it is no longer the whole answer. SQL
Server has the mirror problem — the definition goes *stale* rather than
following — which ADR-0002 handles by impact analysis.

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

So the decision is: **on PostgreSQL, a module change is emitted as drop +
create, and the declared grants on that module are re-emitted after it**, by the
machinery ADR-0005 built.

An earlier version of this sentence said "a module change that `CREATE OR
REPLACE` cannot express", and the qualifier did not survive: *which* changes it
cannot express turns out to be unknowable at plan time without executing DDL
somewhere the tool may not execute it, and the sub-section at the end of this
§3 works that out and lands on **always**. The conditional is removed here
rather than left for a reader to reconcile — two forms of one decision in one
section is how an implementer ends up writing the cheaper one.

The residual loss is named rather than hidden:

- A grant to a **declared** role comes back. Measured behaviour, existing code
  path, and it is the case the tool is for.
- Anything else in the object's ACL does not. It is destroyed by the engine, and
  pbps cannot restore what it does not model. **This must be a refusal, not a
  warning**: a connected plan that is about to drop and create a module reads
  the object's ACL first and refuses unless the declarations can reproduce it
  exactly — the same shape as ADR-0005 note 4's "`plan --db` refuses to plan
  over a role it cannot describe", and for the same reason. Warning and
  proceeding would put "the application lost access" behind a line of output
  nobody reads at 3am.

**The refusal is on any ACL the declarations cannot reproduce, in either
direction**, and the second direction is easy to miss. A first draft of this
section said "a grant the declarations do not hold", which covers somebody
having granted *more* and misses somebody having granted *less*. **Measured**,
the second is the more dangerous one:

```
REVOKE EXECUTE ON FUNCTION w.f(int) FROM PUBLIC;
   -> {postgres=X/postgres}          -- an operator hardens the function

DROP FUNCTION w.f(int); CREATE FUNCTION w.f(a int) RETURNS bigint ...;
   -> NULL                           -- pbps rebuilds it: the default is back
SET ROLE w_nobody; SELECT w.f(7);  -> 7
```

A revocation is not a row in the ACL — it is the *absence* of the engine's
default — so a rebuild restores the default and **silently reopens a function
somebody deliberately closed**. That is a security regression caused by an
ordinary return-type edit, and [ADR-0010](ADR-0010-postgres-privileges.md) §5
records the same fact from the other side: pbps cannot express "revoked from
`PUBLIC`", so it cannot put it back, so it must not take it away.

That refusal is the honest cost of this ADR, and it is worth stating plainly:
on PostgreSQL, pbps will refuse to edit a view or function whose access
somebody adjusted by hand — in either direction — until that adjustment is
either declared or undone. That is friction. The alternative is a deployment
that hands the world execute rights on a function and reports success.

### Ownership is not in the ACL, and a rebuild takes it

The ACL check above is necessary and not sufficient, because PostgreSQL keeps
ownership somewhere else — `pg_class.relowner`, `pg_proc.proowner` — and a
`DROP` followed by a `CREATE` makes the deployment account the owner.
**Measured:**

```
before: owner=m_owner  secdef=true  acl=NULL
                       -- pbps, connected as the deployment account, rebuilds it
after:  owner=postgres secdef=true  acl=NULL
```

The ACL is `NULL` on both sides, so **the two-directional ACL refusal cannot
see this at all**. And the third column is why it matters more than tidiness: a
`SECURITY DEFINER` function keeps that flag through the rebuild and now runs
with the *deployment account's* privileges instead of its original owner's. The
deployment account is the most privileged principal in the environment (ADR-0005
§ self-hosting), so an ordinary return-type edit turns a narrow function into a
privileged one, and nothing in the plan says so.

**Decision.** The connected plan reads the owner alongside the ACL, and:

- **preserves it** — an `ALTER … OWNER TO` after the `CREATE`, written into the
  plan so the approver sees it, exactly as the re-emitted grants are; and
- **refuses the rebuild when it cannot** — and "cannot" is more than one
  condition. A first version of this bullet said `ALTER … OWNER TO` "requires
  the deployment account to be a member of the target role", which is necessary
  and not sufficient. **Measured**, as a non-superuser deployment account (a
  superuser bypasses both checks, which is how the first attempt at this
  measurement missed them):

  ```
  -- ow_deploy owns the view and is a member of ow_owner
  ow_owner holds CREATE on the schema:   ALTER VIEW ow.v OWNER TO ow_owner   accepted
  after REVOKE CREATE ON SCHEMA ow FROM ow_owner:
                                         ALTER VIEW ow.v OWNER TO ow_owner
                                         ERROR:  permission denied for schema ow
  ```

  The object stays validly owned by a role that has lost `CREATE` on its
  schema — so an object can be in a state its own owner could not re-establish.
  And membership can be granted without the right to use it:

  ```
  GRANT ow_owner TO ow_deploy WITH SET FALSE;
      pg_auth_members: set_option=false
      SET ROLE ow_owner  ->  ERROR: permission denied to set role "ow_owner"
  ```

  So the connected plan checks **both prerequisites before emitting the
  statement**: the target owner's `CREATE` on the containing schema
  (`has_schema_privilege`), and that the deployment account may actually assume
  the role (`pg_auth_members.set_option`, which is the flag `WITH SET FALSE`
  clears). Without them the plan is applyable and rolls back at the ownership
  statement, which is the failure §7.5 exists to prevent — and here it would
  arrive *after* the `DROP`, at the end of a rebuild.

  Refusing names the owner, the schema, and which of the two prerequisites is
  missing.

A `SECURITY DEFINER` module whose owner cannot be preserved is the one case
where refusing is not merely conservative but the only defensible answer: the
alternative is a silent privilege escalation that verifies clean.

### And a view carries options that neither the definition nor the ACL holds

Ownership is not the last of them. A view's `security_invoker`,
`security_barrier` and `check_option` live in `pg_class.reloptions`, and
`Module::definition` starts after `AS` (ADR-0002), so **measured**, the
definition cannot show them:

```
reloptions: security_invoker=true, security_barrier=true
pg_get_viewdef shows: SELECT id, a FROM opt_t;
```

**And this one is not confined to the rebuild path.** `CREATE OR REPLACE VIEW`
— the cheap, grant-preserving path this ADR has been treating as the safe one —
drops them too:

```
after CREATE OR REPLACE:   reloptions: NULL — lost
after drop-and-create:     reloptions: NULL — lost
```

What that costs, measured end to end with a reader holding `SELECT` on the view
and nothing on the table beneath it:

```
while security_invoker=true:            refused: permission denied for table opt_t
after a rebuild dropped the option:     accepted
```

The view stops running with the querier's privileges and starts running with its
owner's, so it hands out rows the reader was previously refused. Nothing
compares `reloptions`, so `verify` stays clean and no line of the plan mentions
it.

**Decision.** The connected plan reads `reloptions` alongside the ACL and the
owner, and re-emits them verbatim in the `WITH (…)` clause of whichever
statement it writes — replace or create — so the option survives and the plan
shows it. Where it cannot, it refuses. The option is also reported as
**unexpressible**, down ADR-0005's existing path: the declarations cannot say
`security_invoker`, so `pull` and `status` name it rather than implying the
model covers it.

Preserving something the declarations cannot express is a smaller wrong than
destroying it, and it is the same trade as `ALTER … OWNER TO` above: pbps is
carrying the environment's own state across a statement it had to write, not
inventing a state nobody asked for. **The real fix is a model that can hold view
options**, and until it exists this sits beside `PUBLIC`
([ADR-0010](ADR-0010-postgres-privileges.md) §5) as a named gap rather than a
solved problem.

### The rule these three are instances of

Three review rounds each found the same shape one attribute further out — the
ACL, then the owner, then `reloptions` — and a fourth found the next one:
**measured**, a view column default set with
`ALTER VIEW … ALTER COLUMN … SET DEFAULT` lives in `pg_attrdef`, is absent from
`pg_get_viewdef`, and does not survive a rebuild:

```
pg_attrdef:            'from the view default'::text
pg_get_viewdef shows:  SELECT id, a FROM sp_t;
after a rebuild:       gone
```

An updatable view then silently stops supplying that value, and inserts that
omit the column fail or write something else — while `verify` reports nothing,
because nothing compares `pg_attrdef` either.

Patching a fourth attribute into a list of three would be the wrong repair, so
the rule is stated instead of the list:

> **A `DROP` takes with it everything the catalog attached to the object, and
> `Module::definition` describes almost none of it.** Before a rebuild — one the
> user's edit forced, or one §4 synthesized — the connected plan enumerates what
> the catalog holds for that object, carries each item into the statements it
> writes, and refuses when it cannot carry one.

What that enumeration covers today, each measured on this branch:

| Attribute | Where it lives | Lost by |
|---|---|---|
| grants | `relacl` / `proacl` | drop + create |
| a revocation from `PUBLIC` | the *absence* of the default ACL | drop + create |
| owner, and with it `SECURITY DEFINER`'s meaning | `relowner` / `proowner` | drop + create |
| `security_invoker`, `security_barrier`, `check_option` | `pg_class.reloptions` | drop + create **and `CREATE OR REPLACE`** |
| view column defaults | `pg_attrdef` | drop + create |
| a trigger's enabled state (`DISABLE`, `ENABLE REPLICA`, `ENABLE ALWAYS`) | `pg_trigger.tgenabled` | drop + create |
| grants the *new* object inherits | `pg_default_acl`, keyed to the creating role | **gained**, not lost, by drop + create |

The table is evidence, not the specification — it is what has been measured, and
the next attribute nobody has looked for is exactly as dangerous as these were.
The specification is the paragraph above it: **enumerate from the catalog, not
from memory.** A dialect that implements the list rather than the rule will be
wrong again the moment PostgreSQL attaches something else, and every instance so
far has failed in the same direction — access silently widened, verification
silently clean.

**And "carry" is not enough: the rule is *restore*, which includes taking away
what arrived uninvited.** Every version of this section until now checked the
*old* object and re-emitted what it found, which cannot see something the *new*
object acquires on its own. **Measured**, it acquires plenty: with an
`ALTER DEFAULT PRIVILEGES` entry for the deployment role — the ordinary way an
estate arranges read access, and the construct [ADR-0010](ADR-0010-postgres-privileges.md)
§2 measured as keyed to the creating role — a view that role creates arrives
already granted:

```
the ACL of a view the deployment role just created:
    {m_deploy=arwdDxtm/m_deploy, m_bystander=r/m_deploy}   -- granted by no declaration
```

and the failure is precisely in the gap the old check leaves:

```
old acl NULL  ->  passes a "reproduce the old ACL" check
new acl        {m_deploy=…, m_bystander=r/m_deploy}
```

An object with no grants at all is the easiest case to wave through, and it is
the one where a rebuild hands an unmanaged role `SELECT`. Re-emitting the
declared grants never removes it, because there was nothing declared to
contradict it.

So the enumeration is a **two-sided** obligation in a second sense too: it is
asserted **before the `DROP`** — that what the plan recorded is still what the
object carries — and again **after the `CREATE`**, that the object now carries
exactly that.

**And "before" has to mean "under the object's lock", not "on the preceding
line".** An assertion and a `DROP` are two statements, so another session can
commit `ALTER VIEW … SET (security_invoker = true)` between them; the rebuild
then restores the stale recorded options and the post-create assertion *passes*,
because it compares against that same stale intent. Two checks agreeing with
each other is not the same as either being right.

**Measured**, the remedy exists here — a view can be locked, and the lock stops
the change:

```
BEGIN; LOCK TABLE jj.v IN ACCESS EXCLUSIVE MODE;   accepted
-- from another session, while it is held:
ALTER VIEW jj.v SET (security_invoker = true);
    ERROR:  canceling statement due to lock timeout
```

So the plan takes `ACCESS EXCLUSIVE` on the object **before reading the carried
state** and holds it through the rebuild, which makes the read, the drop, the
create and both assertions one serialized unit.

**That works for a view and not for a routine, which is not a relation.**
**Measured**, `LOCK TABLE` refuses one outright:

```
LOCK TABLE kk.f IN ACCESS EXCLUSIVE MODE;   refused: relation "kk.f" does not exist
```

A row lock on its catalog entry does serialize it — **measured**, holding one
blocks a concurrent `ALTER FUNCTION`:

```
session A:  BEGIN; SELECT oid FROM pg_proc WHERE oid='kk2.f(int)'::regprocedure FOR UPDATE;
session B:  ALTER FUNCTION kk2.f(int) OWNER TO kk_other;
    ERROR:  canceling statement due to lock timeout
    CONTEXT:  while updating tuple (19,41) in relation "pg_proc"
```

**and the deployment account cannot take it.** `SELECT … FOR UPDATE` needs
`UPDATE` on the selected relation, and owning a routine grants nothing on
`pg_proc`. **Measured**, as the non-superuser account that owns the function:

```
ll_deploy:  SELECT oid FROM pg_proc WHERE oid='ll.f(int)'::regprocedure FOR UPDATE;
    ERROR:  permission denied for table pg_proc
postgres:   the same statement                       accepted
```

So the mechanism exists and is out of reach, and a design that prescribed it
would fail before every routine rebuild for exactly the accounts this tool is
built for.

**Decision.** The plan takes the lock **when the account can** — a superuser
deployment account is not rare in a controlled pipeline — and otherwise
**states that this rebuild is not serialized**, in the plan, beside the object.
Refusing instead would refuse every function edit, since §3 makes them all
rebuilds; and pretending is not available, so the residual is named where the
reviewer sees it: a concurrent `ALTER FUNCTION` between the read and the `DROP`
is reverted by the rebuild, and pbps cannot stop it without privileges it should
not need.

**And triggers are the fourth kind, with a lock of their own.** A trigger is not
a relation and not a routine; **measured**, its carried state is lost by the
rebuild like everything else in §3's table —

```
after DISABLE, tgenabled = D
after a drop-and-create rebuild, tgenabled = O — the disable is gone
```

— so an ordinary trigger edit silently reactivates behaviour an operator
disabled. The serializing lock is the **parent table's**, and **measured**, it
works:

```
BEGIN; LOCK TABLE l2.t IN ACCESS EXCLUSIVE MODE;
-- from another session:
ALTER TABLE l2.t DISABLE TRIGGER audit;   ERROR: canceling statement due to lock timeout
```

`tgenabled` therefore joins the enumeration, and `Module::on` — already the
trigger's identity (§1) — is also where its lock comes from.

So "serialize before reading the carried state" has **four** shapes, not two:
the object's own lock for a view, the parent table's for a trigger, a catalog
row lock for a routine *if the account is privileged enough*, and nothing at all
for the sequence of ADR-0013 §2. Only the engine can say which applies, and only
a measurement taken **as the account that will run it** can say whether it is
reachable.

Worth setting beside [ADR-0013](ADR-0013-postgres-reference-data.md) §2, where
the same shape had no such remedy: there the racing operation was `nextval`,
which takes no lock and against which PostgreSQL offers none, and the answer had
to be to refuse the construct. The same question — *can this be serialized?* —
now has three answers in this design: **yes, with a relation lock** (a view),
**yes, with a catalog row lock** (a routine, below), and **no, so refuse** (a
sequence). It is the engine that decides which, and the only way to find out
which one applies is to try it against the engine. The first assertion is the one this section
lacked, and its absence is not hypothetical: `reloptions` are outside the model,
so a DBA who hardens a view with `security_invoker = true` between `plan --db`
and `apply` has that setting silently reverted by the rebuild, with the
checksum unable to see it (§3 measures that nothing compares `reloptions`) and
drift unable to report it. After the `DROP` the original is gone and there is
nothing left to compare against, so the check has to happen while the object
still exists.

With both assertions, the plan brings the object's state to exactly what it
recorded — emitting what is missing **and revoking what appeared** — and refuses
if it cannot. `pg_default_acl` is
where to look for what will appear, and the plan says so, because a `REVOKE`
nobody can explain is worse than one the artifact predicted.

**And `apply` asks again before statement one — which shortens the window and
does not close it.** The revoke list is computed from `pg_default_acl` at plan
time, and `pg_default_acl` is not part of the schema the checksum is taken over,
so a default grant added between planning and applying makes the approved list
stale: the replacement inherits access no statement removes, and the
managed-role comparison still reports clean because the grantee is not a managed
role. A preflight comparing the live default-ACL state with what the plan
assumed catches that, and **measured, an entry added after the preflight and
before the `CREATE` still lands** — nothing locks that catalog, and the
transaction's later statements read it fresh:

```
session A (the apply):  BEGIN; preflight sees 0 default-ACL entries; ...
session B, meanwhile:   ALTER DEFAULT PRIVILEGES FOR ROLE dp_deploy
                            IN SCHEMA dp GRANT SELECT ON TABLES TO dp_bystander;
session A continues:    CREATE VIEW dp.v ...
    the view this transaction created has acl=
        {dp_deploy=arwdDxtm/dp_deploy, dp_bystander=r/dp_deploy}
after commit:           dp_bystander can select the view: true
```

So the preflight is necessary and is not the check that makes this safe.
**The check that does is a postcondition: after each `CREATE`, and before the
transaction commits, the plan asserts the object's ACL is exactly what it
intended.** Anything else aborts the transaction, and §7.5's all-or-nothing then
makes the whole apply a no-op rather than a silent widening.

That is the same shape the row writes already use — a statement-level
postcondition rather than a precondition — and it is the right shape for the
same reason: a precondition can only describe the world before the statement,
and what this design needs to be true is a fact *about the statement's own
result*.

That is not a new mechanism: ADR-0005 note 13 already has `apply` re-asking
about a dropped role's members for exactly this reason — *"the members listed at
plan time can be stale by apply time, and the checksum cannot see it"*. This is
the same sentence with a different catalog, and it is worth noticing that the
sentence generalizes: **anything the plan reasons about that the checksum does
not cover has to be re-asked at apply time.**

Two consequences worth naming, because they follow from the rule rather than
from any one row: **the enumeration is `plan --db`'s work, not the emitter's**,
since only a connection can see what an object carries; and **it runs on every
module edit**, because this section's sub-section below concludes that every
edit on this engine is a rebuild. The intuition that a replace would have been
the cheap safe path did not survive its own measurement — `CREATE OR REPLACE`
drops `reloptions` too — which is the reason the enumeration is not optional
for some edits and mandatory for others.

### How the plan knows a rebuild is needed

The paragraph above conditions drop + create on "what `CREATE OR REPLACE`
cannot express", and §3 has just established that the deciding facts — the
return type, the view's column list — live inside the opaque `definition` this
tool does not parse. Left there, the rule is unimplementable: the planner would
be conditioning on something it has said it cannot know.

**Measured**, the engine will answer the question if it is asked inside a
savepoint:

```
BEGIN;
INSERT INTO w.evidence VALUES ('work done before the attempt');
SAVEPOINT try_replace;
CREATE OR REPLACE FUNCTION w.g(a int) RETURNS bigint ...;
   ERROR:  cannot change return type of existing function
ROLLBACK TO SAVEPOINT try_replace;
INSERT INTO w.evidence VALUES ('work done after rolling back');
COMMIT;                                  -- 2 rows, both committed
```

A failed statement dooms a PostgreSQL transaction, but `ROLLBACK TO SAVEPOINT`
un-dooms it: the attempt is recoverable and the surrounding work survives.

**But the target is the wrong place to ask.** A first version of this decision
had `plan --db` attempt the replace inside a savepoint on the environment being
planned against. Rolling back a savepoint undoes the catalog change and nothing
else: the attempt still takes DDL locks on a live object, and it still fires
`ddl_command_start` event triggers, whose side effects — a `nextval`, a row in
an audit table, a notification — are not transactional and do not roll back.
That turns `plan` into a command that can block a production workload and
advance application state, and `plan` is a read-only command in every other
respect (§9.1). A planning step that mutates the thing it is planning against is
not a preflight; it is a small unreviewed apply.

**Decision: on PostgreSQL a module change is always emitted as drop + create.**
The question is not answered because it cannot be asked, and the constraints
that close every other route were each established separately:

| Route | Closed by |
|---|---|
| Ask the target, inside a savepoint | Planning must not execute DDL on the environment it plans against: the attempt takes DDL locks and fires event triggers whose effects do not roll back |
| Carry both shapes and let `apply` choose | SPEC §7.3 — *"what gets approved is exactly the plan approved at the deployment gate"*. Replace and rebuild are not two spellings of one change; a checksum over "one of these two" pins nothing a reviewer read |
| Ask a dev database from `plan --db` | `crates/pbps-cli/src/main.rs:642` refuses `--dev` with `--db`, and SPEC §9.3 gives the reason — *"a rehearsal answers a preview's question, and combining the two would invite a dev-verified plan to be read as a target-verified one"*, plus *"a dev-database-verified plan is still a preview"* |
| Read the answer out of the declaration | The deciding facts — a view's column list, a function's return type — live inside `definition`, which §8.2 says this tool does not parse |

Two earlier versions of this section took the first two routes in turn. This one
takes none of them, and the reason it costs less than it appears to is §3's own
measurements: `CREATE OR REPLACE` **also** drops `reloptions`, so the replace
path never carried the object's full state either. What it saved over a rebuild
was the ACL and the owner — and the enumeration above restores both anyway.
Always rebuilding therefore removes a question the tool cannot answer, in
exchange for a cost the tool was already paying.

The price is real and belongs in the open: every module edit on PostgreSQL drops
and recreates the object, carries the full restore obligation, and **refuses**
when it cannot carry one. That is a lot of refusing for an estate that adjusts
privileges by hand. It is also the only shape in which the artifact a human
approved is the artifact that runs.

An offline `plan` has nobody to ask at all and says so, which is what §9.1
already means by "anything computed offline is a preview".

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

- **The ordering rank must be per-change, not per-family** — and reordering
  alone is not enough. A module the plan both edits and whose dependency the
  plan changes has to be dropped before the table changes and created after
  them. ADR-0002's brackets ("drops first, creates and alters last") become
  three ranks, with the middle one populated by exactly the modules whose
  dependencies this plan touches. On PostgreSQL that set is computable without
  parsing, from `pg_depend`, at `plan --db` time — which is where ADR-0002's
  impact machinery already asks its questions.

  **But the common case has no module change to rank.** A revision that retypes
  `customer.legacy_code` and leaves the view alone produces exactly one change,
  and the measured refusal above still fires: the differ emitted nothing for the
  view, so there is nothing for a rank to move. Ranking only ever reorders
  changes that exist.

  So `plan --db` must **synthesize** the drop and the create for an unchanged
  managed dependent, from `pg_depend`, and put them in the plan where the
  approver can see them — a view being dropped and recreated is not a detail to
  discover at apply time. Two consequences follow immediately:

  - **A synthesized rebuild destroys the object's grants exactly as an edited
    one does**, so §3's two-directional ACL refusal stands in front of it too.
    The rebuild pbps invented is held to the same bar as the rebuild the user
    asked for.
  - **An affected dependent that pbps does not manage is a refusal.** It cannot
    be recreated from anything the project holds, so dropping it would destroy
    an object with no way back. `plan --db` names it and stops — the same shape
    as ADR-0005 note 10's refusal to drop a role that owns something.

  **And "dependent" does not mean "dependent module".** This paragraph was
  written about views depending on tables and stayed that shape, which leaves
  out everything a *table* can hold that depends on a function — and since §3
  now emits every module change as drop + create, a function edit meets those
  dependents every time. **Measured**, all four refuse the drop:

  ```
  DROP FUNCTION with a managed check constraint on it:  refused: cannot drop
      function fdep(integer) because other objects depend on it
  ... with a column default on it:                      refused (same)
  ... with a generated column on it:                    refused (same)
  ... with an expression index on it:                   refused (same)
  ```

  A plan that does not account for them is **applyable and predictably fails**,
  which is the one outcome §7.5 exists to prevent.

  So the enumeration is over **every reverse `pg_depend` edge**, not over
  modules: the managed ones — a check constraint, a default, a generated column
  or an index the table declaration holds — are dropped and restored around the
  rebuild, in the plan where the approver sees them; the unmanaged ones are the
  refusal above.

  **Their cost has to be visible, because it is not the module's cost.**
  Restoring a check constraint revalidates the table and rebuilding an index
  locks it, so a one-line edit to a function can carry a table scan behind it.
  That belongs in the plan's risk list and in 14.1's estimate, not in a
  footnote: the reviewer is approving the scan, not just the function.

  This is the second time an enumeration in this document was written too
  narrowly — §3's was over the object's *attributes* and missed three, this one
  is over its *dependents* and missed four. The rule stated there covers both if
  it is read as it is written: **enumerate from the catalog, not from memory.**

  ### And the catalog does not know every caller

  That rule has a limit, and it is sharp. **Measured**, `pg_depend` records an
  edge for a caller only when the calling body was parsed at creation time:

  ```
  three callers of m.dep_f(int), written three ways:
      pg_depend records an edge for  dep_atomic()   -- BEGIN ATOMIC
      the plpgsql body and the SQL string-literal body record nothing
  ```

  So with the recording caller removed, the whole rebuild goes through and the
  failure lands somewhere else entirely:

  ```
  DROP FUNCTION m.dep_f(int)                                      accepted
  CREATE FUNCTION m.dep_f(a int, b int) ...                       accepted
  -- the apply commits, and verify has nothing to report
  SELECT m.dep_plpgsql()   refused: function m.dep_f(integer) does not exist
  ```

  A signature change is the obvious case that bites — the caller is looking for
  an identity that no longer exists. **But an unchanged identity is not safety**,
  and a first version of this decision assumed it was. **Measured**, renaming a
  *parameter* keeps `mm.f(integer)` exactly as it was and still breaks a caller
  that used named notation:

  ```
  before the rebuild:                       mm.caller() = 2
  the identity is:                          mm.f(integer)
  after rebuilding f(a int) as f(x int):    still mm.f(integer)
  and the caller:  refused: function mm.f(a => integer) does not exist
  ```

  A changed return type under the same identity does the same to a caller that
  depends on the old one. Both live inside the opaque `definition`, so pbps
  cannot tell them from a harmless edit — which means the trigger for the scan
  cannot be "the identity changed".

  **Decision.** When a plan **rebuilds or removes** a routine — a `DropModule` is
  the case with the most to lose and a first version of this trigger said only
  "rebuild" — the catalog's edges are supplemented by ADR-0002's existing device:
  a **best-effort identifier scan** — that ADR already permits scanning definition text for the
  names of managed objects, "no SQL semantics", for ordering — extended to the
  bodies the catalog holds.

  **What is then done with a managed caller is a refusal, not a rebuild.** A
  first version of this decision said such callers are "rebuilt with the rest",
  which accomplishes nothing: a rebuild recreates the caller from its
  *unchanged* declaration, and **measured**, PostgreSQL accepts a plpgsql body
  that names a function which does not exist —

  ```
  CREATE FUNCTION tz.caller() ... BEGIN RETURN tz.no_such_fn(1); END ...  accepted
  SELECT tz.caller()   refused: function tz.no_such_fn(integer) does not exist
  ```

  — so the apply commits with the defect intact and the caller still fails at
  its next call. Scheduling another rebuild preserves exactly the failure it was
  meant to prevent.

  So: when a plan rebuilds or removes a routine and the scan finds a **managed**
  caller, `plan --db` **reports** it and names both.

  A first version of this said *refuses*, and that rule can never be satisfied.
  The scan matches a **name**, and with overloading a name is not an identity —
  this document says so two paragraphs down — so a managed caller of `f(text)`
  would block every rebuild of `f(integer)`, and editing the caller could not
  clear the block while it still legitimately mentions `f`. A refusal with no
  way out is not conservative: it makes valid work impossible and teaches the
  next person to route around the tool.

  **The line is evidence versus suspicion.** `plan --db` **refuses** where the
  dependency is established — a `pg_depend` edge, or a `depends_on:` the user
  declared — and **reports** where it is only a name match, managed or not. The
  scan's job is to put a candidate in front of a human, which is what it can
  honestly do; the refusals in this design are for facts, and a name is not one. The remedy is the user's, because
  only the user can say what the caller should now call — which is the same
  division of labour as rename and drop intent.

  **The refusal does not lift because the caller's declaration also changed.** A
  first version of this rule exempted a caller the same plan edits, treating a
  changed declaration as evidence the call had been repaired. It is not:
  **measured** (§4, and again here), PostgreSQL accepts recreating a PL/pgSQL
  body that names a function which does not exist, so an unrelated edit to the
  caller buys an exemption and the apply still commits with the defect.

  **But the exposure is narrower than "every opaque body", and this time the
  narrowing is the hazard's shape rather than a convenient proxy.** Measured,
  with `check_function_bodies` at its default `on`:

  ```
  LANGUAGE sql string body naming a missing function:
      refused: function nn.no_such(integer) does not exist
  LANGUAGE plpgsql body naming a missing function:
      accepted
  ```

  So a SQL-language body is validated **when it is created** — and that is the
  whole of it. A first version of this paragraph read that as "SQL-language
  callers are safe", which attaches the exemption to the body's *language* when
  it belongs to *whether the validating event happens*. **Measured**, a
  callee-only rebuild never recreates the caller, so nothing validates anything:

  ```
  reverse pg_depend edges to oo.f:                            none
  DROP FUNCTION oo.f(int)                                     accepted
  CREATE FUNCTION oo.f(a int, b int) ...                      accepted
  SELECT oo.sqlcaller()   refused: function oo.f(integer) does not exist
  ```

  and the engine only steps in when the plan does recreate it:

  ```
  CREATE OR REPLACE FUNCTION oo.sqlcaller() ... $$ SELECT oo.f(1) $$ ...
      refused: function oo.f(integer) does not exist
  ```

  **So the exemption is: a SQL-language caller is excluded from the refusal only
  when this plan actually recreates it**, because then the engine performs the
  check — which is a reason to **suppress the heuristic report**, not a reason to
  refuse anything.

  Reading this paragraph as a refusal rule is what the decision above already
  rejects: a scan hit is a name, and a legitimate caller of `f(text)` must not
  block a rebuild of `f(integer)`. So the whole of it sits under that rule —
  **refuse only on an established dependency** (`pg_depend`, or a declared
  `depends_on:`); **report** a name match; and where the plan recreates a
  SQL-language caller, the engine's own check makes even the report unnecessary,
  because a stale reference in one fails loudly at `CREATE`. PL/pgSQL and
  dynamic SQL keep the report, since nothing validates them at all.

  That is the fourth time in this section a rule has been attached to something
  easy to see — the identity, the caller's declaration changing, the body's
  language — rather than to the event that actually decides the outcome. The
  three earlier ones are recorded above; this one is recorded here; and the
  pattern is worth more than any of the four fixes, because the next rule
  written in this section will be tempting for the same reason. Callers found **outside** the managed set are
  **reported, not refused**, and `depends_on:` remains the explicit escape hatch
  for what a scan cannot see.

  Reported rather than refused, deliberately: the scan matches a *name*, and
  with overloading a name is not an identity, so it over-approximates. A
  refusal on a heuristic that cries wolf is a refusal people learn to work
  around, and this project has enough real refusals to spend that credit on.

  **The residual is the engine's, and it is stated rather than engineered
  away:** a call assembled by dynamic SQL is invisible to `pg_depend`, to the
  scan, and to any analysis short of running the code. What *does* remove the
  hazard is writing SQL-language routines in standard syntax — **measured**, a
  `BEGIN ATOMIC` body is the one of the three that records its dependency — and
  that is worth saying in the PostgreSQL documentation as a recommendation with
  a reason, rather than leaving each project to discover it the way this
  document did.
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
| The state snapshot keeps a module's **declared** text beside the read-back (§2.2) | One field, and a state format bump |
| The state snapshot also keeps the **write path** each *parsed expression-bearing object* was created under ([ADR-0013](ADR-0013-postgres-reference-data.md) §3) | A second field in the same bump. The path's order decides which schema an unqualified name binds to — measured, for a generated column as much as for a module — so it is an input to the declaration's meaning |
| And the **declared column default** beside the one read back (ADR-0013 §4) | A third field. `schema_diff.rs:385` compares defaults as text and PostgreSQL returns `'unnamed'::text` for a declared `'unnamed'`, so without it every connected plan re-emits `AlterColumnDefault` for ever |
| `check_names`' one-namespace rule becomes a dialect question | A trait method; MSSQL keeps today's answer |
| A dialect hook for routine-identity normalization (§1), and one for "which module kinds overload" | A trait method and a datum |
| `ModuleDeps` keyed by `ModuleId` on **both** sides | Today `BTreeMap<ObjectName, BTreeSet<ObjectName>>`, which cannot say that `app.f(integer)` depends on something while `app.f(text)` does not — two valid declarations would share or overwrite one hint entry, and the ordering it exists to fix would be computed from the wrong graph |

Everything else — `Module`, `ModuleKind`, the ids file (modules still carry no
identity), the differ, `docs`, the policy engine — is unchanged.

**The abstraction holds, with one correction to what that costs.** The first
draft of this document claimed the whole bill was one map key. It is one map key
*and three fields in the state snapshot* — what was declared, because §2.2's
convergence argument was wrong; the path it was created under, because ADR-0013
§3 made that path part of what a declaration means; and the declared column
default, because ADR-0013 §4 found the same permanent restatement in the differ's
text comparison of defaults. All three are one fact wearing three hats:
**PostgreSQL hands back its own spelling of whatever it was given**, so anything
compared against a declaration needs the declaration kept beside it. The count
rose twice under review, and recording that rather than quietly widening the
earlier claim is the point: "we checked" is not the same claim as "it held".

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
- **The model cannot hold a view's options** (§3) — `security_invoker`,
  `security_barrier`, `check_option`. They are preserved verbatim across the
  statements pbps writes and reported as unexpressible, which keeps them from
  being destroyed but not from being changed behind the tool's back. This and
  `PUBLIC` ([ADR-0010](ADR-0010-postgres-privileges.md) §5) are the two places
  where PostgreSQL holds security-relevant state the declarations cannot say.
- **Everything here is proposed.** No PostgreSQL dialect exists; this document
  is a set of decisions taken in advance so that the emitter has something to be
  written against, and each is falsifiable by the live suite that must come with
  it.

## Placement

Phase 5, ahead of the emitter. The `ModuleId` key change is a `pbps-model`
change and therefore the most expensive kind to take late (SPEC §12, Phase 0's
lesson) — it should land with, or before, the first PostgreSQL code, and it
costs SQL Server nothing because the argument list is absent there.
