use std::{
    collections::VecDeque,
    ffi::c_void,
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use muxide::{
    api::{MuxerBuilder, VideoCodec},
    fragmented::FragmentedMuxer,
};
use screencapturekit::IOSurface;
use screencapturekit::prelude::*;
use videotoolbox::ProfileLevel;
use videotoolbox::prelude::*;

use crate::{
    cast,
    network::{http_url, local_ip_for, private_route},
    virtual_display::VirtualDisplaySession,
};

const TIMESCALE: u64 = 90_000;
const PLAYLIST_WINDOW_SEGMENTS: usize = 8;
const RETAINED_SEGMENTS: usize = 32;
const STARTUP_SEGMENTS: usize = 1;

pub struct LiveOptions {
    pub cast_host: IpAddr,
    pub cast_port: u16,
    pub display_id: Option<u32>,
    pub extend: bool,
    pub http_port: u16,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    pub bitrate: i32,
    pub duration: Option<Duration>,
    pub serve_only: bool,
}

pub fn cast_desktop(mut options: LiveOptions) -> Result<()> {
    if options.bitrate <= 0 {
        bail!("bitrate must be greater than zero");
    }
    if options.extend && options.display_id.is_some() {
        bail!("--extend cannot be combined with --display");
    }

    let width = even(options.width);
    let height = even(options.height);
    let mut virtual_display = if options.extend {
        Some(VirtualDisplaySession::start(width, height, options.fps)?)
    } else {
        None
    };
    if let Some(session) = virtual_display.as_ref() {
        options.display_id = Some(session.display_id());
    }

    let lan_ip = local_ip_for(options.cast_host, options.cast_port)?;
    let serve_ip = if options.serve_only {
        if lan_ip.is_ipv4() {
            IpAddr::from([127, 0, 0, 1])
        } else {
            IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1])
        }
    } else {
        lan_ip
    };
    let route = private_route()?;
    log::debug!(
        "route to receiver uses local address {lan_ip}; HTTP listener address is {serve_ip}:{}",
        options.http_port
    );
    let store = Arc::new(HlsStore::default());
    let stop = Arc::new(AtomicBool::new(false));
    let server = HttpServer::start(
        SocketAddr::new(serve_ip, options.http_port),
        Arc::clone(&store),
        Arc::clone(&stop),
        route.clone(),
    )?;

    let content = SCShareableContent::get().context(
        "could not enumerate displays; grant Screen Recording permission in System Settings",
    )?;
    let displays = content.displays();
    let display = match options.display_id {
        Some(id) => displays
            .iter()
            .find(|display| display.display_id() == id)
            .ok_or_else(|| anyhow!("display {id} was not found"))?,
        None => displays
            .first()
            .ok_or_else(|| anyhow!("no displays found"))?,
    };
    let source_width = display.width();
    let source_height = display.height();
    log::debug!(
        "scaling source display {}x{} into H.264 output {}x{} with aspect ratio preserved",
        source_width,
        source_height,
        width,
        height
    );
    let keyframe_interval = live_keyframe_interval(options.fps);
    log::debug!(
        "using a {keyframe_interval}-frame VideoToolbox keyframe interval for sub-second HLS segments"
    );

    let encoder = CompressionSession::builder(width as i32, height as i32, Codec::H264)
        .with_real_time(true)
        .with_allow_frame_reordering(false)
        .with_average_bit_rate(options.bitrate)
        .with_expected_frame_rate(options.fps as f64)
        .with_max_keyframe_interval(keyframe_interval)
        .with_profile_level(ProfileLevel::H264Baseline3_1)
        .build()
        .context("could not create the VideoToolbox H.264 encoder")?;

    let failure = Arc::new(Mutex::new(None));
    let pipeline = LivePipeline {
        encoder,
        muxer: None,
        store: Arc::clone(&store),
        last_surface: None,
        frame_index: 0,
        frames_in_segment: 0,
        first_capture_timestamp: None,
        last_timestamp: None,
        segment_start_timestamp: None,
        fps: options.fps,
        width,
        height,
        bitrate: options.bitrate as u32,
    };

    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();
    let frame_interval = CMTime::new(1, options.fps as i32);
    let config = SCStreamConfiguration::new()
        .with_width(width)
        .with_height(height)
        .with_scales_to_fit(true)
        .with_preserves_aspect_ratio(true)
        .with_pixel_format(PixelFormat::YCbCr_420v)
        .with_shows_cursor(true)
        .with_queue_depth(4)
        .with_minimum_frame_interval(&frame_interval);
    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(
        LiveFrameHandler {
            pipeline: Mutex::new(pipeline),
            failure: Arc::clone(&failure),
            repeated_samples: AtomicU64::new(0),
            skipped_samples: AtomicU64::new(0),
        },
        SCStreamOutputType::Screen,
    );

    println!(
        "Capturing display {} at {}x{} into {}x{}, {} fps...",
        display.display_id(),
        source_width,
        source_height,
        width,
        height,
        options.fps
    );
    stream
        .start_capture()
        .context("could not start screen capture")?;

    let startup_started = Instant::now();
    loop {
        if store.wait_until_ready(Duration::from_millis(100), STARTUP_SEGMENTS) {
            break;
        }
        if let Some(error) = take_failure(&failure)? {
            stream.stop_capture().ok();
            bail!("live encoder failed: {error}");
        }
        if let Some(session) = virtual_display.as_mut() {
            session.ensure_alive()?;
        }
        if startup_started.elapsed() >= Duration::from_secs(12) {
            stream.stop_capture().ok();
            bail!("timed out waiting for {STARTUP_SEGMENTS} HLS media segments");
        }
    }

    let url = http_url(
        SocketAddr::new(serve_ip, options.http_port),
        &format!("/{route}/master.m3u8"),
    );
    println!("Live stream ready at {url}");
    if !options.serve_only {
        cast::cast_fmp4_hls(options.cast_host, options.cast_port, &url)?;
        println!("Casting desktop. Press Ctrl-C to stop.");
    } else {
        println!("Serving without contacting the Cast receiver. Press Ctrl-C to stop.");
    }

    let interrupted = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&interrupted);
    ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
        .context("could not install Ctrl-C handler")?;
    let started = Instant::now();
    loop {
        if interrupted.load(Ordering::SeqCst)
            || options
                .duration
                .is_some_and(|duration| started.elapsed() >= duration)
        {
            break;
        }
        if let Some(error) = take_failure(&failure)? {
            bail!("live encoder failed: {error}");
        }
        if let Some(session) = virtual_display.as_mut() {
            session.ensure_alive()?;
        }
        thread::sleep(Duration::from_millis(100));
    }

    stream
        .stop_capture()
        .context("could not stop screen capture")?;
    stop.store(true, Ordering::SeqCst);
    drop(server);
    let stats = store.stats();
    println!(
        "Stopped. Served {} playlists, {} init segments, and {} media segments.",
        stats.playlists, stats.init_segments, stats.media_segments
    );
    if let Some(session) = virtual_display.as_mut() {
        session.stop()?;
    }
    Ok(())
}

