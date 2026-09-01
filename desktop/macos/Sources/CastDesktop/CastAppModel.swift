import AppKit
import Foundation

@MainActor
final class CastAppModel: ObservableObject {
  @Published private(set) var devices: [CastDevice] = []
  @Published private(set) var displays: [CastDisplay] = []
  @Published private(set) var activeCast: ActiveCast?
  @Published private(set) var isDiscovering = false
  @Published var errorMessage: String?

  private let processes = CastProcessController()
  private let preferences: CastPreferences
  private let permissions: ScreenRecordingController
  private var discoveryGeneration = 0
  private var stopRequested = false

  init(preferences: CastPreferences, permissions: ScreenRecordingController) {
    self.preferences = preferences
    self.permissions = permissions
    refreshDisplays()
  }

  var hasActiveCast: Bool { activeCast != nil }

  func refreshDisplays() {
    displays = CastDisplay.current()
  }

  func refreshDevices() {
    discoveryGeneration += 1
    let generation = discoveryGeneration
    isDiscovering = true
    errorMessage = nil
    do {
      let executable = try CastExecutable.resolve()
      try processes.discover(
        executable: executable,
        timeout: preferences.discoveryTimeoutSeconds
      ) { [weak self] result in
        guard let self, generation == self.discoveryGeneration else { return }
        self.isDiscovering = false
        switch result {
        case .success(let devices): self.devices = devices
        case .failure(let error):
          self.devices = []
          self.errorMessage = error.localizedDescription
        }
      }
    } catch {
      isDiscovering = false
      devices = []
      errorMessage = error.localizedDescription
    }
  }

  func startCasting(to device: CastDevice, mode: CastMode) {
    guard activeCast == nil, device.capability.supportsDesktop else { return }
    errorMessage = nil
    guard permissions.isGranted || permissions.requestAccess() else {
      errorMessage =
        "Screen Recording access is required. Enable Cast in System Settings, then restart Cast."
      return
    }
    stopRequested = false
    do {
      let executable = try CastExecutable.resolve()
      let arguments = CastCommandBuilder.desktopArguments(
        device: device,
        mode: mode,
        configuration: preferences.configuration,
        controllerPID: ProcessInfo.processInfo.processIdentifier
      )
      try processes.startSession(executable: executable, arguments: arguments) {
        [weak self] result in
        guard let self else { return }
        let requested = self.stopRequested
        self.activeCast = nil
        self.stopRequested = false
        if case .failure(let error) = result, !requested {
          self.errorMessage = error.localizedDescription
        }
      }
      activeCast = ActiveCast(device: device, mode: mode, isStopping: false)
    } catch {
      activeCast = nil
      errorMessage = error.localizedDescription
    }
  }

  func stopCasting() {
    guard var active = activeCast else { return }
    stopRequested = true
    active.isStopping = true
    activeCast = active
    processes.stopSession()
  }

  func shutdown() {
    discoveryGeneration += 1
    processes.cancelDiscovery()
    stopCasting()
  }
}
