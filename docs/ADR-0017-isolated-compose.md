# ADR-0017: Compose an isolated candidate and publish a new branch

- Status: candidate backend implemented; publication/recovery remain disabled
- Date: 2026-09-21
- Supersedes: ADR-0015 decision 5's same-checkout publication protocol and
  PR #738's proposed DECISIONS 524 live-placement preview
- Related: #744, #494, #745–#748, ADR-0006, DECISIONS 527

## Problem

The UI must turn human-supplied intent into an exactly reviewed Git change.
ADR-0015 additionally chose to update the checkout where the user edited the
declarations. That choice requires a transaction across working files, the
user's index and a branch ref. Git does not provide that transaction. Preserving
editor changes and staged work then requires reversible placement, ownership
evidence, durable rollback direction and restart recovery for each resource.
PR #738's preview enters that transaction and rolls it back before confirmation,
then enters it again to commit. A new check at one boundary leaves the other
boundaries to coordinate. These costs follow from the design contract, not only
from its implementation.

## Requirements and ownership

| Requirement | Selected guarantee |
| --- | --- |
| Human rename/drop intent | Run the same executable's ordinary intent commands with the user's explicit arguments. The browser never decides identity. |
| Valid declarations and ids | Run the existing CLI against a captured candidate; no model dependency or second validator in `pbps-ui`. |
| Exactly reviewed output | Bind one immutable input manifest, resulting tree, parent, intent, commit message and destination to one server-side candidate. |
| Commit, push and MR workflow | Create a fresh named output branch, push that exact commit to one destination, then display the result and hosting link. |
| Preserve unrelated user work | Compose never writes source declarations, ids, HEAD, the source branch or the user's index, including during preview and recovery. It may add Git objects, its private records and its new output ref. |
| Synchronize the source checkout | Removed. The edited declarations and old ids remain there. The result page must explain this before confirmation and after publication. |
| Later editor changes | A preview is a frozen candidate. Later edits are excluded; Refresh creates a new candidate and requires new confirmation. |
| Concurrent Git operations | Refuse observed collisions and use a prepared create transaction. Preserve a foreign ref or unreadable state; never guess that publication failed. This is not a lock on every other checkout or editor. |
| Recover after interruption | Reconcile the operation's exact commit and output ref. There are no source-file exchanges, source-index installs or source-HEAD rollback to recover. |
| Authority and deployment | Git remains the audit trail. No browser approval store, raw SQL, DB apply, new credential store or service is introduced. |

The supported change kinds remain #494's rename, drop reason and `strategy:`
annotation. Strategy annotations already edited in declarations are captured
and validated by the ordinary CLI as part of the reviewed declaration diff;
the browser does not parse or interpret YAML. Any future form that itself
creates such an annotation must expose the same edit through the CLI first,
rather than add a second schema writer inside `pbps-ui`.

The source-preservation guarantee describes writes performed by compose; it
cannot freeze a concurrent editor or another Git command. An operation does not
claim that a filesystem-wide snapshot was captured at a single instant. It
captures a bounded set of bytes, validates them together and shows exactly those
bytes. Read errors, unstable membership or changed inputs during capture refuse
the capture. Changes after the candidate is sealed do not enter that candidate.

## Chosen flow

### Capture and review

1. Resolve the source repository, project and a direct committed base. Refuse
   unsupported layouts explicitly. The initial implementation supports ordinary
   files contained in the project, and the files ref backend on qualified Linux
   configurations. Symlinks, gitlinks, paths escaping the project, unreadable
   inputs and unqualified platforms are refusals, not omitted inputs.
2. Read the base's raw blobs without checkout/archive transforms. Use the CLI's
   `doctor --paths-only` envelope to locate declaration and identity inputs
   before any declaration/identity read. Configuration
   must match the base; the complete edited declaration membership and ids are
   captured with their relevant modes and absence information. Reconcile the
   captured ids against the base under the same CLI semantics. A private
   snapshot may not fall back to live paths or a parent checkout's discovery.
3. Run the ordinary intent command and `validate` in that isolated snapshot.
   For declaration-only intent, `plan --no-dev` resolves ids without the
   project's optional dev rehearsal; `plan --check` validates the result
   against the captured base without writes or engine access.
   Build the complete resulting Git tree with a private index; all entries
   outside the permitted declaration/identity change set remain the base's
   entries. Render the diff from that exact tree with external diff and textconv
   disabled and `--text` forced, including paths marked `-diff`. A binary-only
   summary is not a review of the declaration/ids changes.
   Never reread live bytes to construct the preview's evidence.
