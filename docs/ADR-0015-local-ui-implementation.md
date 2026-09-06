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

The page sends the intent, never the files: the kind of change and its
arguments — `rename dbo.customer.customer_name full_name`, `drop
dbo.customer.national_id --reason <text>`, and the table and role forms —
exactly as SPEC §6.4 prints them for the shell. An intent command does not
edit a declaration; the user has already done that, in an editor, in the
working tree, and the command resolves the edited declarations against the
ids file and rewrites the ids file alone (`cmd_intent` loads the
declarations, pushes the intent, calls `pbps_diff::resolve` and writes the
result; **measured**: against a declaration that still held the old name,
`pbps rename` exited 2 with "this intent matches nothing"). So the working
tree is where the intent is, and the compose is the CLI user's own last
step made in the CLI user's own order, in this order:

- **The snapshot.** The UI writes the recorded tip out the way step 3
  below writes a tree (`git ls-tree -r -z <tip> -- <project>` and `git
  cat-file --batch`, the project's subtree only — a monorepo's other
  gigabytes are not the project's inputs, and `ls-tree` scoped by path
  still names each entry from the worktree root (**measured**) — regular-file
  entries only, under `<git-dir>/pbps-ui/intent/<random>/`, under the path
  rules step 3 states). It holds the tip's `pbps.yml`, so the compose is
  refused first if `pbps.yml` is modified in the working tree (`git status
  --porcelain -z -- <project_file>` not empty): the working tree's
  configuration would answer the next question differently, and the user
  commits the configuration first, as they would before `pbps rename`.
- **Where the inputs are.** The UI asks the CLI, in the snapshot: `pbps
  doctor --format json --no-input --project <snapshot project>`, whose
  envelope carries `declarations` and `identity_file` as the CLI resolved
  them from the project it was given (**measured**: with an absolute
  `schema_dir` in `pbps.yml`, both came back absolute and outside the
  project). It refuses the compose unless both lie under the snapshot's
  project directory, since `Project::schema_dir` and `Project::ids_file`
  join the configured path to the root and a configured absolute path
  discards the root: an intent command pointed at the snapshot would
  otherwise read the live declarations and write the live ids file before
  any of the protocol below had begun, and `validate` in step 3 would check
  live bytes instead of the tree; a project whose inputs lie outside its
  directory is one this UI does not compose for, and the Limits section
  says so. This `doctor` is spawned with an environment holding nothing a
  connection string could be read from — `PATH` and `HOME` alone — because
  `doctor` otherwise examines every configured environment before it
  answers, and a slow or unreachable server would hold up, or under the
  UI's deadline kill, a compose that needs nothing but `git`
  (**measured**: with the environment's variable set to an unreachable
  server `doctor` tried the connection and reported `Connection refused`;
  with it unset it reported the variable unset, connected to nothing, and
  carried both paths, exit 0). The two answers name places in the
  snapshot; the UI takes each relative to the snapshot's project directory
  and joins it to the checkout's, and those two checkout paths are what
  the rest of this list means by *the declarations* and *the ids file*.
- **The listing.** `git status --porcelain -z --untracked-files=all
  --ignored=matching -- <the declarations> <the ids file>`: the pathspec
  is those two and never the project directory, so that a modified
  `README` or anything else the user has open beside the declarations is
  neither laid over the snapshot nor committed with the intent;
  `--untracked-files=all` because `status.showUntrackedFiles=no` in the
  user's configuration would otherwise hide the new side of a file-based
  rename (**measured**: under it the listing held the deleted old path
  alone, and with the flag both); and `--ignored=matching` because a new
  declaration matched by `.gitignore` or `.git/info/exclude` is listed by
  neither (**measured**: it appeared only with the flag, as `!!`), and
  the compose is refused, naming the file, if any `!!` entry is listed —
  the intent command would see a working tree the commit cannot hold, and
  the shell's `git add` would refuse the file too. The page shows the
  listing with the blob id of each file as read.
- **The overlay.** The UI lays the working tree's version of each listed
  file over the snapshot, from bytes read through the file's handle whose
  hash is the id the page carries, and *removes* from the snapshot each
  listed file the working tree no longer has, a `D` in the listing,
  because a table or a role is dropped or renamed by deleting or renaming
  its declaration file, and a snapshot that still held the old file would
  give the command nothing that disappeared to resolve.
