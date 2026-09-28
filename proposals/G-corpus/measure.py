"""Proposal G, section 8: lines, tokens and read-carrier variables per program,
before and after, from the named src blocks of proposals/G-corpus/*.org.

    python3 proposals/G-corpus/measure.py
"""
import re, sys, os

def strip_comments(src):
    out=[]; i=0; n=len(src); instr=False
    while i<n:
        c=src[i]
        if instr:
            out.append(c)
            if c=='\\' and i+1<n: out.append(src[i+1]); i+=2; continue
            if c=='"': instr=False
            i+=1; continue
        if c=='"': instr=True; out.append(c); i+=1; continue
        if c=='#' or (c=='/' and i+1<n and src[i+1]=='/'):
            while i<n and src[i]!='\n': i+=1
            continue
        out.append(c); i+=1
    return ''.join(out)

TOK=re.compile(r'"(?:\\.|[^"\\])*"|[A-Za-z_][A-Za-z0-9_]*|\d+|:-|==|!=|<=|>=|\+=|@[a-z]+|\S')

def tokens(src): return TOK.findall(strip_comments(src))
def lines(src): return [l for l in strip_comments(src).split('\n') if l.strip()]

def split_stmts(src):
    s=strip_comments(src); out=[]; cur=[]; depth=0; instr=False; i=0
    while i<len(s):
        c=s[i]; cur.append(c)
        if instr:
            if c=='\\': cur.append(s[i+1]); i+=2; continue
            if c=='"': instr=False
        elif c=='"': instr=True
        elif c in '([': depth+=1
        elif c in ')]': depth-=1
        elif c=='.' and depth==0 and (i+1==len(s) or s[i+1].isspace()):
            out.append(''.join(cur)); cur=[]
        i+=1
    if ''.join(cur).strip(): out.append(''.join(cur))
    return out

def args_at(text, start):
    # text[start] == '('; return list of top-level args
    depth=0; instr=False; args=[]; cur=''; i=start
    while i<len(text):
        c=text[i]
        if instr:
            cur+=c
            if c=='\\': cur+=text[i+1]; i+=2; continue
            if c=='"': instr=False
        elif c=='"': instr=True; cur+=c
        elif c in '([{':
            depth+=1
            if depth>1: cur+=c
        elif c in ')]}':
            depth-=1
            if depth==0: args.append(cur.strip()); return args
            cur+=c
        elif c==',' and depth==1: args.append(cur.strip()); cur=''
        else: cur+=c
        i+=1
    return args

READS={'attr':3,'arg':3,'setting':2,'output':2,'cloud_attr':3}
VAR=re.compile(r'^[A-Z][A-Za-z0-9_]*$')

def carriers_old(src):
    inputs=set(re.findall(r'\binput\s+(?!relation\b)([a-z_][a-z0-9_]*)\s*:', src))
    inputs.add('env')
    found=set()
    for k,st in enumerate(split_stmts(src)):
        # body = text after ':-' ; also `when p(X) {` guards
        parts=[]
        if ':-' in st: parts.append(st.split(':-',1)[1])
        m=re.match(r'\s*when\s+(.*?)\{', st, re.S)
        if m: parts.append(m.group(1))
        for body in parts:
            for m in re.finditer(r'\b([a-z_][a-z0-9_]*)\(', body):
                name=m.group(1); a=args_at(body, m.end()-1)
                if name in READS and len(a)>READS[name]:
                    v=a[READS[name]]
                elif name in inputs and len(a)==1:
                    v=a[0]
                else: continue
                if VAR.match(v): found.add((k,v))
    return len(found)

# New syntax: a variable bound to a read, `x = <ref>.<path>` in a body or clause.
NEWCARRY=re.compile(r'(?:^|[\s,])(?:if|for)?\s*([a-z_][a-z0-9_]*)\s*=\s*(?:world\.)?[a-z_][A-Za-z0-9_.]*(?:\[[^\]\n]*\])?(?:/[a-z_]+)?\.[A-Za-z_"][A-Za-z0-9_."\-\[\]]*\s*(?:,|$)', re.M)
def carriers_new(src, debug=False):
    s=strip_comments(src)
    n=0; stack=[]
    for line in s.split('\n'):
        t=line.strip()
        ctx = stack[-1] if stack else 'top'
        seg=None
        if ctx=='body': seg=t
        elif re.match(r'^(if|for)\b', t): seg=t
        elif re.search(r'\bif\b', t) and not t.endswith('{'): seg=t.split(' if ',1)[1] if ' if ' in t else None
        if seg:
            hits=NEWCARRY.findall(' '+seg)
            if debug and hits: print('   ', t, hits)
            n+=len(hits)
        opens=t.count('{')-t.count('}')
        if t.endswith('if {'): stack.append('body'); opens-=1
        while opens>0: stack.append('block'); opens-=1
        while opens<0 and stack: stack.pop(); opens+=1
    return n

def corpus(directory):
    """Named src blocks `before/F` and `after/F` from every .org file in directory."""
    import glob
    blocks={}
    for path in sorted(glob.glob(os.path.join(directory, '*.org'))):
        name=None; buf=None
        for line in open(path).read().split('\n'):
            if line.startswith('#+name: '): name=line[8:].strip(); continue
            if line.startswith('#+begin_src') and name: buf=[]; continue
            if line.startswith('#+end_src') and buf is not None:
                blocks[name]='\n'.join(buf); buf=None; name=None; continue
            if buf is not None: buf.append(line[1:] if line.startswith(',') else line)
    return blocks

ORDER="dform.df network.df database.df kubernetes.df iam.df baseline.df stdlib_net.df pngu.df dform-advanced.df gke_two_phase.df bootstrap.df workload.df crud_api.df".split()

if __name__=='__main__':
    b=corpus(os.path.dirname(os.path.abspath(__file__)))
    rows=[]
    for name in ORDER:
        old=b['before/'+name]; new=b['after/'+name]
        rows.append((name,len(lines(old)),len(lines(new)),len(tokens(old)),len(tokens(new)),carriers_old(old),carriers_new(new)))
    tot=[sum(r[i] for r in rows) for i in range(1,7)]
    print('| program | lines before | lines after | tokens before | tokens after | read-carrier vars before | after |')
    print('|-')
    for r in rows+[('total',*tot)]:
        lb,la,tb,ta=r[1],r[2],r[3],r[4]
        print(f'| {r[0]} | {lb} | {la} ({(la-lb)*100//lb:+d}%) | {tb} | {ta} ({round((ta-tb)*100/tb):+d}%) | {r[5]} | {r[6]} |')
