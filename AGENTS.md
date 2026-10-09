# AGENTS.md

Guidance for coding agents working in this repo. Rules here; reasons in `docs/`.

## What this is

`pbps` (PongBiphang Schema): declarative database schema version control. Users
declare the desired schema in YAML; the tool diffs it, generates change scripts,
and applies them behind a risk gate. Rename and drop intent is human-supplied
and recorded in git, changes are risk-classified, saved plans are
checksum-pinned, and state lives in the database itself.

- **[docs/SPEC.md](docs/SPEC.md)** — the design. Read before changing the data
  model or adding a kind of change.
- **[docs/DECISIONS.md](docs/DECISIONS.md)** — index of the record of every
  choice that is not the obvious one; the entries live by topic in
  `docs/decisions/`. Code comments cite their identifiers; never renumber. A new
  entry is `DEC-<issue>.<k>` at the end of its topic file, never the next
  sequential number (the index says why).
- **[docs/PITFALLS.md](docs/PITFALLS.md)** — bugs shipped or nearly shipped, and
  the shapes they belong to.
- **[docs/STATUS.md](docs/STATUS.md)** — phase, command surface, open items.
- **[docs/ORDERING.md](docs/ORDERING.md)** — how a plan's order is decided,
  and every pair of change kinds by how their order is decided. Read before
  adding a kind of change, a sort class or a reordering pass.
- `docs/ADR-*.md` — standalone decision records.

## Working with me

- Write code, comments, docs, commit messages, issues and PR bodies in English.
- Fetch first: every new worktree is cut from the latest `origin/master`.
- Recurring background work (CI watches, check-ins) is welcome; keep the notes
  it carries accurate, and stop it when the work is done.

## Taking an issue

- List open issues **and** open PRs first. Confirm the issue is open and no PR
  carries it, and name the other issues touching the same code.
- Comment on the issue to claim it **before** starting work.
- Say so on the issue and stop if it is a duplicate, blocked, wrong, or already
  fixed.
- Treat the issue as the specification: fix what it says, at the size it says.
- Work in a git worktree, never in the main checkout. Remove it after merge.
- One branch per issue, cut from `origin/master`, named `fix/issue-<n>-<slug>`.
- Before every push, run locally what CI runs: `python3
  scripts/check_decisions_test.py`, `python3 scripts/check-decisions.py`,
  `cargo fmt --all --check`, `cargo clippy --workspace --all-targets`
  warning-free, `cargo test --workspace --all-targets`, `scripts/live-tests.sh`
  and `scripts/live-tests-pg.sh`. All green, then push.
- Push the issue branch without asking. Never push to `master` or to another
  issue's branch.
- Open the PR as a **draft**, with `Closes #<n>` in the body. Taking the issue is
  permission to open it.
- Post `@codex review` as soon as the draft is open.

## The review loop

- Push the round's fixes, verify the PR head is the commit you pushed, then post
  `@codex review` once. Never per commit.
- A P0 finding is always fixed before the loop can advance. For P1–P3, fix a
  finding only if it is one of: **(a)** a valid plan is refused; **(b)** a wrong
  recording with a single deployer and no concurrent writer; **(c)** a
  concurrent case SPEC §7.6 promises to catch and does not.
- Answer every other P1–P3 finding with the SPEC section or DECISIONS entry that
  made the choice, and open an issue for it: `gh issue create --label
  deferred-review`, title `review: ...`, body linking the thread and naming the
  fix and its test. Change nothing in the repo for an answered finding, and add
  no DECISIONS entry for one.
- Except for mandatory P0 fixes, ignore the badge and the size of the fix. A P1
  outside the rules is answered; a P3 inside them is fixed.
- Triage findings you make yourself the same way. Out-of-rule ones become new
  issues, never extra scope on this PR.
- Reply on each fixed thread yourself: "Fixed in `<sha>`. <what changed>.
  <which test pins it>." Those three facts, nothing else.
