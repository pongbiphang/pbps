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

SQL Server uses PR 2a's closed typed predicate parser, built-in allowlist and
version gates at recording, planning and apply. Columns resolve only within
the UID-selected holder, through the target's catalog collation. The engine's
major version and current compatibility level are required evidence. Only the
vetted tree regenerates SQL; the original text is retained for audit/checksum
and escaped diagnostics. Describe binds regenerated SQL without reading rows.
Referenced computed and CLR columns are refused in v1 because they could
introduce execution outside the closed expression vocabulary. Ordinary alias
types remain allowed. Refusals name the stored-column/non-CLR-type remedy and
the transition CHECK or planned assertion alternatives.

### SQL Server predicates without regular expressions

SQL Server drop requirements do not accept `REGEXP_*` functions
(DEC-2426.1). Use the allowlisted arithmetic and string functions for mappings
that they express: `REPLACE` for fixed punctuation, `CHARINDEX` with `LEFT` or
`SUBSTRING` for delimiter-based extraction, and `LOWER`, `UPPER`, `LTRIM` or
`RTRIM` for normalization. For example, a requirement can compare
`normalized_code = LOWER(LTRIM(RTRIM(old_code)))`. Function availability still
depends on the target engine and compatibility level.

These are not general regex substitutes. `LIKE` ranges follow the target
collation, so `[A-Z]` need not enforce case-sensitive ASCII; a fixed number of
`REPLACE` passes need not collapse an arbitrary run of whitespace.

For a mapping outside that vocabulary, add a transition CHECK and validate it
against existing rows. On SQL Server 2025, a whitespace-normalization mapping
can use this CHECK expression:

```sql
CHECK (
    source_text IS NOT NULL
    AND normalized_text IS NOT NULL
    AND normalized_text = REGEXP_REPLACE(LTRIM(RTRIM(source_text)), N'\s+', N' ')
)
```

The explicit NULL guards matter: a CHECK accepts UNKNOWN, while a drop
requirement demands TRUE. Keep the validated CHECK enabled through the contract
step, including writes after planning; do not remove it in an earlier deploy.
If the contract removes its source column or table, retire the CHECK with that
structure. The example requires native engine support: `REGEXP_REPLACE` works
on the measured 2025 build at compatibility 160 and 170; SQL Server 2022 has no
native regex functions. This does not admit regex into a drop requirement or
add an execution hook.

Plan-time assertions (#2401, planned) are another place for evidence outside
the row-predicate vocabulary. Their result describes the rows at planning time;
it does not protect against a violating write before the contract. Use the
transition CHECK when that interval needs continuous enforcement.

### Execution and locking

Immediately before removing the guarded structure, pbps generates the count:

```sql
SELECT count(*) FROM <table> WHERE (<predicate>) IS NOT TRUE
```

Both FALSE and NULL violate the requirement. A positive count refuses the
apply and rolls the transaction back. A failed query, unreadable count,
missing privilege or active row security refuses too; none means zero rows.
The refusal names the table, predicate and count when a count is readable.

The PostgreSQL count runs under an ACCESS EXCLUSIVE lock, taken directly on the table
and its partitions and held through the drop. Readers as well as writers wait
for the scan's duration. It uses apply's existing lock-wait and timeout
behavior, with no new timeout setting and no weaker-lock upgrade. The plan
and explanation state that the scan runs under this lock.

SQL Server counts `CASE WHEN (<regenerated predicate>) THEN 0 ELSE 1 END = 1`
with `COUNT_BIG(*)` and `WITH (TABLOCKX, HOLDLOCK)`. The count itself acquires
the table X lock, including all physical partitions, and the caller retains it
through DDL and commit. There is no preliminary lock query or weaker lock
upgrade. Keeping the hints on the count also avoids stale SNAPSHOT evidence.
Writers wait; versioned readers can proceed. No new timeout setting is added.
An absent or uncommittable caller transaction refuses.

Binding and counting use one `sp_executesql`-scoped pin block: `LANGUAGE
us_english`, `DATEFIRST 7`, `DATEFORMAT mdy`, `ANSI_NULLS ON`, `ANSI_WARNINGS ON`,
`ARITHABORT ON`, `ARITHIGNORE OFF`, `CONCAT_NULL_YIELDS_NULL ON` and
`NUMERIC_ROUNDABORT OFF`. Dates/weekdays, NULL/concatenation and arithmetic
error/rounding behavior must agree across connections. Caller settings restore
even on error. The regenerated identifiers use brackets; storage-only
ANSI_PADDING and target collation are not overridden.

Enabled SQL Server RLS FILTER policies are refused even for dbo. A database
VIEW DEFINITION grant and absence of effective object/schema metadata DENYs
must establish a complete policy catalog; unreadable is not empty. Disabled
and block-only policies do not hide rows and are allowed. A refusal explains
the policy-disable/permission remedy.

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
the new shape and merges after #2451's set 68. PR 2b connects the SQL Server
implementation without changing schema set 74, ids v1 or state v35.

## Validation

Live regressions on PostgreSQL 16 and 18 cover satisfied and NULL predicates,
a violating row arriving after plan, refusal before recording unsafe code or
another table, same-plan renames, checksum changes and a blocked writer that
cannot enter between the count and drop. Removing the late-row protection must
make its regression fail. Other protections receive their own revert checks.

SQL Server regressions additionally cover native capability/catalog binding,
scoped settings restored after success and error, filtered/hidden policies,
computed/CLR refusals, snapshot freshness and a database-observed blocked
writer in another physical partition through the emitted drop.
