import mod, time
t = time.time()
r = mod.fib(30)
dt = time.time() - t
assert r == 832040, r
assert dt < 0.02, f'too slow: {dt:.3f}s'
print('ok')
