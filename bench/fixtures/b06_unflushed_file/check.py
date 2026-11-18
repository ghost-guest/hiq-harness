import mod, tempfile, os
p = tempfile.mktemp()
open(p, 'w', encoding='utf-8').write('old')
mod.write(p, 'new')
got = open(p, encoding='utf-8').read()
assert got == 'new', repr(got)
os.remove(p)
print('ok')
