# PLAN: `cast receive` — turn the CLI into a Google Cast receiver

Status: draft for review. On approval, these phases become Kaneo tickets.

## Goal

Add a `cast receive` command that makes the machine running Cast behave like a
Google Cast receiver. After `cast receive` starts:

- The machine appears as a cast target in Cast sender apps (verified for
  auth-tolerant senders; see the compatibility matrix).
- A sender can connect, launch the Default Media Receiver app, and `LOAD`
  media; the CLI decodes and renders it locally (video in a window, audio to
  the local output device), honouring PLAY/PAUSE/SEEK/STOP/volume commands.
- The existing sender side can be pointed at it (`cast devices`,
  `cast video --host <this-machine>`) as a built-in end-to-end test path.

This inverts the project's current role: today `cast` is a pure sender that
speaks the Cast protocol as a client. The receiver feature adds the server
side of the same protocol, plus a local playback engine.

## Non-goals (for this feature)

- **Mirroring/screen-tab receiver.** Chrome tab casting and phone screen
  mirroring require the receiver side of Google's WebRTC-based mirroring apps
  (ICE/DTLS/SCTP plus the mirroring app namespaces). The project's
  `mirror.rs` implements the sender half; the receiver half is a separate,
  much larger effort and is explicitly out of scope here.
- **First-party sender compatibility via spoofed device-auth chains.** Real
  senders (Chrome, Google Home, Android system cast, YouTube) verify a
  device-auth certificate chain rooted in Google's Cast CA. Faking that chain
  requires keys mined from rooted devices — legally and ethically out of
  scope. We implement the device-auth namespace honestly (self-signed chain)
  and document the consequences.
- **YouTube receiver app (`233637DE`), DASH/DRM, subtitles, queueLoad.**
  Reported as unavailable; clear NACKs where senders expect answers.
- **Accepting our own `cast desktop` mirror transport.** The low-latency
  mirror transport is proprietary and asymmetric; only the HLS fallback is
  tractable (see "Future phases").

## Current-state summary (what we build on)

| Existing asset | Reuse for receiver |
|---|---|
| `rust_cast` fork (pinned) exposes generated protobuf types `cast::cast_channel::CastMessage` and `cast::authority_keys` (protobuf crate 3.7.2) | Server-side message encode/decode without vendoring `cast_channel.proto` or adding prost |
| `mdns-sd` 0.20 (`browse` today) | `ServiceDaemon::register`/`unregister` with TXT properties for advertisement |
| `rustls` 0.23 + `aws_lc_rs` provider | TLS *server* listener; `rcgen` (same backend) generates the runtime self-signed cert |
| `ffmpeg-next` 8.1 (codec/format/swscale/swresample, **`--disable-network`**) | Decode any sender-provided media locally to RGBA + stereo f32 |
| `media_server.rs` | Pattern for the inverse: a range-*fetching* proxy that lets ffmpeg read remote media over localhost HTTP |
| Sender stack (`cast.rs`, `video.rs`, `playback.rs`) | Dogfood client for integration tests and manual E2E |
| `tui::LogCapture` pattern | Optional JSON status output for desktop integrations |

Key constraint: the pinned FFmpeg libraries are built with `--disable-network`
(`scripts/build-ffmpeg-libraries.sh`), so libavformat cannot open sender URLs
directly. The receiver must fetch media itself and hand it to ffmpeg via a
local file or localhost HTTP proxy (described below).

## Protocol design

### 1. Discovery advertisement

Advertise `_googlecast._tcp.local.` on the default interface(s) via
`mdns_sd::ServiceDaemon::register`, instance name = friendly name, port =
receiver port, host name `<friendly-name>.local.`, with TXT records:

- `id` — stable UUID without dashes. Derived once per machine (persisted under
  the user config dir, e.g. `~/.config/cast/receiver-id`; fall back to a
  hostname-derived UUID when unwritable). pychromecast skips devices whose
  `id` is missing/unparseable, so this field is mandatory.
- `fn` — friendly name (matches the instance name).
- `md` — model string, e.g. `Cast Desktop Receiver` (pychromecast maps this
  to a cast type; an unknown model is fine).
- `ca` — capability bitmask: `5` (video) or `4` (audio-only), matching
  `discovery.rs` (`1` = video out, `4` = audio out). Resolved per plan review:
  the default is auto-detected — video when a display is attached, audio-only
  on headless machines; `--capabilities` overrides the detection.
