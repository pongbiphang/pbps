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

    **A stacked pull request retargets itself only once the upstream branch is
    deleted.** GitHub retargets every open pull request based on a merged head
    branch onto that pull request's base, but the trigger is the *deletion* of
    the branch, not the merge. This repository has `delete_branch_on_merge`
    off, and the obvious remedy is refused: with a queue required, `gh pr merge
    --delete-branch` errors out instead of enqueueing, because deleting the head
    branch before the queue has merged closes the pull request and removes it
    from the queue. The branch is therefore deleted as a separate closeout step
    once the merge has landed, and that deletion is what retargets whatever was
    stacked on it. Left undeleted, the downstream pull request stays based on a
    merged feature branch: it never enters the `master` queue, and merging it
    writes to that branch rather than to `master`.

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
