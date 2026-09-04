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

Three exit codes, one meaning each, across the commands a pipeline gates on —
`validate`, `fmt --check`, `plan`, `verify`, `doctor`:

| Code | Means | A pipeline should |
|---|---|---|
| `0` | the command ran and found **no error** — it may still have reported warnings or notes | continue, but read the findings |
| `2` | at least one **error** finding — the command ran, and the answer is bad news | fail the job, and show the finding |
| `1` | a **tool failure** — the command could not answer at all | fail the job, and page whoever owns the pipeline |

`0` is not "nothing to report": only an *error* finding moves the code. Measured
— `validate` on a project whose declared rows exceed `max_data_rows` exits `0`
with a `schema.data-large` **warning**, and an offline `plan` against an empty
baseline exits `0` with `baseline.empty`. Both are things a reviewer should see.
A pipeline that routes findings rather than only gating on them should read
`--format json` on every run, not just on the failing ones.

The split matters because the two failures have different owners: a `2` from
`pbps validate` belongs to the author of the change, and a `1` from
`pbps verify` usually means the database was unreachable, which does not.

**`status` and `explain` are outside it, and that is not an accident.** Both
are *reports*: they describe a situation rather than judging it, so they exit
`0` and put what they found in the report.

- `pbps status` covers every configured environment in one pass — drifted,
  unreachable, mid-deployment — because one unreachable database must not cost
  the operator the other five lines. Measured: an unreachable environment gives
  exit `0`, `"result": "ok"`, and an `environment.unreachable` warning in the
  findings.
- `pbps explain` describes a plan for the human who has to approve it, and says
  so where the plan cannot be applied at all: measured, `explain --plan` on an
  offline preview prints `origin  computed offline — a preview; apply will
  refuse it` and still exits `0`. The refusal belongs to `apply`, which is
  where it happens.

So never branch a pipeline on either one's exit code. Read the JSON, or gate on
`verify` — the command whose answer *is* a gate.

Verified by running them: `validate` on a clean project exits `0`, on a
declaration with an unknown type `2`, and outside any project `1`;
`fmt --check` exits `2` on a file that is not in canonical form.

Add `--format json` to a read-only command for the same findings as typed data
— the shape a job summary or a code-review annotation should be built from.
**Vendor-native annotations are produced by a converter in `scripts/`, never by
the binary** (SPEC 14.3): the JSON is the stable surface, and a converter can be
rewritten for a platform the binary has never heard of.

One command refuses it, on purpose: a **connected** plan. `pbps plan --env prod
--format json` exits `1` with `flags.conflicting` — measured — because
`--format json` describes findings while `plan --db` produces a plan, and the
two are different artifacts. The tool says what to do instead, and so does this
file: write the plan with `--out plan.json`, then read it with
`pbps explain --plan plan.json --format json`.

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
| `PBPS_PROD_URL` | `plan --env prod`, `verify`, `apply`, `status` | ADO.NET form: `Server=host,1433;Database=app;User Id=u;Password=p;TrustServerCertificate=true`. An **environment** secret / **protected** variable — see "Who can reach the credential" |
| `PBPS_STAGING_URL` | the same, for staging | a separate account, with the same permissions |
| `PBPS_PROD_URL` on `monitoring` | `verify`, `status` | the drift watch's copy. `verify` reads and writes nothing, so this one is a **read-only** account |
| the plan's SHA-256 | `apply --checksum` | not a secret; it is the approval. A `workflow_dispatch` input on GitHub, a manual-job variable on GitLab — supplied by whoever approved, at the moment they approve |

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
  workflow_dispatch:            # the approver starts the apply, carrying their approval
    inputs:
      plan_run: { description: 'Run ID of the plan job for this tag', required: true }
      checksum: { description: 'SHA-256 printed by pbps explain', required: true }
      allow:    { description: 'Risk classes approved, comma-separated; empty for none' }

