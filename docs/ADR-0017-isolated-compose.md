# ADR-0017: Compose an isolated candidate and publish a new branch

- Status: candidate and publication backends implemented; HTTP writes remain disabled
- Date: 2026-09-21
- Supersedes: ADR-0015 decision 5's same-checkout publication protocol and
  PR #738's proposed DECISIONS 524 live-placement preview
- Related: #744, #494, #745–#748, ADR-0006, DECISIONS 527, 541

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
   signing is required it must explicitly request signing with the sealed
   public key and format (an unset format is pinned to Git's default), so a
   configuration change after the policy check cannot choose the signature
   (#770). Failure to sign never falls back to an unsigned commit. Persist the
   exact resulting commit id and operation intent before attempting to make a
   branch visible.
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
   Do not use the subsequently read local branch tip as the push source. The
   configured `push.gpgSign` mode is read once and passed to that push
   explicitly. When it requires a certificate, a dry-run preflight refuses a
   destination that cannot accept one before the remote attempt is recorded,
   so the local commit remains and ordinary retry proceeds once the capability
   or policy is repaired. The requirement is never dropped (#775). A dry run
   cannot exercise the signer, so a signer failure is an uncertain push. A failed
   or uncertain push keeps the local commit and displays its branch/id. Query
   that exact remote ref on retry: equal means delivered, different means
   collision, and unreadable means unknown. Absence after an authorized attempt
   remains unknown: the push may have succeeded before an independent deletion.
   Ordinary retry never recreates that ref. An informed explicit republish
   action can authorize the same commit/ref/destination after diagnosis. Its
   displayed generation is consumed durably before observation or push, even
   when the action finds the commit already present; replaying the same request
   cannot become fresh authorization after a later deletion.

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
  through the runner's environment-only remote. Disable HTTP(S) redirects for
  observations and pushes, including inherited URL-scoped `followRedirects`
  overrides: the configured identity does not bind a redirected repository.
  Clear inherited `push.pushOption` values for each invocation: server-specific
  actions such as CI or merge-request controls are not part of the sealed change.
  Authentication stays in the approved environment/helper path; it is never
  embedded into that endpoint.
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

### Remote branch semantics

HTTP/SSH publication uses the ordinary Git server contract: the selected base
and output branch names denote direct branches, and the server does not remap
writes to another ref through symbolic branches, `proc-receive` or equivalent
administrative configuration. The user explicitly selected this boundary for
#746 / PR #768 after reviewing the alternative of refusing network push until
a separate inspection or no-deref server capability exists. The contract is
recorded in DECISIONS 534.

Git advertisements and expected-value leases do not prove this server premise.
A real Git 2.43 local-bare and smart-HTTP experiment created a dangling symbolic
output ref: `ls-remote --symref --refs` advertised nothing, but an expected-absent
push created its unreviewed target and left the output ref symbolic. `--atomic`
behaved the same way. The ordinary receive-pack update does not request
[`REF_NO_DEREF`](https://github.com/git/git/blob/v2.43.0/builtin/receive-pack.c#L1516-L1520).
An empty advertisement alone is therefore not a universal proof of absence.

For network destinations, symbolic output branches and server-side ref remapping
are unsupported server configurations. This is an operator/server prerequisite,
not a promise that the client can discover and reject every violation. A server
violating it can still redirect a write; a post-push observation cannot undo or
justify that write. The form discloses this limitation before confirmation.
No server attestation service, remote command executor or credential store is
added. Visible symbolic/conflicting/unreadable advertisements still refuse,
and endpoint binding, disabled HTTP redirects, exact-commit pushes and uncertain
outcome rules remain required.

Local filesystem destinations provide an additional capability: inspect their
raw Git ref evidence, including dangling symrefs, compare it with the advertised
value, and require direct absence again immediately before push. Refuse symbolic,
unreadable or inconsistent evidence while retaining the exact local commit.
This checks observed local collisions under the ordinary-writer boundary above;
it does not freeze an administrator changing the remote during publication.
The publisher never removes or rewrites a conflicting remote ref to make it fit.

### Durable identity and transient authentication

The following are separate representations, not two serialized views of a raw
URL. A redacted display string or a hash of a secret-bearing URL is not a durable
destination binding (#750). DECISIONS 541 records this policy
and its trade-offs.

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
candidate. The publication states below are implemented by #746; #747 qualifies resource durability and retirement.

#746/#747 must exercise fake credential markers in userinfo, query tokens and
helper output and assert they never appear in durable files, logs or HTTP
responses. Endpoint-drift refusal and authentication refresh for the same
identity need positive/negative controls. These are production implementation
requirements, not properties demonstrated by the local bare-remote spike.

## State and retirement

The live error path and startup recovery use one reconciler and these outcomes:

| Durable state | Permitted external mutation | Reconciliation and failure outcome |
| --- | --- | --- |
| No receipt | Import the reviewed raw tree objects; persist `Preparing` before one `commit-tree` invocation. | Failed admission cannot publish a ref. |
| `Preparing` | Only the current authorized invocation may construct the commit. | Missing acknowledgment stays preparation-unknown; restart cannot regenerate it. |
| `CommitKnown(commit)` | Only the current invocation may acquire and acknowledge its private exact-commit root. | An interrupted root acquisition preserves its record and any ref; restart cannot infer ownership from a matching OID or regenerate the commit. |
| `Prepared(commit)` | Prepare a no-deref create transaction, inspect collisions/checkouts while holding its lock, then persist `LocalAttempt`. | Failed prerequisite persistence aborts the transaction. The known commit remains available. |
| `LocalAttempt(commit)` | Only the already-owned live transaction may send commit. | Exact direct ref means published; absent means unknown; symbolic, changed or unreadable evidence requires recovery. Restart never recreates the ref. |
| `LocalPublished(commit, Unattempted)` | Revalidate endpoint/base and output ref; persist a remote attempt before a bounded expected-absent push. | Definite pre-attempt authentication/observation failure preserves the local result and permits ordinary retry. |
| `LocalPublished(commit, Attempted(generation))` | No automatic replay. An explicit action naming this generation first persists a fresh generation, then may push the same commit. | Exact remote commit means delivered; absence remains uncertain; other/unreadable evidence remains reported. |
| `LocalPublished(commit, Delivered(generation))` | Read/reconcile; an explicit informed republish can consume the displayed generation. | Later deletion or replacement is retained as changed evidence, never undone automatically. |
| Private cleanup pending | Only #747's owned-resource retirement operations. | Publication and cleanup are separate facts; cleanup failure cannot erase a known commit or permit source restoration. |

The stable serialized result carries an outcome discriminator, local and remote
observations, exact operation/commit/tree/ref/destination details, a categorical
problem and pending-cleanup status. It never returns an outer error authorizing
rollback. Live errors, repeated confirmation, ordinary retry and restart use one
reconciler. A durable receipt with an unknown attempt restricts retry even if the
current ref is absent; a failed write may already have installed that receipt.
Unknown, absent and unreadable evidence remain distinct. Remote observations over HTTP/SSH have the server-semantic limit above.
No source restoration or output-ref deletion operation exists in this publisher.

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
success. The resource lifecycle below implements that contract (#747).

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
module but does not mount it or expose write endpoints before #747–#748.

#746 adds the repository-scoped publication service, one prepared Git ref
transaction, durable authorization receipts and shared live/restart result
classification. `compose_publication` drives actual local and smart-HTTP pushes,
discarded acknowledgments followed by independent remote deletion, explicit
republish replay, source preservation, signing, identity drift and persistence
fault boundaries. The shipped result component renders those serialized facts,
retains known commit details after a failed read, discovers saved operations and
separates continuation from an explicitly authorized old-base alternative.
The alternative also pins that base through its next preview.

Resource durability and retirement are implemented by #747 and DECISIONS 535.
The private common-directory namespace is `pbps-compose-v2`; publication receipts,
resource records and snapshots are separate. Discovery validates every record,
then lists only the current source worktree's receipts and resources. Known
records for another source sharing the common directory are retained and excluded
from that view; unknown versions, malformed records and a changed identity at the
same source path still refuse. Per-operation reads and mutations keep the stricter
source identity check, so filtering never authorizes another worktree's cleanup.
Resource and receipt discovery batch validated names and record reads (#784).
Resource discovery and the private-pin admission census bind repository identity
before and after the batch; complete namespace passes bracket each batch, and
resource discovery also rechecks snapshot membership. A missing, unknown,
unreadable or changing entry refuses the batch. Pin-owning records read during
the census are reused within resource discovery, then discarded with the batch.
The cache never authorizes a later operation: direct reads, mutations and receipt
reconciliation retain their individual authority checks. The bounded-pass claim
covers discovery enumeration and admission census, not per-receipt recovery or
per-pin Git verification. Under the resource coordination lease, admission
validates every prior resource record before creating a new record, snapshot or
pin (#785), reusing pin-owner reads from the census. Unknown versions, malformed,
unreadable or nonregular evidence refuse with a named retained-recovery
obligation. Healthy other-source records are validated against the common store;
they grant no current-source cleanup authority. Direct preview admission enforces
this condition even when discovery was never requested.

Each record read binds its bytes to metadata from the same opened, owned regular
file (#799): inode identity, length, modification time and change time. It also
retains a SHA-256 content digest: rapid equal-length overwrites can leave all
those metadata fields unchanged. Recheck the descriptor and verify the named
file's metadata and contents before returning the read. Resource discovery,
admission and receipt discovery retain these compact revisions, including census
owner reads, and compare them again at the closing batch boundary. Detected
replacement or in-place change refuses without granting new acquisition or
cleanup authority. Each JSON record is decoded once, while revision verification
streams its bytes again with an observed-length bound and a fixed-size buffer.
That extra content I/O buys detection of same-timestamp overwrites; record work
remains linear with bounded directory and repository discovery passes. Revisions
are invocation-local and grant no later authority. Admission's existing lease
serializes cooperating resource writers; checks cannot exclude an arbitrary
external write after the last observation. This is not a filesystem transaction
or a stronger guarantee for writers that bypass compose coordination.

Discovery and new-resource admission also census `refs/pbps-compose` (#783).
Read both loose and packed evidence without following symlinks and corroborate
it with Git's direct-ref enumeration. Physical names and byte fingerprints prove
census coverage and stability; Git decodes the ref values, including its accepted
uppercase OIDs and loose-ref whitespace. Only its canonical direct-ref values are
compared with recorded ownership. Git-owned ref files use bounded regular-file
reads without private-record UID or link-count restrictions, permitting shared
repository metadata; private resource records keep their stricter ownership
policy. Repeat the physical census to refuse a
changing view, including refs moving between packed and loose storage. Each
surviving pin requires a valid resource record for the same common directory
and its recorded, non-retired value before source-worktree filtering. Missing,
malformed, symbolic, unreadable or incomplete evidence refuses with a recovery
location. Correlation never acknowledges an interrupted acquisition, deletes an
orphan, or reconstructs a candidate. Empty operation directories left by Git
are harmless; unknown namespace shapes are not. The packed table uses the
existing 64 MiB evidence bound, and the census admits at most two pins per
32,768 resource records; exceeding a bound refuses rather than truncates.

The retained `owner.lock` inode
serializes publishers. A short `resources.lock` lease serializes resource
transitions, so capture does not require closing the publisher. Explicit checked
unlock avoids extending a lease through another thread's forked open description;
destructor unlock is fallback only and never certifies completed cleanup.

| Resource state | Durable evidence and permitted retirement |
| --- | --- |
| `Capturing` | Acquisition intent precedes snapshot creation and each Git pin. Only acknowledged inode/ref ownership is recorded as owned. An interrupted unsealed capture remains a named manual-recovery obligation because surviving children and unacknowledged entries cannot be inferred away. |
| `Sealed` | The complete credential-free binding manifest, private tree root and flushed inventory are acknowledged. Explicit discard, replacement, or 24-hour expiry may retire this unconfirmed preview. |
| `Confirmed` | Publication may proceed only with the same manifest binding and owned base/exact-commit roots. Neither expiry nor preview discard applies. |
| `Retiring` | Retirement intent and its keep-commit choice are durable before deleting anything. Restart resumes that choice, preserving changed, unknown, foreign and unreadable evidence. |
| `Retained` | A completed snapshot and base pin are retired; the compact receipt and exact-commit root remain available. Explicit forget can retire the receipt and that root. |
| `Spent` | A compact tombstone revokes all old candidate handles. Public output branches remain untouched. |

Every read, admission and write validates a record against the transitions that
write it, not only against its own state's fields (#802). Retiring the snapshot
clears its identity and inventory in the same write. A pin is retired only
under `Retiring`, after the snapshot, and the base pin before the commit pin.
The keep-commit choice exists only from `Retiring` onward. A record combining
these any other way cannot come from an interruption. Admission, discovery
and recovery refuse it and nothing consumes it. An unacknowledged base or commit
intent is the interruption the table above describes, and stays admissible.

Before an alternate borrows source objects, an operation-specific private annotated
Git tag roots the base. If the source itself uses alternates or promisor packs,
import the pinned commit's full reachable object closure into its local object
store and flush it before acknowledging the pin. Donor GC cannot see a borrower's
refs. This applies to both base and exact-commit roots; failed import or flush
leaves acquisition unacknowledged. The existing subprocess size and deadline
bounds apply to the closure import, so a large borrowed history can refuse even
when its project snapshot fits the capture limit. Ordinary self-contained stores
do not need this extra pack import. Sealing roots the private candidate tree.
After the one
commit invocation acknowledges its OID, `CommitKnown` records it before acquiring
its exact-commit root; only acknowledged root ownership permits `Prepared`.
If the root acknowledgment is durable but the receipt is still `CommitKnown`,
the shared reconciler verifies the matching owned root and candidate binding,
then persists `Prepared`. Ordinary retry can continue that exact commit without
invoking commit creation again. Missing or unacknowledged pins remain uncertain;
conflicting evidence or a failed receipt transition preserves a pending result.
Pin creation is a fresh no-deref transaction. Pin deletion checks the recorded
value and direct-ref type under Git's transaction lock; a matching value without
acknowledged acquisition is never cleanup permission. Pending and retained roots
survive forced source GC. Packed refs remain valid restart evidence.

Record operations are descriptor-relative, no-follow, bounded regular-file
operations. Replacement checks the prior inode and revision, writes a fresh
private temporary file, flushes it, renames and flushes the containing directory.
Unknown temporary names are retained as manual-recovery evidence, not swept by
name. Snapshot retirement checks its acknowledged inventory, file identities and
file revisions; added or changed entries block completion. Missing entries can
discharge only an already durable retirement. Git object/ref writes request fsync,
and affected file/directory entries are explicitly flushed before acknowledgment.
Private inventory and manifest records are bounded to 64 MiB, 32,768 entries and
80 directory levels; at most 32,768 operation resource records are admitted.
Spent tombstones count toward that bound and are not automatically pruned.

A sealed capture rejected by the alternative-base check is retired through the
same authorized preview-discard flow before its handle is released (#782).
It keeps the ordinary spent tombstone. Failed retirement reports both the base
mismatch and the pending cleanup, preserving its record and any foreign entries
for discovery and restart; confirmed and unknown operations remain ineligible.

Definite capture rejection follows DECISIONS 536 (#780). Bound the base tree and
blobs and check committed configuration before acquiring private resources.
Checks that require the private snapshot first capture a flushed inventory of
the paths produced by completed fixed preparation steps. Unknown files or locks
cannot enter that inventory. A normal CLI refusal or an explicit input-bound/path
refusal can enter durable `Retiring` with a completed-rejection marker and that
pre-check inventory. Only the CLI's defined failure codes 1 and 2 count as normal
refusals; panic/unrecognized exit codes retain the acquisition. Signal termination, failed pipe exchange, timeout, unreadable
evidence and failed acquisition/flush do not authorize this transition. Changed
files or new temporary entries after the checkpoint keep retirement pending;
retirement never adopts a fresh post-error inventory.

The checked input admission also bounds the base/live path union, identity and
configuration bytes, and the combined declaration and attribute evidence (#789).
Discover and size recorded attribute blobs before building private attribute
indexes; `cat-file --batch-check` also admits the complete body stream against
the existing subprocess output limit before requesting bodies. Live attribute
bytes and the ancestor attribute-path count have the same checked admission.
The admitted input bytes become the manifest and snapshot overlay, and the final
capture check rereads the source evidence. This adds no cleanup authority for
an interrupted child, unreadable evidence or an arbitrary later private write.

A rejected capture has never exposed a candidate handle. After all its snapshot
entries and base pin are durably retired, remove its resource record; it does
not consume a permanent spent-identity slot. The durable rejection marker is
valid only for retirement without a commit or reviewed binding. Recovery resumes
that same retirement and reports completion only after actually discharging it;
an arbitrary absent record is not a successful-recovery witness. Existing sealed,
confirmed and forgotten handles retain the ordinary tombstone rules. The bounded
pre-check inventories add filesystem reads and flushes to successful capture;
this cost buys rejection cleanup without a second acquisition/rollback protocol.

The production observer seam exposes operation-before/after boundaries without
making hooks selectable through HTTP or persisted input. Tests inject I/O failure,
kill actual processes, restart retirement twice, retain foreign locks/files, and
exercise ordinary source `git add` after interruption. These checks do not prove
arbitrary power-loss behavior on every filesystem or protection against an
unrestricted malicious same-user process during private Git/CLI execution.
Integrated #494 still gates enabling writes and mounting recovery actions.

#### Acceptance coverage (#748)

Each family of #748's acceptance matrix maps to the named cases below. Paths are relative to `crates/pbps-cli/tests/` unless marked `ui:`
(`crates/pbps-ui/tests/`). "Killed" means an actual child process stopped by
SIGKILL at a named `PublicationBoundary` or resource operation, with core
dumps disabled and asserted off inside the child; "injected" means the
production observer refused an operation, which exercises the same
reconciliation path but is not process death.

| Family | Executed cases | Kind |
| --- | --- | --- |
| 1. Capture versus editor saves, stale form responses, new/deleted/mode-only inputs | `compose_candidate.rs`: `edits_during_capture_are_refused_and_never_become_post_diff_evidence`, `a_new_path_or_mode_change_at_the_capture_barrier_is_not_omitted`, `new_deleted_recreated_paths_and_modes_are_part_of_the_candidate`, `captured_executable_modes_match_real_git_including_group_only_execute`, `index_flags_changed_during_capture_refuse_without_touching_the_writers_index_or_lock`, `refresh_replaces_the_handle_and_expiry_requires_another_preview`; `ui:browser.rs`: `compose_form_keeps_confirmation_bound_to_the_latest_reviewed_generation` | Barrier-synchronized edits; shipped script under out-of-order responses and repeated clicks |
| 2. Resource and pin acquisition before ownership, record replacement, object pinning, preparation and cleanup failures | `compose_publication/resources.rs`: `actual_process_death_preserves_acquisition_ref_handoff_and_retirement_evidence`, `failed_borrowed_object_flush_never_acknowledges_a_base_pin`, `every_receipt_and_retirement_io_failure_preserves_unresolved_evidence`, `replacing_a_receipt_or_lock_inode_cannot_transfer_ownership`, `forced_source_gc_preserves_the_frozen_base_and_exact_unpublished_commit`, `borrowed_and_promised_objects_survive_donor_gc_after_pin_acknowledgment`, `known_commit_recovery_requires_acknowledged_matching_resources_and_durable_transition`, `interrupted_children_and_unknown_or_changed_snapshot_entries_remain_retained`; `compose_publication/resources/lifecycle.rs`: `every_state_refuses_pin_and_keep_commit_combinations_no_transition_writes` | Killed; injected; synthetic states labelled as such |
| 3. Lost acknowledgements, local result surviving a failed or unknown push, no second commit | `compose_publication/process_death.rs`: `death_after_commit_creation_never_yields_a_second_commit`, `death_after_the_local_ref_is_installed_resumes_that_exact_commit`, `death_after_the_remote_attempt_intent_needs_informed_republish`, `death_after_the_push_finds_the_delivered_commit_without_pushing_again`; `compose_publication/mod.rs`: `lost_local_acknowledgement_and_post_write_record_failure_share_recovery`, `missing_commit_acknowledgement_never_generates_a_second_commit`, `unknown_push_then_independent_deletion_requires_fresh_informed_authorization`, `an_offline_destination_is_unavailable_and_restoration_reuses_the_known_commit`; `resources.rs`: `acknowledged_commit_roots_resume_without_another_commit_invocation` | Killed; injected |
| 4. Removed placement, index, HEAD-repair and undo states unreachable; legacy and unknown records refused unchanged | `compose_publication/removed_states.rs`: `receipts_in_removed_or_unknown_states_refuse_without_mutation`; `resources.rs`: `legacy_and_unknown_evidence_refuse_without_modifying_retained_data`, `unknown_resource_versions_and_impossible_states_preserve_all_evidence`; `compose_publication/resources/admission.rs`: `direct_admission_preserves_bad_prior_evidence_without_allocating_resources` | Receipt phase type has no removed variant; synthetic records derived from actual ones |
| 5. Recovery boundaries, conflicting third-party refs and locks, unreadable evidence, repeated recovery | `process_death.rs`: `recovery_killed_before_its_own_receipt_write_restarts_to_the_same_result`; `resources.rs`: `interrupted_retirement_restarts_without_erasing_foreign_locks_or_receipts`, `rejection_retirement_restarts_after_durable_transition_and_file_removal_failures`, `foreign_pin_locks_and_nonregular_receipts_are_never_cleared_or_waited_on`; `mod.rs`: `an_unreadable_prepared_ref_remains_unavailable_and_never_claims_a_known_change`, `deleted_local_publication_and_unreadable_receipts_are_never_empty_evidence`, `late_dangling_symbolic_collision_is_rejected_under_the_prepared_ref_lock` | Killed (recovery itself); injected; concurrent Git writer at an exact boundary |
| 6. Output-branch obligations: source invariance, collisions, unpublished ancestry, output ref found after a lost acknowledgement, expiry and cleanup | `compose_candidate.rs`: `confirmation_keeps_the_reviewed_tree_and_preserves_source_and_staged_work`; `mod.rs`: `direct_symbolic_and_unborn_checked_out_collisions_preserve_the_source`, `a_refused_collision_reports_its_actual_direct_symbolic_or_unreadable_evidence`, `unpublished_ancestry_and_required_signer_failure_refuse_without_unsigned_fallback`; `process_death.rs`: `death_after_the_push_finds_the_delivered_commit_without_pushing_again`; `resources.rs`: `only_unconfirmed_sealed_previews_expire_or_allow_explicit_discard_after_restart`, `cleanup_retains_the_receipt_root_and_forgetting_revokes_every_old_handle` | Killed; injected; real Git collisions |

Each kill regression was checked against a restored defect: disabling the
recovery transitions from `LocalAttempt` to `LocalPublished` and from
`Attempted` to `Delivered`, or letting ordinary retry replay an authorized
push, fails the corresponding `process_death.rs` cases, as does recovery that
runs `commit-tree` or `git push` again: a changed committer identity makes a
repeated commit a new object, and the loopback server logs every push request,
including one with nothing to update. Accepting any receipt
version fails `removed_states.rs`; and a child launched without the core-limit
wrapper under an unlimited parent limit fails its in-child assertion.

The child harness is the test binary re-executing one `#[ignore]`d test, not
the `examples/compose-interrupted.rs` program of the superseded #738 branch.
Remaining limits, stated precisely: SIGKILL and injected refusals do not
exercise a kernel or filesystem that loses acknowledged writes, so power-loss
durability rests on the explicit `fsync` ordering above, not on a test. Only
Linux with Git's files ref backend is qualified; reftable, macOS and Windows
(#471) are not. Remote transport is qualified against local and loopback
smart-HTTP destinations served by `git http-backend`, not TLS, proxies or
hosting services. The browser family runs the shipped script against a Node
DOM harness, not a browser engine. A concurrent same-user process that
bypasses compose coordination is outside the guarantee, as stated above.
Legacy `pbps-ui/composing` evidence refuses new compose with its location and
matching-binary/manual recovery instructions; the new publisher never runs the
old source-restoration algorithm.

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
The private object view borrows base objects through an alternate only after
acknowledging its durable base root; #747 qualifies forced-GC reachability.

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