- `ve` — `02`; plus harmless plausibility records `st=0`, `rm=`, `bs=` (random
  per session), `cd=` (stable hash of `id`), `ic=` (empty).
- `--capabilities audio` also omits video rendering UI paths.

Unregister on Ctrl-C/error paths (the daemon also announces goodbye packets).

### 2. TLS transport and framing

- `rustls::ServerConnection` per accepted TCP connection on `--port` (default
  `8009`), certificate: RSA-2048 self-signed generated at startup with
  `rcgen` (CN = friendly name; 24h validity like real Chromecasts). RSA keeps
  strict senders happy; the key/cert are regenerated each run.
- Framing: 4-byte big-endian length prefix + `CastMessage` protobuf, exactly
  mirroring `rust_cast::message_manager` client behaviour. Payloads are
  JSON (`payload_utf8`) for all namespaces we handle; `payload_binary` only
  for device auth.
- Connection bookkeeping: track each sender's virtual connections
  (`tp.connection` CONNECT/CLOSE), heartbeats, and per-sender addresses.

### 3. Platform namespaces (destination `receiver-0`)

| Namespace | Behaviour |
|---|---|
| `...tp.connection` | Track CONNECT/CLOSE per sender id; broadcast replies to `*` where appropriate |
| `...tp.heartbeat` | Reply `PONG` to `PING` immediately; send own `PING` every 5 s; drop a sender after ~10 s of silence |
| `...receiver` | `GET_STATUS` → `RECEIVER_STATUS` (applications list + volume); `SET_VOLUME` → update volume (see semantics below) + broadcast status; `LAUNCH {appId}` → only `CC1AD845` (Default Media Receiver) is supported: mint `sessionId` + `transportId`, reply `RECEIVER_STATUS` with the running app; `LAUNCH_ERROR` for unsupported ids; `GET_APP_AVAILABILITY` → per-id available map; `STOP {sessionId}` → end session, back to idle |
| `...tp.deviceauth` | Reply to `AUTH_CHALLENGE` with an `AUTH_RESPONSE`: signature over the TLS peer certificate using our generated key, `client_auth_certificate` = generated cert, `ica`/root = our self-signed chain. Honest but unverifiable — strict senders will flag or hide the device (documented) |

Idle (no app) status: `applications: []`. Media session teardown mirrors the
Default Media Receiver: `MEDIA_STATUS` with `playerState: "IDLE"` and
`idleReason`, then `STOP` of the app after a short idle window.

### 4. Media namespace (destination = app `transportId`)

Implement the Default Media Receiver subset:

- `LOAD { media { contentId, contentType, streamType, duration, metadata },
  autoplay, currentTime }` → start the player pipeline; immediately broadcast
  `MEDIA_STATUS` (`playerState: BUFFERING`, `mediaSessionId` allocated);
  transition to `PLAYING` once output starts. Unsupported content types →
  `LOAD_FAILED` status with a diagnosable error reason.
- `GET_STATUS` → current `MEDIA_STATUS`.
- `PLAY`, `PAUSE`, `SEEK { currentTime }`, `STOP` → drive the player; each
  command echoes a `MEDIA_STATUS` with the sender's `requestId`.
- `SET_VOLUME` (session level) and receiver-level `SET_VOLUME` both map to the
  player's output gain; `muted` honoured. We scale our own output rather than
  touching the OS master volume (predictable, no extra platform code).
- Broadcast `MEDIA_STATUS` on every state change and at ~1 s intervals while
  playing, with `currentTime` from the playback clock.
- Single active media session; a new `LOAD` replaces current playback (DMR
  behaviour). Concurrent senders may all control the session; statuses are
  broadcast to every connected sender.
- `queueLoad`, `EDIT_TRACKS_INFO` → explicit `LOAD_FAILED`/`INVALID_REQUEST`
  NACKs in v1 (not silent).

### 5. Media fetching (given `--disable-network`)

The `contentId` URL usually points at the sender's HTTP server (our own
`cast video` serves range-capable MP4 this way; CATT/VLC serve HTTP as well).

