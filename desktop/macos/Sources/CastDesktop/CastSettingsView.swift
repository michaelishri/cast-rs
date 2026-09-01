import SwiftUI

struct CastSettingsView: View {
  @EnvironmentObject private var preferences: CastPreferences
  @EnvironmentObject private var permissions: ScreenRecordingController
  @EnvironmentObject private var loginItem: LoginItemController
  @EnvironmentObject private var diagnostics: CastDiagnostics

  var body: some View {
    TabView {
      general
        .tabItem { Label("General", systemImage: "gear") }
      videoQuality
        .tabItem { Label("Video Quality", systemImage: "display") }
      permissionsAndDiagnostics
        .tabItem { Label("Permissions", systemImage: "lock.shield") }
    }
    .padding(20)
    .frame(width: 540, height: 390)
    .onAppear {
      permissions.refresh()
      loginItem.refresh()
      diagnostics.refresh()
    }
  }

  private var general: some View {
    Form {
      Toggle(
        "Launch Cast at login",
        isOn: Binding(
          get: { loginItem.state.isEnabled },
          set: { loginItem.setEnabled($0) }
        )
      )
      .disabled(loginItem.isChanging)
      Text(loginItem.state.label)
        .font(.caption)
        .foregroundStyle(.secondary)
      if loginItem.state == .requiresApproval {
        Button("Open Login Items Settings") { loginItem.openSettings() }
      }

      Toggle("Include system audio", isOn: $preferences.includeAudio)
      Picker("Transport", selection: $preferences.transport) {
        ForEach(DesktopTransport.allCases) { transport in
          Text(transport.label).tag(transport)
        }
      }
      Stepper(
        "Discovery timeout: \(preferences.discoveryTimeoutSeconds) seconds",
        value: $preferences.discoveryTimeoutSeconds,
        in: 1...30
      )
    }
    .formStyle(.grouped)
  }

  private var videoQuality: some View {
    Form {
      Picker("Resolution", selection: resolutionBinding) {
        ForEach(CastResolution.presets) { resolution in
          Text(resolution.label).tag(resolution.id)
        }
      }
      Stepper(
        "Frame rate: \(preferences.framesPerSecond) fps",
        value: $preferences.framesPerSecond,
        in: 1...60
      )
      Stepper(
        "Bitrate: \(preferences.bitrate / 1_000_000) Mbps",
        value: $preferences.bitrate,
        in: 1_000_000...50_000_000,
        step: 1_000_000
      )
      Stepper(
        "Target delay: \(preferences.targetDelayMilliseconds) ms",
        value: $preferences.targetDelayMilliseconds,
        in: 50...5_000,
        step: 50
      )
    }
    .formStyle(.grouped)
  }

  private var permissionsAndDiagnostics: some View {
    Form {
      LabeledContent("Screen Recording", value: permissions.statusLabel)
      if permissions.isGranted {
        Text(
          "Cast can capture your displays. Restart Cast if access was granted while it was open."
        )
        .font(.caption)
        .foregroundStyle(.secondary)
      } else {
        HStack {
          Button("Request Access") { _ = permissions.requestAccess() }
          Button("Open Screen Recording Settings") { permissions.openSettings() }
        }
        Text(
          "Screen Recording access is required for desktop casting. Restart Cast after enabling it."
        )
        .font(.caption)
        .foregroundStyle(.secondary)
      }

      Divider()
      LabeledContent("Embedded CLI", value: diagnostics.cliVersion)
      LabeledContent("CLI path") {
        Text(diagnostics.cliPath).textSelection(.enabled)
      }
      LabeledContent("Launch at Login", value: loginItem.state.label)

      Divider()
      Label(
        "Extended Desktop is experimental and may behave differently across displays.",
        systemImage: "exclamationmark.triangle"
      )
      .foregroundStyle(.secondary)
    }
    .formStyle(.grouped)
  }

  private var resolutionBinding: Binding<String> {
    Binding(
      get: { "\(preferences.width)x\(preferences.height)" },
      set: { value in
        let components = value.split(separator: "x").compactMap { Int($0) }
        guard components.count == 2 else { return }
        preferences.width = components[0]
        preferences.height = components[1]
      }
    )
  }
}
