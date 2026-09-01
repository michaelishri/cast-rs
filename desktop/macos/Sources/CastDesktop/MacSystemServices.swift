import AppKit
import CoreGraphics
import Foundation
import ServiceManagement

@MainActor
final class ScreenRecordingController: ObservableObject {
  @Published private(set) var isGranted = CGPreflightScreenCaptureAccess()

  var statusLabel: String { isGranted ? "Allowed" : "Not allowed" }

  @discardableResult
  func requestAccess() -> Bool {
    let granted = CGRequestScreenCaptureAccess()
    refresh()
    return granted
  }

  func refresh() {
    isGranted = CGPreflightScreenCaptureAccess()
  }

  func openSettings() {
    guard
      let url = URL(
        string: "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
    else { return }
    NSWorkspace.shared.open(url)
  }
}

enum LoginItemState: Equatable {
  case enabled
  case disabled
  case requiresApproval
  case unavailable(String)

  var isEnabled: Bool { self == .enabled }

  var label: String {
    switch self {
    case .enabled: "Enabled"
    case .disabled: "Disabled"
    case .requiresApproval: "Requires approval in System Settings"
    case .unavailable(let message): message
    }
  }

  static func map(_ status: SMAppService.Status) -> LoginItemState {
    switch status {
    case .enabled: .enabled
    case .notRegistered: .disabled
    case .requiresApproval: .requiresApproval
    case .notFound: .unavailable("Cast.app must be installed in Applications")
    @unknown default: .unavailable("Unknown login item status")
    }
  }
}

@MainActor
final class LoginItemController: ObservableObject {
  @Published private(set) var state: LoginItemState
  @Published private(set) var isChanging = false

  private let service: SMAppService

  init(service: SMAppService = .mainApp) {
    self.service = service
    state = LoginItemState.map(service.status)
  }

  func refresh() {
    state = LoginItemState.map(service.status)
  }

  func setEnabled(_ enabled: Bool) {
    isChanging = true
    defer { isChanging = false }
    do {
      if enabled {
        if service.status == .notRegistered { try service.register() }
      } else if service.status == .enabled || service.status == .requiresApproval {
        try service.unregister()
      }
      state = LoginItemState.map(service.status)
    } catch {
      state = .unavailable("Registration error: \(error.localizedDescription)")
    }
  }

  func openSettings() {
    SMAppService.openSystemSettingsLoginItems()
  }
}

@MainActor
final class CastDiagnostics: ObservableObject {
  @Published private(set) var cliVersion = "Checking…"
  @Published private(set) var cliPath = "Checking…"

  private var process: Process?

  func refresh() {
    guard process == nil else { return }
    do {
      let executable = try CastExecutable.resolve()
      cliPath = executable.path
      let process = Process()
      let output = Pipe()
      let errors = Pipe()
      process.executableURL = executable
      process.arguments = ["--version"]
      process.standardOutput = output
      process.standardError = errors
      self.process = process
      process.terminationHandler = { [weak self] finished in
        let standardOutput = output.fileHandleForReading.readDataToEndOfFile()
        let standardError = errors.fileHandleForReading.readDataToEndOfFile()
        Task { @MainActor [weak self] in
          guard let self, self.process === finished else { return }
          self.process = nil
          let data = finished.terminationStatus == 0 ? standardOutput : standardError
          let text = String(decoding: data, as: UTF8.self).trimmingCharacters(
            in: .whitespacesAndNewlines)
          self.cliVersion = text.isEmpty ? "Unavailable" : text
        }
      }
      try process.run()
    } catch {
      process = nil
      cliPath = "Not found"
      cliVersion = error.localizedDescription
    }
  }
}
