# ADR-0014: The driver seam, tested — what a second driver actually costs `pbps-db`

- Status: proposed. Phase 5 design. This does not revisit
  [ADR-0007](ADR-0007-connection-strategy.md); it **tests one of its claims**.
- Date: 2026-09-05
- Related: docs/SPEC.md §11.2, §11.3, §11.5, §12, open question 10;
  [architecture constraint 9](ARCHITECTURE.md#inviolable-constraints);
  `crates/pbps-db/src/lib.rs`;
  `spikes/pg-driver`

## The claim under test

ADR-0007's fifth decision:

> **The seam is the deliverable that makes all of this cheap.** `pbps-db` now
> owns `Row`, `FromColumn` and `Param`, and `tiberius` is named in exactly one
> file — so ADBC, ODBC, `tiberius-ng` or staying put are all changes to one
> crate. That was built before the decision on purpose: the cost of being wrong
> is what makes a decision hard to take, and this lowers it.

That is a claim about a thing nobody has done. ADR-0007 set the standard for
claims like it by running a spike for ADBC — and reporting honestly that the
spike never reached the behavioural question, because the driver could not be
obtained. **This one reached it.**

## The spike

`spikes/pg-driver` re-implements the public shape of `crates/pbps-db/src/lib.rs`
— `DbError`, `Row`, `FromColumn`, `Param`, `Conn` and its eight methods — over
`tokio-postgres 0.7`, and runs it against PostgreSQL 18.6
(`docker.io/library/postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280`)
on 2026-09-05. Every point where the shape had to bend is marked `SEAM:` in the
source. Output, verbatim:

```
malformed string: Some("BadConnectionString")
refused socket:   Some("Connect")
never answered:   Some("ConnectTimeout")
connect: ok
execute (multi-statement batch): ok
query + FromColumn: answer=Some(42) nothing=None by_index=Some(42)
execute_with: 1 row(s) affected
query_with: v=Some("one")
absent is not empty: v IS NULL = Some(true)
second statement failed: true
  SQLSTATE: Some("23505")
all-or-nothing: rows left by the first statement = Some(0) (must be 0)
undefined table SQLSTATE: Some("42P01"), parses as u32: Some(false)
```

The first three lines are the connection failures the seam tells apart (§3).
The tenth and eleventh are SPEC §11.5's invariant — *a plan whose second
statement fails leaves the first one's effect nowhere* — which is the acceptance
criterion ADR-0007 named for a driver change, run against a second driver for
the first time.

## The verdict

**The seam holds for the question this spike asked, which is narrower than the
one Phase 5 needs to answer.**

The spike *replaced* one driver with another behind the same shape. Phase 5 has
to *keep both*, selected per project — and that is not the same test. Measured
by reading the code rather than by running it: `crates/pbps-cli/src/db.rs`'s
`connect` returns the concrete `pbps_db::Conn`, and **21 functions in
`pbps-mssql` take `&mut Conn` concretely**. A `tokio-postgres` client cannot
appear behind that shape without an enum, a trait, a duplicated connected path,
or generics spread across those call sites. None of that is large, and none of
it is the "one signature" this document first claimed.

So the honest verdict is in two parts:

- **What the spike proves:** the seam's *shape* — `Row`, `FromColumn`, `Param`,
  the four query and execute methods, the transaction framing — absorbs a
  fundamentally different driver without changing a caller. That was the part
  most at risk, and it held.
- **What it does not prove:** that two drivers coexist. The concrete `Conn` at
  the CLI boundary is the seam's untested half, and the cost of the second
  dialect includes making that boundary polymorphic.

With that correction, **it costs one signature *plus* a decision at the CLI
boundary, and it makes one false statement in CLAUDE.md visible.**

| Seam point | Second driver |
|---|---|
| `Row`, `try_get`, `try_get_at` | unchanged |
| `FromColumn` blanket impl | unchanged in shape; NULL is expressed differently by the two drivers and the seam's own `Result<Option<Self>>` absorbs both |
| `Param` enum and `as_sql` | unchanged; needs a `+ Sync` bound that every variant already satisfies |
| `query`, `execute`, `query_with`, `execute_with` | unchanged; `batch_execute` is the analogue of `simple_query` |
| The absent-is-not-empty invariant | holds — `Param::OptStr(None)` arrived as SQL NULL |
| `begin` / `commit` / `rollback` framing | holds; **the statements inside them do not** — §2 |
| `Conn::connect`'s signature | unchanged; what a valid string *is* becomes dialect knowledge — §3 |
| `DbError::Connect` / `ConnectTimeout` | unchanged in shape; **a first draft of the spike folded the timeout into `BadConnectionString`** and the review caught it — §3 |
| `DbError::server_error_number` | **cannot express PostgreSQL's error code** — §1 |

## 1. `Option<u32>` cannot hold a PostgreSQL error code

**Measured.** A duplicate key returns SQLSTATE `23505`; a missing table returns
`42P01`, which does not parse as a `u32`. PostgreSQL's error codes are
five-character alphanumeric strings, and roughly half the classes contain
letters.

The method's own documentation is what makes this worth an ADR rather than a
patch:

> Exposed as a bare number and not interpreted here: what 208 or 229 *mean* is
> SQL Server's vocabulary, and this crate deliberately holds none of it — a
> dialect's error codes belong in that dialect's crate.

The **intent** is exactly right, and the **type** is SQL Server's. This is the
third instance in this design pass of one shape: ADR-0011 Amendment 2 found a
"dialect-neutral" definition normalizer that was T-SQL's scanner, ADR-0013 §1
found two catalog flags whose names match and whose meanings are opposites, and
here a neutral-sounding accessor returns a type only one engine can fill. **The
tell is the same every time: a neutral name over one engine's answer, in code
that compiles fine because there is only one engine.**

**Decision.** `server_error_number() -> Option<u32>` becomes
`server_error_code() -> Option<String>` — **owned, not borrowed**. A first draft
of this ADR proposed `Option<&str>`, and that cannot be implemented on the
driver it has to keep working for: `tiberius_ng::Error::code()` returns
`Option<u32>` (`error.rs:78`), so there is no string anywhere in `DbError` for
the reference to borrow from. The spike had already settled this and the ADR
disagreed with it — `spikes/pg-driver` returns `Option<String>` and compiles;
the sentence proposing `&str` did not have to.

`Cow<'_, str>` would also work and buys nothing here: the SQL Server side has to
allocate either way, and the PostgreSQL side hands back a `&'static str` from
the driver that is cheap to own. Pick the plain owned form until an allocation
in an error path is worth a lifetime.

The cost is exactly two files: the definition, and its single caller —
`crates/pbps-mssql/src/state.rs:250`, which compares against
`INVALID_OBJECT_NAME` and would compare against `"208"` instead. Measured by
grep, not estimated: there are two occurrences of the name in the whole
workspace.

**Applied** in the seam-preparation PR (DECISIONS 193).

## 2. `begin()` holds T-SQL, in the crate that is documented to hold none

The architectural boundaries originally recorded in `CLAUDE.md` (now in
[ARCHITECTURE.md](ARCHITECTURE.md#architectural-boundaries)) said of `pbps-db`:

> Connections (tiberius) plus transaction framing. Owns "there is a network";
> ledger types and prune policy, **no T-SQL**.

`Conn::begin` sends `SET XACT_ABORT ON; BEGIN TRANSACTION;`, and `rollback`
sends `IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;`. Both are T-SQL, both are in
that crate, and both are there for good reasons the code explains at length —
`XACT_ABORT` is what makes §7.5's promise true rather than merely intended.

The constraint was not wrong when it was written; it stopped being true and
nobody re-read it. **Measured**, PostgreSQL needs no counterpart: the spike's
`begin()` sends a bare `BEGIN;` and the all-or-nothing test passes, because any
error already dooms a PostgreSQL transaction.

**Decision.** The three statements move behind the dialect — a small per-dialect
triple, not a new trait — and `pbps-db` keeps the framing, which is what it
actually owns. The value is not the refactor; it is that a stated constraint
stops being false. CLAUDE.md's own rule covers this case: *"A guard whose reason
has gone is a filter nobody re-reads."*

**Applied** in the seam-preparation PR as `Dialect::transaction_framing`, a
required method returning three `&'static str`s that `Conn::begin`, `commit`
and `rollback` take; the `XACT_ABORT` and `@@TRANCOUNT` reasoning moved with
the text into `pbps-mssql` (DECISIONS 194).

## 3. What did not leak

`tokio-postgres` returns a `Client` **and** a `Connection` future that something
must poll, where `tiberius` returns one client. `Conn` therefore has to own a
task handle that has no counterpart in the current shape.

That is the one genuinely structural difference between the two drivers, and it
stayed inside the crate: no caller of `Conn` sees it, and no signature changed.
This is the part of ADR-0007's claim that was most at risk and it held.

**One thing nearly leaked, and by the spike's hand rather than the driver's.**
`pbps-db` keeps three connection failures apart — `BadConnectionString`,
`Connect { addr, source }` and `ConnectTimeout { addr }` — because a malformed
string, a refused socket and a firewall that drops packets need three different
fixes, and the CLI's diagnostics say which. A first draft of the spike mapped
its timeout to `BadConnectionString`, so a target that silently dropped the
connection for thirty seconds would have been reported as a typo. A driver
spike that does not preserve the error categories is not testing the seam it
claims to; the finding is that the shape held once it was actually
reimplemented, and what it cost:

- the timeout variant carries over unchanged;
- `Connect` does not come for free, because tokio-postgres opens the socket
  inside `connect` and wraps the `io::Error` in its own error type. The seam
  wants that `io::Error` by value, and it cannot be moved out of the driver's
  error, so it is rebuilt from the `source()`'s kind and text (`SEAM 9`);
- the address the two variants name comes from the parsed `Config`'s host and
  port lists rather than from one string, which is the same dialect knowledge
  §3 above already places behind `connect`.

**Measured** — a string with a non-numeric port, a closed local port, and a
local socket that accepts and never answers (a blackhole address is refused
outright by a host with no route to it, so it does not reach the timeout):

```
malformed string: Some("BadConnectionString")
refused socket:   Some("Connect")
never answered:   Some("ConnectTimeout")
```

## What this says about the two open decisions

- **Open question 10, which is now the return trip.** The move *to*
  `tiberius-ng` has been taken — `9fe1289` switched the `package` and emptied
  `deny.toml`'s `ignore` list — and what open question 10 holds today is the
  move **back** to upstream `tiberius`, gated on a release newer than 0.12.3
  whose `rustls` feature resolves `rustls >= 0.23`, with `cargo deny check`
  passing on an empty `ignore` and the §11.5 live suite green.

  This spike bounds that return, and bounds it generously. Going back is the
  same driver API, the same error type and no connection future; the spike
  changed a *different* driver, hit two seam points, and still took an
  afternoon. So the standing cost of being wrong about a driver is smaller than
  the one thing anybody has actually measured — which is what ADR-0007 built the
  seam to make true, and it is worth having a number rather than a hope.
- **ADBC is unaffected.** It was refused on source availability and
  distribution, not on the seam, and nothing here touches that.

## Ruled out

- **Deciding *how* `Conn` becomes polymorphic** — enum, trait, or generics.
  Premature with one real driver and one spike: the useful abstraction is the
  one drawn from two implementations that both exist, which is ADR-0007's own
  "the data-driven extraction waits for the second dialect", applied to itself.
  What is *not* deferred, and what this document got wrong the first time, is
  that the boundary has to change at all.
- **Re-exporting the driver's types** to avoid `FromColumn` and `Param`. The
  spike is the argument against it: those two are what let a second driver be a
  change to one crate.
- **Keeping `Option<u32>` and encoding SQLSTATE numerically.** `42P01` has a
  letter in it; any encoding is a second vocabulary nobody asked for.

## Limits

- **The spike uses `NoTls`.** A real PostgreSQL dialect needs a TLS stack, which
  is where open question 10's supply-chain story rejoins — and `tokio-postgres`
  leaves that choice to the caller rather than pinning it, which is the property
  `tiberius 0.12` lacks.
- **It does not implement the ledger, the lock, prune policy, pooling,
  cancellation or long-running statements.** It tests the seam's shape, not
  `pbps-postgres`.
- **It proves the shape absorbs a second driver. It does not prove a second
  dialect is cheap** — ADR-0007 measured that at ~7,000 lines and this changes
  none of it.

## Placement

The spike is evidence and goes once this is settled, like `spikes/yaml-span`.
The two amendments (§1, §2) are small, are not PostgreSQL-specific, and are
better taken while there is still exactly one dialect to update — but they touch
`pbps-db` and `pbps-mssql`, so they wait for PR #10 rather than compete with it.
