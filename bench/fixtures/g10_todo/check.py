import todo,os,json
p='t.json'
if os.path.exists(p): os.remove(p)
t=todo.Todo(p); t.add('a'); t.add('b'); t.done(0)
assert t.list()==[{'text':'a','done':True},{'text':'b','done':False}],t.list()
assert todo.Todo(p).list()==t.list()
print('ok')
