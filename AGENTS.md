# AGENTS.md

Rules for coding agents in this repo. Reasons live in `docs/`.

## What this is

`pbps` (PongBiphang Schema) is declarative database schema version control.
Users declare the desired schema in YAML. The tool diffs it against the
database, generates change scripts, and applies them behind a risk gate:

- humans supply rename and drop intent, and git records it;
- every change is risk-classified;
- saved plans are pinned by checksum;
- state lives in the database itself.

Read before changing things:

- [docs/SPEC.md](docs/SPEC.md): the design. Read before changing the data
  model or adding a kind of change.
- [docs/DECISIONS.md](docs/DECISIONS.md): every non-obvious choice, and the
  topic files in `docs/decisions/`.
  - Add a new entry as `DEC-<issue>.<k>` at the end of its topic file, and add
    no row anywhere else.
  - Never renumber. Code comments cite these identifiers.
  - Look up the closed entries 1–543 in `docs/decisions/legacy-index.md`.
- [docs/PITFALLS.md](docs/PITFALLS.md): bugs shipped or nearly shipped, and
  the shapes they belong to.
- [docs/STATUS.md](docs/STATUS.md): phase, command surface, open items.
- [docs/ORDERING.md](docs/ORDERING.md): how a plan's order is decided. Read
  before adding a kind of change, a sort class or a reordering pass.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): crate boundaries and
  inviolable constraints. Read before changing crate responsibilities,
  dependencies, the model, or the dialect and driver boundaries.
- `docs/ADR-*.md`: standalone decision records.

## Working here

- Write code, comments, docs, commit messages, issues and PR bodies in English.
- Fetch first. Cut every worktree from the latest `origin/master`.
- Keep the notes of recurring background work (CI watches, check-ins)
  accurate. Stop the work when it is done.

## Taking an issue

- List open issues **and** open PRs first.
  - Confirm the issue is open and no PR carries it.
  - Name the other issues that touch the same code.
- Comment on the issue to claim it **before** starting.
- If it is a duplicate, blocked, wrong or already fixed, say so on the issue
  and stop.
- Treat the issue as the specification. Fix what it says, at the size it says.
- Work in a git worktree, never in the main checkout. Remove the worktree after
  merge.
- Record the issue and the hot files it will change (format versions,
  `schema_diff.rs`, `plan.rs`, decision topic files) on the coordination board
  (`~/pbps-workers/board.md`) before starting. Do not start an issue whose hot
  files overlap one in progress; ask the coordinating session (DEC-2415.1).
- Use one branch per issue, cut from `origin/master`, named
  `fix/issue-<n>-<slug>`.
- Push the issue branch without asking. Never push to `master` or to another
  issue's branch.
- Open the PR as a **draft**, with `Closes #<n>` in the body. Taking the issue
  is permission to open it. A PR that carries only part of an issue says
  `Part of #<n>`.
- Post `@codex review` as soon as the draft is open.

## PR size and design

- Keep production code at most 1,000 changed lines (additions plus deletions).
  - Up to about 1,200 is fine; state the reason in the PR body.
  - Past that, look for the missing general solution first. It usually makes
    the PR smaller.
  - Only when no generalisation exists, split along a real seam into several
    PRs of the same issue.
  - Never move scope to a follow-up issue to fit.
- Keep tests at about 1,000 changed lines. More is fine; state why.
- Measure with `python3 scripts/pr-size.py` and put both counts in the PR body.
  - Tests are `#[cfg(test)]` modules, `tests.rs`, `tests/`, fixtures, golden
    data and `scripts/*_test.py`.
  - `docs/` and Markdown count toward neither budget.
- Prefer modular, general solutions over code that handles each case
  separately.
  - Use one when an existing mechanism already fits, or when a pattern appears
    a third time.
  - Do not build a framework for a single case.

## Local checks

- The **full gate** is what CI runs:
  - every step of the `lint` and `quick` jobs in `.github/workflows/ci.yml`:
    the Python checks, `cargo fmt --all --check`, warning-free
    `cargo clippy --workspace --all-targets`, and
    `cargo test --workspace --all-targets --no-fail-fast`;
  - then `scripts/live-tests.sh` and `scripts/live-tests-pg.sh`, run directly
    or through the sharded driver (DEC-2357.1).
- Run the full gate on every head that can be queued (DEC-1838.1,
  DEC-2415.1). All green, then push:
  - before marking the PR ready;
  - before every push in the ready phase;
  - before pushing a rebase, except a docs-only rebase (DEC-2000.1) and a
    rebase whose own patch is unchanged (DEC-2553.3).
