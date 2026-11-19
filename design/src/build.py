import re,os
d=os.path.dirname(os.path.abspath(__file__))
s=open(os.path.join(d,'src.html')).read()
def strip(s,pats):
    for a,b in pats: s=re.sub(a,b,s,flags=re.S)
    return s
prod=strip(s,[(r'<!--@ART-->.*?<!--@/ART-->\n?',''),(r'/\*@ART\*/.*?/\*@/ART\*/\n?',''),(r'//@ART.*?//@/ART\n?','')])
open(os.path.join(d,'..','live.html'),'w').write(prod)
art=re.sub(r'<!--@/?ART-->|/\*@/?ART\*/|//@/?ART','',s)
art=re.sub(r'<!doctype html>\s*<html[^>]*>\s*<head>','',art)
art=re.sub(r'</head>\s*<body>','',art)
art=art.replace('</body>\n</html>','')
open(os.path.join(d,'artifact.html'),'w').write(art)