- Resolve each thread yourself once you have replied on it — fixed or deferred
  to an issue alike. An open thread reads as unanswered.
- **Past seven review rounds without reaching the merge group, stop and
  classify.** Count every completed round, draft and ready together. If the
  recent findings are edge cases of one class — one mechanism on ever narrower
  inputs, or fixes that spawn their neighbours — stop patching them. Open a
  design issue for a general solution and write its reasoning there: the
  problem, the root cause, the design and where it belongs, the cases it
  absorbs, and its acceptance tests. Tell the maintainer, then answer the
  class's findings with that issue. A P0 is still always fixed. A class
  finding that is one of the three fix cases is answered with the design
  issue only once the maintainer has agreed to it, as #1366 was for #1346.
  Findings outside the class still follow the three-case rules.
- In the draft phase, stop after three consecutive completed reviews with no P0
  or P1, or as soon as a completed review reports no findings. A P0 or P1 resets
  the count. Count a review only if its `Reviewed commit:` is the pushed head.
  Never push a docs-only commit to move the count.
- On stopping: kill the watch and post no further `@codex review`, then mark the
  PR ready. Do **not** rebase merely because `master` moved — the queue handles
  that, and rebasing would re-run the local checks and CI and create a new
  review head for nothing (DECISIONS 502). Rebase only for the two cases that
  need the combined tree in your hands: a conflict, or an interaction the merge
  group confirmed;
  that rebase does not repeat the qualified draft gate, but the ready-phase code
  review must then name the resolution head. Marking ready triggers the required
  code review; wait for it. A security review is optional: if run, its findings
  use the same triage rules, but its completion is not a gate.
- In the ready phase, a completed code review with no findings qualifies the PR
  for CI immediately: approve the run on the head. Otherwise, obtain three
  consecutive completed code reviews with no P0 or P1; P2/P3 findings do not reset the count, but each must
  be fixed or deferred under the finding rules. The latest counted review must
  name the current pushed head.
- A P0 or P1 in a ready-phase code review resets the count: address it under the
  finding rules (P0 is always fixed; P1 is fixed or deferred according to the
  three-case list), move the PR back to draft, and resume the loop.
- **CI on a pull request runs only when approved** (DEC-1017.1). Every push
  starts a run, but it stops at `approval`, held by the `ci-approval`
  environment, and runs nothing; the next push cancels it. Do not approve
  during the review loop. Approve exactly one run: the one on the current head,
  once the ready-phase gate below qualifies it — or once the resulting-head
  review of a rebase does. The primary agent approves it:
  `gh api repos/{owner}/{repo}/actions/runs/<run-id>/pending_deployments -X POST
  -F 'environment_ids[]=<id>' -f state=approved -f comment='review qualified'`,
  with the id from `GET .../pending_deployments` on the same run. Until it is
  approved `ci-gate` is pending and the pull request cannot be queued. The
  merge group, `master` and dispatched runs name no environment and start by
  themselves.
- After an actual PR base edit, refresh OPEN/base/head/diff and review
  evidence (DEC-1457.1). When fresh associated CI is required, add the
  `ci-retest` label to that owned PR; if already present, remove only that label
  and add it again. Verify the unchanged reviewed head and intended base before
  and after the label event, then select its new PR-associated run. Every label
  addition uses the approval/full-matrix route; label removal and title/body
  edits do not trigger CI. Never approve merely because a label was added.
  Verify approval bootstraps its immutable defining workflow and selects a
  merge SHA with the intended base and reviewed head as parents. Every job must
  use that pinned SHA, and its base parent must include the event base and
  belong to the intended base branch. Same-base advancement needs no rebase.
  Require the complete approved matrix and actual required-check admission;
  a retained green run alone is insufficient. Do not select a run merely
  because it is newest or green in Actions.
