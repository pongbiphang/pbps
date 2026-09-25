# Compose and the local UI

The local viewer and the compose flow that turns reviewed intent into a git
branch. Part of the [decision record](../DECISIONS.md), which says how to add an
entry here.

<a id="decision-457"></a>

457. **The local viewer is a synchronous CLI consumer (issue #117).**
     `pbps-ui` depends only on serde, serde_json and `tiny_http` 0.12.0, with
     default features disabled. The HTTP crate adds ascii, chunked_transfer
     and httpdate; log was already present. Its [documented synchronous
     server](https://docs.rs/crate/tiny_http/0.12.0/source/src/lib.rs) supplies
     HTTP/1.1 framing without TLS, a framework or another async runtime.
     `cargo deny check` accepted the resulting tree. CLI uses the existing
     rand and base64 dependencies to supply a 256-bit launch token and the
     exact documentation stylesheet's SHA-256 CSP hash. The UI crate has no
     workspace dependency; a test pins its complete dependency list.

     Measured on Linux x86_64 with Rust 1.98.0, the default release profile
     and `cargo build --release -p pbps-cli`: baseline master `2fe40e0` was
     24,835,096 bytes, and the viewer build was 25,678,104 bytes, an increase
     of 843,008 bytes (3.39%). Both builds used the same toolchain and target
     directory, with the baseline binary copied before building the viewer.
     This keeps ADR-0015 decision 6's single-binary placement.

     Each read spawns the running executable with the project directory and
     `--no-input`; environment selections use `--env`, never a browser-sent
     connection string. Only status, verify, saved-plan explain, state list
     and docs are representable. Ordinary CLI behavior includes verify's
     configured on_drift hook; the viewer introduces no scheduler. Raw stderr
     is not a browser data channel. Findings and absent data stay distinct.

     The independently written consumer types refuse unknown envelope and
     payload fields, wrong command/version and inconsistent exit outcomes.
     Drift's changes field is intentionally opaque in the published schema:
     the page displays its entire JSON as text and does not reinterpret
     model variants. No other structured payload is opaque. Golden tests
     feed real offline and live CLI output to this consumer.

     Every request is checked before routing, including unknown routes:
     loopback peer, exactly one bound Host, matching Origin when present
     (required for methods other than GET/HEAD), and exactly one launch token
     header. Only the three immutable shell resources omit the token check.
     Authenticated non-read methods are refused too. The page renders data
     as text and docs in a script-free sandboxed frame; no-store applies to
     responses, and CSP permits only shipped assets plus the measured style
     hash. These boundaries ship with the read views, before any compose or
     deployment UI exists.

<a id="decision-487"></a>

487. **Compose checks ref type before publishing and HEAD's immediate target
     before installing the index (issues #385 and #386).** ADR-0015 decision 5
     uses an interactive `update-ref --stdin` transaction: start, no-deref
     update, prepare, then inspect the live branch under Git's lock before
     committing. The old-value CAS follows symbolic indirection even with
     `--no-deref`; the single-shot form can overwrite a same-tip symbolic
     ref before a post-write type check sees it. Abort preserves that ref.

     Preparation does not replace the post-write guard. After Git releases
     its locks, the UI retakes its persistent HEAD and branch locks and
     requires a direct branch at the composed commit and the exact immediate
     target from `symbolic-ref --no-recurse HEAD`. A recursive read admits
     an intermediate ref neither lock protects. The same immediate-target
     condition governs success recovery; rollback remains irreversible.

     The retained `spikes/git-compose-refs` experiment measures both races,
     held locks, unchanged user index, and ordinary/stale-tip controls on
     Linux with Git 2.43.0. Restoring the single-shot update fails the
     same-tip assertion; restoring recursive HEAD reads fails both inserted
     hop assertions. These are ref-protocol measurements, not an implemented
     UI compose or crash-recovery harness. #494 / #64 step 4 must test the
     full protocol; #471's Windows restoration remains separate. No database
     model, approval, saved-plan format or execution boundary changes.

<a id="decision-527"></a>

527. **Compose owns an isolated candidate and a new output branch, not the
     source checkout.** ADR-0015 decision 5 combined the core intent/review/Git
     workflow with a promise to synchronize working files, the user's index
     and its current branch. Git has no transaction across those resources.
     PR #738's repeated publication and recovery findings expose the cost of
     that contract as well as implementation defects (#744).

     [ADR-0017](../ADR-0017-isolated-compose.md) replaces that promise: capture and
     validate an immutable candidate through the ordinary CLI, show its exact
     tree, create that tree/parent as a commit on a fresh output branch, and
     push that same commit to one reviewed destination. The original checkout
     keeps its edited declarations and old ids. Continuing the result requires
     a separate checkout, explicitly explained by the UI; no automatic
     copy-back or reset recreates the removed transaction.

     This supersedes the live-placement preview proposed as decision 524 on
     the unmerged #738 branch. Entries 523–526 are reserved by that branch;
     their numbers are not reassigned here. Existing identity/signing,
     subprocess, browser-security and no-unrelated-history guarantees remain.
     A prepared ref transaction still needs a live type check: the retained
     Git 2.43 experiment demonstrates that expected-absent creation alone
     overwrites a dangling symbolic ref. An uncertain acknowledgment remains
     uncertain until reconciled; a push failure never undoes the local result.

     The retained `spikes/git-compose-isolated` experiment checks the mechanism
     and its rejected alternatives with real Git and CLI, including unchanged
     source files/index/HEAD, exact output, ref collisions, ancestry, signing
     failure and push outcomes. It is not production crash qualification.
     #745–#748 implement and qualify the selected protocol. The read-only UI
     remains read-only until that delivery is complete; old experimental
     recovery records must be recognized and refused without losing evidence.

<a id="decision-530"></a>

530. **A compose preview owns its input bytes and output tree before it owns
     confirmation (issue #745).** Decision 527's isolated architecture replaces
     the former preview/place/undo/re-read cycle: a server-owned candidate binds
     the complete input membership, absence, modes, attributes, base, intent,
     message, destination and preallocated operation/output-ref name. The diff
     comes from the resulting tree, with forced text rendering even under
     `-diff`. Confirmation accepts only its opaque handle; later source edits
     cannot enter that tree. Refresh replaces the handle even when capture
     fails, and unconfirmed handles expire after 24 hours.

     Paths are obtained from `doctor --paths-only` before the CLI loads any
     declarations. This is a separate tagged report, not a successful diagnosis
     with fabricated zero counts. The UI can then refuse escaping paths without
     parsing YAML. Ordinary intent commands and `validate` retain schema
     ownership. Declaration-only changes use `plan --no-dev` to resolve ids
     without a configured rehearsal; a final `plan --check` reconciles the
     result against the captured base. `--no-dev` still writes the ordinary ids,
     skips only rehearsal and refuses explicit `--dev` or target planning.

     Git resolves line-ending policy through a private index and an empty
     worktree, with attributes sourced from the reviewed base. This preserves
     legacy `crlf` semantics and attribute precedence without reproducing Git's
     parser or executing filters. Relevant attribute files must match the base
     byte-for-byte and enter the manifest: `check-attr` text alone cannot
     distinguish an unset attribute from a string value literally named
     `unset`. Contained parent path components are resolved, while escapes
     beyond the project remain refusals.
     When captured bytes contain CRLF, a sterile private Git probe compares
     raw and converted object identities using only that closed text policy.
     Bare CR and `text=auto` binary heuristics are Git's decisions, not a second
     normalization implementation. The probe loads no source configuration or
     helper and performs no checkout.

     Real Git/CLI scheduling tests pin source preservation and frozen output;
     browser tests execute the shipped form rather than search its source for
     variable names. Node.js is required only to run those development tests
     (`PBPS_TEST_NODE` may select the executable). The viewer remains read-only
     until publication, durable retirement and restart qualification (#746–#748)
     are complete. This entry does not choose the endpoint-admission policy
     whose separate decision record is tracked by #757.

<a id="decision-532"></a>

532. **A compose result records authorization separately from observation.**
     (#746, ADR-0017.) Commit preparation, an exact prepared commit, local
     publication authorization and acknowledged local publication are separate
     durable phases. Remote delivery adds unattempted/attempted/delivered facts
     with an action generation. A missing acknowledgment never authorizes
     source restoration, another commit, or automatic branch recreation.

     Persist local authorization while the prepared no-deref create transaction
     holds the output-ref lock; abort if that prerequisite fails. Persist remote
     authorization before invoking Git push. Ordinary failure and restart use
     the same reconciler, preserving exact commit details and distinguishing
     absent, changed and unreadable evidence. A known local result survives
     failed push, failed result persistence and pending private cleanup.

     An absent remote ref after an attempt can mean deliberate later deletion.
     Only informed explicit republish may recreate it, using the same commit,
     ref and endpoint. Consume the displayed action generation before checking
     even an already-present remote: replay after later deletion cannot become
     new permission. HTTP redirects are disabled for every observation/push,
     including URL-scoped configuration overrides, because configuration alone
     does not bind the redirect target. Authentication remains invocation-only.

     The browser renders typed facts, offers saved-result reconciliation and
     separate-checkout continuation, and requires explicit authorization and a
     fresh preview for an alternative pinned to the original base. The viewer
     still rejects writes until #747 resource durability/retirement and #748
     integrated interruption qualification complete. No source file, index,
     HEAD or source-branch transaction is introduced.

<a id="decision-534"></a>

534. **Compose network pushes rely on ordinary direct-branch server semantics.**
     (#746, PR #768, ADR-0017 Remote branch semantics.) The user selected
     ordinary HTTP/SSH Git push with an explicit server prerequisite over a
     new remote inspection/no-deref capability protocol. Selected base and
     output branch names must denote direct branches; symbolic branches and
     administrative write remapping are unsupported server configurations.
     This narrows the guarantee instead of claiming those configurations can
     always be discovered and refused by a Git client.

     A real Git 2.43 local-bare and smart-HTTP experiment showed why: a dangling
     symbolic output ref produces an empty advertisement, but an expected-absent
     push creates its other target while leaving the output ref symbolic.
     Atomic push does the same. Advertised values and leases are useful within
     the server contract; they do not attest hidden ref type or implementation.
     A server violating the premise can still redirect the write. A later read
     cannot repair that safety claim. Disclose the premise before confirmation.

     Local filesystem destinations additionally expose raw Git ref evidence.
     Compare it with the advertisement and recheck direct absence immediately
     before push. Symbolic, unreadable or inconsistent evidence refuses while
     preserving the exact local commit, and retry/recovery never clears the
     conflicting ref. Observed collision checks do not freeze a remote
     administrator during a push. Keep endpoint identity, disabled HTTP
     redirects, exact-commit delivery and uncertain-outcome rules independently.
     No remote attestation service, arbitrary command executor or new credential
     store is introduced. This decision does not enable the write endpoints
     before #747–#748 and integrated #494 qualification.

<a id="decision-535"></a>

535. **Compose retires acknowledged resources independently of publication.**
     (#747, ADR-0017.) The private resource lifecycle is capturing, sealed,
     confirmed, retiring, retained and spent. Acquisition intent is durable
     before creation; acknowledgment records ownership. A pathname, matching
     hash or matching Git OID without acknowledged acquisition is insufficient
     authority to delete. Interrupted unsealed acquisition retains named manual
     recovery evidence rather than guessing that every child has stopped.

     Root the source base before borrowing objects through an alternate. Root
     the candidate tree in its private repository. Record the returned exact
     commit in `CommitKnown`, then acquire and acknowledge its private root before
     `Prepared`; an interruption before acknowledgment cannot regenerate the
     commit. Operation-specific annotated tags supply independent GC roots.
     Direct no-deref transactions create/delete these private refs, while Git
     owns its temporary ref locks. Never remove a foreign or ambiguous Git lock.

     Descriptor-relative record replacement checks prior identity/revision and
     flushes the file and containing directory. Private retirement is governed
     by an acknowledged inventory and a durable retirement intent. Unknown,
     changed or unreadable entries keep that intent and record pending. Known
     completed deletion is idempotent. Explicit unlock uses a retained lock
     inode; closing one descriptor alone can leave a fork-shared flock alive.
     Destructors cannot establish successful retirement.

     Only unconfirmed sealed previews expire after 24 hours. Completed snapshots
     may retire while a compact receipt and exact-commit root remain. Explicit
     forget retires those private artifacts and leaves a bounded spent tombstone
     so an old candidate handle cannot recreate deleted output. Public branches
     are never cleanup targets. Unknown publication or pin ownership cannot be
     forgotten or expired. Legacy experimental evidence blocks new compose
     without migration or old source recovery. Lower-level interruption tests
     support these rules; #748 and #494 still gate the integrated write surface.

<a id="decision-536"></a>

536. **Completed capture rejection retires pre-check ownership without revoking
     an unexposed handle.** (#780, ADR-0017.) Move bounded base/configuration
     admission before private acquisition. For checks needing an isolated
     snapshot, distinguish a normal completed refusal from uncertain child or
     I/O outcomes. Use an inventory acknowledged before the check, limited to
     the paths produced by completed fixed preparation steps; never infer
     cleanup permission from a generic error, destructor, lock filename or a
     fresh inventory taken after failure. Added or changed entries keep the
     retirement pending, as do unresolved acquisition and required flushes.

     Persist the completed-rejection fact with the ordinary retirement intent
     before deleting anything. It is valid only for an unsealed capture without
     a reviewed binding or commit. Restart uses the same attributable retirement
     operations. Once every private artifact and base pin is durably retired,
     remove that rejection record: no candidate handle was exposed, so no
     permanent revocation tombstone is needed. This prevents routine invalid
     inputs from exhausting the operation-record bound. Sealed, confirmed and
     forgotten handles still require their normal spent-identity protection;
     unknown children, temporary files and ambiguous pins remain retained.
     No source checkout, index, HEAD or public branch is a cleanup target.

<a id="decision-541"></a>

541. **A compose destination is a credential-free identity; authentication is
     reacquired per invocation, and an endpoint that cannot be represented
     without a secret is refused.** (#757, recording the policy #750 specified
     in [ADR-0017](../ADR-0017-isolated-compose.md#durable-identity-and-transient-authentication);
     requested in [#756's review](https://github.com/pongbiphang/pbps/pull/756#discussion_r4062131848).)
     ADR-0017 forbids a credential store, yet a candidate must bind the exact
     destination it was reviewed against across confirmation, retry and
     restart. Those two demands meet in three separate representations:
     a durable *destination identity* (admitted transport, exact host, port
     and repository path, any explicit public SSH principal, or the local
     repository identity, paired with the base and output refs); the
     *candidate identity* that binds it with the reviewed tree and signing
     policy; and *invocation authentication*, which Git and its approved
     helpers obtain afresh for every call and compose never serializes,
     logs, relays to the browser, snapshots or hashes.

     HTTP(S) endpoints carrying userinfo, a query or a fragment are refused
     before sealing, not cleaned. A redacted display string cannot bind
     anything, because two different destinations redact to the same text,
     and equality of redactions is exactly the comparison drift detection
     would then rely on. A hash of the secret-bearing URL is worse: it is
     durable evidence derived from the secret, and it changes when the
     secret rotates, so refreshed authentication for the same repository
     would read as destination drift. Stripping the parts is not safe either,
     since they can encode routing as well as credentials, and the stripped
     URL may name a repository nobody reviewed. The refusal names the unsafe
     component's category without echoing it, and points at a credential-free
     endpoint plus a credential helper; the ordinary CLI remains available for
     other forms.

     An explicit SSH account name (`git@host:team/repository.git`) is kept as
     a bound field. The trade-off is that a public principal becomes part of
     durable records and of the reviewed identity: it is not secret, and for a
     relative scp-like path it can select a different repository, so dropping
     it would lose routing. Changing it therefore requires a new candidate,
     like a changed host, port, path or ref. An implicit principal, or an
     opaque transport whose effective destination cannot be established, is
     refused until it has a qualified identity protocol. None of this attests
     the server; ordinary Git transport and host-key policy still apply.

     On retry or restart, authentication is reacquired from the currently
     approved source and the endpoint re-resolved and compared field by field
     with the sealed identity before contact. Missing authentication is an
     actionable failure with the local result kept; refreshed authentication
     for the same identity proceeds under the ordinary retry rules. The
     regressions for fake credential markers, endpoint drift and refresh
     belong to #745–#747.

<a id="dec-1025-1"></a>

**DEC-1025.1. The viewer triggers `plan` and `apply` through fixed actions with
typed fields, and shows the CLI's own outcome. No approval and no new envelope
are added.** ADR-0006 admits the trigger on one condition: the UI never holds
the approval. The apply form therefore takes the checksum and the allowed risk
classes as a person types them.
- The field starts empty. Nothing the viewer reads fills it, not even the
  plan it has just shown, because a checksum filled in from that plan would
  turn the deployment gate into a click.
- `POST /api/trigger/plan` and `/apply` take a JSON body that refuses unknown
  fields. Each value becomes one joined `--flag=value` argument. The
  environment is passed as `--env`, never `--db`, and a risk class must be a
  lowercase hyphenated word, so no value can become another option, a second
  command or SQL.
- `apply` speaks no envelope. ADR-0015 left open whether step 2 should add one,
  or whether the page should re-read `status` after the fact. What the page
  renders after an apply is the exit code, the CLI's own words (why a checksum
  or an allow list was refused) and the recorded entry. The first two are the
  child's exit status and output, relayed as text and truncated past 64 KiB per
  stream. The third is the ledger, which the Timeline view already reads with
  `state list`. An `apply --format json` would carry a second description of
  what the ledger records, so it is not built.
- The output channel is new for the viewer: its reads relay only a typed
  envelope and drop stderr. A command that speaks no envelope has no other way
  to say why it refused, and the CLI already keeps connection strings out of
  what it prints (ADR-0015 decision 4).
- Tests: `trigger` pins the argument vectors and the refusals.
  `ui.rs`'s `a_connection_string_never_reaches_a_trigger_response` runs a
  connected plan that fails to connect and a malformed apply, and searches
  every response for the password. `browser.rs`'s
  `the_trigger_form_sends_only_the_typed_checksum_and_never_fills_one` runs
  the shipped script.

<a id="dec-1025-2"></a>

**DEC-1025.2. The viewer's `plan --out` must name a path that does not exist.**
`plan --out` replaces whatever is at its path. A saved plan is the artifact a
checksum is approved for, so replacing one from a browser form could put
different bytes behind an approval that is still pending. The viewer refuses
any path that already names something, a dangling link included, and one it
cannot inspect. The check runs before the child starts.

Another process on the same machine could create the file between the check
and the write. That race needs a concurrent writer of the same path under the
same user, which a single-user local tool does not defend against, and the CLI
would still write a complete plan whose checksum `explain` shows. The
alternative was a private directory the viewer owns. It was rejected because a
plan is meant to be passed on to the reviewer who approves it, and a file under
a temporary directory is the wrong place for that.

<a id="dec-1025-3"></a>

**DEC-1025.3. A triggered run is not tied to the request that started it: one
run per environment, polled for its outcome.** The viewer's server answers one
request at a time, and an apply can run for minutes. The child is therefore
spawned, its output drained by two threads, and the request answered at once.
The page asks `POST /api/trigger/runs` every two seconds while a run is going.
- A run is reported as ended only when the child has exited and both of its
  output streams are closed. The page stops asking about an ended run, and an
  exit seen before the last output was drained would lose the refusal's own
  words.
- Closing the tab or dropping the connection cannot stop the child, because
  nothing ties it to them.
- A second run against an environment is refused with 409 until the first
  ends. The ledger's lock still decides between this viewer and a terminal or
  CI job, as it does for two terminals.
- The child is an ordinary member of the viewer's process group. Ctrl-C in
  the viewer's terminal therefore reaches it exactly as it would reach
  `pbps apply` run in that terminal, and the CLI's own interruption behavior
  applies. Detaching it instead would leave an apply running with nobody able
  to see its outcome, and would need platform-specific process code, which
  #1025 keeps out of the trigger.
- Only `std::process` is used, so the trigger is the same on every platform
  and #471 has nothing to port for it.
- The viewer keeps each environment's latest run in memory, with the
  arguments it ran, so the page can show what happened. Nothing is written,
  and it is gone when the viewer exits. A remembered checksum is only a
  record of what ran, and it cannot authorize the next run.

