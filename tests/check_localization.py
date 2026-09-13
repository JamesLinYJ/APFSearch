#!/usr/bin/env python3
"""Validate source coverage, String Catalog compilation and Bundle locale selection.

All language overrides are per-process AppleLanguages launch arguments; this test
never writes the user's preferred languages or patches Foundation at runtime.
"""
import argparse,json,pathlib,plistlib,re,shutil,subprocess,tempfile
root=pathlib.Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser();parser.add_argument('--report',type=pathlib.Path);args=parser.parse_args()
pattern=re.compile(r'\bL[FT]?\(\s*("(?:[^"\\]|\\.)*")')
keys=set();unwrapped=[]
for source in (root/'macos').glob('*.swift'):
 text=source.read_text();matches=list(pattern.finditer(text));localized_spans={m.span(1) for m in matches}
 for m in matches:keys.add(json.loads(m.group(1)))
 for m in re.finditer(r'"(?:[^"\\]|\\.)*"',text):
  if re.search('[\u3400-\u9fff]',m.group()) and m.span() not in localized_spans:
   # Doc comments are not UI strings.
   line=text[text.rfind('\n',0,m.start())+1:m.start()]
   if not line.lstrip().startswith('//'):unwrapped.append({'file':source.name,'line':text.count('\n',0,m.start())+1,'literal':m.group()})
catalogs={table:json.loads((root/f'Resources/{table}.xcstrings').read_text()) for table in ['Localizable','Features']}
catalog={'sourceLanguage':'en','strings':{}}
for table,value in catalogs.items():
 assert not catalog['strings'].keys() & value['strings'].keys(), 'Duplicate catalog keys'
 catalog['strings'].update(value['strings'])
migration=json.loads((root/'Resources/LocalizationKeyMigration.json').read_text())['old_to_new']
assert len(set(migration.values()))==len(migration),'Migration identifiers must be unique'
assert set(migration.values())<=catalog['strings'].keys(),'Migrated identifiers must remain in the catalog'
assert not keys-catalog['strings'].keys(),f'Missing catalog keys: {keys-catalog["strings"].keys()}'
assert not unwrapped,f'Unlocalized Chinese literals: {unwrapped}'
def signature(s):
 tokens=re.findall(r'%(?:([0-9]+)\$)?([@diufg])',s);auto=0;out=[]
 for pos,kind in tokens:
  if not pos:auto+=1;pos=str(auto)
  out.append((int(pos),kind))
 return sorted(out)
for key,record in catalog['strings'].items():
 assert re.fullmatch(r'[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+',key),('Nonsemantic catalog key',key)
 source_value=record['localizations'][catalog['sourceLanguage']]['stringUnit']['value']
 for language in ['en','zh-Hans','zh-Hant']:
  unit=record['localizations'][language]['stringUnit'];assert unit['state']=='translated' and unit['value']
  assert signature(unit['value'])==signature(source_value),(key,language,'placeholder mismatch')