permissions:
  contents: read
  actions: read                 # to download the plan artifact from that run

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
    # The event as well as the ref: the approver dispatches the apply *from
    # this same tag*, and a ref-only condition would start a second plan
    # against production alongside the deployment being approved.
    if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/prod-v')
    runs-on: ubuntu-latest
    # An environment with no reviewers, whose deployment *tag* rule is
    # `prod-v*`: that is what makes the credential below an environment secret
    # released only to a job on such a tag, rather than a repository secret any
    # workflow run can read. See "Who can reach the credential" below.
    environment: production-plan
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

  # ---- what the approver reads, before the gate opens ----
  #      No environment, so no credential: this job only assembles the facts.
  prepare:
    if: github.event_name == 'workflow_dispatch'
    runs-on: ubuntu-latest
    env:
      GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
      PLAN_RUN: ${{ inputs.plan_run }}
      CHECKSUM: ${{ inputs.checksum }}
      ALLOW: ${{ inputs.allow }}
    steps:
      # Dispatch from the same `prod-v*` tag the plan was computed on.
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --release --locked -p pbps-cli
      - run: echo "$PWD/target/release" >> "$GITHUB_PATH"

      # The run id is text somebody typed, and `apply` checks the plan's
      # checksum and the live baseline — not which commit produced the plan.
      # Two tags planned against the same baseline are both applyable, so the
      # artifact is bound to the tag being dispatched before it is fetched.
      - name: the plan must be this tag's
        run: |
          plan_sha=$(gh api "repos/$GITHUB_REPOSITORY/actions/runs/$PLAN_RUN" --jq .head_sha)
          if [ "$plan_sha" != "$GITHUB_SHA" ]; then
            echo "run $PLAN_RUN planned $plan_sha; this dispatch is at $GITHUB_SHA" >&2
            exit 1
          fi
      - uses: actions/download-artifact@v4
        with:
          name: plan
          run-id: ${{ inputs.plan_run }}
          github-token: ${{ secrets.GITHUB_TOKEN }}

      # Anyone with write access can dispatch, and the reviewer approving the
      # run is not always that person. So what they are approving is put in
      # front of them rather than left in the inputs panel.
      - name: say what this deployment is
        run: |
          # Everything below the heading is indented four spaces, which makes
          # it one code block that its own content cannot close. `explain`
          # prints declaration text — a deprecation reason, a declared row —
          # and the three inputs are text somebody typed; a fence here would
          # be closable from inside, and the summary is the control the
          # approver reads.
          {
            echo "## Deploying"
            echo
            {
              echo "tag:       $GITHUB_REF_NAME"
              echo "checksum:  $CHECKSUM"
              echo "allow:     ${ALLOW:-none}"
              echo "plan run:  $PLAN_RUN"
              echo
              pbps explain --plan plan.json
            } | sed 's/^/    /'
          } >> "$GITHUB_STEP_SUMMARY"

  # ---- the gate: `environment:` is what makes GitHub ask a human ----
  apply:
    needs: prepare
    if: github.event_name == 'workflow_dispatch'
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

      # The same binding again, because this job is the one holding the
      # credential and `needs:` only orders jobs — it does not carry a check.
      - name: the plan must be this tag's
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          PLAN_RUN: ${{ inputs.plan_run }}
        run: |
          plan_sha=$(gh api "repos/$GITHUB_REPOSITORY/actions/runs/$PLAN_RUN" --jq .head_sha)
          if [ "$plan_sha" != "$GITHUB_SHA" ]; then
            echo "run $PLAN_RUN planned $plan_sha; this dispatch is at $GITHUB_SHA" >&2
            exit 1
          fi
      - uses: actions/download-artifact@v4
        with:
          name: plan
          run-id: ${{ inputs.plan_run }}
          github-token: ${{ secrets.GITHUB_TOKEN }}

      - name: apply exactly the approved artifact
        env:
          # Through the environment, never interpolated into the script: an
          # input is text somebody typed.
          CHECKSUM: ${{ inputs.checksum }}
          ALLOW: ${{ inputs.allow }}
        run: |
          set -- --env prod --plan plan.json --checksum "$CHECKSUM"
          # `--allow ""` is rejected — "unknown risk class ``" — so the flag
          # goes in only when the approver named a class.
          if [ -n "$ALLOW" ]; then set -- "$@" --allow "$ALLOW"; fi
          pbps apply "$@"
