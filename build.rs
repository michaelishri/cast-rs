fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rerun-if-changed=native/virtual_display.m");
        println!("cargo:rerun-if-changed=native/audio_encoder.c");
        println!("cargo:rerun-if-changed=native/audio_output.c");
        if std::env::var("DOCS_RS").as_deref() != Ok("1") {
            cc::Build::new()
                .file("native/virtual_display.m")
                .flag("-fobjc-arc")
                .compile("cast_virtual_display");
            cc::Build::new()
                .file("native/audio_encoder.c")
                .compile("cast_audio_encoder");
            cc::Build::new()
                .file("native/audio_output.c")
                .compile("cast_audio_output");
        }
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=AudioToolbox");
        println!("cargo:rustc-link-lib=framework=CoreAudio");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
        println!("cargo:rustc-link-lib=framework=Foundation");

        // Swift-built ScreenCaptureKit bindings reference @rpath libraries.
        // Modern macOS supplies these through the dyld shared cache here.
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
        // Release archives place the linked FFmpeg libraries next to the executable.
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/lib");
    } else if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        // Linux release archives place redistributable media libraries here. The
        // packaging job also gives each bundled library a sibling-relative RUNPATH.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/lib");
    }
}
