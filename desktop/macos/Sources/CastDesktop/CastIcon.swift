import AppKit
import SwiftUI

enum CastIcon {
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

struct CastMenuBarIcon: View {
  let isActive: Bool

  var body: some View {
    Image(nsImage: CastIcon.image)
      .renderingMode(.template)
      .overlay(alignment: .bottomTrailing) {
        if isActive {
          Circle()
            .fill(.primary)
            .frame(width: 5, height: 5)
        }
      }
      .accessibilityLabel("Cast")
  }
}
