"""Generate the fixed-seed, fixed-date 60k-commit hot-file timing fixture.
Run with Git 2.55.0 and a fresh scratch directory; never edits a checkout.
"""
import hashlib,json,os,random,subprocess,time
from pathlib import Path
import sys
root=Path(sys.argv[1]); root.mkdir(parents=True, exist_ok=True); repo=root/'hot.git'
if '--verify-only' not in sys.argv:
    subprocess.run(['git','init','--bare','-q',str(repo)],check=True)
    rng=random.Random(1712)
    lines=[(f'package-{i:04d} '+rng.randbytes(128).hex()).encode()[:255]+b'\n' for i in range(1024)]
    assert sum(map(len,lines))==256*1024
    with (root/'hot-import.log').open('wb') as log:
     p=subprocess.Popen(['git','--git-dir='+str(repo),'fast-import','--quiet'],stdin=subprocess.PIPE,stdout=log,stderr=log)
     out=p.stdin
     for i in range(60000):
      for line in rng.sample(range(1024),3):
       lines[line]=(f'package-{line:04d} revision-{i:05d} '+rng.randbytes(128).hex()).encode()[:255]+b'\n'
      body=b''.join(lines)
      out.write(f'commit refs/heads/main\ncommitter Timing <timing@example.test> {1700000000+i} +0000\ndata 12\ncommit {i:05d}\n'.encode())
      out.write(f'M 100644 inline lock.txt\ndata {len(body)}\n'.encode());out.write(body);out.write(b'\n')
      small=f'{i:05d} {rng.randbytes(64).hex()}\n'.encode(); f=i%5000
      out.write(f'M 100644 inline d{f//1000}/d{f//100%10}/d{f//10%10}/f{f:04d}\ndata {len(small)}\n'.encode());out.write(small);out.write(b'\n\n')
      if i%10000==0: print(json.dumps({'hot_commits':i}),flush=True)
     out.write(b'done\n');out.close();assert p.wait()==0
    print('hot import complete',flush=True)
    with (root/'hot-repack.log').open('wb') as log:
     subprocess.run(['git','--git-dir='+str(repo),'-c','pack.threads=1','repack','-adf','--depth=50','--window=250'],stdout=log,stderr=log,check=True)
    print('hot repack complete',flush=True)

pack=next((repo/'objects/pack').glob('*.pack'))
index=pack.with_suffix('.idx')
with (root/'hot-verify.log').open('w') as log:
    subprocess.run(['git','verify-pack','-v',str(index)],stdout=log,check=True)
parents={}; histogram={}; count=0
for line in (root/'hot-verify.log').open():
    fields=line.split()
    if len(fields) not in [5,7] or len(fields[0])!=40: continue
    count+=1; depth=int(fields[5]) if len(fields)==7 else 0
    histogram[depth]=histogram.get(depth,0)+1
    parents[fields[0]]=fields[6] if len(fields)==7 else None
families={}
for oid in parents:
    ancestor=oid
    while parents[ancestor] is not None: ancestor=parents[ancestor]
    families[ancestor]=families.get(ancestor,0)+1
maximum=max(histogram); largest=max(families.values())
assert count==420000, count
assert maximum==50, maximum
assert largest>=50000, largest
import shutil
shutil.copyfile(pack,root/'hot.pack')
with pack.open('rb') as file: sha256=hashlib.file_digest(file,'sha256').hexdigest()
manifest=dict(fixture='hot',pin=subprocess.check_output(['git','--git-dir='+str(repo),'rev-parse','main'],text=True).strip(),objects=count,bytes=pack.stat().st_size,sha256=sha256,depth_histogram=histogram,max_depth=maximum,largest_family=largest)
(root/'hot.json').write_text(json.dumps(manifest,indent=2)+'\n')
print(json.dumps(manifest),flush=True)
