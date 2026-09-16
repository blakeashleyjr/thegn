import datetime,hashlib,json,os,subprocess,sys,time
from pathlib import Path
phase=sys.argv[1]
assert phase in ('clippy','native')
repo=Path('/tmp/thegn-maintenance-07-combined-20260915')
approval=json.loads(Path('/tmp/thegn-maintenance08-primary-compile-approval-final-20260915.json').read_text())
assert approval['decision']=='approved for compiled gates'
source=approval['source']
for path,expected in approval['review_hashes'].items():
 assert hashlib.sha256(Path(path).read_bytes()).hexdigest()==expected,path
assert subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip()==source
assert not subprocess.check_output(['git','status','--porcelain'],cwd=repo,text=True).strip()
unit='thegn-maintenance08-'+phase+('-final' if phase=='clippy' else '')+'-20260915'
base=Path('/tmp/'+unit)
receipt=base.with_suffix('.json');log=base.with_suffix('.log');messages=base.with_suffix('.jsonl')
assert not receipt.exists(), 'do not overwrite a prior build receipt'
args=['cargo','clippy','--locked','--workspace','--all-targets','--keep-going','--','-D','warnings'] if phase=='clippy' else ['cargo','test','--locked','-p','thegn-svc','-p','thegn-host','--lib','--bin','thegn','--no-run','--message-format=json-render-diagnostics']
if phase=='native':
 prior=json.loads(Path('/tmp/thegn-maintenance08-clippy-final-20260915.json').read_text())
 assert prior['source']==source and prior['exit_code']==0
command=['systemd-run','--user','--scope','--quiet','--unit='+unit,'--property=CPUQuota=100%','nice','-n','10',*args]
def now():return datetime.datetime.now(datetime.timezone.utc).isoformat()
def writers():
 result=[]
 for p in Path('/proc').iterdir():
  if not p.name.isdigit():continue
  try:
   comm=(p/'comm').read_text().strip()
   if comm in ('cargo','rustc','clippy-driver'):
    cwd=os.readlink(p/'cwd')
    result.append({'pid':int(p.name),'comm':comm,'cwd_sha256':hashlib.sha256(os.fsencode(cwd)).hexdigest()})
  except OSError:pass
 return result
r={'source':source,'build_checkout':str(repo),'review_checkout':'/tmp/thegn-maintenance08-combined-20260915','command':command,'jobs':1,'cache':'/tmp/thegn-batch03-native-cache-f_3lx_t_','queued_at_utc':now(),'status':'waiting for existing compiler processes','competing_builds':writers()}
def save():receipt.write_text(json.dumps(r,indent=2)+'\n')
save();print(r['status'],flush=True)
start=time.monotonic()
while writers():
 if time.monotonic()-start>900:
  r.update(status='deferred',finished_at_utc=now());save();raise SystemExit(75)
 time.sleep(10)
env=os.environ.copy();env.update(RUSTC_WRAPPER='',CARGO_BUILD_JOBS='1',CARGO_TARGET_DIR=r['cache'],CARGO_INCREMENTAL='0')
r.update(status='running',started_at_utc=now());save();print('starting '+phase+' under one CPU quota',flush=True)
with log.open('wb') as stderr, messages.open('wb') as stdout:
 p=subprocess.Popen(command,cwd=repo,env=env,stdout=stdout if phase=='native' else stderr,stderr=stderr,start_new_session=True)
 verified=False
 for attempt in range(50):
  q=subprocess.run(['systemctl','--user','show',unit+'.scope','--property=ControlGroup','--property=CPUQuotaPerSecUSec'],capture_output=True,text=True)
  props=dict(line.split('=',1) for line in q.stdout.splitlines() if '=' in line)
  cg=props.get('ControlGroup','')
  if cg and Path('/sys/fs/cgroup'+cg+'/cpu.max').exists():
   cpu=Path('/sys/fs/cgroup'+cg+'/cpu.max').read_text().strip();r.update(cgroup=cg,cpu_max=cpu,systemd_properties=props);save()
   verified=cpu=='100000 100000';break
  if p.poll() is not None:break
  time.sleep(.2)
 if not verified:
  if p.poll() is None:subprocess.run(['systemctl','--user','kill',unit+'.scope'],check=False)
  code=p.wait();r.update(status='quota verification failed',exit_code=code);save();raise SystemExit(1)
 code=p.wait()
r.update(status='complete' if code==0 else 'failed; root review required',exit_code=code,finished_at_utc=now(),log=str(log),log_sha256=hashlib.sha256(log.read_bytes()).hexdigest(),messages=str(messages),messages_sha256=hashlib.sha256(messages.read_bytes()).hexdigest());save()
print(json.dumps({'status':r['status'],'exit_code':code,'log':str(log)}),flush=True)
raise SystemExit(code)
