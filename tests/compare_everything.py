#!/usr/bin/env python3
"""Compare live Rust queries with captured Everything 1.5 results. Arguments: dylib, db, fixture, reference, output."""
import ctypes,json,pathlib,sys
lib=ctypes.CDLL(str(pathlib.Path(sys.argv[1]).resolve()))
lib.filesearch_engine_open.argtypes=[ctypes.c_char_p];lib.filesearch_engine_open.restype=ctypes.c_void_p
lib.filesearch_engine_call.argtypes=[ctypes.c_void_p,ctypes.c_char_p];lib.filesearch_engine_call.restype=ctypes.c_void_p
lib.filesearch_engine_free_string.argtypes=[ctypes.c_void_p]
e=lib.filesearch_engine_open(str(pathlib.Path(sys.argv[2]).resolve()).encode())
def call(v):
 p=lib.filesearch_engine_call(e,json.dumps(v).encode());r=json.loads(ctypes.string_at(p));lib.filesearch_engine_free_string(p);return r
scan=call({'op':'scan','roots':[str(pathlib.Path(sys.argv[3]).resolve())],'watch':False,'wait':True})
print('Scan',scan,'Status',call({'op':'status'}))
comparisons=[]
for ref in json.loads(pathlib.Path(sys.argv[4]).read_text(encoding='utf-8-sig')):
 r=call({'op':'query','text':ref['query'],'limit':100});actual=[x['name'] for x in r.get('rows',[])]
 expected=[x['name'] for x in ref['rows']]
 if ref['query']=='folder:':
  # Root names/absolute path sorting differ between platforms; compare directory set.
  expected=[x.rsplit('\\',1)[-1].replace('fixture',pathlib.Path(sys.argv[3]).name) for x in expected]
  expected.sort();actual.sort()
 comparisons.append({'query':ref['query'],'expected':expected,'actual':actual,'pass':actual==expected,'error':r.get('error')})
pathlib.Path(sys.argv[5]).write_text(json.dumps(comparisons,ensure_ascii=False,indent=2))
for c in comparisons:
 if not c['pass']:print(c)
print('Passed',sum(c['pass'] for c in comparisons),'/',len(comparisons))
sys.exit(0 if all(c['pass'] for c in comparisons) else 1)
