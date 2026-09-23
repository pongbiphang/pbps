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
