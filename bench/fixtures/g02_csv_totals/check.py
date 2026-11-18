import summary
r = summary.column_totals('key,value\na,1\nb,2\na,3\n')
assert r == {'a': 4, 'b': 2}, r
print('ok')
