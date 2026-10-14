# Decisions

Every entry here was paid for. Each one records a choice that is not the obvious
one, and *why the obvious one is wrong* — which is the part that is expensive to
reconstruct and the reason this record is longer than a changelog.

Read the relevant topic before changing the data model, the permission checks,
the ledger, or anything a command's exit code depends on. `CLAUDE.md` carries
the rules; this carries the reasons.

Companion files: [PITFALLS.md](PITFALLS.md) for the bugs and traps found the
hard way, [STATUS.md](STATUS.md) for where the product currently stands,
[SPEC.md](SPEC.md) for the design, and `ADR-*.md` for the standalone records.

## Adding a decision

> **The identifiers are load-bearing.** Code comments cite them. Never renumber
> an entry, and never reuse one.

- Append the entry to the end of the topic file it belongs to — the one for the
  area where the rule is *enforced*, not every area it mentions. If none fits,
  add a topic file and a row to the table below in the same change.
- Name it after the issue its branch serves: `DEC-<issue>.<k>`, where `k`
  counts that issue's entries from 1. GitHub allocates issue numbers and one
  branch serves one issue, so two branches can never pick the same identifier,
  and an entry keeps its identifier from the moment it is written. The
  sequential numbers are closed: a branch that takes the "next" one
  collides silently with every other branch that does, in another file,
  where git reports no conflict.
- A new entry adds no row anywhere else: a `DEC-<issue>.<k>` entry is listed
  only in its topic file.
- Write it as an anchor and a paragraph that opens with the identifier and a
  bold one-sentence title:

  ```markdown
  <a id="dec-1234-1"></a>

  **DEC-1234.1. The choice, stated as a rule in one sentence.** Why the
  obvious approach is wrong, and what was measured to show it…
  ```

- Cite it as `DEC-1234.1`. `scripts/check-decisions.py` runs in CI; it refuses
  a duplicate identifier, an entry numbered in the closed sequence, and a
  citation of either form that names no entry.

## Topics

| File | Scope |
| --- | --- |
| [identity](decisions/identity.md) | How objects are identified across revisions: uids, the identity files, rename intents, and the rules a declared name must meet. |
| [plan](decisions/plan.md) | The saved plan: what it carries, how it is pinned to the apply, staged plans, checkpoints and `bootstrap`. |
| [apply-guard](decisions/apply-guard.md) | What `apply` checks after its statements run: postconditions, the read-back, and what the plan itself wrote. |
| [ledger](decisions/ledger.md) | The tables pbps keeps in the target, the lock, and the versions of the recorded state. |
| [drift](decisions/drift.md) | How `verify` and `status` compare the recorded state with the live one. |
| [diff](decisions/diff.md) | How a difference becomes an ordered list of changes and statements. |
| [data/declared](decisions/data/declared.md) | The `data:` block: what a declared cell may hold, how it is spelled, and how it is read back. |
| [data/row-writes](decisions/data/row-writes.md) | How a row insert, update or delete is guarded and held to what the plan recorded. |
| [data/pre-delete](decisions/data/pre-delete.md) | Counting the rows a delete would orphan or cascade into, before it runs. |
| [roles](decisions/roles.md) | Managed roles, what they are granted, and how permissions are compared and changed. |
| [doctor](decisions/doctor.md) | Which permissions `doctor` asks for, and on which securables. |
| [policies](decisions/policies.md) | The policy rules, their settings and suppressions, and what a refusal writes. |
| [cli](decisions/cli.md) | Exit codes, the findings envelope, published schemas, flags, and how messages address the operator. |
| [pull](decisions/pull.md) | Reading a database into declarations: what is read, what is omitted, and how omissions are reported. |
| [expressions](decisions/expressions.md) | Scanning, normalizing and comparing default and check expressions the way the engine reads them. |
| [types-and-probes](decisions/types-and-probes.md) | Type-change risk, the conversion and key probes that measure existing rows, and cost estimates. |
| [modules](decisions/modules.md) | Views, routines and triggers: identity, rebuilds, binding and ordering. |
| [rename-impact](decisions/rename-impact.md) | Finding what depends on an object and what a rename or drop would break. |
| [connection](decisions/connection.md) | The connection seam, the two drivers, TLS, and how database errors are reported. |
| [session](decisions/session.md) | The transaction framing, `search_path`, and the session settings every statement runs under. |
| [resolver](decisions/resolver.md) | Engine-assisted planning: resolver profiles, container admission, process and socket observation. |
| [compose-and-ui](decisions/compose-and-ui.md) | The local viewer and the compose flow that turns reviewed intent into a git branch. |
| [process](decisions/process.md) | CI, the merge queue, and how tests are arranged. |

## Entries 1–543

Entries numbered in one sequence, from before the record was split by topic,
are looked up by number in [the legacy index](decisions/legacy-index.md):
a citation of `DECISIONS <n>` or `decision <n>` names the entry it lists under
`<n>`. That table is closed and takes no new rows.
