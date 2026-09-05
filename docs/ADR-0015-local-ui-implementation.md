# ADR-0015: How the local UI is built — a subprocess, a page and a token

- Status: proposed (design; implementation is Phase 6, steps 2–6 of issue #64)
- Date: 2026-09-06
- Related: [ADR-0006](ADR-0006-optional-ui.md) (what the UI may and may not
  do — nothing here re-argues it); docs/SPEC.md §6.3, §6.4, §8.1, §9.4, §9.7,
  §9.8, §14.2, §14.3; [ARCHITECTURE.md](ARCHITECTURE.md);
  `crates/pbps-cli/src/output.rs`; `crates/pbps-cli/src/integration.rs`

## Background

ADR-0006 admits a UI on one constraint — every action ends as a git commit or
an ordinary CLI invocation, and the UI stores nothing authoritative — and
places it in Phase 6, after the typed JSON of Phase 3.1 exists. That JSON now
exists: every read-only command speaks the one envelope of SPEC §9.8, `explain`
reads a saved plan without credentials, and `docs` renders the ERD. What
ADR-0006 does not say is how the thing is built, and four of those choices are
the kind that are cheap to make now and expensive to reverse once a page
depends on them: what process the UI runs in, what the browser is served, how
the browser is kept from being driven by someone else, and what touches git.

This ADR takes those four, plus where the credentials flow and which crate the
code lives in. Everything here is reasoned rather than measured; the "Limits"
section says what the first implementation step has to measure before it is
believed.

## Decision

### 1. The UI runs the `pbps` binary as a subprocess; it links none of the crates

`pbps ui` starts a server, and every question the page asks is answered by
spawning the same executable (`std::env::current_exe()`) with the same
arguments a user would type — `--no-input` always, and `--format json` for
every command that speaks the envelope of SPEC §9.8 — and relaying what comes
back. `status`, `verify`, `doctor`, `validate` and `explain --plan` are
relayed as the envelope; `docs` speaks none, and is the one command relayed as
what it produces, the HTML page of `docs --format html` (decision 2). That is
the whole of the read path, and none of it is changed for the UI.

The obvious design links the crates: call `pbps_diff` and render the
`ChangeSet` directly, skip the process boundary, skip the JSON. It is refused
because ADR-0006 rests on SPEC §14.2's fourth acceptance criterion — *a
frontend never reimplements validation* — and says it should hold
"structurally rather than by discipline". A UI crate that can see `pbps_load`
can, in some later commit, load a file itself to show a friendlier error, and
from that day the browser and the CLI disagree about what is valid. A UI crate
whose only inputs are JSON envelopes cannot; the compiler holds the line, not a
review comment. The same boundary makes the UI an ordinary consumer of the
envelope — the one SPEC §9.8 names beside the CI annotator and a team's own
dashboard — so what it needs and what it breaks on is exactly what any
third-party consumer needs and breaks on, and step 2 of #64 can test it as one.

The cost is a process per question and the JSON round trip. For one person on
one machine, looking at one project, that is not a cost anyone can see.

Two consequences follow. The `pbps` the UI runs is the `pbps` that is running
the UI, never one found on `PATH`: a stale binary on `PATH` would put a
different `schema_version` on the wire than the page was built for. And the
child inherits the environment, so the connection strings of SPEC §8.1's
`url_env:` reach the command that connects without the UI ever reading them
(decision 4).

### 2. Plain HTML and JavaScript, embedded in the binary; no build step

The page is static files compiled into the binary and served from memory.
There is no Node toolchain, no bundler, no package manifest and no framework.
The ERD is the page `pbps docs --format html` already produces, served as it
is.

The obvious design is a component framework with a build step, because that
is how a browser application is written in 2026. It is refused for what it
does to the rest of the project rather than for what it does to the page. CI
is pure `cargo` on three platforms plus a container; a JavaScript build adds a
second toolchain to every job, a second lockfile to `cargo deny`'s blind spot,
and a second supply chain to the binary that a tool claiming "the reviewed
plan is exactly what runs" is asked to trust. The air-gapped path of SPEC §9.7
also has to keep working: the page fetches nothing from a network it may not
have, so every byte it needs ships in the binary, and a `Content-Security-Policy`
whose `default-src` is `'self'` says so to the browser. One resource is inline
by design and the policy has to say so too: the stylesheet `docs` embeds in
its page, so that the page renders on a machine without internet
(`pbps-docs`'s `html.rs`, which tests for the inline `<style>`). The policy
names that stylesheet by its hash — `style-src 'self' 'sha256-…'`, computed
from the page when it is served — rather than opening `'unsafe-inline'` for
every style; scripts stay `'self'`.

The cost is hand-written DOM code and no component reuse. The page renders a
handful of typed shapes — findings, a drift report, a plan summary, a ledger
timeline, environment rows — and that is a size at which a framework's
overhead exceeds its help. The line to hold when it is asked for again: a
dependency that would need a build step is refused; a single-file library
vendored into the tree under a licence `deny.toml` allows is not, and is
reviewed like any other dependency.

### 3. Loopback is not enough: a per-launch token, an `Origin` check, and a loopback peer

The server binds `127.0.0.1` only, on an ephemeral port, and prints one URL at
launch that carries a random token in its fragment:
`http://127.0.0.1:<port>/#<token>`. Every request but one — reads included —
must present that token in a request header, not in a cookie and not in the
query string, and must arrive from a loopback peer with a `Host` header naming
the address the server bound and an `Origin` (or none, for a same-origin
navigation) that matches it. A request failing any of the four is refused
before it is routed.

The one exception is the request that has to come first. A navigation to the
printed URL cannot carry a header the page has not yet been served to set, so
the shell — the embedded page itself, immutable, holding no project data and
nothing a stranger could not read out of the binary — is served to any
loopback peer with a matching `Host`. Its script reads the token from the
fragment, holds it in memory, and sends it in the header on every request that
follows; every route but the shell requires it. The fragment is the place for
it because a browser sends the fragment neither to the server nor in a
`Referer`; the one place it lands is the browser's history, where it names a
server that is gone when `pbps ui` exits.

Loopback alone is the obvious design, and it would ship a hole through every
guardrail in SPEC §14.3. Any page open in the same browser can `POST` to
`http://127.0.0.1:<port>/…`; the browser attaches nothing that says which site
sent it unless the server asks. Once step 5 of #64 exists, that request is
`apply --plan`. DNS rebinding closes the other route — a hostname that
resolves to `127.0.0.1` after the page is loaded — which is what the `Host`
check is for. The token is a header rather than a cookie because a cookie is
sent by the browser on the attacker's behalf, and rather than a query string
because a query string reaches the server's log and a `Referer`.

None of that is invented here; it is the standard treatment of a local
development server, and the reason it is a decision at all is *when*. Reads
that render a schema are not dangerous, and a UI that ships reads only (step 3)
would be tempted to add the checks "when writes arrive". They are decided now
so that step 3's server already refuses what step 5's needs refused, and no
later step reopens the question.

### 4. Credentials flow through the environment, and nowhere else

The `pbps` process the UI spawns reads the connection string from the
environment variable `url_env:` names, exactly as it does from a shell. The UI
process does not read that variable, holds no copy, and sends the browser the
environment's *name* and the redacted label `pbps-cli`'s `db::redact` already
produces for every diagnostic — never the string. There is no form in which a
connection string is typed, no field in which one is stored, and no request in
which one travels.

The obvious design has a connection form, because every database UI has one.
It is refused by SPEC §8.1's rule that a connection string is never written
down, which is the rule `url_env:` exists to enforce: a string typed into a
browser is written into the browser's memory, its autofill, and the server's
request log. Since the UI cannot connect — only the CLI can — there is nothing
the form would be for.

### 5. Git through the `git` command, not a library

Composing intent (step 4 of #64) ends as the same file edit the CLI's intent
commands make, followed by a commit of *those paths and nothing else* and a
`git push`, run as subprocesses in the checkout the UI was started in, with
the user's own configuration. The commit is `git commit --only -- <paths>`,
after `git add -N` for a path that is new, which takes the named paths from
the working tree and leaves whatever the index already held staged and
uncommitted; the preview the page shows first is `git diff HEAD -- <paths>`,
which is exactly that content. **Measured** on git 2.43: with an unrelated
file staged, `git commit --only -- ids.json` committed the ids file alone and
left the other file staged, and `git diff HEAD -- ids.json` showed the same
hunk — where a plain `git add` and `git commit` would have swept the staged
file into the intent commit and pushed it, and a plain `git diff` would not
have shown it in the preview. After the push the page shows the branch and
links the merge request where the hosting's URL shape is known.

A library (`gix`, `libgit2`) is the obvious design and would remove a runtime
dependency on a `git` binary. It is refused because ADR-0006's audit story is
"what an auditor is shown is a signed commit", and the signing key, the
identity, the credential helper, the hooks and the `push.default` are all
configuration the user's `git` already honours and a library has to re-read
and re-implement — each one a way for a commit made from the UI to differ from
a commit made from the shell, which is the difference an auditor is entitled
to ask about. A `git` that prompts — for a passphrase, for a credential — gets
no terminal from the UI and fails; the page then shows the command to run by
hand, which is SPEC §6.4's rule for the CLI applied unchanged.

### 6. A `pbps-ui` crate that sees no model, reached through `pbps ui`

The server, its request checks and the embedded page live in a new crate,
`pbps-ui`, that depends on `serde`, `serde_json` and one small HTTP crate — and
on nothing else in the workspace. `pbps-cli` depends on it and exposes it as
the `pbps ui` subcommand, passing the path of the running executable and the
project directory. The envelope and the payloads the page renders are
deserialized into `pbps-ui`'s own types, with `deny_unknown_fields`, so a field
the CLI adds and the page does not know fails a test rather than being dropped
on the floor. The direction of the dependency is decision 1 made enforceable:
`pbps-ui` cannot name a model type because it cannot see one.

A separate `pbps-ui` binary would keep the HTTP crate and the page out of the
core binary. It is refused because ADR-0006 names the command `pbps ui`, and
because a second binary is a second thing to install, to put on `PATH`, and to
keep at the same version as the first — the failure decision 1 closes by
running `current_exe()`. The size the HTTP crate and the page add to the binary
is a number, and it is measured before this decision is kept (see "Limits").

The HTTP crate is constrained rather than named: synchronous, HTTP/1.1, no
TLS, no framework, and no second async runtime — the CLI `block_on`s a `tokio`
it holds only for the driver, and a server that wanted that runtime would pull
the UI into the boundary ARCHITECTURE.md draws around `pbps-db`. The candidate
is chosen at step 3 of #64 with `cargo deny check` and the size measurement.

## Placement

Step 1 of #64 is this document. Steps 2 to 6 wait for #43, the last Phase 5
model change that moves a format the page would render; ADR-0006's reason for
Phase 6 being late — a UI over a moving format is a second implementation of
the format — holds for a moving envelope payload as well. The PostgreSQL
dialect itself is not a prerequisite: the page renders envelopes, and an
envelope does not say which engine produced it.

Step 2 has a list to work from. The read path needs nothing new. The write path
does: `plan --db` is deliberately outside the envelope set (SPEC §9.8) and the
page reads its result through `explain --plan`, so it needs only the exit code
and the written file; `apply` speaks no envelope, and the page needs its
outcome and the ledger entry it wrote — the fields `on_apply_attempt`'s payload
(SPEC §9.4) already carries — which step 2 decides between an `apply --format
json` and a re-read of `status` after the fact.

## Ruled out

Each of these is a request the UI will attract, and each is a step back toward
the server ADR-0006 declines to be or the hole decision 3 closes:

- **Binding on anything but loopback**, including a `--host` flag "for the
  team". Refused: it is the hosted deployment ADR-0006 reserves for its own
  ADR, and every check in decision 3 assumes a loopback peer.
- **A "remember this connection" field**, a stored profile, a keychain
  integration. Refused by decision 4; the environment already remembers it.
- **Skipping the token for reads.** Refused by decision 3's reasoning about
  *when*: the check that is not there for reads is the check that is missing
  when writes arrive.
- **A framework "just for the plan view".** Refused by decision 2's line: a
  build step is the cost, not the framework.
- **Calling `pbps_diff` directly "because the JSON is slow".** Refused by
  decision 1; the number that would justify it has to be measured on a project
  that exists, and the remedy is a faster command, not a second caller.

## Limits

What this ADR reasons about and has not measured, in the order the steps of
#64 will meet it:

- **The size the HTTP crate and the page add to `pbps`.** Decision 6 keeps
  the UI in the core binary on the assumption that the addition is small. Step
  3 measures it against the binary without the crate, and a number that
  surprises reopens decision 6, not decision 1.
- **What the page needs from `apply`.** The Placement section names two
  answers; step 2 picks one after listing what the page actually renders after
  an apply.
- **The `git` binary's presence on the machines the UI targets.** Decision 5
  assumes the DBA who will not run five commands still has `git` installed,
  because the checkout they are looking at came from somewhere. A machine
  without one gets the commands to run by hand and no worse.
- **DNS rebinding through browsers that pass a numeric `Host` unchanged.**
  Decision 3's `Host` check is the standard answer; step 3's tests send the
  cross-origin `POST`, the rebinding `Host`, the foreign peer and a request to
  any route but the shell without the header, and watch each refused, which is
  the test ADR-0006's "structurally rather than by discipline" asks for.
