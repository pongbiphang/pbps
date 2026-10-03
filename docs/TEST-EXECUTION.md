# Ignored-test execution ownership

An ignored test can compile forever without being selected by a fixture.
`scripts/ignored-test-owners.json` records the crate, target, exact case name,
execution platform, fixture prerequisites and scheduling owner for these cases.
The Linux quick job and Windows job check the inventory after their ordinary
workspace tests. The checker requires Python 3.11 or newer and the project Rust
toolchain on `PATH`. Run the same checks locally:

```bash
python3 scripts/ignored_test_inventory_test.py
python3 scripts/ignored_test_inventory.py
```

The checker asks Cargo for test artifacts using `--no-run --message-format=json`,
then invokes each test executable only with `--list`. It does not start an
engine, a privileged helper or an ignored test. Explicit custom test harnesses
are refused before their executables are invoked; they need a safe listing
adapter. Cargo determines modules,
`#[path]`, macros and conditional compilation; there is no second Rust module
loader guessing which source functions become tests. A fresh checkout may need
to compile the workspace; a checkout that just ran its tests reuses those builds.

## What an owner means

Ordinary live targets are checked against their actual CI Cargo selectors,
including target identity, exact/substring filtering and `--skip`. Cargo's
optional TESTNAME before `--` joins the libtest filters after it: any matching
filter selects a case. `--exact` applies to both positive filters and `--skip`: a
case is excluded only when a skip matches its complete name in exact mode, or
a substring otherwise. Package, target and profile option values are not test
names. Unsupported Cargo options, multiple TESTNAME arguments and ambiguous
target selectors are refused rather than silently discarded. A step's `if:`
counts as executing only when it is a literal truth value or a conjunction of
`matrix.<axis> == '<value>'` that asks one value of each axis it names, and
that value is on the axis. A `strategy` must be a block whose `matrix` lists
each axis on its own line as `name: [a, b]`. A one-line flow or expression
matrix is refused, because no variant it yields can be read. Axis values must be tokens YAML reads only as
strings: a letter first, no dot, and not a boolean or null word. A quoted
value could hold a comma. A boolean, null or number keeps its type, so the
string comparison never holds. Both are refused. GitHub compares strings
without regard to case; the audit compares them exactly, which can only make
it refuse more. Without
`include`/`exclude`, which are refused, every combination of the axes runs. A
suite split across matrix variants is therefore owned only while the
variants' filters together select each case (DEC-1370.1). Dedicated
Python fixtures have explicit selector data plus scheduling witnesses in the
CI job and relevant Python functions. A nested Rust helper names its compiled
parent and the command construction/launch in that parent's call chain. Rust
witness files must also appear in that test artifact's compiler dep-info; a
leftover file outside the compiled module tree supplies no scheduling evidence.
Parents must themselves lead to an active CI entry. A root-only or PID-1
helper never gains an owner merely because it appears in a library binary.

Python owners use one literal grammar and one source-wide syntactic policy
(DEC-1413.1). The checker parses each owner source once; it never imports or
executes a fixture. Registry data expressions and selected call arguments name
the protected selector constants. Each constant has exactly one plain assignment
at module level. Values use literals, lists/tuples, `+`, and one synchronous
unfiltered list-comprehension generator over literal data. Subscriptions and
opaque calls are outside this grammar. The launch prefix comprehension, server
list, singleton `TEST` and target's named constants all use these same rules.
There are no per-file profiles or helper-skeleton proofs.

A protected name cannot bind elsewhere: assignment/deletion/augmentation,
`global`/`nonlocal`, import alias, definition or argument, exception target,
pattern capture and type parameter fields are all checked. Mutable list
selectors may be read only as the direct iterable of a `for` or comprehension.
Qualified attribute, from-import and class-pattern keyword attribute reads
follow the same restriction. Their aliases, argument passing, container storage,
subscripting and mutation receivers
are therefore refused without tracing Python effects. String and
immutable tuple constants retain ordinary reads. This is a source form, not a
scope or annotation execution interpreter: an otherwise harmless protected
binding in an uncalled body is still outside the form. Ordinary `global ENGINE`
and `global DOCKER_SOCKET` remain allowed.

The whole module is scanned for reflective AST spellings (`globals`, `locals`,
`vars`, `eval`, `exec`, `compile`, `__import__`, `__builtins__`, `__dict__`,
`__globals__`, `f_globals` and `f_locals`), reflection-module imports
(`builtins`, `importlib`, `inspect`, `gc`), qualified `sys.modules`/`sys._getframe`
access through syntactic import aliases, wildcard imports and bindings of
`__name__`. Identifier references in import and pattern string fields are
checked too; ordinary strings containing those words are data. The check does not attempt
runtime alias resolution. `ctypes`, imported repository helpers and intentional
indirect source edits remain a maintainer trust boundary; this lint is not a
sandbox or proof of arbitrary Python behavior. Unrelated operations such as
`~0`, set literals and ordinary annotations are not interpreted or forbidden.