struct LiveFrameHandler {
    pipeline: Mutex<LivePipeline>,
    failure: Arc<Mutex<Option<String>>>,
    repeated_samples: AtomicU64,
    skipped_samples: AtomicU64,
}

impl SCStreamOutputTrait for LiveFrameHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, output_type: SCStreamOutputType) {
        if output_type != SCStreamOutputType::Screen {
            return;
        }

        let result = (|| -> Result<()> {
            let status = sample.frame_status();
            let presentation_time = sample.output_presentation_timestamp();
            let (surface, missing_reason) = match sample.image_buffer() {
                Some(pixel_buffer) => match pixel_buffer.io_surface() {
                    Some(surface) => (Some(surface), None),
                    None => (None, Some("pixel buffer is not IOSurface-backed")),
                },
                None if status.is_some_and(|status| !status.has_content()) => {
                    (None, Some("frame has no new screen content"))
                }
                None => (None, Some("sample has no pixel buffer")),
            };
            let source = self
                .pipeline
                .lock()
                .map_err(|_| anyhow!("live pipeline lock was poisoned"))?
                .encode_sample(surface, presentation_time)?;
            match source {
                FrameSource::Fresh => {}
                FrameSource::Repeated => self.record_repeated_sample(
                    status,
                    missing_reason.unwrap_or("frame reused the previous surface"),
                ),
                FrameSource::Unavailable => self.record_skipped_sample(
                    status,
                    missing_reason.unwrap_or("no reusable screen surface is available"),
                ),
            };
            Ok(())
        })();

