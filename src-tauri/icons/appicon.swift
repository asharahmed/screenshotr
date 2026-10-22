import AppKit
// Screenshotr's app icon on Apple's 1024 grid: an 824 pt rounded body, viewfinder corners and the
// upload arrow from the menu bar icon, with glass-like light. Usage: appicon <out.png>
let out = CommandLine.arguments[1]
let S: CGFloat = 1024
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 1024, pixelsHigh: 1024, bitsPerSample: 8, samplesPerPixel: 4,
                           hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext
let space = CGColorSpace(name: CGColorSpace.displayP3)!
func color(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
  CGColor(colorSpace: space, components: [CGFloat((hex >> 16) & 255) / 255, CGFloat((hex >> 8) & 255) / 255, CGFloat(hex & 255) / 255, a])!
}
func gradient(_ stops: [(UInt32, CGFloat, CGFloat)]) -> CGGradient {
  CGGradient(colorsSpace: space, colors: stops.map { color($0.0, $0.1) } as CFArray, locations: stops.map { $0.2 })!
}

// Continuous-corner body (a superellipse-like squircle).
let body = CGRect(x: 100, y: 100, width: 824, height: 824)
func squircle(_ r: CGRect, _ n: CGFloat = 5.0) -> CGPath {
  let path = CGMutablePath()
  let cx = r.midX, cy = r.midY, a = r.width / 2, b = r.height / 2
  for i in 0...720 {
    let t = CGFloat(i) / 720 * 2 * .pi
    let c = cos(t), s = sin(t)
    let x = cx + a * (c >= 0 ? 1 : -1) * pow(abs(c), 2 / n)
    let y = cy + b * (s >= 0 ? 1 : -1) * pow(abs(s), 2 / n)
    i == 0 ? path.move(to: CGPoint(x: x, y: y)) : path.addLine(to: CGPoint(x: x, y: y))
  }
  path.closeSubpath()
  return path
}
let shape = squircle(body)

// Drop shadow under the body, as on the Dock.
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -12), blur: 40, color: color(0x000000, 0.35))
ctx.addPath(shape); ctx.setFillColor(color(0x1b1d24)); ctx.fillPath()
ctx.restoreGState()

// Body: deep graphite, lit from above.
ctx.saveGState()
ctx.addPath(shape); ctx.clip()
ctx.drawLinearGradient(gradient([(0x3a3f4c, 1, 0), (0x1d2029, 1, 0.55), (0x111318, 1, 1)]),
                       start: CGPoint(x: 512, y: 924), end: CGPoint(x: 512, y: 100), options: [])
// A soft blue bloom behind the arrow.
ctx.drawRadialGradient(gradient([(0x2f7dff, 0.42, 0), (0x2f7dff, 0, 1)]),
                       startCenter: CGPoint(x: 512, y: 470), startRadius: 0, endCenter: CGPoint(x: 512, y: 470), endRadius: 360, options: [])
// Glass sheen across the top, fading to nothing so it has no visible edge.
ctx.drawRadialGradient(gradient([(0xffffff, 0.13, 0), (0xffffff, 0.05, 0.55), (0xffffff, 0, 1)]),
                       startCenter: CGPoint(x: 512, y: 1060), startRadius: 0, endCenter: CGPoint(x: 512, y: 1060), endRadius: 560, options: [])
ctx.restoreGState()

// Rim light: a thin bright edge on top, fading down.
ctx.saveGState()
ctx.addPath(shape); ctx.setLineWidth(5); ctx.replacePathWithStrokedPath(); ctx.clip()
ctx.drawLinearGradient(gradient([(0xffffff, 0.45, 0), (0xffffff, 0.06, 0.5), (0xffffff, 0.12, 1)]),
                       start: CGPoint(x: 512, y: 924), end: CGPoint(x: 512, y: 100), options: [])
ctx.restoreGState()

// Viewfinder corners: thick, round-capped, porcelain white with a slight shadow for depth.
let inset: CGFloat = 244, arm: CGFloat = 124, weight: CGFloat = 50
let lo = inset, hi = S - inset
let corners = CGMutablePath()
for (x, y, dx, dy) in [(lo, lo, 1.0, 1.0), (hi, lo, -1.0, 1.0), (lo, hi, 1.0, -1.0), (hi, hi, -1.0, -1.0)] as [(CGFloat, CGFloat, CGFloat, CGFloat)] {
  corners.move(to: CGPoint(x: x, y: y + dy * arm)); corners.addLine(to: CGPoint(x: x, y: y)); corners.addLine(to: CGPoint(x: x + dx * arm, y: y))
}
let cornerStroke = corners.copy(strokingWithWidth: weight, lineCap: .round, lineJoin: .round, miterLimit: 10)
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -8), blur: 18, color: color(0x000000, 0.45))
ctx.addPath(cornerStroke); ctx.setFillColor(color(0xffffff)); ctx.fillPath()
ctx.restoreGState()
ctx.saveGState()
ctx.addPath(cornerStroke); ctx.clip()
ctx.drawLinearGradient(gradient([(0xffffff, 1, 0), (0xd9e0ec, 1, 1)]), start: CGPoint(x: 512, y: 790), end: CGPoint(x: 512, y: 230), options: [])
ctx.restoreGState()

// The upload arrow: luminous blue, glowing slightly.
let arrow = CGMutablePath()
arrow.move(to: CGPoint(x: 512, y: 384)); arrow.addLine(to: CGPoint(x: 512, y: 632))
arrow.move(to: CGPoint(x: 410, y: 530)); arrow.addLine(to: CGPoint(x: 512, y: 632)); arrow.addLine(to: CGPoint(x: 614, y: 530))
let arrowStroke = arrow.copy(strokingWithWidth: 56, lineCap: .round, lineJoin: .round, miterLimit: 10)
ctx.saveGState()
ctx.setShadow(offset: .zero, blur: 46, color: color(0x3d8bff, 0.85))
ctx.addPath(arrowStroke); ctx.setFillColor(color(0x4a94ff)); ctx.fillPath()
ctx.restoreGState()
ctx.saveGState()
ctx.addPath(arrowStroke); ctx.clip()
ctx.drawLinearGradient(gradient([(0x9ad6ff, 1, 0), (0x2a7bff, 1, 1)]), start: CGPoint(x: 512, y: 700), end: CGPoint(x: 512, y: 330),
                       options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
ctx.restoreGState()

NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
