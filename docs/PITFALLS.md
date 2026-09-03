# Pitfalls

Bugs and traps this codebase has actually shipped or nearly shipped, and the
shape each one belongs to. The numbered *decisions* live in
[DECISIONS.md](DECISIONS.md); this file is the record of what went wrong, so the
same mistake is recognised the second time.

## The four recurring shapes

Every one of these was reported, then swept for, then found again somewhere else.
Treat a new instance as likely rather than surprising.

### 1. An error, an absence and an emptiness read as good news

Sixteen instances so far. **Absent, empty and unreadable are three different
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

## A path in a command is spelled with `to_str`, never `display()`

On Unix a filename is bytes. `display()` substitutes U+FFFD, which `shell_arg`
neither refuses nor leaves bare — so a lossy path came back neatly quoted,
naming a file that does not exist, *after* the report had read the real one. The
same path must stay out of `output::Location`, whose `file` is a `String` for
this reason.

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

## Tests that pass for the wrong reason

Seven so far, every one invisible in a green run. **Assert the specific failure,
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

Since these appeared, every fix is reverted and its new test watched to fail
before the fix is kept. That habit caught three of the seven.

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
