---
name: parallel-issue-loop
description: Coordinate multiple pbps issues in parallel under Claude Code, assigning exactly one issue to each subagent while the primary agent verifies claims, manages dependencies, enforces the repository review loop, and owns every merge decision. Use when the user asks to select, claim, implement, review, or continuously process several pbps issues with subagents. Do not use for a single isolated PR review unless the user asks for the issue loop.
---

# Parallel Issue Loop (Claude Code)

Run concurrent issue loops without giving up centralized ownership. You — the
primary agent — own selection, claims, coordination, final verification, and
merges. Each subagent owns exactly one issue at a time.

This is the Claude Code counterpart of
`.agents/skills/parallel-issue-loop/SKILL.md` (the Codex host). The rules are
the same; only the spawning and messaging mechanics differ. The review procedure
is shared, not duplicated: read
[.agents/skills/parallel-issue-loop/references/review-loop.md](../../../.agents/skills/parallel-issue-loop/references/review-loop.md).
If it and `AGENTS.md` drift, stop and reconcile them before advancing a PR.

## Inputs and defaults

Read overrides from the user's invocation. Use these defaults when omitted:

- `agents`: 3
- `model`: `sonnet` (Sonnet 5)
- reasoning: high — Claude Code has no `reasoning_effort` knob, so deliver it
  by putting `ultrathink` in each worker's spawn prompt.

`agents` means worker subagents and excludes you. If the requested count exceeds
what the host will run at once, run the issues in batches and report the
effective concurrency; finishing a later batch of the same selected set is
completion of the same wave, not a new wave. If fewer eligible issues exist,
spawn only the number that can be safely assigned.

Spawn each worker as:

```
Agent(subagent_type: "pbps-issue-worker", model: "sonnet",
      description: "issue <n>", prompt: "ultrathink\n\n<full assignment>")
```

`pbps-issue-worker` is defined in `.claude/agents/pbps-issue-worker.md` and
already pins `model: sonnet`; pass `model` explicitly only to override it on the
user's request. Never use `subagent_type: "fork"` here — a fork inherits this
conversation and ignores the model override, and a worker must start from the
assignment message alone. Put everything the worker needs in that message: the
issue number and full text, worktree path, branch name, dependency position,
ownership boundary, review-loop requirements, and escalation instructions.

Launch all workers for a batch in a single message so they run concurrently.
Continue an existing worker with `SendMessage` to its name or ID; a fresh
`Agent` call starts a new one with no memory of the assignment.

When the user invokes this skill, that run supersedes the sequential "one active
issue/PR at a time" convention in `AGENTS.md`. It does not change the
one-issue-per-subagent rule and does not alter future non-skill runs.

The default stopping condition is one wave: select at most `agents` eligible
issues, run them to a terminal state, and stop. Refill freed slots only when the
user asked for a continuous loop, a backlog drain, or another explicit
multi-wave condition. For a backlog drain, stop when a fresh issue/PR scan finds
no eligible issue; do not wait for new issues to appear.

This skill does not itself grant authority to mutate GitHub. Before claiming,
pushing, opening a PR, marking it ready, or merging, confirm the user's request
authorizes that stage. A request to run the full loop through merge authorizes
ordinary loop mutations, never an admin or bypass merge.

## Primary-agent responsibilities

1. Read `AGENTS.md` and the design records that apply to each candidate. Confirm
   the current repository and branch before acting. Fetch first: every worktree
   is cut from the latest `origin/master`.
2. Inspect open issues **and** open PRs before selecting. For every candidate,
   confirm immediately before claiming that it:
   - is still open;
   - has no assignee and no claim in comments;
   - is not already implemented or carried by an open PR;
   - still applies on the current base branch;
   - has its parent/sub-issue, blocked-by/blocking and textual issue/PR links
     mapped and classified as prerequisite, dependent, or merely related.
3. Select up to the effective concurrency limit. Recheck claim state to reduce
   races, then post the claim comment yourself. Never delegate the claim
   decision to a worker.
4. Create one branch and one git worktree per issue, `fix/issue-<n>-<slug>` cut
   from `origin/master`, under `.claude/worktrees/issue-<n>-<slug>`. Tell each
   worker that it is not alone in the repository.
5. Spawn one `pbps-issue-worker` per selected issue. A worker must not take a
   second issue until its current one reaches a terminal state and you assign
   another.