        if let Err(error) = result
            && let Ok(mut failure) = self.failure.lock()
            && failure.is_none()
        {
            *failure = Some(format!("{error:#}"));
        }
    }
}

impl LiveFrameHandler {
    fn record_repeated_sample(
        &self,
        status: Option<screencapturekit::SCFrameStatus>,
        reason: &str,
    ) {
        let count = self.repeated_samples.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 || count % 120 == 0 {
            log::debug!(
                "encoded {count} idle ScreenCaptureKit samples by reusing the last frame (latest status={status:?}: {reason})"
            );
        }
    }

    fn record_skipped_sample(&self, status: Option<screencapturekit::SCFrameStatus>, reason: &str) {
        let count = self.skipped_samples.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 || count % 120 == 0 {
            log::debug!(
                "skipped {count} non-video ScreenCaptureKit samples (latest status={status:?}: {reason})"
            );
        }
    }
}

struct LivePipeline {
    encoder: CompressionSession,
    muxer: Option<FragmentedMuxer>,
    store: Arc<HlsStore>,
    last_surface: Option<IOSurface>,
    frame_index: u64,
    frames_in_segment: u32,
    first_capture_timestamp: Option<u64>,
    last_timestamp: Option<u64>,
    segment_start_timestamp: Option<u64>,
    fps: u32,
    width: u32,
    height: u32,
    bitrate: u32,
}

enum FrameSource {
    Fresh,
    Repeated,
    Unavailable,
}

impl LivePipeline {
    fn encode_sample(
        &mut self,
        surface: Option<IOSurface>,
        presentation_time: CMTime,
    ) -> Result<FrameSource> {
        if let Some(surface) = surface {
            self.encode(&surface, presentation_time)?;
            self.last_surface = Some(surface);
            return Ok(FrameSource::Fresh);
        }

        let Some(surface) = self.last_surface.clone() else {
            return Ok(FrameSource::Unavailable);
        };
        self.encode(&surface, presentation_time)?;
        Ok(FrameSource::Repeated)
    }

    fn encode(&mut self, surface: &IOSurface, presentation_time: CMTime) -> Result<()> {
        let timestamp = self.normalized_timestamp(presentation_time);
        let encoder_time = (
            i64::try_from(timestamp).context("live HLS timestamp exceeded VideoToolbox's range")?,
            TIMESCALE as i32,
        );
        let encoded = self
            .encoder
            .encode(surface, encoder_time)
            .context("VideoToolbox could not encode a frame")?;
        if encoded.data.is_empty() {
            return Ok(());
        }

        let keyframe = avcc_contains_nal_type(&encoded.data, 5)?;
        if self.muxer.is_none() {
            if !keyframe {
                return Ok(());
            }
            let (sps, pps) = h264_parameter_sets(encoded.cm_sample_buffer_ptr().cast())?;
            let codec = avc_codec_string(&sps).unwrap_or_else(|| "avc1.42001F".into());
            log::debug!(
                "initializing HLS muxer from H.264 SPS={} bytes, PPS={} bytes, codec={}",
                sps.len(),
                pps.len(),
                codec
            );
            let mut muxer = MuxerBuilder::new(Vec::<u8>::new())
                .video(VideoCodec::H264, self.width, self.height, self.fps as f64)
                .with_sps(sps)
                .with_pps(pps)
                .new_with_fragment()
                .context("could not initialize fragmented MP4 muxer")?;
            self.store.set_init(
                muxer.init_segment(),
                &codec,
                self.width,
                self.height,
                self.fps,
                self.bitrate,
            )?;
            self.muxer = Some(muxer);
            self.segment_start_timestamp = Some(timestamp);
        }

        let muxer = self
            .muxer
            .as_mut()
            .ok_or_else(|| anyhow!("HLS muxer did not initialize on a keyframe"))?;
        if keyframe && self.frames_in_segment > 0 {
            if let Some(segment) = muxer.flush_segment() {
                let start = self.segment_start_timestamp.unwrap_or(timestamp);
                let duration = timestamp.saturating_sub(start) as f64 / TIMESCALE as f64;
                self.store.push_segment(segment, duration.max(0.001))?;
            }
            self.frames_in_segment = 0;
            self.segment_start_timestamp = Some(timestamp);
        }

        muxer
            .write_video(timestamp, timestamp, &encoded.data, keyframe)
            .context("could not add H.264 frame to fragmented MP4")?;
        self.frame_index += 1;
        self.frames_in_segment += 1;
        if self.frame_index % self.fps as u64 == 0 {
            log::trace!(
                "encoded frame {}: {} bytes, keyframe={keyframe}",
                self.frame_index,
                encoded.data.len()
            );
        }
        Ok(())
    }

