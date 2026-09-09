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
- **[docs/DECISIONS.md](docs/DECISIONS.md)** — numbered record of every choice
  that is not the obvious one. Code comments cite these numbers; append, never
  renumber.
- **[docs/PITFALLS.md](docs/PITFALLS.md)** — bugs shipped or nearly shipped, and
  the shapes they belong to.
- **[docs/STATUS.md](docs/STATUS.md)** — phase, command surface, open items.
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
- Before every push, run locally what CI runs: `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets` warning-free, `cargo test --workspace
  --all-targets`, `scripts/live-tests.sh` and `scripts/live-tests-pg.sh`. All
  green, then push.
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
- In the draft phase, stop after three consecutive completed reviews with no P0
  or P1, or as soon as a completed review reports no findings. A P0 or P1 resets
  the count. Count a review only if its `Reviewed commit:` is the pushed head.
  Never push a docs-only commit to move the count.
- On stopping: kill the watch and post no further `@codex review`. Rebase onto
  `origin/master` if it moved, push, then mark the PR ready. That triggers the
  required code review; wait for it. A security review is optional: if run, its
  findings use the same triage rules, but its completion is not a gate. The
  rebase does not repeat the qualified draft gate, but the ready-phase code
  review must name the rebased current head.
- In the ready phase, a completed code review with no findings qualifies the PR
  for CI immediately. Otherwise, obtain three consecutive completed code
  reviews with no P0 or P1; P2/P3 findings do not reset the count, but each must
  be fixed or deferred under the finding rules. The latest counted review must
  name the current pushed head.
- A P0 or P1 in a ready-phase code review resets the count: address it under the
  finding rules (P0 is always fixed; P1 is fixed or deferred according to the
  three-case list), move the PR back to draft, and resume the loop.
- Once the ready-phase gate qualifies, start CI on the PR head with
  `gh workflow run ci.yml --ref <branch>` and wait for it. CI runs only when
  started; a push clears the checks.
- Red CI: fix it, push, and return to the loop as a draft.
- If `master` moves before merge, rebase and push the rebased head with
  `git push --force-with-lease`. A conflict-free rebase requires CI again on the
  new remote head before merge.
- If that rebase needs conflict resolution, resolve it, run the required local
  tests, push, and obtain a completed code review whose `Reviewed commit:` is
  the conflict-resolution head. P0 is always fixed; P1–P3 follow the same
  three-case finding rules, and P2 may be deferred directly to a linked
  `deferred-review` issue. A P0 or P1 must be addressed and followed by another
  completed resulting-head code review. Once that review has no P0 or P1, the
  PR may proceed without repeating the draft or ready three-review gates; all
  branch checks required on that head must still be green.
- Before merge, the primary agent independently verifies the issue-to-diff
  match, architecture, local-test evidence, review counts and heads, thread
  dispositions, dependency order, and required CI checks. Subagents never
  merge.
- The primary agent merges with a merge commit (`gh pr merge --merge`), deletes
  the branch, and removes the worktree only after every gate above passes.
- Never bypass the ruleset that requires green CI on the PR head.
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
- `ci.yml` runs nothing on `master` after a merge: the merged tree is the one
  CI passed. The dependency audit is separate and does run on `master` when the
  merge touched `Cargo.toml`, `Cargo.lock`, `deny.toml` or its own workflow —
  wait for it in that case, and a red audit is the next task, not the next
  issue.
- Take the next issue, following "Taking an issue", only when the user requested
  a continuous or multi-wave loop.

## How to be right here

- **Measure against a real engine before believing yourself.** Three times the
  obviously-correct answer was wrong and only a live server said so. When a
  question is about what SQL Server does, run it.
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
