# Decisions

Every entry here was paid for. Each one records a choice that is not the obvious
one, and *why the obvious one is wrong* — which is the part that is expensive to
reconstruct and the reason this record is longer than a changelog.

Read the relevant topic before changing the data model, the permission checks,
the ledger, or anything a command's exit code depends on. `CLAUDE.md` carries
the rules; this carries the reasons.

Companion files: [PITFALLS.md](PITFALLS.md) for the bugs and traps found the
hard way, [STATUS.md](STATUS.md) for where the product currently stands,
[SPEC.md](SPEC.md) for the design, and `ADR-*.md` for the standalone records.

## Adding a decision

> **The identifiers are load-bearing.** Code comments cite them. Never renumber
> an entry, and never reuse one.

- Append the entry to the end of the topic file it belongs to — the one for the
  area where the rule is *enforced*, not every area it mentions. If none fits,
  add a topic file and a row to the table below in the same change.
- Name it after the issue its branch serves: `DEC-<issue>.<k>`, where `k`
  counts that issue's entries from 1. GitHub allocates issue numbers and one
  branch serves one issue, so two branches can never pick the same identifier,
  and an entry keeps its identifier from the moment it is written. The
  sequential numbers below are closed: a branch that takes the "next" one
  collides silently with every other branch that does, in another file,
  where git reports no conflict.
- Write it as an anchor and a paragraph that opens with the identifier and a
  bold one-sentence title:

  ```markdown
  <a id="dec-1234-1"></a>

  **DEC-1234.1. The choice, stated as a rule in one sentence.** Why the
  obvious approach is wrong, and what was measured to show it…
  ```

- Cite it as `DEC-1234.1`. `scripts/check-decisions.py` runs in CI; it refuses
  a duplicate identifier, an entry numbered in the closed sequence, and a
  citation of either form that names no entry.

Entries 1–11 changed the original spec; SPEC is in sync with all of them.

## Topics

| File | Scope |
| --- | --- |
| [identity](decisions/identity.md) | How objects are identified across revisions: uids, the identity files, rename intents, and the rules a declared name must meet. |
| [plan](decisions/plan.md) | The saved plan: what it carries, how it is pinned to the apply, staged plans, checkpoints and `bootstrap`. |
| [apply-guard](decisions/apply-guard.md) | What `apply` checks after its statements run: postconditions, the read-back, and what the plan itself wrote. |
| [ledger](decisions/ledger.md) | The tables pbps keeps in the target, the lock, and the versions of the recorded state. |
| [drift](decisions/drift.md) | How `verify` and `status` compare the recorded state with the live one. |
| [diff](decisions/diff.md) | How a difference becomes an ordered list of changes and statements. |
| [data/declared](decisions/data/declared.md) | The `data:` block: what a declared cell may hold, how it is spelled, and how it is read back. |
| [data/row-writes](decisions/data/row-writes.md) | How a row insert, update or delete is guarded and held to what the plan recorded. |
| [data/pre-delete](decisions/data/pre-delete.md) | Counting the rows a delete would orphan or cascade into, before it runs. |
| [roles](decisions/roles.md) | Managed roles, what they are granted, and how permissions are compared and changed. |
| [doctor](decisions/doctor.md) | Which permissions `doctor` asks for, and on which securables. |
| [policies](decisions/policies.md) | The policy rules, their settings and suppressions, and what a refusal writes. |
| [cli](decisions/cli.md) | Exit codes, the findings envelope, published schemas, flags, and how messages address the operator. |
| [pull](decisions/pull.md) | Reading a database into declarations: what is read, what is omitted, and how omissions are reported. |
| [expressions](decisions/expressions.md) | Scanning, normalizing and comparing default and check expressions the way the engine reads them. |
| [types-and-probes](decisions/types-and-probes.md) | Type-change risk, the conversion and key probes that measure existing rows, and cost estimates. |
| [modules](decisions/modules.md) | Views, routines and triggers: identity, rebuilds, binding and ordering. |
| [rename-impact](decisions/rename-impact.md) | Finding what depends on an object and what a rename or drop would break. |
| [connection](decisions/connection.md) | The connection seam, the two drivers, TLS, and how database errors are reported. |
| [session](decisions/session.md) | The transaction framing, `search_path`, and the session settings every statement runs under. |
| [resolver](decisions/resolver.md) | Engine-assisted planning: resolver profiles, container admission, process and socket observation. |
| [compose-and-ui](decisions/compose-and-ui.md) | The local viewer and the compose flow that turns reviewed intent into a git branch. |
| [process](decisions/process.md) | CI, the merge queue, and how tests are arranged. |

## Entries 1–543

The record was one file until issue #671 split it by topic. Every entry kept
its number, so a citation of `DECISIONS <n>` or `decision <n>` still names
the same entry: look the number up here. This table is closed — new entries
are `DEC-<issue>.<k>` and are listed only in their topic file.

Numbers 523–526 were never merged: branches reserved them and chose
other numbers before landing. Some records still mention them as proposals.

