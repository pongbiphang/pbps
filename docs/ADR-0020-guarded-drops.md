# ADR-0020: Recorded row preconditions for drops

Status: Accepted design; implementation in progress (#2400, epic #1156).

## Context

A contract step can remove a column before every row has been copied into its
replacement. Ordinary narrowing, constraint and nullability probes cannot say
that `amount_cents = amount * 100` must hold when `amount` disappears. A count
at plan time also leaves a window for another writer to add a violating row.

## Decision

A column or table drop may carry a row predicate supplied with `--require`.
`-` reads the predicate from standard input. The predicate is recorded beside
the tombstone by uid, with the recording operator and time, and is covered by
the saved plan's checksum. Plan and apply cannot supply a replacement predicate.
The requirements of columns removed by a table drop are honored too.

`require` contains the owning `table_uid`, `predicate`, `required_at` and `operator`. Repeating
`--require` adds or replaces it; omission preserves it. `--clear-require`
removes it explicitly. Changing a historical tombstone requires an unambiguous
UID; it does not replace the original drop's reason, operator or date.

### Recording and vetting

Recording a guarded drop needs a target connection and vets the predicate
before writing the identity file. The desired declarations can already omit
the column or table, so the actual target supplies its column scope and types.
The selected retired UID resolves through the target baseline to its table UID.
Requirements and derived guards retain that UID; `PlanNames` resolves holders
at each operation, and dependency checks compare referenced column UIDs. A
rename follows the identity; reuse of its old address cannot match another
holder. Missing baseline identity evidence refuses without a name fallback.
The offline `convert` command records a conversion against its Git source
declaration and defers connected vetting; a drop requirement must instead be
refused when recorded, before any durable write, as acceptance test 3 requires.

PostgreSQL recording, planning and apply use one vetting function derived from
the recorded-conversion mechanism. It binds against the dropped table's own
columns, rejects subqueries and uses DEC-2181.1's bound-tree audit to admit only
built-in types and routines. DEC-2378.1's pre-binding protections and the empty
resolution path of DEC-2378.2 apply before binding. There is no second parser
or alternate security path for PostgreSQL predicates.

SQL Server's parser, built-in allowlist and version gates are a separate slice.
PR 2a supplies that core; PR 2b connects it to this recording and apply boundary.
Until that connection exists, the SQL Server consumer refuses guarded drops.

### Execution and locking

Immediately before removing the guarded structure, pbps generates the count:

```sql
SELECT count(*) FROM <table> WHERE (<predicate>) IS NOT TRUE
```

Both FALSE and NULL violate the requirement. A positive count refuses the
apply and rolls the transaction back. A failed query, unreadable count,
missing privilege or active row security refuses too; none means zero rows.
The refusal names the table, predicate and count when a count is readable.

The count runs under an ACCESS EXCLUSIVE lock, taken directly on the table
and its partitions and held through the drop. Readers as well as writers wait
for the scan's duration. It uses apply's existing lock-wait and timeout
behavior, with no new timeout setting and no weaker-lock upgrade. The plan
and explanation state that the scan runs under this lock.

### Consumers and limits

The approved requirement travels as typed drop metadata through apply,
rehearsal and plan-test paths. Every consumer executes the guard or refuses
explicitly. Standalone SQL export refuses guarded drops. A predicate naming a
column renamed by the same plan is refused with the split-the-plan remedy.
Rebuild-source identities are refused in v1. Referenced retypes/expression
changes and another referenced column drop require separate plans too. Staged
and unframed execution refuse guarded drops; transactional rehearsal executes
the same guard.

This is a check derived by pbps from recorded identity intent under SPEC 7.5;
it adds no user execution hook between approval and statements. It does not
introduce a built-in contract-verification language.

## Formats and delivery

Identity-file version 1 and the current state version 35 remain unchanged by this feature. Compatibility
tests retain their bytes when there is no requirement. Schema set 74 publishes
the new shape and merges after #2451's set 68. PostgreSQL ships in PR 1; the
issue stays open for the SQL Server integration.

## Validation

Live regressions on PostgreSQL 16 and 18 cover satisfied and NULL predicates,
a violating row arriving after plan, refusal before recording unsafe code or
another table, same-plan renames, checksum changes and a blocked writer that
cannot enter between the count and drop. Removing the late-row protection must
make its regression fail. Other protections receive their own revert checks.
