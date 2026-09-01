import AppKit
import SwiftUI

enum CastIcon {
  static let menuBarPointSize: CGFloat = 16
  static let menuItemPointSize: CGFloat = 14

  static let image: NSImage = {
    if let url = Bundle.main.url(forResource: "cast-symbolic", withExtension: "svg"),
      let image = NSImage(contentsOf: url)
    {
      image.isTemplate = true
      image.accessibilityDescription = "Cast"
      return image
    }

    return NSImage(
      systemSymbolName: "airplayvideo",
      accessibilityDescription: "Cast"
    ) ?? NSImage()
  }()
}

struct CastGlyph: View {
  let pointSize: CGFloat

  var body: some View {
    Image(nsImage: CastIcon.image)
      .resizable()
      .renderingMode(.template)
      .aspectRatio(contentMode: .fit)
      .frame(width: pointSize, height: pointSize)
  }
}

struct CastMenuBarIcon: View {
  let isActive: Bool

  var body: some View {
    CastGlyph(pointSize: CastIcon.menuBarPointSize)
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