- Every other push (the first push and draft-round fixes) runs the
  **round gate**:
  - the two decision checks and the two raw-in-messages checks;
  - `fmt` and warning-free `clippy`;
  - the tests that pin the change or the round's fixes, live ones included
    when it is engine-facing.
  - A round fix that breaks something it did not touch is found by the full
    gate before ready.
- A docs-only push runs only the two decision checks.
- **Complete a stopped gate; do not repeat it** (DEC-2553.1). When a sharded
  run stops on an infrastructure fault, or fails only on tests registered in
  `scripts/known-flakes.json` (DEC-2553.2), finish it on the same head with
  `scripts/live-shards.py --resume <previous-output>`.
  - It reruns only the names with no `ok` receipt, and the combined receipts
    must still equal `--list` exactly.
  - It refuses a different head tree or inputs digest, an unreadable or
    partial previous output, and any `FAILED` test that is not a registered,
    unexpired flake. Such a failure needs a fix and a new full gate.
  - Record both outputs' `summary.json` paths and the head tree in the PR.
- Register a flake only for an open issue, with an expiry at most 30 days out.
  The register is read from `origin/master`, so an entry waives nothing in the
  PR that adds it. An expired entry is inactive; the check refuses it only in
  a PR that touches the register and in the nightly run.
- Before queueing a full gate, confirm the head merges cleanly with
  `origin/master`. Rebase a conflicting head first.
- Use the Python version CI pins.
- A new `#[ignore]` test discovered on its owner's platform can use a covered
  target/module rule without a registry edit. Cross-platform discovery needs an
  exact legacy entry or an owner that runs on the discovering platform.
  For an uncovered module, add a unique rule to `scripts/ignored-test-rules.json`.
  Python-driven and child-helper owners retain legacy exact entries; see
  `docs/TEST-EXECUTION.md`.
- Never run two live suites against the same containers at once. Each makes
  the other fail, and the failure reads like a product bug.

## The review loop

- Push the round's fixes, check that the PR head is the commit you pushed, then
  post `@codex review` once. Never post it per commit.
- Always fix a P0 before the loop advances.
- Fix a P1–P3 only if it is one of:
  - **(a)** a valid plan is refused;
  - **(b)** a wrong recording with a single deployer and no concurrent writer;
  - **(c)** a concurrent case SPEC §7.6 promises to catch and does not.
- Answer every other P1–P3 finding:
  - Cite the SPEC section or DECISIONS entry that made the choice.
  - Open `gh issue create --label deferred-review` titled `review: ...`. Link
    the thread and name the fix and its test.
  - Change nothing in the repo for it, and add no DECISIONS entry.
- Ignore the badge and the size of the fix, except that a P0 is always fixed.
  A P1 outside the rules is answered; a P3 inside them is fixed.
- Triage your own findings the same way. Out-of-rule ones become new issues,
  never extra scope on this PR.
- On each fixed thread, reply "Fixed in `<sha>`. <what changed>. <which test
  pins it>." and nothing else.
