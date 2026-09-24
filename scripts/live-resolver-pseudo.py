#!/usr/bin/env python3
"""Exercise real mqueue replacement inside disposable user/mount/IPC namespaces.

Build as the ordinary user first and pass the existing test binary. No Docker,
host mount modification, binary copy, queue contents, or root Cargo is needed.
The caller opens the normally built executable before entering the private
user namespace. A root caller retains permission to create that namespace on
hosts that restrict unprivileged UID mappings; its mounts remain private.
"""

import argparse
import ctypes
import fcntl
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile

TEST = "resolver::native::pseudo::tests::a_foreign_mqueue_mount_invalidates_admission_and_the_retained_lease"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test-binary", required=True, type=Path)
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--binary-fd", type=int, help=argparse.SUPPRESS)
    args = parser.parse_args()
    binary = args.test_binary
    if sys.platform != "linux" or not binary.is_absolute() or not binary.is_file():
        parser.error("requires Linux and an absolute existing prebuilt test binary")
    if not args.inside:
        owner = binary.stat()
        if os.geteuid() not in (0, owner.st_uid):
            parser.error("the fixture must run as the build owner or root")
        if args.binary_fd is not None:
            parser.error("the inherited executable descriptor is internal to the fixture")
        # Dropping root to the build owner makes UID-map creation depend on
        # unprivileged-user-namespace policy (the CI runner refuses it).
        # Open before mapping instead: the root-mapped child can execute this
        # read-only descriptor even when the user's build directory is private.
        # No binary copy, ownership change or host policy change is needed.
        descriptor = os.open(binary, os.O_RDONLY | os.O_CLOEXEC)
        try:
            source = Path(__file__).read_text()
            return subprocess.run([
                "unshare", "--user", "--map-root-user", "--mount", "--ipc",
                "--propagation", "private", "--", sys.executable, "-c", source,
                "--inside", "--test-binary", f"/proc/self/fd/{descriptor}",
                "--binary-fd", str(descriptor),
            ], pass_fds=(descriptor,), timeout=60).returncode
        finally:
            os.close(descriptor)

    # The initial user namespace maps the full UID range, never just one UID.
    fields = Path("/proc/self/uid_map").read_text().split()
    assert len(fields) == 3 and fields[0] == "0" and fields[2] == "1"
    assert os.geteuid() == 0 and os.uname().machine == "x86_64"
    descriptor = args.binary_fd
    assert descriptor is not None and descriptor >= 3
    assert binary == Path(f"/proc/self/fd/{descriptor}")
    assert fcntl.fcntl(descriptor, fcntl.F_GETFL) & os.O_ACCMODE == os.O_RDONLY
    signal.alarm(45)
    libc = ctypes.CDLL(None, use_errno=True)
    libc.syscall.restype = ctypes.c_long

    def call(number, *values):
        result = libc.syscall(ctypes.c_long(number), *values)
        if result < 0:
            raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))
        return result

    def mount_mqueue(target):
        context = call(430, ctypes.c_char_p(b"mqueue"), ctypes.c_uint(1))
        try:
            call(431, ctypes.c_int(context), ctypes.c_uint(6),
                 ctypes.c_void_p(), ctypes.c_void_p(), ctypes.c_int(0))
            mount = call(432, ctypes.c_int(context), ctypes.c_uint(1), ctypes.c_uint(1))
            try:
                call(429, ctypes.c_int(mount), ctypes.c_char_p(b""), ctypes.c_int(-100),
                     ctypes.c_char_p(os.fsencode(target)), ctypes.c_uint(4))
            finally:
                os.close(mount)
        finally:
            os.close(context)

    mounted = []
    child = None
    with tempfile.TemporaryDirectory(prefix="pbps-pseudo-") as temporary:
        foreign = Path(temporary) / "foreign-mqueue"
        foreign.mkdir()
        try:
            mount_mqueue(foreign)
            mounted.append(foreign)
            if libc.unshare(0x08000000):
                raise OSError(ctypes.get_errno(), "create the selected owned IPC namespace")
            mount_mqueue("/dev/mqueue")
            mounted.append(Path("/dev/mqueue"))
            assert os.stat(foreign).st_dev != os.stat("/dev/mqueue").st_dev
            env = dict(os.environ, PBPS_PSEUDO_FOREIGN_MQUEUE=str(foreign))
            if binary.stat().st_uid != 0:
                # In a root-mapped fixture the normally built binary is not
                # root-owned. Lease this fixed root-owned sleeper, never chown
                # or copy the shared build artifact to manufacture provenance.
                child = subprocess.Popen(["/usr/bin/sleep", "40"])
                env["PBPS_PSEUDO_TARGET_PID"] = str(child.pid)
            result = subprocess.run([str(binary), "--ignored", "--exact", TEST,
                                     "--nocapture"], env=env, pass_fds=(descriptor,), timeout=35)
            return result.returncode
        finally:
            if child is not None:
                child.terminate()
                child.wait(timeout=5)
            for path in reversed(mounted):
                if libc.umount2(ctypes.c_char_p(os.fsencode(path)), 2):
                    raise OSError(ctypes.get_errno(), "remove only the owned pseudo mount")


if __name__ == "__main__":
    raise SystemExit(main())
