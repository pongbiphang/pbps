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
target selectors are refused rather than silently discarded. Dedicated
Python fixtures have explicit selector data plus scheduling witnesses in the
CI job and relevant Python functions. A nested Rust helper names its compiled
parent and the command construction/launch in that parent's call chain. Rust
witness files must also appear in that test artifact's compiler dep-info; a
leftover file outside the compiled module tree supplies no scheduling evidence.
Parents must themselves lead to an active CI entry. A root-only or PID-1
helper never gains an owner merely because it appears in a library binary.

Witnesses are deliberately bounded checks on the current runner forms, not a
Python, shell or Rust interpreter. Python uses its AST and literal selector
data; fixture modules are never imported or executed. Module entry witnesses
accept direct calls, literal truth values, and selected branches of equality /
inequality guards between constants and `__name__` for a script launched as
`__main__`. Unsupported conditional
module bodies supply no witness, and direct rebinding of `__name__` is refused.
Function witnesses retain runtime branches: this check does not prove every
runtime precondition or general Python control flow. Rust witnesses use a
comment/string-aware lexer inside an unambiguous named function. They do not
interpret Rust cfgs: the compiler's test list decides whether the parent and
child exist. Unsupported or ambiguous witness forms must be audited before
updating the checker and its negative tests. Comments, stringified calls,
missing calls, disabled CI steps and mismatched runner platforms cannot supply
an execution owner.

Module-level selector data is kept only while its supported literal assignment
remains valid. Unsupported writes, deletion and conditional rebinding discard
that name; visible mutation or escape of a mutable selector also discards its
shared aliases, including aliases inside supported containers. Class-construction
keywords and subscription keys (reads, writes and deletions, including slice
components) are arguments to potentially mutating Python protocols. Literal tuples
retain their immutability and Python concatenation semantics; a tuple can still
expose mutable children. Data selectors accept lists or tuples of names. Unsupported
assignment RHS values also count as escapes inside compound statements,
augmented assignments and assignment expressions. Proven top-level literal
replacement, string prefixes and the existing list comprehensions remain
supported. Unknown calls and containers are not evaluated to guess their
effects. Class-local shadows are discarded before reads in statements that may
delete them, including implicit exception-handler cleanup. This is conservative
about conditional execution; deletions in separate local scopes stay separate.
Function-local bodies are not interpreted as module assignments; this
bounded extraction does not execute helper calls or prove arbitrary runtime
control flow.

Visible `globals()`, `locals()`, `vars()` and unknown dynamic execution can
retain the module namespace: later literal assignments cannot restore evidence
after that exposure. A direct module-level `vars(SimpleNamespace())` call is
recognized as unrelated only with an untouched `types` import and builtin
`vars`; imported aliases follow their final left-to-right binding. The
constructor must receive no arguments, and the call must be an expression or
assign only plain names. Its preceding execution must also be proven inert:
literal data (using the same bounded proof for concatenations, aliases and
supported comprehensions as selector extraction), uncalled helper definitions,
direct `types`/`sys`/`builtins` imports, and the supported `types` constructor
imports. Unknown imports, callbacks, decorators, class construction and custom
targets permanently remove
this exception; importing a constructor again cannot restore it. Writing inert
values under literal string keys into a directly proven fresh dictionary remains
safe.
This boundary limits the reflection exception, not ordinary literal selectors.
Other reflective object forms remain conservatively unsupported; this check
does not import fixtures or resolve arbitrary Python runtime state.

The inventory is a scheduling contract, not runtime coverage or proof that every
branch was visited. Fixture success assertions and exact-case execution checks
remain necessary. In particular, a listed case's early return can still need an
engine-specific review. The Docker rehearsal case belongs to its separate job
with `PBPS_TEST_DEV_IMAGE`, not the ordinary SQL Server run that deliberately
leaves that opt-in unset.

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
