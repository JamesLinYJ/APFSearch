import AppKit
import ImageIO
import UniformTypeIdentifiers

enum IconBuildError: Error, CustomStringConvertible {
    case invalidArguments
    case invalidSource
    case renderFailed(Int)
    case encodingFailed(Int)
    case conversionFailed(Int32)

    var description: String {
        switch self {
        case .invalidArguments:
            return "Usage: swift scripts/build_icon.swift <source.svg> <output.icns>"
        case .invalidSource:
            return "The icon source must be a readable, square image."
        case .renderFailed(let pixels):
            return "Could not render the icon at \(pixels) pixels."
        case .encodingFailed(let pixels):
            return "Could not encode the icon at \(pixels) pixels."
        case .conversionFailed(let status):
            return "iconutil failed with exit status \(status)."
        }
    }
}

func renderIcon(_ image: NSImage, pixels: Int, destination: URL) throws {
    guard let colorSpace = CGColorSpace(name: CGColorSpace.sRGB),
          let context = CGContext(data: nil, width: pixels, height: pixels,
                                  bitsPerComponent: 8, bytesPerRow: pixels * 4,
                                  space: colorSpace,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else {
        throw IconBuildError.renderFailed(pixels)
    }
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: false)
    NSGraphicsContext.current?.imageInterpolation = .high
    image.draw(in: NSRect(x: 0, y: 0, width: pixels, height: pixels),
               from: .zero, operation: .copy, fraction: 1,
               respectFlipped: false, hints: [.interpolation: NSImageInterpolation.high.rawValue])
    NSGraphicsContext.restoreGraphicsState()

    guard let rendered = context.makeImage(),
          let encoder = CGImageDestinationCreateWithURL(destination as CFURL,
                                                       UTType.png.identifier as CFString, 1, nil) else {
        throw IconBuildError.encodingFailed(pixels)
    }
    CGImageDestinationAddImage(encoder, rendered, nil)
    guard CGImageDestinationFinalize(encoder) else {
        throw IconBuildError.encodingFailed(pixels)
    }
}

func buildIcon(source: URL, output: URL) throws {
    guard let image = NSImage(contentsOf: source), image.size.width > 0,
          image.size.width == image.size.height else {
        throw IconBuildError.invalidSource
    }
    let files = FileManager.default
    let temporaryDirectory = files.temporaryDirectory
        .appendingPathComponent("APFSearch-icon-\(UUID().uuidString)", isDirectory: true)
    let iconset = temporaryDirectory.appendingPathComponent("AppIcon.iconset", isDirectory: true)
    try files.createDirectory(at: iconset, withIntermediateDirectories: true)
    defer { try? files.removeItem(at: temporaryDirectory) }

    // Render each representation directly from the vector source, including Retina sizes.
    for points in [16, 32, 128, 256, 512] {
        for scale in [1, 2] {
            let suffix = scale == 2 ? "@2x" : ""
            let name = "icon_\(points)x\(points)\(suffix).png"
            try renderIcon(image, pixels: points * scale,
                           destination: iconset.appendingPathComponent(name))
        }
    }
    let converted = temporaryDirectory.appendingPathComponent("AppIcon.icns")
    let converter = Process()
    converter.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
    converter.arguments = ["--convert", "icns", "--output", converted.path, iconset.path]
    try converter.run()
    converter.waitUntilExit()
    guard converter.terminationStatus == 0 else {
        throw IconBuildError.conversionFailed(converter.terminationStatus)
    }
    try files.createDirectory(at: output.deletingLastPathComponent(), withIntermediateDirectories: true)
    try Data(contentsOf: converted).write(to: output, options: .atomic)
}

do {
    guard CommandLine.arguments.count == 3 else { throw IconBuildError.invalidArguments }
    try buildIcon(source: URL(fileURLWithPath: CommandLine.arguments[1]),
                  output: URL(fileURLWithPath: CommandLine.arguments[2]))
} catch {
    FileHandle.standardError.write(Data("Icon build failed: \(error)\n".utf8))
    exit(EXIT_FAILURE)
}
