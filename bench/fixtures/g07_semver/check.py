import semver
assert semver.cmp('1.2.3','1.10.0')==-1
assert semver.cmp('2.0.0','2.0.0')==0
assert semver.cmp('1.0.1','1.0.0')==1
print('ok')
