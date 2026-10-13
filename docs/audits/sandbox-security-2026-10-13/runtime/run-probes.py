#!/usr/bin/env python3
"""Reproduce audit against cached Podman image plus bwrap/systemd; never pull.
Run from an isolated worktree only. Creates unique owned containers and removes
only those names. All traffic goes to controlled local endpoints.
"""
import argparse
import json
import os
from pathlib import Path
import pty
import select
import shlex
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument('--helper', type=Path, required=True)
parser.add_argument('--image', default='84be7cfa538b9f8180f268e20329f0123b781d471fb490c162294e631b07f9e1')
parser.add_argument('--oci-host', default='unix:///run/user/1000/podman/podman.sock')
parser.add_argument('--only', help='comma-separated case labels')
parser.add_argument('--minimal-oci', action='store_true')
parser.add_argument('--corrected-mounts', action='store_true')
parser.add_argument('--output', type=Path, default=Path(__file__).resolve().parent / 'results')
a = parser.parse_args()
a.output.mkdir(parents=True, exist_ok=True)
helper = str(a.helper.resolve())

def command(cmd, env=None, timeout=20):
    p = subprocess.run(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout, text=True)
    return p.returncode, p.stdout

def run_pty(cmd, env, timeout=90):
    pid, fd = pty.fork()
    if pid == 0:
        os.execve(cmd[0], cmd, env)
    deadline = time.monotonic() + timeout
    data = bytearray()
    status = None
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], .2)
            if ready:
                try:
                    part = os.read(fd, 65536)
                except OSError:
                    break
                if not part:
                    break
                data.extend(part)
            finished, child_status = os.waitpid(pid, os.WNOHANG)
            if finished:
                status = child_status
        if status is None:
            finished, child_status = os.waitpid(pid, os.WNOHANG)
            if not finished:
                os.killpg(pid, signal.SIGTERM)
                _, child_status = os.waitpid(pid, 0)
                data.extend(b'\nAUDIT_TIMEOUT\n')
            status = child_status
    except ChildProcessError:
        pass
    finally:
        os.close(fd)
    return status, data.decode(errors='replace').replace('\r\n', '\n')

# Loopback/gateway are the only probe targets. Listen on host interfaces so
# rootless Podman's synthetic gateway can reach the same controlled server.
tcp = socket.socket()
tcp.bind(('0.0.0.0', 0))
tcp.listen()
udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
udp.bind(('0.0.0.0', 0))

def tcp_server():
    while True:
        conn, peer = tcp.accept()
        with conn:
            conn.settimeout(2)
            if conn.recv(16) == b'AUDIT':
                conn.sendall(b'AUDIT-OK')

def dns_server():
    while True:
        query, peer = udp.recvfrom(512)
        response = bytearray(query)
        response[2:4] = b'\x81\x80'
        response[6:8] = b'\x00\x01'
        response.extend(b'\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x1e\x00\x04\x7f\x00\x00\x01')
        udp.sendto(response, peer)
threading.Thread(target=tcp_server, daemon=True).start()
threading.Thread(target=dns_server, daemon=True).start()

# Preserve PATH only, then inject synthetic credentials. Do not pass host secrets.
env = {'PATH': os.environ['PATH'], 'USER': 'blake', 'LOGNAME': 'blake', 'TERM': 'xterm-256color',
       'SHELL': '/bin/sh', 'XDG_RUNTIME_DIR': '/run/user/1000', 'GH_TOKEN': 'AUDIT-SYNTHETIC-TOKEN'}
