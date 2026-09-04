# ADR-0011: The dialect seam under a second engine — what Phase 0 got right, and three amendments

- Status: proposed. Phase 5 design; nothing is built.
- Date: 2026-09-04
- Related: docs/SPEC.md §11.2, §12; `crates/pbps-dialect/src/lib.rs`;
  [ADR-0009](ADR-0009-postgres-modules.md),
  [ADR-0010](ADR-0010-postgres-privileges.md),
  [ADR-0003](ADR-0003-execution-strategy.md)

## The question

SPEC §12: *"If Phase 5 forces a large change to `pbps-model`, the Phase 0
abstraction was drawn in the wrong place."* ADR-0009 and ADR-0010 answer that
for the **model**, and the answer is no — a map key, an enum, and one field in
the state snapshot. That third item is not decoration: without it the differ
compares a hand-written definition with a deparsed one, and every view and
`BEGIN ATOMIC` routine is rebuilt on every plan
([ADR-0009](ADR-0009-postgres-modules.md) §2.2). This document answers the same
question for the **seam**: the `Dialect` trait and `Statement`, which is where a
second engine actually lands, and which Phase 0 explicitly claims to have
validated against PostgreSQL.

That claim is written into the crate's own header, as a four-row table of "the
four most easily missed differences [that] all fit". Three of the four hold. One
is false, and the interesting part is that its **conclusion** is right while its
**reason** is not — which is the more dangerous of the two ways to be wrong,
because nothing downstream fails.

## Measured, against what

PostgreSQL 18.6 —
`docker.io/library/postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280`,
on 2026-09-04. Three of the findings below were produced by running **this
repository's own code** (`MinimalDialect::normalize_definition`) over
PostgreSQL-shaped input, not by reading it.

## Phase 0's four claims, checked

| The header's claim | What the engine did | Verdict |
|---|---|---|
| Unquoted identifiers fold to lowercase | `CREATE TABLE app.MixedCase` → `relname = mixedcase`; `app."Quoted Name"` preserved | **Holds.** `fold_ident` is the right shape |
| Batch separation not needed | `ALTER TABLE … ADD COLUMN` then `UPDATE` referencing it, one transaction: accepted | **Holds.** `own_batch` stays `false`, `batch_separator` `None` |
| A rename is followed into views automatically | Renaming the table rewrote the view's stored definition; renaming a column produced `name AS full_name` inside it | **Holds, and costs more than "left to Phase 3's `DialectDb`" suggests** — see ADR-0009 §2 |
| A type change and a nullability change need two statements | One statement, two subcommands, accepted: `ALTER TABLE d.t ALTER COLUMN a TYPE varchar(20), ALTER COLUMN a SET NOT NULL` → `character varying(20)`, `attnotnull = t` | **False** |

And, beyond the four, two more the trait already provides for:

- **DDL is transactional.** A `CREATE TABLE` inside `BEGIN … ROLLBACK` left
  nothing behind. §7.5's "one plan, one transaction, all or nothing" is a promise
  this engine can keep.
- **`CREATE INDEX CONCURRENTLY` refuses a transaction block** —
  `ERROR: CREATE INDEX CONCURRENTLY cannot run inside a transaction block`.
  `Statement::transactional` exists for exactly this, and ADR-0003's staged
  apply is the machinery it feeds. PostgreSQL's `strategy: online` will map here.

## Amendment 1: `Vec<Statement>` is right; its stated reason is false

Two places say the same false thing:

```rust
//! | Type + nullability change | needs two statements | can be merged into one | `Dialect::emit` returns a `Vec` |

    /// Returning a `Vec` is necessary: PostgreSQL has to split a type change and
    /// a nullability change into two `ALTER COLUMN` statements, whereas SQL Server
    /// can merge them into one.
```

Measured, PostgreSQL merges them. The `Vec` is still necessary — ADR-0009 §3
produces `DROP VIEW`, `CREATE VIEW` and one `GRANT` per declared permission for
a single `AlterModule`, and SQL Server's own `RenameTable` already needs two
statements, as `Statement::renames`' documentation explains at length. So
nothing built on the `Vec` is wrong.

This is recorded as an amendment rather than shrugged off because of CLAUDE.md's
rule: *"A guard whose reason has gone is a filter nobody re-reads."* Here the
guard's reason was never true, which is worse — the next person to weigh
"could `emit` return one `Statement`?" reads a justification that five seconds
against a real engine disproves, and learns that the file's reasons are not
load-bearing. The fix is to state the reasons that are.

**Not applied in this branch.** The correction touches
`crates/pbps-dialect/src/lib.rs`, which PR #10 also touches; taking a conflict
for a comment change gains nothing. It lands with the first PostgreSQL commit.

## Amendment 2: `normalize_definition` is SQL Server's scanner, and PostgreSQL breaks it in both directions

`Dialect::normalize_definition` has a default implementation — a whitespace
collapser that knows which regions are data — and it is presented as
dialect-neutral. It is not. **Measured**, by running it:

| Input | What it should do | What it does |
|---|---|---|
| `SELECT $tag$a  b$tag$` vs `SELECT $tag$a b$tag$` | Keep them different — the spacing is inside a string literal, so it is data | **Collapses both to `SELECT $tag$a b$tag$`.** They compare **equal** |
| `SELECT a[1  +  2] FROM t` vs `SELECT a[1 + 2] FROM t` | Collapse — a subscript is code | **Keeps the spacing.** They compare **different** |

The cause of both is one line: the scanner opens a quoted region on `'`, `"` or
`[`. `[` is SQL Server's identifier quote and PostgreSQL's array subscript, and
`$$`-quoting does not exist in T-SQL at all.

