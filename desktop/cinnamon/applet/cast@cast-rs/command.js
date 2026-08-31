const VALID_BACKENDS = ["auto", "x11"];
const VALID_ENCODERS = ["auto", "nvenc", "vaapi", "openh264"];
const VALID_TRANSPORTS = ["mirror", "hls"];


function choice(value, allowed, fallback) {
    return allowed.indexOf(value) >= 0 ? value : fallback;
}


var buildDiscoveryCommand = function(executable, timeout) {
    return [executable || "cast", "devices", "--timeout", String(timeout), "--json"];
};


var buildDesktopCommand = function(executable, device, options, controllerPid) {
    let argv = [
        executable || "cast",
        "desktop",
        "--host", String(device.address),
        "--cast-port", String(device.port || 8009),
        "--backend", choice(options.backend, VALID_BACKENDS, "auto"),
        "--transport", choice(options.transport, VALID_TRANSPORTS, "mirror"),
        "--encoder", choice(options.encoder, VALID_ENCODERS, "auto"),
        "--width", String(options.width),
        "--height", String(options.height),
        "--fps", String(options.fps),
        "--bitrate", String(options.bitrate),
        "--target-delay-ms", String(options.targetDelayMs),
        "--controller-pid", String(controllerPid),
    ];

    if (options.audio)
        argv.push("--audio");
    if (options.mode === "extend")
        argv.push("--extend");

    return argv;
};
