import AppKit
import SwiftUI

struct CastPopoverView: View {
  @EnvironmentObject private var model: CastAppModel

  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      HStack {
        Text("Cast")
          .font(.headline)
        Spacer()
        if model.isDiscovering {
          ProgressView().controlSize(.small)
        }
        Button {
          model.refreshDisplays()
          model.refreshDevices()
        } label: {
          Image(systemName: "arrow.clockwise")
        }
        .buttonStyle(.borderless)
        .disabled(model.isDiscovering)
        .help("Refresh devices")
      }

      if let active = model.activeCast {
        activeCastView(active)
      } else {
        receiverList
      }

      Toggle(
        "Include system audio",
        isOn: Binding(
          get: { model.configuration.includeAudio },
          set: { model.configuration.includeAudio = $0 }
        ))

      if let message = model.errorMessage {
        Text(message)
          .font(.caption)
          .foregroundStyle(.red)
          .textSelection(.enabled)
      }

      Divider()
      HStack {
        Button("Settings…") {
          NSApp.sendAction(Selector(("showSettingsWindow:")), to: nil, from: nil)
          NSApp.activate(ignoringOtherApps: true)
        }
        .buttonStyle(.borderless)
        Spacer()
        Button("Quit Cast") {
          model.shutdown()
          NSApp.terminate(nil)
        }
        .buttonStyle(.borderless)
      }
    }
    .padding(14)
    .frame(width: 360)
    .onAppear {
      model.refreshDisplays()
      if model.devices.isEmpty && !model.isDiscovering {
        model.refreshDevices()
      }
    }
  }

  @ViewBuilder
  private var receiverList: some View {
    if model.isDiscovering && model.devices.isEmpty {
      Text("Searching for Cast devices…")
        .foregroundStyle(.secondary)
    } else if model.devices.isEmpty {
      Text("No Cast devices found")
        .foregroundStyle(.secondary)
    } else {
      ForEach(model.devices) { device in
        Menu {
          if device.capability.supportsDesktop {
            if model.displays.isEmpty {
              Text("No displays available")
            } else {
              ForEach(model.displays) { display in
                Button("Mirror \(display.menuLabel)") {
                  model.startCasting(to: device, mode: .mirror(display))
                }
              }
            }
            Divider()
            Button("Extended Desktop (Experimental)") {
              model.startCasting(to: device, mode: .extend)
            }
          } else {
            Text("Audio-only receiver")
          }
        } label: {
          HStack {
            Image(systemName: device.capability.supportsDesktop ? "tv" : "hifispeaker")
            VStack(alignment: .leading) {
              Text(device.name)
              if !device.model.isEmpty {
                Text(device.model).font(.caption).foregroundStyle(.secondary)
              }
            }
            Spacer()
          }
        }
        .disabled(!device.capability.supportsDesktop || model.hasActiveCast)
      }
    }
  }

  private func activeCastView(_ active: ActiveCast) -> some View {
    VStack(alignment: .leading, spacing: 8) {
      Label("Casting to \(active.device.name)", systemImage: "airplayvideo")
        .font(.headline)
      Text(active.mode.label)
        .foregroundStyle(.secondary)
      Button(active.isStopping ? "Stopping…" : "Stop Casting", role: .destructive) {
        model.stopCasting()
      }
      .disabled(active.isStopping)
    }
    .padding(10)
    .frame(maxWidth: .infinity, alignment: .leading)
    .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
  }
}
