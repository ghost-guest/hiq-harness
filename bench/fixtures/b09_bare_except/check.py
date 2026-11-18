import mod
assert mod.parse_int('42')==42
try:
    mod.parse_int('x')
except ValueError:
    print('ok')
else:
    raise AssertionError('silently swallowed the error')
