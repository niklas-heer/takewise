// Renders Takewise's app icon: a "T" standing on a waveform, in the
// window's colors, on a dark rounded square, at every size macOS asks for.
//   swift packaging/make-icon.swift build/AppIcon.iconset
//   iconutil -c icns build/AppIcon.iconset -o packaging/AppIcon.icns
import AppKit

let args = CommandLine.arguments
guard args.count == 2 else {
    FileHandle.standardError.write("usage: make-icon.swift <iconset dir>\n".data(using: .utf8)!)
    exit(64)
}
let output = URL(fileURLWithPath: args[1])
try? FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)

func color(_ hex: UInt32) -> NSColor {
    NSColor(
        srgbRed: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
        blue: CGFloat(hex & 0xff) / 255, alpha: 1)
}

/// Bar heights of the waveform, in points of the 1024 grid.
let bars: [CGFloat] = [70, 140, 230, 160, 270, 160, 230, 140, 70]

func render(_ pixels: Int) -> Data {
    let size = CGFloat(pixels)
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels, bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
        bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let unit = size / 1024
    // Apple's grid: an 824-point body with room for the shadow.
    let body = NSRect(x: 100 * unit, y: 100 * unit, width: 824 * unit, height: 824 * unit)
    let path = NSBezierPath(roundedRect: body, xRadius: 185 * unit, yRadius: 185 * unit)
    let shadow = NSShadow()
    shadow.shadowColor = NSColor(white: 0, alpha: 0.3)
    shadow.shadowOffset = NSSize(width: 0, height: -10 * unit)
    shadow.shadowBlurRadius = 24 * unit
    NSGraphicsContext.saveGraphicsState()
    shadow.set()
    color(0x18181b).setFill()
    path.fill()
    NSGraphicsContext.restoreGraphicsState()
    NSGradient(starting: color(0x26262b), ending: color(0x101012))!.draw(in: path, angle: -90)
    NSColor(white: 1, alpha: 0.08).setStroke()
    path.lineWidth = 2 * unit
    path.stroke()

    // The "T", in the window's text color, centered above the waveform.
    let descriptor = NSFont.systemFont(ofSize: 470 * unit, weight: .heavy).fontDescriptor
        .withDesign(.rounded)!
    let font = NSFont(descriptor: descriptor, size: 470 * unit)!
    let letter = NSAttributedString(string: "T", attributes: [
        .font: font,
        .foregroundColor: color(0xeeeeee),
    ])
    let baseline = 480 * unit
    letter.draw(at: NSPoint(x: (size - letter.size().width) / 2, y: baseline + font.descender))

    // The waveform, in the accent color, centered under the letter.
    let barWidth = 40 * unit
    let gap = 24 * unit
    let total = CGFloat(bars.count) * barWidth + CGFloat(bars.count - 1) * gap
    let center = 310 * unit
    color(0xf5c542).setFill()
    for (i, height) in bars.enumerated() {
        let h = height * unit
        let bar = NSRect(
            x: (size - total) / 2 + CGFloat(i) * (barWidth + gap), y: center - h / 2,
            width: barWidth, height: h)
        NSBezierPath(roundedRect: bar, xRadius: barWidth / 2, yRadius: barWidth / 2).fill()
    }
    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

for points in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let name = scale == 1 ? "icon_\(points)x\(points).png" : "icon_\(points)x\(points)@2x.png"
        try render(points * scale).write(to: output.appendingPathComponent(name))
    }
}
