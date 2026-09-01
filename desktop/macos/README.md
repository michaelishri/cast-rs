# macOS desktop integration

This Swift package builds the native macOS 13+ Cast menu bar app. During development, set `CAST_CLI_PATH` to an executable Cast CLI or build `target/debug/cast` in the repository root before launching the app.

```sh
swift test --package-path desktop/macos
swift build --package-path desktop/macos
CAST_CLI_PATH="$PWD/target/debug/cast" swift run --package-path desktop/macos CastDesktop
```

Release packaging assembles the executable and bundled Cast runtime into `Cast.app`; running the raw SwiftPM executable is intended only for development.