- Resolve every thread you replied on, fixed or deferred alike.
- **Past seven completed review rounds (draft and ready together) without
  reaching the merge group, stop and classify.**
  - If the recent findings are one class (one mechanism on ever narrower
    inputs, or fixes that spawn their neighbours), stop patching.
  - Open a design issue: the problem, the root cause, the design and where it
    belongs, the cases it absorbs, and its acceptance tests.
  - Tell the maintainer. Answer the class's findings with that issue.
  - Still fix every P0. A class finding in the three fix cases is answered with
    the design issue only after the maintainer agrees (as #1366 was for
    #1346).
  - Findings outside the class follow the three-case rules.
- **Draft phase.** Stop after three consecutive completed reviews with no P0 or
  P1, or at the first completed review with no findings.
  - A P0 or P1 resets the count.
  - Count a review only if its `Reviewed commit:` is the pushed head.
  - Never push a docs-only commit to move the count.
- **On stopping.** Kill the watch and post no more `@codex review`. Run the
  full gate on the head, then mark the PR ready.
  - Fix a full-gate failure as a draft round. Its head needs its own completed
    review before ready.
  - Marking ready triggers the required code review. Wait for it.
  - A security review is optional. Its findings follow the same triage; its
    completion is not a gate.
- **Ready phase.**
  - A completed code review with no findings qualifies the PR for CI at once.
  - Otherwise obtain three consecutive completed code reviews with no P0 or P1.
    Fix or defer each P2/P3; they do not reset the count.
  - The latest counted review must name the current pushed head.
  - A P0 or P1 resets the count. Address it under the finding rules, move the
    PR back to draft, and resume the loop.
- Never add "one more round". More review is a new instruction.

## CI and the merge queue

- **CI on a PR runs only when approved** (DEC-1017.1).
  - Every push starts a run that stops at `approval` (the `ci-approval`
    environment); the next push cancels it.
  - Do not approve during the review loop.
  - The primary agent approves exactly one run: the one on the current head,
    once the ready phase qualifies it, or once a rebase's resulting-head review
    does, or directly after a docs-only rebase.
  - Approve with `gh api repos/{owner}/{repo}/actions/runs/<run-id>/pending_deployments
    -X POST -F 'environment_ids[]=<id>' -f state=approved -f comment='review
    qualified'`, taking the id from `GET .../pending_deployments` on that run.
  - Merge-group, `master` and dispatched runs start by themselves.
- The PR run omits `live`, `live-pg`, `resolver` and `release-windows`; the
  merge group runs them with every other job (DEC-1835.1). `master` runs
  nightly, not per merge (DEC-1836.1).
- Wait for `ci-gate` by reading the run, not `check-runs`, which lists only the
  jobs created so far.
- Retry a transient failure with `gh run rerun <run-id>` (`--failed` for the
  failed jobs). Never use `gh workflow run ci.yml` for a PR: a dispatched run
  belongs to no PR, so the required check stays unsatisfied (DECISIONS 206).
- On red CI, fix it, push, and return to the loop as a draft.
- After an actual PR base edit (DEC-1457.1):
  - Refresh OPEN state, base, head, diff and review evidence.
  - When fresh CI is needed, add the `ci-retest` label (remove and re-add it if
    present). Check the reviewed head and intended base before and after.
  - Select the new PR-associated run. Its merge SHA must have the intended base
    and the reviewed head as parents, and every job must use it.
  - Require the complete PR job set and actual required-check admission. Never
    pick a run because it is newest or green, and never approve merely because
    a label was added.
- Before enqueueing, the primary agent independently verifies:
  - the issue-to-diff match and the architecture;
  - local-test evidence (for a resumed gate, both `summary.json` files and the
    head tree; for an unchanged-patch rebase, the `gate_scope.py` output,
    rerun);
  - review counts and heads, and thread dispositions;
  - dependency order and required CI checks.
- Subagents never enqueue and never merge.
- Enqueue with `gh pr merge --merge`. Never pass `--delete-branch`: deleting
  the head before the merge closes the PR and drops it from the queue.
- A queued PR is not a merged one. Wait for the merge before cleanup.
- If the queue ejects the PR, **read the failing job before re-queueing** and
  say which case it was:
  - A failure that reproduces, or a test failing on its merits, is a real
    interaction. Rebase and fix it (below); do not re-queue.
  - An infrastructure fault (the engine startup crashes `ci.yml` and
    docs/PITFALLS.md record) is transient. Re-queue.
  - A conflict is a rebase case.
  - A first failure in a merge-group-only job is read the same way.
- Never bypass the ruleset.
- Report at each merge:
  - the draft and ready review counts;
  - the qualifying route;
  - the merge commit;
  - every finding deferred to an issue.

## Rebasing

- Do not rebase because `master` moved. The queue builds the PR against
  current `master` and everything ahead of it (DECISIONS 502).
- Rebase onto current `master` only for:
  - a **conflict**, which the queue ejects;
  - an **interaction the merge group confirmed**: `master` changed something
    the branch calls.
- To finish a rebase:
  1. Fix what the rebase broke.
  2. Run the full gate.
  3. Push with `git push --force-with-lease`.
  4. Obtain a completed code review whose `Reviewed commit:` is the resulting
     head.
  - Findings follow the same rules; a P2 may go straight to a
    `deferred-review` issue.
  - After a P0 or P1, fix it and get another resulting-head review.
  - Once a review has no P0 or P1, approve the run on that head. The draft and
    ready three-review gates are not repeated.
- **Docs-only rebase** (DEC-2000.1): when every resolved conflict is in
  documentation and `git range-diff` shows the branch's own commits differ only
  in documentation:
  1. Run the two decision checks.
  2. Push with `--force-with-lease`.
  3. Approve the CI run on the resulting head.
  - No full gate and no new review are needed.
  - A rebase for a merge-group interaction is never docs-only.
- **Rebase with an unchanged patch** (DEC-2553.3): when `scripts/gate_scope.py`
  reports `unchanged` for the last head a full gate passed on and the rebased
  head, the rebase needs no new full gate:
  1. Run the two decision checks, the two raw-in-messages checks, `fmt` and
     warning-free `clippy` on the rebased tree.
  2. Push with `--force-with-lease`.
  3. Obtain the resulting-head review as for any rebase, then approve the CI
     run on that head.
  - Record the script's output in the PR.
  - A rebase for a merge-group interaction is never one of these.

## Deleting a merged head branch

Follow DEC-1473.1 and DEC-1475.1; they define the evidence below.

- Confirm the PR merged; an ejected PR also leaves the queue.
- Record the operation window's start, then the parent ref, head and base.
- Scan every PR (`state=all`, all pages).
  - Record the closed ones that used the branch as their base.
  - Then list every open PR based on the branch, with its head and the diff
    expected after the parent's changes.
- For each open dependent, while the branch still exists:
  1. Change its base to the parent's base.
  2. Verify it is OPEN, has that base, has its recorded head, and shows the
     expected diff.
- Re-list dependents right before deletion. A new one passes the same checks.
- An unreadable or incomplete result is not an empty set. Keep the branch when
  any check fails.
- Delete only the owned remote head. Record the request and confirmation times;
  the window ends at confirmed deletion.
- After deletion:
  1. Scan all PRs again and re-read the recorded dependents by ID.
  2. Audit every base and state transition in the window for all of those PRs,
     whatever their age.
  - Snapshots, creation or update times cannot exclude an association in
    between.
  - Missing anchors, hidden history or ambiguous ordering stop cleanup. Do not
    wait or poll for a better snapshot.
- If a PR was associated with the branch in the window:
  - Preserve a closure only when it is proven deliberate, independent and
    strictly before the deletion request, and unchanged through the final read.
  - Otherwise it is a failed closeout, even if it is now closed.
  - Recover the exact owned ref with an atomic missing-ref lease.
  - Reopen only a PR proven closed by this deletion, or one a collaborator
    explicitly names.
  - Retarget and verify, then retry.
  - An unknown closure cause authorizes no PR mutation.
- Remove the local branch and worktree only after the window check passes.
- Each retargeted dependent still needs its own review and current-head CI
  before enqueueing (DEC-1228.1, DEC-1458.1).

## The issue loop

- Hold at most two issues per agent: one waiting on a gate, a review or the
  queue, and one in progress (DEC-2415.1).
- Start the second only when it is independent of the first. Never build on
  a head that has not passed its gate.
- Take a next issue only when the user asked for a continuous or multi-wave
  loop.
- The nightly `master` run is not a gate. Read a red one before calling it a
  regression:
  - A test failing on its merits is the next task.
  - An engine startup crash is not; re-run it on the same tree to tell which.
- When a merge touches `Cargo.toml`, `Cargo.lock`, `deny.toml` or the audit
  workflow, wait for the dependency audit on `master`. A red audit is the next
  task.

## How to be right here

- **Measure against a real engine before believing yourself.** Run any
  question about what SQL Server or PostgreSQL does on that engine.
- **Revert each fix and watch its new test fail** before keeping the fix.
  Restore from a copy, never with `git checkout` on a file with uncommitted
  work.
- **Sweep every call site for the shape you just fixed** before pushing.
- **When you remove the reason for an ordering or check**, update every caller
  that relied on it.
- **Treat absent, empty and unreadable as three different things.** Never let
  an error or a hidden object read as "nothing there".
- Make a failure *unrepresentable* rather than handling it.

## Product guardrails (SPEC 14.3)

Refuse these shortcuts and point here. Each bypasses the typed plan, the
checksum, recorded human intent, or the git audit trail.

- **No `push`.** `plan` then `apply --plan` stays two steps.
- **Rename suggestions, never rename decisions.** No non-interactive flag
  supplies identity intent.
- **`revert`, not rollback.** Apply a historical state as a new forward plan
  through the ordinary gate.
- **No policy SaaS.** Policies and reports are files or stdout, air-gapped.
- **One source of truth for the declarations.** No ORM-model loaders.
- **No plugin execution engine.** Nothing runs between "plan approved" and
  "statements executed".

## Writing conventions

- Write comments that explain **why**, especially why not the obvious approach.
- Name tests after the property (`column_order_does_not_affect_equality`, not
  `test_eq`).
- Give every test module **negative cases**.
- Use conventional commits; explain the trade-offs in the body.
- Use **LF everywhere** (see `.gitattributes`).
- Follow [SPEC §4.3](docs/SPEC.md#43-format-rules) and
  [YAML and file-format traps](docs/PITFALLS.md#yaml-and-file-format-traps)
  for the loader's format rules.
