import AppKit
// Draws the menu bar icon frames at 36×36 px (18 pt at 2×). Template images: black on clear.
let out = CommandLine.arguments[1]
let size: CGFloat = 36
let stroke: CGFloat = 3

func render(_ name: String, _ draw: (CGContext) -> Void) {
  let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8,
                             samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
  NSGraphicsContext.saveGraphicsState()
  NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
  let ctx = NSGraphicsContext.current!.cgContext
  // Flip so y grows downward, like the design grid.
  ctx.translateBy(x: 0, y: size); ctx.scaleBy(x: 1, y: -1)
  ctx.setStrokeColor(NSColor.black.cgColor); ctx.setFillColor(NSColor.black.cgColor)
  ctx.setLineWidth(stroke); ctx.setLineCap(.round); ctx.setLineJoin(.round)
  draw(ctx)
  NSGraphicsContext.restoreGraphicsState()
  try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: "\(out)/\(name).png"))
}

/// Viewfinder corners. `inset` moves them toward the centre (the "closing in" of the overlay).
func corners(_ ctx: CGContext, inset: CGFloat = 0, arm: CGFloat = 8.5) {
  let lo = 4 + inset, hi = size - 4 - inset
  for (x, y, dx, dy) in [(lo, lo, 1.0, 1.0), (hi, lo, -1.0, 1.0), (lo, hi, 1.0, -1.0), (hi, hi, -1.0, -1.0)] as [(CGFloat, CGFloat, CGFloat, CGFloat)] {
    ctx.move(to: CGPoint(x: x, y: y + dy * arm)); ctx.addLine(to: CGPoint(x: x, y: y)); ctx.addLine(to: CGPoint(x: x + dx * arm, y: y))
  }
  ctx.strokePath()
}

/// Upload arrow, centred, shifted by `dy`; clipped to the frame interior so it can rise out of view.
func arrow(_ ctx: CGContext, dy: CGFloat = 0, alpha: CGFloat = 1) {
  ctx.saveGState()
    ctx.setAlpha(alpha)
  let c = size / 2
  ctx.move(to: CGPoint(x: c, y: 23.5 + dy)); ctx.addLine(to: CGPoint(x: c, y: 12.5 + dy))
  ctx.move(to: CGPoint(x: c - 5, y: 17.5 + dy)); ctx.addLine(to: CGPoint(x: c, y: 12.5 + dy)); ctx.addLine(to: CGPoint(x: c + 5, y: 17.5 + dy))
  ctx.strokePath()
  ctx.restoreGState()
}

render("idle") { corners($0); arrow($0) }

// Needs setup: a dot takes the place of the top-right corner, so it reads at 18 pt.
render("attention") { ctx in
  corners(ctx); arrow(ctx)
  ctx.setBlendMode(.clear); ctx.fillEllipse(in: CGRect(x: 21, y: -1, width: 16, height: 16))
  ctx.setBlendMode(.normal); ctx.fillEllipse(in: CGRect(x: 24.5, y: 2.5, width: 9, height: 9))
}

// Uploading: the corners breathe in and out while the arrow lifts, echoing the overlay.
let frames = 8
for i in 0..<frames {
  let wave = (1 - cos(2 * .pi * CGFloat(i) / CGFloat(frames))) / 2
  render("upload-\(i)") { ctx in
    corners(ctx, inset: wave * 2.5, arm: 8.5 - wave * 1.5)
    arrow(ctx, dy: -wave * 2.5)
  }
}

render("done") { ctx in
  corners(ctx, inset: 1.5, arm: 7.5)
  ctx.move(to: CGPoint(x: 12, y: 18.5)); ctx.addLine(to: CGPoint(x: 16, y: 22.5)); ctx.addLine(to: CGPoint(x: 24, y: 13.5))
  ctx.strokePath()
}

render("error") { ctx in
  corners(ctx)
  let c = size / 2
  ctx.move(to: CGPoint(x: c, y: 11.5)); ctx.addLine(to: CGPoint(x: c, y: 19.5)); ctx.strokePath()
  ctx.fillEllipse(in: CGRect(x: c - 2, y: 22.5, width: 4, height: 4))
}
