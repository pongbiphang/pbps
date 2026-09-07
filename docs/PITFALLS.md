# Pitfalls

Bugs and traps this codebase has actually shipped or nearly shipped, and the
shape each one belongs to. The numbered *decisions* live in
[DECISIONS.md](DECISIONS.md); this file is the record of what went wrong, so the
same mistake is recognised the second time.

## The five recurring shapes

Every one of these was reported, then swept for, then found again somewhere else.
Treat a new instance as likely rather than surprising.

### 1. An error, an absence and an emptiness read as good news

Twenty-two instances so far. **Absent, empty and unreadable are three different
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
  makes `OBJECT_ID` answer NULL, so "cannot look" became "no lock". The *ledger*
  table was asked the same way and was not swept with it, so a principal with no
  permission on `__pbps_state` was told the database had never been touched —
  `doctor` saying `uninitialized`, `explain` offering `bootstrap`, `state list`
  drawing an empty history. Presence is attempted now, not asked: 208 absent,
  229 hidden (DECISIONS 219).
- `validate --since` reading a failed `ls-tree` as "no declarations at that
  revision", which calls every table new. From a project in a subdirectory it
  was empty every time: the pathspec lacked the `--full-tree` its sibling
  `load_from_git` had carried for exactly this since the baseline was written.
- A declared module the catalog **cannot read back** reduced to a warning. The
  recorder went ahead, the snapshot's schema could not hold the module, the
  scope every later command rebuilds from that schema forgot it, and the first
  `verify` refused an untouched database as a policy violation. It is a partial
  schema inside the managed set, and the recorders now refuse it as one.
- An entry in the ledger this build **cannot parse** failing the whole read:
  `state list` returned `Err` on the first unreadable row, so one row written
  by a version older than `OLDEST_READABLE_VERSION` erased every newer entry
  above it — the list a person opened the command to see. The row is carried
  now, with the ledger's own columns and the reason (DECISIONS 218). The rule
  holds one level down: it is about a row as much as about a table.
- Two *unreadables* flattened into one: a ledger row whose JSON does not parse
  and a row recorded by a version outside the readable range were carried as the
  same string, and the warning built from it said "recorded by a version this
  build cannot read" for both. The damaged row's operator was sent looking for
  an upgrade that does not exist for a damaged row. Typed apart now (DECISIONS
  222).
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

`state list` broke it a third way, and this one is visible without a database:
the branch emitted an `unanswerable` envelope and then returned `Found`, so the
JSON said "no answer" while the exit code said "act on this". A command that
reaches for `Found` on a path that also emits `unanswerable` has contradicted
itself; going through `or_unanswerable` leaves one thing producing both
(DECISIONS 220).

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

A permission held on a column is held nowhere the object question can see it.
SQL Server authorizes `SELECT`, `UPDATE` and `REFERENCES` column by column, and
`app.t(code PK, label)` with `GRANT UPDATE ON app.t(label)` and nothing wider:

| question | answer |
|---|---|
| `HAS_PERMS_BY_NAME('app.t','OBJECT','UPDATE')` | **0** |
| the same, `…,'UPDATE','label','COLUMN'` | 1 |
| `UPDATE app.t SET label = …` | **runs** |
| the same question for `'INSERT'` or `'DELETE'` at column scope | **NULL** |
| `GRANT INSERT ON app.t(label)` | **syntax error** |

Three lessons, all of them measured. A gap is not the only wrong answer a
readiness check can give: this one was an *over*-demand, and the remedy it
printed (`GRANT UPDATE ON app.t`) widened a grant a careful DBA had narrowed on
purpose. The permissions that take a column sub-entity are a fact about the
engine and not a property of the enum — asked for `INSERT`, the question
answers NULL, and NULL read as "not held" would have turned the repair into a
gap on every table. And the column list to ask about is the **declared** one,
not the catalog's: after `ALTER TABLE app.t ADD extra`, a column-only grantee
answers 0 on `extra` and its `UPDATE` is denied, while an object-level grantee
answers 1 and its `UPDATE` runs — so a list read from the catalog would have
called the account ready for the column the next plan adds.

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

## `sp_rename` carries almost everything, and the exceptions set the order

Measured on SQL Server 2025, one column per dependency kind, each renamed on
its own:

| the column participates in | `sp_rename ... 'COLUMN'` |
|---|---|
| a primary key, even one a foreign key references | carried |
| a unique constraint | carried |
| an index key column, or an `INCLUDE` column | carried |
| a filtered index's key column, where the predicate names another | carried |
| a default constraint | carried |
| **a check constraint** | **refused, 15336** |
| **a filtered index's predicate** | **refused, 5074 then 4922** |
| **a computed column's source** | **refused, 15336** |

The carried column is why `Renames::apply` exists: restating a constraint the
engine already moved is a plan that fights the engine. The refused column is
why the constraint drops sort *before* the column renames: the drop is what
makes the rename possible, and a plan that renamed first was a valid, reviewed
plan the engine would not perform, with no declaration the user could write to
fix it.

The shape: **an ordering derived from one half of a table is wrong for the
other half.** Both halves are the same statement here — `sp_rename` on a
column — and only measurement separates them. And the fix is the whole group,
not the two kinds that need it, because deciding which check names a renamed
column means parsing the expression, which this tool never does (decision
174). Verify a widened move costs nothing rather than narrowing it with a
parser.

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
changes are ranks 11 and 12; `AddCheck`, `AddUnique`, `SetPrimaryKey`,
`AddForeignKey` and `AddIndex` are all 13. So a plan that deletes its own
violations and then tightens was refused for violations that will be gone, and
one that writes violating rows was told there were none. `AlterColumnType`,
`AlterColumnNullability` and `AddColumn` sort at 8-10, before the rows, so
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

## A cell that can end its own row

`state list` draws a table whose widths are counted from the text, and the text
is a `--reason` and an operator name — free-form, and stored verbatim. One
newline in a reason ended the row early, and its tail was printed at column 1
where it read as another entry. The table was not garbled in a way a reader
notices; it was wrong in the one dimension the command exists to report, the
number of times this database was deployed to.

Escape for the terminal, keep the original in the JSON, and do it for **every**
cell rather than for the fields that are free text this week (DECISIONS 221).
The same question is worth asking of any rendering whose layout is computed
from its content.

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

**It happened again in the PostgreSQL emitter, in a scanner written years
after this section.** The rule there is the same on both counts — measured,
`'01/02/' <CR> '2026'` is one string constant and `'01/02/' -- c <CR> '2026'`
is too, because a bare CR both ends a line comment and supplies the newline a
continued constant needs. The gap scanner was written against `'\n'` alone, so
a session-decided default written across a CR walked past the guard that exists
to refuse it (DECISIONS 281).

The lesson is not "remember CR". It is that *this file already said so*, and
the second scanner still went in with one line ending — and then **the fix for
it missed its own sibling one screen away**: the same file's `skip_datum`, used
by the grouping unwrap, kept an LF-only search through that commit, so
`( -- ) <CR> '01/02/2026')` had its closing parenthesis swallowed and the same
default walked past the same guard by the other road. The next review found it.

