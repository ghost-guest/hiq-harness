import mod, tempfile, os
p = tempfile.mktemp()
open(p, 'w', encoding='utf-8').write('中文内容')
got = mod.read_text(p)
assert got == '中文内容', repr(got)
os.remove(p)
print('ok')
