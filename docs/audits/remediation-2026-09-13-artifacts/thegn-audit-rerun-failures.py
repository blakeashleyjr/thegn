import concurrent.futures, json, os, signal, subprocess, sys, tempfile, time
from collections import Counter
from pathlib import Path

binary, label, failure_file = sys.argv[1:]
failed = {r["test"] for r in json.loads(Path(failure_file).read_text()) if r["status"] in ("failed", "timeout")}
listed = subprocess.check_output([binary, '--list'], text=True)
Path('/tmp/' + label + '-testlist.txt').write_text(listed)
names = [line.removesuffix(': test') for line in listed.splitlines() if line.endswith(': test')]
assert failed.issubset(set(names)), failed - set(names)
names = [name for name in names if name in failed]
assert names, 'no selected tests'
root = Path('/tmp/' + label + '-results')
root.mkdir(exist_ok=True)

def run(name):
    with tempfile.TemporaryDirectory(prefix='thegn-audit-state-') as tmp:
        env = {key: os.environ[key] for key in ('PATH', 'LANG', 'LC_ALL', 'TERM') if key in os.environ}
        for key, directory in [('XDG_STATE_HOME', 'state'), ('XDG_CONFIG_HOME', 'config'),
                               ('XDG_DATA_HOME', 'data'), ('XDG_CACHE_HOME', 'cache'),
                               ('XDG_RUNTIME_DIR', 'runtime')]:
            path = Path(tmp) / directory
            path.mkdir(mode=0o700)
            env[key] = str(path)
        env['GIT_CONFIG_GLOBAL'] = '/dev/null'
        env['GIT_CONFIG_NOSYSTEM'] = '1'
        start = time.monotonic()
        proc = subprocess.Popen([binary, '--exact', name, '--nocapture', '--test-threads=1'],
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                env=env, start_new_session=True)
        try:
            output = proc.communicate(timeout=90)[0]
            status = ('passed' if b'1 passed' in output else 'ignored' if b'1 ignored' in output
                      else 'failed') if proc.returncode == 0 else 'failed'
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            output = proc.communicate()[0]
            status = 'timeout'
        log = root / (name.replace('::', '__') + '.log')
        log.write_bytes(output)
        return dict(test=name, status=status, exit=proc.returncode,
                    seconds=round(time.monotonic() - start, 3), log=str(log))

results = []
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
    for result in pool.map(run, names):
        results.append(result)
        if result['status'] != 'passed':
            print(json.dumps(result), flush=True)
Path('/tmp/' + label + '-results.json').write_text(json.dumps(results, indent=2) + '\n')
print('SUMMARY', dict(Counter(r['status'] for r in results)), 'TOTAL', len(results), flush=True)
sys.exit(any(r['status'] in ('failed', 'timeout') for r in results))
