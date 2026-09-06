# Pitfalls

Bugs and traps this codebase has actually shipped or nearly shipped, and the
shape each one belongs to. The numbered *decisions* live in
[DECISIONS.md](DECISIONS.md); this file is the record of what went wrong, so the
same mistake is recognised the second time.

## The five recurring shapes

Every one of these was reported, then swept for, then found again somewhere else.
Treat a new instance as likely rather than surprising.

### 1. An error, an absence and an emptiness read as good news

Nineteen instances so far. **Absent, empty and unreadable are three different
things, and only one of them is good news.**

- A failed permission query reported as "no permissions missing".
- An unreadable `__pbps_lock` reported as "no lock held" — in `doctor`, then
  `explain`, then a third time in `status`.
- An unreadable server version silently skipping the 2016-SP1
  `CREATE OR ALTER` gate.
- `preflight` reading an unreadable probe count as zero rows, *and* counting
  that probe among those that passed.
- `pull`'s existence guard reading a failed listing as "no declarations".
- A declared schema the database lacks reported as nothing at all — pbps never
  emits `CREATE SCHEMA`, so the first table in it fails.
- A lock that outlived its state table, invisible to `status`, then to `doctor`
  and `explain`.
- A lock table the caller has **no permission to read**: metadata visibility
  makes `OBJECT_ID` answer NULL, so "cannot look" became "no lock".
- `validate --since` reading a failed `ls-tree` as "no declarations at that
  revision", which calls every table new. From a project in a subdirectory it
  was empty every time: the pathspec lacked the `--full-tree` its sibling
  `load_from_git` had carried for exactly this since the baseline was written.
- A declared module the catalog **cannot read back** reduced to a warning. The
  recorder went ahead, the snapshot's schema could not hold the module, the
  scope every later command rebuilds from that schema forgot it, and the first
  `verify` refused an untouched database as a policy violation. It is a partial
  schema inside the managed set, and the recorders now refuse it as one.
- An unlock failure **after a command had already failed** dropped on the floor:
  `apply` reported its own error and left `__pbps_lock` held with no word about
  it, so the retry failed as "locked". The same shape in `snapshot`, `baseline`
  and `bootstrap`; the success path had been fixed one round earlier.

### 2. Failures escaping the one-envelope contract

Twenty-eight sites. Sweeping by callee name missed sites in the same module;
sweeping by enclosing function missed the dispatcher; then the entry point
before dispatch; then flag validations that run before the command body; then
one hiding **inside an argument list**, where `?` is evaluated before the
wrapper it appears to sit inside. Finally the envelope broke *itself*:
`Location.file` was a `PathBuf`, and serde's `Path` impl fails on non-UTF-8.

What holds now is structural, not vigilance: an exhaustive match at the entry
point, refusals typed `Result<Infallible>` so an edit cannot fall through one,
and `Location.file` as a `String` so an unspellable path cannot break
serialization.

The `on_apply_attempt` hook had the same shape one layer down: the failure
event was emitted at each site that could fail after the lock, and the
refusals before it — a stale `--checksum`, a preview, an unapproved risk —
returned past it, so the audit sink advertised as seeing every attempt never
saw the rejected artifact. Same repair: everything after the plan is
identified lives in one function whose only exits are a typed outcome or an
error, and the hook fires from the one place both arrive.

### 3. A failure routed to the wrong person

The three exit codes are the feature (decision 34). Both directions have been
broken: the annotation converter mapped `doctor`'s exit 1 to 2, and `verify`
folded an unexpressible live difference into the connection catch-all as
`environment.unreachable` at exit 1 — when the database had been reached and
the difference established, which is drift and exit 2.

### 4. A fix that generalises one step too far

The permission and lock checks were found wrong in **ten consecutive review
rounds**. Every fix was right about the case in front of it:

| fix | correct part | overshoot |
|---|---|---|
| database → schema scope | schema grants are real | too broad for the ledger |
| schema → object scope | object grants are real | dropped the create-time check |
| create-time check restored | it is needed | keyed on "any table exists", not "this one" |
| `dbo` out of the managed set | a project managing `app` never touches it | took the create-time `ALTER` with it |
| absent schema left unasked | no securable to ask about | said nothing at all |
| ledger asked at object scope | that is where the grant sits | chosen once for the pair, so a *missing* table's DML was asked at neither scope |
| `status` reads the lock on an empty ledger | that is a first `bootstrap` mid-run | not when the state table was dropped by hand |
| `lock_holder` checks for its own table | removes the reason for the old ordering | only one of its three callers was updated |
| that check asked `OBJECT_ID` | it is the obvious question | metadata visibility hides the table, so "cannot read" became "no lock" |
| `REFERENCES` added to the list | a foreign key really needs it | asked only on *managed* schemas, so a foreign key into somebody else's was never checked |

### 5. A sweep that asks one of the two questions

New, and it has produced two findings already. A check can be wrong in two
directions, and a sweep phrased around one of them clears every instance of
the other — with a written record saying the ground was covered, which is what
makes the second direction expensive to find later.

- **Can this probe *miss* a violation?** DECISIONS 112 asked exactly that of
  every preflight probe and answered it correctly. Nobody asked whether a probe
  can *report* a violation the plan is about to remove, and `AddForeignKey`
  refused every plan that repaired its own orphans (151).
- **Can this comparison be made?** DECISIONS 146 asked that of a cell whose
  column the plan retypes, answered "by neither type", and carried none — not
  noticing that the same predicate was also the stale-row guard, so removing it
  removed a check nobody had asked about (149).

Both shapes have the same tell: a change that makes something *less* checked,
justified entirely by an argument about accuracy. Write down what the removed
check was for before removing it.

The mirror runs in the post-apply movement guard, three rounds in a row: "can
this guard miss a change" was asked and answered each time, and "can this
guard invent one" was not. It ended up refusing every rename of a granted
table, because a rename is visible from three sides — the table's two names,
the role's two names, and the grant *target* — and the first two were paired
while the third was not (DECISIONS 157). **A guard has two failure directions
and a sweep down one of them is half a sweep.**

## Reasoning loses to measurement

Three times the natural, obviously-correct answer was wrong, and only a real
server said so. **Ask the engine before believing yourself.**

`CONTROL` on the database is not a shortcut past the permission list. With
`CONTROL` held and `DENY ALTER ON SCHEMA::app`:

| question | answer |
|---|---|
| `sys.fn_my_permissions(NULL,'DATABASE')` lists `CONTROL` | yes |
| `HAS_PERMS_BY_NAME('[app]','SCHEMA','ALTER')` | **0** |
| `CREATE TABLE app.t (id int)` | **fails** — Msg 2760 |

A lock table you cannot read is not an absent one, and the catalog cannot tell
you which it is. With `SELECT` on `__pbps_state`, nothing on `__pbps_lock`, and
a lock genuinely held:

| question | answer |
|---|---|
| `OBJECT_ID(N'dbo.__pbps_lock', N'U')` | **NULL** — "absent" |
| `HAS_PERMS_BY_NAME(N'dbo.__pbps_lock','OBJECT','SELECT')` | **0** |
| `SELECT * FROM dbo.__pbps_lock` | **Msg 229**, permission denied |

An absent table gives **Msg 208** on that same statement. `HAS_PERMS_BY_NAME` —
the natural repair — answers 0 for both cases, so only attempting the statement
separates them. Measurement changed the fix here, it did not merely confirm it.

A list of "the kinds of thing a principal can own", written from memory, had
six entries; the catalog has nineteen views with an owner column, and the one
that mattered — a role owning another role — was not on the list. **When the
engine holds the list, read the list off the engine** (`sys.all_columns` for
`principal_id` / `owning_principal_id`), and keep the probe that found it in
the code's comment so the next reader can run it again.

## A comparison that runs what it compares

