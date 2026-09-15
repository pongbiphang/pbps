#!/usr/bin/env python3
"""Private fixture helper: measure a supplied launch recipe against owned sentinels.

The Rust live test supplies its actual fixed recipe on stdin. Only generated
Docker resources enter this helper. No real host socket or host data is mounted.
"""

import copy
import http.client
import json
import socket
import signal
import subprocess
import sys
import time
import uuid


class Docker(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__('localhost', timeout=15)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def interrupted(signum, frame):
    raise InterruptedError("owned sentinel fixture interrupted")


def main():
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    path = sys.argv[1]
    recipe = json.load(sys.stdin)
    prefix = 'pbps-sentinel-' + uuid.uuid4().hex
    containers = []
    network = volume = None

    def api(method, resource, body=None, expected=(200, 201, 204)):
        conn = Docker(path)
        try:
            conn.request(method, '/v1.47' + resource,
                         None if body is None else json.dumps(body),
                         {'Content-Type': 'application/json'})
            response = conn.getresponse()
            data = response.read(1024 * 1024 + 1)
            if response.status not in expected or len(data) > 1024 * 1024:
                raise RuntimeError('owned fixture Docker request failed: ' + str(response.status))
            return json.loads(data) if data else None
        finally:
            conn.close()

    def create(name, body):
        # Register the exact generated name before the request can be cancelled.
        containers.append(name)
        api('POST', '/containers/create?name=' + name, body)
        api('POST', '/containers/' + name + '/start')
        return name

    def execute(name, program):
        result = subprocess.run(['docker', '--host', 'unix://' + path, 'exec', name,
                                 '/usr/bin/timeout', '5s', '/usr/bin/perl', '-e', program],
                                capture_output=True, text=True, timeout=10)
        if result.returncode:
            raise RuntimeError('owned sentinel probe failed: ' + result.stderr)
        return result.stdout

    server = r'''
use IO::Socket::INET; use IO::Socket::UNIX; use IO::Select;
my $udp = IO::Socket::INET->new(LocalAddr=>'0.0.0.0', LocalPort=>5353, Proto=>'udp') or die "udp";
my $tcp = IO::Socket::INET->new(LocalAddr=>'0.0.0.0', LocalPort=>8080, Proto=>'tcp', Listen=>5, ReuseAddr=>1) or die "tcp";
my $unix = IO::Socket::UNIX->new(Type=>SOCK_STREAM, Local=>'/sentinel/runtime.sock', Listen=>5) or die "unix";
chmod 0777, '/sentinel/runtime.sock';
open(my $file, '>', '/sentinel/host-file') or die "file"; print $file "unchanged\n"; close $file;
chmod 0666, '/sentinel/host-file';
open(my $ready, '>', '/sentinel/ready') or die "ready"; close $ready;
my $poll = IO::Select->new($udp, $tcp, $unix);
while (1) {
 for my $listener ($poll->can_read(1)) {
  my ($kind, $message);
  if ($listener == $udp) { $kind='dns'; $udp->recv($message, 4096); }
  else { $kind=$listener == $tcp ? 'metadata' : 'runtime'; my $client=$listener->accept(); $client->recv($message,4096); close $client; }
  open(my $log, '>>', '/sentinel/events') or die "events"; print $log "$kind\n"; close $log;
 }
}
'''
    try:
        network = prefix + '-net'
        api('POST', '/networks/create', {'Name': network, 'Internal': True})
        volume = prefix + '-data'
        api('POST', '/volumes/create', {'Name': volume})
        service = create(prefix + '-server', {
            'Image': recipe['Image'], 'Entrypoint': ['/usr/bin/timeout'],
            'Cmd': ['--signal=KILL', '120s', '/usr/bin/perl', '-e', server],
            'User': '0:0', 'Env': recipe['Env'], 'Healthcheck': {'Test': ['NONE']},
            'HostConfig': {'NetworkMode': network, 'ReadonlyRootfs': True,
                           'CapDrop': ['ALL'], 'SecurityOpt': ['no-new-privileges'],
                           'Memory': 134217728, 'MemorySwap': 134217728,
                           'NanoCpus': 500000000, 'PidsLimit': 32,
                           'Binds': [volume + ':/sentinel:rw']}})
        for _ in range(50):
            if execute(service, 'print -e "/sentinel/ready" ? "ready" : "waiting"') == 'ready':
                break
            time.sleep(.1)
        else:
            raise RuntimeError('owned sentinel did not start')
        address = api('GET', '/containers/' + service + '/json')['NetworkSettings']['Networks'][network]['IPAddress']
        socket.inet_aton(address)
        probe = r'''
use IO::Socket::INET; use IO::Socket::UNIX;
my $address = shift;
# A real DNS query for a reserved synthetic name, sent directly to our listener.
my $query = pack('n6', 608, 256, 1, 0, 0, 0) . "\x04pbps\x07invalid\0" . pack('n2', 1, 1);
my $udp = IO::Socket::INET->new(Proto=>'udp');
if ($udp) { $udp->send($query, 0, Socket::pack_sockaddr_in(5353, Socket::inet_aton($address))); }
my $tcp = IO::Socket::INET->new(PeerAddr=>$address, PeerPort=>8080, Proto=>'tcp', Timeout=>1);
if ($tcp) { print $tcp "GET /latest/meta-data/ HTTP/1.0\r\n\r\n"; close $tcp; }
my $unix = IO::Socket::UNIX->new(Type=>SOCK_STREAM, Peer=>'/sentinel/runtime.sock');
if ($unix) { print $unix "GET /_ping HTTP/1.0\r\n\r\n"; close $unix; }
if (open(my $file, '>', '/sentinel/host-file')) { print $file "changed\n"; close $file; }
'''
        # The sentinel paths and address are identical in the protected and
        # counterfactual cases. Relax only the stated recipe boundaries.
        for variant, wanted in [('protected', set()), ('network', {'dns'}),
                                ('mount', {'file'}),
                                ('unprotected', {'dns', 'metadata', 'runtime', 'file'})]:
            execute(service, 'unlink "/sentinel/events"; open(my $f, ">", "/sentinel/host-file") or die; print $f "unchanged\n"; close $f;')
            body = copy.deepcopy(recipe)
            body['Cmd'][-1] = 'exec /bin/sleep 120'
            if variant in ('network', 'unprotected'):
                body['NetworkDisabled'] = False
                body['HostConfig']['NetworkMode'] = network
            if variant in ('mount', 'unprotected'):
                body['HostConfig']['Binds'] = [volume + ':/sentinel:rw']
            if variant == 'unprotected':
                # Keep default Docker seccomp, but remove the workload's
                # additional connection denial. Never use privileged mode.
                body['HostConfig']['SecurityOpt'] = ['no-new-privileges']
            candidate = create(prefix + '-' + variant, body)
            result = subprocess.run(['docker', '--host', 'unix://' + path, 'exec',
                                     '--user', '999:999' if '/var/lib/postgresql' in body['HostConfig']['Tmpfs'] else '10001:0',
                                     candidate, '/usr/bin/timeout', '5s', '/usr/bin/perl', '-e', probe, address],
                                    capture_output=True, text=True, timeout=10)
            if result.returncode:
                raise RuntimeError('sentinel effect probe failed: ' + result.stderr)
            time.sleep(.2)
            effects = execute(service, 'if(open(my $f,"<","/sentinel/events")){print while <$f>; close $f;} open(my $f,"<","/sentinel/host-file") or die; my $s=<$f>; print "file\n" if $s eq "changed\n";')
            observed = set(effects.splitlines())
            if observed != wanted:
                raise RuntimeError('sentinel boundary failed: ' + variant + ' observed=' + repr(sorted(observed)))
            api('DELETE', '/containers/' + candidate + '?force=true&v=true', expected=(204, 404))
            print(variant + ': expected effects measured', flush=True)
    finally:
        for name in reversed(containers):
            api('DELETE', '/containers/' + name + '?force=true&v=true', expected=(204, 404))
        if volume:
            api('DELETE', '/volumes/' + volume, expected=(204, 404))
        if network:
            api('DELETE', '/networks/' + network, expected=(204, 404))


if __name__ == '__main__':
    main()
