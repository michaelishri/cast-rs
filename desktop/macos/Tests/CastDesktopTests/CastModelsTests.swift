import Foundation
import ServiceManagement
import SwiftUI
import Testing

@testable import CastDesktop

@Test func decodesStructuredReceiverDiscovery() throws {
  let data = Data(
    #"[{"name":"AI PONT","model":"Google TV","capability":"video","address":"192.0.2.5","port":8009}]"#
      .utf8)
  let devices = try JSONDecoder().decode([CastDevice].self, from: data)
  #expect(
    devices == [
      CastDevice(
        name: "AI PONT",
        model: "Google TV",
        capability: .video,
        address: "192.0.2.5",
        port: 8009
      )
    ])
}

@Test func audioOnlyReceiverCannotCastDesktop() {
  #expect(!DeviceCapability.audioOnly.supportsDesktop)
  #expect(DeviceCapability.video.supportsDesktop)
  #expect(DeviceCapability.unknown.supportsDesktop)
}

@Test func buildsMirrorCommandForAChosenDisplay() {
  let device = CastDevice(
    name: "TV", model: "Model", capability: .video, address: "192.0.2.5", port: 9000)
  let display = CastDisplay(id: 42, name: "Studio Display", width: 5120, height: 2880, isMain: true)
  var configuration = CastConfiguration()
  configuration.includeAudio = true
  let arguments = CastCommandBuilder.desktopArguments(
    device: device,
    mode: .mirror(display),
    configuration: configuration,
    controllerPID: 1234
  )

  #expect(arguments.contains("--audio"))
  #expect(arguments.suffix(2) == ["--display", "42"])
  #expect(arguments[arguments.firstIndex(of: "--controller-pid")! + 1] == "1234")
  #expect(arguments[arguments.firstIndex(of: "--cast-port")! + 1] == "9000")
}

@Test func buildsExtendedCommandWithoutDisplayOrAudio() {
  let device = CastDevice(
    name: "TV", model: "", capability: .video, address: "192.0.2.6", port: 8009)
  let arguments = CastCommandBuilder.desktopArguments(
    device: device,
    mode: .extend,
    configuration: CastConfiguration(),
    controllerPID: 99
  )

  #expect(arguments.contains("--extend"))
  #expect(!arguments.contains("--display"))
  #expect(!arguments.contains("--audio"))
}

@Test func discoveryCommandUsesStructuredOutput() {
  #expect(
    CastCommandBuilder.discoveryArguments(timeout: 4) == ["devices", "--timeout", "4", "--json"])
}

@Test func executableOverrideMustPointToAnExecutable() throws {
  let executable = try CastExecutable.resolve(environment: ["CAST_CLI_PATH": "/bin/echo"])
  #expect(executable.path == "/bin/echo")
  #expect(throws: CastProcessError.self) {
    try CastExecutable.resolve(environment: ["CAST_CLI_PATH": "/definitely/missing/cast"])
  }
}

@MainActor
@Test func discoveryAcceptsAnEmptyReceiverList() throws {
  let result = CastProcessController.decodeDevices(
    ProcessOutput(stdout: Data("[]".utf8), stderr: Data(), status: 0))
  #expect(try result.get().isEmpty)
}

@MainActor
@Test func discoveryRejectsMalformedOutput() {
  let result = CastProcessController.decodeDevices(
    ProcessOutput(stdout: Data("not json".utf8), stderr: Data(), status: 0))
  #expect(throws: CastProcessError.self) { try result.get() }
}

@MainActor
@Test func discoveryReportsReceiverCommandFailure() {
  let result = CastProcessController.decodeDevices(
    ProcessOutput(stdout: Data(), stderr: Data("receiver unavailable".utf8), status: 7))
  do {
    _ = try result.get()
    Issue.record("Expected receiver command failure")
  } catch let error as CastProcessError {
    #expect(error.errorDescription == "receiver unavailable")
  } catch {
    Issue.record("Unexpected error: \(error)")
  }
}

@MainActor
@Test func sessionProcessTracksNormalTermination() async throws {
  let controller = CastProcessController()
  let result: Result<Void, Error> = await withCheckedContinuation { continuation in
    do {
      try controller.startSession(executable: URL(fileURLWithPath: "/usr/bin/true"), arguments: [])
      {
        continuation.resume(returning: $0)
      }
    } catch {
      continuation.resume(returning: .failure(error))
    }
  }

  try result.get()
  #expect(controller.sessionProcess == nil)
}