    fn normalized_timestamp(&mut self, presentation_time: CMTime) -> u64 {
        let fallback_step = TIMESCALE / self.fps as u64;
        let absolute = cm_time_to_ticks(presentation_time);
        let base = *self
            .first_capture_timestamp
            .get_or_insert_with(|| absolute.unwrap_or(0));
        let candidate = absolute
            .map(|value| value.saturating_sub(base))
            .unwrap_or_else(|| {
                self.last_timestamp
                    .map_or(0, |last| last.saturating_add(fallback_step))
            });
        let timestamp = advance_timestamp(self.last_timestamp, candidate, fallback_step);
        self.last_timestamp = Some(timestamp);
        timestamp
    }
}

fn advance_timestamp(last: Option<u64>, candidate: u64, fallback_step: u64) -> u64 {
    match last {
        Some(last) if candidate <= last => last.saturating_add(fallback_step),
        _ => candidate,
    }
}

const fn live_keyframe_interval(fps: u32) -> i32 {
    let interval = fps.saturating_add(1) / 2;
    if interval == 0 { 1 } else { interval as i32 }
}

#[derive(Default)]
struct HlsStore {
    state: Mutex<HlsState>,
    ready: Condvar,
    playlist_requests: AtomicU64,
    init_requests: AtomicU64,
    segment_requests: AtomicU64,
}

#[derive(Default)]
struct HlsState {
    master: Option<Arc<Vec<u8>>>,
    init: Option<Arc<Vec<u8>>>,
    segments: VecDeque<Segment>,
    next_sequence: u64,
}

struct Segment {
    sequence: u64,
    duration: f64,
    data: Arc<Vec<u8>>,
}

