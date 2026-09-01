import AppKit
import Foundation

guard CommandLine.arguments.count == 3 else {
  fputs("usage: generate-icon.swift <cast-symbolic.svg> <output.png>\n", stderr)
  exit(2)
}

guard let glyph = NSImage(contentsOfFile: CommandLine.arguments[1]) else {
  fputs("could not load Cast SVG\n", stderr)
  exit(1)
}

let size = NSSize(width: 1024, height: 1024)
let image = NSImage(size: size)
image.lockFocus()

let background = NSBezierPath(
  roundedRect: NSRect(x: 52, y: 52, width: 920, height: 920), xRadius: 205, yRadius: 205)
NSColor(calibratedRed: 0.08, green: 0.42, blue: 0.88, alpha: 1).setFill()
background.fill()

let glyphBounds = NSRect(x: 132, y: 132, width: 760, height: 760)
let glyphScale = min(
  glyphBounds.width / glyph.size.width,
  glyphBounds.height / glyph.size.height
)
let glyphSize = NSSize(
  width: glyph.size.width * glyphScale,
  height: glyph.size.height * glyphScale
)
let glyphRect = NSRect(
  x: glyphBounds.midX - (glyphSize.width / 2),
  y: glyphBounds.midY - (glyphSize.height / 2),
  width: glyphSize.width,
  height: glyphSize.height
)
let tintedGlyph = NSImage(size: size)
tintedGlyph.lockFocus()
glyph.draw(in: glyphRect)
NSColor.white.setFill()
NSRect(origin: .zero, size: size).fill(using: .sourceIn)
tintedGlyph.unlockFocus()
tintedGlyph.draw(in: NSRect(origin: .zero, size: size))

image.unlockFocus()
guard
  let tiff = image.tiffRepresentation,
  let bitmap = NSBitmapImageRep(data: tiff),
  let png = bitmap.representation(using: .png, properties: [:])
else {
  fputs("could not render app icon\n", stderr)
  exit(1)
}
try png.write(to: URL(fileURLWithPath: CommandLine.arguments[2]))
