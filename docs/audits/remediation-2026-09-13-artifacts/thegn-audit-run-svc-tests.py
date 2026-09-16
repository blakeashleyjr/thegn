import concurrent.futures,json,os,signal,subprocess,tempfile,time
from pathlib import Path
binary='/tmp/thegn-audit-build-cache-20260913/debug/deps/thegn_svc-9e40491fa19fc238'
prefixes=('forge::','log::','control::client::','provider_sessions::')
names=[s.removesuffix(': test') for s in Path('/tmp/thegn-audit-svc-test-list.txt').read_text().splitlines() if s.endswith(': test')]
names=[s for s in names if s.startswith(prefixes) or (s.startswith('agent::tests::') and 'floor' in s)]
root=Path('/tmp/thegn-audit-svc-results');root.mkdir(exist_ok=True)
def run(name):
 with tempfile.TemporaryDirectory(prefix='thegn-audit-state-') as tmp:
  env=os.environ.copy()
  for k in list(env):
   if k.startswith('THEGN_') or k.startswith('GIT_'):env.pop(k)
  for k,d in [('XDG_STATE_HOME','state'),('XDG_CONFIG_HOME','config'),('XDG_DATA_HOME','data'),('XDG_CACHE_HOME','cache'),('XDG_RUNTIME_DIR','runtime')]:
   p=Path(tmp)/d;p.mkdir(mode=0o700);env[k]=str(p)
  env['GIT_CONFIG_GLOBAL']='/dev/null';env['GIT_CONFIG_NOSYSTEM']='1'
  start=time.monotonic();p=subprocess.Popen([binary,'--exact',name,'--nocapture','--test-threads=1'],stdout=subprocess.PIPE,stderr=subprocess.STDOUT,env=env,start_new_session=True)
  try:out=p.communicate(timeout=90)[0];status='passed' if p.returncode==0 and b'1 passed' in out else 'failed'
  except subprocess.TimeoutExpired:
   os.killpg(p.pid,signal.SIGKILL);out=p.communicate()[0];status='timeout'
  path=root/(name.replace('::','__')+'.log');path.write_bytes(out)
  return {'test':name,'status':status,'exit':p.returncode,'seconds':round(time.monotonic()-start,3),'log':str(path)}
results=[]
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
 for result in pool.map(run,names):
  results.append(result)
  if result['status']!='passed':print(json.dumps(result),flush=True)
Path('/tmp/thegn-audit-combined-svc-results.json').write_text(json.dumps(results,indent=2)+'\n')
from collections import Counter
print('SUMMARY',dict(Counter(r['status'] for r in results)),'TOTAL',len(results),flush=True)
