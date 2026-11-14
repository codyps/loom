import sys, re, os, collections
loomdir=sys.argv[1]
idx=[l.split(' ',2) for l in open('index.txt').read().splitlines()]
byprog={p:(n,tag) for n,tag,p in idx}
def herd(name, prog):
    try: txt=open(f'hout/{name}.txt').read()
    except FileNotFoundError: return None
    if 'FAILED-OR-TIMEOUT' in txt or 'States' not in txt: return None
    nthreads=prog.count(';')+1
    out=set()
    for line in txt.splitlines():
        if '=' not in line or line.startswith(('Condition','Hash','Observation','Time','Test')): continue
        if not re.match(r'^(\d+:r\d+=|\[[xyz]\]=)', line.strip()): continue
        regs=collections.defaultdict(dict); mem={}
        for item in line.strip().rstrip(';').split(';'):
            item=item.strip()
            m=re.match(r'(\d+):r(\d+)=(\d+)', item)
            if m: regs[int(m.group(1))][int(m.group(2))]=m.group(3); continue
            m=re.match(r'\[([xyz])\]=(\d+)', item)
            if m: mem[m.group(1)]=m.group(2)
        s=''.join(f"{t}:{','.join(regs[t][r] for r in sorted(regs[t]))}|" for t in range(nthreads))
        s+=f"x={mem['x']},y={mem['y']},z={mem['z']}"
        out.add(s)
    return out
loom={}; complete={}
for f in sorted(os.listdir(loomdir)):
    if not re.match(r'^p\d+$', f): continue
    for line in open(os.path.join(loomdir,f)):
        line=line.rstrip('\n')
        if ' => ' in line:
            p,_,o=line.partition(' => '); complete[p]=True
        else:
            p,_,o=line.partition(' ~> '); complete[p]=False
        loom[p]=set(o.split())
stats=collections.Counter(); extra=[]; missing=[]
for n,tag,p in idx:
    h=herd(n,p)
    if h is None: stats['herd-missing']+=1; continue
    if p not in loom: stats['loom-missing']+=1; continue
    l=loom[p]
    if 'PANIC' in l: stats['loom-panic']+=1; extra.append((n,tag,p,['PANIC'])); continue
    stats['compared']+=1; stats[f'compared-{tag}']+=1
    if l-h: stats['loom-allows-forbidden']+=1; stats[f'forbidden-{tag}']+=1; extra.append((n,tag,p,sorted(l-h)))
    if not complete[p]:
        stats['incomplete (only checked for forbidden)']+=1
        continue
    stats['complete']+=1
    if l==h: stats['equal']+=1
    elif h-l: stats['loom-misses-allowed']+=1; stats[f'misses-{tag}']+=1; missing.append((n,tag,p,sorted(h-l)))
for k in sorted(stats): print(f"{k:28} {stats[k]}")
with open(os.path.join(loomdir,'extra.txt'),'w') as f:
    for e in extra: f.write(f"{e[0]} {e[1]} {e[2]} :: {' '.join(e[3])}\n")
with open(os.path.join(loomdir,'missing.txt'),'w') as f:
    for e in missing: f.write(f"{e[0]} {e[1]} {e[2]} :: {' '.join(e[3])}\n")
