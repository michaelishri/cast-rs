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
  @StateObject private var preferences: CastPreferences
  @StateObject private var permissions: ScreenRecordingController
  @StateObject private var loginItem: LoginItemController
  @StateObject private var diagnostics: CastDiagnostics
  @StateObject private var model: CastAppModel

  init() {
    let preferences = CastPreferences()
    let permissions = ScreenRecordingController()
    _preferences = StateObject(wrappedValue: preferences)
    _permissions = StateObject(wrappedValue: permissions)
    _loginItem = StateObject(wrappedValue: LoginItemController())
    _diagnostics = StateObject(wrappedValue: CastDiagnostics())
    _model = StateObject(
      wrappedValue: CastAppModel(preferences: preferences, permissions: permissions))
  }

  var body: some Scene {
    MenuBarExtra {
      CastMenuView()
        .environmentObject(model)
        .onAppear { delegate.model = model }
    } label: {
      CastMenuBarIcon(isActive: model.hasActiveCast)
    }
    .menuBarExtraStyle(.menu)

    Settings {
      CastSettingsView()
        .environmentObject(preferences)
        .environmentObject(permissions)
        .environmentObject(loginItem)
        .environmentObject(diagnostics)
    }
  }
}