4. Allocate a fresh operation id and its output name
   `refs/heads/pbps-compose/<operation-id>` before sealing the candidate. This
   allocates a name, not a Git ref. Seal the candidate on the server and show
   that name with the diff. Its identity binds project/repository, operation id,
   base, complete input manifest, CLI intent, resulting tree, message, public
   signing-policy fields and one credential-free destination identity/output ref
   (defined below). Editing any bound field needs a new preview. The browser
   sends the candidate id for confirmation; it cannot provide a replacement
   manifest, tree or destination. Stale asynchronous
   responses cannot enable confirmation for a different form generation.

An input's hash is not its entire identity: relevant path membership, absence,
Git mode and configuration belong in the manifest. Resolve Git attributes and
exclude filters/external helpers from object creation. Preserve the guarded Git
runner's environment, configuration and execution restrictions; isolation does
not grant permission to execute repository-controlled hooks or filters.

### Commit and publish

1. Before confirmation can publish, prove the selected base equals an advertised
   tip of the selected remote base branch (the integration branch initially, or
   the prior output branch when continuing a result). A base with local unpublished
   ancestors is refused with an explanation to publish/review that history first.
   A narrow push refspec alone also sends its ancestors. Recheck this premise
   before publication; a changed remote base requires a new preview. This is an
   observation of the remote, not a lock preventing a later remote force-push.
2. Obtain the user's normal Git identity and signing policy through the guarded
   runner. `commit-tree` creates precisely the reviewed tree and parent; when
   signing is required it must explicitly request signing. Failure to sign
   never falls back to an unsigned commit. Persist the exact resulting commit
   id and operation intent before attempting to make a branch visible.
3. Consume the operation id and output ref already sealed and displayed by
   Capture and review step 4. Publication does not allocate another name or
   mutate the candidate. Do not overwrite or reuse an arbitrary user branch.
   Prepare a Git `create` transaction with `option no-deref`; while it holds the
   target lock, verify the live target is not a symbolic ref and is not a
   checked-out branch, including an unborn branch. Abort on a collision or unreadable check.
   An expected-zero CAS alone is insufficient: Git 2.43 overwrites a dangling
   symbolic ref in that case (retained experiment below).
4. Commit that one ref transaction. There is no source HEAD lock, worktree
   placement, user-index replacement or follow-up checkout repair. A competing
   checkout can change its own HEAD after our check; we do not hold locks on all
   worktrees. Output ids are unpredictable, and the supported concurrent-writer
   contract is ordinary Git writers honoring ref locks, not hostile processes
   deliberately targeting an in-progress operation. Detect later output-ref
   changes during reconciliation and retain the evidence instead of undoing them.
5. Push the persisted commit id to the one reviewed endpoint and output ref,
   with an expected-absent remote lease, no tags, submodule recursion or hooks.
   Do not use the subsequently read local branch tip as the push source. A failed
   or uncertain push keeps the local commit and displays its branch/id. Query
   that exact remote ref on retry: equal means delivered, absent permits retry
   of the same commit, different means collision, unreadable means unknown.
   An absent observation does not prove an earlier delivery never happened.

The selected endpoint is one credential-free literal destination, not a remote
alias that can fan out to several `pushurl`s or change the refspec. Reuse the
existing design's bounded transport and signing helper policies: no interactive
credential or pinentry prompt, finite deadline, and no automatic retry that
changes the candidate or commit. MR links are derived from the selected known hosting shape;
an unknown host still gets the exact repository, branch and commit result.

The retained Git execution safeguards are part of this contract, not optional
features of the discarded placement protocol:

- Clear inherited Git repository, index and configuration overrides, then set
  only the selected repository/private index and approved invocation settings.
  Disable replace objects, fsmonitor, hooks, external diff, textconv and filters;
  use literal pathspecs and raw objects. Reject relevant unsupported attributes
  instead of silently changing the bytes a user expects Git to record.
- Resolve exactly one push URL and admit it under the durable identity rules
  below. Refuse rewrite rules that can redirect the chosen endpoint between
  observation and push. Freeze the destination binding, not a copy of the raw
  Git configuration or authentication material. Supply the approved endpoint
  through the runner's environment-only remote. Authentication stays in the
  approved environment/helper path; it is never embedded into that endpoint.
- Rebuild display URLs from scheme, host/port and path, excluding userinfo,
  query and fragment. Keep the public SSH principal in a separate labelled field
  when it is part of the durable destination. Copyable commands use only admitted
  public identity fields. Raw authentication-helper/transport diagnostics can
  contain credentials outside a URL: do not persist or relay them merely because
  URL-shaped text was redacted. Return bounded, credential-free outcome fields
  and a generic failure when safe diagnostics cannot be established. The browser
  cannot select an executable, arbitrary Git command, transport helper or
  credential source.