- **Primary path — local range proxy (VOD).** A localhost-only HTTP server
  built on `tiny_http` (decided at plan review) that serves the remote origin:
  ffmpeg is opened on `http://127.0.0.1:<ephemeral>/<id>` while the proxy
  satisfies ffmpeg's range requests by fetching upstream ranges with `ureq`.
  This preserves seeking, works for non-faststart MP4, and needs no full-file
  download. `ureq` currently sits under `cfg(target_os = "linux")` — move it
  to the common dependency table.
- **Fallback path — download-to-temp.** For origins that misbehave under
  range requests, stream the whole body to a temp file with progress, then
  open it locally.
- **HLS/live sources are rejected in v1** with a clear reason. A follow-up
  phase can add a segment-fetching proxy, which would also let our own
  `cast desktop --transport hls` (or `--serve-only`) act as a sender to
  ourselves. ffmpeg *could* demux HLS if it had network enabled; it does not.

## Local playback engine (`receiver/player.rs`)

A self-contained local AV player, driven by the media session:

- **Decode:** `ffmpeg-next` demux/decode the proxied input; swscale to RGBA
  frames; swresample to stereo f32 at 48 kHz.
- **Audio:** `cpal` output stream (CoreAudio on macOS; ALSA host on Linux,
  which rides the system PipeWire). The audio callback consumes samples off a
  ring buffer and is the master clock (samples-played → media clock).
- **Video:** `winit` window + `softbuffer` CPU blit of RGBA frames at
  presentation time (audio-clock driven; drop late frames). Window titled
  `fn` + media title. `--capabilities audio` or `--no-window` skips video
  (audio-only decode of video files still demuxes the audio track).
- **Controls:** pause/resume/seek adjust the clock and ring buffers; volume
  applies output gain; EOF triggers session idle handling.
- This is conventional A/V sync plumbing, but it is the largest single new
  component. It shares no state with the sender paths, which keeps the blast
  radius contained.

## CLI surface

```
cast receive [OPTIONS]

  --name <friendly-name>     advertised name (default: "<hostname> Cast")
  --model <model>            advertised model (default: "Cast Desktop Receiver")
  --port <port>              Cast protocol port (default: 8009)
  --capabilities <video|audio|auto>  advertised capability (default: auto:
                             video with a display attached, audio-only headless)
  --bind <ip>                interface to bind (default: any private address)
  --accept <ip>              optional sender allowlist (repeatable; default: accept LAN)
  --no-window                never open a video window (audio only)
  --json                     machine-readable one-line JSON status events
  --seconds <n>              exit after n seconds (scripted use/tests)
```

Runs until Ctrl-C: registers mDNS, logs connections/loads, unregisters and
shuts down cleanly. `--json` mirrors the desktop-integration conventions used
by `devices --json`.

Per plan review, daemon/background operation is in scope: `cast receive` gains
a headless daemon path that follows the existing desktop-integration patterns
(hidden `--controller-pid` supervision and `--json` status events on stdout),
so the macOS menu bar app and Cinnamon applet can spawn, monitor, and stop a
receiver without a foreground terminal. Details are ticketed as Phase 4 below.

## Module layout

```
src/receiver/mod.rs        orchestrator: options, lifecycle, shutdown
src/receiver/advertise.rs  mDNS registration (mdns-sd register/unregister)
src/receiver/server.rs     TLS listener, CastMessage frame codec, sender registry
src/receiver/platform.rs   receiver namespace state machine (+ session/transport ids)
src/receiver/media.rs      media namespace state machine (session + statuses)
src/receiver/auth.rs       device-auth challenge handling
src/receiver/fetch.rs      origin fetch: range proxy + download fallback
src/receiver/player.rs     decode + cpal/winit/softbuffer playback engine
```

New dependencies: `cpal`, `winit`, `softbuffer`, `rcgen`; promote `ureq` to a
common dependency. All are pure-Rust or already-linked ecosystems (rcgen uses
aws-lc-rs, matching the installed rustls provider; cpal/winit add no new C
dependencies beyond what the platforms ship).

## Phases (each becomes a Kaneo ticket; commit + push per ticket)

### Phase 1 — Receiver skeleton: advertisement + protocol server
- mDNS advertisement with the TXT set above; `cast receive` lifecycle
  (arg parsing, logging, Ctrl-C, `--json`).
- TLS server + framing + connection/heartbeat handling.
- Receiver namespace: status/volume/launch/stop/app-availability; session and
  transport id minting; device-auth response.