Two things follow. When you write a predicate about where a line ends, in any
language, grep this file for the character class before choosing one — the
shape recurs because `\n` is what a person types when they mean "end of line".
And when you fix one, **grep the file you are editing for the literal you just
replaced** before pushing: a character class that lives in a named constant is
what makes the next omission visible, and two spellings of the same rule in one
file is the state that produced this.

**And a third time, in the SQL Server pull.** `skip_ws` — the whitespace and
comment skipper the module header parse runs on — searched for `\n` alone, so
`CREATE VIEW v -- note<CR> AS SELECT …` had the rest of its definition read as
comment, the `AS` was never found, and a module that runs perfectly well was
inventoried as one this tool cannot read. The failure direction was the mild one
— unreadable, not silently wrong — but "unreadable" was false, and the operator
was sent to look at a definition with nothing wrong with it. Found by grepping
for the shape, not by a bug report.

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

## The user's text and our syntax, on one line

Three things a declaration holds are written into the statement **verbatim**,
because only the engine can say what they mean: a column default, a check
expression and an index filter (ADR-0013 §3). The emitter's own syntax followed
each of them on the same line — `);` after a check, `,` after a default in a
column list, `);` after an index filter, `;` after a `SET DEFAULT`. A line
comment at the end of the user's text then swallowed it. Measured:

```text
CREATE TABLE t (n int, CONSTRAINT ck CHECK (n > 0 -- reason));
  -> ERROR: syntax error at end of input
CREATE TABLE t (a int DEFAULT 1 -- why, b int);
  -> ERROR: syntax error at end of input
```

A valid declaration produced a statement that cannot run — in five places, one
per site, because each site spelled the interpolation itself. The fix is one
newline, in one helper the five sites call, so the sixth has somewhere to reach
for (DECISIONS 281).

**The SQL Server emitter had it too, and it was found by looking rather than
by an apply.** The same three expressions, the same syntax behind them —
`) FOR [column];` after a default that is altered, `,` after one in a column
list, `);` after a check, `);` and any `ONLINE` clause after an index filter.
Measured on SQL Server 2025, `CREATE TABLE dbo.t (n int, CONSTRAINT ck CHECK (n
> 0 -- reason));` is `Incorrect syntax near '0'.` and the same text with the
closer on the next line is accepted. Two dialects wrote the same defect
independently, which is what "the emitter's syntax goes on its own line" being
a helper rather than a habit is for.

**And the guard that reads such text needs the same question asked of both
ends.** The bare-literal guard was taught that a comment is whitespace, then
that the grouping unwrap must step over data — and the gap between those two
fixes stayed open for two more rounds: the unwrap tests the *last character*,
so `('01/02/2026') -- note` could not be unwrapped and the ambiguous default
went through (DECISIONS 282). Each fix was correct and neither asked what the
*other* end of the expression looked like.

The general shape: **verbatim text ends in a state, not just in a character.**
Text copied from a declaration into generated code can leave the reader inside
a comment, a string or a quote, and everything the generator writes after it on
that line is then data. Any generator that interpolates user text has to ask
what state the text can end in, and close it — the same question a templating
engine answers with escaping and this one answers with a newline.

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

**And the second engine's counterpart got it wrong in the same place.**
PostgreSQL's `drop_primary_key` has no `EXEC`; it has a `DO` block whose body
runs `format('ALTER TABLE … DROP CONSTRAINT %I', pk)`. The first interpolation,
into `'…'::regclass`, went through `literal` correctly. The second — the format
string, which *looks* like the statement and is a literal — took the
bracket-free quoted name raw, and `app."it's"` closed it at the apostrophe. Two
crates, two dynamic-SQL helpers, one shape, and the second one was written by
someone who had read the first.

There is one twist worth stating, because getting it wrong the other way is
just as easy: the `DO` body is *dollar*-quoted, so the format string needs
**one** level of doubling and not two. The nesting to count is the number of
single-quoted strings the name sits inside, not the number of quotes of any
kind.

## A tag a name can spell

The same `DO` block introduced a second way for a name to escape. PostgreSQL's
lexer scans a dollar-quoted string for its closing tag **literally**, paying no
attention to quotes inside it — so `$pbps$` appearing anywhere in the body,
including inside a `'…'` literal, ends the block there. A table named
`x$pbps$y` is a legal identifier, and `pull` adopts whatever it finds.

Escaping cannot fix this: there is no escape inside a dollar-quoted string, by
definition. What fixes it is choosing the tag *after* building the body — the
first of `$pbps$`, `$pbps1$`, … that the body does not contain. A delimiter
picked before the content is a delimiter the content can forge.

## A setting that cannot take effect where it is written

The obvious way to pin a session setting around one statement is to put the
`SET` in front of it. Measured on PostgreSQL 18.6, that works for some settings
and silently does not for others, and which is which is not a matter of taste:
a multi-statement simple query is **lexed as a whole** before any of it runs,
and then analysed and executed one statement at a time.

```text
one batch:  SET LOCAL standard_conforming_strings = off; SELECT length('it\'s here');
            -> syntax error: the batch was lexed under the old value
one batch:  SET LOCAL search_path = bt, btx; CREATE TABLE bt.t (… CHECK (f(id) > 0));
            -> accepted: `f` resolved through the new path
```

`search_path` is read at parse *analysis*, so it takes effect for the next
statement of the same batch. `standard_conforming_strings` is read by the
*lexer*, so it does not — and the failure is the one that looks like success:
the emitter would carry a pin it believed in and the server would parse the
user's literal under whatever the operator's role had set.

The rule that falls out is per-setting, not per-statement: a setting that
decides how the text *parses* has to be established on an earlier batch, which
is what `transaction_framing().begin` is for; one that decides how a name
*resolves* can ride in the statement's own batch. Asking which of the two a
setting is takes one measurement and cannot be reasoned out from the
documentation, which describes both as session settings.

## A guard shaped for one carrier of a hazard the model carries three ways

`pbps-pg` learned that a session setting decides what a verbatim expression
means, and grew a guard: a bare literal default on a date-or-time column is
refused, because the applying session's `DateStyle` would pick the day. The
guard was right and it was one third of the sweep. ADR-0013 §3 names **three**
verbatim expressions — `Column::default`, `CheckConstraint::expression`,
`Index::filter` — and `CHECK (d >= '01/02/2026')` stores a different date under
a different `DateStyle` exactly as the default does. Review found it, and the
fix that covers all three is not a bigger guard.

The two halves are worth separating, because the second was the trap:

- The guard is **type-directed**. A default sits on a column whose type is
  right there, so "is this literal read through a session-sensitive input
  function" is answerable. A check expression names columns and carries no
  type, and no offline rule can tell `'01/02/2026'` inside one from a string
  that merely looks like a date. Extending the guard by shape would have
  refused `CHECK (status <> 'deleted')` — a valid plan refused, which is the
  one outcome worse than the hazard.
- So the fix went to the **reader** instead of to each carrier: pin the three
  settings in the transaction framing (DECISIONS 267) and the same text means
  one thing wherever it appears. One change, all three expressions, and nothing
  correct refused.

