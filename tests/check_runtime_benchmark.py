#!/usr/bin/env python3
"""Exercise benchmark deadlines and scope handling without contacting a service."""
from test_bundle import MINIMUM_MACOS_VERSION, swift_target
import json
import os
import pathlib
import subprocess
import tempfile

PROJECT = pathlib.Path(__file__).resolve().parents[1]
STUB = r'''import Foundation
final class SearchClient {
    static var waiting = [String: ([String: Any]) -> Void]()
    static var texts = [String: String]()
    static var sequence = 0
    func call(_ request: [String: Any], completion: @escaping ([String: Any]) -> Void) {
        let trace = URL(fileURLWithPath: ProcessInfo.processInfo.environment["RUNTIME_BENCHMARK_TEST_TRACE"]!)
        var line = jsonData(request); line.append(10)
        if !FileManager.default.fileExists(atPath: trace.path) { FileManager.default.createFile(atPath: trace.path, contents: nil) }
        let handle = try! FileHandle(forWritingTo: trace); try! handle.seekToEnd(); try! handle.write(contentsOf: line); try! handle.close()
        DispatchQueue.main.async {
            let operation = request["op"] as? String ?? ""
            switch operation {
            case "status": completion(["success": ProcessInfo.processInfo.environment["RUNTIME_BENCHMARK_TEST_NOT_READY"] == nil, "count": 4, "generation": 42, "scanning": true])
            case "retain_snapshot": completion(["success": true, "snapshot_lease": "scoped-test-lease", "generation": 42])
            case "release_snapshot": completion(["success": true, "released": true])
            case "cancel":
                let identifier = request["request_id"] as! String
                completion(["success": true])
                if Self.texts[identifier] == "timeout" { Self.waiting.removeValue(forKey: identifier)?(["success": false, "error": "cancelled"]) }
            case "query":
                let identifier = request["request_id"] as! String, text = request["text"] as! String
                if text == "timeout" || text == "no_drain" {
                    Self.waiting[identifier] = completion; Self.texts[identifier] = text; return
                }
                Self.sequence += 1
                let offset = request["offset"] as! Int, limit = request["limit"] as! Int
                let suffix = text == "unstable" ? "-\(Self.sequence)" : ""
                let rows = (offset..<min(4, offset + limit)).map { ["path": "/fixture/\($0)\(suffix)", "name": "\($0)\(suffix)"] }
                completion(["success": true, "generation": 42, "total": 4, "rows": rows, "elapsed_ms": 0.1])
            default: completion(["success": false, "error": "Unexpected operation"])
            }
        }
    }
}
'''
with tempfile.TemporaryDirectory(prefix='RuntimeBenchmark-tests-') as directory:
    work = pathlib.Path(directory)
    stub = work/'SearchClient.swift'; stub.write_text(STUB)
    executable = work/'RuntimeBenchmarkTests'
    subprocess.run(['swiftc','-module-cache-path',str(work/'ModuleCache'),'-swift-version','5','-O','-target',swift_target(),str(PROJECT/'macos/ApplicationIdentity.swift'),str(PROJECT/'macos/SearchProtocol.swift'),str(stub),str(PROJECT/'tests/RuntimeBenchmark.swift'),'-o',str(executable)],check=True)
    passed=[]
    def check(name, condition):
        assert condition, name
        passed.append(name)
    list_id='11111111-1111-1111-1111-111111111111'
    def run(name, queries, **options):
        config={'list_id':list_id,'queries':queries,'sort':[{'field':'path','ascending':False},{'field':'name','ascending':True}], 'runs':2,'warmups':1,'timeout':0.01,'setup_timeout':0.02,'pages':2,'page_size':2,'application_bundle':str(work/'Missing.app')}
        config.update(options)
        source=work/(name+'-config.json'); source.write_text(json.dumps(config))
        output=work/(name+'.json'); trace=work/(name+'-requests.jsonl')
        environment=dict(os.environ,RUNTIME_BENCHMARK_TEST_TRACE=str(trace))
        if name=='not-ready': environment['RUNTIME_BENCHMARK_TEST_NOT_READY']='1'
        result=subprocess.run([str(executable),str(source),str(output)],env=environment,text=True,capture_output=True,timeout=20)
        if not output.exists(): raise AssertionError(result.stderr)
        return json.loads(output.read_text()), [json.loads(line) for line in trace.read_text().splitlines()]
    report, requests=run('fast',[{'name':'fast','text':'fast','expected_total':4}])
    check('stable two-page runs and expected totals pass', report['complete'] and report['success'])
    check('warmup is retained but measured sample count excludes it', len(report['queries'][0]['samples'])==3 and report['queries'][0]['measured_samples']==2)
    check('retain query and release all carry the offline scope', all(row.get('list_id')==list_id for row in requests if row['op'] in ['retain_snapshot','query','release_snapshot']))
    check('queries use retained generation and lease with explicit sort', all(row.get('generation')==42 and row.get('snapshot_lease')=='scoped-test-lease' and row['sort'][0]['field']=='path' for row in requests if row['op']=='query'))
    check('every successful page has a stable digest', all(page['page_sha256'] and page['generation_matches_lease'] for sample in report['queries'][0]['samples'] for page in sample['pages']))
    timed, requests=run('timeout',[{'name':'timeout','text':'timeout'}],pages=1)
    check('timed-out samples remain censored and cannot pass target', not timed['success'] and not timed['targets_all_passed'] and timed['queries'][0]['timed_out_samples']==2 and timed['queries'][0]['p95_is_lower_bound'] and timed['queries'][0]['p95_lower_bound_ms']>=10)
    identifiers={row['request_id'] for row in requests if row['op']=='query'}
    cancelled=[row for row in requests if row['op']=='cancel']
    check('cancellation addresses only each unique query and same list', len(identifiers)==3 and len(cancelled)==3 and all(row['request_id'] in identifiers and row['list_id']==list_id for row in cancelled))
    check('timed-out drained queries still release the snapshot', timed['release']['released'])
    stopped, requests=run('no-drain',[{'name':'blocked','text':'no_drain'},{'name':'later','text':'fast'}],runs=1,warmups=0,pages=1)
    check('undrained timeout prevents subsequent heavy queries', len([row for row in requests if row['op']=='query'])==1 and stopped['queries'][1]['not_run'] and not stopped['complete'])
    check('aborted benchmark still releases its snapshot', stopped['release']['released'])
    unstable,_=run('unstable',[{'name':'unstable','text':'unstable','expected_total':4}],warmups=0)
    check('changed page digest is not accepted as stable results', not unstable['success'])
    unready,requests=run('not-ready',[{'name':'fast','text':'fast'}],runs=1,warmups=0)
    check('unready dataset never queues lease allocation or query', not unready['complete'] and unready['lease_allocated'] is False and not any(row['op'] in ['retain_snapshot','query'] for row in requests))
    result={'success':True,'count':len(passed),'passed':passed,'scope':'Production RuntimeBenchmark with isolated asynchronous transport; no real XPC, index, preferences or files changed. Real XPC baselines are separate.'}
    (PROJECT/'validation').mkdir(parents=True, exist_ok=True)
    (PROJECT/'validation/runtime-benchmark-tests.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result))