The row read-back asked the engine, per cell, `CASE WHEN col = (default)`, so
the omitted spelling of a declared row would round-trip. Written for
`'Unlabelled'`, it was also run for `NEWID()`, for `SYSUTCDATETIME()`, and for
`NEXT VALUE FOR dbo.seq` — one evaluation per row of every drift check. The
first is harmless, the second is wasted, the third **advances the sequence**,
and the engine refuses `NEXT VALUE FOR` inside a `CASE` at all, so the table
became unreadable and `plan --db` failed. Only a literal is compared now
(decision 68). The shape: an expression the *declaration* wrote is being
handed to the engine in a context the declaration never meant, and "the
database is the normalizer" does not extend to running things.

The same read had a second fault with the same root: it folded "equals the
default" into "omitted", and a row that spelled a value equal to its default
compared unequal to itself on every plan. The catalog cannot know how a row
was written; only the side reading it can (decision 67).

## A probe reads a state that is not there yet

Two forms, and `order_key` decides which probes have the second.

Every probe runs **before the first statement**, so it may only name what the
catalog holds *now*. `AsStored` exists for that and translates renames — but a
column the plan **adds** is a state the probe cannot read at all, and
`AsStored::column` used to fall back to the declared name, so three probes
asked the engine about a column that would not exist for another few
milliseconds.

The tell is that the failure is silent in the direction that matters: an
invalid probe throws, a throw is reported as *unchecked*, and an unchecked
probe does not stop an apply. The check most worth having — a unique
constraint or a foreign key over a column that has *just* arrived, where every
existing row holds the same value — was the one guaranteed to be skipped.

What such a row will hold is knowable without asking, and the rule is an
engine fact worth keeping written down: **SQL Server backfills only a NOT NULL
column.** `ADD col NULL DEFAULT x` leaves every existing row at NULL. So a
nullable addition reads as `NULL`, a NOT NULL one as its default, and a
default no probe can evaluate reads as no answer at all. Substituting is not
always literal: `GROUP BY NULL` is `Msg 164` — a constant groups nothing, so
it leaves the `GROUP BY` list instead, and an empty list means every row is in
one group.

**The other form: the rows.** A probe attached to a change that sorts *after*
the row changes is not asking about the table the statement will meet. Row
changes are ranks 9 and 10; `AddCheck`, `AddUnique`, `SetPrimaryKey`,
`AddForeignKey` and `AddIndex` are all 11. So a plan that deletes its own
violations and then tightens was refused for violations that will be gone, and
one that writes violating rows was told there were none. `AlterColumnType`,
`AlterColumnNullability` and `AddColumn` sort at 6-8, before the rows, so
reading the current table is exactly right for those — **the rank is the
test**, not the intuition that "a probe should see the future".

The fix is not one fix. Where the constraint's columns are *named* — a unique,
a primary key, a foreign key — `rows_after` builds the relation the plan will
leave and the probe groups over that. A check is an arbitrary predicate over
columns the plan does not carry, and its expression is deliberately never
rewritten, so it can only subtract the rows the plan deletes and give no
answer at all where the plan inserts or updates.

## The engine fills in a type's defaulted arguments

`decimal` is stored as `decimal(18,0)`, `char` as `char(1)`, `float` as
`float(53)`, `nvarchar` as `nvarchar(1)`. A declaration that omits the
arguments therefore does **not** equal the read-back, and any comparison of a
declared type against a catalog type refuses a valid apply.

Measured, twice — once by reasoning about it and once by trying it. The second
is the one that settled it: comparing declared and read-back types passed every
test until the live created-table apply grew a `decimal` column, and then
failed with `column ``bare_dec`` is not the one this plan's CREATE TABLE
declares` against a database that was exactly right. Those four columns stay in
that test for that reason.

## An exclusion wider than its reason

The pre-delete probe left out every child row the plan *updated*, because a
row the plan moves off the doomed parent must not be counted against the
delete. The reason covers an update to the referencing column; the exclusion
covered an update to any column, and a child updated elsewhere — still
pointing at the parent — probed zero. Under `ON DELETE CASCADE` the engine
then deleted it without a word (decision 73). Shape 4: a fix right about the
case in front of it, one step too wide.

