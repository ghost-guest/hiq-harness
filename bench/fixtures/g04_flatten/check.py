import flat
assert flat.flatten({'a':{'b':1},'c':[1,2]})=={'a.b':1,'c.0':1,'c.1':2}
assert flat.flatten({})=={}
print('ok')
