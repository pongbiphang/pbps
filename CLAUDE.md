# CLAUDE.md

Guidance for Claude working in this repo. Rules here; reasons in `docs/`.

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
  --all-targets`, and `scripts/live-tests.sh`. All green, then push.
- Push the issue branch without asking. Never push to `master` or to another
  issue's branch.
- Open the PR as a **draft**, with `Closes #<n>` in the body. Taking the issue is
  permission to open it.
- Post `@codex review` as soon as the draft is open.

## The review loop

- Push the round's fixes, verify the PR head is the commit you pushed, then post
  `@codex review` once. Never per commit.
- Fix a finding only if it is one of: **(a)** a valid plan is refused; **(b)** a
  wrong recording with a single deployer and no concurrent writer; **(c)** a
  concurrent case SPEC §7.6 promises to catch and does not.
- Answer every other finding with the SPEC section or DECISIONS entry that made
  the choice, and open an issue for it: `gh issue create --label
  deferred-review`, title `review: ...`, body linking the thread and naming the
  fix and its test. Change nothing in the repo for an answered finding, and add
  no DECISIONS entry for one.
- Ignore the badge and the size of the fix. A P1 outside the rules is answered;
  a P3 inside them is fixed.
- Triage findings you make yourself the same way. Out-of-rule ones become new
  issues, never extra scope on this PR.
- Reply on each fixed thread yourself: "Fixed in `<sha>`. <what changed>.
  <which test pins it>." Those three facts, nothing else.
- Stop after three consecutive reviews with no P1, or as soon as a review
  reports no findings; a P1 resets the count. Count a review only if its
  `Reviewed commit:` is the pushed head. Never push a docs-only commit to move
  the count.
- On stopping: kill the watch, post no further `@codex review`, mark the PR
  ready, and start CI on its head: `gh workflow run ci.yml --ref <branch>`.
  Ready triggers one more review; wait for both.
- CI does not run on pushes. A push clears the checks; start CI again.
- Red CI: fix it, push, back to the loop as a draft.
- No P1 and green CI: merge with a merge commit (`gh pr merge --merge`),
  delete the branch, remove the worktree. A P1: the count resets — back to the
  loop, as a draft again.
- Never bypass the ruleset that requires green CI on the PR head.
- Report at each merge: the review count, the merge commit, and every finding
  deferred to an issue.
- Never add "one more round" — more review is a new instruction.

## The issue loop

- One issue at a time. Claim the next only after the current PR is merged.
- Wait for CI on `master` to pass after the merge. Red CI is the next task, not
  the next issue.
- Green CI: take the next issue, following "Taking an issue".

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
