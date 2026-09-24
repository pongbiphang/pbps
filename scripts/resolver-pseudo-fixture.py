#!/usr/bin/env python3
"""Replace only a marked fixture's cgroup/mqueue mount, then restore its root.

The helper holds the proc directory, start time, root and namespace handles
and authenticates the owned container before entering its mount namespace.
The foreign mqueue is newly created in an owned IPC namespace, never the host's
messages. Cgroup probes read identities only. No host mount is changed.
"""

import ctypes
import http.client
import json
import os
from pathlib import Path
import re
import socket as sockets
import sys

socket, container, pid_text, expected_start, kind, action = sys.argv[1:]
assert os.geteuid() == 0 and Path(socket).is_absolute()
assert re.fullmatch(r"[0-9a-f]{64}", container)
assert kind in {"cgroup", "mqueue"} and action in {"read", "replace", "restore"}
pid = int(pid_text)
assert pid > 1 and pid != os.getpid() and expected_start.isdecimal()
proc = os.open(f"/proc/{pid}", os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC)


def read_proc(name):
    fd = os.open(name, os.O_RDONLY | os.O_CLOEXEC, dir_fd=proc)
    with os.fdopen(fd) as stream:
        return stream.read()


def start_ticks():
    return read_proc("stat").rsplit(")", 1)[1].split()[19]


assert start_ticks() == expected_start


class Docker(http.client.HTTPConnection):
    def connect(self):
        self.sock = sockets.socket(sockets.AF_UNIX, sockets.SOCK_STREAM)
        self.sock.settimeout(15)
        self.sock.connect(socket)


connection = Docker("localhost")
try:
    connection.request("GET", "/v1.47/containers/" + container + "/json")
    response = connection.getresponse()
    assert response.status == 200
    record = json.loads(response.read())
finally:
    connection.close()
assert record["Id"] == container and record["State"]["Running"]
assert record["State"]["Pid"] == pid
labels = record["Config"]["Labels"] or {}
assert (re.fullmatch(r"[0-9a-f]{32,64}", labels.get("io.pbps.resolver.owner", ""))
        or labels.get("io.pbps.resolver.fixture") == "dedicated-server")
namespace = os.open("ns/mnt", os.O_RDONLY | os.O_CLOEXEC, dir_fd=proc)
ipc = os.open("ns/ipc", os.O_RDONLY | os.O_CLOEXEC, dir_fd=proc)
root = os.open("root", os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC, dir_fd=proc)
assert os.fstat(namespace).st_ino != os.stat("/proc/self/ns/mnt").st_ino
assert start_ticks() == expected_start
leaf = "sys/fs/cgroup" if kind == "cgroup" else "dev/mqueue"


def identity():
    st = os.stat(leaf, dir_fd=root)
    return {"device": st.st_dev, "inode": st.st_ino}


before = identity()
if action == "read":
    print(json.dumps(before))
    raise SystemExit(0)
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long


def checked(result):
    if result < 0:
        raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))
    return result


def call(number, *args):
    return checked(libc.syscall(ctypes.c_long(number), *args))


if kind == "cgroup":
    membership = read_proc("cgroup").strip()
    assert membership.startswith("0::/") and "\n" not in membership
    relative = membership[4:]
    assert relative and all(part not in {"", ".", ".."} for part in relative.split("/"))
    source = Path("/sys/fs/cgroup") / relative if action == "restore" else Path("/sys/fs/cgroup")
    source_fd = os.open(source, os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC)
    mount = call(428, ctypes.c_int(source_fd), ctypes.c_char_p(b""),
                 ctypes.c_uint(1 | os.O_CLOEXEC | 0x1000))
    os.close(source_fd)
    # Only this detached clone becomes read-only; the source never changes.
    class MountAttr(ctypes.Structure):
        _fields_ = [(name, ctypes.c_uint64) for name in
                    ("attr_set", "attr_clr", "propagation", "userns_fd")]
    attrs = MountAttr(1 | 2 | 4 | 8, 0, 0, 0)
    call(442, ctypes.c_int(mount), ctypes.c_char_p(b""), ctypes.c_uint(0x1000),
         ctypes.byref(attrs), ctypes.c_size_t(ctypes.sizeof(attrs)))
else:
    if action == "restore":
        checked(libc.setns(ipc, 0x08000000))
    else:
        checked(libc.unshare(0x08000000))
    context = call(430, ctypes.c_char_p(b"mqueue"), ctypes.c_uint(1))
    call(431, ctypes.c_int(context), ctypes.c_uint(6), ctypes.c_void_p(),
         ctypes.c_void_p(), ctypes.c_int(0))
    mount = call(432, ctypes.c_int(context), ctypes.c_uint(1), ctypes.c_uint(2 | 4 | 8))
    os.close(context)

expected = os.fstat(mount)
if action == "restore" and before == {"device": expected.st_dev, "inode": expected.st_ino}:
    print(json.dumps(before))
    raise SystemExit(0)
assert start_ticks() == expected_start
checked(libc.setns(namespace, 0x00020000))
os.fchdir(root)
# Replacing the row avoids the independent duplicate-mount refusal masking
# this provenance regression. The old kernel filesystem remains pinned by
# the runtime's namespace/cgroup and by any retained lease under test.
checked(libc.umount2(ctypes.c_char_p(leaf.encode()), 0))
call(429, ctypes.c_int(mount), ctypes.c_char_p(b""), ctypes.c_int(root),
     ctypes.c_char_p(leaf.encode()), ctypes.c_uint(4))
os.close(mount)
assert start_ticks() == expected_start
current_namespace = os.open("ns/mnt", os.O_RDONLY | os.O_CLOEXEC, dir_fd=proc)
assert os.fstat(current_namespace).st_ino == os.fstat(namespace).st_ino
actual = identity()
assert actual == {"device": expected.st_dev, "inode": expected.st_ino}
assert actual != before
print(json.dumps(actual))
