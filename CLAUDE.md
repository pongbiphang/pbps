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

- Converse in **Chinese**. Write code, comments, docs, commit messages, issues
  and PR bodies in **English**.
- Tell me what you actually verified. Separate "I ran it and saw this" from "I
  reasoned this" every time, and never present the second as the first.
- When you are wrong, say so in one line and move on. Do not apologise at
  length, and do not re-litigate a decision I have already made.
- Report failures with their output. Say which steps were skipped.
- Do not tell me something is finished until it is.
- Treat quiet as quiet, not as convergence. A reviewer running out of credits is
  not a clean bill of health.
- Never merge. The merge decision is mine.
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
- One branch per issue, cut from `master`, named `fix/issue-<n>-<slug>`.
- Make every check below pass, then push the issue branch without asking. Never
  push to `master` or to another issue's branch.
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
- Stop after three consecutive reviews with no P1; a P1 resets the count. Count
  a review only if its `Reviewed commit:` is the pushed head. Never push a
  docs-only commit to move the count.
- On stopping: kill the watch, post no further `@codex review`, and report the
  count and every finding left unaddressed.
- Mark the draft ready only at merge time, as the last step. Never add "one more
  round" — more review is a new instruction.

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

## Development environment

- **Develop inside WSL** (Linux is the primary target); the project lives at
  `~/pbps`, not under `/mnt/c`.
- Do not attempt `x86_64-pc-windows-gnu`: rustup's mingw lacks a GNU assembler.
  Details in README.
- All three must pass before committing (a Markdown-only change is exempt):

```bash
cargo test --workspace
cargo clippy --workspace --all-targets    # must be warning-free
cargo fmt --all
```

- Touching dependencies also means `cargo deny check`. Every `ignore` entry must
  name the advisory, why it does not endanger the tool, and what removes it.
- Run the live tests when touching the emitter, the catalog queries, the ledger
  or the permission checks: `scripts/live-tests.sh` (set `PBPS_TEST_PORT` if
  14330 is taken, `PBPS_TEST_DEV_IMAGE` to include the `docker://` rehearsal).
- If `docker info` fails, check whether `dockerd` is merely **not started**
  before concluding the live tests cannot run.

## Architectural boundaries

```
pbps-model     Domain model. Dialect-agnostic, span-free, serializes to JSON:
               the ids file, the state snapshot, the saved plan, the drift report
pbps-config    Project configuration (pbps.yml): paths, environments, hooks
pbps-load      YAML -> model; the only crate that may depend on serde-saphyr
pbps-diff      model <-> ids comparison -> ChangeSet. Produces no SQL. Also owns
               the managed-set scope and observed identity
pbps-dialect   Dialect abstraction. Pure: types, validation, emit, preflight
               probes. Connection-bound work is free async fns in the dialect
               crate, not trait methods
pbps-mssql     SQL Server: type catalogue, validation, the T-SQL emitter (the
               only place a *change* becomes SQL), catalog introspection,
               the ledger/lock statements, rename impact
pbps-db        Connections (tiberius) plus transaction framing. Owns "there is
               a network"; ledger types and prune policy, no T-SQL. **`tiberius`
               is named in exactly one file** — `Row`, `FromColumn` and `Param`
               are this crate's own, so a driver change touches nothing else
pbps-docs      Markdown / self-contained HTML / Mermaid ERD from the model.
               Pure: no dialect, no connection, no configuration
pbps-cli       clap, diagnostic output, the deployment commands, exec hooks.
               `output` is the one typed findings envelope every read-only
               command speaks; `prompt` is the TTY intent channel of SPEC 6.3
```

- Only `pbps-db` and the `pbps-mssql` modules that take a `Conn` (`catalog`,
  `state`, `impact`, `edition`) are async; the CLI `block_on`s them per command.
- `spikes/` is workspace-`exclude`d: evaluation crates, not product code.

## Inviolable constraints

Each was paid for. Stop and think before breaking one; the reasoning is in
[docs/DECISIONS.md](docs/DECISIONS.md).

1. **Two semantically identical `Schema`s must be `==`.** No spans, no one-shot
   annotations in the model; normalize type case before comparing.
2. **Containers hold names; elements do not.** `Table` / `Column` have no
   `name` — it is the parent map's key. Functions needing one take `(name, table)`.
3. **SQL appears exactly once, in the dialect emitter.** Risk, gating and impact
   work on the typed `ChangeSet`, never on strings.
4. **Risk is data, not a method.** `intrinsic_risks()` answers only what needs no
   dialect knowledge; the differ attaches the rest.
5. **Serialization is deterministic.** `BTreeMap` / `BTreeSet` throughout. Sole
   exception: `Table::columns` is an `IndexMap` whose equality ignores order.
6. **Only rename and drop need human intent.** Never prompt non-interactively —
   fail with a copy-pastable command.
7. **Only data-bearing objects get identity.** Tables and columns have uids;
   modules have none (ADR-0002).
8. **Annotations travel beside the model, never inside it.** `strategy:` and
   `depends_on:` come back as `Loaded.hints`, or constraint 1 breaks.
9. **`tiberius` is named in exactly one file** (`pbps-db`), so the driver stays
   replaceable.

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

- **`null` cannot be a YAML key** (it is the null literal); the field is
  `nullable`.
- **`no` / `yes` / `on` / `off` parse as booleans**, so `pbps fmt` quotes
  boolean-ish, null-ish and number-shaped string scalars.