impl HlsStore {
    fn set_init(
        &self,
        data: Vec<u8>,
        codec: &str,
        width: u32,
        height: u32,
        fps: u32,
        bandwidth: u32,
    ) -> Result<()> {
        let master = master_playlist(codec, width, height, fps, bandwidth).into_bytes();
        log::debug!(
            "published HLS master playlist (codec={codec}, resolution={width}x{height}, fps={fps}, bandwidth={bandwidth}) and {}-byte initialization segment",
            data.len()
        );
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("HLS store lock was poisoned"))?;
        state.master = Some(Arc::new(master));
        state.init = Some(Arc::new(data));
        Ok(())
    }

    fn push_segment(&self, data: Vec<u8>, duration: f64) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("HLS store lock was poisoned"))?;
        let sequence = state.next_sequence;
        let size = data.len();
        state.next_sequence += 1;
        state.segments.push_back(Segment {
            sequence,
            duration,
            data: Arc::new(data),
        });
        log::debug!(
            "published HLS media segment {sequence}: {duration:.3}s, {} bytes",
            size
        );
        while state.segments.len() > RETAINED_SEGMENTS {
            state.segments.pop_front();
        }
        self.ready.notify_all();
        Ok(())
    }

    fn wait_until_ready(&self, timeout: Duration, segment_count: usize) -> bool {
        let Ok(state) = self.state.lock() else {
            return false;
        };
        let Ok((state, _)) = self
            .ready
            .wait_timeout_while(state, timeout, |state| state.segments.len() < segment_count)
        else {
            return false;
        };
        state.segments.len() >= segment_count
    }

    fn response(&self, path: &str) -> Result<Option<HttpBody>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("HLS store lock was poisoned"))?;
        match path {
            "/" => Ok(Some(HttpBody::text("cast is running\n"))),
            "/master.m3u8" => {
                self.playlist_requests.fetch_add(1, Ordering::Relaxed);
                Ok(state.master.as_ref().map(|data| HttpBody {
                    content_type: "application/vnd.apple.mpegurl",
                    data: Arc::clone(data),
                }))
            }
            "/live.m3u8" => {
                self.playlist_requests.fetch_add(1, Ordering::Relaxed);
                Ok(Some(HttpBody {
                    content_type: "application/vnd.apple.mpegurl",
                    data: Arc::new(playlist(&state).into_bytes()),
                }))
            }
            "/init.mp4" => {
                self.init_requests.fetch_add(1, Ordering::Relaxed);
                Ok(state.init.as_ref().map(|data| HttpBody {
                    content_type: "video/mp4",
                    data: Arc::clone(data),
                }))
            }
            _ => {
                let sequence = path
                    .strip_prefix("/segment-")
                    .and_then(|path| path.strip_suffix(".m4s"))
                    .and_then(|value| value.parse::<u64>().ok());
                let response = sequence.and_then(|sequence| {
                    state
                        .segments
                        .iter()
                        .find(|segment| segment.sequence == sequence)
                        .map(|segment| HttpBody {
                            content_type: "video/iso.segment",
                            data: Arc::clone(&segment.data),
                        })
                });
                if response.is_some() {
                    self.segment_requests.fetch_add(1, Ordering::Relaxed);
                }
                Ok(response)
            }
        }
    }

    fn stats(&self) -> HttpStats {
        HttpStats {
            playlists: self.playlist_requests.load(Ordering::Relaxed),
            init_segments: self.init_requests.load(Ordering::Relaxed),
            media_segments: self.segment_requests.load(Ordering::Relaxed),
        }
    }
}

struct HttpStats {
    playlists: u64,
    init_segments: u64,
    media_segments: u64,
}

fn master_playlist(codec: &str, width: u32, height: u32, fps: u32, bandwidth: u32) -> String {
    let frame_rate = f64::from(fps);
    format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},CODECS=\"{codec}\",RESOLUTION={width}x{height},FRAME-RATE={frame_rate:.3}\nlive.m3u8\n"
    )
}

fn playlist(state: &HlsState) -> String {
    let visible_start = state
        .segments
        .len()
        .saturating_sub(PLAYLIST_WINDOW_SEGMENTS);
    let first_sequence = state
        .segments
        .get(visible_start)
        .map_or(state.next_sequence, |segment| segment.sequence);
    let target_duration = state
        .segments
        .iter()
        .skip(visible_start)
        .map(|segment| segment.duration.ceil() as u64)
        .max()
        .unwrap_or(1)
        .max(1);
    let mut output = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target_duration}\n#EXT-X-MEDIA-SEQUENCE:{first_sequence}\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"init.mp4\"\n"
    );
    for segment in state.segments.iter().skip(visible_start) {
        output.push_str(&format!(
            "#EXTINF:{:.3},\nsegment-{}.m4s\n",
            segment.duration, segment.sequence
        ));
    }
    output
}

struct HttpBody {
    content_type: &'static str,
    data: Arc<Vec<u8>>,
}

impl HttpBody {
    fn text(value: &str) -> Self {
        Self {
            content_type: "text/plain; charset=utf-8",
            data: Arc::new(value.as_bytes().to_vec()),
        }
    }
}

