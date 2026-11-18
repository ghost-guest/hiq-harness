import backoff
r=backoff.delays(5)
assert r[:3]==[0.1,0.2,0.4],r
assert r[-1]==1.0,r
assert backoff.delays(0)==[]
print('ok')