These two point opposite ways, and only one of them is survivable — a third,
found in review and measured below, joins the silent one:

- The subscript case is **noisy**: a reindent inside `a[1 + 2]` reads as a
  changed module and gets restated. §8.2 already accepts that cost ("the cost of
  a false positive is restating one definition").
- The dollar-quote case is **silent**: two function bodies that return different
  strings compare equal, so the change is **never planned at all**. That is the
  failure this project's existing test `whitespace_inside_a_literal_is_data` was
  written to prevent on SQL Server, arriving on PostgreSQL through the shared
  default.

And the alphabet is not the whole of it. **Measured**, on the same code, with
PostgreSQL's escape-string syntax:

| Input | Should |
|---|---|
| `SELECT E'it\'s  here'` vs `SELECT E'it\'s here'` | stay different — `\'` does not close an `E'…'` string, so the spacing is data |

```
two-space and one-space bodies compare equal? true
```

Both collapse to `SELECT E'it\'s here'`. The scanner closes the literal at the
backslash-escaped quote, treats `s  here'` as code, and folds the spacing — so
two function bodies that return **different strings** compare equal and the
change is never planned. Measured against the engine, `E'it\'s  here'` is one
ten-character literal `it's  here`, so there is no ambiguity about which answer
is right. Note that `standard_conforming_strings` is `on` by default, which is
what makes this specific to `E'…'`: in a plain literal the backslash *is*
literal and the quote does close.

**Decision.** `normalize_definition` stops being a shared default that any
dialect inherits. The shared scanner takes a description of the engine's
literals, and that description carries **termination rules, not only
delimiters**:

| | SQL Server | PostgreSQL |
|---|---|---|
| identifier quote | `[` … `]` | `"` … `"` |
| string | `'` … `'`, doubled to escape | `'` … `'`, doubled to escape |
| — | | `E'` … `'`, **backslash escapes** |
| — | | `$tag$` … `$tag$`, no escapes at all |

The *default* implementation is removed, so a new dialect cannot silently
inherit another engine's answer. Two of the three failures on this page came
from a scanner that was right about one engine, and a table of delimiters alone
would have fixed only one of them.

The general shape is worth naming, because it is the third time it has appeared
in this project: **a default implementation that is really one engine's answer
wearing a neutral name.** `validate_module` and `validate_role` default to "no
objection", which is honest — they say they checked nothing. This one defaults
to an answer.

## Amendment 3: `normalize_type` needs a stated contract, and `serial` is the proof

**Measured**, the catalog's spelling of ten declared types:

| Declared | Read back |
|---|---|
| `int` | `integer` |
| `varchar(50)` | `character varying(50)` |
| `bool` | `boolean` |
| `timestamp` | `timestamp without time zone` |
| `"char"` | `"char"` |
| `serial` | **`integer`**, plus an owned sequence `<table>_<col>_seq` |

The first four are what `normalize_type` is for, and they fit. The last one does
not: `serial` is not a type but a macro, and no normalization makes a declared
`serial` equal to what introspection returns. Left alone it produces a schema
that differs from itself on every single run — the permanent phantom change.

The trait never states the contract that rules this out. It should:

> `normalize_type` is idempotent, **and its output is what introspection reads
> back for a column declared that way.** A spelling for which that is
> impossible is an error, not something to normalize.

With that stated, `serial` is refused at load time with `GENERATED … AS IDENTITY`
named as the replacement — which, as ADR-0010 §7 notes, also disposes of the
sequence-grant problem, since an identity column needs no sequence privilege
while a `serial` column does (measured: `ERROR: permission denied for sequence
ser_id_seq`).

## What needs no change

`name`, `type_change_risk` and `TypeChangeRisk`, `fold_ident`, `quote_ident`
(PostgreSQL doubles an embedded `"` — measured: `app."we""ird"` becomes the
relation `we"ird` — which is `quote_ident`'s job, not the signature's),
`validate_table`, `validate_module`, `validate_role`, `preflight` and `Probe`
(one row, one integer: no PostgreSQL obstacle), `batch_separator`,
`render_script`, `Statement`'s `own_batch`, `transactional`, `renames`,
`role_renames` and `creates`.

One item is executor work rather than a trait change, and is recorded so it is
not mistaken for "nothing to do": on PostgreSQL a rename rewrites the stored
definition of every view that referenced the renamed object. An earlier version
of this paragraph concluded that `apply` should therefore re-read managed
modules after such a plan. [ADR-0009](ADR-0009-postgres-modules.md) §2 has since
rejected that as blessing a divergence — the environment would then hold a
definition the declarations can no longer produce — and requires the dependent
modules to be **rebuilt from their declarations** instead. Re-reading survives
only for the modules a plan does not rebuild, so their recorded read-back
matches the live one.

## Ruled out

- **A second trait for PostgreSQL.** Everything above is a method body or a
  contract, not a shape.
- **Parsing definitions instead of normalizing them**, which would make
  Amendment 2 disappear. §8.2 refuses it, and the amendment is smaller than the
  parser.
- **Leaving `normalize_definition`'s default in place and overriding it in the
  PostgreSQL dialect.** It works, and it leaves the trap armed for dialect three.

## How these get verified

Each amendment ships with the test that fails without it, and each of those
tests is reverted and watched to fail before the fix is kept — the rule in
CLAUDE.md that has already caught tests passing for the wrong reason here. Two
of the three are testable offline; the `serial` contract needs the PostgreSQL
live suite, which does not exist yet and is Phase 5's first deliverable rather
than its last.

## Placement

Phase 5, with the first PostgreSQL commits. None of the three is large; all
three are the kind of thing that becomes expensive once a second dialect has
been written against the wrong version.
