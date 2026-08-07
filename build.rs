fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // Swift-built ScreenCaptureKit bindings reference @rpath libraries.
        // Modern macOS supplies these through the dyld shared cache here.
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
