# Partition-move measurements for #2296

Recorded before the effect contract (DEC-2296.1), against source inventory
`875e37a6fe58944d100eee64aaa637a992197f18`. PostgreSQL 16.15 and 18.6;
44 fixtures, 300 two-order pairs, 600 executions, zero setup/recording errors.
Every experiment uses a fresh schema inside BEGIN/ROLLBACK; failures are
caught in subtransactions and their SQLSTATE retained. No pbps plan/apply
success is asserted by this native SQL matrix.

Each cell is **parent change first / move first**. A means both statements
in that execution accepted; its reverse need not accept. Rxxxxx means a
statement refused with that SQLSTATE. D means both orders accepted but their
normalized catalog/rows/default INSERT probes differ. Candidate profiles
select baseline/final column shape or the named negative control.

The shared shape is parent p(k,d), partition b covering [0,10), and candidate
a covering [10,20). For attach a starts standalone; for detach/drop it starts
attached. Drop is the emitter's atomic DETACH+DROP statement group. Bad-row,
identity, owned-NOT-NULL and neither-shape controls are kept separate.
Snapshots retain names, physical column order, type/collation/nullability,
identity/generation/defaults, clone lineage, membership and rows, with OIDs
and per-run schema names excluded. Actual default INSERTs and parent-tree
(k,d) duplicate-group probes supply independent observations. These counts
are measurement evidence, not durable test-count or engine-version policy.

