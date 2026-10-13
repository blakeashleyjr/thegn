#!/usr/bin/env python3
"""Reads/writes only named synthetic canaries; uses local audit endpoints only."""
import json
import os
from pathlib import Path
import socket
import struct
import sys

root = Path(sys.argv[1])
home = root / 'home'
files = {
    'worktree': home / 'wt' / 'tracked',
    'sibling_checkout': home / 'sibling' / 'sibling-canary',
    'shared_git': home / 'repo' / '.git' / 'audit-canary',
    'shared_git_config': home / 'repo' / '.git' / 'config',
    'home': home / 'home-canary',
    'credential_ssh': home / '.ssh' / 'id_test',
    'credential_gpg': home / '.gnupg' / 'key',
    'credential_gpg_default_alias': Path('/rw/key'),
    'cache': home / '.cache' / 'pip' / 'cache-canary',
    'custom_system_fixture': root / 'system' / 'system-canary',
    'host_shared_memory_fixture': Path(sys.argv[5]),
}
for label, path in files.items():
    result = {'label': label}
    try:
        result['read'] = len(path.read_bytes()) > 0
    except OSError as error:
        result['read'] = False
        result['read_errno'] = error.errno
    try:
        # Append zero bytes so success checks write access without corrupting Git config.
        with path.open('ab'):
            pass
        result['write'] = True
    except OSError as error:
        result['write'] = False
        result['write_errno'] = error.errno
    print(json.dumps(result), flush=True)
print(json.dumps({'label': 'system_os_release', 'read': Path('/etc/os-release').exists()}), flush=True)
print(json.dumps({'label': 'credential_forwarding', 'synthetic_GH_TOKEN_present': os.getenv('GH_TOKEN') == 'AUDIT-SYNTHETIC-TOKEN'}), flush=True)
# Host socket address for rootless Podman is provisioned by its generated hosts file.
address = '127.0.0.1'
if sys.argv[4] == 'oci':
    for line in Path('/etc/hosts').read_text().splitlines():
        if 'host.containers.internal' in line.split()[1:]:
            address = line.split()[0]
            break
print(json.dumps({'label': 'endpoint_address', 'address': address}), flush=True)
try:
    with socket.create_connection((address, int(sys.argv[2])), timeout=2) as sock:
        sock.sendall(b'AUDIT')
        response = sock.recv(16)
    print(json.dumps({'label': 'direct_ip_tcp', 'allowed': response == b'AUDIT-OK'}), flush=True)
except OSError as error:
    print(json.dumps({'label': 'direct_ip_tcp', 'allowed': False, 'error': str(error)}), flush=True)
query = struct.pack('!6H', 123, 0x100, 1, 0, 0, 0) + b'\x07blocked\x04test\x00' + struct.pack('!2H', 1, 1)
try:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.settimeout(2)
        sock.sendto(query, (address, int(sys.argv[3])))
        response, _ = sock.recvfrom(512)
    print(json.dumps({'label': 'alternate_dns_udp', 'rcode': response[3] & 15}), flush=True)
except OSError as error:
    print(json.dumps({'label': 'alternate_dns_udp', 'error': str(error)}), flush=True)
