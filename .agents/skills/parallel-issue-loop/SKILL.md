---
name: parallel-issue-loop
description: Coordinate multiple pbps issues in parallel, assigning exactly one issue to each subagent while the primary agent verifies claims, manages dependencies, enforces the repository review loop, and owns every merge decision. Use when the user asks to select, claim, implement, review, or continuously process several pbps issues with subagents. Do not use for a single isolated PR review unless the user asks for the issue loop.
---

# Parallel Issue Loop

Run concurrent issue loops without giving up centralized ownership. The primary
agent owns selection, claims, coordination, final verification, and merges. Each
subagent owns exactly one issue at a time.

## Inputs and defaults

Read overrides from the user's invocation. Use these defaults when omitted:

- `agents`: 3
- `model`: `gpt-5.6-luna`
- `reasoning_effort`: `max`

`agents` means worker subagents and excludes the primary agent. Preserve one
session slot for the primary; if the host cannot run all requested workers at
once, batch the excess. Accept `reasoning`, `effort`, or
`model_reasoning_effort` as aliases for `reasoning_effort`. Validate the chosen
model/effort pair against the current host before spawning and never silently
substitute another pair.

When the user invokes this skill for parallel processing, that run supersedes
the older sequential "one active issue/PR at a time" convention. It does not
change the one-issue-per-subagent rule or silently alter future non-skill runs.

Treat these as spawn-time defaults, not global Codex configuration changes. Do
not edit `.codex/config.toml` merely to apply one invocation. If the requested
agent count exceeds the session concurrency limit, run issues in batches and
report the effective concurrency. If fewer eligible issues exist, spawn only
the number that can be safely assigned.

The default stopping condition is one wave: select at most `agents` eligible
issues, run those issues to a terminal state, and stop. Refill freed slots only
when the user requests a continuous loop, backlog drain, or another explicit
multi-wave condition. For a backlog drain, stop when a fresh issue/PR scan finds
no eligible issue; do not wait indefinitely for new issues.

This skill does not itself grant authority to mutate GitHub. Before claiming,
pushing, opening a PR, marking it ready, or merging, ensure the user's request
authorizes that stage. A request to run the full loop through merge authorizes
ordinary loop mutations, but never an admin/bypass merge unless the user
explicitly authorizes the bypass.

## Primary-agent responsibilities

1. Read the nearest `AGENTS.md` and the repository status/design documents that
   apply to each candidate. Confirm the current repository and branch before
   acting.
2. Inspect open issues and open PRs before selecting work. For every candidate,
   confirm immediately before claiming that it:
   - is still open;
   - has no assignee and no claim in comments;
   - is not already implemented or carried by an open PR;
   - still reproduces or remains applicable on the current base branch;
   - has known parent/sub-issue, blocked-by/blocking, and textual issue/PR links
     mapped and classified as prerequisite, dependent, or merely related.
3. Select up to the effective concurrency limit. Recheck claim state to reduce
   races, then have the primary agent post the claim comment. Never delegate the
   claim decision to a worker.
4. Create one isolated branch and git worktree per issue. Give each subagent its
   exact issue, worktree path, branch, dependency position, ownership boundary,
   review-loop requirements, and escalation instructions. Agents are not alone
   in the repository and must not revert or overwrite another agent's work.
5. Spawn one implementation-focused subagent per selected issue using the
   requested model and reasoning effort. A subagent must not take a second issue
   until its current issue reaches a terminal state and the primary agent assigns
   another one.
6. Maintain a coordination ledger containing issue, agent, branch/worktree,
   dependency edges, current head SHA, phase, review streak, tests, blockers,
   PR, and merge readiness. Report meaningful transitions and required user
   actions; do not present quiet, rate limits, or unfinished reviews as success.
7. Route questions between agents when issues overlap. A blocked subagent should
   first send the primary agent the exact blocker, evidence, attempted remedies,
   and decision needed. The primary agent may solve it, redirect ownership, or
   ask another relevant agent to discuss the interface. Ask the user only when
   the missing decision or authority cannot be safely inferred.

## Parallel dependency chains

Parallel work may include a dependency-connected set when every issue is open,
unclaimed, and not already carried by a PR. The primary agent must own the DAG
and merge order.

- Assign one issue per agent and explicitly identify upstream and downstream
  owners.
- Have affected agents agree on interfaces, invariants, and commit boundaries
  through the primary agent or direct agent messages. Record the decision in
  the coordination ledger.
- Keep worktrees isolated. For a strict code dependency, use a documented
  stacked-branch relationship: base the downstream branch and PR on the
  upstream branch so the downstream PR diff contains only downstream work. Use
  commit provenance for every shared change. If stacking cannot represent the
  dependency safely, pause that downstream issue and escalate to the primary
  agent; workers must not invent a temporary integration branch or copy
  unreviewed changes between worktrees.
- Merge in topological order. After an upstream merge, rebase or retarget each
  downstream branch onto the updated base and rerun required tests. A
  conflict-free rebase requires CI again on the new remote head. A rebase that
  needs conflict resolution follows the resulting-head code-review gate in the
  review reference. Do not automatically repeat the earlier draft/ready streak.
- Do not merge a downstream PR while its required upstream issue remains
  unmerged. Related issues without a true prerequisite may merge independently.

## Per-issue worker contract

Each worker must:

- stay inside its assigned worktree and issue scope;
- follow repository architecture, testing, language, and branch rules;
- inspect the full affected shape and sweep analogous call sites;
- add negative cases where applicable;
- for a bug fix, revert the production fix after writing the regression test,
  observe the new test fail for the expected reason, then restore the fix;
- run and report the required local verification, distinguishing executed facts
  from reasoning and explicitly reporting skipped tests and their reason;
- push only its assigned branch;
- escalate blockers rather than silently widening scope or taking another issue;
- follow the full review procedure in
  [references/review-loop.md](references/review-loop.md).

## Merge gate

Subagents must never merge. When a worker says its PR is ready, the primary agent
independently verifies the issue-to-diff match, architecture, tests, review state,
unresolved threads, current head SHA, dependency order, current-base status, and
CI evidence. Only the primary agent may merge, using a merge commit, and only
when every condition in the review reference is satisfied.

After a merge, verify the intended issue closed, capture the merge commit, clean
the issue worktree and local branch safely, update dependent agents, and report
the review count and any explicitly deferred findings. Continue filling free
slots only when the user's requested stopping condition permits it.
