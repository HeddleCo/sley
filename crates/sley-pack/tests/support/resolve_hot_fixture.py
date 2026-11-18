"""Generate the fixed-seed, fixed-date 60k-commit hot-file timing fixture.
Run with Git 2.55.0 and a fresh scratch directory; never edits a checkout.
"""
import hashlib,json,os,random,subprocess,time
from pathlib import Path
import sys
root=Path(sys.argv[1]); root.mkdir(parents=True, exist_ok=True); repo=root/'hot.git'
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
