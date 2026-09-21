#!/usr/bin/env python3
"""Run acknowledged socket-holder fixtures in a disposable proc/PID view."""
import argparse
import os
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--test-binary', required=True, type=Path)
args = parser.parse_args()
if os.geteuid() != 0:
    parser.error('the disposable mount/PID namespace requires root')
env = dict(os.environ, PBPS_SOCKET_FIXTURE=str(
    Path(__file__).resolve().parent / 'fixtures/socket-holders.py'))
result = subprocess.run([
    'unshare', '--mount', '--pid', '--fork', '--mount-proc', '--kill-child',
    str(args.test_binary.resolve(strict=True)), '--ignored', '--test-threads=1',
    'resolver::native::namespace::sockets::tests::', '--nocapture',
], env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=60)
print(result.stdout, end='', flush=True)
if result.returncode or '3 passed; 0 failed' not in result.stdout:
    raise SystemExit('socket-holder fixture did not complete all three tests')
