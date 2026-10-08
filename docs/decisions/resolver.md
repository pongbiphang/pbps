# The resolver

Engine-assisted planning: resolver profiles, container admission, process and
socket observation. Part of the [decision record](../DECISIONS.md), which says
how to add an entry here.

<a id="decision-490"></a>

490. **Engine-assisted planning is optional infrastructure, but required
     evidence cannot be waived (accepted design; not implemented).**
     [SPEC §9.3.2–9.3.3](../SPEC.md#932-engine-assisted-planning-accepted-not-implemented)
     and [ADR-0016](../ADR-0016-engine-assisted-planning.md) keep offline previews
     and the existing `--dev` rehearsal distinct from a future target-aware
     `--resolve-with` path. Typed/catalog facts retain a lightweight path;
     uncertain covered binding questions require an isolated engine before an
     applyable artifact can be produced. Conservative extra resolver requests
     are accepted instead of growing a SQL-semantic parser or treating every
     candidate as a proven reason to rebuild.

     Current bindings come from the target; desired bindings come from compiling
     the desired namespace with its relevant external prerequisites. Old-YAML
     bootstrap is not the current baseline, CREATE success is not complete
     dependency proof, and empty catalogs are not proof of no dependencies.
     Binding-derived edges order typed changes across object kinds: teardown
     precedes removal of old inputs and desired inputs precede dependent rebuilds,
     including routine-driven default/CHECK/index changes. Combine existing
     structural/data/authorization constraints, refuse unsupported cycles before
     publication, and seal the final order without hidden SQL. Apply and emitters
     preserve that order; no runtime dependency choice or ordinary-plan reorder
     is introduced.
     Runtime/dynamic SQL retains its existing limits. All scratch DDL stays
     outside the target instance/cluster, including when another database or
     connection alias is supplied. Prove separation before scratch DDL or source
     transfer with qualified read-only identity/provisioning evidence; unknown
     identity refuses and reconnects/failovers requalify. No production rows or
     target authentication material are copied.

     DDL can execute expressions or extension code, so every resolver workload
     also needs qualified execution containment before declaration transfer or
     compilation. Deny outbound/external side effects and host-resource access
     outside the run, with controls enforced outside SQL privileges and bounded
     resources/lifetime. Trusted image acquisition is separate. Unknown controls
     refuse; violations abort without evidence, stubs or broader-access retries.
     This qualifies the planned resolver lifecycle, not a new sandbox service.

     Environment discovery, trusted candidate suggestions and actual
     compatibility checks cover both PostgreSQL and SQL Server first. A Docker
     tag is a candidate, not verified compatibility; acquisition is explicit,
     lazy, policy-bound and usable with local images/internal registries.
     Reported versions are not executable identity: qualify actual engine and
     relevant loaded/required native-library provenance and content on both
     sides. Only matching content or a measured, versioned mapping of identified
     builds establishes compatibility; unknown provenance or same-version local
     patches cannot pass by catalog equality. Seal these prerequisites and
     validate them through capture, reconnect, scratch stability and apply;
     qualified read-only deployment evidence introduces no target agent, host
     scan, binary copying or new attestation service.
     Qualification belongs to the actual backend/session: reconnects, failovers
     and replacements recheck full compatibility, effective settings and safety
     controls before proceeding, not only instance separation. Discard partial
     evidence and restart complete compilation in fresh scratch resources even
     when the replacement qualifies; do not mix results across sessions.
     Same-connection qualification also needs stable analysis inputs throughout
     compilation: enforce exclusivity or detect all relevant intervening
     mutations, including change-and-restore, and discard an invalidated run.
     Initial/final equality alone is not evidence of stability; these protections
     apply to scratch, not a new production lock or coordination service.
     Compile under a reproduced, qualified deployment authorization context,
     not the setup administrator's or introspection login's privileges. Pin and
     recheck relevant roles, ownership and effective grants/path visibility,
     including the actual apply context, without copying authentication material
     or introducing target privilege management.
     PostgreSQL binding resolution lands first; SQL Server follows its own
     design and live tests, sharing infrastructure but not binding semantics.

     Saved evidence joins the checksum and includes relevant target candidate
     sets, environment prerequisites and expected bindings. The versioned input
     manifest fingerprints every required external resolution property, including
     casts, types, operators and extensions, with complete membership/absence
     predicates. Identity-only or routine/view-source-only checks are insufficient;
     an unchanged old binding cannot waive a changed premise for not rebuilding.
     Retained external definitions stay private and ephemeral for reconstruction;
     only logical identities and versioned canonical fingerprints enter saved
     evidence.
     These fingerprints can verify guesses of confidential literals; no plaintext
     is not a secret-free promise. Plans carrying external-input fingerprints
     require a validated confidential classification and protected publication
     to authorized recipients, with no verifiers in ordinary diagnostics or
     public derivative checksums. Unknown/public handling refuses; this does
     not introduce an approval service or key-management system.
     The same classification covers the existing SHA-256 approval/audit value:
     qualify literal display, command/CI/process-argument paths and ledger,
     snapshot and history readers before enabling these resolver artifacts.
     Ordinary output omits the verifier; protected invocation still supplies
     the human-approved `--checksum`. Unknown or overly broad access refuses,
     without silently changing grants or replacing the approval mechanism.
     Pre-feature timeline readers can expose the checksum despite an unsupported
     state version. #594 separately owns the ledger/legacy-access design and
     compatibility tests; its acceptance, implementation and passing tests are
     mandatory before confidential resolver publication/apply/recording is
     enabled. Version metadata or new-client redaction cannot waive that gate.
     *Since DEC-952.1 (#952) the fingerprints are keyed per environment, so no
     verifier is recorded and this gate, with #594's ledger design, is gone.*
     This decision selects no physical ledger migration or credential transition
     and preserves ordinary-plan behavior and explicit SHA-256 approval.
     Before source transfer, qualify and enforce engine/platform controls over
     server logging, intermediaries, container capture/forwarding and disposable
     source-bearing storage. Every resolver control/evidence exchange, including
     managed-only analysis and target rechecks, requires authenticated integrity
     and peer validation; private inputs additionally need confidentiality.
     Initial remote profiles use authenticated encryption throughout; localhost
     or unverified TLS is not proof. Private-source logging remains conditional.
     Unknown controls refuse reconstruction; client
     redaction and database deletion are not proof of no retained copies. Do not
     disable pre-existing audit policies. ADR-0016 defines the trusted-runtime
     boundary and required negative tests for both isolation and source handling.
     Publication/apply re-read and fingerprint every required input's properties
     and recheck membership/absence without exporting private source or properties
     through artifacts or diagnostics. Recheck before
     artifact publication and under the deployment lock before apply; changed
     premises require replanning and approval. Each capture must itself be
     coherent; repeated mixed-time reads are not evidence. PostgreSQL inherits
     the owned/caller-owned read boundaries of 250 and 423, with non-snapshot
     inputs handled explicitly. Initial result verification is transactional:
     the closing coherent capture checks both bindings and the complete input
     manifest against sealed post-apply expectations, including approved typed
     changes, before commit/success. A retained old binding cannot waive changed
     prerequisites visible at that boundary; failed checks roll back, without
     promising to prevent later external writes. Apply never starts a
     resolver or changes the approved migration. Preview-only rehearsal, human
     intent, risk gates and the single-deployer limits remain intact; arbitrary
     image synthesis, snapshot-derived deployable plans and resolver-backed
     staged apply are deferred. Existing rebind protections remain until tested
     replacements land. No runtime format changes are made by this decision.

<a id="decision-492"></a>

492. **Ship advisory resolver discovery before qualification, without a
     verified state.** The first bounded child of #595 (#597) adds catalog-only
     environment observations and official image-family suggestions to `doctor`
     on both engines. A separate `pbps-db::resolver::Discovery` report holds
     these connected answers; engine crates own its SQL and candidate rules,
     and CLI dispatch/reporting follows the existing seam. Serialization and
     JSON Schema dependencies describe that shared report without duplicating
     DTOs in CLI or moving transport/engine SQL into the domain model.
     `DiscoveryCompatibility` can only express `Unverified`; these observations
     cannot authorize resolver compilation or enter saved-plan evidence.
     Discovery labels session settings as the introspection connection's, not
     the future deployment context, and names the missing analysis-specific
     build, authorization, compatibility, transport, isolation, containment and
     stability qualification. The initial inventory is not a coherent evidence
     capture or full prerequisite manifest (ADR-0016 decisions 4–5).

     SQL NULL is `NotReported`, distinct from both an observed empty string and
     an unknown qualification with its reason: metadata visibility and product
     support can also produce NULL, so it cannot prove absence or compatibility.
     An actual failed query stays unanswerable; missing optional qualification
     stays advisory to ordinary readiness (SPEC §9.8 and §9.3.3). PostgreSQL
     locale fields span the PostgreSQL 16/18 catalog spellings. SQL Server
     suggestions require an explicitly known boxed product family and release;
     hosted or unknown products do not borrow boxed version numbers. No image
     tag certifies build, platform or target edition equivalence. Nothing is
     provisioned or probed with DDL, and no source, fingerprints or credentials
     are collected. The envelope gains optional fields; published schema set
     11 archives that addition while envelope wire version 1 stays compatible.

<a id="decision-494"></a>

494. **Select a named resolver policy without acquiring or certifying it.**
     #606 advances ADR-0016's shared environment stage after #597. Configuration
     names trusted Docker/server profiles; an internally tagged enum prevents
     ambiguous mixed backends. CLI > environment > project precedence returns
     pure configuration data. Server credentials remain environment-variable
     references, and Docker pull policy defaults to `never`; `if_missing` is
     explicit acquisition authorization for that configured source. Neither a
     source selection nor a discovered candidate proves qualification.

     The CLI accepts profile names instead of inline credentials or arbitrary
     image arguments. Connected summaries expose the selected profile/policy
     as `not_acquired`; selection is not persisted as saved-plan evidence and
     does not waive existing planning protections. The sole status variant
     cannot report success from infrastructure that has not been implemented.
     Acquiring, admitting and qualifying a real runtime belongs to #607–#611;
     binding-backed planning follows later in #595's ordered child issues.
     Offline/check/explain and doctor do not resolve defaults or read scratch
     credentials. An unknown selected profile is an actionable finding before
     target access/output; contradictory explicit workflow flags keep the
     existing flag-error convention. Schema set 12 archives the new config and
     optional summary data without changing the envelope wire version or any
     saved-plan format.

<a id="decision-497"></a>

497. **Docker resolver admission holds native runtime capabilities across a
    source-free bootstrap gate.** Image acquisition is policy-bound and records
    actual immutable content and platform, but grants no compatibility or source
    permission. A protected local Unix socket and its live root-installed Docker
    peer authenticate provisioning. The initial `linux-amd64-v1` profile requires
    the target observer and daemon on the same native Linux kernel; an arbitrary
    root-owned proxy, remote endpoint or unreadable kernel premise is refused.

    Reserve only a root deadline and an unprivileged fixed waiter. Before engine
    initialization, independently bind the target's verified TLS connection to
    its actual engine service/socket owners, reject shared runtime namespaces,
    and measure effective cgroup and visible mount controls. Retain those handles
    through the bootstrap transition. Engine-owned readiness precedes the sole
    private protocol connection; a failed handshake discards the entire run.
    A fixed forwarder in separate PID/mount namespaces owns that connection.
    The workload has no outbound network, executable writable storage, runtime
    socket or access to the forwarder's descriptors. Denying `connect` alone is
    insufficient: measured TCP Fast Open requires flag restrictions on the send
    syscalls too. Unset image environment keys before execution and disable
    inherited healthchecks rather than trusting image metadata as isolation.

    Recheck live process, namespace, mount, cgroup, backend and socket handles
    around continuity reads. Cancellation removes the whole session capability
    before awaiting I/O, so a later matching fingerprint cannot revive it.
    Cleanup is supervised independently of the requester and may reconnect only
    to remove the exact owned resource; it cannot resume analysis. A root timer
    bounds descendants even after client loss. Native target revalidation needs
    no Docker binary, database write probe or installed production agent.

    The CLI owns provisioning and runtime lifecycle; `pbps-db` supplies a bounded
    byte-stream protocol connection without claiming transport qualification.
    Instance SQL remains in the engine crates. No serializable admission flag,
    plan-format change, binding operation or `--dev` certification is added.
    Build/context compatibility and source-handling gates remain later #595
    steps. See [the runtime boundary and measured fixtures](../RESOLVER-RUNTIME.md)
    for supported premises and the distinction between component fixtures and
    complete admission on a disposable native Linux runner.

<a id="decision-498"></a>

498. **Authenticate socket-activated Docker through its accepted Unix peer,
    not the listener creator's credentials.** Linux `SO_PEERCRED` preserves
    the creator of a listener inherited by `dockerd -H fd://`, so requiring
    that PID to be dockerd rejects the standard systemd installation. A
    protected `/run/docker.pid` supplies the candidate only when the listener
    creator is native PID 1 running root-installed systemd. Root process and
    executable checks still apply. One exact `NETLINK_SOCK_DIAG` query binds
    the client inode and kernel cookie to the accepted peer inode; dockerd
    must hold that peer descriptor. Hold and recheck the process/socket binding
    across API requests and attach traffic. Never accept arbitrary root
    proxies, trust Docker response metadata as peer proof, dump every host
    socket or reconnect to recover admission. The existing Rustix dependency
    supplies safe Linux socket calls without project-local unsafe code.
    An inherited-listener fixture measures the creator/acceptor distinction
    and rejects unrelated owners, cookie substitution and lost continuity.

<a id="decision-514"></a>

514. **A supplied scratch server is a known container layout reached through
    the Docker profile's forwarder, not an arbitrary runtime proved contained
    from outside.** An earlier shape of #609 admitted whatever container an
    operator had assembled and set out to prove, from `/proc`, that nothing in
    it could reach the target or be reached by anyone else. Forty-nine review
    findings later that proof was still open, and each finding was real: a
    bind under an allowed prefix, a tmpfs whose root proves nothing, a mount
    stacked on a safe one, a task in no top-level listing, a socket carried in
    from another namespace, a relay forgotten by one check in three. The
    tables of a container someone else built can be arranged in any number of
    ways, so the question has no bottom, and five of its holes were being
    recorded as limits rather than closed. What was being verified was, in
    practice, the Docker profile's own container started by the operator
    instead of by pbps — so the profile now says exactly that. The operator
    names a container under a Docker-API runtime pbps can reach as root;
    admission reads the daemon's record (no privilege, no network, read-only
    root, tmpfs mounts only, bounds present), pins its id, init, start time
    and image, decides target separation on the actual processes before
    anything else, and measures the kernel against the rows the profile names:
    every mount must be one of them, every task at the profile's identity and
    privileges inside the init's cgroup, only loopback in the network
    namespace, and nothing sharing its namespaces that is not in its PID
    namespace except this run's own forwarders. Equality with a layout is
    decidable where containment of an arbitrary one is not, and Docker's and
    Podman's layouts were measured and pinned. The engine is reached as the
    Docker profile reaches its own: a run-owned forwarder from the container's
    image into its network namespace, one TCP session, bound to the one
    established pair the forwarder's processes hold and exactly one engine
    process serves. Because that namespace has no route out, the kernel's
    census of it is complete for what reaches the engine, which the earlier
    shape's Unix-socket channel could never be (its accepted end lived in the
    caller's tables); the engine's session list and cumulative counter remain
    the second signal, since a session that opened and closed between reads
    leaves no row. The cleanup capability is data, not a handle — the
    credentials, the daemon path and the pinned container — so no failure of
    the analysis can drop it; a cancelled await drops only the session it was
    on, and cleanup opens a fresh one. The operator gives up nothing that a
    scratch server needs: storage is a tmpfs bounded by the container's memory,
    and the container stays warm across runs.

<a id="decision-520"></a>

520. **The analysis scope is qualified by comparing measured facts, reproducing
     the deployer, and reading loaded executable content — not by trusting
     versions (issue #610, ADR-0016 cases 5, 14, 16, 21, 23).**

     A resolver run compiles only in a scratch environment measured equivalent
     to the target's, as its own deployer would see it. `pg-analysis-scope-v1`
     compares patch-level version, encoding, the provider's *actual* collation
     version, every installable extension and its native libraries, the
     effective settings the dialect does not pin, and the deployer's effective
     visibility — all as three verdicts, where `Unknown` (a fact a side could
     not read) refuses like a provisioning failure and never reads as a match.
     A version string or an image tag is not an input to the rule, so neither
     can pass a check.

     Executable identity is content, not version: the engine image is hashed
     through the held `/proc/<pid>/exe`, a loaded library through
     `/proc/<pid>/map_files` (root, which the inspector has), and a required
     but unloaded library as a disk candidate resolved the way the loader
     would; a library replaced under a running process is caught by inode
     identity between `maps` and the path, never by device number, which
     overlayfs reports from different filesystems.

     The deployer is the planning connection's `current_user`, never the
     scratch administrator. Its authorization is reproduced on scratch with
     run-local roles named `pbps_role_<n>_<token>` — renamed freely because the
     emitter's write path cannot contain `"$user"`, so identity is carried by
     a role map, not the name — created NOLOGIN with the measured attributes
     and memberships, reachable only under an explicit `SET ROLE`. Compilation
     as the setup administrator fails the negative control. The reproduction is
     verified by re-reading it as the mapped deployer; anything unreproduced is
     a mismatch, and the plan's own preceding grants run on scratch as that
     deployer, so a grant it could not make on the target does not take there
     either. The target's facts and authorization are read in one catalog
     snapshot, so the sealed scope never holds half of a change committed
     between them. The scope is sealed to the target and scratch connections,
     so a reopened session cannot inherit it, and requalified on every check so
     an in-place change invalidates the run. SQL Server is the twin step, 521.

<a id="decision-521"></a>

521. **SQL Server's analysis scope is qualified by the engine's own answers:
     a family gate, an edition limitation, mapped packages as the engine, and
     grants run only after the reproduction is verified (issue #611, ADR-0016
     cases 5, 14, 16, 21, 23).**

     The twin of 520, under its own versioned rules `mssql-analysis-scope-v1`
     and `mssql-auth-v1`, measured on SQL Server 2025 (17.0) for Linux. Four
     things are not PostgreSQL's and are decided here.

     *The product family is a premise, the edition a limitation.* Azure SQL
     Database, Managed Instance, Synapse and Edge report version numbers of
     their own that name no boxed build, so an `EngineEdition` outside 2–4 is
     unknown on either side and the comparison stops there; nothing maps a
     hosted version to an image by guess. Inside the boxed family an edition
     difference is not a binding difference — a Developer scratch resolves
     names exactly as an Express target does, measured on two real instances of
     one build — but it is a capability difference. It verifies with a named
     limitation: the target's own edition and capability checks stay the
     authority, and a statement the scratch accepts proves nothing about them.

     *The engine is its mapped packages.* SQL Server for Linux runs its Windows
     binaries out of `.sfp` packages the platform layer maps into the process
     (measured: `sqlservr.sfp`, `system.common.sfp` and others, thousands of
     ranges), so `/proc/<pid>/exe` is only the loader. The engine router names
     `.sfp` as engine code and the packages are hashed like any loaded library;
     without that, two builds with one loader would be the same engine.

     *Effective answers are the engine's.* The context is read as the deployer:
     `fn_my_permissions` for the database and each in-scope schema,
     `IS_ROLEMEMBER` for its roles. DENY, role nesting, ownership and the fixed
     roles are the engine's to combine; a second implementation would be a
     second thing to keep right. The grant rows it can see are what the
     reproduction replays, under their own grantors (`AS`). What it cannot see
     is why a grantor could grant: a session is shown only the rows that
     concern it. It need not be shown. Measured, a grant made through
     `CONTROL`, `db_owner` or `db_securityadmin` records the securable's owner,
     a database-level grant option cannot grant on a schema, and taking a
     grant option back cascades; so a row naming any other grantor proves that
     grantor holds that permission with the grant option on that securable,
     and the replay gives the mapped grantor exactly that when no visible row
     does. `verify` still reads the result as the deployer. The engine's
     built-in principals — `dbo`, `public`, the fixed `db_` roles — keep their
     identity, as predefined roles do in 520: a clone of `db_ddladmin` would
     carry none of what membership grants. The session's language is the
     login's default, which decides the date format and the first day of the
     week a literal is read under, so the run login is given the deployer's.

     *The plan's grants run after verification, not before.* PostgreSQL records
     a grant under a grantor the engine chooses, so 520 projects the grants
     onto the context and verifies against the projection — and the projection
     was where most of #688's findings lived. SQL Server refuses a grant the
     deployer cannot make outright (error 15151, measured), so the reproduction
     is verified against the target as read and only then does the engine run
     the grants as the reproduced deployer. Nothing predicts a grantor. The
     sealed fingerprint is the context as read together with the grants, and a
     later check compares the scratch side with what was sealed once the scope
     settled, since a scratch that ran the grants no longer equals the target.

     The lifecycle is shared. Every engine-specific step routes through
     `pbps-cli`'s `resolver::scope`, so the run lifecycle and the target
     binding name no engine and a third engine is a compile error in each
     function there rather than a branch someone forgot (#714). SQL Server's
     catalog views are not a snapshot under any isolation level, so its target
     read is bracketed — read twice, and the two must agree — where 520 uses one
     transaction. Qualification is not a binding adapter; SQL Server's stays
     unimplemented (#619, #620).

<a id="decision-522"></a>

522. **A process walk passes over a child that is *over*, and refuses
     anything still alive.** `process_scope` reads a node's `children`, opens
     each pid and reads its `stat`, and refused the whole walk when that
     `stat` named a different parent. Two very different things look like
     that, and refusing both made a valid deployment intermittently refused:
     `socket_owners` walks this for every `SocketOwnerLease::check`, so the
     operator was told "the target this run was aimed at changed" about a
     target that had not moved (#674, three occurrences, both engines).

     **Measured**, walking a shell that spawns and reaps two children in a
     loop: 785,426 walks produced 1,055 refusals, every one at this branch.
     Classified over 1,155,160 walks, 2,769 of 2,771 occurrences were a
     process in state `X` or `Z` — the child caught mid-exit, its `stat`
     already reparented to the reaper while `/proc/<pid>` still answers.

     A process that is over is the one case that can be passed over without
     asking anything else: it holds no descriptor, runs no code and owns no
     socket, so no caller of this walk has a question it could answer. What
     decides that is `exited_stat`, which this file already had, and not a
     state test written at the branch — the count matters as much as the
     state, because a dead leader can retain live threads and a surviving
     thread can hold the socket or have descendants of its own. A second,
     weaker answer to one question was the first shape of this and review
     caught it.

     **`exited_stat` accepts a thread count of none, which it used to refuse.**
     `aa7315d2` wrote it to admit "a dead leader with no surviving threads"
     and then refused `num_threads` 0, and its test pinned the refusal.
     Measured here, that is precisely what a child caught mid-exit reports: a
     complete fifty-field `stat`, state `X` or `Z`, `num_threads` **0**, 1,409
     times. The guard was refusing the case the function set out to admit, one
     value further on, and with it in place this walk still refused every
     dying child — 234 refusals per 197,000 walks with the branch otherwise
     correct. A count that cannot be read is still an error, because that is a
     reading nobody made; a count of none is a reading.

     **Anything still alive refuses, as it did before.** The first shape of
     this fix skipped a pid the parent no longer listed, which is not proof of
     exit: a live descendant reparented to a subreaper leaves its old parent's
     list while remaining in the scope, and passing it over would hand
     `PrivateChannelLease::check` and `check_kernel_parts` an incomplete scan
     that reads as a complete one. Review caught that, and the answer is that
     absence from a list is not an exit — the same distinction 519 is about,
     one question further on.

     Measured after the fix: 1 or 2 refusals per 200,000 walks against 267
     before it. The residual is a pid **reused** between the two reads by a
     process outside the tree, which cannot be told from a reparent without
     comparing the opened process's `starttime` against the moment the list
     was read. Refusing is the fail-closed answer to that ambiguity and stays;
     closing it needs a clock-tick conversion behind a `rustix` feature this
     workspace does not enable, which is #729 rather than a line here. A
     passed-over node's live descendants can still leave the result silently,
     through this branch and through the two `process_gone` branches that
     reach it four times more often, which is #730.

     **The second window is the table, not the tree.** With the reads named,
     CI named one: `peer-server-absent`, the engine's established row missing
     from `/proc/self/net/tcp`. Iterating that file is a `seq_file` walk over
     hash buckets and not a snapshot, so a row can be repeated as well as
     dropped. Measured against a loopback pair held open while four threads
     churned 7.3 million connections: **1,413 of 43,311 reads returned the
     same pair twice, every one carrying the same inode**, and two different
     inodes for one four-tuple was never seen — which is what a four-tuple
     being one socket requires. Refusing the repetition was refusing a read of
     the table rather than a change to the socket, at 14% of reads under that
     load, so a repeated row is now one socket seen twice and only a differing
     inode is the table contradicting itself.

     **The walk does not drop a row, which is the other half of that and was
     worth finding out.** A skipped row was the obvious reading of the
     `peer-server-absent` CI named, and it is wrong: a pool of 3,000
     connections filled and emptied repeatedly with `RST` closes, so that a
     close is an immediate removal, missed nothing in 2,451 reads, and 200
     established pairs held open while four threads churned 8,664,700
     connections missed nothing in **2,202,800 row observations** — while a
     third of those reads carried a duplicate. It re-emits on insertion and
     does not skip on removal. So an absent row is the engine's end not being
     established, which is the check being right rather than a read to be
     hardened, and the refusal stays. What it could not say is *which* absence:
     no row at all, or a row in another state. `PeerServerState` carries
     `/proc/net/tcp`'s own `st` column for the second, read **of the engine's
     row**, which is where the direction lives and is easy to invert.
     **Measured** on a loopback pair, and pinned by
     `the_engines_row_says_which_end_closed_first` rather than left in a
     comment: closing this verifier's end left the engine's row at `08`, and
     closing the engine's end left it at `04` or `05` depending on whether the
     read caught it before our ACK. So on the engine's row `08` (`CLOSE_WAIT`)
     means it is holding *our* FIN and has not closed, while `04`/`05` and
     `06` mean the engine closed first. The first draft of this said the opposite, and a
     refusal that names the wrong end sends the next investigation to the
     wrong process — worse than naming none.

<a id="decision-528"></a>

528. **Observe a pinned PID-namespace procfs view without claiming a complete
     process tree (issue #740).** A parent's exit can reparent a living child
     behind a descendant walk's cursor without making any read fail. A procfs
     view tied to the selected namespace avoids that dependency and bounds a
     private container's enumeration without translating every local PID by
     scanning the host. The anchor, namespace handle, procfs filesystem and
     exact mount are pinned; hidden-process views and unverified mount layouts
     refuse. Ordinary paths cannot cross a mount, including a same-filesystem
     bind over one numeric process entry.

     Task coordinates carry their namespace. A local number is neither an
     observer PID nor a live identity; held task directories and start times
     bind status reads, and per-thread reads include surviving workers after
     their leader exits. A proven exit differs from an unreadable live task,
     unknown view, or replaced identity. Sequential observations are still not
     an atomic snapshot or continuous containment. Namespace membership,
     cgroup membership, engine origin and connection ownership remain separate
     questions, with launch controls enforcing restrictions between reads.

     The primitive is introduced before caller migration (#741), the verified
     launch boundary (#742), and native target integration (#743). It does not
     certify those later contracts or replace the existing production callers
     in this stage. See [the contract, pre-implementation measurements and
     repeatable fixture](../RESOLVER-NAMESPACE.md).

<a id="decision-529"></a>

529. **Container admission checks tasks through their held namespace view;
     process identity and privilege exceptions do not use a bare PID (#741).**
     Runtime-exec tasks may have parents outside the container, and a surviving
     worker may have credentials different from its group leader. Workload and
     forwarder checks therefore inspect every task from the qualified procfs
     view. Supplied-server engine discovery counts process leaders separately;
     a credential match is not evidence of engine origin. Pre-engine membership
     uses the same source rather than a descendant count.

     A namespace-derived lease retains its procfs view for relative parent
     lookups and cannot supply an observer PID. Across procfs instances, the
     same live task has different directory device/inode pairs: identity uses
     the innermost task number, start time and retained namespace handles, with
     both live leases checked before and after comparison. This binds the root
     lifetime guard's exception to the actual task and refuses exited/reused
     identities. Capture and callback failures are ignored only when that held
     incidental task is proven to have exited.

     Production consumes each held task before opening the next; collecting
     all task-directory descriptors first would refuse an otherwise admitted
     container whenever its task count exceeded the observer's descriptor
     budget. Engine discovery retains at most two candidates, enough to refuse
     ambiguity without making descriptor use proportional to the task count.

     Cgroup subtree/resource limits and foreign network/mount/IPC accounting
     remain separate; enumerating a cgroup cannot discover a namespace entrant
     outside it. The foreign-sharer check still uses the observer's wider view.
     Socket ownership remains #743, and launch enforcement remains #742. These
     observations do not establish an atomic inventory, engine provenance or
     protection against the provisioning administrator. The measured Docker
     recipes and namespace fixtures qualify these distinct questions separately.

<a id="decision-531"></a>

531. **The launch drops to a shared workload identity before bootstrap; the
     root deadline retains a separate, necessary authority (#742).** The
     Docker runtime creates storage for the final UID/GID, so the fixed waiter,
     initialization and engine need no ownership privilege. Launch arguments
     and native qualification use one workload identity: PostgreSQL 999/999
     with no capabilities, SQL Server 10001/0 with NET_BIND_SERVICE, and the
     forwarder 65534/65534 with none; each clears supplementary groups. The
     guard's ceiling is SETUID/SETGID/SETPCAP/KILL, adding NET_BIND_SERVICE only
     for SQL Server. Measured PostgreSQL startup, SQL and forwarding still pass
     after removing that unnecessary bit from its guard.

     A maximum mask is not required authority: without effective CAP_KILL,
     root timeout cannot kill its differently owned child (#634). Require the
     bit before releasing the waiter and during later channel qualification,
     and check the final workload's groups as well as all UID and capability
     sets. The guard's own group 0 is permitted; no other task inherits its
     exception. Wrong-group and missing-KILL real-container regressions failed
     before these checks and passed after them. Independent deadline and
     detached-child tests retain the actual termination control.

     The supplied Docker PostgreSQL recipe uses direct uid/gid 999 and
     cap-drop ALL with its runtime-prepared storage. Podman 4.9 rejects Docker's
     tmpfs uid option; its measured component alternative chowns a fresh tmpfs
     root and drops before initialization with only CHOWN/SETUID/SETGID/SETPCAP
     (plus NET_BIND_SERVICE for SQL Server). This does not admit a Podman daemon
     (#686) or turn an operator assertion into runtime evidence.

     Parent, thread, fork and exec measurements preserve the final credential
     and capability ceiling, no-new-privileges and the respective workload or
     forwarder seccomp behavior. They establish inherited restrictions, not
     atomic or permanent namespace membership. Keep task, cgroup, foreign
     namespace, socket and exclusivity observations; runtime-exec entrants are
     provisioned by the trusted administrator, not descended from the dropped
     waiter. Exact policy attestation, provenance and source-handling follow-ups
     remain independent gates. No model, driver boundary, public schema or
     deployment authorization changes here.

<a id="decision-533"></a>

533. **A socket holder is observed; target identity is positively bound under
     trusted provisioning, not proved by an exhaustive census (#743).** Real
     acknowledged FD handoffs kept at least two holders alive throughout a
     flat scan while it observed one, including four repeated passes with
     unchanged process identities and socket cookies. The user approved
     withdrawing the universal uniqueness promise. Kernel/administrator and
     selected engine provisioning are trusted against deliberate endpoint
     sharing; native target observation does not freeze or change production.

     Enumerate the selected service's pinned procfs view and read every visible
     task's descriptor table. Capture executable/process leases for matching
     thread groups only: unrelated host programs need not have root-installed
     executables, and shared SQL Server thread tables must not count as many
     engine processes. A worker's unshared table remains observable. Stable
     siblings and reparented holders remain visible without descendant walks.
     Required unreadable evidence refuses; retaining more than three holder
     leases is unnecessary because the most permissive caller, the fixed
     forwarder, permits three. An empty observation gets one fresh pass for
     late backend visibility, not repeated scans as a completeness proof.

     Native binding separately follows the held holder's parent relation to
     the selected service. Sharing an executable or namespace does not make
     it that service's backend. Retain the actual TLS connection identity,
     endpoint/socket pair, held service/backend identities and engine-owned
     identity query; invalidate replacement, reconnect, canceled checks and
     discarded witnesses. Known alias/proxy and instance-separation negatives
     remain required; they do not prove absence of every cooperating proxy.

     All private-channel and supplied-server holder consumers use the same
     explicitly limited observation. Launch restrictions, cgroup limits,
     foreign namespace accounting, scratch/target separation and engine session
     evidence retain their independent roles. The synchronized two-holder
     omission is documented and tested as a limit of observation, while stable
     extra holders, reparenting, shared/unshared thread tables and a backend
     appearing behind the scan have real-kernel regressions. No saved-plan,
     dialect, driver or deployment authorization boundary changes here.

<a id="decision-542"></a>

542. **The resolver qualifies inherited seccomp behavior before releasing its
     fixed bootstrap, without tracing processes (#633).** A real filter that
     allows `connect` or TCP Fast Open still has mode 2 and a filter count of
     one. Both pinned engine images admitted these incorrect policies through
     the old native waiter check. Docker's reported JSON is not independent
     evidence of what the kernel installed.

     Native target separation and containment come first. A fixed source-free
     probe then checks prohibited calls through x86-64, i386 and x32, including
     multiplexed connect and all three Fast Open send calls. A distinct
     acknowledgement precedes rechecking the retained native leases and
     releasing initialization. Each owned forwarder runs its corresponding
     probe before the fixed protocol program. Failed, unavailable, incomplete
     or canceled evidence cannot release the engine or restore a discarded run.

     The tiny fixed ELF is generated in safe Rust and executed from a sealed
     anonymous file by the images' existing Perl. There is no host compiler,
     additional image, persisted helper, writable executable mount, or new
     unsafe-code exception. Fork/clone/exec inheritance maintains the measured
     restriction for the fixed bootstrap's engine descendants; this adds no
     lifetime process census and does not certify a preexisting supplied
     engine's policy (#684).

     Kernel BPF reads were measured too: they need an unfiltered privileged
     ptracer and a stopped tracee. Equivalent policies also showed different
     architecture-dispatch ordering, making a raw hash unsuitable as a portable
     policy identity. Fixed behavioral checks fit DECISIONS 533's trusted
     provisioning premise; they do not promise arbitrary BPF equivalence or
     protection against an administrator deliberately manufacturing
     probe-specific behavior. Real-engine compilation, additional loopback
     connections, valid-socket Fast Open attempts, wrong-policy bootstrap
     refusal and actual removed-probe controls pin the supported contract.

<a id="dec-804-1"></a>

**DEC-804.1. Private resolver names are qualified in the actual held UTS namespace,
independently of runtime files and Docker configuration (#804).** Both
pinned engines can start with three empty runtime files while arbitrary
kernel hostname/domainname values remain visible. PostgreSQL SQL reads
both; SQL Server's `MachineName` exposes the hostname (its measured procfs
bulk reads fail). Merely restricting the empty-file branch would leave the
same independent kernel inputs unchecked under generated fixed files.

A process lease therefore retains UTS identity alongside its other
namespace handles. A short-lived scoped thread enters only that held UTS
namespace using safe rustix, reads `uname`, and terminates before the
observer continues. A target-root procfs path, even pre-opened, answers for
the reading thread's UTS namespace instead. Moving an async worker and
trying to restore it would introduce an unnecessary recovery obligation.
Unknown permission, failed entry or replaced identity refuses admission.

The complete hostname is `pbps-resolver`; the complete NIS name is empty,
`(none)` or `localdomain`. These literal generic values contain no
operator-specific data. Empty Docker configuration is not an alternative
proof: on the measured daemon it retains `localdomain`. Each workload and
forwarder has its own qualified view, including both supplied forwarders;
existing task observations require the corresponding UTS membership.
Name loss discards live analysis even if the name is later restored.
This adds no continuous census or privileged target write, and retains
DECISIONS 533's trusted-provisioning boundary between observations.

<a id="dec-612-1"></a>

**DEC-612.1. Target capture separates the owned catalog snapshot from rendering,
session and native-build observations (#612).** A PostgreSQL repeatable-read
snapshot can retain an old stored tree while `pg_get_*` prints names from a
newer catalog. Reading the witness again in the same snapshot proves nothing.
The capture therefore closes its owned read before a fresh tuple-witness
observation; a changed row version refuses even after a name was restored.
These physical coordinates are private interval witnesses, never fingerprints.
Canonical and relevant user-context settings are transaction-local, session
inputs are checked across the read, and actual collation-provider versions are
separate from recorded metadata. Native capture encloses a fresh coherent read
with actual executable-content and process/socket checks. Cancellation takes
the complete bound connection away before the first await. Acquisition and
compilation start only after capture has released the target transaction.

<a id="dec-612-2"></a>

**DEC-612.2. A resolver input is a complete logical property record and a complete
membership predicate, not an object name or dependency edge (#612).** Stored
PostgreSQL trees supply observable historical bindings, including pinned
builtins absent from `pg_depend`. Snapshot rows resolve names/signatures and
columns; no cross-database OID or live-cache name stands in for logical
identity. The requested closure includes class-specific properties, referenced
prerequisites and owned/extension members, with explicit empty candidate sets.
Full catalog descriptors and serialized node fields qualify the measured
16/18 layouts. Unknown required properties/classes refuse. Only selected,
qualified definitions and datum/type-modifier output may be rendered.
Versioned cryptographic comparison includes complete definitions and literals,
baseline identity/state and effective session facts. Identity-preserving cast,
type, operator, extension and authorization changes therefore invalidate inputs.

<a id="dec-612-3"></a>

**DEC-612.3. Connected capture stays private until the confidential artifact path
can enforce its contract (#612).** Captured source, canonical properties and
SHA-256 guessing verifiers have no ordinary `Debug`/serialization path or public
verifier getter. Comparison reports contain logical objects and conditions
outside semantic Schema equality. Runtime-bound routine bodies remain named
limitations rather than empty dependency proofs; C routines still name native
file prerequisites even without extension membership. A catalog read is not a
native runtime qualification, a scope request is not proof of complete SQL
analysis, and neither creates an applyable artifact. Reconstruction, compilation,
ordering and sealing the keyed evidence (#614, DEC-952.1) remain separate gates.


<a id="dec-643-1"></a>

**DEC-643.1. Resolver pseudo-filesystems need the identity of the exposed kernel
object; devpts origin remains a provisioning premise (#643).** A cgroup v2
filesystem device identifies the shared hierarchy, not the exposed subtree.
The supplied profile compares its visible root's device and inode with the
already-held bounded cgroup directory. The factory profile instead requires
an actually empty `/sys`, making the underlying cgroup mount unreachable.
Both profiles pin the mqueue root obtained through a detached read-only view
of the held IPC namespace, and compare the visible root on every recheck.
The view is created on a short-lived thread: the ordinary observer never
moves namespaces, no mount is attached, and no target queue is read or written.
Missing permission or kernel support refuses admission.

Measurements on Docker rootful and Podman rootless stock-image layouts confirm
the cgroup and mqueue identities; the latter is a filesystem measurement, not
native Podman daemon admission. Owned namespace probes show that devpts creates
a distinct instance for each filesystem creation even in the same namespaces. An older
instance transferred into a new private mount namespace has the same ordinary
root, flags and propagation fields as a fresh one. Neither a new probe nor
absence of a live foreign holder establishes its origin. Trusted provisioning
must supply the private devpts instance and exclude transferred foreign
terminals; unknown origin requires reprovisioning. The profile's kind/flag
checks do not certify that historical premise.


<a id="dec-876-1"></a>

**DEC-876.1. Native capture transfers an operation, not source-bearing path
strings (#876).** Removing Debug/Serialize from a public record did not hide
its fields: a downstream crate could still extract `probin`, preload names and
`dynamic_library_path`. A callback receiving those strings would preserve the
same escape. Requirements and resolved candidate spellings therefore stay
inside engine-owned opaque types. A read operation accepts an already-held
root and the lifecycle's mapping inventory, returning only a supplied mapping
index or an opaque reader. The reader has no File/raw-handle getter or ordinary
formatting/serialization; even File's Debug can reveal its path. This is API
encapsulation against accidental source disclosure, not an isolation boundary
against privileged code that can independently inspect the same process.

The engine owns PostgreSQL loader rules and the narrowly scoped confined open;
CLI retains process leases, complete loaded-content observations, off-runtime
hashing, inode/size/content alias correlation and cancellation. Candidates open
sequentially, preserving the descriptor bound and first-opened-file behavior.
Exact mapped candidates retain loaded evidence even after their disk name
vanishes. An opened but unreadable file never falls through to a later file.
The selected candidate position and opaque resolution preserve comparison
identity without exporting a path hash. No public verifier, provisioning code,
source publication path or protected ledger storage is introduced.


<a id="dec-882-1"></a>

**DEC-882.1. Closing target capture compares the complete safe public role
record without authentication-catalog access (#882).** `pg_roles` has no
`xmin`, so the owned catalog read's physical witness did not cover an
identity-preserving authorization change. Both supported PostgreSQL majors
accept such changes during the first snapshot. Capture now projects the same
qualified public role fields (excluding the masked password field) in its raw,
rendered and fresh closing reads. The closing cursor shares one new snapshot
across public role values, complete role membership/absence and the existing
physical witnesses; unreadable input refuses rather than becoming empty.
Canonical settings keep timestamp-valued properties comparable without changing
the caller's settings. `pg_auth_members` retains its physical tuple witness.

This public-value comparison preserves ordinary-reader support without
requiring password-verifier access through `pg_authid`. It detects a changed
closing role record, not every intervening ALTER ROLE: changing a public field
and restoring it before the fresh snapshot can compare equal. That measured
limit is explicit; this observation is not a continuous authorization-history
proof or an enabled resolver-backed apply path.


<a id="dec-882-2"></a>

**DEC-882.2. Foreign mount/IPC accounting excludes known PID members before
executable qualification (merge-group repair for #978).** The merge group's
PostgreSQL guard-limit test refused its valid DDL control with a
`capture-executable` / `Containment(Accounting)` failure. Ten isolated engine
repetitions passed, so the original timing-dependent failure was not reproduced
locally. A deterministic kernel control nevertheless demonstrates the extra
requirement: a live known PID member with an executable that cannot earn a
ProcessLease was rejected by a check whose only question is namespace ownership.

The foreign-sharer scan now compares the held task's PID namespace first.
Known members belong to the separate scoped credential/cgroup qualification;
this scan does not open their executables again. Foreign tasks still require
qualification and caller disposition, unreadable namespace evidence refuses,
and the selected anchor must remain live. The regression exercises actual
private namespaces, a normal-user-built executable, foreign mount/IPC sharers
and anchor loss. Removing the production membership filter restores its failure.
This neither relaxes occupant credentials nor makes the live census atomic.


<a id="dec-974-1"></a>

**DEC-974.1. Ordinary capture evidence does not grant its producer's native
source capability (#974).** Opaque fields did not close the public
`runtime_inputs()` conversion: measured downstream PostgreSQL 16/18 captures,
recaptures and native capture distinguished guessed singleton mappings,
accepted an irrelevant root, read crafted-root files and exposed candidate
precedence. Each is a source observation even without a path getter.

Only a fresh `capture_with_runtime_inputs(connection, scope)` issues the
separate RuntimeInputs capability. The engine performs its ordinary coherent
read, then extracts requirements privately for that producer. The ordinary
CapturedInputs result cannot recover, construct or unlock the capability;
NativeTarget keeps it inside its owned capture operation and returns only
ordinary evidence. A downstream recipient cannot apply a new guessed mapping,
root or candidate probe to an existing result. A caller with its own database
connection already has authority to query those source catalogs directly;
this is source authority, not native process admission or isolation of
privileged Rust code. Opaque operations must not be offered to ordinary report
consumers merely because their parameters and results contain no raw path.

The lifecycle still supplies its actual held root/mappings, checks process and
socket leases, correlates loaded content and owns cancellation. Loader rules,
sequential opens, unlinked mapped content and unreadable-first-candidate refusal
are unchanged. No kernel qualification/provisioning moves into the engine or
transport; no caller boolean, forged trait, global secret or protected storage
is introduced. HMAC plan fingerprints do not authorize this in-memory access.

<a id="dec-726-1"></a>

**DEC-726.1. A planned grant's principal is resolved to the catalog's
spelling through `USER_NAME(USER_ID(..))`, not through
`sys.database_principals` (#726).** On a case-insensitive SQL Server database
`readers` and a catalogued `Readers` are one principal, and `PUBLIC` is
`public`; keyed as planned they were two, so the reproduction cloned a second
principal for the plan's spelling, the planned grant landed on the clone, and a
built-in in another casing was cloned as a user. The target's context read now
takes the planned principals and, within each half of its bracketed read,
records each catalog spelling that differs (`AuthorizationContext::spellings`), which the principal map, `is_built_in`
and `apply_planned` use; it is part of the sealed digest, so a rename before a
check moves it. The catalog view was the obvious lookup and is wrong for an
ordinary deployer: it shows only the principals the deployer may see, so a role
it merely grants to has no row and would stay as planned. The metadata
functions answer for every principal (measured on 17.0 as a user holding no
permission), and they are what spells every grantee in the context already. A
name with no principal behind it stays as planned, since the plan may create
it.

<a id="dec-1013-1"></a>

**DEC-1013.1. Every planned principal's resolution is recorded, absence
included (#1013, amending DEC-726.1).** DEC-726.1 recorded a spelling only
where the catalog's differed from the plan's, so a principal spelled exactly
as planned and one that did not exist both left nothing behind. Dropping such a
principal after `qualify`, when nothing else in the context named it, then
read back as the same empty answer, and requalification accepted the scope.
`spellings` now maps every planned name to `Some(<catalog spelling>)` or to
`None`, so presence and absence are sealed apart; the principal map still uses
the planned spelling for `None`.

<a id="dec-613-1"></a>

**DEC-613.1. The desired namespace is the emitter's bootstrap, reordered only
so that expressions wait for modules, and its order is qualified afterwards
from what the engine bound (#613).** The plan's class order creates tables with
their defaults, checks and indexes before any module (`order_key`), so an
expression calling a managed function either fails or, worse, binds whatever
else of that name already exists; a scratch compiled that way answers the
wrong question without an error. Parsing references to order the compile is
the grammar work ADR-0016 refuses. So scratch runs the differ's bootstrap of
the declarations through the ordinary emitter, with a table created bare, its
foreign keys and its indexes without a predicate after every table, the
modules in the differ's name-scan order, and each default, CHECK, predicated
index and trigger last. An index's key columns are plain names, so without a
predicate it needs only its table; created before the modules, it lets a
module that names it through an OID-alias constant compile and be refused by
the capture for that constant, naming the constant's type, not for a missing
relation (#1042). The scan can miss a
reference, and a module compiled before a same-named object it would have
preferred binds the other one silently. That is detected from the captured
bindings instead of prevented: a module that bound any name first made
nameable later in the reconstruction, by an object that could have been
resolved in its place, is unresolved: a later relation, with its row type, for a relation,
a type or a call; its generated array type, read back from the catalog because
the engine may clip or respell `_name`, for a type or a call only a single argument can reach (its row type can take `t(x)`), a later routine for a routine
or a function-style cast, a later index for a relation only, and only one in
the schema the binding named or on the declaration's path. The check is by name within those kinds, so it is
conservative — a qualified reference it cannot tell from a bare one is
flagged too — and a `depends_on` edge that moves the other object first
removes it. Measured on PostgreSQL 16 and 18: f(integer) whose body calls
`f('x'::text)` binds f(character varying) through an implicit cast when f(text)
is compiled after it. Scratch order builds the namespace; it fixes no
deployment order (#614). Grants, roles and rows are not reproduced, since none
changes a binding; the deployer's path and schema privileges are the analysis
scope's (DECISIONS 520). A run compiles once, into its own database, as the
reproduced deployer, and is read back through an administrative session,
because the capture reads settings a least-privilege deployer need not see and
the reading role changes no stored binding. A failed or cancelled compile ends
the analysis; a second question needs a fresh run.


*Amended by [DEC-1274.2](modules.md#dec-1274-2): a table whose generated column
calls a declared function joins the modules phase after that function, and a
foreign key naming it waits for the end of the modules.*

<a id="dec-613-2"></a>

**DEC-613.2. A binding verdict holds only where scratch reproduced every
candidate for the names the surface bound; everything else is unresolved
rather than guessed (#613).** Equal bindings prove nothing if the target has a
candidate scratch lacks: the target would pick it on the next creation. The
candidate sets are derived from what scratch bound — each bound relation,
routine, type, operator, collation or operator class/family name, looked up in
every schema of the deployer's effective path as the analysis scope measured it
(`pg_catalog`, then the write path's schemas it may use; an extra without
`USAGE` is not searched) and in the schema it bound into; a bound type name
is looked up as a routine too (only overloads a single argument can reach,
since a cast has one), and a bound routine a one-argument call can
reach (its declared count less defaults, or variadic) as a type, since `t(x)` is an exact call, else a cast to `t`, else the
best-matching call, and a cast takes exactly one argument —
plus every cast, which routine, operator and coercion resolution consults with
no name, for a surface that looked any of those up. Only a binding some
name lookup selected counts. Every type, operator, routine and relation field the node allowlist admits is classified by node and field:
an operator's implementation, result and transition types, a column's, field's
or placeholder's type, operands' common types, a query's output types, an SQL
value function's fixed type, a grouping's inferred operators and a derived
collation follow from other parts of the tree, which are compared themselves.
A constant's or coercion's type, a written column definition and a named
operator or routine can be a name in the source, stored no differently when
they were not, so they count. The same holds for a call's form: a stored tree
does not say whether `foo('x'::text)` matched `foo(text)` exactly, which a
type `foo` cannot displace, or was written `foo('x')`, which it can; nor
whether `1` was written `'1'::int4`. Both forms are therefore treated as
looked up, and a same-named type the target alone holds leaves either
unresolved. This is a known limitation, kept on purpose: the written form is
not in the catalog, the engine exposes no raw parse tree (measured on 16 and
18), and recovering it would take a SQL parser (ADR-0016 refuses grammar work)
or probe types planted on scratch. The conservative answer records nothing
wrongly, but it does block a plan whose affected surface only an exact call or
a cast constant binds, since an unresolved verdict prevents a deployable plan
(SPEC §9.3.2). The remedy is the operator's: remove or rename the unmanaged
type the target alone holds (#1062). A declaration the plan leaves unchanged has the same text on both sides and
resolves the same names; only the selected objects can differ, and that is
what the bindings carry. On the target each member must be an engine object
scratch has with the same properties, role references removed because the
bootstrap superuser's name is the installation's; or a member of a user schema
that is the project's own: one of its managed objects by name (a type only as
a table's or view's row type, never for an index, and never an array type,
whose generated name an earlier type may hold), or an object the target records, through
an internal dependency, as made by one (an identity column's sequence, a row or
array type). An automatic dependency does not say that, since any index has one
on the columns it covers, and neither does a constraint's index, since an
unmanaged constraint on a managed table makes one; an unnamed key's index is
therefore not accepted. The same identity on scratch is not
enough: an unmanaged routine the plan creates again, or an unmanaged sequence
that took an identity column's generated name first, shares scratch's identity
without being what scratch made (#1041). A routine is managed only as an exact overload:
the one scratch compiled for a declaration the plan keeps, or, for one the plan
drops and so never compiles, the routine its declared signature names on the
target, found with the engine's own signature lookup (`to_regprocedure`) under
the routine schema's write path, as the plan's `DROP` resolves it, inside the
target capture's own snapshot so that it is read coherently with the
candidates it is assessed against (#1148), and only
if it is of the declared kind: an aggregate holding a declared function's
signature is not that function, and the plan's `DROP FUNCTION` refuses it
(#1126). A declaration whose kind the plan changes under the same signature is
dropped as its old kind, and the target's routine is held by that identity,
not by what scratch compiled for the new kind (#1182). On the
target and not on scratch, because scratch holds the desired namespace, where
a type the plan adds can shadow the one the `DROP` finds and a table's array
type keeps the name an unmanaged type holds on the target (#1124). A signature
that names no routine there holds nothing. Counting the dropped overloads instead let an unmanaged overload stand
in for a declared one the target had lost (#1063). An unmanaged overload
beside a managed one, a routine planted
in `pg_catalog` and a built-in cast whose context was changed each leave the
surface unresolved, measured on both majors; so does an extension's object,
because nothing but managed declarations is reconstructed until retained
external input is handled privately (#617). Capturing whole schemas instead
would refuse on unmanaged objects no surface can reach, and would pull their
arbitrary closures into a capture that must refuse what it cannot qualify.
Runtime-bound bodies are compared by header only and are named, never counted
as proven by silence (ADR-0016 decision 3); one the plan creates is named too,
though the target has nothing to compare it with (#1064).

<a id="dec-1011-1"></a>

**DEC-1011.1. The SQL Server authorization context reads and replays grants on
database principals, apart from granted `IMPERSONATE`.** A grant on a principal
(class 4) decides what the deployer sees. For example, `VIEW DEFINITION ON
ROLE::Readers` makes that role's schema grant rows visible to a deployer that
is not in the role. Unreplayed, the reproduction saw fewer rows than the target
and reported `schema:<name>:grants` for an ordinary deployer.

- The rows are read for the same holders as the database grants: the
  deployer, its roles, and `public`.
- They are replayed after the principals exist. Each row is replayed under its
  own grantor, with each principal replayed as its own securable. Each
  principal is replayed under its owner, because the engine records that
  owner as the grantor of what `dbo` grants on it (measured on 17.0). A user
  owns itself. With the owner left as `dbo`, a row granted under a role the
  deployer is in needed a synthetic grant-option row for that role, which the
  deployer then saw.
- Ownership is also visibility with no grant at all: a role the deployer owns,
  or a role it is in owns, shows the deployer that role's grant rows. So the
  owner is kept for every role the context names, not only the roles these
  rows are on. It is read, restored on scratch with `ALTER AUTHORIZATION`, and
  compared as `principal:owners`. Only an owner other than `dbo` is kept.
  `dbo` owns every role scratch creates, so a database whose roles `dbo` owns
  keeps the context it had.
- They are compared under the key `principal:grants`.
- Granted `IMPERSONATE` stays out, because the impersonation list already
  replays it. A `DENY` of it is read like any other row.

Measured on 17.0: a `GRANT` on a principal makes that principal's row visible
to the grantee, and a `DENY` does not. So a principal named only by a `DENY`
has no kind the deployer can read. It is reproduced as a user, as every unseen
owner and grantor already is. A `DENY` shows the deployer nothing either way.

The field is left out of the canonical form when it is empty, so a context
that has no such rows keeps the fingerprint it was sealed with.

Pinned by `a_grant_on_a_principal_is_replayed_so_the_deployer_sees_what_it_sees`
(`crates/pbps-mssql/tests/support/resolver.rs`, live), and by the
`principal:grants` cases and
`a_context_without_principal_grants_keeps_its_canonical_form` in
`crates/pbps-mssql/src/resolver/authorization.rs`.

<a id="dec-614-1"></a>

**DEC-614.1. Persist a checked resolver contract and the final typed order, beside Schema.**
A resolver result cannot be an optional metadata bag: omitting it would let a
binding-dependent deployment be read as an ordinary plan. Saved-plan version 12
requires `analysis`, and resolved database provenance requires the complete,
transactional evidence variant. The model checks the versions, required scope
and absence predicates, logical-property closure, binding occurrences,
authorization step inventory, projected closing manifest and exact ChangeSet
order. Engine readers separately recognize the adapter, major version and
qualification rules they can enforce. This adds no resolver I/O to the differ
or pure dialect, and no transient observations to semantic `Schema` equality.

Persisted catalog properties, session and baseline observations use the target
environment's configured HMAC key (DEC-952.1), with its identifier alone in the
artifact. A distinct environment-key type excludes the process comparison key.
The PG sealing operation is private on a captured result: making it public
would let a recipient choose a known key and test guesses about private source.
The public operation instead performs a fresh authorized catalog read. The CLI
key loader refuses a missing key with the `pbps key generate` remedy. Private
source never becomes a serialized property map.

Measured on PostgreSQL 16 and 18, a new table's default cannot name an absent
routine, while an SQL-standard routine cannot bind an absent table. Splitting
new-table defaults, CHECKs and indexes into existing typed changes lets the bare
table precede the routine and the expressions follow it. ADD COLUMN DEFAULT
stays atomic because it fills existing rows. On a rebuilding dialect, module
replacement is explicit DROP/CREATE, with the ordinary differ's grant and
PUBLIC-execution obligations retained. Observed binding edges combine with
structural, identity, reference-data, authorization and restoration edges;
independent tables do not inherit a false dependency from alphabetical order.
A stable topological order refuses cycles before emission. Emitters and apply
receive that sealed sequence, never instructions to discover another order.
Ordinary planning still uses its existing ordering path. Table/column renames
connect current and desired expression owners only through their recorded UIDs:
teardown uses the old name, restoration the approved final name, and both keep
the final owner's strategy. Comparing surface names alone lost these dependent
rebuilds; PostgreSQL refused the resulting routine drop with `2BP01`.

*Amended by DEC-1274.1: the closing manifest keeps the plan's own records only
as identity-and-bindings placeholders, not as predicted properties.* The
expected closing manifest is projected from approved typed changes and
engine-observed ownership transitions. It preserves untouched external
properties and changes only the approved objects' properties, candidate
membership and lookup results; row changes cannot authorize a catalog change.
Schema grant transitions have their own namespace surface. An unprovable
transition is a planning refusal, not an apply-time choice. Qualified lifecycle
and ownership/binding inventory production remain #615, transactional checking
#616 and plaintext handling #617. Until the apply guard exists, the CLI refuses
resolved deployment artifacts before executing changes.


<a id="dec-614-2"></a>

**DEC-614.2. Check transitions against independent per-record ownership facts.**
A transition's surface name cannot authorize every identity in its inventory:
a valid view transition could carry a retained external prerequisite in both
sets and replace the target's fingerprint with the scratch fingerprint. The
closing manifest would then reject a valid deployment or accept that unrelated
drift. PostgreSQL 16/18 measurements distinguish a view's internally owned
rule/row type from its normally referenced table/function; replacement and drop
leave those external properties and objects intact.

Each prerequisite therefore requires a typed ownership observation, separate
from the proposed transitions. Projection checks every opening record against
the opening manifest and every closing record against the compiled manifest.
Only the named surface, its proved table/column parts, or owners related by
explicit typed renames may participate. Logical addresses remain opaque to the
pure model; name/signature similarity does not prove ownership. The adapter
must qualify the mapping, including engine-internal records. Raw PG catalog
capture records ownership as unqualified: useful read evidence, no authority to
change a record. The complete qualified producer/mapping remains #615. No
optional default turns omitted or unknown ownership into permission.


<a id="dec-1302-1"></a>

**DEC-1302.1. A PG16 supplied server names a child-only storage profile and
proves major 16 on its qualified administrative channel.** The pinned PostgreSQL
16 image declares a VOLUME at `/var/lib/postgresql/data`: putting a tmpfs at its
parent leaves an anonymous host-backed child, which both the daemon and the
complete kernel table must refuse. A real Docker 29.8.1 measurement instead
mounted one fresh tmpfs at that exact child, with root `/`, rw/nosuid/nodev/noexec,
UID/GID 999 and mode 700. The engine reported `160015` and data directory
`/var/lib/postgresql/data/run-data`; the daemon had no volume/bind Mounts and
owned cleanup was confirmed. This storage observation is not resolver evidence
or the held producer's supplied acceptance.

`linux-dedicated-pg16-v1` therefore names that exact child layout for PostgreSQL
only. Its fixed bootstrap uses `/usr/lib/postgresql/16/bin`. Admission first
selects the explicit name and Driver, preserves process separation and every
daemon/kernel control, then reads the numeric version through the already
qualified administrative StreamConn. Only `160000 <= server_version_num < 170000`
meets the new profile. A basename or image tag cannot supply this observation;
unreadable/null/query-error results refuse through the existing admission
cleanup, retaining any unconfirmed forwarder name. The existing sealed query
primitive lets the numeric engine reader answer without exposing transport
internals or adding version to InstanceObservation.

Both existing `linux-dedicated-v1` layouts remain unchanged: PostgreSQL's parent
storage and SQL Server's storage, identities and bounds acquire no new version
query or requirement. Every later runtime/channel check retains the selected
profile and full current mount table; there is no accepted prefix, second
writable storage mount or skipped volume row. Saved readers recognize the exact
new name as a known contract, never an unknown name or an admission substitute.
The fixed fixture selects pinned PG16/PG18 recipes explicitly, defaults to 18,
and retains the ordinary SQL Server recipe. This does not admit a Podman-native
daemon, arbitrary layouts, publish the held producer or enable resolved apply.

<a id="dec-1274-1"></a>

**DEC-1274.1. Seal the resolved producer's exact observations under its selected environment key.**
The key is fixed before the authorized fresh target and scratch reads. The
producer keeps the captures that justified the binding verdict, seals them
inside those read boundaries, and returns no capability to rekey a retained
capture. Managed ownership is proved per catalog record from recorded UIDs
and kind-specific dependency rules; a reference or automatic dependency alone
never authorizes a transition. The resolved evidence format is version 2: its
opening target-environment fingerprint covers complete raw `CatalogFacts`, and
its closing fingerprint covers the approved grant projection of that same
observation. Both use the same versioned canonical input and HMAC component,
so a target-only recheck can compare each phase directly. The final ordered
grant changes must match the projection request or production refuses.

Authorization metadata is not a binding input, and the producer does not
predict it. Measured on PostgreSQL 16 and 18, revoking EXECUTE on the chosen
overload from PUBLIC, giving it to another owner, or revoking every privilege
on a referenced table and changing its owner leaves the view, default and CHECK
bindings unchanged. Revoking the deployer's USAGE on an earlier schema does
change the binding: name lookup skips that schema. Capture rule
`postgres-catalog-inputs-v2` therefore drops owner and ACL fields, and the
`pg_shdepend`, `pg_init_privs` and `pg_default_acl` rows, from every
prerequisite. The deployer's effective schema privileges stay in the
engine-computed authorization condition. The earlier design fingerprinted that
metadata, so every transitioned record needed a predicted closing owner, ACL,
grantor and shared-dependency edge, including the default privileges applied at
creation. That was a second implementation of PostgreSQL's permission rules: it
took most of this issue's commits and review findings, and each round surfaced
another catalog case to model and measure, such as empty default ACLs (#1304).
The same rule leaves a table's TOAST relation out of the capture, with its
index, columns and dependency rows. It is out-of-line storage, physical as the
table's relfilenode already is: no expression binds it, and its name embeds the
table's OID, so scratch compilation and the target never name it alike.
Captured, it was an unowned record that a dropped table still listed in the
closing manifest, and the valid drop failed its recheck.

A rebuild that would drop a routine grant
option is the ordinary connected rebuild guard's refusal
(`modules::before_a_rebuild`), not the producer's.

A foreign key's internal RI triggers are named after their own OID, so a
created or dropped key's triggers could never match between scratch and
target. Measured on PostgreSQL 16 and 18, each has an internal dependency on its
constraint, and its relation, constraint and trigger function are unique, a
self-referencing key included. An internal constraint trigger is identified by
those three, its name is neither a property nor part of its rendered definition,
and it belongs to the table surface that owns the constraint.

The closing manifest does not predict what the plan changes. Scratch reproduces
only what the declarations say; the target also keeps what they do not: names
PostgreSQL generated when the table had another name (an unnamed primary key
and its index, an identity sequence, a PostgreSQL 18 NOT NULL constraint; a
rename keeps all of them, measured on 16 and 18), its physical column order,
storage and compression settings, a column's stored missing value, relation
options. Each review round of this issue found another such property, because
the projection took the plan's own records from scratch and had to correct them
one case at a time.

The closing check exists to catch a concurrent change to an input the plan did
not change, a wrong resulting binding, and an authorization change (ADR-0016
case 22). So the closing manifest keeps every prerequisite the plan does not
change with its full fingerprint, and keeps each record the plan installs only
as a placeholder: identity, ownership and bindings, no property fingerprint
(`MANAGED_CLOSING`), and only when it carries bindings or a kept record,
candidate member, runtime limitation or signature lookup names it. Candidate
membership still adds the plan's own members, which are declared names. An
installed record nobody names, such as an engine-named key or sequence, is left
out. Its declared properties are the ordinary managed revalidation's to check
at apply (DECISIONS 423), and a property no declaration states, such as a
storage setting, does not change a binding. The reader cannot see the compiled
records, so it checks the opening side of every transition and that the closing
manifest is exactly the untouched opening records plus placeholders of
installed records; the closing-side inventory and ownership checks run when the
plan is sealed. The closing manifest's read scope lists every record it holds as
a retained root. An untouched record can lose the only expression that reached
it, as `count` does when a routine's new body no longer calls it, and the
closing read must still reread it rather than miss it.

Every desired surface that binds something at creation needs its binding
verdict, including one the target does not hold yet: one the plan creates, or
relocates through a parent rename. Nothing on the target compares with it, but
its creation binds against the target's candidates, so the assessment checks
that scratch reproduced them, as it does for an existing surface, and calls the
surface `Created` when it did. An unmanaged overload scratch did not
reconstruct leaves the new surface unresolved, and the plan is refused instead
of sealing scratch's fallback as the binding. A surface that binds nothing at
creation, such as a routine whose string body binds only at run time, has no
lookup a candidate could change, and needs none.

The table is the unit of the closing inventory (#1466). One DDL statement
reaches past the surface it names: a retype rebuilds the keys and indexes over
the column, ADD or DROP DEFAULT flips the column's own `atthasdef`, and a rename
rewrites another table's foreign key, its RI triggers and their dependency
rows. Each catalog change therefore carries, besides its exact per-surface
inventory, the rest of every table it touches: the table's whole owned tree,
and every record a dependency row ties to it or whose identity names one, to a
fixed point. The adapter proves each tie. The model accepts such a reference
only on a table-family transition that a catalog change touches, and only for a
record that a table-family surface owns. A view or routine tied to the table
keeps its own transition or its full fingerprint, and an unqualified record
never rides. Referencing still confers no authority anywhere else (DEC-614.2).
The trade-off: a concurrent writer that changes an undeclared, non-binding
property of an untouched sibling inside a touched table is no longer caught by
the closing recheck. Such properties are already the operator's, and the
ordinary apply guard still compares every declared one (SPEC 7.6).

A GRANT, REVOKE or PUBLIC execution change makes no transition. Owners and ACLs
are not fingerprinted, so its target's catalog record does not change: it stays
an untouched input with its full fingerprint, and a concurrent change to it
still fails the closing recheck. Whether the grant took is the ordinary apply
guard's to check: it compares every declared grant the plan touches (SPEC 7.6).
The authorization condition covers only the deployer's schema authorization,
which is what name lookup depends on; an object grant does not change a
binding.

<a id="dec-1498-1"></a>

**DEC-1498.1. The resolver's ordering graph tells tables apart by recorded UID, and an observation keeps each side under its own spelling.**

A plan can give a table's name to another table: it drops `app.a` and renames
`app.b` to `app.a`. The graph's same-table rules (an expression change follows
its table's rename; a removal precedes its table's drop; a removal precedes the
restoration it makes room for) compared names, so the renamed table's check
removal read as a change to the dropped table, and the three rules closed a
cycle no order has. Each step's table is now its recorded UID. A step of the
ordinary plan spells a table as it is named where the differ placed the step,
so the UID is that of the latest earlier rename to the name or creation under
it, and otherwise the base table's. A binding rebuild the resolver appends
after the ordinary plan does not follow its position: it spells its teardown by
the base name, so that the teardown can precede the table's rename, and its
restoration by the final name. Its UID is therefore the base table's for a
teardown and the desired table's for anything else. Read by position, the
teardown of the old `app.b` in a rename chain (`app.b` to `app.c`, then `app.a`
to `app.b`) would belong to the table that took its name. A rule fires only
when the names match and, where both UIDs are known, the UIDs match too. This
only removes the false edges between two tables that share a name in turn; it
adds none.

The observation edges read the same owners. An observation names its opening
record by the base spelling when the base holds the surface, and otherwise by
the final one. A step releases or makes it only if the step changes that table
by UID, and a release may be spelled by the surface's final name: the renamed
table's check removal, spelled `app.a`, releases the `app.b` observation, and
the dropped `app.a`'s observation does not claim it.

The observations need no such re-keying (#1499). A `SurfaceResolution` holds
the opening record under the surface's base spelling and the compiled record
under its desired spelling. When names swap, one spelling therefore pairs two
tables. That is the contract coverage checks: the base surfaces under their
removal spellings, and the desired surfaces as they are. The rebuild check
reads it through recorded UIDs, comparing the opening record of a base surface
with the compiled record of the surface it becomes (`forward`). Pairing the
records inside one observation by UID would make that check map twice.

<a id="dec-1514-1"></a>

**DEC-1514.1. A connected plan finds the target's engine service from its own connection, and names no PID in configuration.**

`NativeTarget::establish` binds a target to one service process. Until #1514,
only test fixtures supplied that PID. A configured PID or pid file was the
obvious source, but it would let one setting point the binding at a process
other than the one serving the connection. `NativeTarget::connect` therefore
derives the service from the verified connection:

- the loopback pair's server end in this host's TCP table;
- the one process holding that socket, read from the host init's PID
  namespace, so its PID is one this observer can capture;
- that process's ancestors, while they run the same executable.

`establish` then verifies every premise of the result, as before.

Each failure is named and refuses:

- a target reached other than over loopback is not on this host;
- no holder means this host does not run the target;
- more than one holder leaves the service unidentified;
- an unreadable process table needs the observer's permission.

None of these reads as "no resolver needed". The target must be on the same
host, which is the premise RESOLVER-RUNTIME already states; a remote target
needs its own measured profile.

`resolver::server::produce` is the one production run (#615 sub-issue 1):

1. bind the target;
2. open the profile's scratch run, a supplied server or a container on the
   native daemon's `/var/run/docker.sock`;
3. produce the sealed order and evidence;
4. close the run before any result leaves.

A refusal reports either what the run released or the exact names whose
cleanup was not confirmed. The supplied path does not retry a server that is
still settling into exclusivity; that refusal is the profile being
unavailable. Connected planning does not call `produce` yet: #1515 adds lazy
acquisition.

<a id="dec-1540-1"></a>

**DEC-1540.1. The daemon socket check reads the descriptor that held the socket last time first, and only its naming the socket answers yes.**

RESOLVER-RUNTIME checks the daemon's ownership of the accepted socket on every
API request and every poll of an attach stream. One resolver run checks it
about 17,000 times. Each check read dockerd's whole descriptor table, about 60
entries on a CI runner: one run spent 5.7 s of its 25 s there (#1540).

Checking less often was the obvious saving. It would change the premise:
traffic would no longer be checked on every poll. Instead, each check reads
first the descriptor number that held the socket last time.

- If that descriptor still names the socket, the daemon holds it, which is
  the same yes the whole table would give.
- Any other reading of it falls back to the whole table, as before. That
  covers a closed descriptor, or a number reused for something else.
- The table also finds a socket the daemon moved to another descriptor, and
  that descriptor is remembered next.

The remembered number is never an answer by itself.

The process checks around the read, and the sock_diag peer check, are
unchanged.

<a id="dec-1550-1"></a>

**DEC-1550.1. A native run checks its premises per request and per step, not on every read: the attach stream before each write, the target's sole holder once per check, the scope once per resolution.**

#1540 measured a producer run at about 25 s, about 17 s of which went to
re-verifying the same premises. DEC-1540.1 made each check cheaper without
changing what it establishes. This entry lowers how often three of them run.
Each change gives up a short window, between two full checks, in which a
change that appears and disappears again is not seen.

- **The attach stream's daemon end is checked before each write, not on each
  read.** A read is the answer to a write that was checked. The bulk of a
  capture arrives in thousands of reads; a run makes about 175 API requests,
  so nearly all of its 17,000 daemon checks were stream polls. Giving up the read window requires dockerd
  itself to pass its end of the stream to another process mid-response, and
  dockerd is inside the provisioning trust boundary (RESOLVER-RUNTIME).
  Opening the stream and every API request are checked as before.
- **A target check observes the socket's sole holder once, before the
  identity query.** After the query, it confirms only that the socket is
  still the same inode and the retained owner still holds it, reading the
  descriptor that held it as DEC-1540.1 does for the daemon. Only that yes
  answers; anything else is the full observation, as before, which also finds
  a socket held in a worker's own descriptor table. A target witness checks
  the same way. The window given up is a
  second holder that appears for the length of one identity query and is gone
  at the next check's opening scan. Producing that needs root on the host, and
  the holder scan was never exhaustive (DECISIONS 533). In the CI fixture the
  service's PID namespace is the host's, so each scan walked every task on
  the host; a run made 60 of them.
- **A resolution re-qualifies the scope once, after its last read and before
  its outcome is sealed.** The steps before that, after compilation and after
  the desired capture, check the runtime, the channels, exclusivity and the
  target binding, as every check does. They publish nothing, and a scope
  change that is still in place when the last check runs refuses the run as
  before. Before compilation the scratch session re-enters the deployer
  `qualify` sealed, so the compilation's role does not depend on the skipped
  requalification. An explicit check still re-qualifies every time.

What stays caught is every change that is still in place at the next full
check: a restarted process fails its lease, a restarted engine its identity
query, a replaced connection its cookie or TLS session. Under #1528 the
measured profiles are the advanced tier, so this cost falls only on users who
choose them.

<a id="dec-1559-1"></a>

**DEC-1559.1. What a session this run retired still holds stays this run's
until it leaves: its forwarder's tasks, matched by the PID namespace its
guard's lease holds open; its two socket ends; and its engine session row.**
Within a run, a step finishes with its administrative or scratch session and
retires it. The connection is dropped at once, but three things go away only
when the processes holding them exit:
- the forwarder's tasks in the engine's network namespace (the container
  itself is removed at cleanup);
- both ends of the session's socket in the engine's TCP table;
- the backend in the engine's session list.

Each check knew only the live sessions. So whenever the run's next check came
before those processes exited, it read the run's own leftovers as an intruder
and refused for good. On CI this showed up as `Containment(Accounting)` on the
supplied producer tests. For an afternoon it failed most runs of
`resolver (pg, second)`, `master` included (#1559).

Retiring now records the session's guard lease, its socket pair and its
session key. Each check treats them as this run's own:
- The network census accepts a task whose PID namespace is a retired guard's
  namespace. It is matched by identity alone, without the liveness check a
  live anchor gets, because the guard may already have exited. That is still
  exact. The lease holds the namespace file open, so the namespace is never
  freed and its identity cannot be given to anyone else's. And a PID
  namespace whose init has exited admits no new process.
- The socket census skips a retired pair's inodes. A retired pair is neither
  foreign nor required, so a live session's missing end is still
  `MissingChannel`. Socket inodes come from the kernel's running counter and
  are not reissued within a run's lifetime.
- The session list must still report every live session. Any further key must
  be a retired one.

Both the supplied and the container profiles retire this way and record the
same things. The trust argument for live forwarders (#681) carries over
unchanged.

Waiting in the fixtures was rejected, as was having the product retry the
refusal. The refusal was the product misjudging its own run, the same check a
user's `plan --db` makes after the same retirement. The accounting part was
the observed failure. A review of the first fix showed that the socket census
and the session list are the same race one gate later.

The tests:
- `a_retired_session_may_linger_but_never_answers_for_a_live_or_foreign_one`
  holds the session-list rule.
- `a_retired_forwarder_still_in_the_engines_network_is_this_runs_own` uses
  the dedicated-server fixture. It sets a session aside exactly as retiring
  does, but holds the session's stream open, so everything it holds is
  certainly present. The test checks that the network census and the socket
  census each refuse without the retired record, and that the run's whole
  check passes with it.
<a id="dec-1515-1"></a>

**DEC-1515.1. A selected resolver runs only when the lightweight assessment needs it; with none selected, ADR-0013 keeps deciding.**

SPEC 9.3.2 refused deployable output whenever a required resolver was
"unavailable", which reads as including "not configured". ADR-0016 also says
existing protections stay until a tested replacement exists. The maintainer
decided (2026-09-28, on #615) that only a **selected** resolver can refuse a
plan. Refusing every plan that raises a binding question would refuse almost
every project with a view the moment a column arrives, for want of a tool it
never asked for. Connected planning therefore has three cases, separate in
code (`resolution::Case`), output and tests:

- **Case 1: no resolver selected.** The assessment does not run. ADR-0013's
  candidate rebuild and the unmanaged-dependent gate decide, the plan stays
  `PlanAnalysis::Ordinary`, and the report carries no resolver field, so
  nothing claims a resolver answered.
- **Case 2: selected, not needed.** The ordinary plan, with nothing the
  resolver owns opened: no fingerprint key, Docker socket or scratch
  connection. The summary keeps `resolver_selection.status: not_acquired`
  and adds `resolver_assessment` with `need: not_needed`.
- **Case 3: selected and needed.** The key is read first, so a missing one
  costs no container. Then `resolver::server::produce` runs. The answer
  splits as DECISIONS 485 and SPEC 9.8 do:
  - **Findings (exit 2):** an unresolved declared surface, an analysis scope
    measured incompatible (`Error::Incompatible`, split from an unreadable
    one), a supplied server measured short of its profile, an engine with no
    binding adapter (SQL Server), a selected profile whose runtime this host
    cannot provide, and a missing key.
  - **Unanswerable (exit 1):** connecting, acquiring, starting, an
    unreadable scope and an unconfirmed cleanup.
  - **An answer** is not yet published: `resolver.publication-unavailable`,
    writing neither file, until #1516 adds the fresh recheck.

**The assessment** is a pure function of the typed plan
(`pbps_diff::resolver::assess`); it reads no catalog. The questions are the
surfaces the target holds and the plan keeps.

- **Rebuild:** a surface the plan recreates from its own declaration,
  including one whose table or column it replaces under a new recorded UID.
- **Unaffected:** every other surface, but only when no change in the plan can
  move a name lookup. Such changes are relations, columns and their types,
  index names where they share the relation namespace (PostgreSQL, not SQL
  Server: DECISIONS 453), modules, and schema grants (a lookup skips a schema
  its role may not use). The list is an exhaustive match, so a new kind of
  change is classified rather than defaulted.
- **Resolve:** every other surface when some change can.

The maintainer chose this coarse rule (2026-10-05) over narrowing by path,
dependency or name. Each narrowing needs reasoning ADR-0016 leaves to the
engine: a qualified overload call, column notation, a relation-namespace
arrival. A wrong narrowing is a silent wrong answer; a wide one costs a
resolver run the project opted into by selecting one.

**ADR-0013's candidate rebuild of an unchanged module is a question, not an
answer.** Read as proof, it would answer the very question the resolver
exists to ask. On the resolver's path the differ therefore adds no candidate
rebuilds (`Rebinding::Evidence`). `pbps_diff::resolver::plan` and the
producer's scope request share that plan (`resolver::ordinary`). A module is
rebuilt there only when the evidence shows its binding moving, or a managed
input being replaced. This is the motivating pair: the irrelevant arrival no
longer rebuilds the view, so its unmanaged dependent no longer refuses the
plan, while the view whose binding moves is rebuilt. Ordinary planning keeps
the candidate rule unchanged.

**The seam is profile-neutral** (#1528). The cases, the assessment, the key
and the refusal classes hold for every profile; only the producer behind
`resolution::resolve` is profile-specific. Today's producers are the measured
profiles, which bind the target by observing its engine service
(DEC-1514.1); that same-host premise is theirs, not resolution's. An
operator-trusted profile plugs in beside them without reshaping the cases.

<a id="dec-1528-1"></a>

**DEC-1528.1. Selecting a resolver gives the operator-vouched resolver; the
measured profiles are frozen.** The measured profiles were built to prove
their own isolation. That needs root, a same-host engine observed through its
socket, executable identity and verified channels, and it still fails on any
existing database whose managed objects reference anything outside the
managed set. #1616 measured three real schemas: no tier answered any of them,
because the scratch compile itself failed. So the default must be one an
ordinary CI runner or laptop can use, and it must answer partial adoptions.

**Three tiers.**
1. No resolver: ADR-0013's conservative rebuild. This stays the default
   (ADR-0013).
2. The operator-vouched resolver: the selection whenever a resolver is
   selected without naming a profile.
3. The measured profiles: frozen now, and to be removed by #1636 once the
   operator-vouched resolver covers every path.

**The operator vouches for what pbps does not measure:** the scratch's
isolation, the confidentiality of what it compiles, and its channels. Both
connections use whatever TLS the operator configured. The evidence names the
profile, so a reviewer can always tell a vouched answer from a measured one.

*Amended by [DEC-1672.1](#dec-1672-1): the target is told from scratch by the
engines rather than the strings, a scratch on the target's cluster needs a
confined account, and the account decides the layout. The cluster
identifier read below needs no privilege after all.*

**It keeps exactly two separation checks.** The first runs before the
run-owned database is created; the second reads that database once it is
created, before any other DDL.
- **Scratch is never the target:** its host, port and database are not all the
  target's, its credential variable differs, and there is no fallback to
  target credentials. That is the mistake an operator can make by accident,
  and it is cheap to catch.
- **Scratch is empty, read in the run-owned database as created rather
  than in a template** (scratch is cloned from `template0`, not `template1`): a
  polluted scratch changes what the declarations bind to.

A third check, that scratch is another engine instance (`system_identifier`),
was dropped. It needs a privileged read, and it guards against a choice the
operator already vouched for. So a scratch database on the target's own
cluster is allowed.

**The shared compatibility qualification is not one of the things vouched
for** (#610, #611). It compares the facts both engines report: version and
build string, extensions, encoding, collation and the deployment context.
These decide whether scratch binds a name the way the target would. That
is a question about meaning, not about isolation, and every reported fact
can be compared. So it runs for the operator-vouched resolver as for every
other, and an incompatible or unreadable fact refuses (#1652 review).

What it cannot compare is executable content. Reading it needs the
observed process (DEC-1514.1, DECISIONS 520), and the target may be remote.
So two builds that report the same facts are taken to bind alike, for
example a same-version build with a parser hook. That much is vouched for
(#1657).

**The scratch database takes the target's encoding and locale.** The encoding
decides how a name is cut to the 63-byte identifier limit (#1627, #1640). A
scratch in another encoding would answer for another database. This is not a
user option.

**Objects outside the managed set are staged from a reviewed baseline,
compared with the target.** Leaving them out (managed-only) fails every
partial adoption, which is #1616's finding. Reconstructing them by reading
their full definitions sends routine source no reviewer approved.
- The baseline is a SQL file in the repository. It runs on scratch only.
- **It runs whole and first, before any managed object is staged** (#1664).
  It runs as the setup role, in its own session.
  - An earlier draft interleaved it with the managed declarations, so that
    an external routine could take a managed table's row type. That forced
    a confined role to keep the baseline away from objects already staged.
    Review then found, one round at a time, a legitimate external object
    the role could not create: a foreign key, a routine over a managed
    type, a cast, an operator or aggregate. Each needed one more privilege,
    and each grant opened a path back into managed state.
  - Run first, the baseline meets no managed object, so no privilege rule
    is needed, and any SQL may be written.
  - The same order is its contract: a baseline's statements name nothing
    managed. A legacy view over an adopted table is written as a shape
    view. An object whose own shape uses a managed type is left out:
    scratch needs it only in the chain below. A statement that fails names
    itself and the remedy (#1652 review).
- **The cost is a chain through the boundary.** A managed object binds to an
  external one whose compared shape names a managed object: a column,
  attribute or argument of a managed type, a managed parent relation, or a
  cast over a managed type. Such a chain
  refuses, naming it, with two remedies: adopt the middle object, or select
  no resolver. A view's query and a routine's body never form a chain, since
  neither is compared; a view over managed tables is staged as a shape view
  (#1652 review).
  - Measured on pagila, AdventureWorks and GitLab: no adoption split by
    kind (tables first; tables and types; views and routines) or by schema
    produced one. Only foreign keys and triggers pointed back, and neither
    is compared.
  - Random half-splits do produce them, through columns typed with a
    shared managed domain.
  - Automatic fill, the third step, generates each object itself. It can
    order them between managed objects from the target's `pg_depend`,
    without parsing SQL, which lifts the refusal for what it fills.
- **Every object it creates is compared with the target, by what a
  creation-time binding can read.**
  - Compared: an object's own class properties as the manifest
    fingerprints them; of a relation's children, its columns, the
    primary-key and unique constraints and indexes a `GROUP BY` or
    `ON CONFLICT` relies on, and its inheritance and partition parents, which
    a row-type coercion such as `c::ext.parent` relies on (#1652 review).
  - Not compared: foreign keys, CHECKs, defaults, triggers, policies, rules,
    non-unique indexes, nor the source text of a routine's
    body or a view's query.
  - So a baseline may omit what only guards or computes, and the common
    back-pointing foreign key and trigger need not be staged. Earlier
    drafts listed compared properties of their own and missed one per
    review (routine defaults, column collations); the manifest's
    definition, cut to what binds, ends that. The recheck still compares
    the target's complete fingerprints.
- **A view is staged as a shape view** (its output columns over typed NULLs,
  returning no row), **never as a table.** A table's system columns change
  what a name binds to. Measured on 18: `v.xmin`, with a function
  `xmin(ext.v)` in scope, binds the function on a view and on a shape view,
  but the system column on a table of the same columns. This replaces "a
  view becomes a table of its output columns" in the design on #1528; the
  point, shapes only and never the original SQL, is unchanged.
- **Everything the baseline leaves behind is accounted for.** An uncompared
  object, a mismatch or a baseline object in the managed set refuses. The
  compatibility qualification reads scratch after the baseline, so a
  setting it changed must match the target too. A wrong or stale baseline
  therefore refuses instead of answering for a database that does not
  exist.
- pbps itself never sends routine source. A routine reaches scratch only
  through the baseline, under what the operator vouches for.
- Two later steps reuse the same comparison: a reviewed draft generated from
  the target (shapes only; a view as a shape view of its output columns), then
  automatic fill of missing shapes, where the baseline wins on overlap.

**Naming.**
- Users see the operator-vouched resolver as plain "resolver".
- SPEC, ADR, DEC text and code always call it the operator-vouched resolver,
  `vouched` (the `Vouched` variant, tests prefixed `vouched_`).
- The frozen tier is "the measured profiles". Containment, process and socket
  observation, executable identity and verified channels belong to them
  alone.
- Text written before this entry that says "resolver" for those means the
  measured profiles. That covers SPEC §9.3.2 and ADR-0016 as first written,
  and issues and entries such as DEC-1514.1, #1541, #1542, #1404, #1411, #617,
  #619, #620 and #1381.
- A reused bare word would let a later reader carry a measured guarantee over
  to a profile that makes none.

Recorded in SPEC §9.3.2 and ADR-0016's amendment. The design and the
maintainer's decisions are on #1528. Pinned by the acceptance tests ADR-0016's
amendment lists, which land with the producer.

<a id="dec-1672-1"></a>

**DEC-1672.1. The operator-vouched resolver tells its scratch from the target
by the engines, and the scratch account decides the layout.** Implemented by
#1672 on the maintainer's decision on #1667.

**The target is told from scratch by the engines, not by the strings.**
- Two connection strings can spell one server two ways (`localhost` and an
  address, a DNS alias, a pooler), so comparing host, port and database
  answers the wrong question.
- **Each session marks itself.** Each sets `application_name` to a
  run-generated token, and the scratch session reads `pg_stat_activity` for
  both tokens. Any role sees another session's `pid` and `application_name`
  there; only its query and timings are hidden (measured on 16 and 18).
  - A target mark the scratch session sees, on the target's backend, is the
    same cluster. On the target's own database as well, it is the target, and
    the run refuses.
  - The scratch session must see its own mark. Otherwise the view shows
    nothing on that cluster, and "not seen" would read as "another cluster".
  - **Both sessions hold a transaction across the check**, and the marks are
    transaction-local. A transaction-pooling proxy releases a backend
    between transactions, so without one the scratch session could be handed
    the target's backend, overwrite its mark and read "another cluster"
    (#1678 review).
  - **The run keeps the scratch backend it checked.** The same transaction
    records that backend: its process ID, its start time and its
    postmaster's start time, which a session reads about itself. A
    connection keeps its socket, not its backend, so the supplied layout
    rechecks it in the transaction that holds its checks, and again before
    compiling. It refuses if the backend moved, and asks for a direct or
    session-pooled connection. The cleanup's `DROP OWNED` runs in one
    transaction with that check, so it is never sent to another backend
    (#1678 review).
  - The identity's times are compared as epochs. Their text follows
    `TimeZone` and `DateStyle`, which the compile pins, so text would make
    one backend read as two.
- **Rejected: a `pg_database` row with the target database's name and
  OID.** Two clusters started from one image with one `POSTGRES_DB` hold
  identical rows, which is an ordinary CI layout. That check would call them
  one cluster and refuse a valid setup.
- **Rejected: `system_identifier`.** A cluster restored from a physical
  backup, or an image with its data directory baked in, shares it.
  - DEC-1528.1 dropped this check because it needs a privileged read. That
    premise was wrong: `pg_control_system()` is executable by `PUBLIC` on 16
    and 18. It is still not the separation check, for the reason above.

**On the target's cluster, the scratch account must be confined** (#1667).
- Confined means nothing the account can do reaches outside its
  database. A compiled definition runs before the plan is approved, so
  whatever the account can do, a declaration can make it do (#1678
  security review).
  - **What it can use.** The privileges of every role it can `SET ROLE`
    to, itself included, of every role one of those inherits from, and of
    PUBLIC. `SET` chains start at the session user, so a role reached by
    `SET` and then inherited counts though neither `USAGE` nor `SET` from
    the login reaches it (measured on 16 and 18; #1678 review).
  - **Attributes.** Neither the account nor any role it can `SET ROLE` to
    has `SUPERUSER`, `CREATEROLE`, `CREATEDB` or `REPLICATION`. A
    replication slot holds WAL for the whole cluster (measured on 16 and 18
    for a non-superuser after `SET ROLE`). A role it can become is as good
    as holding the attribute. The attributes are never inherited, so a
    membership granted `SET FALSE` does not count: the login can neither
    become that role nor use its attribute (#1678 review).
  - **Predefined roles.** It can use no predefined role outside a short
    list whose privileges stay in the database or only read statistics and
    settings: `pg_database_owner`, `pg_read_all_data`,
    `pg_write_all_data`, `pg_maintain`, `pg_monitor`,
    `pg_read_all_settings`, `pg_read_all_stats`, `pg_stat_scan_tables` and
    `pg_use_reserved_connections`. Every other one is refused, a role a
    later version adds included, until it is known. A predefined role's
    privileges are inherited, so inheriting one counts: measured on 16 and
    18, an `INHERIT TRUE, SET FALSE` membership of
    `pg_execute_server_program` runs `COPY ... TO PROGRAM`.
  - **Shared objects.** No role it can use, nor PUBLIC, holds authority
    over a shared object other than the database it is in:
    - ownership of another database or a tablespace;
    - `ADMIN OPTION` on a role;
    - a grant option on a database or tablespace;
    - `ALTER SYSTEM`, or a grant option, on a parameter.

    Each lets a compiled definition write outside the database:
    `ALTER DATABASE` through an inherited membership of its owner, `GRANT`
    of the role, `GRANT` on the database (measured on 16 and 18; #1678
    review). A subscription is the exception: only a session in its own
    database can alter or drop it (measured), and one in this database is
    the emptiness check's. On another cluster none of these is the
    target's, so they are not checked there.
- Reproducing the deployer's authorization creates roles server-wide, some
  possibly `SUPERUSER`. On a shared cluster that is a write to the target's
  cluster, and only an account that cannot make it is safe there.
- An unconfined account on the target's cluster refuses before any write,
  naming the attributes and memberships to remove.

**The account decides the layout.**
- **Run-owned.** A login that is itself a superuser, on another cluster,
  gets the measured run's
  layout: a run-owned login and a database from `template0` with the target's
  recipe, the deployer's authorization reproduced, and a compile as the
  reproduced deployer. All of it is dropped afterwards.
  - Only a superuser. A `CREATEROLE` login is granted `ADMIN` on the roles it
    creates but not `SET` (the default `createrole_self_grant`). So it can
    neither hand them the database nor replay grants as them, and it cannot
    reproduce a superuser deployer at all (#1678 review). It takes the
    supplied layout instead.
  - Nor a member of a superuser role. `SUPERUSER` is not inherited and the
    run never `SET ROLE`s, so it runs without it. Membership still makes the
    account unconfined on the target's cluster.
- **Supplied.** Any other account compiles as itself in the database its
  connection names.
  - "As itself" is enforced: the first statement on every connection the
    run opens as the scratch login is `SET ROLE NONE`, the run-owned
    layout's provisioning connection included. Every step that switches role
    and back returns with `SET ROLE NONE` too, never `RESET ROLE`, which
    returns to the role the login's defaults name.
  - A connection the run opens as a login it created carries none of the
    operator string's startup `options`. Those are the operator's session
    settings for its own login: a `-c role=` there would be refused for
    the new login, and the new login gets the settings the run gives it
    (#1678 review). A role set by the login's defaults or its
    connection options would own what the run creates, out of reach of
    `DROP OWNED BY SESSION_USER`. It would also hide the session's own
    backend timings (#1678 review).
  - It must own that database.
  - The database must hold nothing initdb did not create: no object at or
    above `FirstNormalObjectId`, a subscription created in it included,
    and no large object at all. A large object's OID is its creator's to
    choose, so no cutoff applies, and initdb makes none (#1678 review).
  - **Every write goes through the connection the checks were made on**,
    the cleanup included. A second connection from the same string need not
    reach the same server: a name may resolve to several hosts, or to a
    balancing proxy. `DROP OWNED` there would empty something unchecked
    (#1678 review). The cleanup first ends a failed transaction and resets
    the role.
  - The deployer's role and database defaults become session settings. The
    path, the preload lists and any setting the login may not set are left
    for the comparison to report. So is a setting the session took from its
    startup packet: the target's session, through the same driver, gets the
    same override. The driver always sends `client_encoding=UTF8`, so a
    stored `LATIN1` never takes effect on the target either (measured on 16
    and 18; #1678 review).
  - Each replayed value is an `E'…'` literal with its backslashes and
    quotes doubled. The replay runs before any framing pins
    `standard_conforming_strings`, and the login's own default may turn it
    off. Under that, a plain literal's backslash escapes its closing quote,
    and the rest of a stored value would run as SQL (#1678 review).
  - No role is reproduced. A deployer that differs in schema visibility
    refuses through the compatibility comparison.
  - `DROP OWNED BY SESSION_USER` empties the database afterwards, whether
    the run answered or refused.
  - **Nothing outside the database is within the cleanup's reach.**
    `DROP OWNED` acts on the shared dependencies recorded on the login, so
    the run reads those (`pg_shdepend`) before any write and refuses,
    naming them, while any remains other than the run's own. These count:
    every entry in this database, and every shared entry other than
    ownership. A privilege or membership is revoked; a database, tablespace
    or subscription the login owns is not dropped. The login's own entry on
    the database it compiles in is the run's own.
    - Measured on 16 and 18, this one read covers each case review found
      one at a time before it: a grant to the login on another database,
      tablespace or parameter; a role membership the login granted, to
      another role or to itself; an object it owns here, a large object
      with a chosen low OID included (#1678 review).
    - Left out, as measured: ownership of another database, which
      `DROP OWNED` keeps, a grant the login made on another database's ACL,
      and a membership granted to the login.
- **Why ownership is required.** `DROP OWNED` also revokes what was granted
  to the login on the database, and only an owner keeps its rights through
  that.
- **`DROP OWNED` never runs as a superuser.** A superuser either provisions
  (another cluster) or refuses (the target's), so it never reaches this
  layout. Run as the bootstrap superuser, `DROP OWNED` would reach every
  object initdb made.
- **A run that dies part-way** leaves the supplied database non-empty. The
  next run then refuses on emptiness instead of compiling over the
  leftovers.

**The comparison is rule `pg-reported-scope-v1`:** every catalog fact
`pg-analysis-scope-v1` compares, and no executable.
- Evidence naming the vouched runtime must name this rule, and measured
  evidence the other. A rule that claims coverage the run did not have is
  refused by the artifact reader.
- **The build string, `server_version`, is recorded, not compared.** It
  names the packaging as much as the build: a managed or distribution-packaged
  target and a container scratch of one version report two strings
  (`18.6 (Debian 18.6-1.pgdg13+2)` from the official image). That is the
  common pairing, and refusing it would refuse most real setups. The version
  number, extensions, collations and settings, which decide binding, are
  compared. A packaging that patched name resolution apart within one
  version is part of what the operator vouches for. A differing string is a
  named limitation of the report and is in the build fingerprint; an
  unreadable one is unknown (maintainer's decision on #1678).
- The build fields carry keyed fingerprints of what each engine reports: its
  version number and build string, and its installed extensions at their
  versions.

Recorded in SPEC §9.3.2 and ADR-0016's amendment. Pinned by the `vouched_`
live tests in `resolver::server::vouched`.
