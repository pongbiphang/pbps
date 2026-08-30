# ADR-0001: serde-saphyr as the YAML crate

- Status: decided
- Date: 2026-08-30
- Related: docs/SPEC.md §11.3, §13.1
- Evaluation code: `spikes/yaml-span/` (removable now the choice is settled)

## Background

`pbps` declarations are YAML, and the linter's product value depends directly on
whether an error message can point at **the line that is wrong**. The original
spec listed the YAML crate as the first Phase 0 decision, with the criterion
stated plainly: errors must carry a span with a line number.

`serde_yaml` is unmaintained (0.9.34+deprecated), so something else had to be
chosen.

## Acceptance criteria

| # | Situation | Why it matters |
|---|---|---|
| A | Syntax error | The bare minimum |
| B | Type error | Must point at the **value**, not the start of the document |
| C | Unknown field | A misspelled field name is the most common user error |
| D | Duplicate key | Two columns with one name in a table; **accepting it silently causes silent data loss** |
| E | Span of a semantic error | The document is valid but a value is invalid for the domain (unknown type, duplicate uid). **The linter's core requirement** |
| F | The Norway problem | YAML 1.1 reads `no` / `yes` / `on` / `off` as booleans |

## Measured results

| Criterion | `serde-saphyr` 1.2 | `marked-yaml` 0.8 |
|---|---|---|
| A | `line 5 column 4` plus a source excerpt with a caret | `5:12` |
| B | `line 8 column 15: invalid boolean` — points at the value | `Value was not a boolean` — **no position** |
| C | `line 5 column 5: unknown field \`nullabel\`, expected one of type, nullable` | `Unknown field ...` — **no position** |
| D | `line 5 column 3: duplicate mapping key: email`, configurable via `DuplicateKeyPolicy` | **silently accepted, last one wins** |
| E | `line 7, column 11, span { offset: 102, len: 9 }` | `line 7, column 11`, but `end: None` |
| F | `no` works as a column name; `nullable: no` parses as `false` | not tested |

The `serde_yaml` forks (`serde_yaml_ng`, `serde_yaml_neo` and friends) were not
measured: they inherit `serde_yaml`'s architecture and have no `Spanned<T>`, so
criterion E cannot be satisfied by construction.

## Decision

**Adopt `serde-saphyr`.**

Two points decided it:

1. **E yields a complete `offset + len` span**, which builds directly into a
   `miette::SourceSpan`. `marked-yaml` gives only a start position, which cannot
   be underlined.
2. **D detects duplicate keys out of the box.** If two columns with one name were
   swallowed silently, the declarations and the database would diverge without a
   sound — which is precisely what this tool exists to prevent, and not something
   to patch over with a separate layer of our own checks.

A bonus: the errors for A through D already carry a source excerpt and a caret,
so diagnostic quality is close to hand-rolled miette output.

## Costs and mitigation

- **`serde-saphyr` is a young single-maintainer crate (1.2.0).** The supply-chain
  risk is real. The mitigation already exists in the architecture: all YAML access
  is sealed inside `pbps-load` and no other crate depends on it directly, so
  replacing it is a bounded piece of work.
- **The MSRV rises to 1.89** (`serde-saphyr`'s requirement), and the workspace
  `rust-version` follows.

## A format fix that came with it

The spike found that the field name `null` in spec §4.1 is **invalid YAML**:

```yaml
columns:
  customer_id:
    type: bigint
    null: false     # <- `null` is the YAML null literal, not the string "null"
```

`serde-saphyr` correctly reports `cannot deserialize null into string`.
`marked-yaml` was lenient and accepted it — and "some parsers accept it, some do
not" is exactly the kind of format to avoid.

**Renamed to `nullable`**, which is also closer to SQL's own vocabulary:

```yaml
columns:
  customer_id:
    type: bigint
    nullable: false
```

## A Phase 1 requirement that came with it

F shows that `no` parses as a boolean. If one of our string fields
(`description`, a `deprecated` reason, `type`) happens to receive a bare `no` /
`yes` / `on` / `off` / `~` or a number-shaped value, the result is a type error —
a loud failure rather than silent corruption, which is acceptable.

But **`pbps fmt` must quote proactively when writing string scalars**, covering
the boolean-ish literals (`true`/`false`/`yes`/`no`/`on`/`off`/`y`/`n` and their
case variants), the null-ish ones (`null`/`~`), and any string that parses as a
number. Otherwise the files the tool writes itself would come back as different
types on the next read.
