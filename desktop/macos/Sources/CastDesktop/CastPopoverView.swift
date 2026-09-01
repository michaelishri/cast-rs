import AppKit
import SwiftUI

struct CastMenuView: View {
  @EnvironmentObject private var model: CastAppModel

  var body: some View {
    Group {
      if let active = model.activeCast {
        activeCastMenu(active)
        Divider()
      }

      receiverList

      if let message = model.errorMessage {
        Divider()
        Button(message) {}
          .disabled(true)
      }

      Divider()
      Button(model.isDiscovering ? "Searching…" : "Refresh Receivers") {
        refresh()
      }
      .disabled(model.isDiscovering)

      CastSettingsMenuItem()

      Divider()
      Button("Quit Cast") {
        model.shutdown()
        NSApp.terminate(nil)
      }
    }
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
      Button("Searching for Cast devices…") {}
        .disabled(true)
    } else if model.devices.isEmpty {
      Button("No Cast devices found") {}
        .disabled(true)
    } else {
      ForEach(model.devices) { device in
        Menu {
          if device.capability.supportsDesktop {
            Button("Extended Desktop") {
              model.startCasting(to: device, mode: .extend)
            }
            .disabled(model.hasActiveCast)

            if model.displays.isEmpty {
              Button("No displays available") {}
                .disabled(true)
            } else {
              ForEach(model.displays) { display in
                Button("Mirror \(display.menuLabel)") {
                  model.startCasting(to: device, mode: .mirror(display))
                }
                .disabled(model.hasActiveCast)
              }
            }
          } else {
            Button("Audio-only receiver") {}
              .disabled(true)
          }
        } label: {
          Label(
            device.name,
            systemImage: device.capability.supportsDesktop ? "tv" : "hifispeaker"
          )
        }
      }
    }
  }

  private func activeCastMenu(_ active: ActiveCast) -> some View {
    Menu {
      Button(active.mode.label) {}
        .disabled(true)

      if let state = active.state.label {
        Button(state) {}
          .disabled(true)
      }

      Divider()
      Section("Resolution") {
        ForEach(CastResolution.presets) { resolution in
          Button {
            model.setActiveResolution(resolution)
          } label: {
            if resolution.matches(active.configuration) {
              Label(resolution.label, systemImage: "checkmark")
            } else {
              Text(resolution.label)
            }
          }
          .disabled(active.state.isBusy || resolution.matches(active.configuration))
        }
      }

      Toggle(
        "Cast Audio",
        isOn: Binding(
          get: { active.configuration.includeAudio },
          set: { model.setActiveAudio($0) }
        )
      )
      .disabled(active.state.isBusy)

      Divider()
      Button("Stop Casting", role: .destructive) {
        model.stopCasting()
      }
      .disabled(active.state == .stopping)
    } label: {
      HStack {
        CastGlyph(pointSize: CastIcon.menuItemPointSize)
        Text("Casting to \(active.device.name)")
      }
    }
  }

  private func refresh() {
    model.refreshDisplays()
    model.refreshDevices()
  }
}

private struct CastSettingsMenuItem: View {
  var body: some View {
    if #available(macOS 14.0, *) {
      SettingsLink {
        Text("Settings…")
      }
    } else {
      Button("Settings…") {
        NSApp.activate(ignoringOtherApps: true)
        NSApp.sendAction(Selector(("showSettingsWindow:")), to: nil, from: nil)
      }
    }
  }
}
