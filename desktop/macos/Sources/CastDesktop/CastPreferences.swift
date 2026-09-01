import Foundation

@MainActor
final class CastPreferences: ObservableObject {
  enum Key {
    static let includeAudio = "includeAudio"
    static let transport = "transport"
    static let width = "videoWidth"
    static let height = "videoHeight"
    static let framesPerSecond = "framesPerSecond"
    static let bitrate = "bitrate"
    static let targetDelayMilliseconds = "targetDelayMilliseconds"
    static let discoveryTimeoutSeconds = "discoveryTimeoutSeconds"
  }

  static let defaultConfiguration = CastConfiguration()

  @Published var includeAudio: Bool {
    didSet { defaults.set(includeAudio, forKey: Key.includeAudio) }
  }
  @Published var transport: DesktopTransport {
    didSet { defaults.set(transport.rawValue, forKey: Key.transport) }
  }
  @Published var width: Int {
    didSet {
      width = Self.normalizedEven(width, range: 2...3840)
      defaults.set(width, forKey: Key.width)
    }
  }
  @Published var height: Int {
    didSet {
      height = Self.normalizedEven(height, range: 2...2160)
      defaults.set(height, forKey: Key.height)
    }
  }
  @Published var framesPerSecond: Int {
    didSet {
      framesPerSecond = min(max(framesPerSecond, 1), 60)
      defaults.set(framesPerSecond, forKey: Key.framesPerSecond)
    }
  }
  @Published var bitrate: Int {
    didSet {
      bitrate = min(max(bitrate, 100_000), 50_000_000)
      defaults.set(bitrate, forKey: Key.bitrate)
    }
  }
  @Published var targetDelayMilliseconds: Int {
    didSet {
      targetDelayMilliseconds = min(max(targetDelayMilliseconds, 1), 5_000)
      defaults.set(targetDelayMilliseconds, forKey: Key.targetDelayMilliseconds)
    }
  }
  @Published var discoveryTimeoutSeconds: Int {
    didSet {
      discoveryTimeoutSeconds = min(max(discoveryTimeoutSeconds, 1), 30)
      defaults.set(discoveryTimeoutSeconds, forKey: Key.discoveryTimeoutSeconds)
    }
  }

  private let defaults: UserDefaults

  init(defaults: UserDefaults = .standard) {
    self.defaults = defaults
    let fallback = Self.defaultConfiguration
    includeAudio = defaults.object(forKey: Key.includeAudio) as? Bool ?? fallback.includeAudio
    transport =
      defaults.string(forKey: Key.transport).flatMap(DesktopTransport.init(rawValue:))
      ?? fallback.transport
    width = Self.normalizedEven(
      defaults.object(forKey: Key.width) as? Int ?? fallback.width,
      range: 2...3840
    )
    height = Self.normalizedEven(
      defaults.object(forKey: Key.height) as? Int ?? fallback.height,
      range: 2...2160
    )
    framesPerSecond = min(
      max(defaults.object(forKey: Key.framesPerSecond) as? Int ?? fallback.framesPerSecond, 1),
      60
    )
    bitrate = min(
      max(defaults.object(forKey: Key.bitrate) as? Int ?? fallback.bitrate, 100_000),
      50_000_000
    )
    targetDelayMilliseconds = min(
      max(
        defaults.object(forKey: Key.targetDelayMilliseconds) as? Int
          ?? fallback.targetDelayMilliseconds,
        1
      ),
      5_000
    )
    discoveryTimeoutSeconds = min(
      max(
        defaults.object(forKey: Key.discoveryTimeoutSeconds) as? Int
          ?? fallback.discoveryTimeoutSeconds,
        1
      ),
      30
    )
  }

  var configuration: CastConfiguration {
    CastConfiguration(
      includeAudio: includeAudio,
      transport: transport,
      width: width,
      height: height,
      framesPerSecond: framesPerSecond,
      bitrate: bitrate,
      targetDelayMilliseconds: targetDelayMilliseconds,
      discoveryTimeoutSeconds: discoveryTimeoutSeconds
    )
  }

  private static func normalizedEven(_ value: Int, range: ClosedRange<Int>) -> Int {
    let clamped = min(max(value, range.lowerBound), range.upperBound)
    return clamped.isMultiple(of: 2) ? clamped : max(range.lowerBound, clamped - 1)
  }
}
