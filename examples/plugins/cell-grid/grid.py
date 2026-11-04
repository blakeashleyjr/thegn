#!/usr/bin/env python3
# No terminal escapes: bounded frames, typed keys, and actual region sizes.
import json
import sys

revision, cols, rows, last = 0, 80, 24, 'Press a key; Esc returns to Work'
session = None
while True:
    line = sys.stdin.buffer.readline((1 << 20) + 1)
    if not line:
        break
    if len(line) >= 1 << 20 or not line.endswith(b'\n'):
        raise SystemExit('Host frame exceeds the protocol limit')
    message = json.loads(line)
    method = message.get('method')
    if method == 'deactivate':
        break
    if method == 'activate':
        session = message.get('params', {}).get('session')
    elif method == 'on_event':
        payload = message.get('params', {}).get('payload', {})
        if not session or payload.get('session') != session:
            if 'id' in message:
                print(json.dumps({'id': message['id'], 'error': {'code': 'denied', 'message': 'Stale session'}}), flush=True)
            continue
        data = payload.get('data', {})
        event = payload.get('event')
        if event == 'resize' and data.get('widget') == 'grid':
            cols = max(0, min(512, data['width']))
            rows = max(0, min(256, data['height']))
        elif event == 'key':
            key = data.get('key', {})
            last = str(key.get('value', key.get('name', '')))
        elif event == 'paste':
            last = data.get('text', '')
        if 'id' in message:
            print(json.dumps({'id': message['id'], 'result': None}), flush=True)
    else:
        continue
    revision += 1
    # Diagnostics use ASCII escapes; wide input cannot overlap display cells.
    display = last.encode('unicode_escape').decode('ascii')
    cells = [dict(x=x, y=0, span=dict(text=c, role='Accent'))
             for x, c in enumerate(display[:cols])] if rows else []
    print(json.dumps(dict(method='update', params=dict(surface='cell-grid.demo', app=dict(
        state_scope='cell-grid', revision=revision, status='Cell-grid escape hatch',
        root=dict(kind='grid', id='grid', width=cols, height=rows, cells=cells, cursor=None))))), flush=True)