| Change payload/control | PG16 attach | PG16 detach | PG16 drop | PG18 attach | PG18 detach | PG18 drop |
| --- | --- | --- | --- | --- | --- | --- |
| column-add (`AddColumn`) | base: R42804/A; final: A/R42804 | member: D/D | member: A/A | base: R42804/A; final: A/R42804 | member: D/D | member: A/A |
| column-add-generated (`AddColumn`) | base: R42804/A; final: A/R42804 | member: D/D | member: A/A | base: R42804/A; final: A/R42804 | member: D/D | member: A/A |
| column-add-identity (`AddColumn`) | base: R42P16/R42P16; final: R42P16/R42804 | member: R42P16/R42P16 | member: R42P16/R42P16 | base: R42804/A; final: R55000/R55000 | member: D/D | member: D/D |
| column-drop (`DropColumn`) | base: R42804/A; final: A/R42804 | member: D/D | member: A/A | base: R42804/A; final: A/R42804 | member: D/D | member: A/A |
| column-rename (`RenameColumn`) | base: R42804/A; final: A/R42804 | member: D/D | member: A/A | base: R42804/A; final: A/R42804 | member: D/D | member: A/A |
| type-widen (`AlterColumnType`) | base: R42804/A; final: A/R42804 | member: D/D | member: A/A | base: R42804/A; final: A/R42804 | member: D/D | member: A/A |
| type-collation (`AlterColumnType`) | base: R42P21/A; final: A/R42P21 | member: D/D | member: A/A | base: R42P21/A; final: A/R42P21 | member: D/D | member: A/A |
| type-narrow-bad-row (`AlterColumnType`) | base: R42804/R22003 | member: R22003/A | member: R22003/A | base: R42804/R22003 | member: R22003/A | member: R22003/A |
| not-null-set (`AlterColumnNullability`) | base: R42804/A; final: A/A | member: D/D | member: A/A | base: R42804/A; final: D/D | member: D/D | member: A/A |
| not-null-set-bad-row (`AlterColumnNullability`) | base: R42804/R23502 | member: R23502/A | member: R23502/A | base: R42804/R23502 | member: R23502/A | member: R23502/A |
| not-null-drop (`AlterColumnNullability`) | base: D/D; final: A/R42804 | member: D/D | member: A/A | base: D/D; final: A/R42804 | member: D/D | member: A/A |
| not-null-drop-own (`AlterColumnNullability`) | base: D/D | member: D/D | member: A/A | base: D/D | member: A/A | member: A/A |
| default-set (`AlterColumnDefault`) | base: D/D | member: D/D | member: A/A | base: D/D | member: D/D | member: A/A |
| default-drop (`AlterColumnDefault`) | base: D/D; final: D/D | member: D/D | member: A/A | base: D/D; final: D/D | member: D/D | member: A/A |
| identity-add (`AlterColumnNumbering`) | base: A/A; final: A/A | member: A/A | member: A/A | base: A/A; final: R55000/R55000 | member: A/A | member: A/A |
| identity-drop (`AlterColumnNumbering`) | base: A/A; final: A/A | member: A/A | member: A/A | base: R55000/R55000; final: A/A | member: A/A | member: A/A |
| identity-always (`AlterColumnNumbering`) | base: A/A; final: A/A | member: A/A | member: A/A | base: R55000/R55000; final: R55000/R55000 | member: A/A | member: A/A |
| identity-by-default (`AlterColumnNumbering`) | base: A/A; final: A/A | member: A/A | member: A/A | base: R55000/R55000; final: R55000/R55000 | member: A/A | member: A/A |
| sequence-add (`AlterColumnNumbering`) | base: D/D | member: D/D | member: A/A | base: D/D | member: D/D | member: A/A |
| sequence-drop (`AlterColumnNumbering`) | base: R2BP01/A | member: A/R2BP01 | member: A/A | base: R2BP01/A | member: A/R2BP01 | member: A/A |
| sequence-to-identity (`AlterColumnNumbering`) | base: R2BP01/A | member: A/R2BP01 | member: A/A | base: R2BP01/A | member: A/R2BP01 | member: A/A |
| identity-to-sequence (`AlterColumnNumbering`) | base: A/R42601 | member: D/D | member: A/A | base: R55000/R55000 | member: D/D | member: A/A |
| index-add (`AddIndex`) | base: A/A | member: A/A | member: A/A | base: A/A | member: A/A | member: A/A |
| index-drop (`DropIndex`) | base: D/D | member: D/D | member: A/A | base: D/D | member: D/D | member: A/A |
| unique-index-add (`AddIndex`) | base: A/A | member: D/D | member: A/A | base: A/A | member: D/D | member: A/A |
| unique-index-add-bad-row (`AddIndex`) | base: R23505/R23505 | member: R23505/A | member: R23505/A | base: R23505/R23505 | member: R23505/A | member: R23505/A |
| expression-index-add (`AddIndex`) | base: A/A | member: D/D | member: A/A | base: A/A | member: D/D | member: A/A |
| primary-key-add (`SetPrimaryKey(Some)`) | base: A/A | member: D/D | member: A/A | base: A/A | member: D/D | member: A/A |
| primary-key-add-bad-row (`SetPrimaryKey(Some)`) | base: R23505/R23505 | member: R23505/A | member: R23505/A | base: R23505/R23505 | member: R23505/A | member: R23505/A |
| primary-key-drop (`SetPrimaryKey(None)`) | base: D/D | member: D/D | member: A/A | base: D/D | member: D/D | member: A/A |
| unique-add (`AddUnique`) | base: A/A | member: D/D | member: A/A | base: A/A | member: D/D | member: A/A |
| unique-add-bad-row (`AddUnique`) | base: R23505/R23505 | member: R23505/A | member: R23505/A | base: R23505/R23505 | member: R23505/A | member: R23505/A |
| unique-drop (`DropUnique`) | base: D/D | member: D/D | member: A/A | base: D/D | member: D/D | member: A/A |
| check-add (`AddCheck`) | base: R42804/A; final: A/A | member: D/D | member: A/A | base: R42804/A; final: A/A | member: D/D | member: A/A |
| check-add-bad-row (`AddCheck`) | base: R42804/R23514 | member: R23514/A | member: R23514/A | base: R42804/R23514 | member: R23514/A | member: R23514/A |
| check-drop (`DropCheck`) | base: D/D; final: A/R42804 | member: D/D | member: A/A | base: D/D; final: A/R42804 | member: D/D | member: A/A |
| foreign-key-add (`AddForeignKey`) | base: A/A | member: D/D | member: A/A | base: A/A | member: D/D | member: A/A |
| foreign-key-add-bad-row (`AddForeignKey`) | base: R23503/R23503 | member: R23503/A | member: R23503/A | base: R23503/R23503 | member: R23503/A | member: R23503/A |
| foreign-key-drop (`DropForeignKey`) | base: D/D | member: D/D | member: A/A | base: D/D | member: D/D | member: A/A |
| table-rename (`RenameTable`) | base: A/A | member: A/A | member: A/A | base: A/A | member: A/A | member: A/A |
| table-set-schema (`RenameTable`) | base: A/A | member: A/A | member: A/A | base: A/A | member: A/A | member: A/A |
| partition-key-drop-control (`DropColumn(key)`) | base: R42P16/R42P16 | member: R42P16/R42P16 | member: R42P16/R42P16 | base: R42P16/R42P16 | member: R42P16/R42P16 | member: R42P16/R42P16 |
| partition-key-type-control (`AlterColumnType(key)`) | base: R42P16/R42P16 | member: R42P16/R42P16 | member: R42P16/R42P16 | base: R42P16/R42P16 | member: R42P16/R42P16 | member: R42P16/R42P16 |
| metadata-noop-control (`SetColumnDeprecated`) | base: A/A; reordered: A/A; neither: R42804/R42804 | member: A/A | member: A/A | base: A/A; reordered: A/A; neither: R42804/R42804 | member: A/A | member: A/A |

The artifact set retained by the maintainer is `issue-2296-evidence/`:
`measure.py`, `summarize.py`, `cases.json`, `report.json`, `metadata.json`,
and the exact per-case SQL/stdout/stderr. The hashes below identify the
original evidence; this appendix does not claim the raw files were posted
to GitHub or that the matrix itself tests the product compiler.

- `measure.py`: SHA-256 `a5a2588d662aa7b06230575b4a65004d91ae15fb8d2b502717feca9bf8bc03d0`.
- `cases.json`: SHA-256 `7678eb498cae20029e42b1969d7ad4adbad30cae4c0ab01304b4dec4c6597b98`.
- `report.json`: SHA-256 `abbd9bd9ba2df9b6b78c0ad8759d01713cccc4cd6ee5bd987b008e86606ab1ba`.
