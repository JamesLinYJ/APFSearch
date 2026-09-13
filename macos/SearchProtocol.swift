import Foundation

let serviceName = ApplicationIdentity.serviceIdentifier
let protocolVersion = 2

// Version 2 uses success for operation status and retains structured messages:
// message_key/message_args and error_key/error_args identify String Catalog
// entries with typed text/integer/localized arguments. Conflict/warning arrays
// carry explicit *_messages records. Receivers localize only these fields;
// service process language never determines the GUI's application language.

@objc protocol SearchServiceProtocol {
    func request(_ data: Data, withReply reply: @escaping (Data) -> Void)
}

func jsonData(_ value: [String:Any]) -> Data {
    (try? JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])) ?? Data("{\"success\":false,\"error\":\"Encoding failed\"}".utf8)
}
func jsonObject(_ data: Data) -> [String:Any] {
    (try? JSONSerialization.jsonObject(with: data)) as? [String:Any] ?? ["success": false,"error":"Invalid response"]
}