@MainActor
@Test func sessionProcessInterruptsAndClearsItsState() async throws {
  let controller = CastProcessController()
  let result: Result<Void, Error> = await withCheckedContinuation { continuation in
    do {
      try controller.startSession(
        executable: URL(fileURLWithPath: "/bin/sleep"), arguments: ["30"]
      ) {
        continuation.resume(returning: $0)
      }
      #expect(controller.sessionProcess?.isRunning == true)
      controller.stopSession()
    } catch {
      continuation.resume(returning: .failure(error))
    }
  }

  #expect(throws: CastProcessError.self) { try result.get() }
  #expect(controller.sessionProcess == nil)
}

@MainActor
@Test func repeatedDiscoverySupersedesTheEarlierRefresh() async throws {
  let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
  let executable = directory.appendingPathComponent("fake-cast")
  try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
  defer { try? FileManager.default.removeItem(at: directory) }
  try Data("#!/bin/sh\nsleep 0.2\nprintf '[]'\n".utf8).write(to: executable)
  try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: executable.path)

  let controller = CastProcessController()
  var supersededCallbackRan = false
  try controller.discover(executable: executable, timeout: 1) { _ in
    supersededCallbackRan = true
  }
  let result: Result<[CastDevice], Error> = await withCheckedContinuation { continuation in
    do {
      try controller.discover(executable: executable, timeout: 1) {
        continuation.resume(returning: $0)
      }
    } catch {
      continuation.resume(returning: .failure(error))
    }
  }

  #expect(try result.get().isEmpty)
  #expect(!supersededCallbackRan)
  #expect(controller.discoveryProcess == nil)
}

@MainActor
@Test func preferenceDefaultsMatchTheProductDefaults() {
  let suite = "CastDesktopTests.defaults.\(UUID().uuidString)"
  let defaults = UserDefaults(suiteName: suite)!
  defer { defaults.removePersistentDomain(forName: suite) }
  let preferences = CastPreferences(defaults: defaults)

  #expect(preferences.configuration == CastConfiguration())
}

@MainActor
@Test func preferencesPersistAndNormalizeInvalidValues() {
  let suite = "CastDesktopTests.normalize.\(UUID().uuidString)"
  let defaults = UserDefaults(suiteName: suite)!
  defer { defaults.removePersistentDomain(forName: suite) }
  defaults.set("invalid", forKey: CastPreferences.Key.transport)
  defaults.set(9_999, forKey: CastPreferences.Key.width)
  defaults.set(719, forKey: CastPreferences.Key.height)
  defaults.set(0, forKey: CastPreferences.Key.framesPerSecond)
  defaults.set(-1, forKey: CastPreferences.Key.bitrate)
  defaults.set(99, forKey: CastPreferences.Key.discoveryTimeoutSeconds)

  let preferences = CastPreferences(defaults: defaults)
  #expect(preferences.transport == .mirror)
  #expect(preferences.width == 3840)
  #expect(preferences.height == 718)
  #expect(preferences.framesPerSecond == 1)
  #expect(preferences.bitrate == 100_000)
  #expect(preferences.discoveryTimeoutSeconds == 30)

  preferences.includeAudio = true
  #expect(CastPreferences(defaults: defaults).includeAudio)
}

@MainActor
@Test func preferencesCanBeChangedAfterInitialization() {
  let suite = "CastDesktopTests.changed.\(UUID().uuidString)"
  let defaults = UserDefaults(suiteName: suite)!
  defer { defaults.removePersistentDomain(forName: suite) }
  let preferences = CastPreferences(defaults: defaults)

  preferences.width = 1920
  preferences.height = 1080
  preferences.framesPerSecond = 60
  preferences.bitrate = 8_000_000
  preferences.targetDelayMilliseconds = 300
  preferences.discoveryTimeoutSeconds = 5

  #expect(preferences.width == 1920)
  #expect(preferences.height == 1080)
  #expect(preferences.framesPerSecond == 60)
  #expect(preferences.bitrate == 8_000_000)
  #expect(preferences.targetDelayMilliseconds == 300)
  #expect(preferences.discoveryTimeoutSeconds == 5)
  #expect(defaults.integer(forKey: CastPreferences.Key.width) == 1920)
  #expect(defaults.integer(forKey: CastPreferences.Key.height) == 1080)
}