- **The command.** It runs the CLI's own intent command there — `pbps
  rename <from> <to> --no-input --project <that directory>/<the project's
  path>` and its siblings, with no `--format`, which the intent commands
  do not take (**measured**: `rename --format json` exited 2 with
  `unexpected argument '--format'`); decision 1's list of what speaks the
  envelope is exact, and for these commands the page gets the exit status
  and the blockers report of `stderr`.
- **The paths.** What steps 1 to 6 commit are the listed files and every
  regular file the command left different from the snapshot it was given,
  the ids file among them, found by hashing the snapshot against `git
  ls-tree` — the same set a CLI user commits after `pbps rename`. The
  listed files already hold their bytes in the working tree, so step 2
  places nothing for them and step 1's check that each still hashes to
  the page's id is the whole of their handling; a deleted one must still
  be absent (the no-follow lookup through its directory handle fails with
  `ENOENT`) and is removed from the tree and the index rather than set,
  `git update-index --force-remove -- <path>` in both of step 3's indexes,
  the form that removes the entry whatever the working tree holds
  (**measured**: after it the written tree lacked the path, and with the
  commit on the branch and the prepared index installed, `git status` was
  clean); what the command changed is placed as step 2 describes.

The browser never composes a declaration or an ids-file line: the identity
mapping a rename records comes from `pbps_diff::resolve`, which the intent
command runs and the UI cannot, and a UI that chose the uid itself could
write an ids file that is internally consistent and records a different
identity transition than the one asked for — one `validate` accepts, since
it checks the file's consistency and not what the user meant, and one a
later `plan` reads as a drop and a create. SPEC §14.3's rule is kept as the
CLI keeps it: the arguments are the user's decision, typed into the page
instead of the shell, and `--no-input` declines any question the command
would have asked, so a case it would ask about is refused and shown, never
answered by the UI. Reasoned beyond the measurements above: what an intent
command edits is the CLI's, unchanged here; step 4 of #64 measures the
snapshot round trip.

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
One hook lives outside that directory: a `core.fsmonitor` naming a program
is run by `update-index` whatever `core.hooksPath` says (**measured**: the
locked-copy `update-index` ran it), so every `git` also takes
`-c core.fsmonitor=false` (**measured**: then it did not). The steps:

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
   symbolic-ref HEAD`, `git rev-parse refs/heads/<branch>`), refusing a
   branch that is itself a symbolic ref (`git symbolic-ref
   refs/heads/<branch>` answers instead of failing): `update-ref` would
   move the ref it points to while the lock of step 5 holds the alias, so
   the branch the UI locks must be the branch that moves. It records the
   remote's tip for that branch, under the rules below. It requires each path
   it is about to edit to still hold what the page was shown: the page's
   request carries the blob id of the file it read, step 1 hashes the file
   as it is now (bytes read through the handle, `git hash-object
   --no-filters --stdin`, no `-w`) and
   refuses if the two differ — the locks stop `git`, not an editor saving
   the same file, and the browser's copy would otherwise overwrite newer
   work (**measured**: the page's id and the id after an editor's save
   differed). It requires the path to carry no `filter` attribute: `git
   check-attr --all -z -- <path>` lists every attribute set on the path,
   and no `filter` may be among them. The obvious query, `check-attr
   filter`, answering `unspecified`, is not enough, because a driver may
   be *named* `unspecified` (**measured**: under `*.json
   filter=unspecified` with `filter.unspecified.clean` configured,
   `check-attr filter` printed `filter: unspecified` exactly as it does
   for no attribute, `hash-object --stdin --path` ran the clean program
   all the same, and a pass-through program left the filtered and raw ids
   equal; `--all` listed `filter: unspecified` for that path and nothing
   for a path with no attribute). The same listing is taken with
   `--source <tip>` (git 2.40 and later), and the two must be equal:
   every attribute check here reads the working tree's `.gitattributes`,
   while the commit is built on the tip and carries the tip's, so a
   `.gitattributes` edit not yet committed makes them two different
   questions, and a `filter` or `ident` rule removed locally would pass
   the working-tree checks and leave the commit transforming the path in
   every other checkout (**measured**: with `*.json ident` at the tip and
   the working-tree `.gitattributes` emptied, `check-attr --all` listed
   nothing for the path and `--source HEAD` listed `ident: set`). A clean filter
   is a program neither `core.hooksPath` nor `core.fsmonitor` reaches, and
   a blob stored around it leaves `git status` reporting the path modified
   the moment it is committed, since `git` compares through the filter. The
   same holds for the transformations built into `git` — `ident`, and the
   end-of-line conversion `text`, `eol` and `core.autocrlf` ask for — so
   with no filter program left to run, step 1 also requires `git
   hash-object --stdin --path <path>` and `git hash-object --stdin
   --no-filters`, both fed the bytes read through the file's handle, to
   agree: the first hashes what check-in would store for that name, the
   second the bytes, and any difference is a transformation that would
   leave the installed path modified (**measured**: with `*.json ident`
   and `$Id$` in the file, `filter` was `unspecified` and the two ids
   differed). Neither takes the path as a file to open: a name handed to
   `git` is walked again from the root, through whatever a component has
   become since the handle was opened, which is the reopen-by-name step 2
   refuses for every hash it takes. The same pair of
   hashes is taken of the *replacement* bytes (the same two commands), because an attribute
   that leaves the old content alone can still act on the new: a file
   without a marker passed, and the same file with `$Id: forged $` written
   into it did not (**measured**). Both of these attribute checks answer
   by path name — `--path` and `check-attr` read the `.gitattributes`
   files along the name as it is at that moment — so they are taken
   *after* step 2 has placed the file and confirmed the directory's
   identity, in the same instant as that confirmation, and a failure then
   rolls step 2 back like any other; the answer holds exactly as long as
   the identity check does, which is the limit already named for it. It
   requires the path to be a regular file or absent — never a symlink,
   and never *under* one: every component of the path from the worktree
   root is checked with `lstat` and none may be a link, and every file
   operation the UI makes is done through directory handles opened from
   the root so that no component is followed as a link — on Linux one
   `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, on macOS one
   `openat` per component from the previous component's handle, each with
   `O_DIRECTORY | O_NOFOLLOW`, and on Windows the per-component handles
   step 2 describes. `O_NOFOLLOW` guards the last component only,
   and `RESOLVE_BENEATH` guards the worktree's boundary, not the path: a
   link *inside* the worktree is followed under it (**measured** on Linux
   6.6: with `dir/link` a link to a sibling directory, `openat2` of
   `dir/link/sub` with `O_NOFOLLOW` and `RESOLVE_BENEATH` opened the
   sibling's `sub`, and with `RESOLVE_NO_SYMLINKS` added it failed with
   `ELOOP`, while a path with no link opened under both), so the `lstat`
   walk above is what is checked and the open is what enforces it, and
   the open would otherwise write one directory's file while the commit
   named another's path. A tracked directory replaced by a link to a
   directory outside
   the worktree leaves the leaf a regular file with matching index and tip
   entries (**measured**: `ls-files` and `ls-tree` both still said
   `100644` while `hash-object` read the outside file), and step 2 would
   have exchanged and discarded a file the "outside the worktree" rule
   promised never to touch. The leaf itself must not be a link either:
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
   The entry must also be an ordinary one: `git ls-files -v` must show it
   as `H`, and any other tag — `S` for `skip-worktree`, `h` for
   `assume-unchanged`, `s` for both, `M` for unmerged, `R` for removed —
   refuses the path. Step 6's `--cacheinfo` writes a plain entry and would
   clear the flags (**measured**: `S a h b` became `H a H b`, and a path
   carrying both showed as `s`), and a path the user has told `git` to
   leave alone is not one the UI should quietly bring back. Under the
   lock, what step 1 saw is what step 6 finds.
2. It writes each edited file beside its path, gives it the metadata of
   the file it replaces — the exchange swaps the files' metadata with them,
   and `git`'s `100644`/`100755` carries only the executable bit, so a
   temporary made from the tree mode alone would turn a `0600` file into a
   `0644` one and drop its ACLs and extended attributes — copying the
   owner and group first, where the process may set them (`fchown`), then
   the permission bits (`fstat`, `fchmod`) — in that order, because
   `fchown` clears a setuid or setgid bit even when it sets the owner the
   file already has, so bits copied first would be gone — every extended
   attribute
   (`flistxattr`, `fgetxattr`, `fsetxattr`), and the ACL through the
   platform's ACL interface — on Linux the `system.posix_acl_*` attributes
   the calls above already carry, on macOS `acl_get_fd_np` and
   `acl_set_fd_np`, since Darwin keeps an ACL outside the attribute list —
   all through the handles and never by name, then reading the temporary
   back (`fstat`, the attribute and ACL calls again) and refusing the
   compose if what it holds is not what was copied, or where any item
   cannot be copied at all, rather than exchanging a file that has lost
   something; a new
   file gets the mode `0666` less the umask, which is what an editor would
   give it — and puts it in place
   with an atomic exchange — Linux `renameat2(RENAME_EXCHANGE)`, macOS
   `renamex_np(RENAME_SWAP)` — then hashes the file that came *out* and
   reads back the whole of its metadata, and does the same for the file
   that went *in*, through the descriptor the temporary was written and
   read back through, which the exchange leaves bound to the inode now at
   the path (**measured**: after the exchange, `fstat` of that descriptor
   named the inode `stat` of the path named, and it read the new bytes
   back), refusing if its bytes are no longer the held bytes or its
   metadata no longer what was copied — a temporary is a name beside the
   path for the length of a compose, and a process that wrote to it after
   the read-back and before the exchange would otherwise have its bytes
   at the path under a commit made from the UI's: if the blob that came
   out is not the one the
   page was shown, an editor saved between step 1's check and the
   exchange; if the mode is not the tip entry's, the user changed the
   executable bit in the working tree, which the blob does not carry and
   the exchange would have silently reset — a path the tip does not hold
   has no entry to compare with, so the executable bit of the file that
   came out is instead what step 3 records for it; and if the permission bits,
   owner, group, attributes or ACL are not what the temporary copied a
   moment earlier, something changed them in that gap and the copy is
   already stale — the same comparison the copy was made from, taken
   again on the other side of the exchange. In any of these cases the
   two are exchanged back and the compose is
   refused with what differs shown — and so is every path placed before
   it, in reverse order: an exchanged path is exchanged back, which puts
   the replacement under its temporary name, and a path `link()` created
   has its entry *renamed* to a fresh temporary name beside it
   (`renameat` within the directory handle), never unlinked, because the
   entry may no longer be the UI's inode: an editor that saves by writing
   a temporary and renaming it over the path leaves its own inode there,
   with the UI's still under the temporary it was linked from, and an
   `unlinkat` of the path would take the editor's only name with it
   (**measured**: after such a rename-over, the path held a different
   inode with a link count of one; renaming the entry beside the path
   left the path absent, the editor's bytes under the new temporary name
   and the UI's under the old one). A rename moves whatever the entry
   holds, so it needs no check of whose inode that is, and a check would
   in any case be a moment older than the unlink it guarded. Since an
   intent may edit more than one file,
   a refusal that left some of them replaced would be a partial edit
   nobody asked for. The replacements are then retained under
   `<git-dir>/pbps-ui/previous/` exactly as a swapped-out original is
   below, named on the page as what a refused compose had placed, and
   never deleted: an editor that opened the replacement in the window it
   was at the path holds its inode and may save through it, and a rollback
   that unlinked the last name would take that save with it, which is the
   loss the retention exists to prevent. The UI unlinks one name only:
   the temporary a `link()` was made from, once step 5 has moved the
   branch, so that the new file has one name, at the path, and `git
   status` shows nothing untracked — a name the UI itself created
   exclusively, holding the inode the path holds. Every other name it
   takes away, it takes by exchange or rename, into a temporary name or
   the retention directory. The swapped-out files stay beside their paths
   under their temporary names until step 5 has moved the branch, and the
   retained copies below are made only then: until that point nothing has
   been let go, and every refusal before it — a path failing a check here,
   a `hash-object`, `write-tree` or `commit-tree` failing in steps 2 to 4,
   `validate` refusing the tree or the locked index copy refusing an
   entry in step 3, a signature the user's configuration cannot make, or
   step 5's
   compare-and-swap refusing because the branch moved — undoes step 2 the
   same way, exchanges back and unlinks in reverse order, and restores
   the tree exactly. The compose action promises a tree that matches a
   new commit on the branch, and a tree changed without that commit is a
   working-tree edit nobody made; the commit object a refused step 5
   leaves in the store names no ref and is pruned as garbage. If both match, the old version is
   still not deleted: an editor that opened the file before the exchange
   holds a descriptor to that inode and may write through it after the
   hash, and an unlinked inode would take that save with it. The
   swapped-out file is instead moved under `<git-dir>/pbps-ui/previous/`
   with the path and the time in its name, and named on the page beside
   the commit; a late write lands in a file the user can find, not in one
   that no longer has a name. The UI never deletes one of those files: a
   descriptor can outlive any number of composes, so a file it might still
   be written through is kept until the user removes it, and the page
   lists what is there and how to clear it. Where that directory is on
   another filesystem and
   the move fails, the file stays beside the path under its temporary
   name and the page says so. Every hash the UI takes of a working-tree
   file — here, in step 1, and of the swapped-out file — is taken over
   bytes read through the no-follow handle of step 1, fed to `git
   hash-object --stdin`, never by handing `git` the path to reopen: a
   directory component swapped for a link between the handle's opening
   and a reopen by name would have `git` read an outside file and put its
   bytes into the commit, while the UI had installed its own through the
   handle. A handle is proof of where a directory *was*: after every
   exchange, and after every `link()` below, the UI compares `fstat` of
   the directory handle with `fstat` of a fresh handle to the same path,
   opened from the root the same no-link way as the first — a lookup by
   name would follow a link put in the middle of the path since (**measured**:
   `lstat` of `dir/link/sub` named the sibling's directory) — and if
   they no longer name one directory — the ancestor
   was renamed away and another put in its place — the exchange is undone
   through the same handle, a linked name is renamed through it to a
   temporary name beside it as a rollback does, and the
   compose refused, since the UI's bytes would
   otherwise sit in a directory the worktree no longer contains while the
   commit named the path. `link()` needs this check as much as the
   exchange does: `EEXIST` guards only the leaf name, not where the
   directory is (**measured**: with the handle open, the directory renamed
   out of the tree and another created at its path, `link()` through the
   handle succeeded and put the file in the moved directory, the fresh
   lookup named a different inode from the handle, and a rename through
   the handle took the name away again). A rename after that comparison is a race the UI detects but
   cannot prevent, as nothing on these platforms locks a directory against
   being moved: what it leaves is a path `git status` reports missing and
   a retained copy of what was there, never a lost file, and the Limits
   section says so. Step 1's comparison and this write cannot
   be made one operation, so the exchange makes the write reversible
   instead: nothing is overwritten, only swapped, and what was swapped
   out is inspected and then kept (**measured**: the exchange put the UI's bytes at
   the path and the page's bytes in the swapped-out file, with the
   page's bytes intact to be compared). A path that is absent — a new
   declaration — has nothing to exchange with and is placed with `link()`,
   which creates the name only if it does not exist and refuses with
   `EEXIST` if something made it first (**measured**: the link landed the
   complete file under the new name, and a second `link()` onto a name an
   editor had meanwhile created was refused), so a file that appeared in
   the window is never replaced. On Windows, which has no exchange and
   no `openat`, the same shape needs its own calls: each component from
   the worktree root is opened with `CreateFileW` under
   `FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT`, so that a
   junction, a symbolic link or any other reparse point is opened *as
   itself* and refused when `GetFileInformationByHandle` reports
   `FILE_ATTRIBUTE_REPARSE_POINT`, and every component after the first is
   opened relative to the previous handle — `NtCreateFile` with the
   handle as `RootDirectory`, since `CreateFileW` takes only a whole path
   and would walk it again from the root, through whatever a component
   has become since. The identity comparison after each placement is the
   same as on Linux, taken over the volume serial number and file index
   of the two handles, and a mismatch is undone through the handle that
   made the placement. Within that directory handle the
   existing file is opened denying every other writer and deleter
   (`FILE_SHARE_READ` alone), hashed through that handle, refused if the
   handle reports more than one link (`GetFileInformationByHandle`'s link
   count) — the in-place write changes the file object every hard link
   shares, where the exchange replaces one directory entry — and, if it is
   still the page's version, rewritten through the same handle, so that a
   save cannot land between the check and the write because the editor
   cannot open the file for writing until the handle closes; a new file is
   created with `CREATE_NEW`, which fails if the name exists. An in-place
   write has no swapped-out inode to fall back on, so the retention and
   the rollback are done the other way round: the bytes read through the
   handle for the hash are written under `<git-dir>/pbps-ui/previous/`
   *before* the rewrite, the handle is held through step 5, and a refusal
   before the branch has moved restores the file by writing those bytes
   back through the same handle, which nothing else could have opened for
   writing in between; a name `CREATE_NEW` made is retained and its
   directory entry deleted through the handle (`FILE_DISPOSITION_INFO`),
   which is safe there as an unlink is not on Linux, because the handle,
   held with `FILE_SHARE_READ` alone, stops an editor renaming its own
   file over the name while the UI holds it. That
   in-place write is not crash-atomic, which is a different property from
   the one at stake here. Nothing on Windows has been measured for this
   ADR: step 3 of #64 measures every claim in this paragraph on a Windows
   machine, and until it has, the UI on Windows refuses to compose and
   gives the commands to run by hand, as the Limits section says a machine
   without `git` gets. A platform with none of these
   refuses to compose rather than overwrite a file it cannot prove is the
   one the page saw. Each file is then stored as a blob exactly as written,
   from the bytes the UI holds: `git hash-object -w --no-filters --stdin`.
   Without the flag
   `hash-object` runs the path's `clean` filter like `git add` does — a
   program from `.gitattributes` and the configuration that neither
   `core.hooksPath` nor `core.fsmonitor` reaches, which can rewrite the
   bytes or run anything it likes (**measured**: under `*.json filter=up`
   with an upper-casing `clean`, `hash-object -w` stored the upper-cased
   text and `--no-filters` stored the file). The declarations are this
   tool's own format, LF and UTF-8 by rule, so what the UI wrote is what
   the commit should hold. Whether it *is* a declaration is the CLI's
   question, not the UI's, and step 3 asks it of the tree it writes.
   Both snapshots hold every entry of the project's subtree, and both
   refuse the compose when one of them is not a regular file — a
   `120000` link or a `160000` gitlink — because `pbps_load` walks the
   declarations directory with `read_dir` and reads what it finds through
   the link, so a snapshot that dropped the entry would give the intent
   command and `validate` a schema the committed tree does not have, and
   one that wrote the link's text as a file would give them a
   declaration nobody wrote (**measured**: a tracked link to a
   declaration is a `120000` entry whose blob is the target's name).
   Refusing is the whole of the support: a linked declaration is a
   layout this UI does not compose for, the shell's commands work as
   before, and the Limits section says so.
3. In an index of its own (`GIT_INDEX_FILE`), it reads the recorded tip's
   tree and sets the entry for each edited path to that blob. The mode is
   the tip's for a path step 2 placed, since the exchange refuses a
   working-tree mode that differs from it; but for a *listed* file, which
   keeps the bytes and the mode the user gave it, the mode is the one `git
   add` would record — the execute bit `fstat` reports through the file's
   handle in step 1, or `100644` when `core.fileMode` is false — because
   the user may have changed that bit along with the content, or instead
   of it, and the tip's mode would leave the path modified after a compose
   that was supposed to leave `git status` clean (**measured**: a tracked
   declaration given an execute bit and committed at the tip's `100644`
   had `git status` reporting it modified, committed at `100755` it was
   clean, and `git add` recorded `100755`). For a path the tip does not
   hold, the mode is likewise the one `git
   add` would give the file step 2 found there: `100755` when it existed
   untracked with an execute bit and `core.fileMode` is true, `100644`
   otherwise, since a fixed `100644` leaves the commit disagreeing with
   the file it was made from (**measured**: an untracked `0755` file
   committed as `100644` had `git status` reporting it modified, `mode
   change 100644 => 100755`, and committed as `100755` it was clean; `git
   add` recorded `100755` for it, and `100644` under `core.fileMode`
   false, where the `100644` commit was clean too) — and writes the tree:
   `git read-tree <tip>`, `git update-index --add --cacheinfo
   <mode>,<blob>,<path>`, `git write-tree`. `--add` because a new path is not
   in the tree that was read (**measured**: without it, `cannot add to the
   index - missing --add option?`, exit 128). The user's own index is not
   read here. That tree is what the commit will hold, so it is what `pbps
   validate` is run on: the UI writes it out under
   `<git-dir>/pbps-ui/validate/<random>/` itself, from `git ls-tree -r -z
   <tree> -- <project>` and `git cat-file --batch`, the project's subtree
   only, and runs
   `pbps validate --format json --no-input --project <that
   directory>/<the project's path in the worktree>`, as decision 1 runs
   every command; a failing envelope rolls step 2 back and shows its
   findings. The bytes are an intent command's, but the tree that holds
   them on the tip is the UI's assembly, and ADR-0006 keeps the one
   validation path in the CLI, so the CLI is asked — about the tree, not
   the checkout: an editor can rewrite a placed file while `validate`
   reads the checkout, and a check that read the editor's bytes would
   have passed the UI's. The snapshot is written by the UI rather
   than by `git checkout-index --prefix` or `git archive` because both
   run the `smudge` program of a path's `filter` attribute and
   `cat-file` runs none (**measured**: under `*.json filter=up` with a
   `smudge` that left a marker, `checkout-index -a --prefix` and
   `archive` both left it, `cat-file --batch` and `cat-file blob` did
   not). A tree's names are bytes `git` stores, not paths it has
   checked: `mktree` refuses a name with a slash in it but accepts an
   entry named `..`, and a tree entry so named nests, so `ls-tree -r -z`
   hands back `../file` as a name (**measured**: a tree holding a
   subtree named `..` was made, `ls-tree -r` printed `../file`, and only
   `fsck` complained, `hasDotdot`), and a snapshot writer that joined
   such a name to its directory would write outside it — into the
   repository, the index or the working tree — before any guard of step
   1 ran. Both snapshot writers, here and in the intent snapshot above,
   therefore split every name on `/` and refuse the compose if any
   component is empty, `.`, `..`, or, on Windows, contains `\` or is a
   drive or device name, and create every directory and file relative
   to a handle on the snapshot directory (`mkdirat`, `openat` with
   `O_CREAT | O_EXCL | O_NOFOLLOW`; the relative `NtCreateFile` of step
   2 on Windows), never by joining the name to a path, so that a name
   which passed the check still cannot reach past the handle. Which
   rules `validate` enforces is SPEC's and unchanged here.
   Then, with the tree known to be one `validate` accepts, the same
   entries are written into the locked copy of the user's index —
   `GIT_INDEX_FILE=<index>.lock git update-index --add --cacheinfo
   <mode>,<blob>,<path>` — now rather than in step 6, because that write
   can fail where the temporary index's did not: a staged entry of the
   user's own can occupy a new path's directory as a file, or its name as
   a directory, and `update-index` refuses the entry rather than replace
   the user's (**measured**: with `schema/new` staged as a file, adding
   `schema/new/x.json` to a copy of that index failed with `appears as
   both a file and as a directory`, exit 128, while the same entry went
   into an index read from the tip; a staged `schema/dir/f` refused
   `schema/dir` the same way). Step 1's check covers the edited paths'
   own entries, not their neighbours, so the refusal has to come from the
   write itself, and it has to come before step 5 moves the branch, which
   is why the copy is prepared here and only installed there. `--replace`
   would let the entries through by dropping the user's staged ones,
   which is the work step 1 refuses to touch.
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
   was: `git update-ref --no-deref refs/heads/<branch> <oid> <tip>`, a
   compare-and-swap on the named ref itself — `--no-deref` so that a branch
   turned into a symbolic ref since step 1 is not followed to a target the
   UI never locked — 
   that refuses if anything moved the branch in between (ref locks are not
   the index lock; **measured**, the update went through with the index lock
   held) — a refusal here undoes step 2 as described there, since the tree
   would otherwise hold an edit no commit records — and the one step that
   changes the checkout. `update-ref` takes
   `HEAD`'s lock itself when `HEAD` names the branch it moves (**measured**:
   with `HEAD.lock` held it failed with `cannot lock ref 'HEAD'`), so the
   UI releases `HEAD.lock` for this one command and takes it back right
   after, together with the branch's own lock — an empty
   `<git-common-dir>/refs/heads/<branch>.lock`, created exclusively — that
   lock, like `HEAD.lock` and `index.lock`, is the *files* ref backend's own
   protocol, so the UI refuses a repository whose `extensions.refStorage`
   names another backend (`reftable` keeps branches in tables no such file
   guards) rather than hold a lock that locks nothing — and
   only then checks that `HEAD` is still symbolic to the recorded branch,
   that the branch is still a direct ref and not a symbolic one (`git
   symbolic-ref refs/heads/<branch>` must fail, as in step 1 — a sibling
   worktree can make it one under the locks of step 0, which do not cover
   the branch), and that the branch still names `<oid>` (`git rev-parse
   refs/heads/<branch>`).
   The gap admits a `symbolic-ref` and it admits a `reset --soft`, which
   moves the branch under a held index lock, and either leaves an index
   built for `<oid>` wrong for the checkout; and a branch is shared by every
   worktree of the repository while `HEAD.lock` and `index.lock` belong to
   one, so without the branch's lock a `git update-ref` from a sibling
   worktree could move it after the check and before step 6 (**measured**:
   with the linked worktree's `HEAD.lock` and `index.lock` held, the update
   from the main worktree went through; with the branch's lock held too, it
   failed with `cannot lock ref`, while the UI could still read the ref).
   Both locks are held through step 6. If either check fails, the commit
   exists and is where `update-ref` put it, but the checkout is no longer
   at it, so step 6 does not happen and nothing is pushed: the locks are
   discarded and the index is as it was. What becomes of the placed files
   is decided by the tip the branch *actually* names under the lock, read
   before anything else is undone, never by the assumption that it is the
   UI's commit — a `symbolic-ref` in the gap leaves the branch at that
   commit, but a sibling worktree's `update-ref` or `reset` leaves it at
   something else entirely, and the earlier rule kept the files on a
   premise that had stopped being true. So the UI reads that tip's blob
   for each edited path (`git ls-tree -z <tip> -- <path>`) and keeps the
   placed file where the tip holds exactly what was placed, and undoes
   step 2 for the path where it does not, leaving each path at what the
   branch it is on records. The page then names the commit, the tip the
   branch actually holds, and every path with what became of it, since
   the user's index is the one they had and only they can say which of
   the two states they want. **Measured**
   both ways: undisturbed, the check passed and the index was installed
   clean; with a `symbolic-ref` to a sibling in the gap, the check failed,
   the branch held the commit, the sibling was untouched, and the index was
   left alone.
6. It installs the locked copy of the index that step 3 prepared by
   renaming `<index>.lock` to `<index>`, which is exactly the commit step
   of `git`'s own lock. Now
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

The preview is `git diff --no-ext-diff --no-textconv --text <tip> <tree>`:
the recorded tip against the tree the UI built, exact by construction, and
rendered without presentation filters because a `diff.external` or
`textconv` driver can show two blobs as one text (**measured**: a driver
printing a constant hid a rewritten file), and as text because a
`.gitattributes` line can mark the declarations `-diff` and the two flags
do not override that (**measured**: with `*.json -diff`, the preview said
`Binary files a/ids.json and b/ids.json differ`; with `--text` it showed
the hunk). Every `git` the UI runs takes
`--literal-pathspecs` and `--no-replace-objects`, runs from the worktree's
root (`-C <git rev-parse --show-toplevel>`) with every path spelled relative
to that root and refused if it lies outside it, runs with every
repository-locating `GIT_*` variable of the UI's own environment removed
(`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`,
`GIT_COMMON_DIR` and their kin) so that the repository is the one found from
that root and not one a relative variable pointed at from wherever `pbps ui`
was started (**measured**: from a subdirectory, `GIT_INDEX_FILE=alt-index`
named `../alt-index`, and the same variable under `-C <root>` named a
different file, `alt-index` at the root), every path comes after `--`,
and every command that prints paths takes `-z` and is parsed as bytes — each
for a reason that was measured. `a[12].json` is a pattern to `git` and
selected three files without `--literal-pathspecs`. A file named `-A` made
`git add -N -A` mark every untracked file. `schéma.json` came back C-quoted
from `ls-tree` without `-z`. A pathspec is resolved from the current
directory while a `--cacheinfo` path is an index name from the root: run
from a subdirectory `p/` of the worktree, the pathspec `a` selected `p/a`
and `--cacheinfo …,a` wrote the entry `a`, so step 1 would have checked one
file and step 6 replaced another. And a `refs/replace/<tip>` is honoured by
`read-tree`, `diff` and `merge-base` but recorded by nobody: with the tip
replaced by a commit that changed an unrelated file, the preview showed the
one edited path and the commit — whose parent is the real tip — carried
both, where `--no-replace-objects` read the real tip throughout.

The push is bounded to that one commit, to it by name, and to one
destination. A remote may carry several push URLs, and `git push` sends to
every one of them while `ls-remote` and the lease each speak to
one (**measured**: two `pushurl`s, two destinations listed by `git remote
get-url --push --all`, and a push reaches both, so a lease can hold at the
first and fail at the second after the first has published). The UI
requires that command to name exactly one URL, refuses the remote
otherwise, listing them, and uses *that URL* — not the remote's name — for
`ls-remote` and the push alike, since the name resolves to the fetch URL
for the first and to the push URL for the second, and a
`pushurl` that differs from the fetch URL would have the checks look at one
server and the push go to another. The URL is handed to `git` as a remote
that exists only in the environment of the two processes that use it —
`GIT_CONFIG_COUNT=1`, `GIT_CONFIG_KEY_0=remote.pbps-ui.url`,
`GIT_CONFIG_VALUE_0=<push-url>` — and named on their command lines,
never written on a command line or into the configuration file
(**measured**: `ls-remote` and `push` of that name under that
environment reached the URL, the configuration file did not mention the
name, and without the environment the name resolved to nothing). The
name is `pbps-ui-<random>`, drawn per launch, and the UI first requires
`git config --get-regexp '^remote\.<name>\.'` to find nothing, because a
remote's `url` is multi-valued and an environment entry *adds* to a name
the configuration already has rather than shadowing it (**measured**:
with a remote of that name already configured, `get-url --all` listed
both URLs and one push published to both repositories; overriding the
*existing* remote's `url` the same way appended the override after the
fetch URL; the regexp found nothing for a fresh random name and exit 0
for the configured one), and a `pushurl` under the name would win over
the `url` the same way. A URL may carry a credential, and `git remote get-url` prints one
verbatim (**measured**: `https://tok3n@…` came back as typed) where `git`'s
transport strips it from its own diagnostics (**measured**: `ls-remote`
and `push` against that URL reported `unable to access
'https://127.0.0.1:1/x.git/'`); a command line is readable by every user
of the machine through `/proc/<pid>/cmdline` and the environment by the
process's owner alone (**measured**: `/proc/<pid>/environ` is mode
`0400`), which is who can already read the configuration file. So the
URL stays in the UI process, and every URL the page is shown — the list
on refusal, the destination named after a push, the commands offered for
the shell — is shown with its userinfo removed, everything between the
scheme's `//` and an `@`, and every line of `git` output relayed to the
page passes through the same removal, in case a helper is less careful
than the transport. One more thing can move the destination after the
URL is chosen: a `url.<base>.insteadOf` rule rewrites a URL for every
command and a `url.<base>.pushInsteadOf` rule for pushes, `git remote
get-url --push` has applied one round of them already, and the
environment-only remote's URL is rewritten *again* on use by any rule
whose value is a prefix of it (**measured**: with `pushInsteadOf` rules
chaining `src` to `mid` and `mid` to `fin`, `get-url --push` answered
`mid`, `ls-remote` of the environment-only remote spoke to `mid`, and its
push published to `fin`; an `insteadOf` rule for `mid` then sent
`ls-remote` to `fin` as well). So the UI lists every rule — `git config
--get-regexp '^url\..*\.(push)?insteadof$'` — and refuses the compose,
naming the rule, if any rule's value is a prefix of the chosen URL: the
URL the checks spoke to and the URL the push goes to must be one string,
and a rule that would rewrite it makes them two. The check reads the
configuration rather than asking `git` about the environment-only
remote, because `git remote get-url` does not see a remote that exists
only in the environment (**measured**: `No such remote`) while
`ls-remote` and `push` do. Before composing,
the UI reads the remote's tip for the branch (`git ls-remote <name>
refs/heads/<branch>`) and refuses to compose unless the local tip equals it,
showing the unpushed commits and the commands instead: a refspec bounds the
destination ref, not the range, and **measured**, a branch one unrelated
commit ahead had that commit published under the intent commit by
`HEAD:refs/heads/<branch>`. A branch the remote does not have yet — the first
push of a feature branch, the common case — is published *before* composing,
as its own step the page names as such: `git push --no-verify
--no-follow-tags --recurse-submodules=no <name>
<tip>:refs/heads/<branch>` with an empty lease (`--force-with-lease=
refs/heads/<branch>:`) — the same flags as the final push, since
`push.followTags=true` would otherwise send every annotated tag reachable
from the tip along with the branch (**measured**: the branch and an
unrelated tag both arrived; with `--no-follow-tags`, the branch alone) —
which creates the branch at the tip the checkout
already has and sends nothing the remote lacks when that tip is already
there (**measured**: the branch appeared at the tip, `ls-remote` then
equalled the local tip, and a second attempt was refused as `up-to-date`
because the destination existed). After it the branch is an existing branch
and the equality rule above applies unchanged. An earlier draft of this
design proved the tip was on the remote through a "witness" head and leased
that head in the same push; it is withdrawn because a refspec the remote
already has at that value is dropped from the push as up to date, so the
witness was never in the transaction it was meant to guard, and because a
branch created at a tip the user's checkout is on is a thing the user can
be shown and asked about, which a witness was not. The push then names the
commit by id and leases the destination on the tip it recorded.
It takes `--no-verify`
as well as the empty `core.hooksPath` every `git` here gets, because a
`pre-push` hook is a hook (**measured**: it ran without the flag and not
with it), and `--no-follow-tags`; never a bare `git push`, which under
`push.default=matching` advanced two branches at once when measured:

```
git push --no-verify --no-follow-tags --recurse-submodules=no \
    --force-with-lease=refs/heads/<branch>:<tip> \
    <name> <oid>:refs/heads/<branch>
