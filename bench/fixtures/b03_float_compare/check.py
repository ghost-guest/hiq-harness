import mod
r=mod.total([0.1,0.2])
assert abs(r-0.3)<1e-9,r
print('ok')
