# Commands, exit codes and the envelope

Exit codes, the findings envelope, published schemas, flags, and how messages
address the operator. Part of the [decision record](../DECISIONS.md), which says
how to add an entry here.

<a id="decision-18"></a>

18. **`docs` output must stay deterministic and self-contained** — identical
    declarations produce byte-identical files, and the HTML references nothing
    external (the air-gap rule applies to artifacts). That is why the ERD
    travels as Mermaid source rather than a script-rendered diagram.

<a id="decision-34"></a>

34. **There are three exit codes, and the split is the feature.** 0 clean, 2 the
    command answered and found something to act on, 1 the command could not
    answer. `Found` (was `DriftFound`) carries the 2; an empty message means the
    detail is already printed. A pipeline that cannot tell 1 from 2 wakes the
    wrong person half the time.

<a id="decision-35"></a>

35. **One findings envelope, not one shape per command** (`cli::output`). A
    command's own payload rides in `data` — `verify`'s drift report, `status`'s
    rows, `explain`'s explanation — and never replaces the envelope. `id` is
    stable and is what a future `policies:` block will re-weight, so it must
    survive a reworded message. **The hook payload is deliberately not wrapped**:
    a script's input must not change shape because someone added a flag for
    their own eyes.

<a id="decision-37"></a>

37. **The vendor annotation formats stay outside the binary**
    (`scripts/findings-to-github.py`). Each one compiled in has to be kept
    working forever, including for users who run neither.

<a id="decision-39"></a>

39. **`explain` always exits 0 and needs no connection.** It is the reviewer's
    command, and the reviewer may have no checkout and no credentials; the gate
    is `apply --allow`. A target is optional and answers only the question no
    file can — whether that environment is mid-deployment.

<a id="decision-40"></a>

40. **The prompt is a wrapper, never a shortcut.** Answers become ordinary
    `Intent`s through the same `resolve`, so the artifact is identical.
    Similarity orders the candidates and never decides; nothing is charitably
    interpreted; a partial answer records nothing. `--no-input` declines a
    prompt and can never answer one — that is why it is safe in an alias, and
    why SPEC 14.3 still refuses `--assume-renames`.

<a id="decision-41"></a>

41. **The editor schemas are generated from the loader's own types**, so
    `deny_unknown_fields` reaches an editor as `additionalProperties: false`.
    A schema that accepted more than the loader would be worse than none. The
    copies in `schemas/` are pinned to the binary by a test; regenerate with
    `pbps schema --kind <k> --out schemas/<file>`. Where a *semantic* rule
    outlives the derive's shape, it is stated as a `oneOf`: a module declares
    exactly one kind (and `on:` only on a trigger), and `dev:` names exactly one
    backend. Both were blessing documents the tool then refuses. **Every branch
    must exclude every key the others require**, and with `{"type": "null"}`
    rather than `false`: `oneOf` means *exactly one* branch matches, so a branch
    that constrains only its own key lets a two-kind document through the moment
    another branch is disqualified for an unrelated reason — and `false` would
    refuse `procedure:` written empty, which serde reads as absent.

<a id="decision-43"></a>

43. **`--check` is read-only in every direction.** It refuses `--dev`, `--out`
    and `--sql` rather than ignoring them: a CI check that skipped the write
    would leave the previous run's plan.sql on disk for the job to review. The
    refusals sit with the other flag validations, before the command runs, so
    they reach the JSON envelope — the `--dev` half used to be a `bail!` after
    the writes, and the `--out` half was reached whenever the ids file happened
    to be current.

<a id="decision-47"></a>

47. **A flag is honoured or refused, never accepted and dropped.**
    `plan --db --format json` took the flag and ignored it — prose on success,
    empty stdout on failure, while the flag validations in the same invocation
    answered JSON properly. It is refused now, not implemented: `plan --db`
    produces an *artifact*, and its typed form already exists and is better than
    an envelope — `--out plan.json`, read back with `explain --plan --format
    json`. A second typed rendering would give a reviewer two documents to
    disagree about.

<a id="decision-48"></a>

