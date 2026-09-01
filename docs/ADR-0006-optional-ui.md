# ADR-0006: The optional UI — a viewer that commits, not a server that decides

- Status: decided (design; implementation targeted at Phase 6)
- Date: 2026-09-01
- Related: docs/SPEC.md §7.3, §8.1, §10, §12, §14.1, §14.3;
  [ADR-0003](ADR-0003-execution-strategy.md)

## Background

Bytebase is the reference implementation of the other answer in this category:
deploy a server, and database change becomes an issue in its web UI, with SQL
review rules, an approval queue, drift detection and an audit log. Teams like
it. The reviews say the same thing repeatedly — it makes database change
"less scary", and it reaches people who do not live in a terminal.

That last part is a real gap here, not a competitor's marketing. The people who
must understand a schema change — a DBA signing off on a lock, an auditor
asking what happened to a column, a release manager checking whether prod is
mid-deployment — are not all CLI users. pbps currently answers all three
questions well and answers them **only** to someone holding a terminal.

SPEC §14.3 already refuses "no policy SaaS dependency". Read carelessly, that
reads as "no UI, ever". It is not what the refusal was about, and leaving the
ambiguity in place would cost this project either a capability it should have
or the boundary it must keep.

## The distinction that matters

Bytebase's weakness is not that it has a GUI. It is that **the GUI is the
source of truth**: the approval, the policy and the history live in Bytebase's
own database. Turn the server off and the audit trail is gone with it. That is
what makes it a second system of record — and a second gate, in §14.3's sense,
because a policy stored outside git can change an outcome without anyone
reviewing the change.

pbps's thesis is the inverse, and it is already built:

| The question | Where pbps already keeps the answer |
|---|---|
| What is the schema meant to be? | The declaration files, in git |
| Who decided this column is that column? | The ids file, in git, reviewed in the MR |
| Why was this dropped? | The tombstone's reason, in git |
| Exactly what will run? | The saved plan, pinned by checksum (§7.3) |
| What actually ran, where, and when? | `__pbps_state` plus the CI logs (§8.1) |

So the audit trail Bytebase had to build, pbps has. What it lacks is a way to
**look at it** without a terminal. A UI is therefore a presentation layer over
capability that already exists — which is a far cheaper position to build from
than the one the competition is defending.

## Decision

A UI is admissible, as a **later, optional, local companion**, under one
constraint:

> **Every action the UI takes ends as a git commit or as an ordinary CLI
> invocation, and the UI stores no authoritative state of its own.**

Concretely, three things it may do and one it may not:

- **Render.** Plan summaries, the drift report, the rename impact, the ERD, the
  ledger timeline, `status` across environments. All of these are already typed
  JSON — that is what Phase 3.1's single output format is for — so the UI is a
  viewer, and 14.2's acceptance criterion 4 (a frontend never reimplements
  validation) is satisfied structurally rather than by discipline.
- **Compose.** Help a human record intent — a rename, a drop reason, a
  `strategy:` annotation — where the *output is a file edit plus a commit*,
  landing in the merge request like any other. This keeps 14.3's rename rule
  intact: the UI is a prompt with a mouse, one pair at a time, and what survives
  is the same one line in the ids file.
- **Trigger.** Run `plan --db` or `apply --plan`, passing the same
  checksum-pinned plan file the CLI would. The gate is unchanged because the
  artifact is unchanged.
- **Never hold the approval.** Approval stays where the organization already
  keeps it: the MR approval, or the CI environment protection rule. The UI may
  *display* that state and link to it. An approval recorded in the UI's own
  storage would be exactly the second gate §14.3 refuses, and — worse here —
  the one an auditor cannot reach from git.

**Local-first, single-user — and that is the open-source scope.** It ships as
`pbps ui`, serving on loopback, taking credentials from the same environment
variables the CLI uses (§8.1, and the `url_env:` rule that no connection string
is ever written down).

The line is drawn here deliberately, and it is a product decision as well as a
technical one: **the single-user local UI belongs to the open-source core; a
multi-tenant or hosted deployment is explicitly out of that scope and reserved
as a possible commercial offering.** Two things follow, and both matter more
than the licensing question.

The first is that the open-source promise stays complete without the commercial
one. Everything this ADR admits — rendering the typed JSON, composing intent as
a commit, triggering the checksum-pinned plan — works for one person on one
machine, with no server, no account and no network beyond the database. A user
who never buys anything is not left holding a demo.

The second is that the constraint above does not become negotiable at the
boundary. A multi-tenant deployment needs standing credentials to every
environment, which is the concentration §8.1's trust model exists to avoid, and
commercial pressure pushes hardest in exactly the direction this ADR refuses —
towards the UI holding the approval, because that is what an approval queue is
for. **Whatever is built there gets its own ADR and re-argues that point from
scratch; it does not inherit permission from this one.** Recording it now is
cheap; discovering it after the fact, when a customer has already been sold the
queue, is not.

## Why "git built in" is the right shape

The intended user is a DBA who is not going to run five commands and a `git
push`. Bundling a git client — see the diff, record the intent, commit, push,
open the merge request, all without leaving the window — gives that person the
Bytebase experience while the **artifact remains a commit the platform team
already reviews**. It is the "GitLab for database change" pitch with the second
system of record deleted, and the audit story gets stronger rather than weaker:
what an auditor is shown is a signed commit and a CI log, not a row in a vendor's
table.

This is the strongest version of the idea available to us, and it is only
available because the CLI got built first, in this order.

## Placement, and why it is late

Phase 6, after the typed JSON of Phase 3.1 and after Phase 4 has stopped the
format moving. Building it earlier forces the UI to parse human output or
reimplement checks, which is the failure 14.2 names. There is no partial
version worth shipping sooner: a UI over an unstable format is a second
implementation of the format.

## Ruled out, and the line to hold when it is asked for again

A UI attracts requests, and each of these is a step back toward being the server
this ADR declines to be:

- **Approvals, users, roles and permissions inside the UI.** Refused — the
  reason above. The identity that matters is the git identity.
- **A scheduler.** Refused: CI already runs the drift watch (§9.4, §10), and a
  second scheduler makes "what ran" ambiguous.
- **Editing the plan, or executing anything between approval and apply.**
  Refused by §14.3's plugin-engine test, unchanged — the checksum must keep
  describing what runs.
- **Direct SQL execution against a target** (the "SQL editor" every competitor
  has). Refused: it is the shortest path around the declarations, and everything
  it changes becomes drift the next plan tries to remove.
- **Hosting it, or serving more than one person.** Not refused, and now
  positioned — it is where a commercial offering would sit rather than part of
  the open-source core — but not designed here. Standing credentials to N
  environments is a different trust model, not a deployment detail, and the
  refusal above (the UI never holds the approval) has to be re-argued on its own
  terms in that ADR rather than carried over as settled.