The apply guard had two more. The columns and constraints a plan moves are
excluded from the shape comparison and held to a *presence* check instead —
but the exclusion is of a definition, so a definition somebody else put behind
the plan's name passed (decision 189). And "the column is on one side only"
excused an added or renamed column by *name*, which is true of the one read
spanning its statement and of no later read of a staged run. An exclusion has
a size and a lifetime; check both against the reason.

And a third: a `DEFAULT` cell was dropped from a row's expectations because its
value could not be named (decision 191). "Cannot be compared as a value" is not
"cannot be compared": the read-back *omits* a confirmed default, so presence
was the comparable fact — under conditions the code already knew, one read and
one function away.

## A readiness check that reads only the declarations

`doctor` derived the securables to ask `CONTROL` about from the declared
grants, so a revision that *removed* a role's last grant — or the role — had
nothing to ask about, and the account was called ready for a plan whose
`REVOKE` then failed. Shape 1 again, in a new coat: an absence in the
declarations read as "nothing needed", when the thing needed lives in the
database. The managed roles' grants are asked about too (decision 69) — from
the recorded state, because the first fix read them from the catalog, and the
catalog hides a securable from an account with no permission on it: the live
suite showed the query returning nothing for exactly the login being checked.

## A comment ends at a carriage return

A `--` comment ends at a bare carriage return, and at nothing else that looks
like a line ending. The lexical scan behind the value-source check and the
module dependency scan waited for `\n`, so `-- note\rNULL` read as one long
comment and a required column with that default skipped the gate. Measured with
`EXEC` of a variable holding the character (plain `EXEC('…' + CHAR(13))` is
refused — see above):

| character after `-- c` | `SELECT 1 AS a -- c<char>, 2 AS b` returns |
|---|---|
| CR (`CHAR(13)`) | **two columns** — the comment ended |
| LF (`CHAR(10)`) | two columns |
| NEL (`NCHAR(133)`), U+2028, form feed, vertical tab | one column — still a comment |

And the consequence the check exists to prevent: `ALTER TABLE … ADD c int NOT
NULL DEFAULT --x<CR>NULL` on a table with one row fails with **Msg 515**.

## `shell_arg` has been wrong about shells five times

**The test written to pin the second fix asserted the bug.**

| # | wrong about | consequence |
|---|---|---|
| 1 | POSIX single quotes "fail safe" in `cmd` | `cmd` splits at `&` and runs the remainder |
| 2 | backslashes are safe bare | a POSIX shell eats them: `C:\Users\...` arrives as `C:Users...` |
| 3 | `!` is inert | expands *inside double quotes* under delayed expansion |
| 4 | a leading `-` is an ordinary character | clap reads it as a flag, and quoting cannot help — the shell strips the quotes first |
| 5 | a leading `@` is an ordinary character | PowerShell splats it, substituting the current argument array |

Every one was found by testing rather than reasoning. The allowlist is the risky
half of that function; the placeholder is the safe half. **Reach for the
placeholder early, not as a last resort.**

## git renders a path outside ASCII in C quoting

`ls-tree --name-only`, and every other porcelain-ish path output, applies
`core.quotePath` — on by default. `schema/dbo.té.yml` comes back as
`"schema/dbo.t\303\251.yml"`, **quotes included**, and `git show <rev>:<that>`
answers `fatal: path ... does not exist`.

The failure was not that error, though, which is the part worth keeping. The
quoted form does not end in `.yml`, so the extension filter skipped the file,
the revision read as **empty**, and `plan` announced "Baseline: git HEAD (0
objects)" and exited 0 — every table newly created, against a repository that
was perfectly well formed. Absent, empty and unreadable are three different
things, and a test that asserts an exit code cannot tell them apart.

`-z` and split on NUL. Never `lines()`, never `trim()`.

## A remedy written where the finding is made

`refuse_unplanned_movement` ended its message with what to do about it —
"the transaction was rolled back; then apply again" — and the staged run
wrapped that message under "nothing was rolled back". The staged wrapper then
made the same mistake one level up: "resuming accepts it" at every read,
including the one after the last checkpoint, where a resume refuses (decision
190). A function that finds something does not know what its caller can do
about it. State the finding and the reason there; let each caller name the
way out it actually has — and check that way out against the code that
implements it, not against what it ought to do.

