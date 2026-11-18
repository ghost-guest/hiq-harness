import mod
from datetime import datetime, timezone, timedelta
v=mod.now_utc()
assert v.tzinfo is not None,'naive datetime'
assert abs((v - datetime.now(timezone.utc)).total_seconds()) < 5
print('ok')
