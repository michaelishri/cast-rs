# macOS desktop integration

This Swift package builds the native macOS 13+ Cast menu bar app. During development, set `CAST_CLI_PATH` to an executable Cast CLI or build `target/debug/cast` in the repository root before launching the app.

```sh
swift test --package-path desktop/macos
swift build --package-path desktop/macos
CAST_CLI_PATH="$PWD/target/debug/cast" swift run --package-path desktop/macos CastDesktop
```

Release packaging assembles the executable and bundled Cast runtime into `Cast.app`; running the raw SwiftPM executable is intended only for development.

## Release packaging

Build the Rust release binary and redistributable FFmpeg libraries first, then package on the
matching native architecture:

```sh
./scripts/package-macos-release.sh "$(uname -m)"
```

The resulting archive contains both `Cast.app` and the standalone CLI/lib layout. The packaging
script fixes runtime search paths, creates the application icon and bundle metadata, ad-hoc signs
nested code and the app, verifies the signature and plist, audits linked libraries, launches the
app briefly, extracts the archive, and smoke-tests both CLI copies.

Install by dragging `Cast.app` into **Applications**. Allow Local Network access for discovery and
Screen Recording access for desktop casting. Quit and reopen Cast after granting Screen Recording
access. Launch at Login is available under **Settings → General** and uses the macOS login-item API.

The release is not Developer-ID signed or notarized, so Gatekeeper may require the archive's
quarantine attribute to be removed after its checksum has been verified.