## A path in a command is spelled with `to_str`, never `display()`

On Unix a filename is bytes. `display()` substitutes U+FFFD, which `shell_arg`
neither refuses nor leaves bare — so a lossy path came back neatly quoted,
naming a file that does not exist, *after* the report had read the real one. The
same path must stay out of `output::Location`, whose `file` is a `String` for
this reason.

## Quoted for code, then dropped into a literal

`quote` and `literal` are not two spellings of "make this safe". They make a
value safe for **two different positions**, and each is useless in the other:
`quote` doubles `]`, `literal` doubles `'`.

`drop_primary_key` and `drop_default_block` build a statement as a *string* and
run it through `EXEC`. They passed the bracket-quoted table name to
`OBJECT_ID(...)` through `literal` correctly, and then interpolated the same
name raw into the `N'ALTER TABLE ... DROP CONSTRAINT '` prefix beside it. An
apostrophe is legal in a SQL Server identifier and `pull` adopts one, so
`dbo.o'brien` closed the literal early and the rest of the line parsed as code.
The hostile-identifier test covered `]` only — the character the *other* helper
handles — so it passed throughout.

The tell is a `format!` whose output is a SQL string rather than SQL: inside
one, every interpolation is in literal position, including the parts that look
like code. Build the fragment, then hand the whole thing to `literal` — the
shape `rows.rs` already used for its dynamic collation statement. Then a name
cannot be interpolated raw, because there is nowhere left to interpolate it.

## A guard built twice is a guard that fires early

`dev::Container::start` built its cleanup guard, then shadowed it with a second
one holding the same container id. A shadowed binding is **not** dropped early —
it lives to the end of the function — so the first guard's `Drop` ran
`docker rm -f` on the container just handed to the caller. `plan --dev
docker://...` failed with "connection refused" from the day the feature was
written.

It survived because **every `--dev` test passed a connection string**: the
docker path had no automated coverage at all. Reading found it; running could
not have.

## Bugs only the live suite could catch

The unit suite is structurally unable to find these. Run
`scripts/live-tests.sh` when touching the emitter, the catalog queries, the
ledger or the permission checks.

- Foreign-key ordering between two newly created tables.
- `EXEC()` rejecting function calls in its argument.
- `sql_expression_dependencies` returning one row per referenced *column*.
- Check constraints arriving as dependencies of their own table.
- The module round-trip: only a real `sys.sql_modules` says whether what the
  emitter sent is what comes back.
- Three permission bugs that survived the first live test because `sa` holds
  `CONTROL`. The permission matrix now runs against **real least-privilege
  logins** created inside the container.
- A probe naming a column the same plan is *adding*: `Msg 207, Invalid column
  name`, which the runner reports as unchecked and the apply proceeds past. The
  unit suite could only ever check that the SQL said what its author thought it
  said — and it did.

**A reasoned worry in an ADR's Limits was a shipped bug.** ADR-0013 §4 found,
on PostgreSQL, that comparing a declared expression against the engine's
respelling restates it on every connected plan, and its Limits noted the same
comparison runs on SQL Server "with no normalizer anywhere in the workspace" —
*a reasoned worry, not an observation*. Measured while landing the fix: SQL
Server respells every one of them (`GETDATE()` → `(getdate())`, `n > 0` →
`([n]>(0))`), and `plan --db` straight after a `bootstrap` dropped and rebuilt
an unchanged filtered index, marked destructive, on every run (DECISIONS 208).
Nothing in the live suite had ever re-planned a table with a check or a filter
after applying it. **When a design document says "the same code path runs on
the shipped engine, unmeasured", that sentence is a test that has not been
written yet** — run it before the design lands, because the fix for the future
engine is the fix for the present one.

## A round trip tested only on the simple case