48. **A path is spelled with `to_str`, never `display()`, before it is put in a
    command.** On Unix a filename is bytes; `display()` substitutes U+FFFD,
    which `shell_arg` neither refuses nor leaves bare — so a lossy path came
    back neatly quoted, naming a file that does not exist. A path that cannot be
    spelled is in the same position as one that cannot be quoted: the
    placeholder, with the literal printed where nothing can execute it. The same
    path must also stay out of `output::Location`, whose `file` is a **`String`**
    for this reason: serde's `Path` impl *fails* on non-UTF-8, so a `PathBuf`
    there made that failure the whole envelope's — `explain --format json`
    printed nothing at all. `Finding::at` drops the location rather than store a
    lossy one that points at a different file.

<a id="decision-50"></a>

50. **`shell_arg` has now been wrong about shells five times.** Single quotes in
    `cmd`; backslashes; `!` under delayed expansion; a leading `-`, which no
    quoting can carry because the shell strips the quotes and clap then reads a
    flag (the `--opt=value` form would work, and was declined — it changes every
    advertised command to buy a hyphen-leading name); and a leading `@`, which
    PowerShell splats in argument position. Every one was found by someone
    testing rather than by reasoning, which is the argument for the placeholder
    being the default answer rather than the last resort.

<a id="decision-196"></a>