| # | Topic | Decision |
| ---: | --- | --- |
| 1 | [identity](decisions/identity.md#decision-1) | Comparison matches by uid via two-sided identity files |
| 2 | [identity](decisions/identity.md#decision-2) | Drops require a reason |
| 3 | [identity](decisions/identity.md#decision-3) | Intents are idempotent |
| 4 | [ledger](decisions/ledger.md#decision-4) | `StateSnapshot` carries `ids` |
| 5 | [diff](decisions/diff.md#decision-5) | IDENTITY changes are blocked |
| 6 | [plan](decisions/plan.md#decision-6) | Review has two layers |
| 7 | [plan](decisions/plan.md#decision-7) | `plan` writes only the ids file, never the user's YAML |
| 8 | [identity](decisions/identity.md#decision-8) | `validate` rejects one name mapping to multiple uids |
| 9 | [drift](decisions/drift.md#decision-9) | Drift compares the managed set only |
| 10 | [plan](decisions/plan.md#decision-10) | `apply` is one transaction per plan, all or nothing |
| 11 | [ledger](decisions/ledger.md#decision-11) | `__pbps_state` protects against mistakes, not tampering |
| 12 | [diff](decisions/diff.md#decision-12) | `ALTER COLUMN` restates the whole definition |
| 13 | [pull](decisions/pull.md#decision-13) | Normalization targets what the catalog stores |
| 14 | [pull](decisions/pull.md#decision-14) | `pull` never drops what it cannot express |
| 15 | [expressions](decisions/expressions.md#decision-15) | Default/check expressions are compared after peeling the engine's stored parentheses |
| 16 | [identity](decisions/identity.md#decision-16) | `strategy:` is persistent, unlike `renamed_from` |
| 17 | [pull](decisions/pull.md#decision-17) | `pull` inventories what it cannot manage. |
| 18 | [cli](decisions/cli.md#decision-18) | `docs` output must stay deterministic and self-contained |
| 19 | [ledger](decisions/ledger.md#decision-19) | The ledger's T-SQL lives in `pbps-mssql::state` |
| 20 | [drift](decisions/drift.md#decision-20) | Drift needs `observed_ids`, not the recorded mapping on both sides. |
| 21 | [connection](decisions/connection.md#decision-21) | `pbps.yml` names the env var, never the connection string |
| 22 | [plan](decisions/plan.md#decision-22) | Probes are built per plan, not per change. |
| 23 | [plan](decisions/plan.md#decision-23) | A saved plan carries `origin`, `mode` and the post-plan `ids`. |
| 24 | [plan](decisions/plan.md#decision-24) | `apply` takes the lock before the pre-flight, and releases it on every path. |
| 25 | [drift](decisions/drift.md#decision-25) | `verify` exits 2 on drift |
| 26 | [diff](decisions/diff.md#decision-26) | ONLINE is edition-dependent, and only a connection knows the edition. |
| 27 | [plan](decisions/plan.md#decision-27) | The dev database is always optional |
| 28 | [modules](decisions/modules.md#decision-28) | Modules are matched by name and have no uid |
| 29 | [modules](decisions/modules.md#decision-29) | The emitter adds no terminator to a module. |
| 30 | [modules](decisions/modules.md#decision-30) | `introspect::split_module` may refuse. |
| 31 | [diff](decisions/diff.md#decision-31) | Module changes bracket the table changes. |
| 32 | [plan](decisions/plan.md#decision-32) | A staged plan is one logical change, and its mode lives in the file. |
| 33 | [plan](decisions/plan.md#decision-33) | A checkpoint's `ids` are the names at that checkpoint |
| 34 | [cli](decisions/cli.md#decision-34) | There are three exit codes, and the split is the feature. |
| 35 | [cli](decisions/cli.md#decision-35) | One findings envelope, not one shape per command |
| 36 | [drift](decisions/drift.md#decision-36) | `status` findings are warnings on purpose. |
| 37 | [cli](decisions/cli.md#decision-37) | The vendor annotation formats stay outside the binary |
| 38 | [doctor](decisions/doctor.md#decision-38) | `doctor` reimplements nothing and writes nothing. |
| 39 | [cli](decisions/cli.md#decision-39) | `explain` always exits 0 and needs no connection. |
| 40 | [cli](decisions/cli.md#decision-40) | The prompt is a wrapper, never a shortcut. |
| 41 | [cli](decisions/cli.md#decision-41) | The editor schemas are generated from the loader's own types |
| 42 | [plan](decisions/plan.md#decision-42) | `db::git_sha` takes the project root. |
| 43 | [cli](decisions/cli.md#decision-43) | `--check` is read-only in every direction. |
| 44 | [drift](decisions/drift.md#decision-44) | A drift report keeps both halves. |
| 45 | [drift](decisions/drift.md#decision-45) | `status` reads the lock even when the ledger is empty. |
| 46 | [ledger](decisions/ledger.md#decision-46) | The lock is asked before initialization, in all four commands. |
| 47 | [cli](decisions/cli.md#decision-47) | A flag is honoured or refused, never accepted and dropped. |
| 48 | [cli](decisions/cli.md#decision-48) | A path is spelled with `to_str`, never `display()`, before it is put in a command. |
| 49 | [process](decisions/process.md#decision-49) | A guard built twice is a guard that fires early. |
| 50 | [cli](decisions/cli.md#decision-50) | `shell_arg` has now been wrong about shells five times. |
| 51 | [data/declared](decisions/data/declared.md#decision-51) | The catalog reads rows under a scope the command supplies, never on its own. |
| 52 | [plan](decisions/plan.md#decision-52) | The saved plan carries the declarations' data scope (`data`), and the plan version is 3. |
| 53 | [data/declared](decisions/data/declared.md#decision-53) | Values are read back in the engine's spelling, and a cell equal to its default is read back as omitted. |
| 54 | [data/declared](decisions/data/declared.md#decision-54) | A table whose live key is not a single column is unreadable, and the whole read fails. |
| 55 | [data/pre-delete](decisions/data/pre-delete.md#decision-55) | The pre-delete probe asks `sys.foreign_keys` at run time, counts cascades, and leaves out the rows the plan itself moves. |
| 56 | [roles](decisions/roles.md#decision-56) | A role change has no table; `Change::table()` became an `Option` and `subject()` is the label. |
| 57 | [roles](decisions/roles.md#decision-57) | `grant-widen` is a risk class that is never gated. |
| 58 | [roles](decisions/roles.md#decision-58) | Grants are compared under the target's post-plan name, and never revoked on an object the plan drops. |
| 59 | [roles](decisions/roles.md#decision-59) | Inside a managed role, only grants on managed objects are compared. |
| 60 | [pull](decisions/pull.md#decision-60) | The catalog reports what the model cannot hold; it never folds it. |
| 61 | [diff](decisions/diff.md#decision-61) | An unnamed declared primary key matches any stored name. |
| 62 | [doctor](decisions/doctor.md#decision-62) | `doctor` asks for the role permissions only of a project that declares a role. |
| 63 | [roles](decisions/roles.md#decision-63) | A dropped role's members are written into the connected plan, never found at apply time. |
| 64 | [policies](decisions/policies.md#decision-64) | A rule's finding carries the rule's id, and the envelope's id became a `String`. |
| 65 | [policies](decisions/policies.md#decision-65) | A block with problems is refused whole at the plan point. |
| 66 | [policies](decisions/policies.md#decision-66) | A rule setting accepts the YAML boolean `false` as `off`. |
| 67 | [data/declared](decisions/data/declared.md#decision-67) | A cell at its default is read in the spelling of whoever reads it. |
| 68 | [data/declared](decisions/data/declared.md#decision-68) | Only a literal default is compared by the read; every other default is taken at the declaration's word. |
| 69 | [doctor](decisions/doctor.md#decision-69) | `doctor` asks about what the managed roles hold live, not only what the declarations grant. |
| 70 | [data/declared](decisions/data/declared.md#decision-70) | A `data:` block may not put a value into a binary column. |
| 71 | [data/declared](decisions/data/declared.md#decision-71) | A declared key is read back under the declaration's spelling, and the engine says which row it names. |
| 72 | [roles](decisions/roles.md#decision-72) | A securable this plan drops and creates again is granted from nothing. |
| 73 | [data/pre-delete](decisions/data/pre-delete.md#decision-73) | The pre-delete probe excludes an updated child row only for the column its update sets. |
| 74 | [data/declared](decisions/data/declared.md#decision-74) | Two spellings of one row on one side are refused, never reconciled. |
| 75 | [plan](decisions/plan.md#decision-75) | A connected plan is pinned under the recorded scopes plus the tables it covers for the first time. |
| 76 | [roles](decisions/roles.md#decision-76) | Role files live in `roles/`, not under a `.role.yml` suffix. |
| 77 | [plan](decisions/plan.md#decision-77) | A connected plan reads the declarations under the names the database has now. |
| 78 | [roles](decisions/roles.md#decision-78) | A role name may contain a dot. |
| 79 | [policies](decisions/policies.md#decision-79) | A suppression's `until` is checked against the calendar. |
| 80 | [data/declared](decisions/data/declared.md#decision-80) | A cell whose default was never asked about is unknown, not at its default. |
| 81 | [identity](decisions/identity.md#decision-81) | `validate --since` compares the declarations at the revision, not only the identities. |
| 82 | [policies](decisions/policies.md#decision-82) | A rule switched on without what it runs on is refused, in either spelling. |
| 83 | [roles](decisions/roles.md#decision-83) | A connected plan refuses to drop a role that owns a securable. |
| 84 | [ledger](decisions/ledger.md#decision-84) | The state snapshot is version 4, because the schema gained roles. |
| 85 | [data/pre-delete](decisions/data/pre-delete.md#decision-85) | The pre-delete probe lets the engine say whether an update moves a row off the deleted key. |
| 86 | [plan](decisions/plan.md#decision-86) | `bootstrap`'s empty-target guard counts roles, and `pull --data` runs the model's data rules. |
| 87 | [data/declared](decisions/data/declared.md#decision-87) | A declared cell must be of the kind its column reads back as, and a `sql_variant` cannot hold a declared value. |
| 88 | [roles](decisions/roles.md#decision-88) | The owned-securable check names every class the catalog carries an owner for. |
| 89 | [roles](decisions/roles.md#decision-89) | A grant's permissions are checked against what the target is, with the engine's table. |
| 90 | [data/declared](decisions/data/declared.md#decision-90) | A spatial cell cannot hold a declared value. |
| 91 | [roles](decisions/roles.md#decision-91) | A role rename faces the `rename` gate. |
| 92 | [roles](decisions/roles.md#decision-92) | `apply` reads a dropped role's members and ownership again before statement one. |
| 93 | [roles](decisions/roles.md#decision-93) | A statement that renames a role says so, as one that renames a table does. |
| 94 | [data/declared](decisions/data/declared.md#decision-94) | A non-key IDENTITY column is never read back. |
| 95 | [roles](decisions/roles.md#decision-95) | A grant `WITH GRANT OPTION` is unexpressible drift, never the plain grant. |
| 96 | [policies](decisions/policies.md#decision-96) | A clock field in a window is two digits, checked before it is read. |
| 97 | [roles](decisions/roles.md#decision-97) | Every permission the model cannot hold on a managed role is unexpressible drift, not a warning. |
| 98 | [plan](decisions/plan.md#decision-98) | The pinned baseline is the union of the recorded scope and the plan's, table by table. |
| 99 | [data/declared](decisions/data/declared.md#decision-99) | A row key has to be spellable in its key column's type. |
| 100 | [plan](decisions/plan.md#decision-100) | An object a statement creates enters the live identities the moment the statement commits. |
| 101 | [data/declared](decisions/data/declared.md#decision-101) | A declared text is refused before it is written unless the engine reads it back as written. |
| 102 | [roles](decisions/roles.md#decision-102) | A staged resume re-checks a role drop for the members whose statements have not run. |
| 103 | [data/declared](decisions/data/declared.md#decision-103) | `validate` refuses a key whose text no spelling of its type has. |
| 104 | [data/declared](decisions/data/declared.md#decision-104) | An integer outside what its column holds is refused offline. |
| 105 | [roles](decisions/roles.md#decision-105) | The permission read takes every class, and the ones the model does not hold are unexpressible drift. |
| 106 | [data/declared](decisions/data/declared.md#decision-106) | Two declared keys the engine reads as one row are refused before anything is written. |
| 107 | [policies](decisions/policies.md#decision-107) | A suppression's `until` is compared as it was validated. |
| 108 | [data/declared](decisions/data/declared.md#decision-108) | A plan that changes the type of a key column is refused while a declared key is spelled differently from the stored one. |
| 109 | [plan](decisions/plan.md#decision-109) | `bootstrap` refuses a declared object the identity file does not know. |
| 110 | [roles](decisions/roles.md#decision-110) | A permission the declarations cannot express stops every command that records a state, not only `plan --db`. |
| 111 | [policies](decisions/policies.md#decision-111) | `pull` draws its row-count line from the `data.max-rows` rule, not from `max_data_rows`. |
| 112 | [data/pre-delete](decisions/data/pre-delete.md#decision-112) | The pre-delete probe counts the rows a plan puts *onto* the parent, not only the ones already there. |
| 113 | [identity](decisions/identity.md#decision-113) | A revision `--since` or `--base` cannot resolve is refused, not read as an empty history. |
| 114 | [policies](decisions/policies.md#decision-114) | `pull` refuses to write when `data.max-rows` is `error`. |
| 115 | [data/declared](decisions/data/declared.md#decision-115) | `money` and `smallmoney` are read with conversion style 2. |
| 116 | [data/pre-delete](decisions/data/pre-delete.md#decision-116) | The pre-delete probe asks about every foreign key to the table, not only the ones that reference the key column. |
| 117 | [data/pre-delete](decisions/data/pre-delete.md#decision-117) | An inserted row carries the defaults of the columns it omits, and the pre-delete probe reads a defaulted write as an arrival. |
| 118 | [roles](decisions/roles.md#decision-118) | A declared role's name is checked against every database principal before a connected plan or a bootstrap is written. |
| 119 | [roles](decisions/roles.md#decision-119) | Which principal holds a role's name is the engine's call, for a rename's target as much as a creation's, and it is asked again before apply. |
| 120 | [policies](decisions/policies.md#decision-120) | `pull --data` draws its row line through `validate`'s own evaluation, not a count of its own. |
| 121 | [data/pre-delete](decisions/data/pre-delete.md#decision-121) | The pre-delete probe matches a composite foreign key as one tuple. |
| 122 | [data/row-writes](decisions/data/row-writes.md#decision-122) | A row `UPDATE` holds the row to what the plan recorded, and a row `UPDATE` or `DELETE` has to reach exactly one row. |
| 123 | [diff](decisions/diff.md#decision-123) | The names a plan's remaining statements need free are compared with one another, not only with the catalog. |
| 124 | [data/pre-delete](decisions/data/pre-delete.md#decision-124) | A write left to a default the probe cannot evaluate, on a column a foreign key to the deleted row's table spans, is refused. |
| 125 | [drift](decisions/drift.md#decision-125) | `status` reports a permission the declarations cannot hold as drift, as `verify` does. |
| 126 | [roles](decisions/roles.md#decision-126) | Two spellings of one grant target in a role file are refused, not merged. |
| 127 | [roles](decisions/roles.md#decision-127) | Roles dropped together are dropped parent before member. |
| 128 | [data/pre-delete](decisions/data/pre-delete.md#decision-128) | The pre-delete probe ignores the foreign keys the plan removes first. |
| 129 | [data/row-writes](decisions/data/row-writes.md#decision-129) | A row delete carries its own guard, under locks it keeps. |
| 130 | [data/row-writes](decisions/data/row-writes.md#decision-130) | The key-alias guard resolves the plan's table name through the identities first. |
| 131 | [data/declared](decisions/data/declared.md#decision-131) | Two declared keys are compared under the key column's collation, not the database's. |
| 132 | [data/row-writes](decisions/data/row-writes.md#decision-132) | A row write holds itself to what it wrote. |
| 133 | [data/row-writes](decisions/data/row-writes.md#decision-133) | A row write is held to the columns it left to their defaults too. |
| 134 | [doctor](decisions/doctor.md#decision-134) | A permission on a schema is probed before the plan runs. |
| 135 | [identity](decisions/identity.md#decision-135) | Two declarations whose files differ only in case are refused before either is written. |
| 136 | [data/row-writes](decisions/data/row-writes.md#decision-136) | A row write is held to the whole declared row, not to the cells it changes. |
| 137 | [data/row-writes](decisions/data/row-writes.md#decision-137) | A row write holds its cells by the rendering that reads them back, under a binary collation — an insert as an update. |
| 138 | [ledger](decisions/ledger.md#decision-138) | A version 3 state snapshot is still read, as an environment with no managed roles. |
| 139 | [roles](decisions/roles.md#decision-139) | The role drops are re-ordered after `plan --db` fills their members. |
| 140 | [data/row-writes](decisions/data/row-writes.md#decision-140) | A row update reads each cell by two types: the one the base recorded, and the one the column will have when the statement runs. |
| 141 | [identity](decisions/identity.md#decision-141) | Every command that reads declarations asks the same questions of them. |
| 142 | [roles](decisions/roles.md#decision-142) | A `schema::` grant target the database spells differently is refused, before a plan is written that could never converge. |
| 143 | [data/row-writes](decisions/data/row-writes.md#decision-143) | A row delete is keyed *and* held to the row the plan recorded. |
| 144 | [data/pre-delete](decisions/data/pre-delete.md#decision-144) | A disabled foreign key is not counted when a row is deleted. |
| 145 | [plan](decisions/plan.md#decision-145) | The artifact format versions reset to 1 at the first release. |
| 146 | [data/row-writes](decisions/data/row-writes.md#decision-146) | A cell whose column this plan retypes is carried, and held by nothing. |
| 147 | [apply-guard](decisions/apply-guard.md#decision-147) | The read-back and the ledger entry are inside the apply's own transaction. |
| 148 | [apply-guard](decisions/apply-guard.md#decision-148) | The spelling checks name the catalog's objects, not the plan's. |
| 149 | [types-and-probes](decisions/types-and-probes.md#decision-149) | A retyped column carries both of its types, and the engine converts between them. |
| 150 | [apply-guard](decisions/apply-guard.md#decision-150) | What `apply` records has to be the baseline plus the plan, and the part of that the tool can check exactly is everything the plan does not touch. |
| 151 | [types-and-probes](decisions/types-and-probes.md#decision-151) | The new foreign key is probed against the rows the plan will leave, not the ones it finds. |
| 152 | [types-and-probes](decisions/types-and-probes.md#decision-152) | A probe that unions rows names its columns and states their type. |
| 153 | [apply-guard](decisions/apply-guard.md#decision-153) | A table the plan touches is exempt down to the rows the plan names, and no further. |
| 154 | [policies](decisions/policies.md#decision-154) | A plan a policy refuses writes nothing, the identity file included. |
| 155 | [plan](decisions/plan.md#decision-155) | A baseline is read at the paths its own revision used. |
| 156 | [roles](decisions/roles.md#decision-156) | A role the plan touches is exempt down to the permissions it moves, and no further. |
| 157 | [roles](decisions/roles.md#decision-157) | A grant target is spelled the way the plan will leave it, before the roles are compared. |
| 158 | [apply-guard](decisions/apply-guard.md#decision-158) | Two more sides of the same rename, and one of a drop. |
| 159 | [plan](decisions/plan.md#decision-159) | A staged apply cannot roll back, so it stops instead — and `status` records rather than returns. |
| 160 | [apply-guard](decisions/apply-guard.md#decision-160) | What the plan itself wrote is checked, not excused — and "empty" is only safe where it is true. |
| 161 | [apply-guard](decisions/apply-guard.md#decision-161) | A postcondition is only fair once the statement has run, and only against the net result. |
| 162 | [apply-guard](decisions/apply-guard.md#decision-162) | Three places the guard looked, rather than three things it compared. |
| 163 | [apply-guard](decisions/apply-guard.md#decision-163) | A loop over the baseline never visits what the plan creates. |
| 164 | [modules](decisions/modules.md#decision-164) | A module leaves the managed set when its `DROP` runs, not when the plan is written — and an empty new parent is an answer. |
| 165 | [apply-guard](decisions/apply-guard.md#decision-165) | The cells a plan spells, and the difference between an empty table and an unspellable one. |
| 166 | [apply-guard](decisions/apply-guard.md#decision-166) | A touched table answers for the shape the plan leaves alone, and a historical path is composed rather than asked about. |
| 167 | [apply-guard](decisions/apply-guard.md#decision-167) | Three refinements of 166, and one of them is a gate that should never have been there. |
| 168 | [apply-guard](decisions/apply-guard.md#decision-168) | What the plan does to a table's parts is checked, the skip set knows which namespace it is in, and a failed read does not unfind what was already found. |
| 169 | [apply-guard](decisions/apply-guard.md#decision-169) | A postcondition is keyed by what it is about, not collected per change. |
| 170 | [apply-guard](decisions/apply-guard.md#decision-170) | Two questions that shared one answer, one question asked a row too late, and one hazard that turned out to be unrepresentable. |
| 171 | [apply-guard](decisions/apply-guard.md#decision-171) | A probe may only name what the catalog holds now — and a column this plan adds is not that. |
| 172 | [policies](decisions/policies.md#decision-172) | The editor schema spells the rule catalogue, because a schema that blesses what the loader refuses is worse than no schema. |
| 173 | [apply-guard](decisions/apply-guard.md#decision-173) | An exclusion the size of its reason: per field, not per column — and "narrow" includes NOT NULL. |
| 174 | [apply-guard](decisions/apply-guard.md#decision-174) | One read for the staged baseline, and a probe over the rows its own statement will meet. |
| 175 | [types-and-probes](decisions/types-and-probes.md#decision-175) | The key probes moved onto the relation the foreign key probe already used — and it subsumed 171's substitution. |
| 176 | [roles](decisions/roles.md#decision-176) | An unsupported permission on somebody else's object is somebody else's business, exactly as the ordinary one beside it is. |
| 177 | [pull](decisions/pull.md#decision-177) | A name is kept as the database spells it; `trim()` asks whether there is one, and nothing more. |
| 178 | [pull](decisions/pull.md#decision-178) | 177 one crate over, and the sweep that missed it. |
| 179 | [apply-guard](decisions/apply-guard.md#decision-179) | The read-back omitting a NULL excuses its absence, and nothing else. |
| 180 | [plan](decisions/plan.md#decision-180) | A historical tree is listed with `-z`, and the bug it hid was silence rather than an error. |
| 181 | [apply-guard](decisions/apply-guard.md#decision-181) | A table this plan creates answers for its shape, by name. |
| 182 | [apply-guard](decisions/apply-guard.md#decision-182) | A created table's `CREATE` payload is not everything it will hold. |
| 183 | [apply-guard](decisions/apply-guard.md#decision-183) | A part is not just a name, where the declaration says what it is. |
| 184 | [apply-guard](decisions/apply-guard.md#decision-184) | The last of the created table's parts: a foreign key's definition. |
| 185 | [apply-guard](decisions/apply-guard.md#decision-185) | A created column's stable fields, and the measurement that decided which ones they are. |
| 186 | [apply-guard](decisions/apply-guard.md#decision-186) | A created column's type is compared, normalized. |
| 187 | [policies](decisions/policies.md#decision-187) | A policy rule sees types in the dialect's spelling. |
| 188 | [policies](decisions/policies.md#decision-188) | Each rule's schema is that rule's own shape. |
| 189 | [apply-guard](decisions/apply-guard.md#decision-189) | A planned column or part is held to what the plan gives it, not to being there. |
| 190 | [apply-guard](decisions/apply-guard.md#decision-190) | A refusal names the remedy of the read that found the change. |
| 191 | [apply-guard](decisions/apply-guard.md#decision-191) | A cell the plan leaves to its default is held to being at it, at the closing read. |
| 192 | [drift](decisions/drift.md#decision-192) | `status` decides the row verdict before it writes the inventory, and lands a failed read last. |
| 193 | [connection](decisions/connection.md#decision-193) | `DbError` reports the server's error code as text. |
| 194 | [session](decisions/session.md#decision-194) | The transaction framing's text is the dialect's; `pbps-db` runs it. |
| 195 | [expressions](decisions/expressions.md#decision-195) | The shared definition scanner tracks block-comment depth. |
| 196 | [cli](decisions/cli.md#decision-196) | A message names a command only to a caller who can run it, and the target carries which one that is. |
| 197 | [cli](decisions/cli.md#decision-197) | A remedy spells the target the way its caller named it. |
| 198 | [process](decisions/process.md#decision-198) | A live test arranges its state over its own connection, in its own database. |
| 199 | [process](decisions/process.md#decision-199) | One helper owns a per-test database, and a guard drops it. |
| 200 | [modules](decisions/modules.md#decision-200) | A module is identified by a typed `ModuleId`, and which fields carry that identity depends on the kind. |
| 201 | [identity](decisions/identity.md#decision-201) | Namespace sharing and overloading are dialect questions, asked of the dialect. |
| 202 | [modules](decisions/modules.md#decision-202) | Routine identity is normalized by its own hook, and a collision is reported rather than merged. |
| 203 | [ledger](decisions/ledger.md#decision-203) | The state snapshot's oldest readable version becomes its current one. |
| 204 | [modules](decisions/modules.md#decision-204) | A guarantee the map key used to give is now a check, because removing the reason for one is not replacing it. |
| 205 | [modules](decisions/modules.md#decision-205) | A module whose name the id's string form cannot carry is inventoried, not recorded. |
| 206 | [process](decisions/process.md#decision-206) | CI is a gate started by hand, not feedback on every push. |
| 207 | [ledger](decisions/ledger.md#decision-207) | The state keeps what was declared beside what it read back, and a version 6 state reads as having declared nothing. |
| 208 | [diff](decisions/diff.md#decision-208) | The differ compares the declarations against what was declared when each object was last written, and falls back to the read-back where nothing was recorded. |
| 209 | [modules](decisions/modules.md#decision-209) | A binding is the dialect's to record, and SQL Server records none. |
| 210 | [roles](decisions/roles.md#decision-210) | `Permission` is the union of the engines' words, and each dialect refuses the ones its engine lacks — in three places, from one table. |
| 211 | [roles](decisions/roles.md#decision-211) | Role existence is a dialect capability, `Dialect::manages_roles`, and SQL Server's answer is `true`. |
| 212 | [modules](decisions/modules.md#decision-212) | Among the modules that share a routine's name, only `depends_on:` orders. |
| 213 | [cli](decisions/cli.md#decision-213) | The declaration schema says what the loader accepts, and completes from what `fmt` writes. |
| 214 | [cli](decisions/cli.md#decision-214) | The envelope's schema is published as one document with a branch per command, selected by `command`. |
| 215 | [cli](decisions/cli.md#decision-215) | `plan --db` stays outside the envelope set. |
| 216 | [cli](decisions/cli.md#decision-216) | `state list` carries the ledger's columns, never the recorded schema. |
| 217 | [cli](decisions/cli.md#decision-217) | A `--limit` too large saturates; it does not wrap and does not refuse. |
| 218 | [ledger](decisions/ledger.md#decision-218) | An entry this build cannot read is carried, not thrown. |
| 219 | [ledger](decisions/ledger.md#decision-219) | Presence is asked by attempting the statement, never by `OBJECT_ID`. |
| 220 | [cli](decisions/cli.md#decision-220) | An `unanswerable` envelope exits 1, and never 2. |
| 221 | [cli](decisions/cli.md#decision-221) | A table cell is escaped for the terminal; the JSON keeps the original. |
| 222 | [ledger](decisions/ledger.md#decision-222) | "Older than this build reads" and "damaged" are two answers, not one. |
| 223 | [cli](decisions/cli.md#decision-223) | A field `serde` may omit is a field `schemars` must call optional. |
| 224 | [cli](decisions/cli.md#decision-224) | The published envelope pins its own version, as it pins the command. |
| 225 | [connection](decisions/connection.md#decision-225) | `Conn` becomes an enum over two drivers — not a trait object, not a type parameter. |
| 226 | [expressions](decisions/expressions.md#decision-226) | `normalize_definition` takes a description of the engine's literals, and every dialect must supply one. |
| 227 | [types-and-probes](decisions/types-and-probes.md#decision-227) | `normalize_type`'s contract is stated on the trait, and the `serial` family is refused rather than normalized. |
| 228 | [connection](decisions/connection.md#decision-228) | One rustls crypto provider in the tree, and the connector names it anyway. |
| 229 | [connection](decisions/connection.md#decision-229) | The connection seam dials one TCP endpoint, and refuses every string that means anything else. |
| 230 | [identity](decisions/identity.md#decision-230) | Identifier rules are the engine's, measured, and neither is inherited from the SQL Server side. |
| 231 | [connection](decisions/connection.md#decision-231) | `target_session_attrs` is reproduced at the seam, not refused and not dropped. |
| 232 | [connection](decisions/connection.md#decision-232) | Opening the socket is one function, shared by both drivers, and it gives every resolved address a chance inside one budget. |
| 233 | [expressions](decisions/expressions.md#decision-233) | A dollar-quote tag follows the engine's grammar, which is over bytes. |
| 234 | [connection](decisions/connection.md#decision-234) | A TLS stack is built only when the connection may use one, and ALPN is offered only for direct SSL. |
| 235 | [data/declared](decisions/data/declared.md#decision-235) | Reference data is asked for its own DML, on the table, and only for what its declaration can emit. |
| 236 | [diff](decisions/diff.md#decision-236) | A unique index is gated and counted as the constraint it is, and a filtered one only over the rows its predicate keeps — or not at all. |
| 237 | [diff](decisions/diff.md#decision-237) | The constraint and index drops run before the column renames, all of them. |
| 238 | [plan](decisions/plan.md#decision-238) | The state fingerprint sorts a table's columns; the plan's does not. |
| 239 | [expressions](decisions/expressions.md#decision-239) | The gap around a dot is closed from the text already emitted, not from the text still to come. |
| 240 | [types-and-probes](decisions/types-and-probes.md#decision-240) | The PostgreSQL catalogue is closed, and every bound in it is the engine's own — including the two the engine does not enforce. |
| 241 | [types-and-probes](decisions/types-and-probes.md#decision-241) | The cost of a change stays out of `TypeChangeRisk`. |
| 242 | [types-and-probes](decisions/types-and-probes.md#decision-242) | A precision on `time` or `timestamp` is refused, because this model cannot hold the engine's own spelling of it. |
| 243 | [types-and-probes](decisions/types-and-probes.md#decision-243) | `Incompatible` is defined by a measured matrix, not by a rule. |
| 244 | [types-and-probes](decisions/types-and-probes.md#decision-244) | `Safe` is decided by what a type *holds*, not by how many digits it has. |
| 245 | [rename-impact](decisions/rename-impact.md#decision-245) | The dependency scan folds case for the whole alphabet, character for character, and is knowingly wider than the collation in three places. |
| 246 | [identity](decisions/identity.md#decision-246) | Two rename intents claiming one name are refused, before anything else is judged. |
| 247 | [pull](decisions/pull.md#decision-247) | A catalog row of a kind the reader does not know is reported, never folded into the nearest kind it does. |
| 248 | [pull](decisions/pull.md#decision-248) | A foreign key whose referential action the model cannot spell is left out and named, not read back as the nearest action it can. |
| 249 | [pull](decisions/pull.md#decision-249) | A foreign key's referenced columns are read out of `pg_get_constraintdef`, not resolved with a second catalog join. |
| 250 | [pull](decisions/pull.md#decision-250) | The whole catalog read is one `REPEATABLE READ READ ONLY` transaction, and the canonical search path is set inside it. |
| 251 | [pull](decisions/pull.md#decision-251) | A property of an object the model cannot hold means the object is left out; a fact about the rows already there means it is carried. |
| 252 | [pull](decisions/pull.md#decision-252) | A foreign key's referenced columns are resolved against the referenced table's own columns, which the pull already has. Supersedes 249. |
| 253 | [pull](decisions/pull.md#decision-253) | A pull inside the caller's own transaction is refused, not accommodated. |
| 254 | [pull](decisions/pull.md#decision-254) | The pull's canonical scope pins how values print, not only how names do. |
| 255 | [pull](decisions/pull.md#decision-255) | A name is round-tripped through the declaration format, not checked against a rule. |
| 256 | [pull](decisions/pull.md#decision-256) | The foreign keys are assembled in a second pass, after everything that could take their uniqueness away. |
| 257 | [pull](decisions/pull.md#decision-257) | The pull's own SQL carries no backslash escape. |
| 258 | [pull](decisions/pull.md#decision-258) | The declaration round trip asks for the same value, not for a value. |
| 259 | [session](decisions/session.md#decision-259) | The write `search_path` is set per statement, in the statement's own batch, and given back in the same one. |
| 260 | [session](decisions/session.md#decision-260) | The two settings that decide how a definition *parses* are pinned by the transaction framing, not by the statement. |
| 261 | [expressions](decisions/expressions.md#decision-261) | A bare-literal default on a setting-sensitive column is refused, offline, with the resolved spelling named. |
| 262 | [diff](decisions/diff.md#decision-262) | `online` builds an index concurrently only when it has no filter. |
| 263 | [types-and-probes](decisions/types-and-probes.md#decision-263) | A type change this engine refuses outright is refused by the emitter, with the clause named. |
| 264 | [session](decisions/session.md#decision-264) | The `DO` block's dollar-quote tag is chosen against the body it wraps. |
| 265 | [session](decisions/session.md#decision-265) | The write path's extras live on the dialect value, and the `pbps.yml` key waits for a reader. |
| 266 | [diff](decisions/diff.md#decision-266) | A nullable primary key column is refused on PostgreSQL too, and for the opposite reason. |
| 267 | [session](decisions/session.md#decision-267) | Every setting that changes what a declared expression means is pinned in the transaction framing, not around each statement. |
| 268 | [types-and-probes](decisions/types-and-probes.md#decision-268) | A type change the session's `TimeZone` would answer is refused, the way a `USING` clause is. |
| 269 | [diff](decisions/diff.md#decision-269) | A primary key that is only dropped is ordered with the constraint drops; one that is replaced is not. |
| 270 | [diff](decisions/diff.md#decision-270) | A replaced primary key is planned as two changes, its drop and its add. |
| 271 | [diff](decisions/diff.md#decision-271) | A column with a default and a new type is three phases, one rank each. |
| 272 | [diff](decisions/diff.md#decision-272) | `CREATE TABLE` names its access method, and does so in the statement. |
| 273 | [identity](decisions/identity.md#decision-273) | A table declared in a schema the pull never reads is refused offline. |
| 274 | [identity](decisions/identity.md#decision-274) | The two table names this tool owns are refused in every schema. |
| 275 | [session](decisions/session.md#decision-275) | `$user` is refused as a schema name and as a write-path extra. |
| 276 | [session](decisions/session.md#decision-276) | `pg_catalog` is left out of the write path, so it is searched first. |
| 277 | [session](decisions/session.md#decision-277) | `pg_catalog` is refused as a write-path extra, not dropped from the path. |
| 278 | [expressions](decisions/expressions.md#decision-278) | A comment is whitespace to the bare-literal guard, and the two comment forms are not the same whitespace. |
| 279 | [expressions](decisions/expressions.md#decision-279) | The grouping unwrap counts only the parentheses that are code. |
| 280 | [apply-guard](decisions/apply-guard.md#decision-280) | The apply guard keys a column's promises by field, and the last one wins. |
| 281 | [expressions](decisions/expressions.md#decision-281) | A declared expression is followed by a newline before any syntax the emitter owns, and a line ends at `\r` as much as at `\n`. |
| 282 | [expressions](decisions/expressions.md#decision-282) | Trailing whitespace and comments are stripped before the grouping test, by walking the expression forward. |
| 283 | [types-and-probes](decisions/types-and-probes.md#decision-283) | `timestamp` is classified as opaque, not as a binary type with the width `sys.types` reports. |
| 284 | [ledger](decisions/ledger.md#decision-284) | The PostgreSQL ledger lives in `public`, and the qualified names move out of `pbps-db` into the dialects. |
| 285 | [ledger](decisions/ledger.md#decision-285) | The lock is a table on this engine too, not an advisory lock. |
| 286 | [ledger](decisions/ledger.md#decision-286) | The lock is taken with `INSERT ... ON CONFLICT (id) DO NOTHING`, and zero rows affected is the refusal. |
| 287 | [ledger](decisions/ledger.md#decision-287) | The ledger's times are defaulted from `clock_timestamp()` and read through `to_char`, never cast. |
| 288 | [ledger](decisions/ledger.md#decision-288) | `is_initialized` attempts a statement here too, and this engine answers the three cases apart in the SQLSTATE. |
| 289 | [doctor](decisions/doctor.md#decision-289) | `doctor` asks about ownership on this engine, because no privilege authorizes DDL. |
| 290 | [session](decisions/session.md#decision-290) | A staged apply pins its session, through `Dialect::session_pins`. |
| 291 | [ledger](decisions/ledger.md#decision-291) | A reason is cut by characters here and by UTF-16 units there. |
| 292 | [ledger](decisions/ledger.md#decision-292) | `ensure_tables` treats a concurrent creator's failure as success. |
| 293 | [ledger](decisions/ledger.md#decision-293) | The ledger's DDL is not sent when there is nothing to create |
| 294 | [doctor](decisions/doctor.md#decision-294) | `doctor` asks for `USAGE` on a schema wherever objects in it are used, not only where they are created. |
| 295 | [diff](decisions/diff.md#decision-295) | A referenced foreign-key target may be a partitioned table; a managed table may not. |
| 296 | [ledger](decisions/ledger.md#decision-296) | Tolerating the creation race is not enough inside a transaction, so the `CREATE` runs under a savepoint. |
| 297 | [ledger](decisions/ledger.md#decision-297) | Every answer this ledger gives by tolerating an error is taken under a savepoint, and the tolerated `42P07` is verified rather than believed. |
| 298 | [types-and-probes](decisions/types-and-probes.md#decision-298) | A `timestamp` / `rowversion` declaration may be nullable, but its nullability may never be altered. |
| 299 | [types-and-probes](decisions/types-and-probes.md#decision-299) | A narrowing into a non-Unicode character type is probed by exact round trip, in addition to any character count. |
| 300 | [expressions](decisions/expressions.md#decision-300) | The PostgreSQL datum scanner recognizes national-character literals even where the current caller would reject them later. |
| 301 | [modules](decisions/modules.md#decision-301) | A routine argument type is its own text type, not a `ColumnType`. |
| 302 | [modules](decisions/modules.md#decision-302) | A PostgreSQL trigger's table is in its identity *and* in its definition, and a declaration where the two disagree is refused. |
| 303 | [modules](decisions/modules.md#decision-303) | A routine argument this dialect's catalogue does not know is passed through, not refused. |
| 304 | [modules](decisions/modules.md#decision-304) | A module whose deparsed statement this reader cannot cut is named and left out, never recorded with an empty body. |
| 305 | [pull](decisions/pull.md#decision-305) | Extension-owned objects are left out of the pull silently, and that is not the "absent, empty and unreadable" failure. |
| 306 | [modules](decisions/modules.md#decision-306) | On this dialect every carried attribute refuses the rebuild today, because there is no declared grant for one to come back from. |
| 307 | [modules](decisions/modules.md#decision-307) | The rebind test is a name and a path, not a position on it. |
| 308 | [modules](decisions/modules.md#decision-308) | A routine's parameter list is checked against its identity, and only where the disagreement is certain. |
| 309 | [modules](decisions/modules.md#decision-309) | A module the deparse could not find is the catalog moving, not a reader out of step with its query. |
| 310 | [modules](decisions/modules.md#decision-310) | A pull and a rebuild can deadlock, and the answer is a sentence rather than a lock order. |
| 311 | [modules](decisions/modules.md#decision-311) | The drop order for dependents is a topological order, not a depth. |
| 312 | [session](decisions/session.md#decision-312) | The transaction probe compares against a value it invented, not against a constant. |
| 313 | [modules](decisions/modules.md#decision-313) | A routine argument folds ASCII case only. |
| 314 | [rename-impact](decisions/rename-impact.md#decision-314) | `pg_depend` holds a row per column a dependent uses, not a row per dependent. |
| 315 | [rename-impact](decisions/rename-impact.md#decision-315) | The dependency scan lexes with the dialect's rules. |
| 316 | [rename-impact](decisions/rename-impact.md#decision-316) | A bare reserved word is not a reference. |
| 317 | [rename-impact](decisions/rename-impact.md#decision-317) | A bare name is a reference only where the engine would look it up. |
| 318 | [rename-impact](decisions/rename-impact.md#decision-318) | A quoting character doubled inside a name is one character of it. |
| 319 | [data/declared](decisions/data/declared.md#decision-319) | A declared value is rendered as an `E'…'` with its backslashes doubled, and a `bytea` as `decode('…','hex')` — an encoding rule, not a settings rule. |
| 320 | [data/pre-delete](decisions/data/pre-delete.md#decision-320) | The pre-delete probe counts every foreign key, and `convalidated` is never read. |
| 321 | [data/declared](decisions/data/declared.md#decision-321) | An identity-keyed `data:` block is refused on PostgreSQL, by `validate` and again by the emitter. |
| 322 | [data/declared](decisions/data/declared.md#decision-322) | The row read-back renders every column with one expression, `CAST(… AS text)`, and the canonical settings are what fix the spelling. |
| 323 | [expressions](decisions/expressions.md#decision-323) | A literal default is recognised through the cast the catalog welds on. |
| 324 | [data/declared](decisions/data/declared.md#decision-324) | The spelling queries are fenced with `OFFSET 0`, and the fence is load-bearing for exactly the types that fold. |
| 325 | [data/pre-delete](decisions/data/pre-delete.md#decision-325) | The pre-delete probe's dynamic SQL runs through `query_to_xml`. |
| 326 | [data/row-writes](decisions/data/row-writes.md#decision-326) | The delete's own guard locks the parent row, where SQL Server's locks a range of the child. |
| 327 | [data/declared](decisions/data/declared.md#decision-327) | Offline `validate` says it did not judge the row keys, rather than reporting clean. |
| 328 | [data/row-writes](decisions/data/row-writes.md#decision-328) | A row statement is a `DO` block, and its refusals are `RAISE EXCEPTION USING MESSAGE`. |
| 329 | [data/row-writes](decisions/data/row-writes.md#decision-329) | Every side of a comparison the engine will make goes through the engine's own type first, and every value a plan writes is one the probe can compare. |
| 330 | [data/row-writes](decisions/data/row-writes.md#decision-330) | The read of a defaulted cell asks the same question the write does, and the delete's own guard knows the row it is about to remove. |
| 331 | [data/row-writes](decisions/data/row-writes.md#decision-331) | A guard for a native `=` outlived every native `=` it guarded, and the probe this dialect ported kept only half of what 124 asks for. |
| 332 | [data/row-writes](decisions/data/row-writes.md#decision-332) | The key a write puts there is a cell, and is held to its exact spelling like every other one. |
| 333 | [data/pre-delete](decisions/data/pre-delete.md#decision-333) | The probe refuses an incomplete count too, and not only the delete's own guard. |
| 334 | [data/pre-delete](decisions/data/pre-delete.md#decision-334) | A relation is counted the way its foreign key covers it, and a key is asked about as the tuple it is. |
| 335 | [data/pre-delete](decisions/data/pre-delete.md#decision-335) | The pre-delete probe sees every key the delete will meet: the catalog's, the ones this session cannot count through, and the ones this plan adds. |
| 336 | [data/pre-delete](decisions/data/pre-delete.md#decision-336) | A key this plan adds on a column it also adds is counted through the value the column is added with, and a NULL the plan writes decides the tuple before an unevaluable default does. |
| 337 | [data/pre-delete](decisions/data/pre-delete.md#decision-337) | A planned key is backfilled on the referenced side as well, and a NULL the probe already holds is a NULL however it was spelled. |
| 338 | [data/pre-delete](decisions/data/pre-delete.md#decision-338) | A surviving parent row is the row this plan leaves there. |
| 339 | [data/pre-delete](decisions/data/pre-delete.md#decision-339) | A backfilled literal is compared through its column's type, and an identity column is a backfill no probe can evaluate. |
| 340 | [data/pre-delete](decisions/data/pre-delete.md#decision-340) | A planned key on a column this plan retypes compares the converted values, and a session that can read the columns the count reads can count. |
| 341 | [data/pre-delete](decisions/data/pre-delete.md#decision-341) | The survivors of a retyped referenced column are asked too, and every literal the probe compares to another literal goes through the column's type. |
| 342 | [data/pre-delete](decisions/data/pre-delete.md#decision-342) | A probe answers in `int4`, the width the runner reads; and a table the session may not reach through its schema is one it cannot count. |
| 343 | [data/pre-delete](decisions/data/pre-delete.md#decision-343) | A child this plan creates is a child whose arrivals are counted; a key column an insert leaves to the engine is refused; and a probe's answer is clamped before it is narrowed. |
| 344 | [data/pre-delete](decisions/data/pre-delete.md#decision-344) | A foreign key whose delete action will not run in this session is not one the delete meets. |
| 345 | [data/pre-delete](decisions/data/pre-delete.md#decision-345) | Every key this plan adds is asked about from the plan; the synthetic constraint row is gone. |
| 346 | [data/pre-delete](decisions/data/pre-delete.md#decision-346) | The delete's guard asks about the referencing relations, not the catalog's copies of their keys. |
| 347 | [data/pre-delete](decisions/data/pre-delete.md#decision-347) | Whether a written row's tuple holds a NULL is decided from that row, not from the table. |
| 348 | [data/pre-delete](decisions/data/pre-delete.md#decision-348) | A stored row's tuple is read from the row, too. |
| 349 | [data/pre-delete](decisions/data/pre-delete.md#decision-349) | A cell an update leaves alone is part of the tuple the update writes. |
| 350 | [expressions](decisions/expressions.md#decision-350) | A default spelled `CAST(x AS type)` is read through the cast, as `x::type` is. |
| 351 | [expressions](decisions/expressions.md#decision-351) | A default that is NULL is refused at the declaration, because this engine does not keep one. |
| 352 | [expressions](decisions/expressions.md#decision-352) | A stored key is compared as the engine's own check compares it: under the referenced column's collation, through the operator the constraint records. |
| 353 | [expressions](decisions/expressions.md#decision-353) | A planned key compares two stored columns under the referenced column's collation, spliced in by the engine. |
| 354 | [expressions](decisions/expressions.md#decision-354) | The parent's own readability is asked before the children are counted. |
| 355 | [expressions](decisions/expressions.md#decision-355) | A default is a literal in every spelling this engine reads one. |
| 356 | [expressions](decisions/expressions.md#decision-356) | `U&'…' UESCAPE '…'` is one literal. |
| 357 | [expressions](decisions/expressions.md#decision-357) | The cast scanners read a string the way the engine does, and the prefix test reads bytes. |
| 358 | [expressions](decisions/expressions.md#decision-358) | A comment in a default is whitespace to the reader of defaults, as it is to the engine. |
| 359 | [expressions](decisions/expressions.md#decision-359) | The cast scanners read a comment as a gap, wherever it stands. |
| 360 | [expressions](decisions/expressions.md#decision-360) | A default is a number in every spelling this engine reads one. |
| 361 | [expressions](decisions/expressions.md#decision-361) | A typed NULL default is refused only where the engine erases it: a NULL of the column's own unmodified type. |
| 362 | [expressions](decisions/expressions.md#decision-362) | A comment inside a type's text is the whitespace it is to the engine. |
| 363 | [expressions](decisions/expressions.md#decision-363) | The cast scanners end a line comment where the engine does, and read a comment as the gap around `AS`. |
| 364 | [expressions](decisions/expressions.md#decision-364) | The cast scanner ends a token where the lexer does, and the validator reads a cast type as the grammar spells it. |
| 365 | [expressions](decisions/expressions.md#decision-365) | A sign is read through the trivia, groupings and casts between it and its operand. |
| 366 | [data/pre-delete](decisions/data/pre-delete.md#decision-366) | A parent row this plan inserts meets an arriving child under the referenced column's collation. |
| 367 | [expressions](decisions/expressions.md#decision-367) | A typed literal is the constant it is. |
| 368 | [expressions](decisions/expressions.md#decision-368) | What follows a typed literal's string is an interval qualifier or nothing. |
| 369 | [expressions](decisions/expressions.md#decision-369) | A Unicode-escaped type name is the name it spells. |
| 370 | [roles](decisions/roles.md#decision-370) | PostgreSQL answers `manages_roles` with `false`, and the differ builds no `CreateRole`, `DropRole` or `RenameRole` on such a dialect. |
| 371 | [roles](decisions/roles.md#decision-371) | The engine's default ACL is the zero point: expanded, reported, and never compared as a grant. |
| 372 | [roles](decisions/roles.md#decision-372) | `GRANT … ON ROUTINE` is the only word that covers what this model calls a routine, and which word a bare object target takes is read off the permissions. |
| 373 | [roles](decisions/roles.md#decision-373) | A grant's routine signature is spelled with `unnest(proargtypes)`, not with `pg_get_function_identity_arguments`. |
| 374 | [process](decisions/process.md#decision-374) | `maintain` is gated on the connected server, and the live suite therefore runs two PostgreSQLs. |
| 375 | [process](decisions/process.md#decision-375) | A live permission test that runs as a superuser measures nothing. |
| 376 | [roles](decisions/roles.md#decision-376) | Every catalog that holds an `aclitem[]` is read, and the two kind alphabets are kept apart by construction. |
| 377 | [roles](decisions/roles.md#decision-377) | A rename is elided only where the *old* name is gone from the cluster. |
| 378 | [roles](decisions/roles.md#decision-378) | A routine's arguments travel as rows, never as a rendered signature. |
| 379 | [roles](decisions/roles.md#decision-379) | On PostgreSQL a bare grant target is read in the namespace its permissions name, not in the relations first. |
| 380 | [roles](decisions/roles.md#decision-380) | A grant is folded into a role only when the pull recorded the object it is on, and only when the target survives being written out. |
| 381 | [roles](decisions/roles.md#decision-381) | One spelling per engine for a routine grant, and it is the one the catalog gives back. |
| 382 | [roles](decisions/roles.md#decision-382) | The pull's own existence check reads the target's namespace too. |
| 383 | [roles](decisions/roles.md#decision-383) | The `public` schema is reachable without a grant on it, so §1 does not apply there. |
| 384 | [roles](decisions/roles.md#decision-384) | The managed-set cut reads a grant target's namespace too. |
| 385 | [pull](decisions/pull.md#decision-385) | A schema the pull does not read is a schema a declaration may not name. |
| 386 | [ledger](decisions/ledger.md#decision-386) | The ledger is hidden by name *and* by kind. |
| 387 | [types-and-probes](decisions/types-and-probes.md#decision-387) | A cast is not the assignment the `ALTER` performs, so the conversion probe measures the value. |
| 388 | [types-and-probes](decisions/types-and-probes.md#decision-388) | A `NaN` and an infinity sort greatest here rather than outside the order, so a range test finds them and a target that accepts one has to take it back out. |
| 389 | [types-and-probes](decisions/types-and-probes.md#decision-389) | The engine tests the value it would store, so the probe rounds first — the way that target rounds. |
| 390 | [types-and-probes](decisions/types-and-probes.md#decision-390) | A float target overflows at the midpoint above its largest value, and the threshold is written as the engine's own arithmetic. |
| 391 | [types-and-probes](decisions/types-and-probes.md#decision-391) | `NULLS DISTINCT` is this engine's rule and `GROUP BY`'s is the opposite, so the duplicate count excludes a key holding any NULL. |
| 392 | [types-and-probes](decisions/types-and-probes.md#decision-392) | A probe over a key spanning a column this plan narrows is not built. |
| 393 | [types-and-probes](decisions/types-and-probes.md#decision-393) | The orphan count compares under the referenced column's collation, spliced in from the catalog at the comparison site. |
| 394 | [rename-impact](decisions/rename-impact.md#decision-394) | What a rename breaks on this engine is invisible to the dependency graph, and what the graph holds is what survives. |
| 395 | [rename-impact](decisions/rename-impact.md#decision-395) | The objects a rename is carried into are reported, as their own list. |
| 396 | [rename-impact](decisions/rename-impact.md#decision-396) | Nothing blocks a rename on this engine, and the empty list says so. |
| 397 | [rename-impact](decisions/rename-impact.md#decision-397) | A module drop is not asked about here. |
| 398 | [rename-impact](decisions/rename-impact.md#decision-398) | A column the catalog does not have is an error, not an empty report. |
| 399 | [types-and-probes](decisions/types-and-probes.md#decision-399) | The estimate is a separate axis and carries no risk class. |
| 400 | [types-and-probes](decisions/types-and-probes.md#decision-400) | A rewrite is avoided only where the target constrains no byte already stored. |
| 401 | [types-and-probes](decisions/types-and-probes.md#decision-401) | Whether the table is rebuilt and whether every row is read are two facts, because one is invisible to the other. |
| 402 | [types-and-probes](decisions/types-and-probes.md#decision-402) | A foreign key locks the table nobody named. |
| 403 | [types-and-probes](decisions/types-and-probes.md#decision-403) | An unparsed default expression is `unknown`, never free. |
| 404 | [types-and-probes](decisions/types-and-probes.md#decision-404) | A shape the measurements never covered takes the answer back to `unknown`, whatever the static half said. |
| 405 | [types-and-probes](decisions/types-and-probes.md#decision-405) | `reltuples = -1` is "nobody has looked", not "no rows". |
| 406 | [types-and-probes](decisions/types-and-probes.md#decision-406) | A probe may only measure a rendering that every session renders alike. |
| 407 | [rename-impact](decisions/rename-impact.md#decision-407) | A column a plan renames is named to the catalog with *both* halves taken back. |
| 408 | [rename-impact](decisions/rename-impact.md#decision-408) | A scan for an identifier steps by a character, not a byte. |
| 409 | [types-and-probes](decisions/types-and-probes.md#decision-409) | An estimate names its table twice: as the plan has it, and as the catalog does. |
| 410 | [types-and-probes](decisions/types-and-probes.md#decision-410) | A retype is the one plan change that leaves a probe able to run and wrong, so a probe over a retyped table is skipped rather than allowed to answer. |
| 411 | [types-and-probes](decisions/types-and-probes.md#decision-411) | A binary float is measured in its own domain, never through `numeric`. |
| 412 | [types-and-probes](decisions/types-and-probes.md#decision-412) | The calendar probe takes `infinity` out by name, because the target keeps it. |
| 413 | [types-and-probes](decisions/types-and-probes.md#decision-413) | A created table's columns are remembered from the plan, because its key is the one column no row change types. |
| 414 | [types-and-probes](decisions/types-and-probes.md#decision-414) | The missing-value count is kept only for a value this crate can read without running anything. |
| 415 | [types-and-probes](decisions/types-and-probes.md#decision-415) | A probe pins its own session, and the allow-list of 406 widens to every type the catalogue holds. |
| 416 | [rename-impact](decisions/rename-impact.md#decision-416) | SQL Server rename impact takes both halves of a column name back to the catalog's spelling. |
| 417 | [connection](decisions/connection.md#decision-417) | The connected seam is a `match` on the connection's driver in `pbps-cli`, and the answers it routes are `pbps-db`'s types. |
| 418 | [apply-guard](decisions/apply-guard.md#decision-418) | The apply's read-back on PostgreSQL runs inside the apply transaction under a savepoint, and the command says which kind of read it wants. |
| 419 | [roles](decisions/roles.md#decision-419) | A completed cluster role rename changes the connected baseline's names, before its drift gate. |
| 420 | [modules](decisions/modules.md#decision-420) | PostgreSQL module rebuilds reach the carried-state check, on both sides of the DDL. |
| 421 | [roles](decisions/roles.md#decision-421) | An identity-only PostgreSQL deployment includes role additions and removals, not only renames. |
| 422 | [modules](decisions/modules.md#decision-422) | Rebinding is part of the typed diff, before ordering and approval. |
| 423 | [pull](decisions/pull.md#decision-423) | A transactional PostgreSQL catalog read has one statement snapshot and its managed recording is revalidated. |
| 424 | [pull](decisions/pull.md#decision-424) | Read absent PostgreSQL constraint flags under their older semantics. |
| 425 | [pull](decisions/pull.md#decision-425) | An introspection limitation keeps its object's namespace. |
| 426 | [pull](decisions/pull.md#decision-426) | An omitted PostgreSQL module is still in the unmanaged inventory. |
| 427 | [pull](decisions/pull.md#decision-427) | SQL Server's unreadable trigger still occupies a shared module name. |
| 428 | [doctor](decisions/doctor.md#decision-428) | PostgreSQL doctor distinguishes data privileges from grant authority. |
| 429 | [roles](decisions/roles.md#decision-429) | Permission support is checked on the server that will execute the plan. |
| 430 | [types-and-probes](decisions/types-and-probes.md#decision-430) | Connected cost is advisory output beside the plan, never part of its risk. |
| 431 | [rename-impact](decisions/rename-impact.md#decision-431) | Table and column drop impact follows catalog object addresses and removal order. |
| 432 | [plan](decisions/plan.md#decision-432) | Connected role and rebuild checks report their actual checked scope. |
| 433 | [types-and-probes](decisions/types-and-probes.md#decision-433) | The required-add-value scan's identifier boundary is the caller's, not always SQL Server's. |
| 434 | [connection](decisions/connection.md#decision-434) | The PostgreSQL seam honours the socket-tuning parameters 231 left open, rather than refusing them. |
| 435 | [cli](decisions/cli.md#decision-435) | `state list`'s timeline reads five new ledger columns, never `state_json`, in the steady state (issue #103). |
| 436 | [plan](decisions/plan.md#decision-436) | A staged checkpoint compares the rename that its statement performed. |
| 437 | [process](decisions/process.md#decision-437) | Retain settled spikes as evidence outside the production workspace. |
| 438 | [types-and-probes](decisions/types-and-probes.md#decision-438) | A `numeric` with a negative scale is judged by its granularity, not its magnitude alone, when the target is a binary float. |
| 439 | [doctor](decisions/doctor.md#decision-439) | `doctor`'s object-scope permission questions are resolved against the environment's own recorded name, not the declared one. |
| 440 | [doctor](decisions/doctor.md#decision-440) | `pbps_pg::doctor`'s `Needed::Referenced` falls back to the *declared* columns a key names, not the target's whole catalog. |
| 441 | [ledger](decisions/ledger.md#decision-441) | A relation of another kind occupying a ledger name is caught after `ensure_tables`'s DDL, not by narrowing `ledger_is_there`'s probe. |
| 442 | [data/row-writes](decisions/data/row-writes.md#decision-442) | Deleted rows carry dropped baseline cells in a separate review map. |
| 443 | [pull](decisions/pull.md#decision-443) | A column's collation is reported against the connected database's own default, not against every explicit `COLLATE`. |
| 444 | [identity](decisions/identity.md#decision-444) | A name containing pbps's own `.` separator is refused where it enters, not escaped in the serialized form. |
| 445 | [data/row-writes](decisions/data/row-writes.md#decision-445) | Reference-data writes do not authorize ambient PostgreSQL triggers. |
| 446 | [types-and-probes](decisions/types-and-probes.md#decision-446) | A column type's argument position is a word boundary in its base name. |
| 447 | [modules](decisions/modules.md#decision-447) | A module rebuild restates a declared role's grant because the plan says so, not because the declarations do. |
| 448 | [rename-impact](decisions/rename-impact.md#decision-448) | The rename-impact text-body scan folds an unquoted mention, never a quoted one, and only when the target's own name could have come from an unquoted spelling. |
| 449 | [types-and-probes](decisions/types-and-probes.md#decision-449) | A key this plan adds over a column it narrows excludes the rows its `CAST` would raise on, one row at a time — it does not skip the whole key. |
| 450 | [modules](decisions/modules.md#decision-450) | Trigger-function trust includes inherited ownership rights. |
| 451 | [data/row-writes](decisions/data/row-writes.md#decision-451) | The named table is not the write set: a row operation is guarded over the foreign keys whose actions write for it. |
| 452 | [data/declared](decisions/data/declared.md#decision-452) | Declaration key rules follow PostgreSQL's engine, not the sibling validator. |
| 453 | [identity](decisions/identity.md#decision-453) | An index — and the index behind a named primary key or unique constraint — is a third case of 201's rule, not a new one. |
| 454 | [connection](decisions/connection.md#decision-454) | `From<tokio_postgres::Error> for DbError` reads the server's own sentence off `as_db_error()`, not `Display`, and only `message()` plus the object identifiers — never `detail()`, `hint()` or `where_()`. |
| 455 | [connection](decisions/connection.md#decision-455) | What the operator's own terminal sees and what `pbps` writes down are deliberately different, from PR #464's apply failure path onward. |
| 456 | [connection](decisions/connection.md#decision-456) | Database error context is a separate frame, never part of the driver's message (issue #488). |
| 457 | [compose-and-ui](decisions/compose-and-ui.md#decision-457) | The local viewer is a synchronous CLI consumer (issue #117). |
| 458 | [session](decisions/session.md#decision-458) | SQL scripts carry the same session pins as deployment (issue #174). |
| 459 | [identity](decisions/identity.md#decision-459) | Declared constraint kinds share a table-local name check (issue #179). |
| 460 | [diff](decisions/diff.md#decision-460) | Replacing a referenced key carries its foreign keys through the typed plan (issue #177). |
| 461 | [diff](decisions/diff.md#decision-461) | SQL Server retypes preserve defaults and explicitly rebuild managed dependents (issue #180). |
| 462 | [types-and-probes](decisions/types-and-probes.md#decision-462) | An identity increment must fit PostgreSQL's directional sequence span (issue #181). |
| 463 | [diff](decisions/diff.md#decision-463) | A failed concurrent index build recovers its own invalid artifact (issue #186). |
| 464 | [session](decisions/session.md#decision-464) | The PostgreSQL write path names `pg_temp` once, after every project schema (issue #190). |
| 465 | [cli](decisions/cli.md#decision-465) | A published schema version identifies a fixed set of documents (issue #191). |
| 466 | [doctor](decisions/doctor.md#decision-466) | SQL Server foreign-key readiness uses the declared target-column union (issue #195). |
| 467 | [doctor](decisions/doctor.md#decision-467) | SQL Server doctor asks managed probe reads on each table (issue #194). |
| 468 | [data/pre-delete](decisions/data/pre-delete.md#decision-468) | SQL Server delete counts refuse active row filters and unreadable policy metadata (issue #208). |
| 469 | [data/declared](decisions/data/declared.md#decision-469) | Offline row-key spelling is a separate question from collisions (issue #211). |
| 470 | [data/pre-delete](decisions/data/pre-delete.md#decision-470) | SQL Server's delete guard excludes the doomed row from its own self-reference. |
| 471 | [data/row-writes](decisions/data/row-writes.md#decision-471) | This dialect's read of a defaulted cell asks the same question the write does, and the guard that asked about the type had nothing left to guard. |
| 472 | [data/row-writes](decisions/data/row-writes.md#decision-472) | SQL Server row writes hold assigned text and preserve existing key aliases (issue #218). |
| 473 | [modules](decisions/modules.md#decision-473) | A user constraint trigger is one trigger module, including its `CONSTRAINT` marker, rather than a second table constraint. |
| 474 | [diff](decisions/diff.md#decision-474) | The column drop that frees a name sorts before the rename that claims it. |
| 475 | [expressions](decisions/expressions.md#decision-475) | Definition layout follows the engine's whitespace class. |
| 476 | [types-and-probes](decisions/types-and-probes.md#decision-476) | A planned PostgreSQL key gets a collation compatibility probe even when its row values cannot be projected. |
| 477 | [rename-impact](decisions/rename-impact.md#decision-477) | PostgreSQL rename-impact advisories match complete identifier tokens after masking literals and comments. |
| 478 | [types-and-probes](decisions/types-and-probes.md#decision-478) | Connected PostgreSQL estimates own their catalog provenance. |
| 479 | [types-and-probes](decisions/types-and-probes.md#decision-479) | Nullability scans follow the statement and its surviving CHECKs. |
| 480 | [types-and-probes](decisions/types-and-probes.md#decision-480) | Narrowing projections exclude unconvertible rows, not whole keys. |
| 481 | [cli](decisions/cli.md#decision-481) | Explain's optional environment must use the saved plan's dialect. |
| 482 | [pull](decisions/pull.md#decision-482) | Omitted catalog objects have one managed scope and one fact per omission. |
| 483 | [doctor](decisions/doctor.md#decision-483) | Doctor distinguishes grant options from adopted ACL revocation authority. |
| 484 | [doctor](decisions/doctor.md#decision-484) | Doctor resolves data permissions by identity and predicts new-table ACLs. |
| 485 | [cli](decisions/cli.md#decision-485) | Connected JSON refusals retain the finding that answered the question. |
| 486 | [plan](decisions/plan.md#decision-486) | Constraint-drop approval distinguishes uniqueness from FK/CHECK relaxation (issue #241). |
| 487 | [compose-and-ui](decisions/compose-and-ui.md#decision-487) | Compose checks ref type before publishing and HEAD's immediate target before installing the index (issues #385 and #386). |
| 488 | [data/row-writes](decisions/data/row-writes.md#decision-488) | Operational row work is shared; storage mechanics remain engine-specific (issue #255). |
| 489 | [data/pre-delete](decisions/data/pre-delete.md#decision-489) | A referential-action lock follows the engine's inheritance boundary. |
| 490 | [resolver](decisions/resolver.md#decision-490) | Engine-assisted planning is optional infrastructure, but required evidence cannot be waived (accepted design; not implemented). |
| 491 | [pull](decisions/pull.md#decision-491) | The source-default collation reminder follows emitted character columns. |
| 492 | [resolver](decisions/resolver.md#decision-492) | Ship advisory resolver discovery before qualification, without a verified state. |
| 493 | [types-and-probes](decisions/types-and-probes.md#decision-493) | Unicode conversion probes measure the capacity ALTER enforces. |
| 494 | [resolver](decisions/resolver.md#decision-494) | Select a named resolver policy without acquiring or certifying it. |
| 495 | [connection](decisions/connection.md#decision-495) | Peer-verified TLS is a connection primitive, not resolver admission. |
| 496 | [diff](decisions/diff.md#decision-496) | Table renames and the drops that release their names form specific dependencies, and a moved drop carries its execution address. |
| 497 | [resolver](decisions/resolver.md#decision-497) | Docker resolver admission holds native runtime capabilities across a source-free bootstrap gate. |
| 498 | [resolver](decisions/resolver.md#decision-498) | Authenticate socket-activated Docker through its accepted Unix peer, not the listener creator's credentials. |
| 499 | [modules](decisions/modules.md#decision-499) | A module rebuild re-resolves its name once the lock is held (issue #232). |
| 500 | [connection](decisions/connection.md#decision-500) | A still-starting engine's refused login is retried; everything else it says is reported once (issue #638). |
| 501 | [process](decisions/process.md#decision-501) | CI is triggered by the pull request again; the gate is a check run, not a commit status. |
| 502 | [process](decisions/process.md#decision-502) | A merge queue on `master`, and the strict up-to-date policy off. |
| 503 | [modules](decisions/modules.md#decision-503) | A rebuild's lock pins an object; the `DROP` that follows names a qualified name, and the two halves of that name are held by different things (issues #547, #548). |
| 504 | [expressions](decisions/expressions.md#decision-504) | An expression is empty when the *engine's* lexis finds nothing in it, which is neither of Rust's whitespace classes (issues #480, #482). |
| 505 | [doctor](decisions/doctor.md#decision-505) | `doctor` asks for the delete count's policy-catalog read, at the database and on each securable an effective metadata `DENY` sits on; `VIEW SECURITY DEFINITION` is not what decides it (issues #522, #524). |
| 506 | [identity](decisions/identity.md#decision-506) | A foreign key's two column lists are two different rules, and its width is neither of them (issues #475, #476). |
| 507 | [identity](decisions/identity.md#decision-507) | A named primary key and a unique constraint of one table sharing a name is the table-local rule's to report, not the relation namespace's (issue #498). |
| 508 | [data/declared](decisions/data/declared.md#decision-508) | The identity-text exemption is about the conversion, and one key defeats it; SQL Server's note may not promise a spelling check it does not make (issues #526, #528). |
| 509 | [doctor](decisions/doctor.md#decision-509) | A foreign key's target is somebody else's table when the declarations do not hold it, whatever schema it sits in. |
| 510 | [doctor](decisions/doctor.md#decision-510) | `doctor` asks for the read that closes an apply, over the columns the declaration names. |
| 511 | [data/pre-delete](decisions/data/pre-delete.md#decision-511) | The delete count's children are found in the catalog, because that is where the probe finds them. |
| 512 | [doctor](decisions/doctor.md#decision-512) | A table moving between schemas is asked about at two securables, because the move drops the permissions on it. |
| 513 | [doctor](decisions/doctor.md#decision-513) | A key the declarations no longer name carries no demand, because the plan drops it before anything reads it. |
| 514 | [resolver](decisions/resolver.md#decision-514) | A supplied scratch server is a known container layout reached through the Docker profile's forwarder, not an arbitrary runtime proved contained from outside. |
| 515 | [types-and-probes](decisions/types-and-probes.md#decision-515) | A retype asks the catalog for the keys standing on its column, not only for the keys the plan drops (issue #503). |
| 516 | [types-and-probes](decisions/types-and-probes.md#decision-516) | The guard a narrowing key's probe carries is the condition of a `CASE`, not a conjunct beside the `CAST` it protects (issue #435). |
| 517 | [roles](decisions/roles.md#decision-517) | A routine arrives closed to `PUBLIC`, and a declaration is what opens it again (issue #318). |
| 518 | [roles](decisions/roles.md#decision-518) | A `REVOKE` carries one privilege, and revocability is decided per privilege. |
| 519 | [rename-impact](decisions/rename-impact.md#decision-519) | A rename impact report resolves its relation once, and an absent one is a refusal rather than an empty report. |
| 520 | [resolver](decisions/resolver.md#decision-520) | The analysis scope is qualified by comparing measured facts, reproducing the deployer, and reading loaded executable content — not by trusting versions (issue #610, ADR-0016 cases 5, 14, 16, 21, 23). |
| 521 | [resolver](decisions/resolver.md#decision-521) | SQL Server's analysis scope is qualified by the engine's own answers: a family gate, an edition limitation, mapped packages as the engine, and grants run only after the reproduction is verified (issue #611, ADR-0016 cases 5, 14, 16, 21, 23). |
| 522 | [resolver](decisions/resolver.md#decision-522) | A process walk passes over a child that is *over*, and refuses anything still alive. |
| 527 | [compose-and-ui](decisions/compose-and-ui.md#decision-527) | Compose owns an isolated candidate and a new output branch, not the source checkout. |
| 528 | [resolver](decisions/resolver.md#decision-528) | Observe a pinned PID-namespace procfs view without claiming a complete process tree (issue #740). |
| 529 | [resolver](decisions/resolver.md#decision-529) | Container admission checks tasks through their held namespace view; process identity and privilege exceptions do not use a bare PID (#741). |
| 530 | [compose-and-ui](decisions/compose-and-ui.md#decision-530) | A compose preview owns its input bytes and output tree before it owns confirmation (issue #745). |
| 531 | [resolver](decisions/resolver.md#decision-531) | The launch drops to a shared workload identity before bootstrap; the root deadline retains a separate, necessary authority (#742). |
| 532 | [compose-and-ui](decisions/compose-and-ui.md#decision-532) | A compose result records authorization separately from observation. |
| 533 | [resolver](decisions/resolver.md#decision-533) | A socket holder is observed; target identity is positively bound under trusted provisioning, not proved by an exhaustive census (#743). |
| 534 | [compose-and-ui](decisions/compose-and-ui.md#decision-534) | Compose network pushes rely on ordinary direct-branch server semantics. |
| 535 | [compose-and-ui](decisions/compose-and-ui.md#decision-535) | Compose retires acknowledged resources independently of publication. |
| 536 | [compose-and-ui](decisions/compose-and-ui.md#decision-536) | Completed capture rejection retires pre-check ownership without revoking an unexposed handle. |
| 537 | [session](decisions/session.md#decision-537) | A pre-flight probe runs in a read-only transaction of its own on PostgreSQL. |
| 538 | [data/row-writes](decisions/data/row-writes.md#decision-538) | A baseline with no primary key has its retained rows matched on the declared key, but only when the same plan restores that key on a column the baseline already has. |
| 539 | [data/row-writes](decisions/data/row-writes.md#decision-539) | A key restored on a column the baseline does not have is refused with its own diagnostic, not as a moved key. |
| 540 | [data/declared](decisions/data/declared.md#decision-540) | SQL Server floats are rendered through a bounded `varchar(99)` before widening to `nvarchar(max)`. |
| 541 | [compose-and-ui](decisions/compose-and-ui.md#decision-541) | A compose destination is a credential-free identity; authentication is reacquired per invocation, and an endpoint that cannot be represented without a secret is refused. |
| 542 | [resolver](decisions/resolver.md#decision-542) | The resolver qualifies inherited seccomp behavior before releasing its fixed bootstrap, without tracing processes (#633). |
| 543 | [connection](decisions/connection.md#decision-543) | A PostgreSQL connection string that names no `sslmode` is connected with verified TLS, not the driver's `prefer`. |
