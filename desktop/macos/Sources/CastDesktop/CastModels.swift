import AppKit
import Foundation

enum DeviceCapability: String, Codable, Sendable {
  case audioOnly = "audio_only"
  case video
  case unknown

  var supportsDesktop: Bool { self != .audioOnly }
}

struct CastDevice: Codable, Identifiable, Equatable, Sendable {
  let name: String
  let model: String
  let capability: DeviceCapability
  let address: String
  let port: UInt16

  var id: String { "\(address):\(port)" }
}

struct CastDisplay: Identifiable, Equatable, Sendable {
  let id: UInt32
  let name: String
  let width: Int
  let height: Int
  let isMain: Bool

  var menuLabel: String {
    isMain ? "\(name) (Main)" : name
  }

  @MainActor
  static func current() -> [CastDisplay] {
    NSScreen.screens.compactMap { screen in
      guard
        let number = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")]
          as? NSNumber
      else {
        return nil
      }
      let size = screen.convertRectToBacking(screen.frame).size
      return CastDisplay(
        id: number.uint32Value,
        name: screen.localizedName,
        width: Int(size.width.rounded()),
        height: Int(size.height.rounded()),
        isMain: screen == NSScreen.main
      )
    }
    .sorted { left, right in
      if left.isMain != right.isMain { return left.isMain }
      return left.name.localizedStandardCompare(right.name) == .orderedAscending
    }
  }
}

enum DesktopTransport: String, CaseIterable, Identifiable, Sendable {
  case mirror
  case hls

  var id: String { rawValue }
  var label: String { self == .mirror ? "Low-latency mirror" : "HLS" }
}

struct CastConfiguration: Equatable, Sendable {
  var includeAudio = false
  var transport: DesktopTransport = .mirror
  var width = 1280
  var height = 720
  var framesPerSecond = 30
  var bitrate = 6_000_000
  var targetDelayMilliseconds = 200
  var discoveryTimeoutSeconds = 3
}

enum CastMode: Equatable, Sendable {
  case mirror(CastDisplay)
  case extend

  var label: String {
    switch self {
    case .mirror(let display): "Mirroring \(display.name)"
    case .extend: "Extended Desktop"
    }
  }
}

struct ActiveCast: Equatable, Sendable {
  let device: CastDevice
  let mode: CastMode
  var isStopping: Bool
}