When a hazard is "a setting decides what this text means", count the places the
model can carry that text before writing the check, and ask whether the fix
belongs on the text or on the thing reading it. A guard per carrier is a sweep
you have to repeat every time the model grows a fourth.

**And then the list was closed against the wrong rule twice.** The pin went in with three
settings — the three that decide what a *temporal* literal is — and the next
review found a fourth, `timezone_abbreviations`, which `TimeZone = 'UTC'` does
not cover and which puts the same declared instant fifteen and a half hours
away. Sweeping for that one found a fifth, `transform_null_equals`, which is
not an input function at all: it is a parser rewrite that turns
`CHECK (x = NULL)` into `CHECK (x IS NULL)`, a different predicate from the one
in the approved plan. Three of the five were already named by ADR-0013 §3 and
the implementation carried the ones the case at hand had shown.

The list was closed against the wrong rule. "The settings temporal input reads"
is a category the first three fit and the next two do not; the rule the entries
were derived from is "a setting that changes what the declared text means",
which reaches all five and says how to test a sixth. When a list is derived,
write the derivation next to it — a list whose rule is left implicit gets
extended by resemblance to its existing members.

**And the exclusions need the same treatment as the entries.** Two settings
were kept out with a measurement and a reason: `bytea_output` and
`extra_float_digits` are *output-only*, measured, since a declared expression
stores the same constraint under `hex`/`1` as under `escape`/`0`. Both halves
were true and the conclusion was wrong, because "output-only" is a fact about
where the engine *reads* the setting and the question was whether a plan's
result depends on it. A cast to text runs a stored value through an output
function: measured, one approved `ALTER COLUMN b TYPE text` leaves `\x0102`
under `hex` and `\001\002` under `escape`.

An exclusion carries a claim as load-bearing as an entry's, and it is the half
nobody re-reads — the entries get exercised by every test that uses them, while
the reason a setting is absent is exercised by nothing. Write the exclusion's
measurement *and the case it was measured on*, so the next reader can see what
it does not cover.

## One change carrying both directions, in a list that orders directions

`order_key` sorts a plan by what each change *is*, and the classes are laid out
by direction: drops early, so they stop blocking, and adds late, so what they
name exists. `SetPrimaryKey` carries `from` and `to` in one variant, so it is a
drop and an add at once — and a variant can only be in one class. It was in the
addition class, which meant a key that was only being dropped was ordered as if
it were being added, behind every column change it blocks. Measured, the plan
that falls out is refused on both engines (DECISIONS 269).

The tell is in `order_key`'s own comment, written for a different case: *"Anything
with a real order between them belongs in separate classes; this tiebreaker
cannot express it."* A change that is two directions at once has a real order
against itself, and no class expresses that.

The first fix keyed the class on the change's *contents* — `to: None` is a drop
and travels with the drops — because it needed no new class and no renumbering.
That closed the shape only where one direction is absent, and the next review
found the other half by the same reasoning: a *replacement* is both directions
at once, no class is right for it, and the same plan is refused. So the
replacement became two changes, one per direction, and the ordering question
answers itself.

Then it happened a third time, to `AlterColumnDefault`, which carries `from`
and `to` the same way — a default *replaced* on a column that is also being
retyped has to have the type change run between its halves, and one change
cannot. Three instances is not a coincidence; it is the shape.

The lesson is the second half, not the first. A variant with one entry per
*object* rather than one per *direction* cannot be ordered by direction, and
patching the case where one direction happens to be absent leaves the case
where neither is. When a model has such a variant, the question is not "which
class does this belong in" but "does this change have one answer" — and if it
does not, it is not one change. Splitting it costs the plan a line, and buys
each half its own class, its own risk and its own place in what a reviewer
reads.

**And splitting has a downstream cost, which the next review found.** A plan is
read by things that were written when one change meant one word about a field.
The apply guard collects what the plan *promises* about each column field and
holds the closing read to all of it; with the default split in two, one column
carried `Default(false)` and then `Default(true)`, and no database can satisfy
both. Measured through the CLI, the three statements applied and the guard then
called its own result movement — `dbo.t column ``n`` does not have the default
this plan gives it` — and rolled the whole thing back, so a valid migration
could not be applied at all.

The same collector already had the answer beside it: the *parts* are keyed by
name rather than collected, because a redefinition is a drop and an add under
one name and holding both outcomes refused every one (DECISIONS 169). The
column fields were a `Vec` because until the split no plan said two things
about one field. So when you split a change in two, grep for what reads the
plan as a list of promises — a collector that was correct under "one change,
one promise" is a contradiction under two, and it fails *after* the statements
have run, which is the most expensive place to find out (DECISIONS 280).

## An accidental order that was load-bearing

`order_key` puts a column's type change and its default change in the same
class, and within a class the tiebreaker is the change's `Debug` rendering. So
`AlterColumnDefault` ran before `AlterColumnType` — by the alphabet, and by
nothing else. Measured on SQL Server, that alphabet was holding a plan up: an
`ALTER COLUMN` that changes a type is refused while a default constraint stands
on the column (5074, with 4922 behind it), and the default's drop happening to
sort first was the only reason a retyped defaulted column had ever worked.

PostgreSQL needs the opposite for the other half — a default written for the new
type cannot be set against the old one — so the rank went in as
`AlterColumnType => -1`, measured on that engine, and quietly broke the other.
Nothing in either suite covered a retyped column that has a default: the whole
guarantee lived in a sort that nobody had written down as a guarantee.

Two things to take from it. When you change an order, ask what was relying on
the old one — including the parts that were relying on it by accident, which
are exactly the parts no comment mentions. And when a fix is measured on one
engine, measure the same statement on the other before believing the rank:
here each engine refuses a *different* end of the same pair, and only running
both says the answer is three phases rather than two.

## A predicate written from the shape of the first measurement

The guard against a conversion the session's `TimeZone` answers went in with
one measurement behind it: `timestamp` → `timestamptz` stores a different
instant under a different zone. The predicate written from it asked **"does the
offset change"**, which is what that pair does, and the review found the pair
that does not: `timestamptz` → `timetz` keeps its offset on both sides and is
still the session's answer — measured, `12:00:00+00` from a `UTC` session and
`07:00:00-05` from `America/New_York` — because what moves is the *date* part,
and a zone decides which day a value was in.

The measurement was right and the generalisation was drawn from its silhouette.
The rule the guard exists for is "the session decides"; "the offset changes" is
one way that happens, and a predicate that spells the symptom passes everything
that reaches the same place by another route.

The tell is available before the review: the guard's own doc comment said
"True for exactly one shape", and a rule that is true for exactly one shape is
usually a rule that has only been looked at once. When you write a predicate
from a measurement, enumerate the family the measurement belongs to — here, the
six date-and-time types and the conversions between them — and check each
member against the *rule*, not against the example.

## The scanner's rule, respelled from memory in the crate that needed it

`pbps-dialect` records PostgreSQL's identifier grammar as a **byte** rule
(DECISIONS 233): `[A-Za-z\200-\377_0-9$]`, so every byte of a non-ASCII
character continues an identifier, and its comment carries the measurement —
`á` spelled `a` then U+0301 continues a name for the engine and ends one for
`char::is_alphanumeric`.