struct HttpServer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HttpServer {
    fn start(
        address: SocketAddr,
        store: Arc<HlsStore>,
        stop: Arc<AtomicBool>,
        route: String,
    ) -> Result<Self> {
        let listener = TcpListener::bind(address)
            .with_context(|| format!("could not bind live HTTP server to {address}"))?;
        listener.set_nonblocking(true)?;
        log::debug!("HTTP server listening on {address}");
        let server_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("cast-http".into())
            .spawn(move || {
                while !server_stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Err(error) = stream.set_nonblocking(false) {
                                eprintln!("Could not configure HTTP client socket: {error}");
                                continue;
                            }
                            let store = Arc::clone(&store);
                            let route = route.clone();
                            thread::spawn(move || {
                                if let Err(error) = handle_http(stream, &store, &route) {
                                    eprintln!("HTTP request failed: {error:#}");
                                }
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20));
                        }
                        Err(error) => {
                            eprintln!("HTTP server failed: {error}");
                            break;
                        }
                    }
                }
            })
            .context("could not start live HTTP server thread")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_http(mut stream: TcpStream, store: &HlsStore, route: &str) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut request = [0_u8; 8192];
    let size = stream.read(&mut request)?;
    let first_line = String::from_utf8_lossy(&request[..size])
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or("/").split('?').next().unwrap_or("/");
    let peer = stream.peer_addr().ok();
    if method != "GET" && method != "HEAD" {
        log::debug!("HTTP {method} {path} from {peer:?} -> 405");
        write_http(
            &mut stream,
            "405 Method Not Allowed",
            None,
            method == "HEAD",
        )?;
        return Ok(());
    }

    let route_prefix = format!("/{route}");
    let routed_path = path
        .strip_prefix(&route_prefix)
        .filter(|remainder| remainder.is_empty() || remainder.starts_with('/'));
    let response = match routed_path {
        Some("") => store.response("/")?,
        Some(path) => store.response(path)?,
        None => None,
    };
    match response {
        Some(body) => {
            log::debug!(
                "HTTP {method} {path} from {peer:?} -> 200 {} bytes ({})",
                body.data.len(),
                body.content_type
            );
            write_http(&mut stream, "200 OK", Some(body), method == "HEAD")?;
        }
        None => {
            log::debug!("HTTP {method} {path} from {peer:?} -> 404");
            write_http(&mut stream, "404 Not Found", None, method == "HEAD")?;
        }
    }
    Ok(())
}

fn write_http(
    stream: &mut TcpStream,
    status: &str,
    body: Option<HttpBody>,
    head_only: bool,
) -> Result<()> {
    let content_type = body
        .as_ref()
        .map_or("text/plain; charset=utf-8", |body| body.content_type);
    let length = body.as_ref().map_or(0, |body| body.data.len());
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n"
    )?;
    if !head_only && let Some(body) = body {
        stream.write_all(&body.data)?;
    }
    stream.flush()?;
    Ok(())
}

fn avcc_contains_nal_type(data: &[u8], wanted: u8) -> Result<bool> {
    let mut offset = 0;
    while offset < data.len() {
        if data.len() - offset < 4 {
            bail!("truncated AVCC NAL length");
        }
        let length = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if length == 0 || length > data.len() - offset {
            bail!("invalid AVCC NAL length {length}");
        }
        if data[offset] & 0x1f == wanted {
            return Ok(true);
        }
        offset += length;
    }
    Ok(false)
}

fn avc_codec_string(sps: &[u8]) -> Option<String> {
    (sps.len() >= 4).then(|| format!("avc1.{:02X}{:02X}{:02X}", sps[1], sps[2], sps[3]))
}

fn cm_time_to_ticks(time: CMTime) -> Option<u64> {
    if !time.is_valid() || time.timescale <= 0 || time.value < 0 {
        return None;
    }
    let ticks =
        i128::from(time.value).saturating_mul(i128::from(TIMESCALE)) / i128::from(time.timescale);
    u64::try_from(ticks).ok()
}

fn h264_parameter_sets(sample: *mut c_void) -> Result<(Vec<u8>, Vec<u8>)> {
    if sample.is_null() {
        bail!("encoded keyframe had no CoreMedia sample buffer");
    }
    let format = unsafe { CMSampleBufferGetFormatDescription(sample) };
    if format.is_null() {
        bail!("encoded keyframe had no H.264 format description");
    }

    let read = |index| -> Result<Vec<u8>> {
        let mut pointer = std::ptr::null();
        let mut size = 0_usize;
        let mut count = 0_usize;
        let mut header_length = 0_i32;
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format,
                index,
                &mut pointer,
                &mut size,
                &mut count,
                &mut header_length,
            )
        };
        if status != 0 || pointer.is_null() || size == 0 {
            bail!("CoreMedia could not read H.264 parameter set {index} (status {status})");
        }
        Ok(unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec())
    };

    Ok((read(0)?, read(1)?))
}

