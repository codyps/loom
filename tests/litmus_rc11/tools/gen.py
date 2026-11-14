import itertools, random, os, sys
random.seed(20261113)

def canon_ops(alpha_vars, orders, fence=True, xchg=True):
    ops=[]
    for v in alpha_vars:
        for o in orders:
            ops.append(('W',v,o)); ops.append(('R',v,o))
            if xchg: ops.append(('X',v,o))
    if fence: ops.append(('F',))
    return ops

def render(threads):
    # assign distinct store values
    val=0; out=[]
    for t in threads:
        toks=[]
        for op in t:
            if op[0]=='W': val+=1; toks.append(f"W{op[1]}{val}{op[2]}")
            elif op[0]=='X': val+=1; toks.append(f"X{op[1]}{val}{op[2]}")
            elif op[0]=='R': toks.append(f"R{op[1]}{op[2]}")
            else: toks.append("F")
        out.append(",".join(toks))
    return ";".join(out), val

def swapvars(threads, m):
    return tuple(tuple((op[0], m[op[1]], op[2]) if len(op)>1 else op for op in t) for t in threads)

def key(threads):
    # canonical under thread permutation and variable renaming
    best=None
    vars_=sorted({op[1] for t in threads for op in t if len(op)>1})
    for perm in itertools.permutations(vars_):
        m=dict(zip(vars_, perm))
        # rename to x,y,z by first-appearance is implied by trying all perms onto same set
        sw=swapvars(threads, m)
        for tp in itertools.permutations(sw):
            if best is None or tp<best: best=tp
    return best

progs={}
def add(threads, tag):
    # must touch memory at all, and every variable used should be written or read by >1 thread ideally
    if all(len(op)==1 for t in threads for op in t): return
    k=key(tuple(tuple(t) for t in threads))
    if k in progs: return
    s,val=render(k)
    if val>9: return
    progs[k]=(s,tag)

# A: exhaustive 2x2 over x,y with r/s
A=canon_ops("xy","rs")
for t0 in itertools.product(A,repeat=2):
    for t1 in itertools.product(A,repeat=2):
        add((t0,t1),"2x2")
nA=len(progs)

B=canon_ops("xy","ras")
C=canon_ops("xyz","rs")
def rnd(alpha, shape):
    return tuple(tuple(random.choice(alpha) for _ in range(n)) for n in shape)
def sample(n, alpha, shapes, tag):
    tries=0; start=len(progs)
    while len(progs)-start<n and tries<n*50:
        tries+=1
        add(rnd(alpha, random.choice(shapes)), tag)

sample(2500, A, [(3,2),(3,3),(2,3)], "2xN")
sample(2500, A, [(2,2,2),(2,2,1),(2,1,1)], "3x2")
sample(1500, B, [(2,2),(3,2),(2,2,2),(2,2,1)], "acqrel")
sample(1200, C, [(2,2,2),(1,1,2,2),(1,2,2)], "3var")
sample(1200, A, [(1,1,2,2),(1,2,2,2),(1,1,2,3)], "4thr")
sample(600, A, [(3,3,2),(3,2,2),(3,3,3)], "3x3")

ord_c={'r':'memory_order_relaxed','s':'memory_order_seq_cst'}
def c_order(o, kind):
    if o=='a': return {'W':'memory_order_release','R':'memory_order_acquire','X':'memory_order_acq_rel'}[kind]
    return ord_c[o]

os.makedirs("lit", exist_ok=True)
with open("programs.txt","w") as pf, open("index.txt","w") as idx:
    for n,(k,(s,tag)) in enumerate(progs.items()):
        name=f"t{n:05d}"
        pf.write(s+"\n"); idx.write(f"{name} {tag} {s}\n")
        locs=[]; body=[]
        for ti,t in enumerate(s.split(";")):
            lines=[]; r=0
            for tok in t.split(","):
                if tok[0]=='W':
                    lines.append(f"  atomic_store_explicit({tok[1]}, {tok[2]}, {c_order(tok[3],'W')});")
                elif tok[0]=='R':
                    lines.append(f"  int r{r} = atomic_load_explicit({tok[1]}, {c_order(tok[2],'R')});"); locs.append(f"{ti}:r{r}"); r+=1
                elif tok[0]=='X':
                    lines.append(f"  int r{r} = atomic_exchange_explicit({tok[1]}, {tok[2]}, {c_order(tok[3],'X')});"); locs.append(f"{ti}:r{r}"); r+=1
                else:
                    lines.append("  atomic_thread_fence(memory_order_seq_cst);")
            body.append(f"P{ti} (atomic_int* x, atomic_int* y, atomic_int* z) {{\n"+"\n".join(lines)+"\n}\n")
        with open(f"lit/{name}.litmus","w") as f:
            f.write(f"C {name}\n{{ [x] = 0; [y] = 0; [z] = 0; }}\n\n"+"\n".join(body)+"\nlocations ["+"; ".join(["x","y","z"]+locs)+";]\nexists (x=0)\n")
from collections import Counter
print(len(progs), "programs;", Counter(t for _,t in progs.values()))
