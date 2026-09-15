# Docker resolver runtime boundary

This describes the internal runtime layer for #608. Named selection is exposed
by the CLI (#606); binding planning is still gated on the later #595 steps.
Acquisition and a live candidate are not environment compatibility, deployment
authorization, retained-source permission or sealed binding evidence.

## Supported initial profile

`linux-amd64-v1` uses a direct local Docker Unix API and a native Linux target
observer on the same kernel. The API's actual Unix peer must be a live,
root-installed Docker daemon, reached through a protected root-owned socket
path. A root-owned proxy is insufficient. The target adapter accepts direct
loopback verified TLS whose actual server socket belongs to the selected
engine service's process tree. A configured PID bounds inspection; it does
not assert identity. Database names, credentials and aliases do not establish
instance separation.

The initial fixed layouts are the tested PostgreSQL 18 and Linux SQL Server
images used by the fixture scripts. The selected image must be explicitly
trusted. Missing images obey `never`/`if_missing`; registry credentials stay
with Docker's configured credential helpers. Acquisition records Docker's
immutable content ID, registry digests and platform separately. It never treats
an image digest as proof of running mounts or loaded native code.

Unsupported or unreadable process, channel, platform and runtime premises
refuse qualification. Remote targets, arbitrary proxies, emulated runtimes,
rootless Docker and other layouts need their own measured profiles. The
observer needs permission to read the selected live processes' proc metadata,
executable and namespace handles, sockets, mounts and cgroup-v2 limits. This
is a read-only runtime observer, not a production agent or a target-side SQL
write probe. Native target verification does not invoke Docker; the factory's
Docker dependency belongs to scratch provisioning.

The host kernel, its administrators, the selected daemon and explicitly trusted
image installation form the provisioning trust boundary. SQL privileges in
scratch grant no authority over those external controls. This profile does not
claim protection from a compromised kernel or provisioning administrator.

## Startup and continuity

1. Acquire the selected image on the actual API connection. Another connection
   cannot reuse that acquisition's provenance for startup.
2. Reserve an empty private runtime with a root lifetime guard and a fixed,
   unprivileged bootstrap waiter. Remove inherited image environment variables
   before execution and disable inherited healthchecks. No database is created.
3. Qualify the actual target, namespace separation, empty external network,
   effective mounts and resource limits. Retain these process and kernel
   handles while releasing the fixed bootstrap command.
4. Await the engine's source-free readiness signal. Listening alone does not
   establish PostgreSQL readiness. A failed handshake discards the run; it
   never reconnects a partly qualified database session.
5. Open a fixed forwarder in separate PID/mount namespaces, sharing only this
   run's network namespace. Bind its one database connection to the actual
   backend and forwarder socket owners, authenticated daemon streams and target
   connection identity. Then recheck the measured runtime premises.

The lifecycle API exposes source-free identity, continuity and cleanup only.
Engine build/native-library compatibility and deployment context belong to
#610/#611. Binding adapters must finish that qualification and the applicable
source-handling gates before transferring declarations or consuming results.

Process, namespace, socket and connection handles remain live capabilities;
they are not serializable booleans. Kernel and engine identity checks surround
continuity reads. Cancellation takes the complete session state before awaiting
I/O, so a later restoration cannot revive it. Any lost control, replacement or
unreadable premise discards the connection and requests owned cleanup. Cleanup
may establish a separately authenticated connection only to remove the exact
owned resource; it cannot resume analysis. A fresh analysis starts from fresh
resources and qualification.

Scratch also retains a weak witness to the target's live binding and its actual
socket. Dropping or cancelling the target invalidates scratch's next identity
or continuity check; the scratch handle cannot keep that discarded connection
alive merely by retaining its opaque identifier.

## External containment

| Boundary | Enforced premise |
| --- | --- |
| Network | Only a private loopback device, no external routes, no published ports or host network; workload `connect`, compat `socketcall` and TCP Fast Open are denied |
| Control | Fixed loopback destination and program, separate PID/mount namespaces, one authenticated protocol session, no reconnect, bounded Docker framing and aggregate traffic |
| Filesystem | Read-only image root, limited private tmpfs storage, no host binds/runtime socket/devices, masked host proc/sys interfaces, no executable private storage |
| Privileges | Root deadline guard; engine-specific unprivileged UID and bounded capabilities; no-new-privileges; deny-by-default seccomp derivative |
| Resources | cgroup-v2 CPU/memory/swap/PID bounds, tmpfs bounds, file-descriptor limits and an independent root deadline |
| Cleanup | Locally generated ownership token plus immutable container ID; exact absence confirmation, including automatic-removal races; no label-wide deletion |

The control process alone can initiate the private database connection. The
workload cannot open a second session, acquire the forwarder's descriptors or
reach an external writer. Namespace isolation and the fixed bootstrap exclude
another session from the run's scratch scope, including a change-and-restore
sequence between fingerprint reads. These are runtime premises; image labels,
initial/final SQL hashes or the existence of one live socket do not replace
them. Native-library/parser-hook and engine-specific analysis requirements
remain separate qualification gates.

## Verification

`scripts/live-resolver.py <pg|mssql>` requires the explicit preloaded fixture
images and runs the Docker ownership, gate, actual kernel pairing, filesystem,
network and lifetime tests. Disposable internal-network listeners receive real
DNS and metadata-shaped traffic, and a private volume holds a synthetic host
file and mock runtime socket. The protected recipe reaches none; removing each
corresponding fixture boundary produces the expected effects. No real host data
or Docker socket enters those sentinels. The pairing inspector has a private PID namespace
and receives only read-only proc directories from its owned workload/control
trees. It receives no host PID namespace, runtime socket or external network.
The root-control inspector separately measures its one owned root guard and
read-only cgroup controls. These test access paths are not production adapters.

`scripts/live-resolver-target.py <pg|mssql>` creates disposable TLS targets and
confines each inspector to its owned target's PID/network namespaces. It checks
aliases, database/login changes, backend-child refusal, canceled bindings and
intermediate proxies. A TLS-terminating proxy deliberately alters a managed-only
query on a plaintext backend connection: the real client accepts the TLS hop
and engine result, while native qualification refuses the proxy. The database
transport fixtures also reject corrupted and replayed TLS application records.

The dedicated CI resolver matrix runs both profiles separately. On its
disposable native Linux runner, `live-resolver-target.py --native-host` also
exercises the complete public factory using a prebuilt test binary with root
process-inspection access. All required CI jobs, including this matrix, must
pass on the current PR head before merge. A component-only test run cannot
stand in for this complete factory check.

The fixed bootstrap's environment removal follows Moby's key-only override
semantics ([Moby environment implementation](https://github.com/moby/moby/blob/v28.3.3/container/env.go)).
The seccomp derivative retains its Apache-2.0 attribution in the adjacent
`NOTICE`; its additional network restrictions include the implicit connection
performed by [TCP Fast Open](https://man7.org/linux/man-pages/man2/send.2.html).
PostgreSQL readiness uses the engine's
[versioned PID-file status](https://github.com/postgres/postgres/blob/REL_18_6/src/include/utils/pidfile.h).