```

### Who can reach the credential

The `apply` job is behind `environment: production` with required reviewers, and
that is the approval gate. It is not, by itself, the credential boundary: the
`plan` job runs first, builds this repository from the tagged commit, and needs
a deploy-capable connection string to query production at all. So whoever can
create a `prod-v*` tag can run code of their choosing with that credential in
its environment, before any reviewer sees anything.

Three settings make that boundary deliberate rather than accidental, and none
of them is optional:

- **`PBPS_PROD_URL` is an environment secret, not a repository secret**, on both
  `production-plan` and `production`. A repository secret is readable by any
  workflow run that names it; an environment secret is released only to a job
  that references that environment, after its protection rules pass
  ([GitHub docs](https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/manage-environments#environment-secrets)).
- **Both environments carry a deployment tag rule of `prod-v*`**, so neither
  releases anything to a job on some other ref.
- **A repository ruleset restricts who may create *and update* `prod-v*` tags.**
  The tag push is what starts all of this; without those rules the two settings
  above only decide *which job* gets the credential, never *who* set it running.
  Both rules, not one: GitHub counts restricting creations and restricting
  updates as [separate rules](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets#restrict-updates),
  and `on: push: tags:` fires for an update as readily as for a creation — so
  restricting only creation holds until the first release tag exists, and then
  anyone who can move it runs their own code in `production-plan` again. The
  same question is worth asking of a GitLab project: a protected tag pattern
  decides who may create one, and who may move it is the other half.

The drift watch is the fourth setting, and the easiest one to get wrong: a
scheduled run references no deployment environment, so it receives nothing from
either of them. It has an environment of its own (`monitoring`) holding a
read-only account — see "Drift watch".

`production-plan` has no required reviewers on purpose. Putting them there would
ask a human to approve before the plan exists — before there is a checksum to
approve — which is the thing this pipeline exists to avoid.

The dispatch inputs are part of that boundary too. `apply` pins the artifact by
its checksum and the live baseline; it does not care which commit produced the
plan, so two `prod-v*` tags planned against the same baseline are both
applyable and an approval meant for one could run the other's plan. Both jobs
therefore refuse a `plan_run` whose `head_sha` is not the ref being dispatched,
before anything is downloaded — in `prepare` so the run fails early, and again
in `apply`, because `needs:` orders jobs and carries no check of its own.

**Whoever dispatches is not necessarily whoever approves.** GitHub lets anyone
with write access start a `workflow_dispatch` run
([docs](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow#running-a-workflow)),
and the environment's reviewers are then asked to approve a job whose inputs —
the plan, the checksum, the risk classes — somebody else chose. There is no
setting that narrows dispatch to the reviewer set, so the reviewer *is* the
control, and the pipeline's job is to make their approval an informed one
rather than a click.

That is what `prepare` is for. It holds no credential and touches no database;
it verifies the binding, downloads the artifact, and writes the deployment into
the run summary the approver is already looking at: the tag, the checksum that
was supplied, the risk classes that were named, and `pbps explain` on the
artifact itself — which prints the plan's *own* checksum, its changes and its
risks. A supplied checksum that does not match the plan is then visible before
anyone approves, rather than being discovered by `apply` afterwards.

**The summary is written as an indented code block, not a fence**, and that is
not formatting. `explain` prints text from the declarations — measured, a
column's deprecation reason comes out verbatim, newlines included:

```
  dbo.t
    ~ legacy marked deprecated: moved to email
