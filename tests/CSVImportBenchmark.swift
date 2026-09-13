// Compile against the same source list as ServiceTests. Add -D STREAMING_CSV
// for the current parser; omit it for the sealed baseline. The fixture path is
// explicit and only that regular CSV is read. Use /usr/bin/time -l for peak RSS.
import Foundation
import Darwin
@main struct CSVImportBenchmark {
  static func main() throws {
    let path = CommandLine.arguments[1]
    let started = ProcessInfo.processInfo.systemUptime
    var rows = 0, bytes = 0
    #if STREAMING_CSV
    try CSVReader.read(path: path) { record in
      rows += 1; bytes += record.reduce(0) { $0 + $1.utf8.count }
    }
    #else
    let records = try CSVReader.parse(String(contentsOfFile: path, encoding: .utf8))
    for record in records { rows += 1; bytes += record.reduce(0) { $0 + $1.utf8.count } }
    #endif
    var usage = rusage()
    let measured = getrusage(RUSAGE_SELF, &usage) == 0
    print("{\"peak_rss_bytes\":\(measured ? usage.ru_maxrss : -1),\"rows\":\(rows),\"field_bytes\":\(bytes),\"elapsed_seconds\":\(ProcessInfo.processInfo.systemUptime - started)}")
  }
}
