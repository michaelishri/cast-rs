import AppKit
import Foundation

guard CommandLine.arguments.count == 2 else {
  fputs("usage: generate-icon.swift <output.png>\n", stderr)
  exit(2)
}

let size = NSSize(width: 1024, height: 1024)
let image = NSImage(size: size)
image.lockFocus()

let background = NSBezierPath(roundedRect: NSRect(x: 52, y: 52, width: 920, height: 920), xRadius: 205, yRadius: 205)
NSColor(calibratedRed: 0.08, green: 0.42, blue: 0.88, alpha: 1).setFill()
background.fill()

NSColor.white.setStroke()
let screen = NSBezierPath()
screen.lineWidth = 58
screen.lineCapStyle = .round
screen.lineJoinStyle = .round
screen.move(to: NSPoint(x: 245, y: 745))
screen.line(to: NSPoint(x: 795, y: 745))
screen.line(to: NSPoint(x: 795, y: 315))
screen.line(to: NSPoint(x: 690, y: 315))
screen.stroke()

func drawArc(radius: CGFloat) {
  let arc = NSBezierPath()
  arc.lineWidth = 58
  arc.lineCapStyle = .round
  arc.appendArc(
    withCenter: NSPoint(x: 245, y: 315),
    radius: radius,
    startAngle: 0,
    endAngle: 90
  )
  arc.stroke()
}

let dot = NSBezierPath(ovalIn: NSRect(x: 216, y: 286, width: 58, height: 58))
NSColor.white.setFill()
dot.fill()
drawArc(radius: 160)
drawArc(radius: 310)

image.unlockFocus()
guard
  let tiff = image.tiffRepresentation,
  let bitmap = NSBitmapImageRep(data: tiff),
  let png = bitmap.representation(using: .png, properties: [:])
else {
  fputs("could not render app icon\n", stderr)
  exit(1)
}
try png.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
