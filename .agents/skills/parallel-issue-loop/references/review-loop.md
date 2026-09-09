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
`@codex review`. Rebase onto `origin/master` if it moved, push, and mark the PR
ready. The rebase does not repeat the qualified draft gate, but the ready-phase
code review must name the rebased current head. Never add one extra draft round
without a new user instruction.

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

After the ready gate qualifies, start CI on the remote PR head with
`gh workflow run ci.yml --ref <branch>` and wait. A push clears the checks. A
red CI result requires a fix, push, and return to the draft loop.

If `master` moves before merge, rebase and push the new head with
`git push --force-with-lease`:

- A conflict-free rebase requires CI again on the new remote head.
- If conflict resolution is required, resolve it, run the required local tests,
  push, and obtain a completed code review whose `Reviewed commit:` is the
  conflict-resolution head.
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

Subagents never merge. The primary agent uses a merge commit, deletes the issue
branch, and removes its worktree only after every gate passes.

## 7. Closeout

After merge, verify the PR state, merge commit, and issue closure. Report the
draft and ready review counts, qualifying routes, merge commit, tests actually
run, tests skipped, every deferred finding and its issue, and cleanup status.