- Wait for `ci-gate` on the approved run and read the run, not `check-runs`,
  which lists only the jobs created so far. Retry a transient failure with
  `gh run rerun <run-id>`
  (`--failed` for the failed jobs alone): a re-run keeps the run's pull-request
  association, so its `ci-gate` is the one the merge box reads. Never reach for
  `gh workflow run ci.yml` to do that — it starts a `workflow_dispatch` run
  whose check suite belongs to no pull request, so it goes green in the Actions
  tab while the required check stays unsatisfied (DECISIONS 206). Dispatch is
  for a branch that has no pull request.
- Red CI: fix it, push, and return to the loop as a draft.
- **`master` moving is not your problem any more.** The merge queue builds the
  pull request against the current `master` and against whatever is queued
  ahead of it, so a branch that is merely behind is merged without anyone
  rebasing it. Do not rebase to clear `BEHIND`; the strict policy that made
  that necessary is off (DECISIONS 502).
- **Two things still put the branch back in your hands**, and both start the
  same way: rebase onto current `master`, because neither can be reproduced on
  a branch that predates it. A **conflict**, which the queue cannot resolve and
  ejects instead. And an **interaction the merge group confirmed** — `master`
  changed something this branch still calls — which a stale branch cannot even
  compile, let alone test. Rebase, fix it, run the required local tests, push
  with `git push --force-with-lease`, and obtain a completed code review whose
  `Reviewed commit:` is the resulting head. P0 is always fixed;
  P1–P3 follow the same three-case finding rules, and P2 may be deferred
  directly to a linked `deferred-review` issue. A P0 or P1 must be addressed
  and followed by another completed resulting-head code review. Once that
  review has no P0 or P1, the PR may proceed without repeating the draft or
  ready three-review gates; approve the run on that head, and all branch checks
  required on it must still be green.
- Before **enqueueing**, the primary agent independently verifies the
  issue-to-diff match, architecture, local-test evidence, review counts and
  heads, thread dispositions, dependency order, and required CI checks. That
  verification is unchanged; only the step it precedes has moved. Subagents
  never enqueue and never merge.
- The primary agent enqueues with `gh pr merge --merge` once every gate above
  passes — never with `--delete-branch`, which `gh` refuses outright when a
  merge queue is required, because deleting the head branch before the queue
  has merged closes the pull request and drops it from the queue. With a merge
  queue required, that command **adds the pull request to the queue** rather
  than merging it: GitHub builds a branch of `master` plus everything queued
  ahead plus this pull request, runs `ci.yml` on the `merge_group` event, and
  merges only if that is green. Wait for the merge to land before deleting the
  branch and removing the worktree — a queued pull request is not a merged one.
