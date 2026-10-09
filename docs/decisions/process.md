# Repository process

CI, the merge queue, and how tests are arranged. Part of the [decision
record](../DECISIONS.md), which says how to add an entry here.

<a id="decision-49"></a>

49. **A guard built twice is a guard that fires early.** `dev::Container::start`
    built its cleanup guard, then *shadowed* it with a second one holding the
    same container id. A shadowed binding is not dropped early — it lives to the
    end of the function — so the first guard's `Drop` ran `docker rm -f` on the
    container just returned, and `plan --dev docker://...` failed with
    "connection refused" from the day the feature was written. It survived
    because **every `--dev` test passes a connection string**: the docker path
    had no coverage at all. `scripts/live-tests.sh` now sets
    `PBPS_TEST_DEV_IMAGE` to cover it; CI's live job deliberately does not, as a
    second SQL Server on that runner is a CI decision of its own.

<a id="decision-198"></a>

198. **A live test arranges its state over its own connection, in its own
    database.** Four tests in `flow.rs` needed state the tool will not produce —
    a lock held by somebody else, a table with an `IDENTITY` no `ALTER` can add
    — and reached for it with `docker exec pbps-test-mssql … sqlcmd`. Each
    treated a failure to reach the container as a reason to `return`, so on a
    host whose `docker` cannot see it (this suite also runs under podman) they
    reported a pass having executed nothing. Measured: with `docker` replaced by
    a program that exits 1, all four were green; the bug one of them was written
    to catch went unmeasured for as long as that was true.

    They now run their statements through `pbps_db::Conn` on the connection the
    test already has, which the file does in a dozen other places, and a failure
    is a panic: setup is a precondition, and a test that cannot arrange its
    state has not passed. Against an unreachable server the four now fail with
    `cannot reach the server under test`.

    Making them run exposed the second half. All four worked in whatever
    database `PBPS_TEST_DB` names, shared with every other test in the file, and
    the ledger entries and tables they leave behind made *other* tests fail —
    which ones depending on the order they ran in. So each takes a database of
    its own, as every other state-writing live test here already does. Teardown
    stays tolerant: after the assertions, a failed `DROP` hides nothing, while a
    panic there would replace the failure the test actually found.

<a id="decision-199"></a>

199. **One helper owns a per-test database, and a guard drops it.** 198 gave
    four live tests a database of their own by copying what eleven others did
    by hand, and the copy inherited both defects the hand-rolled shape carries.

    The connection string was built as `format!("{server};Database={name}")`
    with `PBPS_TEST_DB` verbatim. An ADO.NET string is a list of `key=value;`,
    so a trailing separator is legal and makes `;;`, which tiberius refuses:
    "Key must not be empty". Measured — every one of those tests creates its
    database and then cannot connect to it. `with_key` trims the trailing
    separator and any whitespace, and is the only place a key is appended.

    The drop was each test's last statement, so an assertion that panicked
    skipped it, and the name carries the pid, so the next run created a
    differently named database rather than reclaiming the old one. Counted on
    the shared container: 58 left behind. `OwnDatabase` drops in `Drop`, which
    runs while unwinding. The teardown stays tolerant for a second reason
    there: a panic during unwinding aborts the process and would take the rest
    of the suite with it.

    Both halves are pinned by tests that fail when reverted — the string one
    without a server, since the defect is in the string.

<a id="decision-206"></a>

206. **CI is a gate started by hand, not feedback on every push.** The
    workflow no longer has a `pull_request` trigger. A review round takes
    several commits, and running the full matrix on each of them spent the
    private repository's minutes on states nobody would merge. Instead, the
    checks CI runs are run locally before every push (CLAUDE.md), and CI is
    started once, on the commit about to be merged, with
    `gh workflow run ci.yml --ref <branch>`. A repository ruleset
    (`ci-before-merge`, outside the repo — hence this entry) requires the
    `ci-gate` commit status to be green on the PR head, so a push after the
    run clears it and the run has to be repeated. The ruleset is strict: the
    branch must contain the latest `master` before the merge, so the tree CI
    ran on is the tree the merge commit holds, and this workflow runs nothing
    on `master` after a merge. (The dependency audit is a separate workflow and
    keeps its own `master` trigger: its subject is the advisory database, which
    moves without the tree.) The price is that a PR waiting while `master`
    moves has to rebase and run CI again; with one issue in flight at a time
    that is rare, and the alternative — a post-merge run on `master` as a
    safety net — doubled the minutes of every merge to cover it.

    **The first version of this did not work, and looked as though it did.**
    It required the five job names directly, on the theory that a check run
    on the head commit is a check run. It is not: a check run reaches a pull
    request through the *check suite* that holds it, and a suite is
    associated with the PR only when the run's event is one of
    `pull_request`, `pull_request_target`, `push` or `merge_group`. A
    `workflow_dispatch` suite is associated with nothing. So the Actions tab
    showed five green jobs on the PR head while the PR's own checks list
    showed none and the merge box sat on "Expected — Waiting for status to be
    reported" forever. The gate this entry describes had locked out the very
    pull request that introduced it.

    The repair keeps the trigger and changes what is reported: a `gate` job
    needing all five writes one `ci-gate` **commit status** to the dispatched
    SHA. A commit status has no suite — it is addressed to a commit and read
    off that commit — so the association problem cannot arise. It is also
    what makes the expiry exact rather than incidental: a status belongs to
    one SHA and is never inherited, so the next push starts with no `ci-gate`
    and the gate is shut without anyone clearing anything.

    Not the Checks API, which is the more commonly suggested workaround for
    the same limitation: a check run created that way still lands in a suite,
    and a suite is the mechanism that just failed here. Not a
    `pull_request` trigger with a label or `ready_for_review` guard either —
    a required job skipped by `if:` reports `skipped`, which GitHub counts as
    success, so the guard that saves the minutes is also the guard that opens
    the gate. Not a merge queue: `merge_group` would be associated correctly,
    but merge queues need an organisation-owned or public repository and this
    one is neither.

<a id="decision-374"></a>

374. **`maintain` is gated on the connected server, and the live suite
    therefore runs two PostgreSQLs.** ADR-0010's amendment set the rule: the
    model holds no server version, so `validate_role` cannot ask. The reading
    is `pbps_pg::roles::unsupported_permissions`, called on the connected path
    the way `plan --db` gates on SQL Server's edition.

    "This engine refuses the word" and "this engine takes it" are two
    different servers, and no single one can show both, so
    `scripts/live-tests-pg.sh` and the `live-pg` CI job start a pinned
    PostgreSQL 16 beside the pinned 18. Measured on it: `server_version_num`
    160015, `GRANT MAINTAIN ON t TO r` is `unrecognized privilege type
    "maintain"` (SQLSTATE 42601, the parser stopping at the word), and the
    owner's default relation ACL has no `m`. A bump of that pin must stay
    below 17, or the test asserting the refusal passes for no reason.