@MainActor
@Test func changedPreferencesNormalizeAndPersistInvalidValues() {
  let suite = "CastDesktopTests.changed-normalize.\(UUID().uuidString)"
  let defaults = UserDefaults(suiteName: suite)!
  defer { defaults.removePersistentDomain(forName: suite) }
  let preferences = CastPreferences(defaults: defaults)

  preferences.width = 9_999
  preferences.height = 719
  preferences.framesPerSecond = 0
  preferences.bitrate = -1
  preferences.targetDelayMilliseconds = 8_000
  preferences.discoveryTimeoutSeconds = 99

  #expect(preferences.width == 3840)
  #expect(preferences.height == 718)
  #expect(preferences.framesPerSecond == 1)
  #expect(preferences.bitrate == 100_000)
  #expect(preferences.targetDelayMilliseconds == 5_000)
  #expect(preferences.discoveryTimeoutSeconds == 30)
  #expect(defaults.integer(forKey: CastPreferences.Key.width) == 3840)
  #expect(defaults.integer(forKey: CastPreferences.Key.height) == 718)
  #expect(defaults.integer(forKey: CastPreferences.Key.framesPerSecond) == 1)
  #expect(defaults.integer(forKey: CastPreferences.Key.bitrate) == 100_000)
  #expect(defaults.integer(forKey: CastPreferences.Key.targetDelayMilliseconds) == 5_000)
  #expect(defaults.integer(forKey: CastPreferences.Key.discoveryTimeoutSeconds) == 30)
}

@Test func mapsEveryLaunchAtLoginStatus() {
  #expect(LoginItemState.map(.enabled) == .enabled)
  #expect(LoginItemState.map(.notRegistered) == .disabled)
  #expect(LoginItemState.map(.requiresApproval) == .requiresApproval)
  #expect(LoginItemState.map(.notFound) == .disabled)
  #expect(LoginItemState.enabled.isRegistered)
  #expect(LoginItemState.requiresApproval.isRegistered)
  #expect(!LoginItemState.disabled.isRegistered)
  #expect(!LoginItemState.unavailable("Unavailable").isRegistered)
}

@MainActor
@Test func firstLoginItemRegistrationHandlesNotFoundStatus() {
  let service = LoginItemServiceStub(status: .notFound)
  let controller = LoginItemController(service: service)

  #expect(controller.state == .disabled)
  controller.setEnabled(true)

  #expect(service.registerCallCount == 1)
  #expect(controller.state == .enabled)
}

@MainActor
@Test func loginItemPendingApprovalRemainsSelectedAndCanBeDisabled() {
  let service = LoginItemServiceStub(status: .requiresApproval)
  let controller = LoginItemController(service: service)

  #expect(controller.state.isRegistered)
  controller.setEnabled(false)

  #expect(service.unregisterCallCount == 1)
  #expect(controller.state == .disabled)
}

@Test func resolutionPresetsMatchConfiguration() {
  var configuration = CastConfiguration()
  #expect(CastResolution.presets[1].matches(configuration))
  #expect(configuration.resolutionLabel == "1280 × 720")

  configuration.width = 1920
  configuration.height = 1080
  #expect(CastResolution.presets[2].matches(configuration))
  #expect(!CastResolution.presets[1].matches(configuration))
}

@Test func activeCastReportsRestartAndStopStates() {
  #expect(ActiveCastState.casting.label == nil)
  #expect(!ActiveCastState.casting.isBusy)
  #expect(ActiveCastState.restarting.label == "Restarting…")
  #expect(ActiveCastState.restarting.isBusy)
  #expect(ActiveCastState.stopping.label == "Stopping…")
}

@MainActor
@Test func castIconUsesNativeMenuDimensions() throws {
  #expect(CastIcon.menuBarPointSize == 16)
  #expect(CastIcon.menuItemPointSize == 14)
  #expect(CastIcon.menuBarImage.size == NSSize(width: 16, height: 16))
  #expect(CastIcon.menuItemImage.size == NSSize(width: 14, height: 14))

  let menuBarImage = try #require(
    ImageRenderer(content: CastGlyph(image: CastIcon.menuBarImage)).nsImage)
  let menuItemImage = try #require(
    ImageRenderer(content: CastGlyph(image: CastIcon.menuItemImage)).nsImage)
  #expect(menuBarImage.size == NSSize(width: 16, height: 16))
  #expect(menuItemImage.size == NSSize(width: 14, height: 14))
}

private final class LoginItemServiceStub: LoginItemServicing {
  var status: SMAppService.Status
  private(set) var registerCallCount = 0
  private(set) var unregisterCallCount = 0

  init(status: SMAppService.Status) {
    self.status = status
  }

  func register() throws {
    registerCallCount += 1
    status = .enabled
  }

  func unregister() throws {
    unregisterCallCount += 1
    status = .notRegistered
  }
}
