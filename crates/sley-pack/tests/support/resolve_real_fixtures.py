"""Opt-in network fixture builder. All revisions must be supplied as pinned OIDs.

resolve_real_fixtures.py DIR --network --iroh-pin SHA --heddle-pin SHA
Uses Git 2.55.0, a single pack thread and captured logs. No existing checkout is
read or changed. The timing driver consumes the resulting packs and manifests.
"""
import argparse,hashlib,json,re,struct,subprocess
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('directory',type=Path); p.add_argument('--network',action='store_true',required=True)
p.add_argument('--iroh-pin',required=True); p.add_argument('--heddle-pin',required=True)
a=p.parse_args(); a.directory.mkdir(parents=True,exist_ok=True)
assert subprocess.check_output(['git','--version'],text=True).strip()=='git version 2.55.0'
for name,repo,pin in [('iroh','n0-computer/iroh',a.iroh_pin),('heddle','HeddleCo/heddle-v7',a.heddle_pin)]:
    assert re.fullmatch('[0-9a-f]{40}',pin), 'a full pinned commit is required'
    bare=a.directory/(name+'.git'); path=a.directory/(name+'.pack')
    with (a.directory/(name+'-fixture.log')).open('wb') as log:
        if not bare.exists():
            subprocess.run(['git','init','--bare','-q',str(bare)],stdout=log,stderr=log,check=True)
        subprocess.run(['git','--git-dir='+str(bare),'fetch','--no-tags','https://github.com/'+repo+'.git',pin],stdout=log,stderr=log,check=True)
        with path.open('wb') as output:
            subprocess.run(['git','--git-dir='+str(bare),'-c','pack.threads=1','pack-objects','--stdout','--revs'],input=(pin+'\n').encode(),stdout=output,stderr=log,check=True)
    with path.open('rb') as file:
        count=struct.unpack('>I',file.read(12)[8:])[0]; file.seek(0)
        sha256=hashlib.file_digest(file,'sha256').hexdigest()
    row=dict(fixture=name,repository=repo,pin=pin,objects=count,bytes=path.stat().st_size,sha256=sha256)
    (a.directory/(name+'.json')).write_text(json.dumps(row,indent=2)+'\n')
    print(json.dumps(row),flush=True)
