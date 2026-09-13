import Foundation
import AppKit
import ImageIO
import UniformTypeIdentifiers
import CoreGraphics

@main struct ContentAndFileTests {
 static func main()throws{
    let folder=URL(fileURLWithPath:CommandLine.arguments[1])
    try FileManager.default.createDirectory(at:folder,withIntermediateDirectories:true)
    setenv("APFSEARCH_DATA_DIR",folder.appendingPathComponent("db").path,1)
    let engine=SearchEngine(),content=ContentIndexer(engine:engine),ops=FileOperations(directory:folder)
    var passed=[String](), failed=[String]()
    func check(_ name:String,_ condition:Bool)throws{
        if condition { passed.append(name) } else { failed.append(name) }
    }
    let parsed=try CSVReader.parse("Filename,Size\r\n\"/a/有,逗号.txt\",3\r\n\"/a/line\nquote\"\".txt\",4\r\n")
    try check("EFU CSV quoted comma newline and quote",parsed.count==3 && parsed[2][0]=="/a/line\nquote\".txt")
    let csvFixture = Data("\u{feff}Filename,Size\r\n\"C:\\离线\\a,\"\"b.txt\",4\r\n\"/line\nbreak.txt\",5\r\n".utf8)
    for chunk in [1, 2, 3, 7, 65536] {
      var records = [[String]]()
      var reader = CSVReader { records.append($0) }
      for start in stride(from: 0, to: csvFixture.count, by: chunk) {
        try reader.feed(csvFixture.subdata(in: start..<min(start + chunk, csvFixture.count)))
      }
      try reader.finish()
      try check("streamed CSV UTF-8 BOM quotes and CRLF at chunk \(chunk)", records == [["Filename", "Size"], ["C:\\离线\\a,\"b.txt", "4"], ["/line\nbreak.txt", "5"]])
    }
    func csvRejects(_ data: Data, limits: CSVLimits = CSVLimits()) -> Bool {
      do { var reader = CSVReader(limits: limits) { _ in }; try reader.feed(data); try reader.finish(); return false }
      catch { return true }
    }
    try check("CSV rejects malformed UTF-8 without replacement", csvRejects(Data([0x46, 0x0a, 0xc0, 0xaf, 0x0a])))
    try check("CSV rejects misplaced quote and unfinished quote", csvRejects(Data("a\"b,c".utf8)) && csvRejects(Data("\"unfinished".utf8)))
    var bounds = CSVLimits(); bounds.fieldBytes = 4
    try check("CSV enforces field bytes inside quotes", csvRejects(Data("\"12345\"".utf8), limits: bounds) && !csvRejects(Data("\"1234\"".utf8), limits: bounds))
    bounds = CSVLimits(); bounds.columns = 2
    try check("CSV enforces column budget including empty columns", csvRejects(Data(",,".utf8), limits: bounds) && !csvRejects(Data(",".utf8), limits: bounds))
    bounds = CSVLimits(); bounds.rows = 2
    try check("CSV counts empty records toward row budget", csvRejects(Data("a\n\n\n".utf8), limits: bounds) && !csvRejects(Data("a\n\n".utf8), limits: bounds))
    bounds = CSVLimits(); bounds.recordBytes = 6
    try check("CSV enforces record budget across many small fields", csvRejects(Data("a,b,c,d".utf8), limits: bounds))
    bounds = CSVLimits(); bounds.bytes = 5
    try check("CSV enforces cumulative bytes across chunks", try {
      var reader = CSVReader(limits: bounds) { _ in }; try reader.feed(Data("123".utf8))
      do { try reader.feed(Data("456".utf8)); return false } catch { return true }
    }())
    let cancelCSV = ImportJob()
    var cancelledRows = 0
    var cancelledReader = CSVReader(cancelled: { cancelCSV.isCancelled }) { _ in cancelledRows += 1; cancelCSV.cancel() }
    var cancelledCSV = false
    do { try cancelledReader.feed(Data("Filename\na\nb\n".utf8)); try cancelledReader.finish() } catch { cancelledCSV = true }
    try check("CSV cancellation stops inside an already-read chunk", cancelledCSV && cancelledRows == 1)
    let descriptorCSV = folder.appendingPathComponent("descriptor.efu")
    try "Filename\n/original.txt\n".write(to: descriptorCSV, atomically: true, encoding: .utf8)
    let csvAlias = folder.appendingPathComponent("descriptor-link.efu")
    try FileManager.default.createSymbolicLink(at: csvAlias, withDestinationURL: descriptorCSV)
    var aliasRows = [[String]]()
    try CSVReader.read(path: csvAlias.path) { aliasRows.append($0) }
    try check("CSV regular-file symlink keeps descriptor-based import compatibility", aliasRows.count == 2 && aliasRows[1][0] == "/original.txt")
    try check("CSV descriptor rejects directories before reading", {
      do { try CSVReader.read(path: folder.path) { _ in }; return false } catch { return true }
    }())
    try check("CSV descriptor rejects initial file byte budget", {
      var limits = CSVLimits(); limits.bytes = 5
      do { try CSVReader.read(path: descriptorCSV.path, limits: limits) { _ in }; return false } catch { return true }
    }())
    var changedRows = 0, changedRejected = false
    do {
      try CSVReader.read(path: descriptorCSV.path) { _ in
        changedRows += 1
        if changedRows == 1 {
          let handle = try FileHandle(forWritingTo: descriptorCSV)
          try handle.seekToEnd(); try handle.write(contentsOf: Data("/added.txt\n".utf8)); try handle.close()
        }
      }
    } catch { changedRejected = true }
    try check("CSV verifies descriptor metadata after reading a changing file", changedRejected)
    let file=folder.appendingPathComponent("测试 alpha.txt")
    try "内容中文 needle42\nsecond line".write(to:file,atomically:true,encoding:.utf8)
    let extracted=try content.extract(file)
    try check("UTF-8 Chinese content extraction",extracted.0.contains("内容中文 needle42"))
    let docs=folder.appendingPathComponent("docx",isDirectory:true)
    try FileManager.default.createDirectory(at:docs.appendingPathComponent("word"),withIntermediateDirectories:true)
    try "<?xml version=\"1.0\"?><w:document xmlns:w=\"urn:test\"><w:p><w:t>Docx 文本 42</w:t></w:p></w:document>".write(to:docs.appendingPathComponent("word/document.xml"),atomically:true,encoding:.utf8)
    let zip=Process();zip.executableURL=URL(fileURLWithPath:"/usr/bin/zip");zip.currentDirectoryURL=docs
    let docx=folder.appendingPathComponent("文档.docx")
    zip.arguments=["-q","-r",docx.path,"word"];try zip.run();zip.waitUntilExit()
    try check("DOCX extraction",try content.extract(docx).0.contains("Docx 文本 42"))
    let pdf=folder.appendingPathComponent("sample.pdf")
    let doc=NSAttributedString(string:"PDF searchable sample",attributes:[.font:NSFont.systemFont(ofSize:14)])
    let tv=NSTextView(frame:NSRect(x:0,y:0,width:500,height:100));tv.textStorage?.setAttributedString(doc)
    try tv.dataWithPDF(inside:tv.bounds).write(to:pdf)
    try check("PDFKit extraction",try content.extract(pdf).0.contains("PDF searchable"))
    func officeArchive(_ name: String, parts: [String: String]) throws -> URL {
        let directory = folder.appendingPathComponent(name + "-source", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        for (name, text) in parts {
            let path = directory.appendingPathComponent(name)
            try FileManager.default.createDirectory(at: path.deletingLastPathComponent(), withIntermediateDirectories: true)
            try text.write(to: path, atomically: true, encoding: .utf8)
        }
        let archive = folder.appendingPathComponent(name)
        if FileManager.default.fileExists(atPath: archive.path) { try FileManager.default.removeItem(at: archive) }
        let process = Process(); process.executableURL = URL(fileURLWithPath: "/usr/bin/zip")
        process.currentDirectoryURL = directory; process.arguments = ["-q", "-r", archive.path, "."]
        try process.run(); process.waitUntilExit()
        guard process.terminationStatus == 0 else { throw ExtractionError(reason: "Failed to create fixture archive") }
        return archive
    }
    let spreadsheet = try officeArchive("shared.xlsx", parts: [
        "xl/workbook.xml": "<workbook><sheets><sheet name=\"统计\"/></sheets></workbook>",
        "xl/sharedStrings.xml": "<sst><si><t>Shared 中文</t></si><si><r><t>Rich </t></r><r><t>text</t></r></si></sst>",
        "xl/worksheets/sheet1.xml": "<worksheet><sheetData><row><c t=\"s\"><v>0</v></c><c t=\"s\"><v>1</v></c><c t=\"inlineStr\"><is><t>inline 字符</t></is></c><c><v>42</v></c><c t=\"b\"><v>1</v></c></row></sheetData></worksheet>"
    ])
    let spreadsheetText = try content.extract(spreadsheet).0
    try check("XLSX resolves shared strings and rich runs", spreadsheetText.contains("Shared 中文\tRich text\t"))
    try check("XLSX inline numeric and boolean cells", spreadsheetText.contains("inline 字符\t42\tTRUE"))
    try check("XLSX does not index shared string offsets", !spreadsheetText.contains("0\t1\t"))
    let slides = try officeArchive("slides.pptx", parts: [
        "ppt/slides/slide1.xml": "<p:sld xmlns:p=\"urn:p\" xmlns:a=\"urn:a\"><a:p><a:t>Slide 中文 one</a:t></a:p></p:sld>",
        "ppt/slides/slide2.xml": "<p:sld xmlns:p=\"urn:p\" xmlns:a=\"urn:a\"><a:p><a:t>Slide two</a:t></a:p></p:sld>",
        "ppt/slides/slide10.xml": "<p:sld xmlns:p=\"urn:p\" xmlns:a=\"urn:a\"><a:p><a:t>Slide ten</a:t></a:p></p:sld>"
    ])
    let slidesText = try content.extract(slides).0
    try check("PPTX extraction", slidesText.contains("Slide 中文 one") && slidesText.contains("Slide ten"))
    try check("PPTX natural slide order", slidesText.range(of: "Slide two")!.lowerBound < slidesText.range(of: "Slide ten")!.lowerBound)
    func rejects(_ url: URL) -> Bool { do { _ = try content.extract(url); return false } catch { return true } }
    let invalidSheet = try officeArchive("invalid.xlsx", parts: [
        "xl/workbook.xml": "<workbook/>", "xl/sharedStrings.xml": "<sst><si><t>only</t></si></sst>",
        "xl/worksheets/sheet1.xml": "<worksheet><row><c t=\"s\"><v>99</v></c></row></worksheet>"
    ])
    try check("XLSX invalid shared string rejected", rejects(invalidSheet))
    let unsafe = try officeArchive("dtd.docx", parts: ["word/document.xml": "<!DOCTYPE x [<!ENTITY e SYSTEM \"file:///etc/passwd\">]><x>&e;</x>"])
    try check("OOXML DTD refused", rejects(unsafe))
    let incomplete = try officeArchive("missing.docx", parts: ["word/header1.xml": "<header>text</header>"])
    try check("Missing Office document body refused", rejects(incomplete))
    let unsupported = folder.appendingPathComponent("unsupported.xyz")
    try Data([0, 1, 2, 3]).write(to: unsupported)
    try check("Unsupported extractor reported", rejects(unsupported))
    let link = folder.appendingPathComponent("link.txt")
    if FileManager.default.fileExists(atPath: link.path) { try FileManager.default.removeItem(at: link) }
    try FileManager.default.createSymbolicLink(at: link, withDestinationURL: file)
    try check("Symlink content not followed", rejects(link))
    let png = folder.appendingPathComponent("dimensions.png")
    let colors = CGColorSpaceCreateDeviceRGB()
    let context = CGContext(data: nil, width: 32, height: 17, bitsPerComponent: 8, bytesPerRow: 32 * 4, space: colors, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    context.setFillColor(CGColor(red: 0.2, green: 0.4, blue: 0.8, alpha: 1)); context.fill(CGRect(x: 0, y: 0, width: 32, height: 17))
    let destination = CGImageDestinationCreateWithURL(png as CFURL, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(destination, context.makeImage()!, nil)
    try check("PNG fixture encoded", CGImageDestinationFinalize(destination))
    let imageProperties = try content.extract(png).1
    try check("ImageIO image dimensions", (imageProperties["width"] as? NSNumber)?.intValue == 32 && (imageProperties["height"] as? NSNumber)?.intValue == 17)
    var wav = Data()
    func ascii(_ value: String) { wav.append(contentsOf: value.utf8) }
    func little<T: FixedWidthInteger>(_ value: T) { var value = value.littleEndian; withUnsafeBytes(of: &value) { wav.append(contentsOf: $0) } }
    ascii("RIFF"); little(UInt32(36 + 1600)); ascii("WAVEfmt "); little(UInt32(16)); little(UInt16(1)); little(UInt16(1)); little(UInt32(8000)); little(UInt32(16000)); little(UInt16(2)); little(UInt16(16)); ascii("data"); little(UInt32(1600)); wav.append(Data(repeating: 0, count: 1600))
    let audio = folder.appendingPathComponent("duration.wav"); try wav.write(to: audio)
    let audioProperties = try content.extract(audio).1
    try check("AVFoundation async audio duration", abs((audioProperties["duration"] as? Double ?? 0) - 0.1) < 0.01)
    let large = folder.appendingPathComponent("large.txt")
    FileManager.default.createFile(atPath: large.path, contents: nil)
    let largeHandle = try FileHandle(forWritingTo: large); try largeHandle.truncate(atOffset: 33 * 1024 * 1024); try largeHandle.close()
    try check("Text extraction size limit enforced", rejects(large))
    content.cancel()
    try check("Content extraction respects cancellation", rejects(file))
    let copyDir=folder.appendingPathComponent("copied",isDirectory:true)
    try FileManager.default.createDirectory(at:copyDir,withIntermediateDirectories:true)
    let copying:[String:Any]=["action":"copy","paths":[file.path],"destination":copyDir.path]
    try check("copy succeeds",ops.perform(copying)["success"] as? Bool == true)
    var dry=copying;dry["dry_run"]=true
    let conflict=ops.perform(dry)
    try check("copy collision preflight",!(conflict["conflicts"] as? [String] ?? []).isEmpty)
    try check("copy refuses overwrite",ops.perform(copying)["success"] as? Bool == false)
    try check("undo copied file",ops.perform(["action":"undo"])["success"] as? Bool == true)
    try check("rename and journal",ops.perform(["action":"rename","paths":[file.path],"new_name":"renamed.txt"])["success"] as? Bool == true)
    try check("undo rename",ops.perform(["action":"undo"])["success"] as? Bool == true && FileManager.default.fileExists(atPath:file.path))
    try check("trash",ops.perform(["action":"trash","paths":[file.path]])["success"] as? Bool == true)
    try check("undo trash",ops.perform(["action":"undo"])["success"] as? Bool == true && FileManager.default.fileExists(atPath:file.path))
    try check("invalid rename",ops.perform(["action":"rename","paths":[file.path],"new_name":"../escape"])["success"] as? Bool == false)
    print(String(data:jsonData(["success":failed.isEmpty,"tests":passed,"failures":failed,"count":passed.count]),encoding:.utf8)!)
    if !failed.isEmpty { exit(1) }
 }
}