- **Verify dependents before deleting the merged remote head branch.** Confirm
  the parent actually merged, then start the operation window and complete an
  initial repository-wide `state=all` scan. Retain the identities, states,
  bases, heads and closure times of already closed parent dependents, then
  enumerate all open PRs based on its head branch, completing pagination.
  Record each dependent's head and expected
  remaining diff after the parent's merged changes. At every dependent read,
  before retargeting or requiring OPEN state, apply DEC-1475.1's same closure
  disposition. A qualifying closed dependent is preserved, not retargeted or
  rejected for being closed; an open dependent must pass the checks below.
  Before a deletion request exists, use the completed read as the provisional
  closure cutoff and retain its evidence; recheck against the actual request
  and through the final read before allowing local cleanup. Unknown evidence
  stops the operation. While the parent branch still exists, explicitly change
  each open dependent's base to the merged parent's base. Verify it is OPEN, has
  that base and its unchanged recorded head, and shows the expected remaining
  diff. If it closes during these steps, apply the same disposition before
  declaring failure or making another PR mutation. Re-enumerate immediately before
  deletion; any new dependent must pass the same checks. An unreadable or
  incomplete result is not an empty dependent set; retain the parent branch
  when any verification fails. A successfully verified empty set needs no base
  changes. Delete only the owned remote head after these checks, reverify the
  recorded dependents and enumeration afterwards, then remove the local branch
  and worktree only after the following operation-window check.
  Record the window before the first enumeration, the parent ref/head/base,
  and the deletion request and confirmation times; the window ends at confirmed
  deletion. After deletion, complete a
  repository-wide `state=all` PR scan and re-read recorded dependents by ID.
  Audit base/state history through that fixed window for the union of IDs in
  both all-state scans and the recorded dependents, regardless of PR age.
  Complete pagination and retain a known base/state anchor plus every transition
  through the post-delete read. Reconstruct whether the owned parent was a base
  at any point in the window, including either timestamp boundary; use both
  previous and current refs of a base-change event. Creation time, updated time,
  current base and equal endpoint snapshots cannot exclude an intervening
  association. Activity without parent association is not itself a failure.
  Missing anchors, hidden/incomplete history, inconsistent state or ambiguous
  boundary ordering stop cleanup; do not replace history with a delay or poll
  indefinitely for a favorable snapshot (DEC-1473.1).
  For a proven parent association, apply DEC-1475.1's closure-provenance check
  before classifying failure or changing a closed PR. Preserve a deliberate
  closure proven independent and strictly before the deletion request, regardless
  of PR age. Retain complete closure/reopen/base history, available actors and
  timestamps, linked evidence of the deliberate close, and its head/content
  baseline; actor or timestamp alone does not prove intent. Require unchanged
  closed state/base/head/content through the final read, with no later reopen or
  renewed parent association. Proven unrelated reopen/close activity needs no
  recovery. Otherwise a newly discovered parent-dependent PR is failed
  closeout, including one that is now closed. Do not invent a pre-delete head
  or expected diff for it; retain its observed state/base/head/diff and inspect
  the commit provenance before recovery. The same independent-closure exemption
  applies to PRs closed before the window. Failed, incomplete or ambiguous
  reads stop local cleanup. Recover only the exact owned parent ref; an atomic
  missing-ref lease must prevent overwriting a changed or foreign ref, then
  autonomously reopen only a dependent proven closed by this deletion. Explicit
  collaborator direction may instead authorize reopening the identified PR
  within that instruction; direction for another PR/action is not permission.
  Then explicitly retarget and verify affected open dependents before retrying.
  Unknown closure cause or ambiguous ordering alone authorizes no PR mutation.
  Obtain missing evidence or scoped collaborator direction; authorization to
  reopen does not waive unresolved cleanup evidence, owned-ref protection or
  state/base/head/content and review/CI checks. A later reopen, changed head/content or renewed parent association
  invalidates the closure exemption and requires fresh state/base/head/expected
  patch checks (DEC-1475.1).
  A confirmed empty set must pass both scans. Do not rely on automatic
  retargeting or rebase merely for cleanup. Existing review and current-head CI
  gates still apply to each dependent before enqueueing; refresh them after the
  base change (DEC-1228.1, DEC-1458.1).
- A pull request must be green on its **own** head before it can be queued, so
  a merge costs **two pre-merge runs** of `ci.yml` — one on the pull request,
  one on the merge group — plus the post-merge run on `master`, which gates
  nothing. That is the price of the queue and it is the cheap half of the trade:
  what it buys is that none of them has to be repeated because somebody else
  merged first.
- If the queue ejects the pull request, **read the failing job before
  re-queueing**, and say which of the two it was. A merge-group failure that
  reproduces, or that is a test failing on its merits, is a real interaction
  with what merged ahead of it: rebase and fix it as above, do not re-queue.
  A merge-group failure
  whose log shows an infrastructure fault — the resource-shaped engine startup
  crashes `ci.yml` and docs/PITFALLS.md both record — is transient, and
  re-queueing is the right move. What is never right is re-queueing without
  reading, which is how a real interaction gets merged on the second roll.
  A conflict is the case above.