```

Both pushes — this one and the one that publishes a new branch — take
`--recurse-submodules=no`, because `push.recurseSubmodules=only` makes
`git push` skip the superproject's own refs and report success
(**measured**: under it the push said `Everything up-to-date` and the remote
stayed at the tip; with the flag it moved to `<oid>`), and a UI that read
that exit code would show a publication that never happened.

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
ask about. A `git` that would prompt — for a passphrase, for a credential —
must fail rather than wait, and taking away the terminal is not enough for
that: a credential helper, `GIT_ASKPASS`, `SSH_ASKPASS` or a pinentry can put
up a window of its own or block with no terminal at all (**measured**: with
`GIT_TERMINAL_PROMPT=0`, `git credential fill` still ran the program
`GIT_ASKPASS` named). So every `git` the UI runs gets `GIT_TERMINAL_PROMPT=0`,
`GIT_ASKPASS` and `SSH_ASKPASS` pointed at a program that exits non-zero
(**measured**: `git` then fails at once with `unable to read askpass
response`), `SSH_ASKPASS_REQUIRE=never`, and a deadline: a subprocess still
running when it expires is killed with its process group, and the page shows
the command to run by hand, which is SPEC §6.4's rule for the CLI applied
unchanged. The deadline is what bounds the helpers the environment cannot
reach, such as a pinentry the user's `gpg-agent` chooses.

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
- **A declaration reached through a symbolic link.** `pbps_load` follows
  one; decision 5's snapshots cannot hold one without either dropping a
  declaration or inventing one, so a project whose subtree holds a
  `120000` or `160000` entry is refused with the entry named. Supporting
  it means deciding what a commit of a link's target should mean, which
  is a question for when someone has such a layout.
- **A project whose declarations or ids file lie outside its directory.**
  Decision 5 snapshots the project's subtree and asks `doctor` where the
  inputs are; a `schema_dir` or `ids_file` that resolves outside the
  directory holding `pbps.yml` is refused with the paths shown, and the
  shell's commands work as before. Composing for such a layout needs the
  snapshot to follow the paths, which is a decision for when someone has
  one.
- **The `git` binary's presence on the machines the UI targets.** Decision 5
  assumes the DBA who will not run five commands still has `git` installed,
  because the checkout they are looking at came from somewhere. A machine
  without one gets the commands to run by hand and no worse.
- **An ancestor directory moved during a compose.** Nothing on Linux,
  macOS or Windows locks a directory against being renamed by another
  process, so decision 5 detects the move after each exchange or `link()`
  and refuses,
  and a move after that detection leaves a path `git status` reports
  missing beside a retained copy. Step 4 measures how narrow that window
  is and whether resolving the exchange's own paths with `openat2`'s
  `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS` (Linux 5.6+) closes it.
- **DNS rebinding through browsers that pass a numeric `Host` unchanged.**
  Decision 3's `Host` check is the standard answer; step 3's tests send the
  cross-origin `POST`, the rebinding `Host`, the foreign peer and a request to
  any route but the shell without the header, and watch each refused, which is
  the test ADR-0006's "structurally rather than by discipline" asks for.
