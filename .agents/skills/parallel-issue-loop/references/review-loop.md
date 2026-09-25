# pbps Review Loop

Read and apply this file for every issue assigned by `parallel-issue-loop`. It
mirrors the review rules in the repository `AGENTS.md`; explicit user
instructions remain authoritative. If the two files drift, stop and reconcile
them before advancing a PR.

## 1. Implement and verify locally

1. Read the issue, relevant design records, current base implementation, and all
   affected call sites before changing code.
2. Implement the smallest complete fix. Preserve unrelated user or agent work.
3. Add positive and negative tests. For bug fixes, temporarily revert the
   production fix and observe the new regression test fail for the expected
   reason before restoring the fix.
4. Sweep every analogous site for the same bug shape before pushing.
5. Before every push, run and observe all commands required by `AGENTS.md`:

   ```bash
   python3 scripts/check_decisions_test.py
   python3 scripts/check-decisions.py
   cargo fmt --all --check
   cargo clippy --workspace --all-targets
   cargo test --workspace --all-targets
   scripts/live-tests.sh
   scripts/live-tests-pg.sh
   ```

   Run `cargo deny check` when dependencies change. If a required command cannot
   run or does not pass, report the command, failure output, and reason, then
   stop before committing or pushing until every required check runs and passes.
6. Commit with a conventional commit message, push only the assigned branch,
   and open or update a draft PR when authorized. Record the pushed head SHA.

## 2. Triage every finding

Push the round's fixes, verify the PR head is the commit just pushed, and post
`@codex review` once for the round, never once per commit. Inspect summaries,
inline comments, and threads; a summary alone is insufficient.

- A P0 finding is always fixed before the loop can advance.
- For P1–P3, fix a finding only if it is one of:
  1. a valid plan is refused;
  2. a wrong recording occurs with a single deployer and no concurrent writer;
  3. a concurrent case SPEC §7.6 promises to catch is not caught.
- Answer every other P1–P3 finding with the SPEC section or DECISIONS entry that
  made the choice. Open a follow-up with `gh issue create --label
  deferred-review`, title `review: ...`, and a body that links the review thread
  and names the proposed fix and its test. Change nothing in the current PR for
  that finding and add no DECISIONS entry for it.
- Except for mandatory P0 fixes, priority and fix size do not change this rule:
  a P1 outside the list is deferred, and a P3 inside it is fixed.
- Apply the same rules to findings discovered by an agent. Out-of-rule findings
  become new issues, never extra scope in the current PR.
- Reply to every fixed thread using exactly three facts: `Fixed in <sha>. <what
  changed>. <which test pins it>.`
- Reply to every deferred thread with its follow-up issue and disposition.
  Resolve each thread after replying; an open thread is unanswered.
- A running, failed, timed-out, quiet, rate-limited, or out-of-credits review is
  not complete and does not count.

## 3. Draft gate

Stop the draft loop through either route:

1. **No-findings shortcut:** one completed review reports no findings.
2. **No-P1 threshold:** three consecutive completed reviews report no P0 or P1.

A P0 or P1 resets the count. A round containing only P2/P3 does not, but every
finding still needs the disposition above. Count a review only when its
`Reviewed commit:` is the current pushed head. Never push a docs-only commit to
move the count.

When either route qualifies, kill the watch and post no further draft
`@codex review`, then mark the PR ready. Do **not** rebase merely because
`master` moved: the merge queue builds the PR against current `master`, so a
branch that is only behind needs nothing (DECISIONS 502). Rebase only to
resolve a conflict; that rebase does not repeat the qualified draft gate, but
the ready-phase code review must then name the resolution head. Never add one
extra draft round without a new user instruction.

## 4. Ready gate

Marking the PR ready triggers the required code review. Wait for it. A security
review is optional: if run, triage its findings by the same rules, but neither
its completion nor its result is a required ready-gate round.

Qualify for CI through either route:

1. **No-findings shortcut:** one completed ready-phase code review reports no
   findings.
2. **No-P1 threshold:** three consecutive completed ready-phase code reviews
   report no P0 or P1. P2/P3 findings do not reset the count, but each must be
   fixed or deferred under the finding rules.

