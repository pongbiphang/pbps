# Drift

How `verify` and `status` compare the recorded state with the live one. Part of
the [decision record](../DECISIONS.md), which says how to add an entry here.

<a id="decision-9"></a>

9. **Drift compares the managed set only**; expressions are never parsed —
   after apply the DB's stored form is read back into `state_json`, and the
   differ side uses the dialect's lightweight normalization.

<a id="decision-20"></a>

20. **Drift needs `observed_ids`, not the recorded mapping on both sides.**
    `diff` matches by uid, so two sides sharing one ids file see only attribute
    changes; a hand-added or hand-dropped column would be invisible. The live
    side is identified by what is there, with `Uid::derived` (deterministic, so
    a hook payload is stable) for objects that have none. Never use `derived`
    to mint a real identity — two branches would collide.

<a id="decision-25"></a>

25. **`verify` exits 2 on drift**, and drift includes what the differ cannot
    phrase: `DriftReport.unexpressible` carries those, so the findings, the
    envelope and the **`on_drift` hook** all see them. A parallel path for them
    exited 2 correctly and skipped the hook, which is the one thing a scheduled
    drift-watch exists for. `unmanaged: error` is the same shape: the database
    was read successfully and the project's own policy refused it, so it is a
    finding (exit 2), not `environment.unreachable` (exit 1). Distinct from 1 for a tool failure: a
    scheduled drift-watch wakes different people for each. `status` always
    exits 0 — it is a report, and one unreachable environment must not cost
    the operator the other five lines.

<a id="decision-36"></a>

36. **`status` findings are warnings on purpose.** It always exits 0 (decision
    25), so an error-severity finding would make `result` disagree with the exit
    code. The per-environment truth is in `state`.

<a id="decision-44"></a>

44. **A drift report keeps both halves.** `diff` returns `Err(errs)` and throws
    away the changes it *had* expressed; `diff_partial` returns both, and
    `verify` uses it. `plan` keeps the `Result`: a plan that cannot express
    every difference must not be applied at all, and that is the one place the
    two callers differ.

<a id="decision-45"></a>

45. **`status` reads the lock even when the ledger is empty.** `state::lock`
    calls `ensure_tables`, so a lock held over an empty ledger is what a *first*
    `bootstrap` looks like while it runs — and what an interrupted one leaves
    behind. Not in the `NotInitialized` branch, though: `dbo.__pbps_state` being
    absent means nothing ever took a lock **by any path the tool controls** —
    but a hand-dropped state table leaves the lock behind, so both branches read
    it. What makes that safe on a first run is that `lock_holder` and `unlock`
    now attempt the statement and read the server's error number: **208**
    (invalid object name) means absent and answers "no lock", **229**
    (permission denied) and everything else stay errors. `unlock` had the same
    confusion and could not release a lock that outlived its state table.
    **Never ask `OBJECT_ID` instead**: metadata visibility hides an object from a
    principal with no permission on it, so it answers NULL for a lock table that
    exists and is held — turning "not authorized to look" into "no lock".
    `HAS_PERMS_BY_NAME` does not separate them either (measured: 0 for both).
    `DbError::server_error_code` (`server_error_number` until 193) exists so
    `pbps-mssql` can read the code without a second crate naming `tiberius`.

<a id="decision-125"></a>

125. **`status` reports a permission the declarations cannot hold as drift,
    as `verify` does.** 95 and 105 carried such a permission beside the
    schema, not in it, so `verify` could report it and every command that
    records a state could refuse it — and `status`, which computes the
    same checksum from the same schema, could not see it: a `DENY` on a
    managed role read "ok" on the status screen and "drift" from `verify`.
    `status` now applies `verify`'s own filter (a managed role's, not an
    unmanaged one's) to what introspection could not express, and records
    drift with the permission named, before the checksum it cannot enter.

<a id="decision-192"></a>

192. **`status` decides the row verdict before it writes the inventory, and
    lands a failed read last.** The third instance of the shape 159 and 168
    named: a check that ended the function hid every check after it. The row
    read was the last such return, and what followed it — the managed
    limitations, the unreadable modules, the objects `unmanaged: warn/error`
    sees — needs only the catalog, which had already succeeded, so a read that
    failed was reported *instead of* a stray object rather than beside it. The
    obvious fix, moving those checks above the read, changes what the row
    says on the ordinary path: `record_drift` yields to a state already on the
    row, so a row that moved beside an `unmanaged: warn` would have read
    "warning" with drift demoted to a supplemental issue. So the verdict is
    computed first, as a `Result<bool, String>`, and recorded in two places:
    drift immediately, keeping its rank over the warning; the failure at the
    very end, through `record_unreachable`, which keeps whatever is on the row
    and moves the previous primary state into the issues. The assembly is a
    sync function handed the read's result, so a test can hand it a failure;
    the two tests that do fail against the old return with exactly the
    missing finding.

<a id="dec-1804-1"></a>

