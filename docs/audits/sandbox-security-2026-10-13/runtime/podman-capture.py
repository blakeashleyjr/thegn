#!/usr/bin/env python3
"""Transparent diagnostic wrapper; refuses image pulls during this audit."""
import json
import os
import subprocess
import sys
if 'pull' in sys.argv[1:]:
    print('AUDIT: registry pulls are forbidden', file=sys.stderr)
    sys.exit(97)
if 'exec' in sys.argv[1:]:
    os.execv(os.environ['AUDIT_REAL_PODMAN'], [os.environ['AUDIT_REAL_PODMAN'], *sys.argv[1:]])
result = subprocess.run([os.environ['AUDIT_REAL_PODMAN'], *sys.argv[1:]], capture_output=True)
with open(os.environ['AUDIT_CLI_LOG'], 'a') as log:
    log.write(json.dumps({'argv': sys.argv[1:], 'exit': result.returncode, 'stderr': result.stderr.decode(errors='replace')}) + '\n')
sys.stdout.buffer.write(result.stdout)
sys.stderr.buffer.write(result.stderr)
sys.exit(result.returncode)