Two P1s on the `ModuleId` PR (#47) were the same mistake in two places: an
identity was reduced to something shorter than itself, and the reduction was
tested only on inputs where it happened to be lossless.

`FromStr` split a routine's argument list on every comma, so
`app.f(decimal(10, 2))` — one argument — parsed as `decimal(10` and ` 2)`. The
round-trip test covered `app.f(integer,text)`, where flat splitting is right.
The failure was not a refused parse in isolation: `Display` wrote that key into
the state snapshot and the saved plan, so the artifact the tool wrote was one it
could not read back.

`declaration_file::module_path` built a trigger's filename from
`object_name()`, which is `schema.name` — the trigger's table, half of its
identity, was dropped on the way to disk. Two triggers named `audit` on
different tables produced one file, `pull` wrote the second over the first, and
the next plan would have dropped the trigger whose file had vanished. The whole
point of the change was that those are two objects.

**A third P1, the same PR, the inverse shape.** Widening a key from
`ObjectName` to `ModuleId` made two modules with one name representable for the
first time, and the whole-schema check still only compared modules against
tables — it had never needed to compare them against each other, because the
map could not hold the collision. The uniqueness was a property of the
container, so nothing in the diff looked like a deleted check. **When a key
gets wider, list what its narrowness was silently enforcing, and write each one
down as a check before the widening lands.**

**A fourth, the next round: the punctuation was already in the name.** The
string form uses `.` and `(` as structure, and SQL Server lets a quoted
identifier contain both. A view named `[audit.v1]` had always failed loudly at
the snapshot read — `dbo.audit.v1` was no shape an `ObjectName` could take —
and the typed key gave that string a meaning: a trigger named `v1` on
`dbo.audit`. A parse that used to refuse now succeeded with a different
identity, and no test noticed because none had held a name containing the
delimiter. **When a string form gains a grammar, every input that used to be
unparseable becomes a candidate for being parsed as something else** — list
them, and make the round trip `to_string().parse() == self` a checked property
where engine names enter (DECISIONS 205).

**The shape.** Whenever a typed identity is flattened to a string — a map key,
a filename, a message — test the flattening on the case where the parts are
*not* separable by the obvious character, and on two values that must not
collide. A round trip proved on `f(integer,text)` proves nothing about
`f(decimal(10, 2))`; a filename proved on one trigger proves nothing about two.
Check the sibling call sites in the same pass: the other two comma splits in
this repo (a column list, a type's own modifier args) are safe only because
their elements cannot nest, and that is a property worth confirming rather
than assuming.

## Tests that pass for the wrong reason

Nine so far, every one invisible in a green run. **Assert the specific failure,
not merely that something failed.**

- A plan fixture that failed at deserialization instead of at the emitter.
- `plan`'s identity check firing before its baseline load.
- A `pull` guard test taking the not-a-directory branch.
- A `status` detail test asserting a string the test itself had built.
- A permission test asserting a `why` string that had been guessed.
- A live permission test asked as a project managing `dbo`, where the `ALTER`
  it checked is required for an unrelated reason.
- A non-UTF-8 path test built on a **preview** plan, for which `explain`
  correctly prints no approval command at all.
- A GraphQL page read as a whole answer: `reviewThreads(first: 100)` on a PR
  with 105 threads returned `hasNextPage: true` and I reported "no new
  findings" from the truncated page. Not code, but the same shape as every
  entry in section 1, and the reason this bullet is here: **a paginated read
  that does not check `hasNextPage` is an absence, not an emptiness.**
- A format-version test asserting `json.contains(r#""version":1"#)` on a
  serialized state snapshot — which embeds an ids file whose own version is 1.
  It matched the *nested* field and went on passing through the bumps to 2, 3
  and 4, checking nothing about the snapshot's own version. **A `contains` over
  a serialized document is answered by any field that looks like the one you
  meant.** Assert the parsed field. The same shape was one bump away in the
  plan and ids tests, and all three were fixed together (DECISIONS 149's
  commit).

Since these appeared, every fix is reverted and its new test watched to fail
before the fix is kept. That habit caught three of them. It did not catch
the version assertion, and could not have: nothing about that test's own
subject was ever broken, so no revert of a *fix* would have failed it. What
finds that shape is asserting on the parsed field in the first place.

**Three fixes carry no test at all**, stated in `flow.rs` rather than papered
over: `pull`'s guard against a failed listing on a real directory, and the
`plan` / `fmt` write failures. All need permission bits or an immutable flag;
the suite may run as root **and** runs on Windows.

## YAML and file-format traps

- **`null` cannot be a YAML key** (it is the null literal); the field is
  `nullable`.
- **`no` / `yes` / `on` / `off` parse as booleans.** `pbps fmt` must quote
  boolean-ish, null-ish (`null` / `~`) and number-shaped string scalars.
- **LF everywhere in version control** (see `.gitattributes`); the tool writes
  files itself, and platform line endings would break "the tool owns the file
  format" on Windows.

## CI

- On Windows, `cmd.exe /C` does not decode the standard argv quoting that
  `Command::arg` produces. A hook containing an ordinary quoted path reached
  `cmd` with backslash-escaped quotes and failed before reading its payload.
  Build that command with Windows `CommandExt::raw_arg`, including the outer
  quote pair that `/S` removes; do not weaken the hook test by avoiding spaces
  or quotes in its path.
- The `live` job's SQL Server service container has crashed at startup twice, on
  two different commits: the failing step is `wait for SQL Server`, both cargo
  steps are **skipped**, and a re-run of the same commit passed both times. Read
  the job's step list before blaming the diff. **The cause is still unproven** —
  it looks resource-shaped (`errno 11`, core dump, 2-CPU runner), not
  image-shaped. That step now prints the container's own log on failure, which
  the raw job output otherwise buries under teardown noise.
- The engine is **pinned by digest** in both `ci.yml` and
  `scripts/live-tests.sh`. That is not a fix for the crashes above; it means the
  next one is reproducible rather than unrepeatable, and that a suite whose job
  is to answer "what does the engine actually do" cannot have the engine change
  under it between two runs of the same commit. Bump deliberately, and run the
  live tests against the new digest first.
- Find the service container by **published port**, never by
  `--filter ancestor=<image>`. That filter repeats the image reference, so the
  two spellings drift apart the moment one is pinned.
- The `docker://` rehearsal runs in its own job (`dev-rehearsal`), not alongside
  the service container: two engines on one runner is the pressure worth not
  adding. It stays opt-in locally through `PBPS_TEST_DEV_IMAGE`.
- **A name filter in CI must be asserted, not trusted.** `cargo test -- <name>`
  exits 0 having run nothing if the name stops matching, so `dev-rehearsal`
  greps for `1 passed`. Without that a rename leaves the job green and the
  coverage gone — the same shape as the tests that pass for the wrong reason.
- An opt-in test must **skip** when its variable is unset, not panic. Copying
  the `panic!` used for `PBPS_TEST_DB` — which CI always sets — turned "this
  test is not enabled here" into a red job.
- **A check run being green on the head commit does not mean the pull request
  can see it.** A check run reaches a PR through the *check suite* that holds
  it, and GitHub associates a suite with the PR only when the run's event is
  `pull_request`, `pull_request_target`, `push` or `merge_group`. A
  `workflow_dispatch` suite is associated with nothing, so the Actions tab
  showed five green jobs on the PR head while the PR's own checks list showed
  none and the merge box waited on "Expected — Waiting for status to be
  reported" forever. Querying check runs *by SHA* returns them and agrees with
  the Actions tab, which is why this reads as a GitHub fault rather than a
  configuration one. The gate reports a **commit status** instead: a status is
  addressed to a commit, not to a suite, so there is nothing left to associate
  (DECISIONS 206).
- **A required job skipped by `if:` counts as passing.** GitHub treats
  `success`, `skipped` and `neutral` alike in a required check, so guarding
  an expensive job with a label or a `draft` test opens the gate instead of
  closing it. If a condition must gate a merge, the job has to run and fail —
  or the gate has to be a separate report, as `ci-gate` is.
