import AppKit

/// Main-thread demand and cache; bounded workers perform filesystem icon lookup.
/// Only visible paths remain queued when the viewport or query changes.
final class ResultIconLoader {
    private static let workers: OperationQueue = {
        let queue = OperationQueue()
        queue.name = "APFSearch.ResultIcons"
        queue.qualityOfService = .utility
        queue.maxConcurrentOperationCount = 2
        return queue
    }()

    private let cache = NSCache<NSString, NSImage>()
    private var pending: [String: BlockOperation] = [:]
    private let load: (String) -> NSImage
    var imageAvailable: (() -> Void)?

    init(load: @escaping (String) -> NSImage = { NSWorkspace.shared.icon(forFile: $0) }) {
        self.load = load
        cache.countLimit = 512
    }

    deinit { pending.values.forEach { $0.cancel() } }

    func cachedImage(for path: String) -> NSImage? { cache.object(forKey: path as NSString) }

    func updateDemand(_ paths: Set<String>) {
        dispatchPrecondition(condition: .onQueue(.main))
        for path in Array(pending.keys) where !paths.contains(path) {
            pending.removeValue(forKey: path)?.cancel()
        }
        for path in paths where pending[path] == nil && cachedImage(for: path) == nil {
            let operation = BlockOperation()
            let load = self.load
            operation.addExecutionBlock { [weak self, weak operation] in
                guard let operation, !operation.isCancelled else { return }
                let image = autoreleasepool { load(path) }
                // OS lookup already in progress cannot be interrupted. Discard
                // it on completion if its demand was cancelled or replaced.
                DispatchQueue.main.async { [weak self] in
                    guard let self, self.pending[path] === operation else { return }
                    self.pending.removeValue(forKey: path)
                    guard !operation.isCancelled else { return }
                    self.cache.setObject(image, forKey: path as NSString)
                    self.imageAvailable?()
                }
            }
            pending[path] = operation
            Self.workers.addOperation(operation)
        }
    }
}
