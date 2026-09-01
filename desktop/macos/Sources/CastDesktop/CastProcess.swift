import Foundation

enum CastProcessError: LocalizedError {
  case executableNotFound
  case launchFailed(String)
  case commandFailed(status: Int32, message: String)
  case invalidOutput(String)

  var errorDescription: String? {
    switch self {
    case .executableNotFound:
      "The bundled cast command could not be found. Reinstall Cast.app."
    case .launchFailed(let message):
      "Could not launch cast: \(message)"
    case .commandFailed(_, let message):
      message.isEmpty ? "The cast command failed." : message
    case .invalidOutput(let message):
      message
    }
  }
}

struct ProcessOutput: Sendable {
  let stdout: Data
  let stderr: Data
  let status: Int32
}

final class LockedOutputBuffer: @unchecked Sendable {
  private let lock = NSLock()
  private var data = Data()
  private let limit: Int

  init(limit: Int = 64 * 1024) {
    self.limit = limit
  }

  func append(_ incoming: Data) {
    guard !incoming.isEmpty else { return }
    lock.lock()
    defer { lock.unlock() }
    data.append(incoming)
    if data.count > limit {
      data.removeFirst(data.count - limit)
    }
  }

  func snapshot() -> Data {
    lock.lock()
    defer { lock.unlock() }
    return data
  }
}

enum CastExecutable {
  static func resolve(environment: [String: String] = ProcessInfo.processInfo.environment) throws
    -> URL
  {
    if let override = environment["CAST_CLI_PATH"], !override.isEmpty {
      let url = URL(fileURLWithPath: override)
      guard FileManager.default.isExecutableFile(atPath: url.path) else {
        throw CastProcessError.executableNotFound
      }
      return url
    }

    if let resources = Bundle.main.resourceURL {
      let bundled = resources.appendingPathComponent("runtime/cast")
      if FileManager.default.isExecutableFile(atPath: bundled.path) {
        return bundled
      }
    }

    let source = URL(fileURLWithPath: #filePath)
    let repository =
      source
      .deletingLastPathComponent()
      .deletingLastPathComponent()
      .deletingLastPathComponent()
      .deletingLastPathComponent()
      .deletingLastPathComponent()
    let development = repository.appendingPathComponent("target/debug/cast")
    guard FileManager.default.isExecutableFile(atPath: development.path) else {
      throw CastProcessError.executableNotFound
    }
    return development
  }
}

@MainActor
final class CastProcessController {
  private(set) var discoveryProcess: Process?
  private(set) var sessionProcess: Process?
  private var sessionErrorBuffer: LockedOutputBuffer?

  func cancelDiscovery() {
    guard let process = discoveryProcess else { return }
    if process.isRunning { process.terminate() }
    discoveryProcess = nil
  }

  func discover(
    executable: URL,
    timeout: Int,
    completion: @escaping @MainActor (Result<[CastDevice], Error>) -> Void
  ) throws {
    cancelDiscovery()
    let process = Process()
    let stdout = Pipe()
    let stderr = Pipe()
    process.executableURL = executable
    process.arguments = CastCommandBuilder.discoveryArguments(timeout: timeout)
    process.standardOutput = stdout
    process.standardError = stderr
    discoveryProcess = process
    process.terminationHandler = { [weak self] finished in
      let output = ProcessOutput(
        stdout: stdout.fileHandleForReading.readDataToEndOfFile(),
        stderr: stderr.fileHandleForReading.readDataToEndOfFile(),
        status: finished.terminationStatus
      )
      Task { @MainActor [weak self] in
        guard self?.discoveryProcess === finished else { return }
        self?.discoveryProcess = nil
        completion(Self.decodeDevices(output))
      }
    }
    do {
      try process.run()
    } catch {
      discoveryProcess = nil
      throw CastProcessError.launchFailed(error.localizedDescription)
    }
  }

  func startSession(
    executable: URL,
    arguments: [String],
    completion: @escaping @MainActor (Result<Void, Error>) -> Void
  ) throws {
    guard sessionProcess == nil else { return }
    let process = Process()
    let stderr = Pipe()
    let buffer = LockedOutputBuffer()
    process.executableURL = executable
    process.arguments = arguments
    process.standardOutput = FileHandle.nullDevice
    process.standardError = stderr
    stderr.fileHandleForReading.readabilityHandler = { handle in
      buffer.append(handle.availableData)
    }
    sessionProcess = process
    sessionErrorBuffer = buffer
    process.terminationHandler = { [weak self] finished in
      stderr.fileHandleForReading.readabilityHandler = nil
      buffer.append(stderr.fileHandleForReading.readDataToEndOfFile())
      let errorText = String(decoding: buffer.snapshot(), as: UTF8.self).trimmingCharacters(
        in: .whitespacesAndNewlines)
      let result: Result<Void, Error> =
        finished.terminationStatus == 0
        ? .success(())
        : .failure(
          CastProcessError.commandFailed(status: finished.terminationStatus, message: errorText))
      Task { @MainActor [weak self] in
        guard self?.sessionProcess === finished else { return }
        self?.sessionProcess = nil
        self?.sessionErrorBuffer = nil
        completion(result)
      }
    }
    do {
      try process.run()
    } catch {
      stderr.fileHandleForReading.readabilityHandler = nil
      sessionProcess = nil
      sessionErrorBuffer = nil
      throw CastProcessError.launchFailed(error.localizedDescription)
    }
  }

  func stopSession() {
    guard let process = sessionProcess, process.isRunning else { return }
    process.interrupt()
  }

  static func decodeDevices(_ output: ProcessOutput) -> Result<[CastDevice], Error> {
    guard output.status == 0 else {
      let error = String(decoding: output.stderr, as: UTF8.self).trimmingCharacters(
        in: .whitespacesAndNewlines)
      return .failure(CastProcessError.commandFailed(status: output.status, message: error))
    }
    do {
      let devices = try JSONDecoder().decode([CastDevice].self, from: output.stdout)
      return .success(
        devices.sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending })
    } catch {
      return .failure(
        CastProcessError.invalidOutput(
          "Cast returned invalid device data: \(error.localizedDescription)"))
    }
  }
}
