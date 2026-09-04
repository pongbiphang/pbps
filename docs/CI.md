# Pipelines

Complete, copy-pastable pipelines for GitHub Actions and GitLab CI, and the
contract they rest on. SPEC 14.1 lists this as the answer to "bootstrap CI
without transcribing documentation": the pipeline in [SPEC §10](SPEC.md) shows
the *shape* of the flow, and this file is the thing you paste.

**No generator is shipped, deliberately.** A generated pipeline that has since
been edited can never be upgraded, so the generator ends up maintained for
nobody (SPEC 14.1). These are examples you own from the moment you paste them.

Every command, flag and exit code below was checked against the binary rather
than transcribed from a design document — including one place where SPEC §10 is
now out of date, noted in "Credentials" below.

## The contract a pipeline branches on

Three exit codes, one meaning each, the same across every read-only command:

| Code | Means | A pipeline should |
|---|---|---|
| `0` | success, nothing to report | continue |
| `2` | a **finding** — the command ran, and the answer is bad news | fail the job, and show the finding |
| `1` | a **tool failure** — the command could not answer at all | fail the job, and page whoever owns the pipeline |

The split matters because the two failures have different owners: a `2` from
`pbps validate` belongs to the author of the change, and a `1` from
`pbps verify` usually means the database was unreachable, which does not.

Verified by running them: `validate` on a clean project exits `0`, on a
declaration with an unknown type `2`, and outside any project `1`;
`fmt --check` exits `2` on a file that is not in canonical form.

Add `--format json` to any read-only command for the same findings as typed
data — the shape a job summary or a code-review annotation should be built
from. **Vendor-native annotations are produced by a converter in `scripts/`,
never by the binary** (SPEC 14.3): the JSON is the stable surface, and a
converter can be rewritten for a platform the binary has never heard of.

## Credentials

Declare each environment in `pbps.yml` with the **name** of the variable
holding its connection string, never the string:

```yaml
environments:
  prod:
    url_env: PBPS_PROD_URL
  staging:
    url_env: PBPS_STAGING_URL
```

Then every command takes `--env prod`, and the secret reaches it through the
environment.

**Prefer this to `--db "$PBPS_PROD_URL"`, which SPEC §10's older example uses.**
A connection string on the command line is visible in the process table to
every other process on the runner, and it lands in the trace of any job that
runs under `set -x`. `url_env` predates neither problem by accident: `pbps.yml`
has no `url:` field for the same reason.

| Secret | Used by | Notes |
|---|---|---|
| `PBPS_PROD_URL` | `plan --env prod`, `verify`, `apply`, `snapshot`, `status` | ADO.NET form: `Server=host,1433;Database=app;User Id=u;Password=p;TrustServerCertificate=true` |
| `PBPS_STAGING_URL` | the same, for staging | a separate account, with the same permissions |
| `APPROVED_PLAN_SHA256` | `apply --checksum` | not a secret; it is the approval, supplied by whoever approved |

`pbps doctor --env prod` is the one command to run first: it answers whether
that account can actually deploy — reachability, edition, the minimum
permission set, ledger and lock access — instead of discovering each at a
different step later.

## Installing pbps in CI

There is **no tagged release yet** (the workspace is `0.0.0`), so a pipeline
builds it from source and caches the result:

```yaml
- run: cargo build --release --locked -p pbps-cli
- run: echo "$PWD/target/release" >> "$GITHUB_PATH"
```

When releases exist, replace both lines with whatever installs the binary; the
rest of this file does not change. This is the one step here that is temporary,
and it is called out rather than hidden so that it gets replaced.

## The two layers

The flow has two layers and they answer different questions (SPEC §7.3), which
is why the pipeline has two plan-shaped stages rather than one:

- **The merge-request layer** is offline. It answers *do we want this change?*
  and needs no database at all — `plan --check`, `validate`, `fmt --check`, and
  a preview for the reviewer.
- **The deployment layer** runs at tag time against the target environment as
  queried. It answers *what will this do to prod?* The artifact it produces is
  what the approver reads, and `apply` runs only that artifact.

`apply` is manual, and what the human confirms is the deployment-layer plan —
not whether the change is a good idea. That was settled in the merge request.

## GitHub Actions

`.github/workflows/schema.yml`:

```yaml
name: schema

on:
  pull_request:
  push:
    tags: ['prod-v*']

permissions:
  contents: read

jobs:
  # ---- the merge-request layer: offline, no database, no secrets ----
  check:
    if: github.event_name == 'pull_request'
    runs-on: ubuntu-latest
    env:
      PBPS_BASE_SHA: ${{ github.event.pull_request.base.sha }}
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0          # `plan --since` needs history, not a shallow clone
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --release --locked -p pbps-cli
      - run: echo "$PWD/target/release" >> "$GITHUB_PATH"

      - name: declarations are canonical
        run: pbps fmt --check
      - name: declarations are valid
        run: pbps validate
      - name: no unresolved rename or drop intent
        run: pbps plan --check --since "$PBPS_BASE_SHA"

      - name: preview for the reviewer
        run: pbps plan --since "$PBPS_BASE_SHA" --out preview.json --sql preview.sql
      - uses: actions/upload-artifact@v4
        with:
          name: preview
          path: |
            preview.json
            preview.sql

  # ---- the deployment layer: on a tag, against prod as queried ----
  plan:
    if: startsWith(github.ref, 'refs/tags/prod-v')
    runs-on: ubuntu-latest
    env:
      PBPS_PROD_URL: ${{ secrets.PBPS_PROD_URL }}
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --release --locked -p pbps-cli
      - run: echo "$PWD/target/release" >> "$GITHUB_PATH"

      - name: the account can deploy
        run: pbps doctor --env prod
      - name: prod matches its recorded state
        run: pbps verify --env prod     # drift is a finding, so this exits 2 (SPEC 14.1)
      - name: the plan for this environment
        run: pbps plan --env prod --out plan.json --sql plan.sql
      - name: what the approver reads
        run: pbps explain --plan plan.json      # prints the SHA-256 and the exact apply command
      - uses: actions/upload-artifact@v4
        with:
          name: plan
          path: |
            plan.json
            plan.sql

  # ---- the gate: `environment:` is what makes GitHub ask a human ----
  apply:
    needs: plan
    if: startsWith(github.ref, 'refs/tags/prod-v')
    runs-on: ubuntu-latest
    environment: production        # configure required reviewers on this environment
    env:
      PBPS_PROD_URL: ${{ secrets.PBPS_PROD_URL }}
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --release --locked -p pbps-cli
      - run: echo "$PWD/target/release" >> "$GITHUB_PATH"
      - uses: actions/download-artifact@v4
        with: { name: plan }

      - name: apply exactly the approved artifact
        run: |
          pbps apply --env prod \
            --plan plan.json \
            --checksum "${{ vars.APPROVED_PLAN_SHA256 }}" \
            --allow rename,narrowing
      - name: record the new state
        run: pbps snapshot --env prod
```

Two things about the `apply` job are load-bearing:

- **`--checksum` is required**, not optional. It pins the artifact to the one a
  human read: `pbps explain --plan plan.json` prints the SHA-256 and the exact
  `pbps apply --env … --plan … --checksum …` line to run. **A checksum computed
  by the same job that produced the plan approves nothing** — it would confirm
  only that a file equals itself — so the value has to come from the approver.

  GitHub's `environment:` gate approves *the run*, not the artifact, and gives
  the reviewer nowhere to type a value. Two honest ways to close that:

  ```yaml
  # (a) the approver dispatches the deployment and pastes what `explain` printed
  on:
    workflow_dispatch:
      inputs:
        checksum: { description: 'SHA-256 from pbps explain', required: true }
        allow:    { description: 'Risk classes approved', default: '' }
  # ...then: --checksum "${{ inputs.checksum }}" --allow "${{ inputs.allow }}"
  ```

  ```yaml
  # (b) the reviewer records it as a repository variable before approving,
  #     and the environment gate stops the job until they have.
  #     --checksum "${{ vars.APPROVED_PLAN_SHA256 }}"
  ```

  (a) is the better shape: the value travels with the approval instead of
  beside it. The job below shows (b) because it is the smaller diff from a
  pipeline that already exists — swap it once the flow is familiar. GitLab
  needs neither, since a manual job takes variables at start time.
- **`--allow` names the risk classes that were approved.** It is the gate, and
  the list belongs in the pipeline only if this environment genuinely accepts
  those classes every time; otherwise let the approver supply it too.

## GitLab CI

`.gitlab-ci.yml`:

