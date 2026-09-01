import AppKit
import SwiftUI

@MainActor
final class CastAppDelegate: NSObject, NSApplicationDelegate {
  weak var model: CastAppModel?

  func applicationWillTerminate(_ notification: Notification) {
    model?.shutdown()
  }
}

@main
struct CastDesktopApp: App {
  @NSApplicationDelegateAdaptor(CastAppDelegate.self) private var delegate
  @StateObject private var model = CastAppModel()

  var body: some Scene {
    MenuBarExtra {
      CastPopoverView()
        .environmentObject(model)
        .onAppear { delegate.model = model }
    } label: {
      Image(systemName: model.hasActiveCast ? "airplayvideo.circle.fill" : "airplayvideo")
        .accessibilityLabel("Cast")
    }
    .menuBarExtraStyle(.window)

    Settings {
      VStack(spacing: 10) {
        Image(systemName: "airplayvideo").font(.largeTitle)
        Text("Cast settings are installed with the next integration component.")
      }
      .padding(30)
      .frame(width: 460, height: 220)
    }
  }
}
