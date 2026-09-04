# ADR-0008: The `policies:` block and the analyzer catalogue

- Status: accepted and built (Phase 4; see "Implementation status")
- Date: 2026-09-03
- Related: docs/SPEC.md §9.8, §13.3, §14.1 (the two P1 rows), §14.3;
  [ADR-0003](ADR-0003-execution-strategy.md);
  [ADR-0004](ADR-0004-reference-data.md);
  [ADR-0005](ADR-0005-roles-and-grants.md)

## Background

Built-in validation says what the engine will refuse and what the model cannot
express. It cannot say what *this organization* refuses: a naming convention,
a table that has grown past what a lookup table should be, a change that is
legal and unwise. Competitors answer with a rule catalogue (Bytebase's hundred
rules, Atlas's analyzers), and SPEC §14.1 ranks the gap P1 twice — once for the
organization's own rules, once for hazards the risk classes do not name.

SPEC §14.3 already draws the lines this design has to stay inside: declarative,
no embedded code, nothing runs between "approved" and "executed", no policy
outside git. What is left is the shape, and the shape decides data structures
that are expensive to change later. That is why this is an ADR before it is
code, as reference data and roles were.

## Decision 1: two evaluation points, one rule set

Some rules read the **declarations** (a column name that breaks the convention,
a `data:` block past its size); some read the **plan** (a narrowing, a revision
that both adds and drops in one table, a widening grant). Pretending they are
one kind would either run plan rules against a declaration that has no base,
or make `validate` compute a plan it was promised never to compute.

So `validate` evaluates the declaration rules and `plan` evaluates the plan
rules, both through the same `policies:` block, the same rule ids, the same
severities and the same suppressions. A rule declares which point it belongs
to; the block does not have to know.

## Decision 2: findings live beside risks, never inside them

`RiskClass` is the closed set the gate reads, and `--allow` names its members.
A policy finding needs an id, a severity that the project can change, and a
suppression — none of which a gate class has, and putting them there would
turn every lint into a flag. So `PlannedChange` gains a `findings` field
beside `risks`, and the findings envelope of §9.8 carries them with the ids
the block raises or lowers.

The split of §14.1 stays exactly as written: `Change::intrinsic_risks()` for
what needs no dialect, the dialect for what does, and **every analyzer works
on the typed `ChangeSet`**. A rule that inspected emitted SQL would be refused
on sight; that is constraint 3.

## Decision 3: what `error` blocks

Three severities — `error`, `warning`, `note` — mapping onto the three exit
codes of §9.8 exactly as the existing findings do: an `error` makes `validate`
exit 2, and makes `plan` refuse to *produce* a plan. **`apply` is not
touched.** It runs a checksum-pinned plan that already passed the gate, and a
policy that could stop it would be a second gate evaluated later than the one
that approved the artifact — the drift between "reviewed" and "run" that §7.3
exists to close. A plan an `error` refuses is never written, so there is
nothing for `apply` to disagree with.

`--allow` and policies do not meet: `--allow` names risk classes, a policy
`error` is fixed in the declarations or suppressed in git.

## Decision 4: suppression, and what expiry does

A suppression names a rule id, a reason, and optionally an expiry date; it
lives in the `policies:` block, scoped to an object name or to the project.
The reason is not decoration — it is the tombstone's `reason` again, the
field an audit reads, and a blank one is refused.

When the expiry passes, the suppression **stops applying and the finding comes
back at its configured severity**. It does not escalate to `error`: a rule the
project set to `warning` was a judgement about the rule, and a date passing
should not silently overturn it. The evaluation therefore depends on the
clock, and that is deliberate and documented: the same repository reports
differently on the day after the expiry, which is the whole point of writing
one. `--check` in CI is where it bites, by design.

## Decision 5: parameters, not code

Rules take **data** — a regular expression for a naming rule, a number for a
size rule, a time zone and a window for a change-window rule — and never an
expression or a script. The test is the one §14.3 already applies to the
execution engine: can the parameter change what runs, or only what is
reported? A regex cannot execute anything; an embedded expression language
would be the plugin engine by another name.

## Decision 6: the first catalogue

Declaration rules, evaluated by `validate`:

| id | Parameters | Default |
|---|---|---|
| `naming.table`, `naming.column`, `naming.index`, `naming.constraint` | regex | off |
| `data.max-rows` | rows | on, `warning` — the existing `max_data_rows` becomes this rule's parameter |
| `column.no-deprecated-type` | — | on, `warning` (`text`, `ntext`, `image`) |

Plan rules, evaluated by `plan`:

| id | Parameters | Default |
|---|---|---|
| `change.expand-contract` | — | on, `warning` — one revision both adds and drops or narrows in the same table (§13.3) |
| `change.narrowing-on-data` | — | on, `note` — a narrowing the preflight probe will measure at apply time |
| `grant.widen` | — | on, `note` — restates the `grant-widen` label as a finding a project can raise |
| `change.window` | time zone, allowed windows | off; **evaluated by `plan --db` only**, because it needs a clock and a target |

The change-window rule is the one that does not fit an offline `validate`, and
the answer is not to bend `validate` but to place the rule where a connection
and a moment already exist. Operational estimates (§14.1's "how long will this
take") are not in this catalogue: they need the target's row counts, and they
belong to the connected pre-flight, not to a policy.

## Decision 7: where the block lives, and what `--since` compares

The block is in `pbps.yml`. The SPEC says "block", the file already holds the
project's other judgements (`max_data_rows`, `unmanaged:`), and a second file
would be a second thing to find. A project that wants its policies reviewed
separately reviews the diff of one file.

`validate --since <rev>` evaluates the declaration rules only for objects whose
**identity** appears in the revisions' difference: the ids file at `<rev>` and
now are compared by uid, so a renamed table counts as changed under both its
names and a file moved between directories does not count at all. Comparing
file paths instead would make a directory reorganization re-lint the whole
estate and a rename lint nothing.

## Implementation status

Built: the `policies:` block in `pbps.yml` (`rules`, `suppress`), the
catalogue of Decision 6 in the `pbps-policy` crate, `validate` at the
declaration point with `--since <rev>` by identity, `plan` and `plan --db` at
the plan point with findings attached to each change and carried by the saved
plan into `explain`, refusal on `error` before anything is written, and the
editor schema regenerated from the config's own types.

Decisions taken during implementation:

1. **The block is checked before anything is evaluated.** A misspelled rule
   id, a parameter a rule does not take, an invalid pattern, a suppression
   with no reason or a bad date each produce a `policy.invalid` finding from
   `validate`, and `plan` refuses to evaluate a block with problems rather
   than run the half of it that parsed.
2. **`data.max-rows` replaced the `schema.data-large` finding**, with
   `max_data_rows` in `pbps.yml` as its default parameter. One rule, one id,
   one place to re-weight it.
3. **The change window is written with a UTC offset, never a zone name.** A
   zone database is the dependency being declined; `+08:00` says what it
   means without one. A window switched on with no `allow` list is refused
   by `validate`, because it would refuse every plan.
4. **Modules have no identity, so `--since` evaluates them every time.** No
   declaration rule reads a module today, so nothing is lost; the day one
   does, this is the sentence to revisit.
5. **The naming rules match the whole name.** A pattern is wrapped in
   `^(?:...)$`, so `[a-z_]+` refuses `CustomerId` rather than matching the
   `ustomer` inside it — the failure a lint that silently partially matched
   would never surface.
6. **`off` is a YAML boolean, and the block reads it as one.** `naming.table:
   off` parses as `false` (so would `on`, `yes` and `no`); the setting accepts
   the boolean `false` as off rather than making the operator quote the word.
   `true` is refused, because "on" says nothing about the severity — the
   rule is switched on by naming one.
7. **`pull --data` evaluates `data.max-rows` the way `validate` does.** The
   same function, over the pulled schema, narrowed to that rule: its
   severity, its row count and the project's suppressions apply at the
   moment the block is written exactly as on the next `validate`, and at
   `error` nothing is written (DECISIONS 111, 114, 120).

## Ruled out

- **Custom rules in v1**, in any language. The parameters above cover the
  rules the P1 rows name; anything past them gets its own ADR.
- **A policy that gates `apply`.** Decision 3.
- **Rules over emitted SQL.** Constraint 3; the analyzer catalogue is depth on
  the `ChangeSet`, not string inspection.
- **Fetching policies from anywhere but the repository.** §14.3.

## Placement

Phase 4, after roles. The block is additive to `pbps.yml` and the findings
field is additive to the plan, so nothing forces it earlier; the ADR exists
first because Decisions 2, 3 and 7 fix data structures.
