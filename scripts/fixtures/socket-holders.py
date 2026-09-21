"""Acknowledged real socket schedules for the disposable native fixture."""
import ctypes
import json
import os
import socket
import subprocess
import sys
import threading


def worker():
    control = socket.socket(fileno=int(sys.argv[2]))
    transfer = socket.socket(fileno=int(sys.argv[3]))
    held = socket.socket(fileno=int(sys.argv[4])) if len(sys.argv) > 4 else None
    stop = threading.Event()
    threads = []
    control.send(b'ready')
    while True:
        command = control.recv(64).decode()
        if command == 'receive':
            _, descriptors, _, _ = socket.recv_fds(transfer, 1, 1)
            assert len(descriptors) == 1
            held = socket.socket(fileno=descriptors[0])
        elif command == 'send':
            socket.send_fds(transfer, [b'x'], [held.fileno()])
        elif command == 'close':
            held.close()
            held = None
        elif command == 'threads':
            for _ in range(6):
                thread = threading.Thread(target=stop.wait)
                thread.start()
                threads.append(thread)
        elif command == 'worker-only':
            ready = threading.Event()
            result = []
            def separate_table():
                libc = ctypes.CDLL(None, use_errno=True)
                result.append((libc.unshare(0x400), ctypes.get_errno()))  # CLONE_FILES
                ready.set()
                stop.wait()
            thread = threading.Thread(target=separate_table)
            thread.start()
            threads.append(thread)
            ready.wait()
            assert result[0][0] == 0, result
            # The worker retains its own table and socket reference.
            held.close()
            held = None
        elif command == 'orphan':
            read_end, write_end = os.pipe()
            intermediate = os.fork()
            if intermediate == 0:
                os.close(read_end)
                grandchild = os.fork()
                if grandchild == 0:
                    os.write(write_end, str(os.getpid()).encode())
                    os.close(write_end)
                    control.close()
                    transfer.close()
                    # The supervisor is a subreaper and explicitly kills and
                    # reaps this holder before shutting down the namespace.
                    threading.Event().wait()
                    os._exit(0)
                os._exit(0)
            os.close(write_end)
            orphan = int(os.read(read_end, 64))
            os.close(read_end)
            os.waitpid(intermediate, 0)
            control.send(json.dumps(orphan).encode())
            continue
        elif command == 'stop':
            stop.set()
            for thread in threads:
                thread.join()
            control.send(b'null')
            return
        else:
            raise AssertionError(command)
        control.send(b'null')


class Actor:
    def __init__(self, transfer, held=None):
        self.control, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
        fds = [child.fileno(), transfer.fileno()]
        if held is not None:
            fds.append(held.fileno())
        self.process = subprocess.Popen(
            [sys.executable, __file__, 'worker', *map(str, fds)], pass_fds=fds)
        child.close()
        assert self.control.recv(64) == b'ready'

    def ask(self, command):
        self.control.send(command.encode())
        return json.loads(self.control.recv(4096))

    def close(self):
        self.ask('stop')
        self.process.wait(timeout=5)
        self.control.close()


def supervisor(case):
    assert ctypes.CDLL(None).prctl(36, 1, 0, 0, 0) == 0  # PR_SET_CHILD_SUBREAPER
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        listener.listen()
        client = socket.create_connection(listener.getsockname())
        accepted, _ = listener.accept()
    inode = int(os.readlink(f'/proc/self/fd/{accepted.fileno()}')[8:-1])
    left, right = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    b = Actor(right)
    c = Actor(left, accepted) if case == 'handoff' else None
    if c:
        dummy, other = socket.socketpair()
        a = Actor(dummy, accepted)
        dummy.close()
        other.close()
    else:
        a = Actor(left, accepted)
    left.close()
    right.close()
    accepted.close()
    orphan = None

    def move(source, destination, close=True):
        destination.control.send(b'receive')
        source.ask('send')
        assert destination.control.recv(64) == b'null'
        if close:
            source.ask('close')

    if case == 'two':
        move(a, b, False)
    elif case == 'threads':
        a.ask('threads')
    elif case == 'worker-only':
        a.ask('worker-only')
    elif case == 'orphan':
        orphan = a.ask('orphan')
    print(json.dumps(dict(anchor=os.getpid(), a=a.process.pid, b=b.process.pid,
                          c=c.process.pid if c else None, inode=inode,
                          local='%s:%d' % client.getsockname(),
                          peer='%s:%d' % client.getpeername(), orphan=orphan)), flush=True)
    try:
        for command in sys.stdin:
            command = command.strip()
            if command == 'move':
                move(c or a, b)
            elif command == 'reset':
                move(b, c)
            elif command == 'stop':
                return
            else:
                raise AssertionError(command)
            print('ok', flush=True)
    finally:
        if orphan:
            os.kill(orphan, 9)
            os.waitpid(orphan, 0)
        for actor in [a, b] + ([c] if c else []):
            actor.close()
        client.close()


if sys.argv[1] == 'worker':
    worker()
else:
    supervisor(sys.argv[1])
