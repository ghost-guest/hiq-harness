import wc
r=wc.top_words('b a b c a b',2)
assert r==[('b',3),('a',2)],r
assert wc.top_words('',3)==[]
print('ok')