```yaml
stages: [check, plan, apply, record]

default:
  image: rust:1-bookworm
  cache:
    key: cargo
    paths: [.cargo/, target/]
  before_script:
    - export CARGO_HOME="$PWD/.cargo"
    - cargo build --release --locked -p pbps-cli
    - export PATH="$PWD/target/release:$PATH"

variables:
  GIT_DEPTH: 0                     # `--since` needs history

# ---- the merge-request layer ----
check:
  stage: check
  rules:
    - if: $CI_PIPELINE_SOURCE == "merge_request_event"
  script:
    - pbps fmt --check
    - pbps validate
    - pbps plan --check --since "$CI_MERGE_REQUEST_DIFF_BASE_SHA"
    - pbps plan --since "$CI_MERGE_REQUEST_DIFF_BASE_SHA" --out preview.json --sql preview.sql
  artifacts:
    paths: [preview.sql, preview.json]
    expire_in: 1 week

# ---- the deployment layer ----
plan:prod:
  stage: plan
  rules:
    - if: $CI_COMMIT_TAG =~ /^prod-v/
  script:
    - pbps doctor --env prod
    - pbps verify --env prod
    - pbps plan --env prod --out plan.json --sql plan.sql
    - pbps explain --plan plan.json
  artifacts:
    paths: [plan.json, plan.sql]    # what the gate's approver actually reads
    expire_in: 1 month

apply:prod:
  stage: apply
  needs: ['plan:prod']
  rules:
    - if: $CI_COMMIT_TAG =~ /^prod-v/
      when: manual                  # the approval gate, not a decision point
  script:
    - pbps apply --env prod --plan plan.json
        --checksum "$APPROVED_PLAN_SHA256"
        --allow rename,narrowing

record:prod:
  stage: record
  needs: ['apply:prod']
  rules:
    - if: $CI_COMMIT_TAG =~ /^prod-v/
  script:
    - pbps snapshot --env prod
```

`PBPS_PROD_URL` is a masked, protected CI/CD variable; `APPROVED_PLAN_SHA256`
is supplied by the approver when they run the manual job.

## Drift watch

Monitoring is a scheduled pipeline, not a service. The state never leaves your
database and nothing is hosted.

```yaml
# GitHub Actions — .github/workflows/drift.yml
on:
  schedule: [{ cron: '0 * * * *' }]

jobs:
  drift:
    runs-on: ubuntu-latest
    env:
      PBPS_PROD_URL: ${{ secrets.PBPS_PROD_URL }}
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --release --locked -p pbps-cli
      - run: echo "$PWD/target/release" >> "$GITHUB_PATH"
      - run: pbps verify --env prod --format json
```

`verify` exits `2` on drift, which fails the job. The `on_drift` hook has
already delivered by then — a webhook, a chat message, a ticket; its command
decides (SPEC §13.5). The scheduler is your CI and the delivery is your webhook.

`pbps status --format json` is the same idea across every configured
environment at once: last apply, git sha, drift state, last verified.

## Things that will bite

- **Shallow clones.** `plan --since <base>` needs that base revision in the
  checkout. Use `fetch-depth: 0` / `GIT_DEPTH: 0`.
- **The base SHA only exists inside a review pipeline.** That is why the check
  job is gated on the event (`if:` / `rules:` above). Outside one the variable
  is empty, and an empty `--since` is not refused: it plans against an empty
  baseline, so every table reads as newly created. It exits `0` with a warning,
  which is exactly the kind of green a pipeline should never be built on.
- **`--no-input` is global and safe to add everywhere.** It declines prompts and
  can never answer one — no flag may supply rename or drop intent (SPEC 14.3) —
  so it only ever makes a run more conservative. A non-interactive runner
  already behaves this way; setting it makes the intent explicit.
- **`verify` before `plan --env`, not after.** Planning against an environment
  that has drifted plans against a surprise. Exit `2` there is the pipeline
  working.
- **Never echo a connection string.** No `set -x` in a job that touches one, and
  no `--db` on a command line. `pbps` redacts connection strings in its own
  output; your shell does not.
- **A staged plan is resumable, and that is not automatic.** If `apply --staged`
  stops midway, the next run needs `--resume`; SPEC §7.5's all-or-nothing
  promise covers the transactional path, and the staged path trades it for
  operations no transaction can hold (ADR-0003).
- **`state prune --keep N` on a schedule**, or the ledger grows forever.

## What is deliberately not here

- **A generator** (SPEC 14.1) — see the top of this file.
- **A `push` command.** `plan` then `apply --plan` stays two steps, in CI as
  everywhere else (SPEC 14.3).
- **Vendor-native annotations from the binary.** `--format json` plus a
  converter in `scripts/`; the binary learns no vendor's output format.
- **A first-party Action or CI component.** SPEC 14.1 lists one as **P2**, and
  it differs from a generator in the one way that matters: it is *referenced* by
  version, so a fix reaches every user. It is not built yet, and these examples
  are what it would wrap.