- Acceptance: `cast devices` lists the machine; `catt -d <name> info` (or any
  pychromecast client) sees a Default-Media-Receiver-capable device and can
  launch/stop the app (no playback yet — LOAD NACKs as unsupported).
- Unit tests for framing, TXT records, and the platform state machine.

### Phase 2 — Media receiver (VOD playback)
- Media namespace state machine + `fetch.rs` range proxy/download fallback.
- `player.rs` decode/playback engine (audio clock, window, seek/pause/volume).
- Acceptance: `cast video --host <this-machine> file.mp4` plays in a local
  window; CATT (`catt cast file.mp4 -d <name>`) and VLC (renderer output) cast
  to it; PLAY/PAUSE/SEEK/STOP and volume/mute commands work; end-of-media
  returns the app to idle cleanly.

### Phase 3 — Hardening + integration tests + docs
- Sender lifecycle edge cases: second LOAD replaces playback, CLOSE teardown,
  sender disconnect mid-playback, allowlist enforcement, malformed frames.
- Loopback integration test: boot receiver on `127.0.0.1` ephemeral port,
  drive it with the existing sender stack against a tiny generated MP4,
  assert status transitions (no mDNS in CI).
- README section, troubleshooting entries (macOS Local Network permission for
  the terminal, firewall inbound prompt for port 8009), compatibility matrix.
- Acceptance: quality gate passes (`fmt`, `clippy -D warnings`, `test`,
  release build) on macOS and Ubuntu CI.

### Phase 4 — Daemon/background operation (per plan review decision)
- Headless `--json` status stream + hidden `--controller-pid` supervision for
  `cast receive`, matching the `cast desktop` controller pattern.
- Hook-up so the existing desktop integrations can start/stop a receiver in
  the background and observe session state (follow-on tickets per desktop
  integration may be needed there).
- Acceptance: `cast receive --json --controller-pid <pid>` runs detached,
  emits valid one-line JSON events, and shuts down cleanly when the
  controller exits; quality gate still passes.

## Testing strategy

- **Unit:** frame codec round-trips, namespace state machines as pure logic,
  TXT record contents, id persistence, allowlist matcher.
- **Dogfood integration (loopback):** the receiver's protocol server is
  exercised by our own rust_cast-based sender — launch, load a small fixture,
  seek, stop. This is cheap, deterministic, and guards the protocol surface
  against regressions in either role.
- **Manual matrix per phase:** CATT, VLC, Home Assistant (pychromecast-based),
  our own CLI; document which first-party senders discover/flag/refuse the
  device so the README can be honest.

## Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| First-party senders (Chrome/Google Home/YouTube) verify device auth | Device hidden or "unverified"; no cast | Out of scope by design; document matrix; third-party senders + our CLI are the supported targets |
| Sender codecs exceed local decode ability (e.g. sender transcodes anyway) | LOAD_FAILED | ffmpeg handles common H.264/VP8/VP9/AV1-in-MP4/WebM; explicit error reasons otherwise |
| mp4 without faststart over the proxy stalls | Slow start | Proxy fetches metadata ranges (moov probe) and falls back to full download when needed |
| Port 8009 conflicts / permission prompts (macOS firewall, Local Network) | Runtime friction | `--port` override; actionable first-run diagnostics mirroring existing README tone |
| Playback engine complexity (A/V sync, device quirks) | Schedule slip in Phase 2 | Isolated `player.rs`; audio-clock design is well-trodden; window can be disabled for CI-ish checks |
| Advertised name length / mDNS quirks | Discovery misses | Validate/clamp instance names; unit-test TXT set; reuse `discovery.rs` fixtures in tests |

## Compatibility matrix (intended claims, verified in Phase 3)

| Sender | Device auth enforced | Expected result |
|---|---|---|
| Cast CLI (this project) | No | Full support (primary dogfood target) |
| CATT / pychromecast / Home Assistant | No | Discover + cast media |
| VLC, Kodi | No | Discover + cast media |
| Chrome tab/screen cast | Yes + WebRTC mirroring | Out of scope (non-goal) |
| Google Home / Android system sender | Yes | Likely "unverified"; unsupported in v1 |

## Resolved decisions (plan review)

1. Default friendly name: `"<hostname> Cast"`.
2. Audio-only auto-detection on machines with no display: yes —
   `--capabilities` defaults to `auto`.
3. Daemon/background mode via the desktop integrations: in scope (Phase 4).
4. Origin fetch proxy: `tiny_http`.
