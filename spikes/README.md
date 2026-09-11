# Retained decision evidence

These experiments stay in the repository so the original inputs, methods and
observations behind a decision can be inspected and re-run together. ADRs record
the conclusions; they do not replace the experiment that produced them
(DECISIONS 437).

| Directory | Evidence | Production boundary |
| --- | --- | --- |
| `yaml-span` | YAML parser/span evaluation for ADR-0001 | Cargo package excluded from the root workspace |
| `pg-driver` | Driver-seam experiment for ADR-0014 | Independent Cargo workspace, also explicitly excluded at the root |
| `pg-measurements` | SQL observations supporting the Phase 5 ADRs | SQL and scripts, no Cargo package; run manually |

They are historical experiments, not shipped components or workspace CI jobs.
The production crates and their live tests are the authority for current
behavior. Retaining an experiment does not make its dependencies part of the
product or promise that its historical output matches every later engine.

Re-run an experiment deliberately when revisiting its decision. Keep changed
observations visible and explain which engine or dependency version produced
them; do not silently turn historical evidence into a new baseline. The
[measurement README](pg-measurements/README.md) describes that experiment's
commands and recorded-output comparison.
