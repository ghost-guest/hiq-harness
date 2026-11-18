import lru
c=lru.LRU(2)
c.put('a',1); c.put('b',2); c.get('a'); c.put('c',3)
assert c.get('b') is None
assert c.get('a')==1 and c.get('c')==3
print('ok')
