#!/usr/bin/env python3
"""Mutate only a marked test container's pinned mount namespace.

The caller keeps the owned runtime alive and removes it after observation.
This helper enters that namespace in its own process; the observer and daemon
retain their original namespaces. No host mounts or kernel settings change.
"""
import ctypes
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys

socket, container, pid_text, expected_start, mode = sys.argv[1:]
assert mode in {"observe", "missing-directory", "missing-file", "wrong-device",
                "nonempty-directory", "writable-directory", "unreadable-directory"}
assert os.geteuid() == 0 and Path(socket).is_absolute()
assert re.fullmatch(r"[0-9a-f]{64}", container)
assert pid_text.isdecimal() and expected_start.isdecimal()
pid = int(pid_text)
assert pid > 1 and pid != os.getpid()
self_info = os.open("/proc/self/fdinfo", os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC)
proc = os.open(f"/proc/{pid}", os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC)
def start_ticks():
    fd = os.open("stat", os.O_RDONLY | os.O_CLOEXEC, dir_fd=proc)
    with os.fdopen(fd) as stream:
        return stream.read().rsplit(")", 1)[1].split()[19]
assert start_ticks() == expected_start
record = json.loads(subprocess.check_output([
    "docker", "--host", "unix://" + socket, "inspect", container], text=True))[0]
assert record["Id"] == container and record["State"]["Running"]
assert record["State"]["Pid"] == pid
labels = record["Config"]["Labels"]
assert labels.get("io.pbps.resolver.mask-fixture") == "v1"
assert re.fullmatch(r"[0-9a-f]{32}", labels["io.pbps.resolver.owner"])
assert "/proc/acpi" in record["HostConfig"]["MaskedPaths"]
namespace = os.open("ns/mnt", os.O_RDONLY | os.O_CLOEXEC, dir_fd=proc)
root = os.open("root", os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC, dir_fd=proc)
assert os.fstat(namespace).st_ino != os.stat("/proc/self/ns/mnt").st_ino
assert start_ticks() == expected_start

def leaves():
    for name in ["asound", "acpi", "interrupts", "kcore", "keys", "latency_stats", "timer_list", "timer_stats", "sched_debug", "scsi"]:
        try:
            fd = os.open("proc/" + name, os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=root)
        except FileNotFoundError:
            print(json.dumps({"path":name,"absent":True}), flush=True)
            continue
        meta = os.fstat(fd)
        info_fd = os.open(str(fd), os.O_RDONLY | os.O_CLOEXEC, dir_fd=self_info)
        with os.fdopen(info_fd) as stream:
            info = stream.read()
        mount = next(row.split(":",1)[1].strip() for row in info.splitlines() if row.startswith("mnt_id:"))
        print(json.dumps({"path":name,"mode":oct(meta.st_mode),"rdev":meta.st_rdev,"mount":mount}), flush=True)
        os.close(fd)

leaves()
if mode == "observe":
    sys.exit(0)
libc = ctypes.CDLL(None, use_errno=True)
libc.setns.argtypes = [ctypes.c_int, ctypes.c_int]
libc.umount2.argtypes = [ctypes.c_char_p, ctypes.c_int]
def checked(result):
    if result != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p,
                       ctypes.c_ulong, ctypes.c_char_p]
checked(libc.setns(namespace, 0x00020000))
os.fchdir(root)
leaf = b"proc/interrupts" if mode in {"missing-file", "wrong-device"} else b"proc/acpi"
checked(libc.umount2(leaf, 0))
meta = os.stat(leaf, dir_fd=root, follow_symlinks=False)
assert stat.S_ISREG(meta.st_mode) if leaf.endswith(b"interrupts") else stat.S_ISDIR(meta.st_mode)
if mode == "wrong-device":
    checked(libc.mount(b"dev/zero", leaf, None, 4096, None))  # MS_BIND
elif mode in {"nonempty-directory", "writable-directory", "unreadable-directory"}:
    checked(libc.mount(b"tmpfs", leaf, b"tmpfs", 2 | 4 | 8, b"size=4096,mode=0755"))
    if mode == "nonempty-directory":
        with open("proc/acpi/exposed-interface", "w") as stream:
            stream.write("owned negative-control marker\n")
    if mode == "unreadable-directory":
        os.chmod("proc/acpi", 0)
    if mode != "writable-directory":
        checked(libc.mount(None, leaf, None, 1 | 2 | 4 | 8 | 32, None))  # RO + REMOUNT
assert start_ticks() == expected_start
print("owned mask mutation: " + mode, flush=True)
leaves()
