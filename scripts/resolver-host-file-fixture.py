#!/usr/bin/env python3
"""Overmount only an owned container's empty image file; never change a layer."""
import ctypes
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys

socket, container, pid_text, expected_start, token, path, mode, text = sys.argv[1:]
assert os.geteuid() == 0 and Path(socket).is_absolute()
assert re.fullmatch(r"[0-9a-f]{64}", container)
assert re.fullmatch(r"[0-9a-f]{32}", token)
assert path in {"/etc/resolv.conf", "/etc/hosts", "/etc/hostname"}
assert mode in {"apply", "restore"} and len(text.encode()) <= 4096
pid = int(pid_text)
assert pid > 1 and pid != os.getpid()
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
assert re.fullmatch(r"[0-9a-f]{32,64}", record["Config"]["Labels"]["io.pbps.resolver.owner"])
namespace = os.open("ns/mnt", os.O_RDONLY | os.O_CLOEXEC, dir_fd=proc)
root = os.open("root", os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC, dir_fd=proc)
assert os.fstat(namespace).st_ino != os.stat("/proc/self/ns/mnt").st_ino
assert start_ticks() == expected_start
libc = ctypes.CDLL(None, use_errno=True)
libc.setns.argtypes = [ctypes.c_int, ctypes.c_int]
libc.umount2.argtypes = [ctypes.c_char_p, ctypes.c_int]
libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_char_p]
def checked(result):
    if result:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
checked(libc.setns(namespace, 0x00020000))
os.fchdir(root)
leaf = path.lstrip("/")
directory = "tmp/pbps-host-file-" + token
source = directory + "/" + path.rsplit("/", 1)[1]
if mode == "apply":
    os.mkdir(directory, 0o700)
    fd = os.open(source, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o444)
    with os.fdopen(fd, "w") as stream:
        stream.write(text)
    checked(libc.mount(source.encode(), leaf.encode(), None, 4096, None))
    checked(libc.mount(None, leaf.encode(), None, 4096 | 32 | 1 | 2 | 4 | 8, None))
    assert os.statvfs(leaf).f_flag & os.ST_RDONLY
elif os.path.exists(directory):
    meta = os.stat(source, follow_symlinks=False)
    actual = os.stat(leaf, follow_symlinks=False)
    assert stat.S_ISREG(meta.st_mode) and meta.st_uid == 0
    assert (meta.st_dev, meta.st_ino) == (actual.st_dev, actual.st_ino)
    checked(libc.umount2(leaf.encode(), 0))
    os.unlink(source)
    os.rmdir(directory)
assert start_ticks() == expected_start
assert Path(leaf).read_text() == text
