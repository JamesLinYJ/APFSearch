import Foundation

/// Resolves only selected pages against one leased generation. The caller owns
/// cancellation and decides whether the completed selection is still relevant.
/// Transport injection keeps pagination and failure cases independently testable.
final class SelectionResolver {
  typealias Row = [String: Any]
  typealias Send = (Row, @escaping (Row) -> Void) -> Void
  private let selected: IndexSet
  private let request: Row
  private let total: Int
  private let send: Send
  private let complete: (Result<[Row], Error>) -> Void
  private let pageSize = 1000
  private var rows: [Int: Row]
  private var pages: [Int]
  private var position = 0
  private var stopped = false

  init(selected: IndexSet, cached: [Int: Row], total: Int, request: Row,
    send: @escaping Send, completion: @escaping (Result<[Row], Error>) -> Void) {
    self.selected = selected; self.total = total; self.request = request
    self.send = send; complete = completion
    rows = cached.filter { selected.contains($0.key) && $0.value["path"] is String }
    // A compressed IndexSet may represent a very large selection. Reject it
    // before materializing page IDs or allocating per-row operation metadata.
    if selected.count <= 100_000 {
      var needed = IndexSet()
      for range in selected.rangeView {
        needed.insert(integersIn: (range.lowerBound / pageSize)...((range.upperBound - 1) / pageSize))
      }
      pages = Array(needed)
    } else { pages = [] }
  }
  func cancel() { stopped = true }
  func start() {
    guard selected.count <= 100_000 else { finish(.failure(LT("files.batch_limit"))); return }
    guard selected.first.map({ $0 >= 0 }) ?? true, selected.last.map({ $0 < total }) ?? true else { finish(.failure(LT("files.selection_incomplete"))); return }
    advance()
  }
  private func finish(_ result: Result<[Row], Error>) {
    guard !stopped else { return }; stopped = true; complete(result)
  }
  private func advance() {
    guard !stopped else { return }
    while position < pages.count {
      let lower = pages[position] * pageSize
      let upper = min(total, lower + pageSize)
      let needed = selected.intersection(IndexSet(integersIn: lower..<upper))
      if needed.allSatisfy({ rows[$0] != nil }) { position += 1; continue }
      guard request["snapshot_lease"] is String else { finish(.failure(LT("files.selection_expired"))); return }
      var pageRequest = request
      pageRequest["op"] = "query"; pageRequest["offset"] = lower; pageRequest["limit"] = pageSize
      send(pageRequest) { [weak self] reply in
        guard let self, !self.stopped else { return }
        guard reply["success"] as? Bool == true else {
          self.finish(.failure(NSError(domain: "APFSearch.Selection", code: 1,
            userInfo: [NSLocalizedDescriptionKey: reply["error"] as? String ?? L("files.selection_expired")]))); return
        }
        guard (reply["offset"] as? NSNumber)?.intValue == lower,
          (reply["total"] as? NSNumber)?.intValue == self.total,
          let generation = reply["generation"] as? NSNumber,
          generation == self.request["generation"] as? NSNumber,
          let incoming = reply["rows"] as? [Row], incoming.count == upper - lower else {
          self.finish(.failure(LT("files.selection_incomplete"))); return
        }
        for (offset, row) in incoming.enumerated() where needed.contains(lower + offset) {
          guard row["path"] is String else { self.finish(.failure(LT("files.selection_incomplete"))); return }
          self.rows[lower + offset] = row
        }
        self.position += 1
        // Do not recurse on a synchronous test transport or cached response.
        DispatchQueue.main.async { [weak self] in self?.advance() }
      }
      return
    }
    let result = selected.compactMap { rows[$0] }
    guard result.count == selected.count else { finish(.failure(LT("files.selection_incomplete"))); return }
    finish(.success(result))
  }
}
