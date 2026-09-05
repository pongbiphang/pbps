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
The ERD is the page `pbps docs --format html` already produces. It is a
document with project data in it, so the shell fetches it with the token like
any other answer (decision 3) and renders it in a sandboxed `srcdoc` iframe,
with scripts disallowed — the page carries none, and `pbps-docs`'s test
forbids one — rather than navigating to it, which could carry no token.

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
names that stylesheet by its hash — `style-src 'self' 'sha256-…'` — rather
than opening `'unsafe-inline'` for every style; scripts stay `'self'`. A
`srcdoc` iframe inherits the policy of the page that holds it, so the hash is
in the shell's policy: the stylesheet is a constant of `pbps-docs`, `pbps-cli`
computes its hash and hands it to the UI at launch beside the executable's
path, and a test pins that hash to what `docs --format html` emits.

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
the address the server bound and an `Origin` that matches it. `Origin` is
required on every request that is not a `GET` or `HEAD`, and on those two it
must match when present but may be absent: browsers omit it on a same-origin
`GET`, and a cross-site `GET` cannot carry the token header without a
preflight that does carry `Origin`, so an absent `Origin` on a read has
nothing to hide, while an absent one on a write is a refusal. A request
failing any of the four is refused before it is routed.

The one exception is the requests that have to come first. A navigation to
the printed URL cannot carry a header the page has not yet been served to set,
and the script that would set it is itself a request the browser makes before
any script has run — decision 2's policy keeps scripts to `'self'`, so the
script is a file, not an inline block. So the shell — the embedded page, its
script and its stylesheet, all immutable, holding no project data and nothing
a stranger could not read out of the binary — is served to any loopback peer
with a matching `Host`. The script reads the token from the fragment, holds it
in memory, and sends it in the header on every request that follows; every
route that answers a question about the project requires it. The fragment is
the place for
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
environment's *name* and what the envelope already says about it —
`status`'s `detail` and `doctor`'s findings, each written never to hold a
connection string — and no label of its own: the redacted target label the
CLI prints to a terminal is not in the envelope, and a UI that cannot read
`pbps.yml` has nothing to redact. There is no form in which a connection
string is typed, no field in which one is stored, and no request in which one
travels.

The obvious design has a connection form, because every database UI has one.
It is refused by SPEC §8.1's rule that a connection string is never written
down, which is the rule `url_env:` exists to enforce: a string typed into a
browser is written into the browser's memory, its autofill, and the server's
request log. Since the UI cannot connect — only the CLI can — there is nothing
the form would be for.

### 5. Git through the `git` command, not a library — and the commit through plumbing, which runs no hook