- Disable terminal/askpass interaction, bound the entire subprocess group by
  a deadline, and retain the ordinary configured identity/signing and approved
  credential-helper behavior within that boundary. A timeout reports a refusal
  or uncertain publication as appropriate; it does not imply no side effect.
- Disclose beside confirmation that client hooks will not run. Server/CI policy
  remains the organization's enforcement point.

### Durable identity and transient authentication

The following are separate representations, not two serialized views of a raw
URL. A redacted display string or a hash of a secret-bearing URL is not a durable
destination binding (#750).

| Representation | Contents and lifetime |
| --- | --- |
| Destination identity | Admitted transport, exact host/port and repository path, and any explicit public SSH principal needed to identify that path; for a local Git destination, the admitted local repository identity/path. Paired with the selected remote base ref and sealed output ref. May be persisted. |
| Candidate/operation identity | The operation id, destination identity, reviewed input/tree/parent/message and public signing-policy fields. Immutable from preview through confirmation, commit creation, retry and restart. |
| Invocation authentication | Git and its approved helpers obtain credentials through the existing environment/helper/configuration boundary. The UI's runner holds only the capability to invoke that boundary. Authentication material is never serialized by compose, logged, sent to the browser, stored in a snapshot, or hashed into durable binding evidence. |

These persistence/output restrictions apply to compose-owned artifacts and
diagnostics. An existing credential helper's own configured authentication store
remains in the user's Git boundary; compose does not copy it into its records.

Admit an endpoint only if its repository identity can be represented without
authentication material. HTTP(S) endpoints containing userinfo, a query or a
fragment are refused **before sealing**, even if stripping those parts would
produce a plausible URL. They can encode both credentials and routing; dropping
them would silently choose another destination. A refusal identifies the unsafe
component category without echoing its contents, and explains how to configure
a credential-free endpoint plus an approved credential helper/environment.
The existing CLI remains available for unsupported endpoint forms.

For supported SSH forms, an explicit public account name is routing identity,
not the private key or passphrase. Preserve that principal as a bound field;
changing it may select a different repository for a relative scp-like path.
An omitted/implicit principal or an opaque transport whose effective destination
cannot be established is refused until that form has a qualified identity
protocol. Never infer equality by comparing two redacted strings. Normal Git
transport and host-authentication policy remain required; this identity is not
a new remote-server attestation scheme. The underlying URL forms are described
in [Git's push documentation](https://git-scm.com/docs/git-push); compose's
admission rules above are deliberately narrower than all forms Git can parse.

On restart/retry, have Git reacquire authentication from the currently approved
source and independently resolve the endpoint again. Compare all admitted identity
fields and refs with the sealed values before contacting it for publication.
Missing authentication is an actionable failure with the local result retained.
Refreshed authentication for the same bound destination can proceed under the
ordinary state/retry rules; destination or ref drift requires a new candidate
and review. Do not persist a raw config dump, helper command, environment,
askpass response or secret-bearing URL to make retries reproducible. Durable
signing policy contains only public requirements/selectors, not signing secrets
or arbitrary helper configuration.

| Input or event | Required behavior |
| --- | --- |
| HTTPS URL with userinfo | Refuse the endpoint without copying the userinfo into a candidate, receipt, log or HTTP response. |
| HTTPS URL with a query token | Refuse the query form; do not remove the query and claim the resulting repository was reviewed. |
| Explicit `git@host:team/repository.git` | Bind the public `git` principal, host, exact repository path and refs. Key/passphrase material stays transient. |
| Credential-free HTTPS plus a credential helper | Persist only the admitted destination fields; obtain authentication for each invocation through the approved helper. |
| Restart with missing/expired authentication | Preserve the receipt and local commit; report an authentication failure without changing the destination or regenerating a commit. |
| Authentication refreshed for the same identity | Reuse the same operation and recorded commit where the publication state permits retry; do not regenerate a branch name. |
| Changed host, port, path, public SSH principal or ref | Refuse reuse of the candidate, even if a display label or remote alias stayed the same. |
| Unknown or unrepresentable endpoint | Named refusal; no guessed normalization or durable raw-URL fallback. |

The sequence is therefore: allocate identity/name, seal and show it, confirm that
candidate, create its exact commit, publish its already named ref, and reconcile
that same operation on retry/restart. Refresh creates a new candidate identity
and requires a new confirmation. An old confirmation never targets the refreshed
candidate. The concrete publication/retirement states remain #746/#747.

#746/#747 must exercise fake credential markers in userinfo, query tokens and
helper output and assert they never appear in durable files, logs or HTTP
responses. Endpoint-drift refusal and authentication refresh for the same
identity need positive/negative controls. These are production implementation
requirements, not properties demonstrated by the local bare-remote spike.

## State and retirement

The live error path and startup recovery use one reconciler and these outcomes:

| State | Permitted next action |
| --- | --- |
| Prepared, publication not attempted | Confirm, refresh or discard private candidate; no source restoration. |
| Publication may have happened | Inspect exact output ref and recorded commit. No rollback, automatic second commit or push until classified. |
| Locally published | Report the existing commit; push/reconcile the same commit. Never remove it because push failed. |
| Needs recovery | Preserve record and objects, name the failed/unreadable resource, permit explicit retry after diagnosis. |
| Complete | Retire transient snapshots only after durable outcome evidence and all required cleanup succeed. |

Persist publication intent before the ref attempt. A missing acknowledgment,
failed persistence after the write, or interrupted subprocess is an uncertain
outcome, not evidence of an absent commit. If a ref disappears after possible
publication, retain uncertainty; an automatic recreation could undo somebody's
deliberate deletion. Git's ref is authoritative about its present value, and the
record explains the operation that may have created it. Startup does not repeat
the intent CLI or regenerate signed commits from a timestamped recipe.

Records are operational evidence, not schema truth or approval. Keep a compact
receipt of each published operation (base/input identity, commit, output ref,
credential-free destination identity and result) outside browser storage. A
receipt is never a copy of the raw endpoint/authentication configuration.
On restart the UI lists these results and pending recovery before accepting another confirmation. Repeated
confirmation/retry of the same operation returns the existing outcome. A new
launch token grants access to this local evidence without reviving an old
browser token.

Unconfirmed previews expire after 24 hours; expiry requires another preview.
Keep at most one unconfirmed candidate per browser workflow and bound candidate
bytes/count at creation. Replacing it retires only its exclusively owned private
snapshot. An in-progress or uncertain operation never expires automatically.
Completed snapshots may be removed after the receipt is durable; output branches
and compact receipts are never automatically deleted. The results view provides
an explicit forget action for completed receipts, explaining that it leaves the
Git branch intact. Refusal and cleanup failure must remain distinguishable from
success. #747 defines the precise durable record/cleanup implementation.

## Continued editing

The confirmation page says: "This publishes the reviewed snapshot on a new
branch. Your current checkout will keep its existing edits and ids. Later
edits are not included." The result lists source base, resulting commit,
local branch, remote outcome and the next action. A push failure still shows
the successfully created local result.

For this iteration, continuation uses a **separate checkout of the result
branch**, chosen by the user in their Git client. The page also offers exact
copyable commands for `git worktree add <new-path> <result-branch>` and
`pbps --project <new-project-path> ui`, with safe quoting. It explains that this
checkout contains the renamed ids and reviewed declarations together. These
commands are user-executed; automatic workspace creation is separate scope.
There is no automatic copy-back, source checkout switch, reset or clean.

The old source may still be used to inspect or revise uncommitted work, but
after a result is published it is not silently treated as the result's next
base. Redisplaying/confirming that same operation returns its result. Composing
again from the old source requires an explicit "Start an alternative from this
base" action and a new preview; it creates a sibling proposal. To extend the
published result, use the new checkout and finish/retry its predecessor's push
first so the remote-base premise can be proven. This is a deliberate usability
cost, not a hidden promise to synchronize the old ids later.

## Alternatives and implementation boundary

Snapshot-only preview with same-checkout commit removes one placement cycle but
keeps the actual multi-resource publication and recovery protocol. Keeping the
old behavior with typed states improves correctness but still incurs that cost.
A clean user index, `git add/commit`, or copying results back from a worktree
does not remove it. A patch export alone does not satisfy the integrated
commit/push/MR requirement. The selected design pays for an explicit continuation
step in exchange for removing the source mutation protocol.

#744 changes the contract and retains feasibility evidence; it does not enable a
write endpoint. Delivery remains #494, coordinated with PR #738's owner:

| Issue | Selected scope |
| --- | --- |
| #750 | Clarify immutable operation/destination identity, credential-free persistence and the orchestration responsibility boundary before production implementation. |
| #745 | Immutable isolated capture/tree/diff, guarded subprocess reuse, browser candidate generation and exact-output tests. Preview never invokes live placement. |
| #746 | Single fresh-branch publication, explicit uncertain/published outcomes, receipts, retry and shared restart reconciler. No source-index or source-HEAD transaction. |
| #747 | Durable private-record/resource ownership and retirement; remove live-placement recovery paths and safely identify legacy experimental records. |
| #748 | Deterministic subprocess interruption/fault seams introduced alongside the operations above, then the full restart and browser qualification matrix. |

Work proceeds sequentially after the #750 contract clarification: #745, #746,
#747, then #748.
Each implementation introduces the minimal controllable boundaries and property
tests it needs; #748 completes the cross-boundary qualification. Do not build a
large test harness for a protocol that is being deleted. The feature remains
disabled until all delivery acceptance criteria pass. A preview API alone is
not permission to expose a partially qualified commit/push workflow.

The old protocol is not in the baseline production tree (`9c487db0`). If users
have run experimental #738 binaries, a new binary must recognize the old record
namespace/version and refuse compose with the location and recovery instructions.
It must not treat old records as empty, migrate them by guessing, or remove
their retained files. Recovery with the matching experimental binary or an
explicitly reviewed migration is required first. #747 pins this behavior and
proves no new call path reaches the old placement/index recovery code. #471's
old Windows restoration work is not silently closed: the selected architecture
removes that particular requirement, but Windows compose stays disabled until
its new filesystem/ref/durability behavior has its own qualification.

## Evidence and remaining qualification

The #745 candidate service and shipped form module are covered by
`crates/pbps-cli/tests/compose_candidate.rs` (real Git and CLI) and
`crates/pbps-ui/tests/browser.rs` (executes the shipped JavaScript with
controlled DOM events and asynchronous responses). The viewer serves the
module but does not mount it or expose write endpoints before #746–#748.

Initial capture uses Linux `openat2` no-follow reads, regular UTF-8 paths and
the files ref backend. A selected project must have committed configuration;
declaration and identity paths must be distinct and contained, and the
declaration directory cannot be the project root. The latter does not redefine
the loader policy tracked by #739. Filter, ident and encoding attributes,
uncommitted relevant attributes, conversion-dependent CR input, ignored
untracked declarations and special index flags refuse capture. Force-added
declarations retain Git's tracked-file admission, while their captured working
bytes, rather than staged content, enter the candidate. Git mode is the observed
executable bit even when a user's ordinary Git configuration ignores modes.
Relevant attribute files are pinned as raw bytes; Git's resolved line-ending
policy is queried using a private index and an empty worktree, without filters
or live declaration reads. Source capture budgets include those attribute files.
Captured bytes are refused only when Git's raw and normalized object identities
differ in a sterile private probe with the resolved text policy; CR-only input
and non-converting `text=auto` content remain supported.
File/directory replacements use compatible private attribute indexes for the
old and new path shapes. The snapshot and candidate index remove admitted absent
blobs before adding replacements. Empty private directories may be removed,
but a replacement that would erase unrelated base content refuses.
The CLI remains available for unsupported capture layouts.

Each workflow retains one unconfirmed candidate for at most 24 hours. Source
capture and the base project snapshot each have a 64 MiB budget, subprocess
output has a 64 MiB limit per stream, and traversal is bounded by 4096 entries
and 64 directory levels. The base tree outside the project is reused without
materialization; its complete private index is also limited to 4096 entries.
Confirmed candidates cannot be refreshed or expired by the
preview service; publication/receipt retirement belongs to #746/#747.
The private object view currently borrows base objects through an alternate;
#747 must establish durable GC reachability before enabling operations.

[`spikes/git-compose-isolated`](../spikes/git-compose-isolated/README.md) runs
real Git and the baseline CLI against local disposable repositories. It checks
unrelated staged/untracked work, frozen preview output after source edits,
direct/dangling-symbolic/unborn-checked-out destination collisions, unpublished
ancestry, explicit signing failure, failed push, discarded push acknowledgment
and continuation in a separate checkout. Reverted controls demonstrate that
the ref-type check and frozen candidate assertions detect their counterexamples.

This is feasibility evidence, not shipped recovery or a power-loss test. It
does not qualify production path handling, manifests, browser behavior, signing
success, remote transport, record durability, hostile filesystem races, macOS,
Windows or another Git ref backend. Those belong to #745–#748. The older
[`git-compose-refs`](../spikes/git-compose-refs/README.md) remains historical
evidence for the superseded same-checkout design, not a new delivery obligation.

Git's documented primitives are
[`commit-tree`](https://git-scm.com/docs/git-commit-tree) (an object with a chosen
tree and parent) and [`update-ref`](https://git-scm.com/docs/git-update-ref)
(ref transactions). Neither provides a transaction over a user's worktree,
index and refs.
