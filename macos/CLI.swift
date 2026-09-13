import Foundation

@main struct CommandLineMain {
  static func main() {
    var args = Array(CommandLine.arguments.dropFirst())
    var request: [String: Any]
    if args.first == "--json" {
      args.removeFirst()
      let raw =
        args.isEmpty
        ? FileHandle.standardInput.readDataToEndOfFile() : Data(args.joined(separator: " ").utf8)
      request = jsonObject(raw)
    } else {
      let op = args.first ?? "help"
      if !args.isEmpty { args.removeFirst() }
      switch op {
      case "status": request = ["op": "status"]
      case "volumes": request = ["op": "volumes"]
      case "search":
        request = ["op": "query", "text": args.joined(separator: " "), "limit": 200, "offset": 0]
      case "scan": request = ["op": "scan", "roots": args]
      case "cancel": request = ["op": "cancel", "request_id": args.first ?? ""]
      default:
        print(
          L(
            "cli.usage"
          ))
        return
      }
    }
    var done = false
    var result: [String: Any] = [:]
    SearchClient.shared.call(request) { r in
      result = r
      done = true
    }
    let deadline = Date().addingTimeInterval(300)
    while !done && Date() < deadline { RunLoop.current.run(until: Date().addingTimeInterval(0.02)) }
    if !done { result = ["success": false, "error": L("error.request_timeout")] }
    let output =
      (try? JSONSerialization.data(
        withJSONObject: result, options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]))
      ?? jsonData(result)
    FileHandle.standardOutput.write(output)
    print("")
    exit(result["success"] as? Bool == true ? 0 : 1)
  }
}