Composing intent (step 4 of #64) ends as the same file edit the CLI's intent
commands make, followed by a commit of *those paths and nothing else* and a
push of *that commit and nothing else*, both run as `git` subprocesses in the
checkout the UI was started in, with the user's own configuration. The page
asks for the message, prefilled from the intent the way the CLI's own error
output spells it (`rename dbo.customer.customer_name full_name`), and shows
the diff before the commit, the branch after the push, and a link to the
merge request where the hosting's URL shape is known.

The first design was the porcelain `git commit --only -m <message> --
<paths>`, with the commit read back afterwards and pushed only if it matched
the preview. It is refused, and the record of why is worth keeping, because
every step of it was **measured** on git 2.43 before it was given up. A
`pre-commit` hook runs with the index in its hands: one that ran `git add b`
widened `--only -- a` to a commit of `a` and `b`; one that rewrote `a`
committed the rewritten content; one that ran `chmod +x` left the blob id
equal and turned the tree entry from `100644` to `100755`. A `commit-msg`
hook appending a line changed the message `-m requested` had asked for. And a
`post-commit` hook that pushes runs before `git commit` returns: with the
staging hook beside it, the remote held `a` and `b` before the UI could run
its first read-back. Five checks were written for the first four of those,
and the fifth showed the shape of the mistake: once a hook can publish, a
check after the fact is a check too late. The rule this repository already
holds applies — a failure that can be made unrepresentable is not to be
checked for — and `git` has the tools to make it so.

The commit is built with plumbing, under the index lock `git` itself uses so
that nothing else can touch the checkout while it is built, and **every
`git` the UI runs takes `-c core.hooksPath=<an empty directory>`**. Plumbing
runs fewer hooks than porcelain, not none: `update-ref` runs
`reference-transaction` and `update-index` runs `post-index-change`
(**measured**: one of each fired, in `prepared` and `committed` phases for
the first), and a `committed`-phase hook can push before the UI does. Rather
than know which command runs which hook, the UI points every `git` it runs
at a directory with no hooks in it (**measured**: the same two commands
under that setting fired neither, and the control without it fired both).
The steps:

0. The UI takes the index lock the way `git` does: it asks where the index
   is — `git rev-parse --git-path index`, because in a linked worktree
   `.git` is a file and the index lives under the main repository's
   `worktrees/<name>/` (**measured**) — creates `<index>.lock` beside it
   exclusively, as a copy of the index, and refuses to compose if the file
   already exists — another `git` is mid-operation. Every `git` that would
   change the index or switch the checkout fails on that file until it is
   gone (**measured**: `git add` and `git switch` both exited 128 with
   `Unable to create '.git/index.lock': File exists`). The
   lock is held through step 6; on any failure it is deleted without being
   installed, and the index is as it was. It takes `HEAD`'s lock the same
   way — an empty `<git-path HEAD>.lock`, created exclusively — because the
   index lock does not cover `HEAD`: **measured**, with `index.lock` held,
   `git symbolic-ref HEAD refs/heads/<sibling>` went through, and with
   `HEAD.lock` held it failed with `Unable to create 'HEAD.lock'`.
1. It records the branch `HEAD` is symbolic to and its tip (`git
   symbolic-ref HEAD`, `git rev-parse refs/heads/<branch>`), and the
   remote's tip for that branch, under the rules below. It requires each path
   it is about to edit to be a regular file or absent — never a symlink:
   `git hash-object` follows a link and hashes the target's content
   (**measured**: the link `a` hashed as `a`'s content, where `git add`
   stores the link text at mode `120000`), so a linked declaration file
   would commit its contents as a link's destination. The tip's entry for
   the path must be a regular file too (`100644` or `100755`) or absent:
   step 3 reuses the tip's mode, and a path whose link was replaced by a
   file since the tip would otherwise pass the filesystem check and be
   committed as a `120000` entry holding the document. And it requires the
   index entry for each path to be the tip's entry (`git ls-files -s -z --
   <path>` against `git ls-tree -z <tip> -- <path>`), refusing otherwise and
   showing the staged change: staged content of the user's own is work not
   yet committed, and step 6 would replace it (**measured**: a version
   staged and a different one in the working tree left the staged blob
   unreachable from the index after the refresh, with `git status` clean).
   Under the lock, what step 1 saw is what step 6 finds.
2. It writes the edited files, and stores each as a blob:
   `git hash-object -w -- <path>`.
3. In an index of its own (`GIT_INDEX_FILE`), it reads the recorded tip's
   tree, sets the entry for each edited path to that blob at the mode the
   path had at the tip — `100644` for a new one — and writes the tree:
   `git read-tree <tip>`, `git update-index --add --cacheinfo
   <mode>,<blob>,<path>`, `git write-tree`. `--add` because a new path is not
   in the tree that was read (**measured**: without it, `cannot add to the
   index - missing --add option?`, exit 128). The user's own index is not
   read here.
4. It makes the commit from that tree, on that parent, with that message:
   `git commit-tree <tree> -p <tip> -m <message>`. The commit is what was
   previewed *by construction* — those paths, those blobs, that parent, that
   message — and there is nothing to read back. It is signed exactly when the
   shell's would be, and that takes one step porcelain does by itself:
   `commit-tree` does *not* read `commit.gpgSign` (**measured**: with it set
   and a key that cannot sign, `git commit` failed, `commit-tree` succeeded
   unsigned, and `commit-tree -S` failed the way `git commit` had), so the
   UI reads `git config --type=bool commit.gpgSign` and passes `-S` when it
   is true, leaving the key, the format and the program to the user's
   configuration. The page shows `git log -1 --format=%G? <oid>`. ADR-0006's
   "signed commit" is the organization's signing policy applied by the user's
   configuration, not a guarantee this UI adds — a forced signature fails on
   a machine without a key, and so does the shell's.
5. It moves the branch to the commit only if the branch is still where it
   was: `git update-ref refs/heads/<branch> <oid> <tip>`, a compare-and-swap
   that refuses if anything moved the branch in between (ref locks are not
   the index lock; **measured**, the update went through with the index lock
   held), and the one step that changes the checkout. `update-ref` takes
   `HEAD`'s lock itself when `HEAD` names the branch it moves (**measured**:
   with `HEAD.lock` held it failed with `cannot lock ref 'HEAD'`), so the
   UI releases `HEAD.lock` for this one command and takes it back right
   after, then checks that `HEAD` is still symbolic to the recorded branch.
   If it is not — a `symbolic-ref` slipped into that gap — the commit is on
   the branch, which is correct, but the checkout is no longer on that
   branch, so step 6 does not happen: the index lock is discarded, the index
   is as it was, and the page says which branch holds the commit. **Measured**
   both ways: undisturbed, the check passed and the index was installed
   clean; with a `symbolic-ref` to a sibling in the gap, the check failed,
   the branch held the commit, the sibling was untouched, and the index was
   left alone.
6. It writes the same entries into the locked copy of the index —
   `GIT_INDEX_FILE=<index>.lock git update-index --add --cacheinfo
   <mode>,<blob>,<path>` — and installs it by renaming `<index>.lock` to
   `<index>`, which is exactly the commit step of `git`'s own lock. Now
   `git status` is clean for what the UI did and untouched for everything
   else, and the entries replaced were the tip's, as step 1 established and
   the lock preserved.

**Measured** on git 2.43, with staging, message-editing, pushing and pre-push
hooks all installed: `commit-tree` made a commit holding `a` alone with the
message intact, the remote did not move, and no hook ran; `update-ref` with a
stale expected value refused with `is at <x> but expected <tip>` and left the
branch where it was, and with the recorded one moved it, leaving `HEAD`
symbolic to it; the write through the held lock and the rename left `git
status` clean. The races the porcelain route needed a parent check and a
branch check for — another process moving or switching the branch between the
record and the commit — are closed by the lock and the compare-and-swap.

The preview is `git diff --no-ext-diff --no-textconv <tip> <tree>`: the
recorded tip against the tree the UI built, exact by construction, and
rendered without presentation filters because a `diff.external` or
`textconv` driver can show two blobs as one text (**measured**: a driver
printing a constant hid a rewritten file). Every `git` the UI runs takes
`--literal-pathspecs`, every path comes after `--`, and every command that
prints paths takes `-z` and is parsed as bytes, each for a reason that was
measured: `a[12].json` is a pattern to `git` and selected three files
without `--literal-pathspecs`; a file named `-A` made `git add -N -A` mark
every untracked file; and `schéma.json` came back C-quoted from `ls-tree`
without `-z`.

The push is bounded to that one commit, and to it by name. Before composing,
the UI reads the remote's tip for the branch (`git ls-remote <remote>
refs/heads/<branch>`) and refuses to compose unless the local tip equals it,
showing the unpushed commits and the commands instead: a refspec bounds the
destination ref, not the range, and **measured**, a branch one unrelated
commit ahead had that commit published under the intent commit by
`HEAD:refs/heads/<branch>`. A branch the remote does not have yet — the first
push of a feature branch, the common case — is not an ahead branch: the
destination is absent rather than behind, and the check is instead that the
tip is already on the remote, an ancestor of one of the heads `ls-remote`
lists. That head is fetched first into a ref of the UI's own (`git fetch
--no-tags --no-write-fetch-head <remote>
+refs/heads/<witness>:refs/pbps-ui/witness` — the second flag so the user's
`FETCH_HEAD` still names what the user last fetched; **measured**, it did),
because
`ls-remote` names an object the local repository need not have
(**measured**: `merge-base --is-ancestor` against the bare id exited 128,
against the fetched ref it answered). The push then names the commit by id,
leases the destination on the tip it recorded — or on its absence, with an
empty expected value — and, for a first push, names the witness at its
fetched value in the same push under `--atomic`, since a lease on a ref the
push does not name is ignored (**measured**: with the witness moved on the
remote, the lease alone let the branch be created; the atomic push naming the
witness was refused as stale and created nothing). `--atomic` is asked for
only on that two-ref push: a server without atomic push support refuses the
option outright, and the one-ref push of an existing branch has nothing to
be atomic about. It takes `--no-verify`
as well as the empty `core.hooksPath` every `git` here gets, because a
`pre-push` hook is a hook (**measured**: it ran without the flag and not
with it), and `--no-follow-tags`; never a bare `git push`, which under
`push.default=matching` advanced two branches at once when measured:

```
git push --no-verify --no-follow-tags [--atomic] \
    --force-with-lease=refs/heads/<branch>:<tip-or-empty> \
    [--force-with-lease=refs/heads/<witness>:<fetched> <fetched>:refs/heads/<witness>] \
    <remote> <oid>:refs/heads/<branch>
```

What this gives up is stated plainly: **the user's hooks do not run for a
commit or a push the UI makes**, and the page says so beside the commit. A
hook is the shell's policy at commit time; this tool keeps policy in files
and in CI (SPEC §14.3, and the `policies:` block of ADR-0008), the intent
commit is one line in the ids file and the declaration it names, and a
`plan --check` in the pipeline reads it the same whether a hook saw it or
not. An organization that needs a rule enforced on every commit enforces it
where a rebase cannot skip it either — on the server or in CI — and one that
relied on a client hook alone had no enforcement to lose.

A library (`gix`, `libgit2`) is the obvious design and would remove a runtime
dependency on a `git` binary. It is refused because ADR-0006's audit story is
"what an auditor is shown is a signed commit", and the signing key, the
identity, the credential helper and the remote configuration are all things
the user's `git` already honours and a library has to re-read and
re-implement — each one a way for a commit made from the UI to differ from a
commit made from the shell, which is the difference an auditor is entitled to
ask about. A `git` that prompts — for a passphrase, for a credential — gets
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

Step 2 has a list to work from. The read path needs one thing: the ledger
timeline. `status` carries only the newest entry of each environment, the
`state` command exposes only `prune`, and the history read is
`pbps-mssql`'s, which decision 1 keeps the UI from calling — so step 2 adds a
`state list --format json` envelope, the first half of the `state show / diff
/ export` row SPEC §14.1 already holds at P1. The write path needs more:
`plan --db` is deliberately outside the envelope set (SPEC §9.8) and the
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
