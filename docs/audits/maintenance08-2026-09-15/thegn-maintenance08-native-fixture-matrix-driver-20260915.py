import datetime,hashlib,json,os,re,subprocess,sys,time
from pathlib import Path
repo=Path('/tmp/thegn-maintenance-07-combined-20260915')
source=json.loads(Path('/tmp/thegn-maintenance08-primary-fixture-compile-approval-20260915.json').read_text())['source']
build=json.loads(Path('/tmp/thegn-maintenance08-native-fixture-artifact-admission-20260915.json').read_text())
assert build['source']==source and build['exit_code']==0 and len(build['artifacts'])==1
plan=json.loads(Path('/tmp/thegn-maintenance08-native-selection-plan-20260915.json').read_text())
selection=[]
for label in ['svc']:
 a=build['artifacts'][label];binary=Path(a['path']);assert hashlib.sha256(binary.read_bytes()).hexdigest()==a['binary_sha256']
 out=subprocess.check_output([str(binary),'--list'],cwd=repo,text=True)
 names=[line[:-6] for line in out.splitlines() if line.endswith(': test')]
 rules=plan[label];selected=[]
 for name in names:
  if any(name==x or name.endswith('::'+x) for x in rules.get('exclusions',[])):continue
  if any(name.startswith(prefix) for prefix in rules['prefixes']) or any(name.endswith('::'+suffix) for suffix in rules.get('exact_suffixes',[])):selected.append(name)
 assert selected,label
 for prefix in rules['prefixes']:assert any(n.startswith(prefix) for n in selected),(label,prefix)
 for suffix in rules.get('exact_suffixes',[]):assert sum(n.endswith('::'+suffix) for n in selected)==1,(label,suffix)
 selection.extend({'crate':label,'binary':str(binary),'selector':n} for n in selected)
manifest=Path('/tmp/thegn-maintenance08-native-fixture-selected-20260915.json')
manifest.write_text(json.dumps({'source':source,'selection':selection},indent=2)+'\n')
print(json.dumps({'selected':{label:sum(x['crate']==label for x in selection) for label in ['svc']},'manifest':str(manifest)}),flush=True)
if '--run' not in sys.argv:raise SystemExit(0)
cg=Path('/proc/self/cgroup').read_text().strip().split('::',1)[1]
assert Path('/sys/fs/cgroup'+cg+'/cpu.max').read_text().strip()=='100000 100000'
assert not subprocess.check_output(['git','status','--porcelain'],cwd=repo,text=True).strip()
assert subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip()==source
log=Path('/tmp/thegn-maintenance08-focused-native-fixture-20260915.log');receipt=Path('/tmp/thegn-maintenance08-focused-native-fixture-receipt-20260915.json')
r={'source':source,'started_at_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'tests':[],'log':str(log),'runner':plan['runner'],'runner_sha256':hashlib.sha256(Path(plan['runner']).read_bytes()).hexdigest()}
assert not log.exists() and not receipt.exists(), 'preserve previous native receipts'
with log.open('wb') as f:
 for row in selection:
  selector=row['selector'];cmd=['timeout','30s','python3',plan['runner'],row['binary'],selector,'--exact','--nocapture','--test-threads=1']
  start=time.monotonic();res=subprocess.run(cmd,cwd=repo,stdout=subprocess.PIPE,stderr=subprocess.STDOUT);output=res.stdout
  f.write(('\n=== '+row['crate']+' '+selector+' ===\n').encode());f.write(output);f.flush()
  passed=res.returncode==0 and re.search(rb'test result: ok\. 1 passed; 0 failed; 0 ignored;',output) is not None
  item={**row,'exit_code':res.returncode,'seconds':round(time.monotonic()-start,3),'exact_one_pass':passed,'output_sha256':hashlib.sha256(output).hexdigest()};r['tests'].append(item)
  receipt.write_text(json.dumps(r,indent=2)+'\n')
  if not passed:print(json.dumps({'failed':item}),flush=True)
r.update(finished_at_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),passed=sum(x['exact_one_pass'] for x in r['tests']),failed=sum(not x['exact_one_pass'] for x in r['tests']),log_sha256=hashlib.sha256(log.read_bytes()).hexdigest());receipt.write_text(json.dumps(r,indent=2)+'\n')
print(json.dumps({'passed':r['passed'],'failed':r['failed'],'receipt':str(receipt)}),flush=True)
raise SystemExit(0 if r['failed']==0 else 1)