196. **A message names a command only to a caller who can run it, and the
    target carries which one that is.** The staged checkpoint refusal was given
    a pointer to `pbps status` (192's sibling, #15): the checkpoint records the
    movement, so `verify` reads clean, and `status` shows the refusal again as
    the failed entry's reason. True for half the callers. `status` takes no
    target at all — it reports on the environments `pbps.yml` configures
    (SPEC 9.2) — so a run that named its database with `--db` was sent to a
    command that answers about other databases, or says none are configured.
    The refusal exists to say where the record is; a pointer that cannot be
    followed is worse there than no pointer.

    The condition is not guessable from what the message had. `Target::label`
    is the environment name for `--env` and a redacted `server/database` for
    `--db`, and the two are the same *kind* of string: reading the origin off
    the label is a heuristic. So `Target` carries `environment: Option<String>`,
    set where `--db` and `--env` are already told apart, and the message asks
    it. The sentence stands without the pointer — "the list above is the record
    of what moved" is the answer — so the `--db` form loses nothing but a dead
    end. Measured against the live server, the same shape sits in `doctor`,
    whose remedies spell `--env <redacted label>` for a `--db` target; that is a
    separate command and a separate issue, not scope here.

<a id="decision-197"></a>

197. **A remedy spells the target the way its caller named it.** 196 stopped a
    message naming a command the caller could not run; this is the same shape
    one level down, in the commands a remedy *is*. `doctor`'s per-environment
    remedies — `baseline`, `apply --staged --resume`, `unlock` — each require
    one of `--db` and `--env`, and all three spelled `--env` with
    `EnvDiagnosis::environment`. That field is the display name: the
    environment for an `--env` target, and `db::redact`'s `server/database` for
    a `--db` one. Measured against the live server, `doctor --db` offered
    `pbps unlock --env "localhost,14330"` — an environment `pbps.yml` does not
    have, in a line advertised as copy-pastable.

    So the diagnosis carries `env_name: Option<String>` beside the display
    name, `Some` only where the caller gave one (the `--env` argument, or a key
    of `pbps.yml` in the all-environments loop), and one `target_arg` builds
    the flag: the quoted name, or `--db <connection string>`. The connection
    string is a placeholder rather than the string itself — it carries the
    password, and remedies are printed into CI logs (`explain` already made
    this choice for its approval command, which is why it was not wrong).
    The field is not serialized: it repeats `environment` where it is `Some`
    and says nothing where it is `None`.

    Watching the reverted fix fail is what showed the live half was not being
    measured at all: `doctor_does_not_report_ready_while_the_lock_is_held`
    takes its lock through `docker exec`, and on a host where `docker` cannot
    reach the container it returns early and reports a pass. Under a shim it
    fails against the old spelling with the bad remedy in its output.

<a id="decision-213"></a>

213. **The declaration schema says what the loader accepts, and completes from
    what `fmt` writes.** `Permission::from_str` folds case and reads `_` or a
    space where the canonical word has `-`, so that a user who types what the
    engine prints (`VIEW DEFINITION`, `view_definition`) is not corrected
    (ADR-0010 §6). The editor schema listed the canonical words alone, so a
    schema-aware editor flagged `dbo.t: [SELECT]` while `pbps validate`
    accepted it — the editor stricter than the loader, and the editor is what
    a user reads first.

    The `items` schema is now two branches. A `pattern` says which spellings
    validate; an `enum` of the canonical words is what an editor completes
    from. One list could not do both: a closed list wide enough to accept
    every spelling would offer all of them as completions, and the word `fmt`
    writes would be one suggestion among four.

    The pattern is written out per character — `[Ss][Ee][Ll]...` — rather than
    with a case-insensitive flag, because JSON Schema's patterns are ECMA-262,
    which has no inline `(?i)`; a schema carrying one would be a pattern every
    validator reads differently. Both branches are generated from
    `Permission::ALL`, so neither can drift from the loader, and the tests
    assert each spelling against `Permission::from_str` as well as against the
    pattern — a pattern nothing runs is a claim, not a check.

    The padding is not `\s` either, for the same reason at a smaller scale.
    ECMA-262's `\s` is neither a subset nor a superset of Rust's
    `char::is_whitespace`, which is what `trim` asks: it omits U+0085, which
    `trim` removes, and includes U+FEFF, which `trim` leaves in place. Written
    with `\s` the schema would have broken *both* halves of 172 at once —
    refusing a padded declaration `validate` accepts, and blessing one it
    refuses — two characters wide in each direction. The class is scanned out
    of `char::is_whitespace` and written with literal characters, because the
    two readers of this pattern share no escape syntax: ECMA-262 spells U+1680
    `\u1680` and has no `\x{...}`, and the `regex` family the tests run it with
    is the mirror image. A literal character is what both read.

    `SCHEMA_VERSION` goes to 8, for the reason it exists: version 7 published a
    closed list that refused `SELECT`, and this one accepts it. A consumer
    keying on that number to cache or select an artifact could not otherwise
    tell the two apart, which is the drift detection the field is for.

<a id="decision-214"></a>

214. **The envelope's schema is published as one document with a branch per
    command, selected by `command`.** SPEC §9.8's shape has been a Rust type
    and an example since Phase 3.1; ADR-0015 decision 1 makes it the whole
    contract between the UI and the tool, and a contract nothing publishes is
    one nobody can check against.

    One document rather than one per command, because what a consumer holds is
    *an envelope*: it reads `command` and only then knows what `data` is.
    Publishing them separately would make it choose a schema before reading
    the field that decides which one applies. Each branch pins `command` to a
    constant, so `oneOf` picks exactly one and a payload that happens to fit
    another command's shape is rejected rather than silently read as that
    command's.

    Every branch is generated from the type the command serializes, the way
    the declaration and config schemas are generated from the loader's and the
    config's types (SPEC §14.1). The list of command-to-payload pairs is
    written once, in `envelope_branches!`, and a flow test reads the command
    names back out of the published schema and compares them with the
    commands `--help` says take `--format json` — so a command given the flag
    without a line in that list, or a line without the flag, is a failing test
    rather than an envelope nothing describes.

<a id="decision-215"></a>

215. **`plan --db` stays outside the envelope set.** #64 asked whether the
    connected plan should join it now that a UI will trigger one (step 5).

    It does not, and the reason is the one SPEC §9.8 already gives: `plan --db`
    is not a read-only command. It connects, reads the ledger, and *writes the
    deployment artifact* — the file the checksum gate pins. What a reviewer
    reads from that artifact is `explain`, which does speak the envelope, and
    which reads the file rather than the run that produced it. A UI that
    rendered the producing run's own JSON would be showing a description of the
    artifact that was not computed from the artifact, which is precisely the
    gap the checksum exists to close.

    So the UI's trigger path stays: run `plan --db`, then read the plan back
    with `explain --plan --format json`. ADR-0015 decision 1's list of what
    speaks the envelope is exact and unchanged.

<a id="decision-216"></a>

216. **`state list` carries the ledger's columns, never the recorded schema.**
    The timeline the UI draws needs an id, a time, a kind, an operator and the
    provenance fields; the snapshot's `schema` and `ids` are the whole database
    twice over. A payload that carried them would send megabytes to a page
    drawing a list of dates, and they are what `state show` and `state export`
    are for — the second half of SPEC §14.1's row, deliberately not built here.

    `initialized` sits beside `entries` because an empty list is the one
    rendering that must never stand for "this database has no ledger" or "the
    server could not be reached". The three are a note plus `initialized:
    false`, a note plus `initialized: true`, and `unanswerable` with no `data`
    at all — three answers, as the repository's rule for absent, empty and
    unreadable requires.

<a id="decision-217"></a>

217. **A `--limit` too large saturates; it does not wrap and does not refuse.**
    `TOP (n)` takes a signed 32-bit count and the flag takes a `u32`, so
    `--limit 4294967295` cast with `as` became `TOP (-1)` and the server
    refused the whole query. A number meaning "more than there could ever be"
    turning into an error is the wrong answer twice over: the caller asked for
    everything and got nothing, and the message named a syntax error rather
    than a limit.

    Two guards, because they fail differently. The flag's parser refuses `0`
    and anything above `i32::MAX`, so a person who types a number the ledger
    cannot mean is told so by name. The reader saturates, because it is a
    library function whose caller need not be the CLI, and "as many as the
    server can return" is the only reading of a count larger than any table.

<a id="decision-220"></a>

220. **An `unanswerable` envelope exits 1, and never 2.** `state list`'s
    ledger-read failure built its own findings and returned `Found::reported()`,
    which `main` maps to `EXIT_FINDING`. The JSON said the question could not be
    answered while the exit code said there was something to act on — decision
    34's two audiences, given contradictory instructions by the same run.

    The branch goes through `output::or_unanswerable` like every other step in
    the command, so the envelope and the exit code are produced by one thing.
    That is the general rule: a command that reaches for `Found` on a path where
    it also emits `unanswerable` has routed a tool failure to the wrong person.

<a id="decision-221"></a>

221. **A table cell is escaped for the terminal; the JSON keeps the original.**
    `state list --format human` lays its columns out by counting characters, and
    `operator` and `reason` are free text: `--reason $'ticket-9\nwhy'` reaches
    the ledger as written, and a `failed` entry can carry a driver's multi-line
    message. A cell holding a line break ended its row early, so the rest of the
    row began again at column 1 and read as an entry of its own — a table that
    did not say "this reason had a newline in it" but said something false about
    how many times the database had been deployed to, in a command whose whole
    output is that list.

    Control characters are shown escaped (`\n`, `\r`, `\t`, `\u{7}`) rather than
    stripped: what was recorded is the point of the column, and a silently
    shortened reason is the same class of lie one column over. `--format json`
    is untouched — a consumer parsing the envelope wants the bytes the operator
    typed, and JSON has its own escaping for them.

    Applied to every cell rather than to the two that are free text today. The
    rule the repository keeps arriving at: prefer making the bad value
    unrepresentable over checking for it at the sites that happen to hold it now.

<a id="decision-223"></a>

223. **A field `serde` may omit is a field `schemars` must call optional.**
    `EnvDiagnosis` skips `permissions_unknown` when it is false and
    `absent_schemas` when it is empty — the good news, which is what an
    operator sees most. `schemars` has no way to know that: it reads
    `skip_serializing_if` as nothing at all and lists the field under
    `required`. So the published envelope schema rejected the healthy `doctor`
    output, and the contract ADR-0015 decision 1 rests on was broken by the
    first command to exercise it.

    `#[serde(default, skip_serializing_if = ...)]` is the pairing that keeps
    them honest, and it is what every other such field in the workspace already
    carried — these two were the only pair without it, which is why the sweep
    for this shape found nothing else. `default` on a type that is only
    serialized reads oddly for a moment and then reads correctly: the value the
    field takes when it is absent is exactly what `skip_serializing_if` says it
    was.

    The test that missed it was validating `doctor`'s envelope already — with
    no environments configured, so the type in question was never serialized.
    Covering a command is not covering its payload's nested types, and the
    envelope validation now runs `doctor` against a real environment, where the
    two fields are absent because the news is good.

<a id="decision-224"></a>

224. **The published envelope pins its own version, as it pins the command.**
    Each branch already fixed `command` with a `const`, because that is the
    field `oneOf` selects on. `schema_version` was left as a plain integer, so
    this document — the one a consumer written against envelope version 1
    validates with — accepted an envelope from a later version whose extra
    fields it does not describe.

    SPEC §9.8 says what that field is for: it is the version of the envelope
    alone, and it "moves when a consumer would have to change". A schema that
    accepts any value there says "fine" about the one case the field exists to
    refuse, which is this repository's oldest mistake in a new place — a reader
    told "not readable" and answering "nothing wrong".

    Pinned, a version bump is a validation error the consumer already has a
    branch for, at the moment the envelope arrives, rather than a payload
    silently read as something it is not. It also makes the bump a deliberate
    act on this side: `output::SCHEMA_VERSION` moves, the checked-in schema
    moves with it, and the drift test refuses to let one move without the other.

<a id="decision-435"></a>

435. **`state list`'s timeline reads five new ledger columns, never
    `state_json`, in the steady state (issue #103).** Measured before
    choosing: a 300-table snapshot recorded 50 times and read back through
    `SELECT_TIMELINE` transferred and deserialized ~28.4 MB of `state_json` on
    SQL Server (8.39s) and ~9.9 MB on PostgreSQL (609ms), against ~1.1 KB and
    ~700 B respectively for the columns the command actually prints — the
    issue's claim was reasoned, not measured, and the engines confirmed it on
    both dialects, not only the one the issue named.

    Two shapes were on the table, and the choice was architectural, not about
    which measured faster — nothing here measures a `JSON_VALUE`/`OPENJSON` or
    `jsonb` projection, so no performance claim is made against it:

    - **Columns** (chosen): `state_version`, `tables_count`, `modules_count`,
      `staged_completed`, `staged_total` beside `state_json`, written from the
      snapshot at `record` time. This is the third time `__pbps_state` makes
      this move — `kind`/`git_sha`/`plan_checksum`/`operator`/`reason` already
      exist so a reader can filter and count without parsing JSON (SPEC §8.1)
      — and it keeps that principle rather than making an exception to it.
    - **SQL projection** (not chosen): `JSON_VALUE`/`OPENJSON` on SQL Server,
      `jsonb` operators on PostgreSQL. Smaller PR, no migration — and it
      contradicts the reason the five existing columns exist, once per
      dialect, forever: every future field `StateSnapshot` gains at a new path
      would need two hand-written query fragments kept in step with Rust
      structs the compiler does not check, the exact coupling the projected
      columns were introduced to avoid.

    **The JSON snapshot format is untouched.** `CURRENT_VERSION` and
    `OLDEST_READABLE_VERSION` (`pbps-model::state`) do not move: the five new
    columns are a `__pbps_state` **table** change, not a `StateSnapshot`
    format change, exactly as the five existing projected columns never bumped
    it either. `record` writes them from the snapshot; nothing in `state_json`
    itself changed shape.

    **Nullable, because a row recorded before this shipped must still be
    listed** (DECISIONS 218: an entry this build cannot read is carried, not
    thrown — extended here to a row it has not yet been given the columns
    for). `timeline` reads the projected columns first; a row whose
    `state_version` is NULL is legacy, and its id is asked for in a second
    query, `SELECT id, state_json FROM __pbps_state WHERE id IN (...)`, sent
    only for those ids. A ledger with no legacy rows never sends it. Migration
    is idempotent and runs from `ensure_tables`: **measured**, SQL Server's
    `IF COL_LENGTH(...) IS NULL BEGIN ALTER TABLE ... END` evaluates the guard
    before it would need `ALTER` to act on the `THEN`, so a login holding only
    `SELECT`/`INSERT`/`DELETE` never needs it once migrated; PostgreSQL's
    `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` is refused by ownership
    *before* the `IF NOT EXISTS` is looked at, even when every column already
    exists, so `pbps_pg::state::migrate_timeline_columns` asks a world-readable
    catalog probe first and sends the `ALTER` only when it says the columns
    are actually missing.

    **`timeline` asks the ledger's shape before it asks for a row.** A
    round-1 review finding caught what the first version of this entry's own
    promise did not yet keep: the columns above are nullable so a row
    recorded before they existed is still listed, but `state list` is a read
    and must not require `ALTER` or ownership to run — it cannot call
    `ensure_tables` to make an *unmigrated table's* columns appear, only a
    *row's* NULL columns were ever handled. `SELECT_TIMELINE` naming five
    columns that do not exist at all failed the whole call outright, not
    per-row, on exactly the ledger a real upgrade meets first — the
    development tests only ever exercised a table `ensure_tables` had
    already touched, which is why the gap shipped. The fix treats "the
    columns are not there yet" as a fourth known ledger shape rather than an
    error: `timeline` probes for the columns first (`COL_LENGTH` on SQL
    Server, the same world-readable catalog probe `migrate_timeline_columns`
    already uses on PostgreSQL — neither needs anything wider than an
    ordinary read), and an unmigrated table sends the pre-#103 six-column
    query instead, with every row routed through the same legacy fallback a
    partly-migrated ledger's NULL rows already use. One code path serves
    both shapes; nothing about the fallback itself changed.

    **The version gate is unavoidable, not merely present.** A second
    round-1 finding: the projected path parsed `state_version` into a
    `TimelineState` directly, so a row a newer pbps wrote — its columns
    populated like any other row's — was presented as ordinary data with
    counts, where the JSON fallback's `StateSnapshot::read_json` would have
    refused the same version as `Unreadable::UnsupportedVersion`. The two
    paths had come to disagree about what "readable" means. Fixed by giving
    `pbps_db::ledger::TimelineState` one constructor for the projected path,
    `from_projected`, that calls the new `pbps_model::check_readable_version`
    before it will build the value at all — checking and constructing are
    the same call, so a third path built later cannot skip the check either
    (the shape AGENTS.md asks for: prefer a failure unrepresentable over a
    branch that tests for it).

    **The version is checked before the other columns are decoded, not only
    before `TimelineState` is built from them.** A round-3 finding on the
    previous paragraph's own fix: `from_projected`'s check runs when it is
    called, but `projected_row` on both dialects computed `tables`/`modules`
    — decoding `tables_count`/`modules_count` out of the row — *before*
    calling it, as ordinary function-call arguments. A row a newer pbps
    wrote populating those two columns in a shape this build cannot parse
    (bound by construction from a version this build wrote, per
    `as_count`'s own doc comment — never true of *this* build's own rows,
    but nothing here controls what a newer one writes) failed the whole
    `timeline()` call on that decode, before `from_projected` was ever
    reached to refuse the row by version instead. The JSON fallback never
    had this gap: `read_json` checks the version before it touches the rest
    of the document at all, and it is exactly that ordering the projected
    path did not yet keep. Fixed by checking
    `pbps_model::check_readable_version` first, immediately after
    `state_version` is read and before `tables_count`/`modules_count` are
    touched at all; `from_projected` remains the only constructor and is
    still called to build the value once the check has already passed, so
    the two checks cannot disagree — the second one is guaranteed to
    succeed, which is what its `.expect` documents rather than skips.

    **`(None, None)` is the only pair that means "not staged."** A round-4
    finding: `staged_completed`/`staged_total` are two independently
    nullable columns, but `record` only ever writes both together or
    neither — the JSON side's `Option<StagedProgress>` makes the pair one
    field, not two, so it cannot come apart on any path this crate writes.
    `projected_row`'s catch-all arm read `(Some, None)` and `(None, Some)`
    the same as a genuine `(None, None)`, silently reinterpreting a row
    that had gone out of step as one that was simply never mid-deployment —
    exactly the "unreadable read as nothing there" AGENTS.md names, and on
    columns this PR itself introduces. `as_count`'s own doc comment states
    the rule this row breaks: a value `record` would never produce means
    "the row and this reader have gone out of step," reported rather than
    silently reinterpreted. A half-populated pair is that same rule, one
    column over — fixed by refusing it as `Unreadable::Malformed`, naming
    which column is present and which is NULL, rather than folding it into
    the `_ => None` arm.

    **The legacy fallback asks in pieces, not one statement, on both
    dialects.** A third: `select_legacy_state_json` bound one parameter per
    legacy id with no ceiling, and until a deployer runs a deployment after
    upgrading, every row on a ledger is legacy — the normal case immediately
    after this ships, not an exotic one. SQL Server refuses more than 2,098
    user parameters in one bound statement (`pbps_mssql::doctor::MAX_PARAMETERS`,
    measured and already shared with the object-permission queries there);
    `timeline`'s fallback there now asks in chunks of that size. A round-2
    review finding caught that the fix stopped one dialect short:
    PostgreSQL's extended protocol writes a Bind message's parameter count as
    an `int16`, so it has the same shape of ceiling, just a different number
    — **measured** against the pinned image (`pbps_pg::state::MAX_PARAMETERS`,
    the live test `a_query_may_bind_the_most_parameters_the_extended_protocol_represents`):
    65,535 bound parameters succeed, 65,536 are refused with "error parsing
    response from server". `pbps_pg::state::timeline`'s fallback now chunks
    by that measured number too, the same shape as SQL Server's fix, one
    dialect apart, with its own constant rather than the other engine's
    2,098 — a different protocol, not a reused number.

    **A fourth `Unreadable` case, not a third `Malformed`.** A row a fallback
    query cannot read because a principal was denied `state_json` is neither a
    build too old to read the format (`UnsupportedVersion`) nor a damaged row
    (`Malformed`) — reporting a permission gap as either sends an operator to
    upgrade a build that is fine or hunt for damage that is not there. A third
    `pbps_model::Unreadable::Denied(String)` variant, threaded through
    `pbps_db::TimelineEntry` (now typed `Result<TimelineState, Unreadable>`,
    not `Result<StateSnapshot, Unreadable>` — a timeline row reading only the
    projected columns never has the schema or the identity mapping in hand,
    and a type claiming to be a `StateSnapshot` while routinely holding neither
    would be lying about what it carries) and into `state list`'s own
    `Unreadable` schema as `denied`.

    **`doctor` does not yet ask for the right this migration needs** — SQL
    Server's `ALTER` on an existing ledger, PostgreSQL's ownership of it —
    because today's `Needed::Ledger` demands only `SELECT`/`INSERT`/`DELETE`
    once the ledger exists, and `Needed::LedgerCreation` covers `ALTER` only
    while it does not. A login or role holding exactly what `doctor` reports
    ready meets a driver error on its first `ensure_tables` after upgrading;
    both dialects now rewrite that failure to name the ledger, the five
    columns and the right needed rather than pass through the driver's own
    words (SQL Server's Msg 1088, PostgreSQL's "must be owner"). Closing the
    `doctor` gap itself is out of this PR's scope and tracked separately.

    Pinned by live tests on both engines: a `__pbps_state` created with the
    pre-#103 DDL is migrated in place by `ensure_tables`, a legacy row keeps
    listing its counts through the JSON fallback, and a row recorded
    afterwards reads them from the columns; the sharp test the issue names —
    a fully-migrated ledger whose `state_json` the reader's principal may not
    see still answers `state list` — on both dialects; `timeline` answers a
    ledger nobody has ever migrated, through the same path `engine::timeline`
    calls, never through `ensure_tables`; and a row a newer pbps wrote is
    `Unreadable::UnsupportedVersion` on the projected path exactly as an
    older pbps's JSON fallback already refuses it, beside the positive case
    that a supported version is not refused. Both dialects also pin the
    legacy fallback succeeding past their own measured parameter ceiling —
    2,098 on SQL Server, 65,535 on PostgreSQL — each with a revert-and-watch-
    fail cycle confirming the failure the fix removes names the protocol
    limit, not something incidental. Both dialects also pin a row carrying
    both an unsupported version and a count this build cannot parse being
    refused by its version — the whole `timeline()` call still succeeding —
    with its own revert-and-watch-fail cycle confirming the unfixed ordering
    fails the whole call on the count instead. Both dialects also pin a
    hand-written row with `staged_completed` set and `staged_total` NULL
    being refused as `Malformed`, the whole call still succeeding, with its
    own revert-and-watch-fail cycle confirming the unfixed catch-all reads
    it as "not staged" instead. A unit test pins a denied row rendering as
    denied, never as malformed and never with tables/modules silently
    reading zero, and another pins the projected path's version gate
    directly against `pbps_model::CURRENT_VERSION`/`OLDEST_READABLE_VERSION`.

<a id="decision-465"></a>

465. **A published schema version identifies a fixed set of documents (issue #191).**
     `integration::SCHEMA_VERSION` moves once per merged change to the JSON
     content of any published schema kind, not per tool release or individual
     commit. All three kinds move together. Whitespace, object-key order and
     the top-level tool-version stamp are excluded; everything else, including
     editor descriptions, is part of this conservative content contract. Exact
     parsed-document equality is stronger than equal validation behavior and
     avoids pretending to decide arbitrary JSON Schema equivalence.

     Version 10 covers the accumulated changes after 9, including envelope
     version pinning, doctor optional fields, timeline unreadability variants,
     the positive state-list limit and later payload additions. The envelope's
     own wire version, saved plans, state snapshots and declaration formats do
     not move with this schema-set stamp. Existing version-9 publications have
     different contents; assigning a new number now cannot repair a cached
     legacy copy. Regenerate it from the updated binary or retain the exact
     document rather than relying on the old number alone.

     Tests archive the complete published set under its version, separately
     from the mutable top-level copies. The current generator must match the
     archive selected by its version, ignoring only the tool stamp. A future
     schema change therefore needs a new number and a new complete archive;
     old archives stay unchanged. Regenerating today's checked-in documents
     alone cannot make the version guard pass. An actual version-9 publication
     from `9a01bfa` pins the old acceptance of `state list` with `limit: 0`;
     the current document refuses it under a distinct schema-set version,
     while both accept a positive limit and reject a future envelope version.

<a id="decision-481"></a>

481. **Explain's optional environment must use the saved plan's dialect.**
     SPEC 9.6 makes the file authoritative even beside another project's
     configuration. A pre-resolved `--env` target therefore has its driver
     compared with the saved plan's before any connection (#309). A mismatch
     becomes diagnostic context with no connection left to query, retaining the
     complete file explanation and exit 0. It uses the existing `unconfigured`
     target state and `target.not-ready` finding; no output envelope changes.

     A known mismatch also removes that environment's name from the approval
     command, leaving the existing environment placeholder. An environment
     whose configuration could not be read is still distinct: its engine is
     unknown, so its previous guidance remains. Matching environments and bare
     `--db` targets keep their behavior; the latter gets its driver from the
     plan regardless of any local project.

     The saved dialect string is deserialized as the shared `DialectName` and
     passed to `dialect_for` and `db::driver_for` (#310, 417). There is no second
     string-to-engine factory in `explain`. Unknown names retain their existing
     explanation error and `plan.unsupported-dialect` envelope. A future
     configured name requires a deliberate answer in both exhaustive selectors.

<a id="decision-485"></a>

485. **Connected JSON refusals retain the finding that answered the question.**
    A policy error and unresolved rename/drop intent already have typed findings;
    neither is an operational failure to be flattened into `plan.failed`.
    Connected planning emits those fields through the ordinary findings envelope
    before either artifact can be written. Its caller passes the reported
    `Found` result through unchanged, so the operational-error wrapper cannot
    emit a second envelope or misclassify the refusal. Genuine connection/read
    errors still use `unanswerable` and exit 1; finding refusals use exit 2.

    JSON policy and edition warnings live only in the envelope. Human output
    keeps its existing prose and refusal behavior. The connected count/check
    summary and help/SPEC contract follow decision 429; they do not replace the
    checksum-pinned saved plan, change policy or identity decisions, or grant
    any new execution authority. CLI regressions compare human and JSON paths,
    warning/error and unresolved/resolved cases, refused artifact preservation,
    operational errors, and real SQL Server edition warnings (#338–#341).
