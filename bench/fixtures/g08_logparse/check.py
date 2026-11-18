import logparse
r=logparse.parse('[2026-01-02 03:04:05] ERROR db: boom')
assert r=={'ts':'2026-01-02 03:04:05','level':'ERROR','source':'db','message':'boom'},r
assert logparse.parse('nope') is None
print('ok')
