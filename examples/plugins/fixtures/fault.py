#!/usr/bin/env python3
# Synthetic resident used by preview/transport tests; stdout is only NDJSON.
import argparse
import json
import os
from pathlib import Path
import sys
import time

parser = argparse.ArgumentParser()
parser.add_argument('--mode', choices=['normal', 'invalid-view', 'malformed', 'flood', 'stderr-flood', 'crash', 'no-read', 'delay'], default='normal')
parser.add_argument('--pid-file', type=Path)
args = parser.parse_args()
if args.pid_file:
    args.pid_file.write_text(str(os.getpid()))

def emit(value):
    print(json.dumps(value, ensure_ascii=False), flush=True)

def view(revision):
    return {'method': 'update', 'params': {'surface': 'fixture.workspace', 'app': {'state_scope': 'fixture', 'revision': revision, 'status': 'Synthetic', 'root': {'kind': 'text', 'id': 'text', 'title': 'Fixture', 'text': 'Ready'}}}}

if args.mode == 'no-read':
    time.sleep(60)
    raise SystemExit()
if args.mode == 'delay':
    time.sleep(0.2)
if args.mode == 'crash':
    raise SystemExit(3)
# Deliberately emits before activate: verifies the startup registration barrier.
emit(view(1))
if args.mode == 'malformed':
    print('{invalid-json', flush=True)
if args.mode == 'invalid-view':
    invalid = view(2)
    invalid['params']['app']['root']['id'] = ''
    emit(invalid)
if args.mode == 'flood':
    for revision in range(2, 2002):
        emit(view(revision))
if args.mode == 'stderr-flood':
    sys.stderr.write(('fixture diagnostic' + chr(10)) * 4000)
    sys.stderr.flush()
for line in sys.stdin:
    request = json.loads(line)
    if request.get('method') == 'deactivate':
        break
    if 'id' in request:
        emit({'id': request['id'], 'result': None})