worktree_root = Path(__file__).resolve().parents[4]
with tempfile.TemporaryDirectory(prefix='.audit-fixture-', dir=worktree_root) as td, tempfile.TemporaryDirectory(prefix='thegn-audit-system-', dir='/dev/shm') as sd:
    system_canary = Path(sd) / 'system-canary'
    system_canary.write_text('AUDIT-SYNTHETIC-SYSTEM-SHM\n')
    root = Path(td)
    home = root / 'home'
    home.mkdir()
    # Capture OCI create failures that ensure() otherwise summarizes.
    bindir = root / 'bin'
    bindir.mkdir()
    real_podman = shutil.which('podman')
    capture = Path(__file__).with_name('podman-capture.py').read_text()
    wrapper = bindir / 'podman'
    wrapper.write_text('#!' + shutil.which('python3') + '\n' + capture.split('\n', 1)[1])
    wrapper.chmod(0o755)
    env['AUDIT_REAL_PODMAN'] = real_podman
    env['AUDIT_CLI_LOG'] = str((a.output / 'podman-cli.jsonl').resolve())
    env['PATH'] = str(bindir) + ':' + env['PATH']
    env.update({'HOME': str(home), 'XDG_CONFIG_HOME': str(home / '.config'), 'XDG_DATA_HOME': str(home / '.local/share'), 'XDG_STATE_HOME': str(home / '.local/state')})
    for relative in ['home-canary', '.ssh/id_test', '.gnupg/key', '.cache/pip/cache-canary', '.cargo/registry/cache-canary']:
        path = home / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('AUDIT-SYNTHETIC-CANARY\n')
    (home / '.gitconfig').write_text('[user]\n name = Audit\n email = audit@test\n[commit]\n gpgsign = false\n')
    (root / 'system').mkdir()
    (root / 'system/system-canary').write_text('AUDIT-SYNTHETIC-SYSTEM\n')
    repo = home / 'repo'
    command(['git', 'init', '-q', str(repo)], env)
    (repo / 'tracked').write_text('baseline\n')
    command(['git', '-C', str(repo), 'add', 'tracked'], env)
    command(['git', '-C', str(repo), 'commit', '-qm', 'synthetic-baseline'], env)
    wt = home / 'wt'
    command(['git', '-C', str(repo), 'worktree', 'add', '-qb', 'audit', str(wt)], env)
    (repo / '.git/audit-canary').write_text('AUDIT-SYNTHETIC-SHARED-GIT\n')
    sibling = home / 'sibling'
    command(['git', 'init', '-q', str(sibling)], env)
    (sibling / 'sibling-canary').write_text('AUDIT-SYNTHETIC-SIBLING\n')
    shutil.copyfile(Path(__file__).with_name('probe_inside.py'), wt / 'probe_inside.py')
    env.update({'AUDIT_IMAGE': a.image, 'AUDIT_OCI_HOST': a.oci_host})
    if a.minimal_oci:
        env['AUDIT_OCI_MINIMAL'] = '1'
    if a.corrected_mounts:
        env['AUDIT_CORRECTED_MOUNTS'] = '1'
    inventory = {'source_commit': command(['git', 'rev-parse', 'HEAD'])[1].strip(), 'platform': os.uname().sysname + ' ' + os.uname().release,
                 'tcp_port': tcp.getsockname()[1], 'dns_port': udp.getsockname()[1], 'oci_host': a.oci_host, 'cached_image': a.image}
    (a.output / 'inventory.json').write_text(json.dumps(inventory, indent=2) + '\n')
    for mode in ['dns', 'floor']:
        code, output = command([helper, mode], env)
        (a.output / (mode + '.log')).write_text('exit=' + str(code) + '\n' + output)
    envrepo = root / 'env-repo'
    envrepo.mkdir()
    code, output = command([helper, 'env-selection', str(envrepo)], env)
    policyrepo = root / 'policy-repo'
    policyrepo.mkdir()
    policycode, policyoutput = command([helper, 'policy-overlay', str(policyrepo)], env)
    (a.output / 'policy-overlay.log').write_text('exit=' + str(policycode) + '\n' + policyoutput)
    (a.output / 'env-selection.log').write_text('exit=' + str(code) + '\n' + output)
    cases = [('bwrap-hardened', 'run', 'bwrap', 'hardened', 'nat', ''),
             ('bwrap-hardened-deny-all', 'run', 'bwrap', 'hardened', 'nat', 'deny-all'),
             ('bwrap-network-none', 'run', 'bwrap', 'hardened', 'none', ''),
             ('bwrap-sealed', 'run', 'bwrap', 'sealed', 'nat', ''),
             ('systemd-hardened', 'run', 'systemd', 'hardened', 'nat', ''),
             ('systemd-network-none', 'run', 'systemd', 'hardened', 'none', ''),
             ('systemd-sealed', 'run', 'systemd', 'sealed', 'nat', ''),
             ('podman-hardened-reuse-network-none', 'reuse', 'podman', 'hardened', 'nat', ''),
             ('podman-network-none', 'run', 'podman', 'hardened', 'none', ''),
             ('podman-sealed', 'run', 'podman', 'sealed', 'nat', ''),
             ('podman-hardened-deny-all', 'run', 'podman', 'hardened', 'nat', 'deny-all'),
             ('bwrap-custom-mount', 'run', 'bwrap', 'hardened', 'none', ''),
             ('missing-backend-empty-chain-fallback', 'plan', 'apple', 'sealed', 'none', ''),
             ('missing-apple-fallback', 'plan', 'apple', 'sealed', 'none', '')]
    for label, mode, backend, profile, network, policy in cases:
        if a.only and label not in a.only.split(','):
            continue
        name = 'thegn-audit-20261013-' + str(os.getpid()) + '-' + label
        env['AUDIT_CONTAINER_NAME'] = name
        env.pop('AUDIT_EMPTY_CHAIN', None)
        env.pop('AUDIT_CUSTOM_MOUNT', None)
        if label == 'missing-backend-empty-chain-fallback':
            env['AUDIT_EMPTY_CHAIN'] = '1'
        if label == 'bwrap-custom-mount':
            env['AUDIT_CUSTOM_MOUNT'] = str(root / 'system')
        python = shutil.which('python3')
        container_mode = 'oci' if backend == 'podman' else 'host'
        env['AUDIT_INNER'] = shlex.join([python, str(wt / 'probe_inside.py'), str(root), str(tcp.getsockname()[1]), str(udp.getsockname()[1]), container_mode, str(system_canary)])
        cmd = [helper, mode, str(wt), backend, profile, network] + ([policy] if policy else [])
        print('running ' + label, flush=True)
        try:
            code, output = run_pty(cmd, env)
            (a.output / (label + '.log')).write_text('wait_status=' + str(code) + '\n' + output)
            if mode == 'reuse' and a.minimal_oci:
                restartcmd = [helper, 'run', str(wt), backend, profile, 'none']
                restartcode, restartoutput = run_pty(restartcmd, env)
                (a.output / (label + '-process-restart.log')).write_text('wait_status=' + str(restartcode) + '\n' + restartoutput)
                containercode, containeroutput = command(['podman', '--url', a.oci_host, 'restart', name], env, timeout=30)
                (a.output / (label + '-container-restart.log')).write_text('exit=' + str(containercode) + '\n' + containeroutput)
                aftercode, afteroutput = run_pty(restartcmd, env)
                (a.output / (label + '-after-container-restart.log')).write_text('wait_status=' + str(aftercode) + '\n' + afteroutput)
        finally:
            if backend == 'podman':
                statecode, stateoutput = command(['podman', '--url', a.oci_host, 'inspect', '--format', '{{.State.Running}} {{.Id}}', name], env)
                (a.output / (label + '-after-process-exit.log')).write_text('exit=' + str(statecode) + '\n' + stateoutput)
                code, output = command(['podman', '--url', a.oci_host, 'rm', '-f', name], env)
                (a.output / (label + '-cleanup.log')).write_text('exit=' + str(code) + '\n' + output)
