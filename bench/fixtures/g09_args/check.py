import args
r=args.parse(['--name','x','-v','pos1'])
assert r=={'name':'x','v':True,'_':['pos1']},r
print('ok')