fn take_failure(failure: &Mutex<Option<String>>) -> Result<Option<String>> {
    Ok(failure
        .lock()
        .map_err(|_| anyhow!("encoder failure lock was poisoned"))?
        .take())
}

const fn even(value: u32) -> u32 {
    value - value % 2
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetFormatDescription(sample_buffer: *mut c_void) -> *mut c_void;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        video_description: *mut c_void,
        parameter_set_index: usize,
        parameter_set_pointer: *mut *const u8,
        parameter_set_size: *mut usize,
        parameter_set_count: *mut usize,
        nal_unit_header_length: *mut i32,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::{
        HlsState, Segment, advance_timestamp, avc_codec_string, avcc_contains_nal_type,
        cm_time_to_ticks, live_keyframe_interval, master_playlist, playlist,
    };
    use screencapturekit::CMTime;
    use std::{collections::VecDeque, sync::Arc};

    #[test]
    fn detects_idr_in_avcc() {
        let data = [0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 1, 0x41];
        assert!(avcc_contains_nal_type(&data, 5).unwrap());
        assert!(!avcc_contains_nal_type(&data, 7).unwrap());
    }

    #[test]
    fn rejects_truncated_avcc() {
        assert!(avcc_contains_nal_type(&[0, 0, 0, 4, 0x65], 5).is_err());
    }

    #[test]
    fn builds_codec_string_from_sps() {
        assert_eq!(
            avc_codec_string(&[0x67, 0x42, 0xe0, 0x1f]),
            Some("avc1.42E01F".into())
        );
        assert_eq!(avc_codec_string(&[0x67, 0x42]), None);
    }

    #[test]
    fn converts_core_media_time_to_video_ticks() {
        assert_eq!(cm_time_to_ticks(CMTime::new(3, 2)), Some(135_000));
        assert_eq!(cm_time_to_ticks(CMTime::INVALID), None);
    }

    #[test]
    fn advances_a_stalled_capture_timestamp_by_one_frame() {
        assert_eq!(advance_timestamp(Some(90_000), 90_000, 3_000), 93_000);
        assert_eq!(advance_timestamp(Some(90_000), 90_001, 3_000), 90_001);
    }

    #[test]
    fn selects_a_half_second_live_keyframe_interval() {
        assert_eq!(live_keyframe_interval(30), 15);
        assert_eq!(live_keyframe_interval(60), 30);
        assert_eq!(live_keyframe_interval(1), 1);
    }

    #[test]
    fn playlist_references_available_segments() {
        let state = HlsState {
            master: None,
            init: Some(Arc::new(vec![1])),
            segments: VecDeque::from([Segment {
                sequence: 7,
                duration: 1.0,
                data: Arc::new(vec![2]),
            }]),
            next_sequence: 8,
        };
        let value = playlist(&state);
        assert!(value.contains("#EXT-X-MEDIA-SEQUENCE:7"));
        assert!(value.contains("#EXTINF:1.000,"));
        assert!(value.contains("segment-7.m4s"));
    }

    #[test]
    fn playlist_advertises_only_the_latest_live_window() {
        let segments = (0..10)
            .map(|sequence| Segment {
                sequence,
                duration: 0.5,
                data: Arc::new(vec![sequence as u8]),
            })
            .collect();
        let state = HlsState {
            master: None,
            init: Some(Arc::new(vec![1])),
            segments,
            next_sequence: 10,
        };

        let value = playlist(&state);
        assert!(value.contains("#EXT-X-MEDIA-SEQUENCE:2"));
        assert!(!value.contains("segment-1.m4s"));
        assert!(value.contains("segment-2.m4s"));
        assert!(value.contains("segment-9.m4s"));
    }

    #[test]
    fn master_playlist_describes_the_video_variant() {
        let value = master_playlist("avc1.42001F", 1280, 720, 30, 6_000_000);
        assert!(value.contains("BANDWIDTH=6000000"));
        assert!(value.contains("CODECS=\"avc1.42001F\""));
        assert!(value.contains("RESOLUTION=1280x720"));
        assert!(value.contains("FRAME-RATE=30.000"));
        assert!(value.ends_with("live.m3u8\n"));
    }
}
