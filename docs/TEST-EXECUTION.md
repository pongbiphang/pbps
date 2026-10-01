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
positional bases and keywords, along with subscription keys (reads, writes and
deletions, including slice components), are arguments to potentially mutating
Python protocols. For a direct module class without decorators, fully proven
literal bases expose only the mutable objects actually passed: starred native
containers pass their elements, and native indexing/slicing with literal integer
bounds preserves immutable reads and independent copies. Shared mutable children
still escape. Unknown base expressions and nested classes retain conservative
reference tracking; this proof does not extend the selector assignment grammar.

The exact-value class-base exemption also requires a pristine execution prefix
and a bounded proof of the whole construction. Literal keywords and native dict
unpacking (including pristine literal dictionary aliases) are accepted. A custom
metaclass must be a directly defined, unmodified function (or a proven alias),
whose reachable helpers only operate on native arguments. Any `type` construction
must use the unshadowed builtin with empty bases, only as the metaclass return
operation; helper-produced classes cannot be treated as native containers.
Mutable global access, reflection, unknown calls, modified prepare hooks,
and unsupported callable bodies retain conservative base-reference effects.
The class body may contain literal local assignments and plain method definitions
with inert definition-time expressions; an uncalled method body is not executed.
This is a deliberately bounded proof, not execution or import of fixture code.
Empty native base expansions also permit the default metaclass or an unshadowed
builtin `type`, with the same class-body proof.

Literal tuples
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
after that exposure. This includes calls through direct `builtins` module
imports, imported reflective callables and plain-name alias assignments.
Tuple/list assignment targets also retain corresponding aliases through nested
and starred targets. Literal RHS elements are snapshotted once in Python order,
before any chained target writes, and targets bind from left to right. A real
later native shadow replaces the alias; an unused reflective alias is not an
exposure. The snapshots last only for that assignment. Unknown iteration,
unsupported expression results, mismatched arity and target protocols refuse
ownership rather than manufacture harmless bindings. Container contents retain
possible reflection without acquiring a contained callable's native exemption;
stored-container and general expression provenance remain separate proofs
(DEC-1299.1).

Bindings are followed in statement order: direct shadows replace aliases,
class-local bindings do not replace module bindings, and uncalled function
bodies do not expose namespaces. Unknown conditional writes retain possible
reflective aliases conservatively. With/AsyncWith keeps the running path's bindings
separate from the prefixes that an entered manager may preserve by suppressing
a failure. Calls inside that path see successful shadows; the exit joins the
retained prefixes before subsequent statements. A guaranteed following shadow
still replaces the joined aliases. This does not prove that every retained
prefix actually throws, or that a manager always completes normally.

Exact active shadows require conservative handling of opaque effects that
could restore an erased reflective alias. A separate, permanently invalidated
pristine-prefix proof recognizes only narrow native calls: literal-container
`len`, constant exception construction, a fresh SimpleNamespace with a single
empty callable member, imported nullcontext/suppress, and a plain async manager
whose two methods return only None and a boolean. These are AST proofs, never
fixture imports or executed callbacks. Unproven calls, protocols, mutations and
manager effects cannot acquire this exception after a possibly reflective alias
has been erased. Legal async fixtures qualify visitor components separately;
interpreting arbitrary called async helpers is not a module ownership route
(DEC-1383.1).
Each retained continuation captures bindings through its owning scope. A class
entered beneath a manager adds its own local continuation while inheriting the
enclosing continuations and shared module state. Its stack is copied, so leaving
the class cannot retain stale class locals in later enclosing statements. Nested
classes retain enclosing captures for opaque effects but resolve names against
their own locals and the module, not an enclosing class's locals.

To preserve the measured inert-helper and real following-shadow controls, the
pristine-prefix proof also recognizes zero-argument plain functions whose only
statement returns None, and one narrowly proven class-frame write. That helper
must consist solely of `sys._getframe(1).f_locals[<literal string>] = <value>`;
its native sys binding and reflective builtin or builtins-module RHS are resolved
from the module at call time. Only a plain native class namespace and a local
target qualify. Other helper bodies, replaced frame access, class namespace hooks,
and unknown RHS effects stay opaque; an actual namespace escape remains sticky
after later shadows. These proofs do not interpret arbitrary helpers or import
fixture code. Actual async execution remains separate from component proof and
complete ownership validation (DEC-1389.1).

Potential namespace access is tracked
separately from the stricter pristine-binding proof for safe object inspection;
passing an imported module to an opaque helper cannot make a later reflective
call harmless. A direct module-level `vars(SimpleNamespace())` call is
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