- Never bypass the ruleset. It requires green CI on the pull request head and
  green CI on the merge group.
- Report at each merge: the draft and ready review counts, the qualifying route,
  the merge commit, and every finding deferred to an issue.
- Never add "one more round" — more review is a new instruction.

## The issue loop

- Without an explicit parallel invocation, take one issue at a time and claim
  the next only after the current PR is merged.
- When `parallel-issue-loop` is explicitly invoked, the primary agent may claim
  and coordinate multiple eligible issues concurrently. Each subagent owns
  exactly one issue at a time, uses an isolated branch and worktree, and never
  merges. Dependency-linked issues merge in topological order.
- `ci.yml` runs on `master` after a merge. It is not a gate — the merge queue
  already ran it on the exact tree the merge produced — so do not wait for it
  before taking the next issue. **A red one is read, not assumed**, by the same
  procedure as an ejection: the merge group passed this identical tree minutes
  earlier, so open the failing job before calling it a regression. A test
  failing on its merits is one, and becomes the next task; the resource-shaped
  engine startup crash that `ci.yml` and docs/PITFALLS.md both record is not,
  and a re-run on the same tree settles which it was. The dependency audit is a
  separate workflow and does run on `master` when the merge touched
  `Cargo.toml`, `Cargo.lock`, `deny.toml` or its own workflow — wait for it in
  that case, and a red audit is the next task, not the next issue.
- Take the next issue, following "Taking an issue", only when the user requested
  a continuous or multi-wave loop.

## How to be right here

- **Measure against a real engine before believing yourself.** Three times the
  obviously-correct answer was wrong and only a live server said so. When a
  question is about what SQL Server or PostgreSQL does, run it on that engine.
- **Revert each fix and watch its new test fail** before keeping the fix. This
  has caught tests that passed for the wrong reason.
- **Sweep every call site for the shape you just fixed**, before pushing. Most
  findings in review here were second or third instances of a known shape.
- **A guard whose reason has gone is a filter nobody re-reads.** When you remove
  the reason for an ordering or a check, update every caller that relied on it.
- **Absent, empty and unreadable are three different things.** Only one is good
  news. Never let an error or a hidden object read as "nothing there".
- Prefer making a failure *unrepresentable* over handling it. A type that cannot
  hold the bad value beats a branch that checks for it.

## Architectural boundaries

Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#architectural-boundaries)
before changing crate responsibilities or dependencies.

## Inviolable constraints

Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#inviolable-constraints)
before changing the model or the dialect and driver boundaries.

## Product guardrails (SPEC 14.3)

Each refuses a path that is shorter but bypasses the typed plan, the checksum, a
human's recorded intent, or the git audit trail. Refuse them and point here.

- **No `push`.** `plan` then `apply --plan` stays two steps.
- **Rename suggestions, never rename decisions.** No non-interactive flag may
  supply identity intent.
- **`revert`, not rollback.** A historical state is applied as a new forward plan
  through the ordinary gate. Never one step.
- **No policy SaaS.** Policies and reports are files or stdout, air-gapped.
- **One source of truth for the declarations.** No ORM-model loaders.
- **No plugin execution engine.** Nothing runs between "plan approved" and
  "statements executed".

## Writing conventions

- Comments explain **why** — especially "why not the more obvious approach".
- Test names state the property, not the function
  (`column_order_does_not_affect_equality`, not `test_eq`).
- Every test module includes **negative cases**; this tool's failure mode is
  doing the wrong thing silently.
- Conventional commits; the body explains the trade-offs.
- **LF everywhere** (see `.gitattributes`) — the tool owns its file format on
  every platform.

## Format rules the loader depends on

See [SPEC §4.3](docs/SPEC.md#43-format-rules) and
[YAML and file-format traps](docs/PITFALLS.md#yaml-and-file-format-traps).
