#!/usr/bin/env python3
"""Read or change synthetic UTS names only in an explicitly owned fixture."""
import ctypes
import http.client
import json
import os
from pathlib import Path
import re
import socket as sockets
import sys

socket, container, pid_text, expected_start, action, value = sys.argv[1:]
assert os.geteuid() == 0 and Path(socket).is_absolute()
assert re.fullmatch(r"[0-9a-f]{64}", container)
assert action in {"read", "hostname", "domainname"}
assert value in {"", "pbps-resolver", "(none)", "localdomain",
                 "pbps-804-host.invalid", "pbps-804-domain.invalid"}
pid = int(pid_text)
assert pid > 1 and pid != os.getpid()
proc = os.open(f"/proc/{pid}", os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC)
def start_ticks():
    fd = os.open("stat", os.O_RDONLY | os.O_CLOEXEC, dir_fd=proc)
    with os.fdopen(fd) as stream:
        return stream.read().rsplit(")", 1)[1].split()[19]
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
namespace = os.open("ns/uts", os.O_RDONLY | os.O_CLOEXEC, dir_fd=proc)
assert os.fstat(namespace).st_ino != os.stat("/proc/thread-self/ns/uts").st_ino
assert start_ticks() == expected_start
libc = ctypes.CDLL(None, use_errno=True)
libc.setns.argtypes = [ctypes.c_int, ctypes.c_int]
libc.uname.argtypes = [ctypes.c_void_p]
libc.sethostname.argtypes = libc.setdomainname.argtypes = [ctypes.c_char_p, ctypes.c_size_t]
def checked(result):
    if result:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
checked(libc.setns(namespace, 0x04000000))
assert os.stat("/proc/thread-self/ns/uts").st_ino == os.fstat(namespace).st_ino
if action != "read":
    raw = value.encode()
    checked((libc.sethostname if action == "hostname" else libc.setdomainname)(raw, len(raw)))
result = ctypes.create_string_buffer(65 * 6)
checked(libc.uname(result))
fields = [result.raw[i * 65:(i + 1) * 65].split(b"\0", 1)[0].decode() for i in range(6)]
assert start_ticks() == expected_start
current = os.open("ns/uts", os.O_RDONLY | os.O_CLOEXEC, dir_fd=proc)
assert os.fstat(current).st_ino == os.fstat(namespace).st_ino
print(json.dumps({"hostname": fields[1], "domainname": fields[5]}))