`pbps-pg`'s emitter then needed to know where a dollar-quoted literal ends, and
wrote the tag rule again: `c.is_alphabetic()`, `c.is_alphanumeric()`. The same
`á` measured the same way — `$á$…$á$` is one literal on 18.6 — read as *not* a
literal, so the guard that refuses an ambiguous date default never looked at
it. The rule had been decided, measured and written down, in a crate this one
already depends on, and the second spelling still went in.

The fix is not a better predicate, it is one predicate: `continues_ident` is
public now and the emitter calls it. When you find yourself writing a
character-class test for another system's grammar, search for it first — a rule
subtle enough to need a DECISIONS entry is subtle enough that your second
attempt will differ from your first.

## The host language's whitespace, standing in for the engine's

The same shape as the section above, one guard along. The unresolved-default
guard asks whether a declared default is one bare literal, and a string
constant may be *continued*: two quoted pieces separated by whitespace with a
newline in it are one constant. The scanner read that gap with `trim_start`.

`trim_start` is Rust's whitespace. The engine's `{whitespace}` counts a `--`
comment among it — measured, `'01/02/' -- c ⏎ '2026'` is the single constant
`01/02/2026` — so a default written that way read as *not* a bare literal and
went through the guard unrefused, to store February in one environment and
January in another. And the sibling form is not the sibling rule: a
`/* … */` comment is whitespace everywhere in this engine *except* in that
gap, where it ends the continuation outright (DECISIONS 278).

Two lessons, and the second is the one that cost the round:

- A lexical class named the same in two languages is not the same class.
  `whitespace`, `identifier`, `digit` and `newline` all differ between Rust and
  PostgreSQL, and `trim_start` is as much a respelling from memory as
  `is_alphanumeric` was.
- **Ask which way the guard fails silent.** This one refuses when it says
  *yes*, so every form it cannot read is a form that gets through. A guard with
  that polarity has to be told what it does not understand; leaving a
  construct unhandled is not neutral there, it is a permit.

The second lesson came back in the next round, in the same guard. The unwrap
that takes `('01/02/2026')` down to its literal counted **every** parenthesis,
and a comment beside the code said so on purpose: a stray one in a literal can
only make the test fail, and failing to unwrap merely costs a refusal. Same
mistake, written down and reviewed and kept — `(/* ) */ '01/02/2026')` is one
`)` of comment text, and the ambiguous default sails through (DECISIONS 279).

When a comment argues that a guard is *allowed* to be wrong in one direction,
check that direction against what the guard's answer does, not against how the
sentence sounds. Both of these read as caution and both were permits.

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

## One rule, spelled in three places

The definition scanner asks "does the identifier before this character end
here?" three times: before a `$` that might open a tag, before the `E` of an
escape string, and inside the tag itself. A review round measured PostgreSQL's
answer — the grammar is over **bytes**, `[A-Za-z\200-\377_0-9\$]`, so every
non-ASCII character continues a name — and fixed the one helper the finding
named. The other two kept asking Rust's `char::is_alphanumeric`, which says
that `á` spelled `a` then U+0301 ends a name. The next review found both.

Neither failed loudly. With `á$tag$` the scanner opened a literal where the
engine had a name, closed it at the real literal's opener, and then collapsed
the literal's body as code; with `áE'a\'` it armed the backslash rule over a
plain string and ran past the quote that ends it. Both end the same way: two
module definitions that differ compare equal, and a real change is never
emitted. The repair is one predicate all three call.

**When a measurement corrects a rule, give the rule one home — do not correct
the caller the finding happened to name.** A rule that lives in three places is
three chances to be measured once and fixed once.

## One property of a type standing in for what it holds

Six instances, five of them in the classification that decides whether a change
needs a human's approval, all in the direction that skips one. Each rule named a
real property of the type — its digit count, its significant digits, its
components, its range, its width in bytes — and each time the property was true
and not the whole answer.

- **`numeric(10,0)` and `integer` are both "ten digits".** Measured,
  `9999999999` into an `integer` is `integer out of range`; on SQL Server,
  `decimal(10,0)` into `int` is `Arithmetic overflow`. The integer types are not
  powers of ten, and a digit count cannot say so.
- **"the digits fit in the float" is not "the float holds the value".** `0.1` in
  a `real` is `0.10000000149011612`. The engine prints the shortest decimal that
  reads back as the same float, so `0.1::real::text` is `0.1` and every round
  trip through text agrees the value survived; ten of them sum to `1.0000001`
  where the exact sum is `1.0`.

- **"it stores every component the other one does" is not "it holds every
  value".** A `date` runs to 5874897 AD and a `timestamp` stops at 294276 AD,
  so adding a time to a date — the textbook widening — fails on
  `'300000-01-01'` with `date out of range for timestamp`.