with tempfile.TemporaryDirectory(prefix='FileSearch-l10n-') as tmp:
 tmp=pathlib.Path(tmp);bundle=tmp/'Probe.app';contents=bundle/'Contents';resources=contents/'Resources';macos=contents/'MacOS';resources.mkdir(parents=True);macos.mkdir()
 for name in ['Localizable','Features','InfoPlist']:
  subprocess.run(['xcrun','xcstringstool','compile',str(root/f'Resources/{name}.xcstrings'),'--output-directory',str(resources)],check=True)
 plist={'CFBundleExecutable':'APFSearch','CFBundleIdentifier':'local.filesearch.app.localizationtest','CFBundleName':'APFSearch','CFBundlePackageType':'APPL','CFBundleDevelopmentRegion':'en','CFBundleLocalizations':['en','zh-Hans','zh-Hant']}
 (contents/'Info.plist').write_bytes(plistlib.dumps(plist))
 for language in ['en','zh-Hans','zh-Hant']:
  for table,value in catalogs.items():
   compiled=subprocess.check_output(['plutil','-convert','json','-o','-',str(resources/f'{language}.lproj/{table}.strings')]);strings=json.loads(compiled)
   assert strings=={key:v['localizations'][language]['stringUnit']['value'] for key,v in value['strings'].items()}
 probe=tmp/'Probe.swift';probe.write_text('''import Foundation
@main struct Probe { static func main() throws {
let output:[String:Any] = ["bundle":Bundle.main.bundlePath,"preferred":Bundle.main.preferredLocalizations,"locale":Locale.current.identifier,"title":L("app.name"),"settings":L("action.open_settings"),"folder":L("filter.folders"),"format":L("status.selected_count",localizedCount(12345)),"number":localizedCount(12345),"decimal":localizedDecimal(12.34,fractionDigits:2),"date":localizedDate(Date(timeIntervalSince1970:0)),"displayName":Bundle.main.object(forInfoDictionaryKey:"CFBundleDisplayName") as? String ?? "missing"]
print(String(data:try JSONSerialization.data(withJSONObject:output,options:[.sortedKeys,.withoutEscapingSlashes]),encoding:.utf8)!)
}}''')
 subprocess.run(['swiftc','-module-cache-path',str(tmp/'ModuleCache'),'-target','arm64-apple-macos15.0',str(root/'macos/Localization.swift'),str(probe),'-o',str(macos/'APFSearch')],check=True)
 for name in ['FileSearchService','filesearch-cli']:shutil.copy2(macos/'APFSearch',macos/name)
 observations=[]
 for language,locale,title,folder in [('en','en_US','APFSearch','Folders'),('zh-Hans','zh_CN','APFSearch','文件夹'),('zh-Hant','zh_TW','APFSearch','資料夾')]:
  for executable in ['APFSearch','FileSearchService','filesearch-cli']:
   result=json.loads(subprocess.check_output([str(macos/executable),'-AppleLanguages',f'({language})','-AppleLocale',locale]));assert result['title']==title and result['folder']==folder and result['displayName']==title,result
   assert result['bundle']==str(bundle),result
   result['executable']=executable;observations.append(result)
 # UI language and regional number/date preferences remain independent.
 regional=json.loads(subprocess.check_output([str(macos/'APFSearch'),'-AppleLanguages','(en)','-AppleLocale','de_DE']))
 assert regional['title']=='APFSearch' and regional['decimal']=='12,34',regional
 unsupported=[]
 for executable in ['APFSearch','FileSearchService','filesearch-cli']:
  result=json.loads(subprocess.check_output([str(macos/executable),'-AppleLanguages','(fr-FR)','-AppleLocale','fr_FR']))
  # Foundation can return the development localization more than once when
  # it is also explicitly declared. Check the selected language and real text.
  assert result['preferred'] and set(result['preferred'])=={'en'} and result['title']=='APFSearch' and result['folder']=='Folders' and result['settings']=='Settings…' and result['displayName']=='APFSearch',result
  result['executable']=executable;unsupported.append(result)
 report={'success':True,'string_catalog_keys':len(catalog['strings']),'localized_source_keys':len(keys),'languages':['en','zh-Hans','zh-Hant'],'fallback_language':'en','unlocalized_literals':unwrapped,'compiler':'xcrun xcstringstool','runtime_tests':observations,'unsupported_language_fallback_tests':unsupported,'independent_region_test':regional,'technical_detail_boundary':'Rust query errors, OS-supplied error.localizedDescription, user paths, file contents, and user-defined names are retained as technical/user data; they are not represented as fully translated application prose.'}
 if args.report:args.report.parent.mkdir(parents=True,exist_ok=True);args.report.write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')
 print(json.dumps({'success':True,'catalog_keys':len(catalog['strings']),'languages':report['languages'],'runtime_tests':len(observations)+len(unsupported)+1,'fallback_language':'en'},ensure_ascii=False))
