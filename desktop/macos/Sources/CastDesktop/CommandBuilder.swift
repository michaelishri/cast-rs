import Foundation

enum CastCommandBuilder {
  static func discoveryArguments(timeout: Int) -> [String] {
    ["devices", "--timeout", String(timeout), "--json"]
  }

  static func desktopArguments(
    device: CastDevice,
    mode: CastMode,
    configuration: CastConfiguration,
    controllerPID: Int32
  ) -> [String] {
    var arguments = [
      "desktop",
      "--host", device.address,
      "--cast-port", String(device.port),
      "--transport", configuration.transport.rawValue,
      "--width", String(configuration.width),
      "--height", String(configuration.height),
      "--fps", String(configuration.framesPerSecond),
      "--bitrate", String(configuration.bitrate),
      "--target-delay-ms", String(configuration.targetDelayMilliseconds),
      "--controller-pid", String(controllerPID),
    ]
    if configuration.includeAudio {
      arguments.append("--audio")
    }
    switch mode {
    case .mirror(let display):
      arguments.append(contentsOf: ["--display", String(display.id)])
    case .extend:
      arguments.append("--extend")
    }
    return arguments
  }
}
