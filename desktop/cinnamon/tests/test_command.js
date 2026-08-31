imports.searchPath.unshift(ARGV[0]);
const Command = imports.command;

let discovery = Command.buildDiscoveryCommand("/opt/cast", 4);
if (discovery.join(" ") !== "/opt/cast devices --timeout 4 --json")
    throw new Error(`unexpected discovery command: ${discovery}`);

let desktop = Command.buildDesktopCommand("cast", {address: "192.0.2.1", port: 8009}, {
    mode: "extend",
    audio: true,
    backend: "x11",
    transport: "mirror",
    encoder: "vaapi",
    width: 1280,
    height: 720,
    fps: 30,
    bitrate: 6000000,
    targetDelayMs: 200,
}, 4242);

for (let expected of ["desktop", "--host", "192.0.2.1", "--extend", "--audio", "--controller-pid", "4242"])
    if (desktop.indexOf(expected) < 0)
        throw new Error(`missing ${expected} in desktop command: ${desktop}`);

let fallback = Command.buildDesktopCommand("cast", {address: "192.0.2.2"}, {
    mode: "mirror",
    audio: false,
    backend: "unsupported",
    transport: "unsupported",
    encoder: "unsupported",
    width: 640,
    height: 480,
    fps: 25,
    bitrate: 1000000,
    targetDelayMs: 100,
}, 99);
if (fallback.indexOf("--extend") >= 0 || fallback.indexOf("--audio") >= 0)
    throw new Error(`mirror fallback unexpectedly enabled flags: ${fallback}`);
if (fallback[fallback.indexOf("--backend") + 1] !== "auto")
    throw new Error(`invalid backend was not sanitized: ${fallback}`);
