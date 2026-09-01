import AppKit
import SwiftUI

enum CastIcon {
  static let menuBarPointSize: CGFloat = 16
  static let menuItemPointSize: CGFloat = 14

  private static let sourceImage: NSImage = {
    if let url = Bundle.main.url(forResource: "cast-symbolic", withExtension: "svg"),
      let image = NSImage(contentsOf: url)
    {
      return image
    }

    return NSImage(
      systemSymbolName: "airplayvideo",
      accessibilityDescription: "Cast"
    ) ?? NSImage()
  }()

  static let menuBarImage = makeTemplateImage(pointSize: menuBarPointSize)
  static let menuItemImage = makeTemplateImage(pointSize: menuItemPointSize)

  private static func makeTemplateImage(pointSize: CGFloat) -> NSImage {
    // MenuBarExtra reads the NSImage's logical size directly, so constrain the
    // AppKit image itself rather than relying on SwiftUI frame modifiers.
    let logicalSize = NSSize(width: pointSize, height: pointSize)
    let image = NSImage(size: logicalSize, flipped: false) { destination in
      let sourceSize = sourceImage.size
      guard sourceSize.width > 0, sourceSize.height > 0 else { return false }
      let scale = min(
        destination.width / sourceSize.width,
        destination.height / sourceSize.height
      )
      let fittedSize = NSSize(
        width: sourceSize.width * scale,
        height: sourceSize.height * scale
      )
      let fittedRect = NSRect(
        x: destination.midX - (fittedSize.width / 2),
        y: destination.midY - (fittedSize.height / 2),
        width: fittedSize.width,
        height: fittedSize.height
      )
      sourceImage.draw(in: fittedRect)
      return true
    }
    image.isTemplate = true
    image.accessibilityDescription = "Cast"
    return image
  }
}

struct CastGlyph: View {
  let image: NSImage

  var body: some View {
    Image(nsImage: image)
      .renderingMode(.template)
  }
}

struct CastMenuBarIcon: View {
  let isActive: Bool

  var body: some View {
    CastGlyph(image: CastIcon.menuBarImage)
      .overlay(alignment: .bottomTrailing) {
        if isActive {
          Circle()
            .fill(.primary)
            .frame(width: 4, height: 4)
        }
      }
      .accessibilityLabel("Cast")
  }
}
