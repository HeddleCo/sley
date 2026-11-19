"""Public-API trusted timing gate: interleaved median of six, at 4/16 threads.

Usage: resolve_timing.py SCRATCH TARGET --fixtures FIXTURES [--only NAME]
A fresh SCRATCH holds source exports (not worktrees), build logs and timings.
FIXTURES holds whole.pack, mixed-ofs.pack, hot.pack, iroh.pack.
Use resolve_hot_fixture.py to generate hot.git; real packs are opt-in network
fixtures, pinned in tests/fixtures/resolved/timing-pins.json. All commands capture output.
"""
import argparse, hashlib, io, json, os, random, re, statistics, struct
import subprocess, tarfile, zlib, shutil
from pathlib import Path
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('scratch',type=Path); parser.add_argument('target',type=Path)
parser.add_argument('--fixtures',required=True,type=Path)
parser.add_argument('--only',action='append'); parser.add_argument('--trials',type=int,default=6)
parser.add_argument('--threads',type=int,nargs='+',default=[4,16])
parser.add_argument('--steal-disabled',action='store_true')
args=parser.parse_args(); scratch=args.scratch; target=args.target; fixtures=args.fixtures
scratch.mkdir(parents=True,exist_ok=False)
configured=Path(json.loads(subprocess.check_output(["cargo","metadata","--no-deps","--format-version","1"],text=True))["target_directory"])
assert target.resolve()==configured.resolve(), "use the configured Cargo target; wait if busy"
root=Path.cwd(); head=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()
refs={'base':'61848386','head':head}
example=(root/'crates/sley-pack/tests/support/resolve_timing.rs').read_bytes()
env=os.environ.copy(); env['RUSTUP_TOOLCHAIN']='stable'
for name,ref in refs.items():
    source=scratch/name; source.mkdir()
    with tarfile.open(fileobj=io.BytesIO(subprocess.check_output(['git','archive',ref]))) as tar:
        tar.extractall(source,filter='data')
    directory=source/'crates/sley-pack/examples'; directory.mkdir(exist_ok=True)
    (directory/'r7_timing.rs').write_bytes(example)
    if name=='head':
        (directory/'r7_bounded_timing.rs').write_bytes((root/'crates/sley-pack/tests/support/resolve_bounded_timing.rs').read_bytes())
    if name=='head' and args.steal_disabled:
        path=source/'crates/sley-pack/src/resolved.rs'; text=path.read_text()
        original=text
        text=text.replace('item = pop((worker + other) % workers, true)?;', 'let _ = other; item = None;')
        assert text!=original, 'steal guard mutation must apply'
        path.write_text(text)
    with (scratch/(name+'-build.log')).open('w') as log:
        subprocess.run(['cargo','build','--manifest-path',str(source/'Cargo.toml'),'--locked','--release','-p','sley-pack','--example','r7_timing'] + (['--example','r7_bounded_timing'] if name=='head' else []),env=env,stdout=log,stderr=subprocess.STDOUT,check=True)
    shutil.copy2(target/'release/examples/r7_timing',scratch/(name+'-timing'))
    if name=='head': shutil.copy2(target/'release/examples/r7_bounded_timing',scratch/'head-bounded-timing')

def header(kind,size):
    result=bytearray([kind<<4 | (size&15) | (128 if size>15 else 0)]); size>>=4
    while size:
        byte=size&127; size>>=7; result.append(byte | (128 if size else 0))
    return result

def varint(size):
    out=bytearray()
    while True:
        byte=size&127; size>>=7; out.append(byte | (128 if size else 0))
        if not size: return out

def ofs(distance):
    out=bytearray([distance&127]); distance>>=7
    while distance:
        distance-=1; out.append((distance&127)|128); distance>>=7
    return out[::-1]

fixtures.mkdir(parents=True,exist_ok=True)
for mode in ['whole','mixed-ofs']:
    path=fixtures/(mode+'.pack')
    if path.exists(): continue
    count=420000; payload=random.Random(1712).randbytes(880 if mode=='whole' else 1740)
    digest=hashlib.sha1(); offset=0
    with path.open('wb') as file:
        def emit(data):
            global offset
            file.write(data); digest.update(data); offset+=len(data)
        emit(b'PACK'+struct.pack('>II',2,count))
        for i in range(count if mode=='whole' else count//2):
            base=offset; body=struct.pack('>Q',i)+payload
            emit(header(3,len(body))); emit(zlib.compress(body,1))
            if mode=='mixed-ofs':
                delta=varint(len(body))+varint(len(body)); size=len(body)-1
                delta.extend([0xb0,size&255,size>>8,1,i&255]); at=offset
                emit(header(6,len(delta))); emit(ofs(at-base)); emit(zlib.compress(delta,1))
        file.write(digest.digest())

if hasattr(os,'sched_getaffinity'):
    os.sched_setaffinity(0,sorted(os.sched_getaffinity(0))[-16:])
rows=[]; summary=[]
for mode in args.only or ['whole','mixed-ofs','hot','iroh']:
    path=fixtures/(mode+'.pack')
    with path.open('rb') as file:
        objects=struct.unpack('>I',file.read(12)[8:])[0]
        file.seek(0); digest=hashlib.file_digest(file,'sha256').hexdigest()
    pins=json.loads((root/'crates/sley-pack/tests/fixtures/resolved/timing-pins.json').read_text())
    expected=420000 if mode in ['whole','mixed-ofs','hot'] else pins[mode]['objects']
    assert objects==expected, 'fixture object count drift'
    if mode=='hot':
        metadata=json.loads((fixtures/'hot.json').read_text())
        assert metadata['max_depth']==50 and metadata['largest_family']>=50000
        assert metadata['sha256']==digest and metadata['objects']==objects
    print(json.dumps(dict(fixture=mode,objects=objects,bytes=path.stat().st_size,sha256=digest)),flush=True)
    for threads in args.threads:
        for trial in range(1,args.trials+1):
            # Alternate order too, to reduce systematic thermal/cache bias.
            for name in (['base','head'] if trial%2 else ['head','base']):
                binary=scratch/(name+'-timing')
                result=subprocess.run([str(binary),str(path),str(threads)],capture_output=True,text=True,check=True)
                match=re.search(r'seconds=([\d.]+) objects=(\d+) idx_sha256=(\w+) tail=([\d.]+)',result.stdout)
                assert match,result.stdout
                assert int(match[2])==objects, 'indexer object count drift'
                row=dict(fixture=mode,threads=threads,version=name,stealing_disabled=bool(args.steal_disabled and name=='head'),ref=refs[name],trial=trial,seconds=float(match[1]),objects=int(match[2]),idx_sha256=match[3],tail=float(match[4]))
                rows.append(row); (scratch/'timings.json').write_text(json.dumps(rows,indent=2)); print(json.dumps(row),flush=True)
        group=[r for r in rows if r['fixture']==mode and r['threads']==threads]
        assert len({r['idx_sha256'] for r in group})==1
        medians={name:statistics.median(r['seconds'] for r in group if r['version']==name) for name in refs}
        tails={name:statistics.median(r['tail'] for r in group if r['version']==name) for name in refs}
        ratio=medians['head']/medians['base']; summary.append(dict(fixture=mode,threads=threads,medians=medians,tails=tails,head_over_base=ratio,gate=ratio<=1.05))
        print(json.dumps(summary[-1]),flush=True); (scratch/'summary.json').write_text(json.dumps(summary,indent=2))
    assert len({r['idx_sha256'] for r in rows if r['fixture']==mode})==1
assert all(row['gate'] for row in summary), 'trusted performance regression exceeds 5%'
