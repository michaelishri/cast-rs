import AppKit
import Foundation

@MainActor
final class CastAppModel: ObservableObject {
  private struct CastRequest {
    let device: CastDevice
    let mode: CastMode
    let configuration: CastConfiguration
  }

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
  private var pendingRestart: CastRequest?

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
    launchSession(
      CastRequest(device: device, mode: mode, configuration: preferences.configuration))
  }

  func setActiveResolution(_ resolution: CastResolution) {
    guard let active = activeCast else { return }
    preferences.width = resolution.width
    preferences.height = resolution.height
    var configuration = active.configuration
    configuration.width = resolution.width
    configuration.height = resolution.height
    restartActiveCast(with: configuration)
  }

  func setActiveAudio(_ enabled: Bool) {
    guard let active = activeCast else { return }
    preferences.includeAudio = enabled
    var configuration = active.configuration
    configuration.includeAudio = enabled
    restartActiveCast(with: configuration)
  }

  func stopCasting() {
    guard var active = activeCast else { return }
    pendingRestart = nil
    stopRequested = true
    active.state = .stopping
    activeCast = active
    processes.stopSession()
  }

  func shutdown() {
    discoveryGeneration += 1
    processes.cancelDiscovery()
    stopCasting()
  }

  private func restartActiveCast(with configuration: CastConfiguration) {
    guard var active = activeCast else { return }
    pendingRestart = CastRequest(
      device: active.device,
      mode: active.mode,
      configuration: configuration
    )
    active.configuration = configuration
    if active.state == .casting {
      active.state = .restarting
      stopRequested = true
      activeCast = active
      processes.stopSession()
    } else if active.state == .restarting {
      activeCast = active
    }
  }

  private func launchSession(_ request: CastRequest) {
    stopRequested = false
    do {
      let executable = try CastExecutable.resolve()
      let arguments = CastCommandBuilder.desktopArguments(
        device: request.device,
        mode: request.mode,
        configuration: request.configuration,
        controllerPID: ProcessInfo.processInfo.processIdentifier
      )
      try processes.startSession(executable: executable, arguments: arguments) {
        [weak self] result in
        guard let self else { return }
        let requested = self.stopRequested
        let restart = self.pendingRestart
        self.pendingRestart = nil
        self.activeCast = nil
        self.stopRequested = false
        if let restart {
          self.launchSession(restart)
          return
        }
        if case .failure(let error) = result, !requested {
          self.errorMessage = error.localizedDescription
        }
      }
      activeCast = ActiveCast(
        device: request.device,
        mode: request.mode,
        configuration: request.configuration,
        state: .casting
      )
    } catch {
      pendingRestart = nil
      activeCast = nil
      errorMessage = error.localizedDescription
    }
  }
}