The latest counted review must name the current pushed head. A P0 or P1 resets
the count: address it under the finding rules, move the PR back to draft, and
resume the draft loop. If a P2/P3 fix changes the head, review that new head;
do not reset the no-P1 count solely because of that lower-priority finding.

## 5. CI and base changes

A pull request's CI runs only when approved (DEC-1017.1): every push starts a
run that stops at the `approval` job, held by the `ci-approval` environment,
and runs nothing. Do not approve during the review loop. When the ready gate
qualifies the head — or the resulting-head review of a rebase does — report
it to the primary agent, which approves the run on that head through
`POST .../actions/runs/<run-id>/pending_deployments`; a worker never approves.
Then wait for `ci-gate` on the approved run. Retry a transient failure
with `gh run rerun <run-id>` (`--failed` for the failed jobs alone), which keeps
the run's pull-request association. Never use `gh workflow run ci.yml` for that
— it raises a `workflow_dispatch` event whose check suite belongs to no pull
request, so it goes green in the Actions tab while the required check stays
unsatisfied (DECISIONS 206, 501). A red CI result requires a fix, push, and
return to the draft loop.

`master` moving before merge needs no action: the queue rebuilds the PR against
current `master` when it is enqueued.

- If the queue ejects the PR for a conflict, resolve it, run the required local
  tests, push with `git push --force-with-lease`, and obtain a completed code
  review whose `Reviewed commit:` is the conflict-resolution head.
- P0 from the conflict-resolution review is always fixed. P1–P3 use the same
  three-case finding rules above; a P2 may be deferred directly to a linked
  `deferred-review` issue.
- A P0 or P1 in that review must be addressed and followed by another completed
  resulting-head code review. Once that review has no P0 or P1, proceed without
  repeating either three-review gate. All branch checks required on that head
  must still be green.

Never bypass the ruleset that requires green CI on the PR head.

## 6. Primary-agent merge gate

Before merge, the primary agent—not an issue worker—must independently confirm:

- the issue is still open and the PR closes the intended issue;
- the final diff is a complete solution limited to the issue;
- architecture and product guardrails are preserved;
- all required local tests actually ran and passed;
- the qualifying draft and ready reviews belong to the recorded pushed heads;
- every finding has the required fix or linked deferred issue;
- no review thread is unresolved;
- dependency merge order is satisfied;
- required CI checks are green on the mergeable PR head.

Subagents never enqueue and never merge. Once every gate passes the primary
agent enqueues with `gh pr merge --merge` — never with `--delete-branch`, which
`gh` refuses outright when a merge queue is required, because deleting the head
branch before the queue has merged closes the PR and drops it from the queue.
With a merge queue required that command **adds the PR to the queue** rather
than merging it: GitHub builds `master` plus everything queued ahead plus this
PR and runs `ci.yml` on the `merge_group` event, merging only if that is green
(DECISIONS 502). Wait for the merge to land before deleting the issue branch
and removing its worktree — a queued PR is not a merged one, and the queue can
still eject it. Delete the
branch on the **remote** as well as locally: GitHub retargets a PR stacked on it
onto `master` when the branch is deleted, not when it is merged, and nothing
deletes it here otherwise.

If the queue ejects the PR, read the failing job before re-queueing and say
which case it was. A conflict is handled in §5. A merge-group failure that
reproduces, or a test failing on its merits, is a real interaction with what
merged ahead — rebase onto current `master` so the combined tree is in your
hands, fix it there, and follow §5's resulting-head gate; do not re-queue. A
merge-group failure whose log
shows an infrastructure fault (the resource-shaped engine startup crashes
`ci.yml` and docs/PITFALLS.md record) is transient, and re-queueing is correct.
Re-queueing without reading is never correct: it is how a real interaction gets
merged on the second roll.

## 7. Closeout

After merge, verify the PR state, merge commit, and issue closure. Report the
draft and ready review counts, qualifying routes, merge commit, tests actually
run, tests skipped, every deferred finding and its issue, and cleanup status.