**DEC-1804.1. Verify classifies an unanswered comparison at the failing step.**
An `anyhow` catch-all labelled missing and empty ledgers, damaged or newer
snapshots, and denied reference-row reads `environment.unreachable`, even
after the server had answered. `VerifyFailure` carries the ID and original
error and deliberately has no `From` conversion: each fallible step must
choose its classification. Only connecting uses `environment.unreachable`.
The ledger result distinguishes `state.uninitialized`, `state.no-entries`,
and `state.unreadable`; managed catalog and row reads use
`environment.unreadable`, and other operational comparison failures use
`verify.failed`. Policy findings and drift retain their existing exit-2
behavior. The typed ledger cause, target label, human error, and single JSON
envelope remain intact, with exit 1 for an unanswered comparison.

The boundary is the step, not an engine-message parser: finer permission and
snapshot-version causes remain the general diagnostic work in #1405. Real
PostgreSQL and SQL Server regressions cover absent/empty ledgers, malformed
and future snapshots, denied ledger and declared-row reads, wrong passwords,
and repaired successful reads; the refused-TCP control retains the connection
ID. Reverting the classification reproduces the mislabel without changing
the failures' underlying causes. Other commands' classifications are outside
this fix.

<a id="dec-2855-1"></a>

**DEC-2855.1. A kind a plan cannot yet change is decided once, in the model: one classification, a compared form, a recorded form, a buildable form and one drift report.**
Text search objects (DEC-2728.1) are held by name only: no identity record, and no
plan creates, alters or drops one. Every consumer of a whole `Schema` assumed
that everything in it has an identity and can be planned, and decided what to do
with such a kind at its own call site, with "treat it like a table" as the
default. On `master` that was already inconsistent before a second such kind
existed: a state recorded every live object (declared or not) while `verify`
compared only the recorded ones, so an undeclared object read as drift once it
was dropped; the checksums leave the kind out, so `status` and the `plan --db`
gate called an environment clean where `verify` named a dropped or changed
recorded object (#2846); the bootstrap emptiness check counted an undeclared
object; the rebuild comparison read an object only the catalog holds as an
"unrelated schema edit"; two copies of the whole-schema type normalization knew
no name-only kind. Patching each site (the first attempt for sequences was
fourteen call sites in one PR, and every review round found another) leaves the
next consumer and the next kind to repeat the mistake, because the compiler
cannot tell a consumer that has decided from one that has not.

The rule lives in `pbps-model` (`name_only.rs`), in the only functions that know
which fields of `Schema` are which:

- **One classification.** `Schema::classify` splits a `Schema` into the plannable
  kinds and the name-only kinds by exhaustive destructuring (no `..`). A new
  field fails to compile there until it is classified; the other functions are
  built on it, and nothing else enumerates kinds. A kind that gains plans moves
  out of `NameOnly` and every consumer follows.
- **Compared form** (`compared_with`; `pbps_dialect::compared::{compared, spelled}`):
  type spellings normalized for every kind, and a name-only object kept only where
  the other side names it, by key. A drift comparison projects the live side onto
  the record's names (`recorded_matches`); a comparison of peers projects each onto
  the other (`equal_as_compared`; the rebuild comparison). The projection needs no
  dialect and lives in `pbps-model`; the normalization needs one, and `pbps-model`
  cannot depend on `pbps-dialect`, so the wrapper is in `pbps-dialect`, the lowest
  crate that sees both. It replaces the two copies (`canonical_schema` in
  `pbps-diff`, `types_as_the_dialect_spells_them` in `pbps-cli`).
- **Recorded form.** A state records a name-only object only where the
  declarations name it (`as_recorded`, applied in `managed_state_once` to every
  recording from a read: `snapshot`, `baseline`, `bootstrap`). An apply and the
  staged checkpoint and closing entries carry the previous record forward
  (`as_recorded_after_apply`), since no plan can have changed one and what the
  read-back holds of it is not the plan's result. An object nothing names has no
  identity, so recording it would make dropping it drift.
- **Buildable form** (`buildable`): without the kinds no plan can build, for a
  build from nothing (`pbps_diff::bootstrap`, `lower_between`: the rehearsal and
  the resolver). The differ from nothing refused a held object by name before.
  The ordinary differ still refuses a declared one, so no plan claims to have
  made it.
- **One drift report** (`Schema::name_only_drift`): a recorded name-only object
  that is gone or changed, called by `verify` (as `unexpressible` drift, replacing
  `only_recorded_search_objects` and the differ's "arrives with a later part"
  wording), `status` and the `plan --db` drift gate. The checksums are unchanged,
  so no format version moves.

`refuse_pinned_movement` deliberately never reads a name-only kind: nothing a plan
promised can move one, and the carried record never takes what the read-back
holds. Serializers, per-kind validators, identity resolution and the role and
collation projections are not comparers, recorders or builders and are left alone.
`apply` itself does not run the drift report: a plan is pinned to a baseline the
`plan --db` gate has already held to it.

Adding a second name-only kind (sequences) is a field of `NameOnly`, a line in each
of its functions (the compiler lists them) and one row of the table-driven test in
`name_only/tests.rs`.
