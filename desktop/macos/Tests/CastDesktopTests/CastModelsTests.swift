import Foundation
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

@Test func mapsEveryLaunchAtLoginStatus() {
  #expect(LoginItemState.map(.enabled) == .enabled)
  #expect(LoginItemState.map(.notRegistered) == .disabled)
  #expect(LoginItemState.map(.requiresApproval) == .requiresApproval)
  #expect(
    LoginItemState.map(.notFound) == .unavailable("Cast.app must be installed in Applications"))
}