A data/call selector runner has a unique undecorated module-level `main` and ends
with exactly `if __name__ == "__main__": main()` or
`if __name__ == "__main__": raise SystemExit(main())`. `main` need not read the
selector directly: existing named-function witnesses check the helper invocation
route. Witness-only scripts without selector data retain direct module execution;
this includes the sockets runner. No file-specific entry exception is configured.
Function witnesses retain runtime branches and exclude inactive literal branches,
nested definitions, comments and stringified calls. Tokens preserve statement
boundaries, so `run` followed by a separate `()` cannot manufacture `run()`;
multiline, nested, continued and partial call fragments remain supported.

Rust witnesses retain their comment/string-aware lexer inside an unambiguous
named function. Cargo's compiled test list supplies cfg and module reachability.
Python, shell and Rust witnesses do not prove every runtime precondition. Missing
invocations, disabled CI steps, mismatched platforms and ambiguous source scopes
cannot establish ownership. Unsupported Python form diagnostics identify the
file, line and construct rather than producing an empty successful selection.

The namespace/effects evaluator and its class, continuation and pristine-prefix
proofs are retired. DEC-1413.1 supersedes DEC-1299.1, DEC-1383.1, DEC-1389.1 and
the evaluator policy of DEC-1300.1. Historical decisions remain as provenance.
Regression dispositions identify the exact program and actual Python result:
a safe program may now qualify, or be refused for a named form restriction.
Each operational policy rule has a permanent full-owner test that first observes
its runtime control, refuses it, removes only that rule and observes admission,
then restores refusal. A refusal by another guard does not count as that rule's
counterfactual. Literal node decoding has no execution fallback.

Historical program compatibility is explicit (DEC-1428.1). A corpus row may
declare `python.minimum` (inclusive), `python.before` (exclusive), and a required
reason. Untagged programs retain the documented Python 3.11 minimum. Type-alias
statements require 3.12; the preserved named-expression annotation programs
compile before 3.14. Original source, adapter, runtime outcome, provenance and
owner disposition remain intact in both historical tables.

Every row still runs in an actual child interpreter. Eligible rows must match
their original runtime outcome and complete owner disposition. Ineligible rows
must produce `SyntaxError`, not an arbitrary execution error. Below a syntax
minimum the owner must report invalid Python; above the annotation boundary the
AST remains parseable, so its original owner disposition is still checked.
Completed runtime and owner checks produce per-case receipts; independent
eligibility and coverage assertions reject dropped rows or broad skips.

Run `scripts/ignored_test_inventory_test.py` with actual CPython 3.11, 3.12 and
3.14 when changing these boundaries; CI runs its configured Python version.
The integrated suite includes both historical consumers and closed-form
controls. Generic type-parameter bindings retain their original source as an
unsupported-syntax control on 3.11 and a binding-rule removal control on 3.12+.
Independently removing each version boundary must restore its actual-interpreter
failure, and skipping tagged but eligible rows must fail the execution ledger.
Syntax eligibility does not establish annotation execution semantics. Separate
module-annotation controls (DEC-1262.1) retain the original bare
`__annotations__` access: it mutates the selector before 3.14 and raises
`NameError` on 3.14+. Access through the running module's `__annotations__`
attribute must actually empty the selector on both versions. Both complete
owners are refused on every interpreter at the protected list read in the
annotation; disabling only `list_reads` must admit them. The version branch
applies only to the original runtime oracle, never the static refusal or the
portable successful mutation control. Run the integrated suite on actual
3.12 and 3.14 interpreters when changing these controls.

## Maintaining the inventory

- Add a newly ignored case's fully qualified libtest name to the correct target
  and owner group. The checker reports the tuple for an unowned case. Register
  a new owner if no existing invocation can execute it; do not assign a case to
  a convenient but unrelated fixture.
- `validate_on` is the platform where the owner compiles and executes the case.
  On that platform a removed name or target fails. On every checked platform,
  newly discovered ignored cases must be registered, and present cases must
  agree with `ignored_on`. This allows Linux-only native helpers to be absent
  on Windows while detecting newly added Windows-only ignored cases there.
- The two APFS-inapplicable filename cases are ignored on macOS and owned by
  ordinary Linux tests. Their platform condition is explicit rather than an
  exemption from the inventory. No macOS CI runner is implied.
- Preserve the owner path when refactoring a fixture. Update its selector and
  scoped witnesses together, and verify a removed invocation is rejected.
  Exact selector names that no longer identify compiled cases also fail.
- An added helper must identify a reachable compiled parent and the necessary
  private fixture. Never replace explicit helper selection with a blanket
  `cargo test -- --ignored` against the workspace or CLI library.

The ordinary quick job explicitly runs the owned Linux rename-churn measurement.
It previously had only a manual-run note. Its assertions remain unchanged; the
checker also rejects removal of that scheduling command.