~~~
## Nothing to see here
This deployment is routine.
~~~
```

A fence around that is closable from inside it, and what follows renders as
Markdown in the one place a reviewer is told to trust. Four spaces on every
line cannot be closed by its own content. The three dispatch inputs go through
the same indent for the same reason: they are text somebody typed.

Approving without reading that summary is the residual risk, and no YAML
closes it. An organisation that cannot accept it should move the apply into a
repository whose write access *is* the releaser set.

There is deliberately **no recording step after `apply`**. `apply` reads the
database back and records that state itself, with the checksum of the plan it
applied. Measured on a real deployment: `apply` writes entry #2 (`apply`,
checksum `56297bd9dc3e`), and a `pbps snapshot --env prod` after it writes
entry #3 — kind `apply`, **checksum `(none)`**. Every deployment would be in
the ledger twice, the newest entry would be the one no plan produced, and a
transient failure in that step would fail a job whose deployment had already
committed. `snapshot` is for adopting a state nobody planned, not for
confirming one that was.

Two things about the `apply` job are load-bearing:

- **`--checksum` is required**, not optional. It pins the artifact to the one a
  human read: `pbps explain --plan plan.json` prints the SHA-256 and the exact
  `pbps apply --env … --plan … --checksum …` line to run. **A checksum computed
  by the same job that produced the plan approves nothing** — it would confirm
  only that a file equals itself — so the value has to come from the approver.

  GitHub's `environment:` gate approves *the run*, not the artifact, and gives
  the reviewer nowhere to type a value. So the apply is **dispatched by the
  approver**, who pastes what `explain` printed: the value travels with the
  approval rather than beside it, and the environment's required reviewers
  still stand behind it.

  A repository configuration variable — `--checksum "${{ vars.APPROVED_PLAN_SHA256 }}"`,
  recorded by the reviewer while the job waits — is **not** offered here, and
  the reason is not taste. A run reads repository-level variables when it is
  queued, not when a waiting job resumes
  ([GitHub docs](https://docs.github.com/en/actions/reference/workflows-and-actions/variables#configuration-variable-precedence)),
  so the first deployment would apply with an empty checksum and every later
  one with the value from the deployment before it. A gate that fails that way
  is worse than no gate: it looks like one.

  GitLab needs none of this *for the checksum*, since a manual job takes its
  variables at start time — but it needs its own answer to "who may deploy",
  which is a protected environment; see the GitLab section.
- **`--allow` names the risk classes that were approved.** It is the gate, and
  the list belongs in the pipeline only if this environment genuinely accepts
  those classes every time; otherwise let the approver supply it too.

## GitLab CI

`.gitlab-ci.yml`:

```yaml
stages: [check, plan, apply]

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
  # `when: manual` decides *when*, never *who*. The environment does that, and
  # only if `production` is configured as a protected environment with its
  # deployers named — see below.
  environment: production
  rules:
    - if: $CI_COMMIT_TAG =~ /^prod-v/
      when: manual                  # the approval gate, not a decision point
  script:
    - pbps apply --env prod --plan plan.json
        --checksum "$APPROVED_PLAN_SHA256"
        --allow rename,narrowing
```

`PBPS_PROD_URL` is a masked, protected CI/CD variable; `APPROVED_PLAN_SHA256`
is supplied by the approver when they run the manual job.

**`when: manual` is not an authorization.** It says the job waits for a person;
it does not say which person. The pipeline is already running on a protected
tag, so the job holds the protected credential, and the checksum is printed in
the plan job's own output — so anyone allowed to run manual jobs in that
pipeline could start the deployment. `environment: production` on the job is
what makes that answerable, and only once `production` is configured as a
**protected environment** naming its allowed deployers (and approval rules, if
you want a second pair of eyes)
([GitLab docs](https://docs.gitlab.com/ci/jobs/job_control/#protect-manual-jobs)).
It is the same control the GitHub half gets from `environment:` with required
reviewers; neither platform gives it for free.

**Protect the `prod-v*` tags as well, in the same setting-up.** A protected
variable is given only to jobs running on a protected branch or a protected tag
([GitLab docs](https://docs.gitlab.com/ci/variables/#protect-a-cicd-variable)),
and the `rules:` regex above selects tag pipelines without protecting anything.
Protect the variable and not the tag pattern and the two deployment jobs run
with `PBPS_PROD_URL` unset — `pbps doctor --env prod` then fails on a
`url_env:` naming a variable that is not there, which reads as a broken
pipeline rather than as the missing permission it is. The protected tag pattern
is also what decides who may start a deployment at all, which is the same
boundary the GitHub half draws with a ruleset.

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
    # Its own environment, holding its own copy of the credential: the
    # deployment environments release nothing to a scheduled run, and this job
    # only ever reads. See "Who can reach the credential".
    environment: monitoring
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

A scheduled run references no deployment environment, so it is released
nothing by `production-plan` or `production` — a credential kept only there
would leave `PBPS_PROD_URL` empty and every drift check failing as
unconfigured rather than reporting drift. Hence `monitoring`, and it is the
right place for a **read-only account**: `verify` reads the catalog and the
ledger and writes nothing, so the watch that runs every hour unattended need
not hold a credential that could deploy.

`verify` exits `2` on drift, which fails the job. The `on_drift` hook has
already delivered by then — a webhook, a chat message, a ticket; its command
decides (SPEC §13.5). The scheduler is your CI and the delivery is your webhook.

`pbps status --format json` is the same idea across every configured
environment at once: last apply, git sha, drift state, last verified. It is a
report and not a gate — it exits `0` whatever it finds (see the contract
above), so a watch built on it reads the JSON.

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
