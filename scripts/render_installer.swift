import AppKit

// Finder reads the logical image size from TIFF resolution metadata. Both
// representations share an 800 x 500 point canvas, including on Retina screens.
let canvas = NSSize(width: 800, height: 500)

func drawCentered(_ text: String, top: CGFloat, font: NSFont, color: NSColor) {
    let attributes: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: color]
    let size = (text as NSString).size(withAttributes: attributes)
    (text as NSString).draw(at: NSPoint(x: (canvas.width - size.width) / 2,
                                      y: canvas.height - top - size.height),
                           withAttributes: attributes)
}

do {
    guard CommandLine.arguments.count == 3,
          let background = NSImage(contentsOfFile: CommandLine.arguments[1]),
          background.size == canvas else {
        throw NSError(domain: "InstallerArtwork", code: 1, userInfo: [
            NSLocalizedDescriptionKey: "Usage: swift scripts/render_installer.swift <800x500.svg> <output.tiff>"
        ])
    }
    var representations = [NSBitmapImageRep]()
    for scale in [1, 2] {
        let width = Int(canvas.width) * scale, height = Int(canvas.height) * scale
        guard let colorSpace = CGColorSpace(name: CGColorSpace.sRGB),
              let context = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8,
                                      bytesPerRow: width * 4, space: colorSpace,
                                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else {
            throw NSError(domain: "InstallerArtwork", code: 2)
        }
        context.scaleBy(x: CGFloat(scale), y: CGFloat(scale))
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: false)
        background.draw(in: NSRect(origin: .zero, size: canvas))
        drawCentered("APFSearch", top: 40, font: .systemFont(ofSize: 29, weight: .semibold),
                     color: NSColor(srgbRed: 0.10, green: 0.16, blue: 0.25, alpha: 1))
        let secondary = NSColor(srgbRed: 0.40, green: 0.46, blue: 0.55, alpha: 1)
        drawCentered("File search for macOS", top: 80, font: .systemFont(ofSize: 14), color: secondary)
        drawCentered("Drag APFSearch into Applications", top: 365,
                     font: .systemFont(ofSize: 17, weight: .medium),
                     color: NSColor(srgbRed: 0.20, green: 0.28, blue: 0.39, alpha: 1))
        drawCentered("将 APFSearch 拖入 Applications 文件夹", top: 397,
                     font: .systemFont(ofSize: 13), color: secondary)
        NSGraphicsContext.restoreGraphicsState()
        guard let image = context.makeImage() else { throw NSError(domain: "InstallerArtwork", code: 3) }
        let representation = NSBitmapImageRep(cgImage: image)
        representation.size = canvas
        representations.append(representation)
    }
    guard let data = NSBitmapImageRep.tiffRepresentationOfImageReps(in: representations) else {
        throw NSError(domain: "InstallerArtwork", code: 4)
    }
    let output = URL(fileURLWithPath: CommandLine.arguments[2])
    try FileManager.default.createDirectory(at: output.deletingLastPathComponent(), withIntermediateDirectories: true)
    try data.write(to: output, options: .atomic)
} catch {
    FileHandle.standardError.write(Data("Installer artwork failed: \(error.localizedDescription)\n".utf8))
    exit(EXIT_FAILURE)
}
