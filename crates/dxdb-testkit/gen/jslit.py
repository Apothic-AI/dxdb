"""Convert an upstream JS object/array literal to Python data.

Deliberately strict: anything it does not understand raises, so a construct we
cannot represent is a loud failure rather than a silently dropped case.
"""
import re

class Unsupported(Exception): pass

TOKEN = re.compile(r"""
    (?P<ws>\s+)
  | (?P<comment>//[^\n]*|/\*.*?\*/)
  | (?P<str>'(?:\\.|[^'\\])*'|"(?:\\.|[^"\\])*")
  | (?P<num>-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?)
  | (?P<ident>[A-Za-z_$][\w$]*)
  | (?P<punct>[{}\[\]:,.])
  | (?P<other>.)
""", re.S | re.X)

ESC = {'n':'\n','t':'\t','r':'\r','b':'\b','f':'\f','0':'\0','\\':'\\',"'":"'",'"':'"','\n':''}

def unescape(s):
    out=[]; i=0
    while i < len(s):
        c=s[i]
        if c=='\\' and i+1 < len(s):
            n=s[i+1]
            if n=='u':
                out.append(chr(int(s[i+2:i+6],16))); i+=6; continue
            if n=='x':
                out.append(chr(int(s[i+2:i+4],16))); i+=4; continue
            out.append(ESC.get(n,n)); i+=2; continue
        out.append(c); i+=1
    return ''.join(out)

def tokens(src):
    pos=0
    while pos < len(src):
        m=TOKEN.match(src,pos)
        if not m: raise Unsupported('tokenize at %r' % src[pos:pos+40])
        pos=m.end()
        kind=m.lastgroup
        if kind in ('ws','comment'): continue
        yield kind, m.group()

class Parser:
    def __init__(self, src):
        self.toks=list(tokens(src)); self.i=0
    def peek(self, k=0):
        return self.toks[self.i+k] if self.i+k < len(self.toks) else (None,None)
    def take(self):
        t=self.peek(); self.i+=1; return t
    def value(self):
        kind,tok=self.peek()
        if kind=='punct' and tok=='{': return self.obj()
        if kind=='punct' and tok=='[': return self.arr()
        if kind=='str': self.take(); return unescape(tok[1:-1])
        if kind=='num': self.take(); return float(tok) if ('.' in tok or 'e' in tok.lower()) else int(tok)
        if kind=='ident':
            self.take()
            if tok=='true': return True
            if tok=='false': return False
            if tok=='null': return None
            if tok=='undefined': return UNDEFINED
            # `Number.MAX_SAFE_INTEGER` appears in one upstream schema
            # (`lastOpenedAt`'s `maximum`) and is 2**53 - 1.
            if tok=='Number':
                nk,nt=self.take()
                if nk=='punct' and nt=='.':
                    k2,t2=self.take()
                    if (k2,t2)==('ident','MAX_SAFE_INTEGER'): return 9007199254740991
                    if (k2,t2)==('ident','MIN_SAFE_INTEGER'): return -9007199254740991
                    raise Unsupported('Number.%s' % t2)
                raise Unsupported('bare Number')
            raise Unsupported('identifier %r' % tok)
        raise Unsupported('value at %r (kind %s)' % (tok,kind))
    def obj(self):
        self.take(); out={}
        while True:
            kind,tok=self.peek()
            if kind is None: raise Unsupported('unterminated object')
            if kind=='punct' and tok=='}': self.take(); return out
            if kind=='punct' and tok==',': self.take(); continue
            # key
            kind,tok=self.take()
            if kind=='str': key=unescape(tok[1:-1])
            elif kind=='num': key=tok
            elif kind=='ident': key=tok
            else: raise Unsupported('key %r kind %s' % (tok,kind))
            kind,tok=self.take()
            if not (kind=='punct' and tok==':'): raise Unsupported('expected : got %r' % (tok,))
            out[key]=self.value()
    def arr(self):
        self.take(); out=[]
        while True:
            kind,tok=self.peek()
            if kind is None: raise Unsupported('unterminated array')
            if kind=='punct' and tok==']': self.take(); return out
            if kind=='punct' and tok==',': self.take(); continue
            out.append(self.value())

class _Undefined:
    def __repr__(self): return 'undefined'
    def __bool__(self): return False
UNDEFINED=_Undefined()

def parse(src):
    p=Parser(src)
    v=p.value()
    if p.peek()[0] is not None: raise Unsupported('trailing tokens %r' % (p.peek(),))
    return v

def slice_balanced(lines, start_idx, opener='[', closer=']'):
    """Return source text from the balanced opener at/after start_idx."""
    text='\n'.join(lines[start_idx:])
    off=text.index(opener)
    depth=0; instr=None; i=off; esc=False
    while i < len(text):
        c=text[i]
        if instr:
            if esc: esc=False
            elif c=='\\': esc=True
            elif c==instr: instr=None
        elif c in '\'"': instr=c
        elif c=='/' and i+1<len(text) and text[i+1]=='/':
            j=text.find('\n',i); i=len(text) if j<0 else j; continue
        elif c==opener: depth+=1
        elif c==closer:
            depth-=1
            if depth==0: return text[off:i+1]
        i+=1
    raise Unsupported('unbalanced')