<a id="decision-375"></a>

375. **A live permission test that runs as a superuser measures nothing.** The
    SQL Server suite learned it with `sa`, which holds `CONTROL` and
    short-circuits the whole permission list — how three permission bugs
    survived that suite's first run. On PostgreSQL it is worse: a superuser
    does not consult an ACL at all. So every test in the roles section of
    `pbps-pg/tests/live.rs` acts through a `LOGIN` role created for it, and
    the pull is exercised as that role too — a read that needs a superuser is
    a read that fails in the one environment that matters. `pg_roles` rather
    than `pg_authid` for the same reason: the second holds the password hashes
    and is superuser-only.

    Those tests assert on the **SQLSTATE**, not on the message.
    `tokio_postgres::Error` renders as `db error` and keeps the server's text
    in a source the seam deliberately does not carry (ADR-0014 §1), so a test
    matching on prose would pass on any failure at all — including the wrong
    one.

<a id="decision-437"></a>

437. **Retain settled spikes as evidence outside the production workspace.**
    `spikes/yaml-span`, `spikes/pg-driver` and `spikes/pg-measurements` stay in
    the tree together (#302). The ADRs preserve the conclusions, but the
    experiments also preserve the inputs, method and original observations
    needed to challenge or reproduce them. Recovering those pieces from old
    commits is possible; keeping the small evidence directories beside their
    live ADR links makes that audit direct.

    Retention does not promote the spikes to product code or CI gates. The
    root workspace explicitly excludes both Rust experiments; `pg-driver`
    keeps its independent workspace and `pg-measurements` is not a Cargo
    package. Their historical dependencies are not added to the production
    workspace. Current behavior is verified by the production crates' tests
    and live suites. Re-running old measurements is a deliberate investigation,
    with changed observations and versions recorded rather than silently
    replacing the evidence. This supersedes the deletion promises in the
    spike manifests and ADR-0001/ADR-0014; `spikes/README.md` states the common
    policy for all three.

<a id="decision-501"></a>

501. **CI is triggered by the pull request again; the gate is a check run, not
    a commit status.** This supersedes 206, whose every premise was a property
    of the private repository. That entry made CI a hand-started
    `workflow_dispatch` workflow because "running the full matrix on each
    [commit] spent the private repository's minutes on states nobody would
    merge", and made its `gate` job report a **commit status** because a
    `workflow_dispatch` check suite is associated with no pull request. Since
    the move to the `pongbiphang` organisation the repository is public,
    standard runners bill nothing, and the trigger that forced the workaround
    is gone. `ci.yml` now runs on `pull_request`, on `push` to `master`, on
    `merge_group`, and still on `workflow_dispatch` for running the matrix on a
    branch that has no pull request; `gate` is an ordinary job named `ci-gate`,
    and its check run reaches the pull request the ordinary way.

    **Dispatch cannot retry a pull request's CI**, and saying it could was the
    first error this entry's own pull request made. `gh workflow run` raises a
    `workflow_dispatch` event, and 206's finding applies to it unchanged: the
    suite belongs to no pull request, so the run goes green in the Actions tab
    while the required check stays unsatisfied. `gh run rerun <run-id>` is the
    retry, because it is another attempt at the *same* run and keeps its event
    and its association. Measured on this pull request rather than assumed:
    after `gh run rerun`, run 35114661123 reported `event=pull_request`,
    `attempt=2`, and the pull request's own checks list showed it.

    Measured against fifteen comparable projects rather than chosen: every one
    of rust-analyzer, cargo, diesel, bevy, rust, clap, tokio, sqlx, ripgrep,
    atlas, deno, uv, polars, sea-orm and paradedb triggers on `pull_request`
    together with `push` to the default branch. None is dispatch-only.

    **One required check rather than seven job names, and it is not a style
    choice.** A ruleset lives outside the repository: it has no history, no
    review, and nothing ties it to a job rename. Requiring the job names makes
    a rename close the gate forever and a new job open it silently. More
    decisively, GitHub counts a **skipped** required check as a passing one, so
    requiring job names directly cannot survive any future conditional job. The
    aggregating job can, because it inspects `needs.*.result` itself and treats
    `skipped` as the failure it is. The same reasoning is visible in
    rust-analyzer's `conclusion` job and clap's `ci` job, both of which carry
    the warning in a comment. The cost is that the `needs` list must name every
    job; the gate's comment says so, and a check that asserts it is issue #637.

    **The gate runs on `always()`, and the first version of this got that
    wrong too.** It carried `if: ${{ !cancelled() }}` over from the
    commit-status design without re-deriving it. Cancel the run and
    `!cancelled()` skips the gate; a skipped required check passes; the merge
    box opens over a matrix that never ran. The commit-status version had no
    such hole, because it wrote nothing on cancellation and an absent required
    status blocks. The guard's own reason belonged to that version as well: a
    status is addressed to a SHA, so a cancelled run writing `failure` late
    could overwrite the run that superseded it — whereas check runs never
    overwrite one another, each belonging to its own run. The race was gone and
    the guard against it was still there, which is the shape AGENTS.md calls a
    filter nobody re-reads. `always()` buys the opposite error: a cancelled run
    reports a failing gate, which a re-run clears. Wrongly shut is recoverable;
    wrongly open is not.

    Measured on the pull request that made the change, not argued: run
    35117078034 on the commit that introduced `always()` was cancelled by
    `cancel-in-progress` when the next commit was pushed, and the gate reported

        run conclusion = cancelled
          ci-gate: failure
        GET /commits/<sha>/check-runs -> ci-gate status=completed conclusion=failure

    A real cancellation produced a real `ci-gate` check run, and it is
    `failure`. Under `!cancelled()` that check run would not have existed as a
    conclusion at all — the job would have been skipped, and a skipped required
    check passes.

    **`paths` and `paths-ignore` stay off the trigger.** A required check that
    never reports leaves a pull request blocked with nothing that can clear it —
    the same class of stall 206 records, reached from the other side. Skipping
    work for documentation-only changes is therefore not attempted here; the
    projects above that do it either keep those checks unrequired or filter at
    the job level behind exactly such an aggregator.

    What does not change: the `ci-before-merge` ruleset still requires the one
    `ci-gate` context, so this is a workflow change and not a ruleset change,
    and the fan-in of every job onto `quick` stays. That fan-in was justified
    by the minutes and is kept for fail-fast alone — nothing expensive should
    start for a commit that does not compile.

    The strict up-to-date policy 206 relies on is left in place here. It
    serialises merges when several pull requests are in flight, which is the
    subject of issue #636: a merge queue is the automated form of the same
    guarantee, and 206 rejected one only because "merge queues need an
    organisation-owned or public repository and this one is neither". It is now
    both.

<a id="decision-502"></a>

502. **A merge queue on `master`, and the strict up-to-date policy off.** With
    several pull requests in flight the strict policy serialised every merge:
    merging one left the rest `BEHIND`, each had to rebase and re-run the full
    matrix, and whoever merged next invalidated the others again. That cost is
    inherent to the policy rather than to how long CI takes, so making CI
    faster could not have removed it.

    The queue is the automated form of the same guarantee, not a weaker one.
    GitHub states the relationship plainly: it "provides the same benefits as
    the **Require branches to be up to date before merging** branch
    protection, but does not require a pull request author to update their
    pull request branch". The property 206 wanted — that the tree CI ran on is
    the tree the merge commit holds — is what the queue enforces, by building
    `master` plus everything queued ahead plus this pull request and running
    `ci.yml` on the `merge_group` event against exactly that.

    206 rejected a queue for a reason that expired: "merge queues need an
    organisation-owned or public repository and this one is neither". Since the
    move to `pongbiphang` it is both. 501 added the `merge_group` trigger so
    that turning the queue on would be a ruleset change alone; a queue whose
    required checks do not run on the merge group never advances.

    **Strict is turned off, and the two are not mutually exclusive.** GitHub
    accepts both at once. Leaving strict on would nevertheless have made the
    queue pointless: the author would still have to update the branch before it
    could be queued, which is the exact work the queue exists to remove. So
    this is a deliberate pairing rather than a constraint.

    **What it costs, stated rather than discovered later.** A pull request must
    be green on its *own* head before it can be queued, so a merge costs **two
    pre-merge runs** of `ci.yml` — one on the pull request, one on the merge
    group — and 501's `push` trigger adds a third on `master` afterwards, which
    gates nothing. That is the cheap half of the trade: what disappears is not a
    run but the *repetition* of runs caused by somebody else merging first,
    which had no bound. A conflict is still the author's to resolve; the queue
    ejects a pull request it cannot merge rather than guessing.

    **An ejection is read, not re-rolled.** A merge-group failure is not
    automatically an interaction with what merged ahead: this repository's CI
    has a documented resource-shaped engine startup crash, and issue #638 was
    two genuinely flaky tests. Nor is it automatically a flake — that reading is
    how a real interaction merges on the second attempt. The rule is therefore
    procedural rather than a verdict: read the failing job, name which it was,
    and only then fix or re-queue. The `push` run on `master` afterwards is read
    the same way and for a stronger reason: the merge group passed that exact
    tree minutes earlier, so a red one is a flake until the failing job says
    otherwise.

    **A stacked pull request needs a verified base change after its upstream
    merge.** A PR still based on a merged feature branch does not enter the
    `master` queue. DEC-1228.1 replaces the assumption that deleting that branch
    reliably retargets every dependent: the primary agent explicitly changes
    and verifies dependent bases before deleting the merged remote head, then
    verifies them again, including DEC-1458.1's operation-window/all-state check
    for newly discovered dependents. Deletion remains a separate step after the
    merge
    actually lands. With a queue required, `gh pr merge --delete-branch` still
    errors instead of enqueueing, because early head deletion can close the PR
    and remove it from the queue.

    Measured against the field rather than chosen. Of the fifteen projects
    surveyed for 501, the four running a merge queue — rust-analyzer, cargo,
    diesel, bevy — are precisely the ones with many concurrent pull requests
    and long CI, and rust-lang/rust ran bors, the same idea, for years before
    GitHub shipped one. The projects without one (atlas, tokio, sqlx, ripgrep,
    clap) keep the strict policy and pay the serialisation. This repository has
    the first shape, not the second.

    Settings: `merge_method: MERGE`, because the merge commit is what this
    repository keeps (AGENTS.md). `grouping_strategy: ALLGREEN`, so every pull
    request's own merge commit must pass, not only the head of the group —
    the weaker setting would let a pull request merge on somebody else's green.
    `max_entries_to_build: 5`, which is what makes the validation parallel and
    therefore what actually removes the serialisation; `max_entries_to_merge: 5`
    to match it, since a group cannot merge more than it built;
    `min_entries_to_merge: 1` with no wait, so a ready entry merges instead of
    waiting to be batched. `check_response_timeout_minutes: 60`, because the
    engine-backed jobs take tens of minutes and a timeout shorter than the
    matrix ejects healthy entries for not having answered yet. These values are
    recorded here because the ruleset lives outside the repository: this entry
    is the only reproducible record of them.

    **The ruleset is not sufficient on its own: `allow_auto_merge` has to be
    enabled on the repository too.** Adding a pull request to the queue goes
    through the auto-merge API, so with the ruleset in place and that repository
    setting still off, enqueueing fails outright — measured here as `gh pr merge
    --merge` answering `Auto merge is not allowed for this repository
    (enablePullRequestAutoMerge)`. The queue is therefore two switches rather
    than one, and the second is easy to miss because the ruleset reads as
    complete without it.

    The rest of the ruleset is unchanged and worth naming, because one of its
    values is routinely misread: alongside the single required context
    `ci-gate`, it sets `required_approving_review_count: 0` and
    `required_review_thread_resolution: true`. A pull request that is `BLOCKED`
    with green CI is therefore never waiting for an approval — there is none to
    wait for. It is waiting for an unresolved review thread, and looking for a
    reviewer instead has cost time here more than once.

<a id="dec-671-1"></a>

**DEC-671.1. The record is split by topic, and a new entry is named after its
issue rather than given the next number.** While the record was one file, every
branch that added an entry appended at the same end of it, so any two such
branches conflicted by construction; and because the number each took was "the
next one", resolving the conflict meant renumbering the entry and every
citation of it. #640's entry went 499 → 501 → … → 509 in one session. A conflicted
pull request also gets no merge ref, so its CI does not fail but never starts.

Splitting the file alone would have made this worse, not better. Two branches
taking the same next number in two topic files merge with no textual conflict,
and the duplicate lands unseen. So the split and the naming rule are one
change: `DEC-<issue>.<k>` uses a number GitHub has already allocated to exactly
one branch, and the identifier is final from the moment it is written. The
existing 539 entries kept their numbers, because about 1,600 citations name
them, and that sequence is closed.

`scripts/check-decisions.py` enforces what the naming makes likely but cannot
make certain: that no identifier is claimed twice, that no entry takes a new
number in the closed sequence, and that every `DECISIONS <n>` and
`DEC-<issue>.<k>` citation resolves. The issue suggested asserting that numbers
are contiguous. That no longer fits: issue-numbered identifiers have gaps by
design, and the closed sequence already has four (523–526, numbers branches
reserved and abandoned, which ADR-0015 and ADR-0017 still mention).

Two branches that add entries to the same topic file still conflict, at its
end. That conflict is resolved by keeping both, with no renumbering. A
`merge=union` attribute would resolve it automatically, but it would also keep
both sides of two concurrent edits to one existing entry as a silently spliced
paragraph. That is the silent failure this change exists to remove, so the
attribute is not set.

<a id="dec-1017-1"></a>

**DEC-1017.1. A pull request's CI runs once, when the head it will merge has
qualified in review, not on every push.** Every push to a pull request used to
run the whole matrix, and a review round pushes several times: every run but
the one on the qualifying head was discarded, and the matrix only grows. The
merge group runs the matrix again on the exact tree that merges, so the
per-push runs decided nothing the merge group does not decide again.

Dropping the `pull_request` trigger was not an option. The merge queue admits a
pull request only once `ci-gate` is green on its own head, and a required check
that never reports blocks it with nothing that can clear it. Nor was a
`pull_request` run that skips the matrix and passes the gate: that reopens the
hole DECISIONS 206 closed, a green `ci-gate` for a matrix that did not run. A
label that starts the run was the other candidate; it leaves the label on the
pull request, where the next push, cancelling nothing it can see, reads as
already approved.

So every push still starts a run, and its first job, `approval`, names the
`ci-approval` environment, whose required reviewer must approve before any job
starts. An unapproved run is pending, so `ci-gate` is pending and the merge box
shut; a rejected one fails `approval`, which skips everything after it and
fails the gate, which treats `skipped` as failure. A newer push cancels the
waiting run through the pull-request concurrency group. The merge group,
`master` and dispatched runs name no environment and start by themselves, so
the queue's own run needs no one.

The cost is where a failure surfaces. Windows, the MSRV build, the resolver
fixtures and the dev rehearsal are not in the local checks, so a failure there
now appears on the approved run rather than on the push that caused it, after
review has qualified the head. That run is a single run on a head that is about
to be queued, which is where a failure is cheapest to act on; the local checks
remain the per-push gate.

<a id="dec-958-1"></a>

**DEC-958.1. The dedicated-server fixture starts SQL Server once more after a
startup core dump, and only then.** Retrying a flaky fixture usually hides the
failure it should expose. The shape here is narrow enough to tell apart. In
every merge-group failure of the `resolver (mssql)` job read for #958, five of
five, the target server core-dumped about two seconds after `docker start`.
No test had connected by then, so no code under test could have caused it.
The fixture also spent three minutes polling an engine that had already
exited, and the 80-line log tail its reporter kept was all dump-collector
noise.

The retry needs every one of these conditions:
- the engine is SQL Server;
- the container has exited;
- its log names a `core.sqlservr` dump.

It happens once, and a second crash fails the run. An engine that is still
running, an exit with no dump, and every PostgreSQL failure fail as before.
Each crash is reported before the container is removed, so the retry leaves
the first occurrence on record. The reporter keeps 300 lines, which is long
enough to reach past the dump report to what the engine printed first.

The cause is still unproven, as docs/PITFALLS.md records for the `live` job.
This makes the crash cost one start instead of a queue ejection, and makes its
next occurrence readable. It is not a fix for the crash itself.

Pinned by `scripts/live_resolver_server_test.py`, which runs in the `lint`
job.

<a id="dec-1208-1"></a>

**DEC-1208.1. CPU-heavy engine fixtures use an optimized test profile with all
test assertions retained.** The ordinary quick suite keeps Cargo's unoptimized
`test` profile. PostgreSQL's private capture regressions and the resolver
fixtures use `live-test`, inheriting `test` with `opt-level = 1`,
`debug-assertions = true` and `overflow-checks = true`. A release build would
remove checks that these fixtures must still exercise; removing catalog
observations or executable-content hashes would remove the facts they prove.
Local and CI runners select the same profile, and native fixtures discover
the resulting binary from Cargo artifacts rather than guessing its directory.

The measured tradeoff is additional compilation for less repeated CPU work,
not fewer test cases or weaker qualification. The representative timings and
reproduction commands are in [CI performance](../CI-PERFORMANCE.md). Engine
versions, test filters, serialization, fixture ownership and both merge gates
remain as before.

<a id="dec-1130-1"></a>

**DEC-1130.1. Ignored tests have checked execution owners discovered from compiled
artifacts.** A source attribute is not evidence that a CI runner selects the
case, and a library-wide `--ignored` would execute private PID-1/root helpers
without their parents. The execution inventory uses Cargo artifacts and libtest
listings for names, targets and platform conditions, then checks the maintained
CI selectors and scoped fixture invocation witnesses. Nested helpers name their
compiled parent and its fixture path. Linux and Windows check the cases they
compile; conditionally ignored cases can be owned by ordinary tests on another
platform when that relationship is explicit. The inventory establishes a
scheduling contract, not dynamic branch coverage or permission to weaken the
fixture's execution assertions. See [test execution](../TEST-EXECUTION.md).

<a id="dec-1370-1"></a>

**DEC-1370.1. Every job waits for compilation only, and each engine suite is
spread over as many runners as it takes to stay level with the rest of the
run.** A run took about 45 minutes. Two thirds of that was one job, `live-pg`,
running suites that must be serial against one server: `flow_pg` for 20
minutes and the private `pbps-pg` regressions for 13. No other job ran half as
long (runs 36841115280, 36830024040, 36817575465). Compilation took one to
three minutes per job and the cache hit in full, so build tuning had little
to offer.

The obvious alternatives each weaken something. Running those suites with
more threads against one server reintroduces the races they were made serial
for (#437). A sampled or nightly subset would drop cases from the gate. A
`paths` filter is refused for the reason DECISIONS 501 gives. So the
suites keep their exact commands, filters and serial schedules, and are cut
across runners instead. Each variant runs a share against its own pinned pair
of servers, and the shares are a filter and its exact `--skip` complement. A
new case lands in one of them whatever it is called. `resolver` is cut the
same way, at the point where each engine's sequence of fixture scripts
halves. Each script already owns and removes its own containers and scratch
root, so none depended on one before it, and each half builds the test binary
for itself.

`scripts/ignored_test_inventory.py` already proved that every ignored case had
an owner whose CI command selects it. Its matrix reading now covers any axis
and conjunctions of equalities. Without `include` or `exclude` every
combination runs, so that reading is exact. A split that leaves a case in no
variant therefore fails `quick`, rather than merging with that case silently
unrun.

The fan-in DECISIONS 501 kept "for fail-fast alone — nothing expensive should
start for a commit that does not compile" stays, but it waits only for that.
Fmt and clippy over `--all-targets` moved to `lint`, which every other job
`needs`. The workspace tests stay in `quick`, which now runs beside the
engine jobs. A unit-test failure no longer skips the engine jobs; they report
their own results beside it, and `ci-gate` still requires every job.

The price is runners: about fifteen jobs at once instead of nine. The
organisation's concurrency limit can queue a few of them when two runs
overlap. Standard runners bill nothing on a public repository.

<a id="dec-1383-1"></a>

**DEC-1383.1. An active With path and its suppressed continuation use separate
binding states.** Merging skipped writes into every active read refused an
ordinary safe shadow, but deleting suppression handling lost reflective aliases
when an exception skipped that shadow. The running path therefore applies
writes in order; snapshots of possible skipped prefixes join only at the exit.
Class observers share module-state capture with their enclosing managers.
Explicit following writes replace the joined state. A directly raised exception
ends that body's statement scan; unknown exceptions retain conservative prefixes.

Superseded for the current Python inventory checker by DEC-1413.1. The
historical evaluator behavior below is retained as provenance.

A successful shadow does not prove an intervening opaque callback harmless.
Narrow native-call and inert-manager facts distinguish the required safe
controls from unproved execution after a reflective alias was erased. Opaque
execution permanently removes that native proof and keeps selector evidence
conservative. The proof is intentionally limited to literal/native arguments,
empty fresh-object callables and trivial async-manager methods, without importing
or executing source. It is not a general Python evaluator or a new witness
adapter. Context-result targets, general manager effects, compound expression
provenance and direct-spelling SelectorEffects precision remain separate issues.
Acceptance requires actual Python plus complete ownership for synchronous
cases, explicitly component-scoped legal async evidence, and independent
counterfactual failures for active transfer, suppression joining and opaque-effect
refusal. See [test execution](../TEST-EXECUTION.md).

<a id="dec-1389-1"></a>

**DEC-1389.1. Namespace continuations carry their scope owner.** An enclosing
manager's retained module bindings cannot describe an erased class-local alias.
Each continuation therefore captures and compares state through its owning
observer; an inherited class adds its own local capture and copies the stack.
Enclosing captures still observe their own state and the shared module, while
class-local state never becomes an enclosing lookup scope. This also prevents a
finished class's captures from refusing an unrelated later enclosing call.
Suppressed exits retain the prefix join from DEC-1383.1.

Superseded for the current Python inventory checker by DEC-1413.1. The
historical evaluator behavior below is retained as provenance.

Treating every class helper as opaque would lose the issue's measured safe
controls. Two bounded AST facts preserve them: a plain zero-argument function
consisting only of `return None`, and a single native class-frame assignment
through `sys._getframe(1).f_locals` under a literal string key. The latter resolves
sys and its reflective builtin or builtins-module RHS from module bindings at
call time, then records the proven local write. A subsequent real shadow may
replace it. Decorators, defaults, annotations, extra statements, nonlocal target
semantics, custom class namespaces, modified frame access and unproved RHS effects
cannot use this proof. A zero-argument lambda returning a fresh empty dictionary
is a proven following shadow; it does not erase exposure from an opaque helper.
This is neither fixture execution nor general source-helper interpretation.

Acceptance requires actual Python and complete ownership validation for class
restoration, nested classes and managers, deleted-local fallback, enclosing-class
effects, shared module suppression and real following shadows, with both selector
orders. Legal async execution qualifies only the separately replayed visitor
component. Independently removing class capture, stack isolation, opaque-effect
refusal, suppressed-prefix joining and the two bounded helper proofs must break
their corresponding properties before the exact source is restored. General
helper effects, context-result targets and direct reflective-spelling precision
remain separate issues. See [test execution](../TEST-EXECUTION.md).

<a id="dec-1299-1"></a>

**DEC-1299.1. Unpacking assignments retain corresponding namespace aliases.**
A tuple or list target does not make its reflective RHS harmless. Assignment-only
immutable snapshots preserve the values of supported literal elements before any
target writes; chained assignments reuse that snapshot and nested/starred targets
bind in Python's left-to-right order. A following proven native binding replaces
the alias, while merely retaining an unused reflective value does not expose the
module namespace.

Superseded for the current Python inventory checker by DEC-1413.1. The
historical evaluator behavior below is retained as provenance.

Unknown shape or iteration, unsupported scalar results, arity failures and
unproved attribute/subscript target protocols cannot establish execution ownership.
They retain conservative exposure instead of empty alias facts. A starred
remainder is a list, not its contained native callable or module, so contained
native facts never grant its calls a native exemption. Recursive unknown elements
also remain unknown when captured by a starred name.

The snapshots are private to one assignment; this does not cache mutable container
structure, execute fixture code or duplicate the general NamedExpr/IfExp/BoolOp
provenance work. Loop targets, context-result targets, class exports and selector
literal extraction retain their separate boundaries. Acceptance requires actual
Python plus complete ownership for callable/module aliases, nested/starred targets,
RHS and target ordering, unused/shadow controls, unknown shapes, suppressed arity
failures and opaque target escapes. Independent counterfactuals pin transfer,
snapshot reuse, target order, nested/starred precision and fail-closed unknown
handling; native-container exemption evidence is explicitly component-scoped.
See [test execution](../TEST-EXECUTION.md).

<a id="dec-1300-1"></a>

**DEC-1300.1. Conditional deletion retains an explicit namespace fallback.**
A skipped deletion keeps its existing shadow; a completed deletion removes that
scope's entry. The latter is absence, not an empty non-reflective value. The
namespace audit joins those states and carries absence as a distinct marker so
later reads use the live module binding or builtin fallback. Copying the fallback
value at deletion time is insufficient: an executed nested class can replace
the module binding before the deleted local is read. A further conditional write
keeps that possibility, while a guaranteed following shadow replaces it.

Superseded for the current Python inventory checker by DEC-1413.1. The
historical evaluator behavior below is retained as provenance.

Class-local deletion uses the class namespace; an explicit class global uses the
module namespace. A readable fallback does not make an absent local deletable.
Known missing targets refuse execution-owner evidence rather than treating their
NameError as a successful deletion. Definite deletion remains a removal, and
merely retaining an unused reflective fallback does not expose the namespace.

The same lookup resolves captured continuation states against their captured
module dictionaries. An opaque callback cannot hide a reflective fallback that
was possible before a following native shadow. This preserves the existing
scope-owned continuation proof without executing helper bodies or fixture code.
Acceptance uses actual Python and complete ownership for module builtin lookup,
class-local/module/global lookup, taken and skipped branches, nested control,
live module updates, subsequent conditional/guaranteed shadows, opaque callback
restoration and missing-target failures. Independent counterfactuals remove the
deletion join, absence lifetime, conditional preservation, scope destination,
continuation lookup and missing-target refusal. General control-flow truth,
compound results, protocol dispatch and unpacking completion retain their
separate issues. See [test execution](../TEST-EXECUTION.md).


<a id="dec-1413-1"></a>

**DEC-1413.1. Python fixture owners share a literal grammar and a syntactic
form, rather than a namespace evaluator.** Extending alias, class protocol,
continuation and cached-value proofs made each new Python feature another
proof obligation. The checker now reads literal selector dependencies and
checks spelling, binding and use contexts. It never executes an owner. This
supersedes DEC-1299.1, DEC-1383.1, DEC-1389.1 and DEC-1300.1 for active checker
behavior; their measurements and historical descriptions remain recorded.

The same grammar supports list/tuple/string data, addition and one synchronous
unfiltered literal-data comprehension. Every protected name has one plain
module assignment and no other binding. A mutable list may be read only in a
direct iteration position, including qualified, from-import and class-pattern
keyword attribute references, preventing mutable aliases without following
effects. String and immutable tuple reads remain ordinary. Source-wide AST
restrictions refuse reflective spellings, reflection-module imports (including
gc), syntactically qualified sys module/frame access, wildcard imports and any
__name__ binding. Identifier reference fields in imports and class patterns
are checked too; ordinary strings are data. Imports, arguments, definitions,
exception/match targets and type parameters are binding fields too. These are
syntactic rules, including in uncalled or annotation scopes; they do not pretend
to reproduce each scope's execution semantics. Unrelated Python expressions
are not evaluated or banned.

Data/call selector owners end with a canonical __main__ guard calling the unique
undecorated module main, directly or through raise SystemExit(main()). Existing
named-function witnesses check helper invocation routes; main need not read
the selector. Witness-only module runners retain their direct execution form.
Neither launch nor server receives a per-file profile or a pinned helper
skeleton. Statement boundaries remain in Python witness tokens, while supported
partial, continued, multiline and nested call fragments keep their meaning.

This is a maintenance lint, not a hostile-code sandbox. ctypes and repository
helper imports remain trusted source boundaries; deliberate indirect writes or
changes to those helpers require maintainer review, not a growing interpreter
inside the checker. Issue #1100 records the analogous distinction for deliberate
UI guardrail evasion; it does not supply a pre-existing policy for this checker.
This decision establishes the checker boundary explicitly.

Retired regressions keep their exact sources, actual-Python controls and named
construct/line dispositions. Safe cases within the form are accepted; safe
cases outside it are refused by the rule they actually violate. Operator
permissiveness cannot make a prohibited import acceptable. Permanent operational
rule-removal tests must admit a complete owner after disabling only their rule;
a second guard's refusal is not evidence. Unsupported literal nodes never use
an execution or evaluator fallback. See [test execution](../TEST-EXECUTION.md).


<a id="dec-1428-1"></a>

**DEC-1428.1. Historical Python programs carry explicit syntax windows and
earn execution receipts on every supported interpreter.** The fixture checker
supports Python 3.11+, but historical inputs include type-alias statements
introduced in 3.12 and named expressions in annotations rejected in 3.14.
Changing those sources to fit one interpreter would lose their provenance;
skipping them would turn missing evidence into successful coverage.

Optional corpus metadata supplies an inclusive minimum, an exclusive upper
boundary and a nonempty reason. Untagged rows keep the documented minimum.
Malformed, empty or contradictory ranges fail. Independent case-to-boundary
assertions prevent a broader qualification from hiding ordinary programs.
The shared harness still invokes the actual interpreter and complete owner for
every row. Eligible programs retain their original runtime and owner oracles.
Ineligible programs must fail with SyntaxError; below a syntax minimum the
owner must refuse parsing. Python 3.14 still parses the annotation AST before
rejecting compilation, so those rows retain their original owner diagnostic.

Only completed runtime and owner checks earn receipts. Independent eligibility
and full-row coverage assertions catch dropped tasks and broad skips. Actual
3.11, 3.12 and 3.14 runs qualify the boundaries; removing each boundary restores
the corresponding regression, while skipping eligible tagged rows fails the
ledger. Existing source-form rule-removal controls remain required. This does
not expand DEC-1413.1 into an effect interpreter or prove the separate module
annotation behavior in #1262. See [test execution](../TEST-EXECUTION.md).


<a id="dec-1228-1"></a>

**DEC-1228.1. Explicitly retarget and verify dependent PRs before deleting an
actually merged parent branch, then verify them again.** The automatic
retargeting assumption in Decision 502 made remote deletion the action that
would establish the downstream base. An observed failure made that ordering
unsafe: deleting #1216's merged head closed #1226 with its old base intact.

[GitHub's branch documentation](https://docs.github.com/en/pull-requests/how-tos/commit-changes/managing-branches-within-your-repository)
describes automatic retargeting after merged-head deletion. The retained
[#1216](https://github.com/pongbiphang/pbps/pull/1216) and
[#1226](https://github.com/pongbiphang/pbps/pull/1226) events instead show
`base_ref_deleted` and `closed` at 2026-09-27T23:37:04Z (31952476780 and
31952477016), followed by parent restoration, reopening and an explicit base
change. The dependent head remained `ed622e0bd1bd6a88b7278333c0ce24050fc85fd2`.
Reopening started [another CI run](https://github.com/pongbiphang/pbps/actions/runs/36359397387)
after [the qualified run](https://github.com/pongbiphang/pbps/actions/runs/36356526170).
These events do not establish a cause or equate every deletion mechanism.

The primary agent first verifies the parent actually merged and begins
DEC-1458.1's window before enumeration, then reads every page of open PRs based
on its head branch. For each dependent, record the head
and expected surviving patch before changing its base to the merged parent's
base, while the parent branch still exists. For an ordinary stack, verify the
merged parent head is an ancestor of the dependent head and use their diff to
identify the downstream work; otherwise inspect the commit provenance rather
than assume a subtraction. After retargeting, compare the complete PR diff to
that expected patch, checking content as well as file names. OPEN state, the
intended base and the unchanged head are separate required checks. Unexpected
parent changes or missing downstream changes are failures, even with the right
base name. Refresh the enumeration immediately before deletion and apply the
same checks to new dependents. A failed or incomplete read cannot authorize
deletion; a successfully verified empty set needs no base changes.

Only then delete the owned remote head. Re-read the recorded PRs by ID and
complete DEC-1458.1's all-state operation-window check afterwards, so a recorded
or late-created closed dependent cannot vanish from the check.
On failure, stop cleanup and report the observed state. The historical recovery
was to restore the exact owned parent ref, reopen the dependent, explicitly
retarget it and verify its state, head and remaining diff before retrying
deletion; a changed or foreign ref must not be overwritten. Remove the local
branch and worktree after successful verification.

This procedure changes bases without rewriting reviewed heads. It does not
waive review or CI gates: refresh current evidence after the base change and
require the existing gates before enqueueing each dependent. A conflict or
confirmed merge-group interaction still follows Decision 502's rebase path;
ordinary cleanup and movement of `master` do not. Validation uses the retained
failure/recovery sequence and a real successfully enumerated empty-dependent
closeout, not throwaway production PRs.

<a id="dec-1262-1"></a>

**DEC-1262.1. Module-annotation controls separate versioned execution from
unconditional owner refusal.** Python 3.14 defers annotation evaluation
([Python's porting guidance](https://docs.python.org/3.14/whatsnew/3.14.html#pep-649-and-pep-749-deferred-evaluation-of-annotations)).
The original `annotation: TESTS; __annotations__["annotation"].clear()`
program clears the selector on earlier versions, but bare-name access raises
NameError on 3.14+. Retain that exact source with its explicit versioned runtime
oracle; an execution error must never count as successful selector mutation.
Accessing the running module object's `__annotations__` attribute forces
evaluation and retains actual mutation evidence on both 3.12 and 3.14.

Both complete owner sources must be refused on every interpreter at the
annotation's protected list read, independently of their runtime outcome.
Disabling only the `list_reads` rule admits each owner, pinning the shared
closed-form restriction without reviving an annotation effect interpreter.
The portable mutation control is unconditional; the compatibility branch
applies only to the original bare-name runtime oracle. These actual-Python
controls resolve the execution question left separate by DEC-1428.1 and retain
DEC-1413.1's maintenance-lint boundary. See [test execution](../TEST-EXECUTION.md).

<a id="dec-1458-1"></a>

**DEC-1458.1. A closeout window includes newly closed dependents, rather than
only the PRs still open after deletion.** DEC-1228.1's recorded-ID checks catch
closure of a known dependent. A dependent created after the final enumeration
has no recorded ID, so an open-only post-delete scan can silently miss its
closure. [GitHub's PR listing API](https://docs.github.com/en/rest/pulls/pulls#list-pull-requests)
supports `state=all`; complete that repository-wide scan after deletion.

Before the first enumeration, record the operation window and the owned parent
ref, reviewed head and merged base. Complete an initial `state=all` scan and
retain the identities, states, bases, heads and closure times of already closed
parent dependents. Retain the deletion request and confirmation times; confirmed
deletion ends the window. Use inclusive timestamp boundaries and require
evidence before treating a PR as outside the window. Re-read all recorded dependents by ID and inspect
PRs created in the window. Current base names do not prove prior association:
inspect base-ref history when needed, and stop if that evidence cannot decide.
Subject to DEC-1475.1's independent deliberate closure exemption, every newly
discovered parent-dependent PR is failed closeout, including one now closed or
already retargeted. Record its observed state, base, head and
remaining diff, and inspect commit provenance to establish expected surviving
work; no earlier head or patch is fabricated. Recorded dependents must retain
their expected OPEN/base/head/content checks unless DEC-1475.1 establishes an
independent deliberate closure. That exemption also applies to closures before
the window.

Any failed, incomplete or ambiguous read stops local cleanup. Restore only the
exact recorded owned parent head using an atomic missing-ref lease; a recreated
changed or foreign ref is not overwritten. Reopen and explicitly retarget
affected dependents only as authorized by DEC-1475.1's closure-provenance rule,
verify their state/head/base/expected content, and repeat
the closeout before retrying deletion. Existing review and associated current-
head CI gates remain required. A verified empty-dependent case needs no base
changes but still requires the pre-delete and all-state post-delete reads.

Validation retains #1216/#1226's actual closure/restoration/reopen/retarget
events and unchanged surviving patch. They establish closure and recovery, not
an observed late creation. Bounded negative controls cover late-created closure,
older intentional closure, changed head/base, missing content, incomplete reads
and pagination, and changed or foreign restoration refs. Removing the all-state
selection must hide the late-closed control; removing the window distinction
must turn the older-closed control into a false alarm. #1463's real closeout
supplies the successfully enumerated empty-dependent all-state control, without
creating a throwaway production PR.


<a id="dec-1457-1"></a>

**DEC-1457.1. Request fresh retargeted CI with a PR label event; exclude
metadata edits at the trigger.** GitHub's default `pull_request` activity types
are `opened`, `synchronize` and `reopened`. Add `labeled` as an explicit,
head-preserving request for a new PR-associated run on the current base's
merge ref. After refreshing OPEN/base/head/remaining diff and existing review
evidence, the primary agent adds `ci-retest` to the owned PR. If it is already
present, remove only that label before adding it again. Removal is not a CI
activity type. Verify head/base around the event and select the newly created
associated run, including when the head has no passing required check. The
label grants no approval: the current reviewed head must qualify before its
`ci-approval` deployment is approved, and the full matrix must pass.

Every label addition uses the same route, including ordinary triage labels;
there is no label-name condition that can replace the required gate with a
skipped check. Label additions start an approval-held run and cancel older PR
runs through the existing concurrency group. Title/body edits and base edits
alone do not trigger this workflow. A base change that requires fresh evidence
therefore needs the explicit label action; unrelated description edits need
nothing and spend no matrix. The gate remains the literal `ci-gate`, requires
every job through `always()`, and treats failure, cancellation and unexpected
skips as failure. Master, merge groups and dispatch remain uncancelled and
need no approval environment.

The rejected alternative admitted `edited` and skipped/renamed the metadata
gate in a separate concurrency group. Real #1535 evidence disproved it: after
run 37293158064 passed the entire matrix on head
`fe2f7a4839c957929c664a6127b41097fe75527f`, a body-only edit created skipped
run 37298874061. The successful run and its `ci-gate` check remained, but the
PR changed from `clean` to `blocked`. Ordinary queue admission returned
`Required status check "ci-gate" is expected.` The metadata suite had a literal
expression as its skipped name. Preserving the run or seeing green required
checks through the check API did not establish merge-box admission. Excluding
`edited` prevents that replacement at event ingestion instead of trying to
hide it with conditions or supply a passing gate for tests that never ran.

A PR event SHA is not accepted as proof of the tested combined tree. The
historical edited run 37273424527 was associated with the right PR/head/base,
yet checkout `b66f67da589c5fc5ba2eb0ffcd5e7a755b61844d` had the former base
as its first parent. Approval bootstraps the immutable defining workflow
revision (`github.workflow_sha`), so reviewed heads predating a new selector
need no rebase to acquire it. Select the live `refs/pull/N/merge` only when
stable complete PR/ref snapshots and both commit parents match the event's
reviewed head and intended base ref. The event base SHA is an ancestry floor:
the selected base parent includes it and belongs to the intended branch.
GitHub can retain a stable earlier merge ref while the same base advances;
ancestry checks allow that valid movement without rebasing. Pin the selected
SHA as an approval output for every execution job. Missing, unreadable, changed
or ambiguous evidence fails the bounded selector. Non-PR events retain their
exact event SHA.

A transient failure reruns its PR-associated run with `gh run rerun`; dispatch
cannot supply the PR check (Decision 206). A rerun keeps the original event
head/base intent, so use a new label event for a newly retargeted base. Missing
retry evidence stops admission; it never permits close/reopen, rewriting the
head or bypassing required checks. Moving master alone needs no rebase or
new review (Decision 502); the merge group tests the exact integrated tree.

Permanent tests use GitHub's pinned expression parser and verify trigger
activity types as well as approval, cancellation, complete fan-in, bootstrap
and per-job checkout contracts. Actual owned-PR controls must prove that
metadata edits create no run both while CI is pending and after success, and
that the complete current-head run remains admissible after the latter edit.
Retain actual label/base/run/ref/parent evidence, including missing-check
recovery; no throwaway production PR is needed. See
[GitHub's pull-request events](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#pull_request),
[rerun identity](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/re-run-workflows-and-jobs),
and [required check troubleshooting](https://docs.github.com/en/pull-requests/how-tos/merge-and-close-pull-requests/troubleshooting-required-status-checks).


<a id="dec-1473-1"></a>

**DEC-1473.1. Audit parent association over the fixed closeout window for every
observed PR identity, regardless of age.** DEC-1458.1 selects newly created PRs
for its history check. An older PR can acquire the parent base and lose it again
before the final read, leaving the same base, head and state at both endpoints.
Neither creation time nor snapshot equality proves that it was never dependent.
An update timestamp is not an association log, and unrelated metadata activity
is not evidence of dependency.

Use the union of identities from the initial and final complete `state=all`
scans and the explicitly recorded dependents. For each identity, retain readable,
complete base/state history with a known anchor and all transitions through its
post-delete read. Reconstruct association during the fixed window from before
the first enumeration through confirmed deletion. A base-change event's previous
and current ref both matter: moving away from the parent proves prior
association even when the final base is already `master`. Include events at
either timestamp boundary. When timestamp precision cannot order relevant
events relative to deletion, retain the ambiguity and stop cleanup. Do not
exclude a PR merely because its snapshots or `updated_at` match.

The audit is bounded by this identity set, fixed window and finite paginated
history reads, not a new background monitor or an executable cleanup service.
History may be limited to a trustworthy anchor before the window and all later
transitions through the final read; absent, hidden, truncated or inconsistent
evidence cannot establish an empty association history. Reconcile replayed
base/state with the observed endpoints. A disappeared identity, changed recorded
head, unavailable content or incomplete page stops cleanup under the existing
checks. No wait or repeated snapshot can substitute for missing history.

A newly established parent dependent fails closeout even if it is now closed or
already retargeted. Retain its observed state/base/head/diff without inventing
an earlier head or expected patch, then inspect commit provenance and apply
the existing recovery and verification rules, subject to DEC-1475.1's deliberate
independent closure exemption. Proven absence of parent association requires no
recovery even if an older PR reopens and closes again within the window. A
reopen or renewed association of an actual dependent instead requires fresh
checks under DEC-1475.1; PR age does not decide either case. This decision
extends association detection; DEC-1475.1 supplies closure provenance. Preserve
actual merge, recorded-ID reads, exact-owned-head atomic missing-ref restoration, refusal to overwrite a recreated foreign
ref, expected surviving content and associated current-head review/CI gates.

Read-only retained #1226 history supplies real closure, reopen and
`BaseRefChangedEvent` evidence naming both
`fix/issue-1130-ignored-test-ownership` and `master`, with complete pagination.
It demonstrates the available history fields, not an observed older-PR race.
Offline controls must separately qualify old PRs acquiring and losing parent
association, acquiring then closing, equal endpoint snapshots, unchanged older
intentional closure, unrelated activity, changed head or missing content,
incomplete/missing history and boundary ambiguity. Removing only this
association-history rule must restore the missed-dependency result. These are
policy qualifications; they do not claim an automated enforcement mechanism.


<a id="dec-1475-1"></a>

**DEC-1475.1. Parent association requires investigation, not permission to undo
an independent deliberate closure.** DEC-1458.1's blanket failure/reopen rule
can override a collaborator who intentionally closed a dependent during the
window but before the owned deletion request. DEC-1473.1 detects association
regardless of age; the closure disposition must also be independent of age.

First establish parent association with the bounded complete history audit.
Proven unrelated closure or reopen/close activity needs no recovery, including
an older PR with equal final snapshots. Missing association history remains
unknown, not unrelated. For an actual dependent, preserve its closed state and
base without reopening or retargeting only when all of the following hold:

- Complete readable closure, reopen and base-ref history, with a trustworthy
  anchor and available actors/timestamps, establishes a deliberate closure
  independent of the owned deletion and strictly before its request. Retain
  linked evidence of an explicit close action or collaborator confirmation;
  actor, `closed_at`, state reason and the absence of a deletion event alone do
  not establish deliberate intent or causation. Timestamp ties or precision
  insufficient to order closure against the request are ambiguous, not exempt.
- Record the closure's head and content baseline from retained observations or
  verifiable event/commit provenance; never invent an earlier head or expected
  patch for a newly discovered PR. The final closed state, base, head and content
  must agree with that baseline, with no subsequent reopen or renewed parent
  association through the post-delete read. Missing content or provenance stops
  cleanup. Apply this same rule to recorded and newly discovered dependents,
  whether created before or during the window.

This exception preserves an independent deliberate closure; it does not exempt
an open dependent, deletion-caused closure, changed head/content, later reopen
or renewed association. Those cases require fresh current state/base/head and
expected surviving patch checks. Reopen only when evidence establishes closure
caused by this owned deletion, then explicitly retarget and verify the open PR.
For an independent closure that no longer qualifies, or an unknown closure
cause, stop cleanup and PR recovery mutations: retain the observations and
obtain missing evidence or the collaborator's direction before changing their closed
PR. Restoring the exact owned parent ref under an atomic missing-ref lease may
preserve recoverability; it does not itself authorize reopening. Never overwrite
a changed or foreign ref. No repeated polling substitutes for missing evidence.

The exemption changes the disposition after association is established, not
the identity set, operation window or actual-merge prerequisite. Retain both
all-state scans, recorded-ID reads, complete history including both timestamp
boundaries, exact-owned-head protection and expected surviving content. Any
retargeted open dependent still needs existing current-head review and complete
associated CI before enqueueing. No cleanup executor or new service is added.

Read-only #1226 history retains a `ClosedEvent` and `BaseRefDeletedEvent` at
2026-09-27T23:37:04Z with the same actor, followed by reopen and base change.
The closure's `intent` and `closer` fields are null; its `COMPLETED` state reason
also occurs on the later merged closure. These are actual API observations, not
proof that an actor name or state reason identifies a deliberate independent
close. Retain them without creating throwaway production PRs or inferring an
unobserved intentional-close race.

Offline policy controls cover deliberate pre-request closure of both older and
in-window PRs, deletion-caused closure, independent-close-then-reopen, renewed
association, changed head/content, missing baseline, unavailable/incomplete
history, unrelated closures and timestamp ambiguity. The unrelated older
reopen/close case must stay clear even with equal endpoint snapshots. Removing
only the exemption must restore false recovery of intentional-closure controls;
weakening unknown-history refusal must expose unsafe acceptance. These qualify
the written policy, not a shipped automated enforcement mechanism.