6. Keep a coordination ledger: issue, agent name, branch and worktree, dependency
   edges, current head SHA, phase, review streak, tests run and skipped,
   blockers, PR, merge readiness. Report meaningful transitions and any action
   the user must take. Never present quiet, rate-limited or unfinished reviews
   as success.
7. Route questions between workers when issues overlap. A blocked worker sends
   you the exact blocker, evidence, attempted remedies and the decision needed;
   you solve it, redirect ownership, or have two workers agree on the interface
   through you. Ask the user only when the missing decision or authority cannot
   be safely inferred.

Use `ListAgents` to recover worker names, and `SendMessage` to steer a worker
that is drifting, to deliver a dependency decision, or to hand it the result of
another worker's interface change.

## Parallel dependency chains

Parallel work may include a dependency-connected set when every issue is open,
unclaimed, and not already carried by a PR. You own the DAG and the merge order.

- Assign one issue per agent and name the upstream and downstream owners.
- Have affected workers agree on interfaces, invariants and commit boundaries
  through you. Record the decision in the ledger.
- Keep worktrees isolated. For a strict code dependency, use a documented
  stacked branch: base the downstream branch and PR on the upstream branch so
  the downstream diff carries only downstream work. Use commit provenance for
  every shared change. If stacking cannot represent the dependency safely, pause
  that downstream issue and decide yourself; workers must not invent an
  integration branch or copy unreviewed changes between worktrees.
- Merge in topological order. After an upstream merge, **rebase** each
  downstream branch onto the updated `master`, push with
  `--force-with-lease`, and rerun the required tests. Retargeting the PR base
  is not an alternative: it leaves the downstream head untested against the new
  base, and because `ci-gate` is a commit status on that head, the stale green
  from before the upstream merge survives the retarget. A conflict-free rebase
  requires CI again on the new remote head; a rebase that needs conflict
  resolution follows the resulting-head code-review gate in the review
  reference. Do not automatically repeat the draft or ready streak.
- Never merge a downstream PR while its required upstream issue is unmerged.
  Related issues without a true prerequisite may merge independently.

## Per-issue worker contract

Each worker must: stay inside its worktree and issue scope; follow the
repository's architecture, testing, language and branch rules; inspect the full
affected shape and sweep analogous call sites; add negative cases; for a bug fix
revert the production fix, watch the new regression test fail for the expected
reason, then restore it; run and report the required local verification,
separating executed facts from reasoning and naming every skipped test and its
reason; push only its assigned branch; escalate blockers instead of widening
scope or taking another issue; and follow the review procedure in the shared
review reference.

## Local verification and the live suites

Workers run, before every push:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo test --workspace --all-targets
scripts/live-tests.sh
scripts/live-tests-pg.sh
```

The live suites need a database container, and concurrent workers sharing one
container make each other fail. Serialize them through you: hold a single live
suite token and hand it to one worker at a time. A separate port per worker is
**not** enough — `scripts/live-tests.sh` and `scripts/live-tests-pg.sh` hardcode
the container names `pbps-test-mssql` and `pbps-test-pg` and reuse an existing
container as-is, so a second worker with its own port silently connects to a
port nothing is listening on, and two simultaneous starts race on
`docker run --name`. Until those scripts take a container name as well as a
port, serialization is the only safe arrangement.

A worker that cannot run a required command reports the command and the reason
and stops before pushing — it never reports a skipped suite as green.

## Merge gate

Workers never merge. When a worker says its PR is ready, verify independently:
the issue-to-diff match, architecture and product guardrails, that the required
local tests actually ran and passed, that the qualifying draft and ready reviews
belong to the recorded pushed heads, that every finding has a fix or a linked
deferred issue, that no review thread is unresolved, that dependency order is
satisfied, and that the required CI checks are green on the mergeable head.

Only you merge, with `gh pr merge --merge`, and only after every gate passes.
Then delete the branch, remove the worktree, and confirm the intended issue
closed.

If the merge touched `Cargo.toml`, `Cargo.lock`, `deny.toml` or the
dependency-audit workflow, wait for the dependency audit; a red audit is the
next task, not the next issue. Otherwise refill a free slot only if the user's
stopping condition permits it.

## Closeout report

Per merged issue: the draft and ready review counts, the qualifying route, the
merge commit, the tests actually run, the tests skipped and why, every deferred
finding with its issue number, and cleanup status.