- **"the seed fits the column" is not "the sequence will start there".** An
  `identity:` the model can spell carries a seed and an increment and nothing
  else, so the sequence behind it takes PostgreSQL's default bounds — `1 ..
  type_max` counting up, `type_min .. -1` counting down. A seed of `0` fits
  every integer type the engine will carry an identity on and is refused,
  `START value (0) cannot be less than MINVALUE (1)`; a seed of `5` counting
  down is refused for being too *large*. The column's range is the wrong range
  at both ends, and it is the one a reader reaches for.

- **"eight bytes, variable length" is not "a binary column".** SQL Server's
  `timestamp` (`rowversion`) was grouped with `varbinary`, so a capacity
  comparison answered every pair it took part in. Measured, the engine refuses
  `ALTER COLUMN` on *either* end of it — 4928 leaving the type, 4927 arriving
  at it — so `timestamp -> varbinary(8)`, the pair whose capacities match
  exactly, came out `Safe` for a statement that will not compile. The obvious
  repair is the wrong one twice over: the width the classifier read was 1, and
  correcting it to the 8 `sys.types` reports makes the same pair `Safe` for the
  same reason. The property that governs is not a width (#141).

Every one reads as obviously correct, and a test written by the same hand asks
the same question the rule does. **What catches them is a row at the boundary,
on a real server**: the largest value the source holds, and a value the target
cannot represent, with the promise asserted as *the statement runs and the value
does not change*. A classification cannot be checked against itself, and the
second one cannot be checked against the engine's own printing either.

The sixth is the exception that says what the others have in common. No row
catches it, because no row is wrong: `CONVERT(timestamp, 0xAB)` succeeds and
returns `0xAB00000000000000`, so the pre-flight probe over the column counts
zero and prints as a pass. **Where the boundary row cannot answer, the thing to
run is the statement itself** — and a prohibition no probe can see is one only
the classification can carry, which is why the fix had to remove the probe as
well as change the answer.

## The engine accepted the declaration and stored a different one

Not an error, not a warning worth the name, and not visible again until the
next run reports a change nobody made. Measured on PostgreSQL 18.6:

| Declared | Stored | How you find out |
|---|---|---|
| an identifier of 64 bytes | truncated to 63 | a `NOTICE` nothing reads |
| `interval(7)` | `interval(6)` | nothing at all |
| `time(7)`, `timestamp(7)` | `time(6)`, `timestamp(6)` | nothing at all |

Each one records itself at one value and reads back at another, so the drift
report never goes quiet and no plan can settle it — and each is *accepted*,
which is why none of them shows up in a test that only checks statements
succeed. Two names differing after byte 63 go further and **collide**: the
second `CREATE TABLE` fails naming a table the declarations do not contain.

**The rule.** A dialect enforces the engine's limits itself wherever the engine
*adjusts* rather than refuses. Where the engine refuses, the limit may be left
to it — the failure is loud and names itself. The two are found the same way,
and only one way: declare the out-of-range value and read the catalog back.

## A blanket refusal removed, and only part of it replaced

The PostgreSQL crate refused every table outright while the type catalogue was
unbuilt. Building the catalogue turned that one refusal into a real
`validate_table` — and the replacement covered the schema name, the table name,
the column names and the column types, because those are what the step was
about. It did not cover the primary key's name, the keys of `unique`,
`foreign_keys`, `checks` and `indexes`, nor any rule about `identity:`. Every
one of those had been refused the day before, by the blanket, and was silently
admitted the day after.

Nothing in the diff looked wrong. The new code was strictly more useful than
what it replaced, and each thing it checked, it checked correctly. The gap only
exists relative to what the blanket used to cover, and a diff does not show
that.

**The shape:** a coarse refusal is replaced by a precise one, and the precise
one is written from the feature that motivated it rather than from the set the
coarse one held. Anything the blanket covered incidentally is now permitted.

**How to avoid it:** when you delete a refusal, enumerate what it was refusing —
not what you are about to allow — and account for every item. Here that means
every name the object owns and every field of the declaration, not only the
ones the current step reads.

## The second implementation did not inherit the first one's scar

`Dialect::type_change_risk` documents that the caller normalizes the types
first. The SQL Server dialect normalizes them again anyway, with a comment
saying why: *a dialect that only works when it is called correctly is a trap,
and normalizing twice is free.* The PostgreSQL dialect, written from the trait,
did not — the trait is where the contract is written, and the contract says the
caller does it.

One caller cannot. `validate_saved_plan` re-derives a plan file's risks
*because the file may have been edited*, so its types are spelled however the
editor liked. Measured on the unguarded version: `int -> integer` came back
`Incompatible` and would block a plan that changes nothing, and
`character(5) -> character` came back `Safe` — an argument-free `character`
looks unbounded and is `character(1)` — which is a narrowing walking past the
gate.

**The shape:** a precondition stated on an interface, defended in the first
implementation and re-stated nowhere. The scar is in the older implementation's
comment, not in the trait, so the next implementation is written against the
contract and repeats the bug. This is the failure the phased dialects invite
most: eight more steps of #76 each re-implement methods SQL Server has already
been burned by.

**How to avoid it:** when a defensive measure exists in one implementation and
not in the interface, move the *reason* to the interface. The trait now says
the implementation must not depend on the caller having normalized, and names
the caller that cannot. Reading the sibling implementation beside the trait is
the other half, and it is what a call-site sweep is for.

**And a scar can be the wrong shape on the second engine, not just missing.**
`pbps-mssql` refuses a nullable primary key column because SQL Server refuses
the table; the rule buys an earlier, clearer failure and nothing else.
`pbps-pg` had no such rule, and measured, PostgreSQL does not refuse the table
— it sets `NOT NULL` itself and says nothing. The same missing rule is
therefore a *different and worse* defect on the second engine: not a late
failure but no failure, a declaration silently rewritten, and every plan
afterwards proposing a `DROP NOT NULL` the engine refuses with `column "id" is
in a primary key`.

Copying the rule across would have got the right behaviour for the wrong
reason, and the comment would have said "the engine refuses this" about an
engine that does not. Port the *question* the rule asks, then measure the
second engine's answer — it can be worse than the first one's, which is not
what "inherit the scar" leads you to expect.

## A vocabulary the engine has, answering a question it does not ask

`pbps-mssql`'s `doctor` is 1,773 lines of hard-won scope: which permission,
asked at which securable, and a comment on each saying which false positive or
false negative it was written for. Porting it to PostgreSQL is a translation
job — `HAS_PERMS_BY_NAME` becomes `has_table_privilege`, `ALTER` becomes... and
that is where it stops being one.

Measured on 18.6, as a role holding `GRANT ALL PRIVILEGES ON own.t`:

```text
has_table_privilege('own.t', 'SELECT,INSERT,UPDATE,DELETE,REFERENCES,TRIGGER')  ->  t
ALTER TABLE own.t ADD COLUMN c int   ->  42501: must be owner of table t
```

**On this engine DDL is authorized by ownership, and ownership is not a
privilege.** A readiness check built from the privilege vocabulary asks real
questions, gets true answers, and reports an environment ready that cannot run
one statement of a plan. It is not a check that is *sometimes* wrong: it is a
check that cannot ever fire, on the commonest misconfiguration there is.

**The shape:** the second engine has the *word* the first engine's question was
about, so the port compiles, runs, and reads plausibly — while the thing the
question was for is decided somewhere the port never looks. It is the sibling
of "the second implementation did not inherit the first one's scar": there, the
defence was missing; here, the defence is present and pointed at nothing.

**How to avoid it:** for each requirement, run the statement it is about as the
role the check just described, and assert the engine agrees. The live test does
exactly that — it reports the gap, then attempts the `ALTER TABLE` and asserts
`42501` — so the check and the engine are pinned to the same answer rather than
to the same vocabulary.

**And it is not one question, it is a family.** Review found the same shape
twice more in the same file, both times with `has_table_privilege` answering
`true`:

```text
REVOKE USAGE ON SCHEMA public FROM <role>;   -- the grants on the tables stay
has_table_privilege('public.__pbps_state', 'INSERT')  ->  t   (asked by oid)
INSERT INTO public.__pbps_state ...          ->  42501: permission denied for schema public
```

The privilege function is asked **by oid** and never resolves the name, so it
answers about the table's own ACL and knows nothing about the schema around it.
A check that asked only about the two ledger tables therefore reported an
environment ready in which no ledger statement can run — and the same hole was
one securable out, on the schema a referenced foreign-key target lives in.

The lesson for the *next* dialect: when a permission model has containers
(schemas, databases, roles), a check that asks about a leaf has to ask about
every container on the path to it. "Which securables does this statement touch"
is the question, not "which privilege does this statement need".

## A recovery path that the engine's own transaction rules delete

The SQL Server lock takes itself with an `INSERT` and, *when that insert fails*,
reads the lock table to name the holder. It is the natural shape: the insert is
the gate, and the read is only for the message.

Measured on PostgreSQL 18.6, the same shape does not merely fail — it takes the
caller's transaction with it. A failed statement aborts the whole transaction,
so the read that was supposed to produce the message comes back
`25P02: current transaction is aborted, commands ignored until end of
transaction block`, and everything the caller had done in that transaction is
gone. The refusal an operator sees would name no holder, and the deployment
that hit it would be in a state its own code did not put it in.

`INSERT ... ON CONFLICT (id) DO NOTHING` is the same gate without the failure:
a second inserter blocks on the index until the first commits and then affects
zero rows, so exactly one caller ever sees a count of 1.

**The shape:** an error path that is fine on one engine because errors there are
cheap, ported to one where an error is a cliff. The code reads identically; what
changed is what "the statement failed" costs. Look for it wherever the first
dialect *recovers* from a failure rather than propagating it — a `match` on an
error that continues working with the same connection is the tell.

**And the tell caught a second one in the same file, one review round later.**
`ensure_tables` tolerates the loser's `23505` from a concurrent
`CREATE TABLE IF NOT EXISTS`, because the table is there now and that is what
the caller asked for. True in autocommit; measured, inside a transaction the
same error aborts it, so `Ok(())` handed back a connection whose next statement
was `25P02`. The ledger entry is written inside the apply's transaction, so the
caller was always going to be one. The fix is a savepoint around the `CREATE` —
and the savepoint is taken by *trying* it, because `SAVEPOINT` outside a
transaction is `25P01` and harms nothing, which makes one round trip answer both
"can I recover here" and "am I in a transaction at all".

Two instances, one file, one shape: **every** `Err(e) if ... => Ok(())` in a
PostgreSQL path is a claim that the connection is still usable, and on this
engine that claim is false inside a transaction unless something rolled back.

## A time that reads differently to whoever asks

The ledger stores an ISO 8601 timestamp as text, and the obvious way to produce
it is `applied_at::text`. Measured on 18.6, under `DateStyle = 'German, DMY'`
that cast is `31.08.2026 09:14:22.517` — not sortable as text, not parseable as
ISO 8601, and produced by the *reading* session's setting rather than by
anything the writer did. Two operators reading the same row get two answers, and
one of them is a history that will not sort.

`to_char` with an explicit pattern is the fix, and the pattern has to be checked
for locale-sensitive fields as well: `TM`-prefixed ones read `lc_time`, so a
pattern holding one would have moved the same way for a different reason.

**The shape:** a rendering that looks like a property of the data and is a
property of the session. This crate already had the rule on the read side —
`catalog.rs` pins nine settings before it reads a definition — and the ledger is
a *different* file that renders a *different* kind of value, so the rule did not
arrive with it. The test that keeps it honest asserts the negative half too:
that the same column cast under that session really is unreadable, so it cannot
pass because the setting failed to take.

## One match arm, two directions, one direction's reason

`change_risk` classified `time -> interval` and `interval -> time` in a single
arm, and the comment above it argued one of them: *`interval '30 hours'` into
`time` is accepted and stores `06:00:00` — a day and a half is gone.* True, and
it says nothing at all about the other direction, which is a length of time
keeping its length. Measured, every boundary a `time` has — `00:00:00`,
`24:00:00`, `23:59:59.999999` — reads back from an `interval` unchanged.

The arm is the tell. Two orderings joined by `|` produce one answer, so the
author writes the reason for whichever ordering they were thinking about, and
the other rides along under it. Both were `Narrowing` here, which is the
harmless direction to be wrong in — a gate asked for on a change that never
loses — and the same construction with the answer `Safe` is a gate skipped.

It is also not a case of "the reviewer was right and the fix is the opposite
answer". The review asked for `time -> interval` to be `Safe`, and that is
wrong too: measured, `12:34:56.654321` into `interval(0)` **rounds** to
`12:34:57`, and into `interval(5)` to `12:34:56.65432`. The answer depends on
the target's seconds precision, and neither of the two blanket answers is it.

**The shape:** an arm serving two directions, justified for one. **How to
avoid it:** an arm that matches both orderings of a pair needs its comment to
say something about each, or it needs to be two arms. Splitting it is what
forced the measurement that found the precision.

## Measured on the right engine, through the wrong client

`psql` speaks the simple query protocol; `tokio-postgres` speaks the extended
one. Asked from `psql`, `xact_start < query_start` on `pg_stat_activity` was
exactly "a transaction was already open when this statement began" — equal
outside a transaction block, earlier inside one, and earlier too for a caller
that had run `BEGIN` and nothing else. Three cases, all correct, and the check
built on it refused **every** pull: through the driver the two timestamps differ
in both states, because Parse opens the implicit transaction before Execute
starts the statement's clock. `transaction_timestamp() =
statement_timestamp()` fails the same way and for the same reason.

**"Measure against a real engine" is not enough when the client is part of the
answer.** The question here was not what PostgreSQL stores but what this
connection can observe about itself, and only the driver the code actually uses
can answer that. What survived is a probe whose mechanism is the transaction
itself rather than a clock: `SET LOCAL` a custom GUC, read it back in a second
statement, and a value that is still there is a transaction block that outlived
the statement.

## The snapshot the rendering functions do not read from

`REPEATABLE READ` was added to the PostgreSQL pull so that five catalog queries
could not disagree about what exists. It does that. What it does **not** do is
make the pull immune to concurrent DDL, because `pg_get_constraintdef`,
`pg_get_expr` and `format_type` do not read the catalog tables — they go through
the syscache, which follows the latest committed state. **Measured**: with a
transaction open on a fixed snapshot, another session's `DROP TABLE` makes the
next rendering call fail with

```text
ERROR:  cache lookup failed for attribute 1 of relation 115849   (XX000)
```

**The shape:** a mechanism that fixes one layer, applied to a problem that
spans two. The isolation level governs what the *rows* say; the rendering
functions are a second source of truth reached by a different path, and no
isolation level covers them.

**How to avoid it:** name what the guard actually guarantees, in the place the
guard is set. Here the snapshot buys "the reads cannot disagree about what
exists" and not "the read cannot fail", and the loud failure is the better half
of that trade only because it is labelled — an `XX000` rendered by the seam as
`db error` (issue #167) would have been the third of absent, empty and
unreadable, wearing the clothes of the first.

## A join widened by one letter, matching a third thing

The columns query found an identity's sequence through `pg_depend` with
`deptype = 'i'`. Review pointed out that a `serial`'s sequence is `deptype =
'a'`, so the join was widened to `IN ('i', 'a')` — correct as far as it went,
and wrong, because `'a'` is *also* how an **index** depends on the columns it
indexes. Every indexed column acquired a sequence it does not have, and every
column with two indexes on it appeared in the pull twice.

The fix is one more condition — the dependent object has to be `relkind = 'S'`
— and the point is that the widening read as a two-value set when it is a
predicate over a relationship whose *other end* was never constrained.

**The shape:** a filter that names one kind of relationship is widened to a
second, and the widened form admits a third that was never in view. Nothing in
the diff shows the third one; it lives in the catalog's documentation.

**How to avoid it:** when a filter selects rows by a *kind*, say what the row
points at as well. Here the query asked "which dependency" and never "of what".
It was caught by the live test asserting that an ordinary table earns **no
warning at all** — the negative case, which is the one that noticed a warning
appearing where nothing had changed.

**And the widening was wrong a second way, which the narrowing did not fix.**
`IN ('i', 'a')` still asks one question of two relationships that can both hold
at once: measured, `ALTER SEQUENCE s OWNED BY t.c` on a column that is *already*
an identity is legal, and then the column has an `i` row and an `a` row. The
join returned the column twice, `RawIdentity` was built from whichever row came
back first, and the assembler's map kept the last — so a column declared
`IDENTITY (START WITH 7 INCREMENT BY 3)` read back as `START WITH 900 INCREMENT
BY 11`, the unrelated sequence's. Two joins now, one per dependency type, each
supplying the fact it means.

**The general form:** a set in an `IN` says "either of these", and the row set
says "both of these, sometimes". Widening a filter to a second kind is only safe
where the two kinds are exclusive, and nothing in the query says whether they
are.

**The same shape, one field over.** `pg_constraint.conindid` is "the index this
constraint is enforced by", and the pull built its skip set — the indexes not to
report again under `indexes:`, because they *are* a constraint — from every
constraint's `conindid`. Measured, a foreign key's `conindid` is the unique
index on the **referenced** table: another table's ordinary standalone index,
enforcing nothing for this key, and the only thing that makes the key legal. The
skip set swallowed it, so the pull compared clean while describing a schema that
cannot be built — adding the foreign key back would fail for want of the index
the pull did not mention. The filter said "constraints that have an index" where
it meant "constraints whose index is their own": `p`, `u`, `x`, and not `f`.

**And once more, in `pg_constraint`'s flags.** PostgreSQL 18 keeps `contype`
unchanged for two constraints that are not ordinary ones — `conperiod` marks
`WITHOUT OVERLAPS` / `PERIOD`, `conenforced = false` marks `NOT ENFORCED` — and
`connoinherit` has done the same for checks since long before. Each was read
back as the ordinary constraint it wears the type of. `NOT ENFORCED` is the
sharpest: it sits beside `convalidated`, which the pull *did* read, and the two
say opposite things — a `NOT VALID` constraint checks every new row, a
`NOT ENFORCED` one checks nothing and never will.

**And the second order of the fix itself.** The guard that takes a table out of
the pull for a name the declaration cannot write was a `continue` in the
per-table loop — after the lookup maps were built from every table the catalog
returned. A foreign key is assembled on the *referencing* table's turn, which
can come first, so it still resolved its target through a map that had not heard
about the refusal, and the pull recorded a key pointing at a table it had just
decided not to record. **A guard placed after the thing it guards is a guard for
one caller.** The decision now runs before anything that names a table is built,
and the maps describe the tables in the pull rather than the tables the catalog
returned.

**And a third time, from the other end.** A foreign key was recorded against a
uniqueness the pull had just refused — the referenced primary key had an
`INCLUDE` payload, so it was left out, and its backing index with it, and the
key still went in because the arm checked only that the target table and columns
existed. **A decision reachable through a map built before the decision was
made is a decision that has not happened yet.** The foreign keys now run in a
second pass, against what the constraint and index arms actually recorded
(DECISIONS 256).

**A guard whose condition is narrower than its reason.** The pull refused a
table with `relrowsecurity`, and the message said why: "whose policies this
model does not hold". The policies were the reason; the switch was the
condition. Measured, `CREATE POLICY` without `ENABLE ROW LEVEL SECURITY` leaves
`relrowsecurity` false and the policy rows there, so the table came through as
an ordinary one — and a rebuild drops the policies, after which whoever turns
row-level security on gets a table that is open where this one was about to be
closed. `relforcerowsecurity` is a third flag, independent of both. **Read the
guard's own sentence and check the code says the same thing**: when the reason
names an object and the condition names a switch, the condition is the narrower
of the two.

**A round trip that asked whether it parses.** The guard that refuses a value
the declaration format cannot write back was performed rather than reasoned
about — and then asked `is_err()`. `bit(3)` parses. It parses into a *different
type* than the one written out, because the pull stores an unreadable spelling
whole as the base and the parser splits it at the parenthesis. **"It parses" and
"it comes back the same" are two questions, and only the second one is the round
trip.** The check is now render-parse-compare-equal, and it is asked of the
value that is actually stored: one function decides that value for both the
guard and the construction, because a check on something *like* what is stored
is a check on nothing.

**One end of a relationship guarded, the other left open.** An inheritance
child was refused — its columns are somebody else's — and the parent was pulled
as an ordinary table. Measured, it is not one: a `SELECT` from the parent
returns the children's rows as well as its own, and `ALTER TABLE parent ADD
COLUMN` gives the column to every child. A managed parent would compare clean
while a plan against it silently changed tables nobody had declared. The live
test even asserted the wrong half, in a comment that stated the behaviour
instead of justifying it: "the inheritance parent is an ordinary table and
stays". **A relationship has two ends, and refusing one of them is a decision
about the other that nobody wrote down.**

**The shape both share:** the catalog answers "what kind of thing is this?" in
one column and "and is it that kind of thing after all?" in another. A reader
that switches on the first and never looks at the second is not reading the
catalog, it is reading half of it.

## Bugs only the live suite could catch

The unit suite is structurally unable to find these. Run
`scripts/live-tests.sh` when touching the emitter, the catalog queries, the
ledger or the permission checks, and `scripts/live-tests-pg.sh` when touching
the PostgreSQL crate, the connection seam or anything it depends on.

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

**A dependency's default features can panic a seam that compiles, lints and
audits clean.** `pbps-db` asked for `rustls` with `ring` while `tiberius-ng`
resolves the same `rustls` with its own default provider, so **both** were
compiled in. rustls then cannot determine a process-level provider and
**panics** — not errs — inside `ClientConfig::builder()`, which runs inside a
connection, where nothing can report it. `cargo build`, `cargo clippy` and
`cargo deny` were all green; the first PostgreSQL live test to open a real
connection was what said so, and it said so before there was a PostgreSQL
dialect to test (DECISIONS 228). Two rules come out of it: **name the provider,
never take the process default**, and **a new TLS or crypto dependency is a
live-suite change**, because nothing offline can tell you which providers your
dependency tree ended up with.

## A comment that describes a check the code does not make

Three now, and the third is the shape at its most flattering: a comment that
*reasons* rather than states, and is right about half its subject.

- `pbps-db::postgres::endpoint` fell back to `localhost` for a connection
  string it could not read, under a comment saying the connection would then
  "fail to connect saying so, instead of silently reaching a different host".
  It did the second thing: a machine configured with a Unix socket is the
  machine with a server on `localhost:5432`.
- `pbps-pg`'s `quote_ident` carried "the engine's own limit is bytes, not
  characters" and enforced no limit at all, while the SQL Server counterpart it
  was written from enforces one.

- `pbps-pg`'s `type_change_risk` fell back to the declared type when
  normalization failed, under a comment saying that value "no family claims and
  every pair therefore calls `Incompatible` — the conservative answer". True of
  an unknown *base*. Not true of a rejected *modifier* on a known base, which
  keeps the base every family does claim: `numeric(1000) -> numeric(1001)` came
  out `Safe` on a precision this engine does not have, and
  `interval(6) -> interval(7)` came out `Safe` on the precision the engine
  silently stores as 6 — which is the round trip the catalogue refuses the
  declaration for. The unit test that was meant to pin the claim asked it only
  of `serial` and `nonesuch`, both unknown bases, so it passed on the half that
  was true. The same fallback was in the shipped SQL Server dialect, and stayed
  there after the PostgreSQL half was fixed, because the finding was written
  about the file it was found in and closing it there read as closing it:
  `decimal(38,0) -> decimal(39,0)` and `varchar(8000) -> varchar(9000)` read
  `Safe` past a maximum precision of 38 and a maximum length of 8000 (#160).

A comment stating a rule reads, to the next person and to the reviewer skimming
for one, as a rule that is applied. **When a comment names a limit or a
guarantee, the line that enforces it should be the next one** — and when it
cannot be, the comment has to say that the check is somewhere else and where.
A comment that argues *why* the code is safe needs the argument's cases
enumerated and each one tested, or the test pins the case the author was
already thinking of. **Prefer removing the fallback to reasoning about it**: the
fixed version returns `Incompatible` when either side fails to normalize, and
there is no longer a case to be half right about.

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

## A matching loop where the loser leaves no trace

A loop that pairs supplied intent against `disappeared` and `appeared` decides
by consumption: `disappeared.remove(from) && appeared.remove(to)`. Nothing in
that expression asks whether a second intent wanted the same name, so the first
to be reached wins and the rest fall through — and the winner is declaration
order, which is not a decision anyone made.

The reason it was *silent* rather than merely wrong is the second half.
`resolve` sweeps unmatched intents at the end and reports each as an
`UnusedIntent`, but the sweep asks `intent_is_absorbed`, which reads the ids
file — into which the winning rename has by then written the loser's target as
a fresh column. The loser therefore reads as **already done**. A guard placed
after a mutation is asking about a world the mutation has already changed
(DECISIONS 246).

The tell is that the two arms of the failure look different from each other:
contending for a source returned `Ok` and renamed the wrong column, contending
for a target returned `Err` with "matches nothing … likely a typo" about an
intent whose every name exists. **When one shape produces a wrong success and a
misleading failure depending on which half collides, the check is missing
upstream of both** — not in either arm.

**Ask "can two of these claim one thing?" of every loop that consumes from a
set.** The answer decides whether the loop is a matching or a race, and the
question is not visible in the loop's own text.

**And the guard for it inherits the loop's filter.** The first form of this one
grouped every rename intent it was given, which is wider than the loop it
protects: the loop only ever considers an intent whose source is disappearing
and whose target is appearing. A leftover annotation — the thing
`intent_is_absorbed` exists to tolerate — shares a source with a live rename as
soon as the vacated name is reused, and the wider guard refused that valid plan.
**A guard that admits more than the code it guards is a new way to say no.**

**And a guard that stops the work is a new way to say the wrong no.** The first
form raised its blocker and skipped the rest of the resolver, which reads as
caution and is not: the skipped decision was discarded anyway, and skipping it
stranded every unrelated intent of that kind, so the sweep at the end called a
correct annotation a likely typo. **A check that has found something should add
to the report, not subtract from the work** — unless the work would write
something, and here it never does.

**Three rounds, three ways to lose the same index.** The misleading half was
suppressed by recording each contending intent's position in a `used` set, and
that record was defeated twice more: by the early return above, which stranded
the *neighbours* rather than the contenders, and by collapsing repeated intents
before their positions were recorded, which stranded one copy of a claimant
already inside the conflict. The fix that ended it was not a fourth patch but a
move: the sweep now asks whether a conflict already names this intent, by
equality. **When the same defect returns with a new way to lose the bookkeeping,
stop mending the bookkeeping and derive the answer from the thing itself.**

## Tests that pass for the wrong reason

**And its mirror: a test that is green under the command you happened to run.**
The three introspection live tests each built a probe schema named from the
process id, and each dropped it before creating it. Run with
`--test-threads=1` — the command a developer reaches for while writing one —
all three passed. Run through `scripts/live-tests-pg.sh`, which does not
serialise, two of them destroyed the third's schema mid-read. The failure was
loud, so this cost minutes rather than a release; the lesson is that **the local
command and the CI command are two different tests**, and only one of them
counts. A fixture name must be unique per test, not per process.

Twelve so far, every one invisible in a green run. **Assert the specific failure,
not merely that something failed.**

A fixture for "a state recorded by a version this build cannot read", written
by hand at that version, was not one: the reader parsed the shape before the
version, so it failed on a missing field and exercised the *malformed* branch
instead. The test asserted the wording of a warning it was never producing for
the reason it named. `StateSnapshot::from_json` reading the version first (#50)
makes the case reachable, and the fixture now carries one row of each of the
three ways a state can be unreadable rather than one row asserted about twice.

- The envelope-schema test validated `doctor`'s output — with no environments
  configured, so `EnvDiagnosis`, the type with the fields that vanish when the
  news is good, was never serialized. The command was covered; the shape was
  not, and the published schema required two fields the healthy case omits
  (DECISIONS 223). **Covering a command is not covering its payload's nested
  types.**
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
- A live introspection test that asserted over `pulled.warnings` — every
  warning in the **whole database**. The shared test server carries the probe
  schemas of every run that crashed before its `DROP SCHEMA`, so an expectation
  the current fixture no longer produced was still satisfied, by a schema
  written by an older build of the same test. Two of its expectations had also
  drifted out of matching the message they named (`` `collated` `` where the
  message says `` `schema.collated` ``, and a trailing backtick where the
  message says `COLLATE "C"`), and neither showed. **A live assertion scoped to
  the server rather than to the fixture is answered by whatever else is on the
  server.** Scoped to this suite's own schema, both drifts failed at once — and
  a third thing fell out: a table the pull refuses whole earned a warning but no
  `Limitation`, so the list that names a table said nothing about the tables
  nothing could be said about.
- A format-version test asserting `json.contains(r#""version":1"#)` on a
  serialized state snapshot — which embeds an ids file whose own version is 1.
  It matched the *nested* field and went on passing through the bumps to 2, 3
  and 4, checking nothing about the snapshot's own version. **A `contains` over
  a serialized document is answered by any field that looks like the one you
  meant.** Assert the parsed field. The same shape was one bump away in the
  plan and ids tests, and all three were fixed together (DECISIONS 149's
  commit).
- A guard over several kinds, asserted on the one kind where it does nothing.
  The PostgreSQL dependency reader filters `pg_depend`'s **internal** edges,
  `deptype <> 'i'`, and the test that covered it asked for the dependents of a
  *function* — which has no internal reverse edges at all, so removing the
  filter changed nothing and the test stayed green. A view has two, its
  `_RETURN` rule and its row type, and without the filter every view depends on
  itself and no view can be rebuilt. **Revert-and-watch-fail is what found it,
  and only because the revert was run:** the test was written first and looked
  like coverage. When a guard names a set — kinds, catalogs, classes — the case
  it is asserted on has to be one the guard actually changes, which is not
  always the first one that comes to hand.

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
- **A `cfg`-gated test takes its imports and helpers with it.** A test that is
  Linux-only because it depends on Linux's behaviour is right to be absent
  elsewhere, but the `use` it needs is not: `-D warnings` turns an import
  nothing uses into an **error on the other platform only**, which the
  developer's own machine cannot see. Gate the import with the same `cfg`, and
  check it by flipping the gate to a platform that is not this one
  (`target_os = "windows"` here) and building the workspace — the compiler then
  removes exactly what the other platform removes.
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
