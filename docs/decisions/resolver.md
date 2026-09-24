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
ordering and #594's protected publication/apply/recording remain separate gates.
