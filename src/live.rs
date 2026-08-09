use std::{
    collections::{HashMap, HashSet, VecDeque},
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
    audio::{self, AudioFrameHandler, AudioSubmitter, AudioWorker, EncodedAudioFrame, MediaClock},
    cast,
    network::{http_url, local_ip_for, private_route},
    virtual_display::VirtualDisplaySession,
};

const TIMESCALE: u64 = 90_000;
const PLAYLIST_WINDOW_SEGMENTS: usize = 8;
const RETAINED_SEGMENTS: usize = 32;
const STARTUP_SEGMENTS: usize = 1;

#[derive(Clone)]
pub struct LiveOptions {
    pub cast_hosts: Vec<IpAddr>,
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
    pub audio: bool,
}

pub fn cast_desktop(options: LiveOptions) -> Result<()> {
    if options.bitrate <= 0 {
        bail!("bitrate must be greater than zero");
    }
    if options.extend && options.display_id.is_some() {
        bail!("--extend cannot be combined with --display");
    }
    validate_cast_hosts(&options.cast_hosts)?;
    validate_serve_only(&options.cast_hosts, options.serve_only)?;
    let width = even(options.width);
    let height = even(options.height);
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&interrupted);
    ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
        .context("could not install Ctrl-C handler")?;

    let mut virtual_displays = Vec::new();
    if options.extend {
        for (index, host) in options.cast_hosts.iter().copied().enumerate() {
            let ordinal = u32::try_from(index + 1)
                .context("the number of extended displays exceeded the supported ordinal range")?;
            let session = VirtualDisplaySession::start(width, height, options.fps, ordinal)?;
            println!(
                "Mapped receiver {host} to temporary extended display {ordinal} (display {}).",
                session.display_id()
            );
            virtual_displays.push((host, session));
        }
    }

    let shared_store = (!options.extend).then(|| Arc::new(HlsStore::new(options.audio)));
    let mut targets = Vec::with_capacity(options.cast_hosts.len());
    let mut private_routes = HashSet::with_capacity(options.cast_hosts.len());
    for host in options.cast_hosts.iter().copied() {
        let lan_ip = local_ip_for(host, options.cast_port)?;
        let serve_ip = if options.serve_only {
            loopback_for(lan_ip)
        } else {
            lan_ip
        };
        let store = shared_store
            .as_ref()
            .map(Arc::clone)
            .unwrap_or_else(|| Arc::new(HlsStore::new(options.audio)));
        let route = loop {
            let candidate = private_route()?;
            if private_routes.insert(candidate.clone()) {
                break candidate;
            }
        };
        targets.push(HlsTarget {
            host,
            serve_ip,
            route,
            store,
            audio: Arc::new(AtomicBool::new(options.audio)),
            url: String::new(),
        });
    }

    let stop = Arc::new(AtomicBool::new(false));
    let failure = Arc::new(Mutex::new(None));
    let mut interface_order = Vec::new();
    let mut interface_routes: HashMap<IpAddr, HashMap<String, HlsRoute>> = HashMap::new();
    for target in &targets {
        if !interface_routes.contains_key(&target.serve_ip) {
            interface_order.push(target.serve_ip);
        }
        interface_routes.entry(target.serve_ip).or_default().insert(
            target.route.clone(),
            HlsRoute {
                store: Arc::clone(&target.store),
                audio: Arc::clone(&target.audio),
            },
        );
    }
    let mut servers = HashMap::new();
    for serve_ip in interface_order {
        let routes = interface_routes
            .remove(&serve_ip)
            .expect("interface route table was created");
        let server = HttpServer::start(
            SocketAddr::new(serve_ip, options.http_port),
            Arc::new(routes),
            Arc::clone(&stop),
            Arc::clone(&failure),
        )?;
        log::debug!(
            "HLS HTTP server for interface {serve_ip} is listening on {} with {} private routes",
            server.local_addr(),
            server.route_count()
        );
        servers.insert(serve_ip, server);
    }
    for target in &mut targets {
        let address = servers
            .get(&target.serve_ip)
            .expect("target HTTP interface was started")
            .local_addr();
        target.url = http_url(address, &format!("/{}/master.m3u8", target.route));
    }

    let mut captures = Vec::new();
    if options.extend {
        for (target, (_, session)) in targets.iter().zip(virtual_displays.iter()) {
            captures.push(LiveCapture::start(
                Some(session.display_id()),
                &options,
                Arc::clone(&target.store),
                Arc::clone(&failure),
            )?);
        }
    } else {
        captures.push(LiveCapture::start(
            options.display_id,
            &options,
            Arc::clone(shared_store.as_ref().expect("shared HLS store exists")),
            Arc::clone(&failure),
        )?);
    }

    let startup_started = Instant::now();
    loop {
        if captures.iter().all(LiveCapture::is_ready) {
            break;
        }
        if let Some(error) = take_failure(&failure)? {
            bail!("live HLS startup failed: {error}");
        }
        for (_, session) in &mut virtual_displays {
            session.ensure_alive()?;
        }
        if startup_started.elapsed() >= Duration::from_secs(12) {
            bail!("timed out waiting for {STARTUP_SEGMENTS} HLS media segments");
        }
        thread::sleep(Duration::from_millis(100));
    }
    for target in &targets {
        println!(
            "Live stream for receiver {} ready at {}",
            target.host, target.url
        );
    }

    let mut receiver_sessions = if options.serve_only {
        Vec::new()
    } else {
        start_hls_receivers(&targets, options.cast_port)?
    };
    if options.serve_only {
        println!("Serving without contacting the Cast receiver. Press Ctrl-C to stop.");
    } else {
        println!(
            "Casting desktop to {} receivers over HLS. Press Ctrl-C to stop.",
            receiver_sessions.len()
        );
    }

    let started = Instant::now();
    let mut run_result = (|| -> Result<()> {
        loop {
            if interrupted.load(Ordering::SeqCst)
                || options
                    .duration
                    .is_some_and(|duration| started.elapsed() >= duration)
            {
                break;
            }
            if let Some(error) = take_failure(&failure)? {
                bail!("live HLS group failed: {error}");
            }
            for (_, session) in &mut virtual_displays {
                session.ensure_alive()?;
            }
            for (host, session) in &receiver_sessions {
                session
                    .ensure_alive()
                    .with_context(|| format!("HLS receiver {host} failed"))?;
            }
            thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    })();

    let mut capture_result = Ok(());
    for capture in &mut captures {
        if let Err(error) = capture.stop_and_release()
            && capture_result.is_ok()
        {
            capture_result = Err(error);
        }
    }
    log::debug!("released desktop capture resources before display teardown");
    let mut receiver_result = Ok(());
    for (_, session) in &mut receiver_sessions {
        if let Err(error) = session.stop()
            && receiver_result.is_ok()
        {
            receiver_result = Err(error);
        }
    }
    stop.store(true, Ordering::SeqCst);
    drop(servers);
    if run_result.is_ok() {
        run_result = match take_failure(&failure) {
            Ok(Some(error)) => Err(anyhow!("live HLS group failed: {error}")),
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        };
    }
    print_hls_stats(&targets);

    let mut display_result = Ok(());
    for (_, session) in virtual_displays.iter_mut().rev() {
        if let Err(error) = session.stop()
            && display_result.is_ok()
        {
            display_result = Err(error);
        }
    }
    run_result?;
    capture_result?;
    receiver_result?;
    display_result
}

struct HlsTarget {
    host: IpAddr,
    serve_ip: IpAddr,
    route: String,
    store: Arc<HlsStore>,
    audio: Arc<AtomicBool>,
    url: String,
}

#[derive(Clone)]
struct HlsRoute {
    store: Arc<HlsStore>,
    audio: Arc<AtomicBool>,
}

impl HlsRoute {
    fn response(&self, path: &str) -> Result<Option<HttpBody>> {
        self.store.response(path, self.audio.load(Ordering::SeqCst))
    }
}

fn validate_cast_hosts(hosts: &[IpAddr]) -> Result<()> {
    if hosts.is_empty() {
        bail!("at least one --host is required");
    }
    let mut unique = HashSet::with_capacity(hosts.len());
    for host in hosts {
        if !unique.insert(*host) {
            bail!("duplicate Cast receiver host {host}");
        }
    }
    Ok(())
}

fn validate_serve_only(hosts: &[IpAddr], serve_only: bool) -> Result<()> {
    if serve_only && hosts.len() != 1 {
        bail!("--serve-only requires exactly one --host");
    }
    Ok(())
}

fn loopback_for(address: IpAddr) -> IpAddr {
    if address.is_ipv4() {
        IpAddr::from([127, 0, 0, 1])
    } else {
        IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1])
    }
}

fn start_hls_receivers(
    targets: &[HlsTarget],
    cast_port: u16,
) -> Result<Vec<(IpAddr, cast::LiveMediaSession)>> {
    let results = thread::scope(|scope| {
        let mut workers = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let host = target.host;
            let url = target.url.clone();
            let audio = Arc::clone(&target.audio);
            let store = Arc::clone(&target.store);
            workers.push((
                index,
                host,
                scope.spawn(move || {
                    let with_audio = audio.load(Ordering::SeqCst) && store.audio_enabled();
                    let first = if with_audio {
                        cast::LiveMediaSession::start_fmp4_hls_with_aac(
                            host,
                            cast_port,
                            url.clone(),
                        )
                    } else {
                        cast::LiveMediaSession::start_fmp4_hls(host, cast_port, url.clone())
                    };
                    match first {
                        Err(error) if with_audio => {
                            eprintln!(
                                "Receiver {host} did not accept desktop audio; retrying video only: {error:#}"
                            );
                            audio.store(false, Ordering::SeqCst);
                            cast::LiveMediaSession::start_fmp4_hls(host, cast_port, url)
                        }
                        result => result,
                    }
                }),
            ));
        }
        workers
            .into_iter()
            .map(|(index, host, worker)| {
                let result = worker
                    .join()
                    .map_err(|_| anyhow!("HLS Cast control worker for {host} panicked"))?;
                Ok((index, host, result))
            })
            .collect::<Result<Vec<_>>>()
    })?;

    let mut slots: Vec<Option<(IpAddr, cast::LiveMediaSession)>> =
        (0..targets.len()).map(|_| None).collect();
    let mut errors = Vec::new();
    for (index, host, result) in results {
        match result {
            Ok(session) => slots[index] = Some((host, session)),
            Err(error) => errors.push(format!("{host}: {error:#}")),
        }
    }
    let mut sessions = slots.into_iter().flatten().collect::<Vec<_>>();
    if !errors.is_empty() {
        for (_, session) in &mut sessions {
            if let Err(error) = session.stop() {
                log::warn!("could not stop HLS receiver after partial startup: {error:#}");
            }
        }
        bail!("could not start every HLS receiver: {}", errors.join("; "));
    }
    Ok(sessions)
}

fn print_hls_stats(targets: &[HlsTarget]) {
    let mut reported = HashSet::new();
    for target in targets {
        let identity = Arc::as_ptr(&target.store) as usize;
        if !reported.insert(identity) {
            continue;
        }
        let receivers = targets
            .iter()
            .filter(|candidate| Arc::ptr_eq(&candidate.store, &target.store))
            .map(|candidate| candidate.host.to_string())
            .collect::<Vec<_>>();
        let stats = target.store.stats();
        println!(
            "HLS source for {} served {} playlists, {} init segments, and {} media segments.",
            receivers.join(", "),
            stats.playlists,
            stats.init_segments,
            stats.media_segments
        );
    }
}

struct LiveCapture {
    stream: Option<SCStream>,
    store: Arc<HlsStore>,
    audio_worker: Option<AudioWorker>,
}

impl LiveCapture {
    fn start(
        display_id: Option<u32>,
        options: &LiveOptions,
        store: Arc<HlsStore>,
        failure: Arc<Mutex<Option<String>>>,
    ) -> Result<Self> {
        let width = even(options.width);
        let height = even(options.height);
        let content = SCShareableContent::get().context(
            "could not enumerate displays; grant Screen Recording permission in System Settings",
        )?;
        let displays = content.displays();
        let display = match display_id {
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
        let encoder = CompressionSession::builder(width as i32, height as i32, Codec::H264)
            .with_real_time(true)
            .with_allow_frame_reordering(false)
            .with_average_bit_rate(options.bitrate)
            .with_expected_frame_rate(options.fps as f64)
            .with_max_keyframe_interval(keyframe_interval)
            .with_profile_level(ProfileLevel::H264Baseline3_1)
            .build()
            .context("could not create the VideoToolbox H.264 encoder")?;
        let clock = Arc::new(MediaClock::default());
        let (audio_submitter, audio_worker) = if options.audio && store.audio_enabled() {
            let audio_store = Arc::clone(&store);
            match AudioWorker::start(Arc::clone(&clock), Arc::clone(&failure), move |frame| {
                audio_store.push_audio(frame)
            }) {
                Ok((submitter, worker)) => (Some(submitter), Some(worker)),
                Err(error) => {
                    eprintln!("System audio is unavailable; continuing with video only: {error:#}");
                    store.disable_audio()?;
                    (None, None)
                }
            }
        } else {
            (None, None)
        };
        let pipeline = LivePipeline {
            encoder,
            muxer: None,
            store: Arc::clone(&store),
            last_surface: None,
            frame_index: 0,
            frames_in_segment: 0,
            clock,
            audio_submitter: audio_submitter.clone(),
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
        let config = if audio_submitter.is_some() {
            config
                .with_captures_audio(true)
                .with_sample_rate(audio::SAMPLE_RATE as i32)
                .with_channel_count(audio::CHANNELS as i32)
                .with_excludes_current_process_audio(true)
        } else {
            config
        };
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
        if let Some(audio_submitter) = audio_submitter {
            stream.add_output_handler(
                AudioFrameHandler::new(audio_submitter),
                SCStreamOutputType::Audio,
            );
        }
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
        Ok(Self {
            stream: Some(stream),
            store,
            audio_worker,
        })
    }

    fn is_ready(&self) -> bool {
        self.store
            .wait_until_ready(Duration::ZERO, STARTUP_SEGMENTS)
    }

    fn stop_and_release(&mut self) -> Result<()> {
        let Some(stream) = self.stream.take() else {
            return Ok(());
        };
        stream
            .stop_capture()
            .context("could not stop screen capture")?;
        drop(stream);
        if let Some(worker) = self.audio_worker.take() {
            worker.stop()?;
        }
        Ok(())
    }
}

impl Drop for LiveCapture {
    fn drop(&mut self) {
        if let Err(error) = self.stop_and_release() {
            log::warn!("could not stop live desktop capture: {error:#}");
        }
    }
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
        if count == 1 || count.is_multiple_of(120) {
            log::debug!(
                "encoded {count} idle ScreenCaptureKit samples by reusing the last frame (latest status={status:?}: {reason})"
            );
        }
    }

    fn record_skipped_sample(&self, status: Option<screencapturekit::SCFrameStatus>, reason: &str) {
        let count = self.skipped_samples.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 || count.is_multiple_of(120) {
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
    clock: Arc<MediaClock>,
    audio_submitter: Option<AudioSubmitter>,
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
                if let Some(audio) = &self.audio_submitter {
                    audio.advance_to(video_to_audio_timestamp(timestamp));
                }
                self.store
                    .push_segment(segment, duration.max(0.001), start, timestamp)?;
            }
            self.frames_in_segment = 0;
            self.segment_start_timestamp = Some(timestamp);
        }

        muxer
            .write_video(timestamp, timestamp, &encoded.data, keyframe)
            .context("could not add H.264 frame to fragmented MP4")?;
        self.frame_index += 1;
        self.frames_in_segment += 1;
        if self.frame_index.is_multiple_of(self.fps as u64) {
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
        let candidate = self
            .clock
            .ticks(presentation_time, TIMESCALE)
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
    metadata: Option<HlsMetadata>,
    init: Option<Arc<Vec<u8>>>,
    segments: VecDeque<Segment>,
    pending_segments: VecDeque<PendingSegment>,
    audio_frames: VecDeque<EncodedAudioFrame>,
    audio_watermark: u64,
    audio_enabled: bool,
    next_sequence: u64,
}

struct HlsMetadata {
    codec: String,
    width: u32,
    height: u32,
    fps: u32,
    bandwidth: u32,
}

struct Segment {
    sequence: u64,
    duration: f64,
    video: Arc<Vec<u8>>,
    audio: Option<Arc<Vec<u8>>>,
}

struct PendingSegment {
    duration: f64,
    start: u64,
    end: u64,
    video: Vec<u8>,
}

impl HlsStore {
    fn new(audio_enabled: bool) -> Self {
        Self {
            state: Mutex::new(HlsState {
                audio_enabled,
                ..HlsState::default()
            }),
            ..Self::default()
        }
    }

    fn audio_enabled(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.audio_enabled)
            .unwrap_or(false)
    }

    fn disable_audio(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("HLS store lock was poisoned"))?;
        state.audio_enabled = false;
        publish_ready_segments(&mut state);
        self.ready.notify_all();
        Ok(())
    }

    fn set_init(
        &self,
        data: Vec<u8>,
        codec: &str,
        width: u32,
        height: u32,
        fps: u32,
        bandwidth: u32,
    ) -> Result<()> {
        log::debug!(
            "published HLS metadata (codec={codec}, resolution={width}x{height}, fps={fps}, bandwidth={bandwidth}) and {}-byte initialization segment",
            data.len()
        );
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("HLS store lock was poisoned"))?;
        state.metadata = Some(HlsMetadata {
            codec: codec.to_owned(),
            width,
            height,
            fps,
            bandwidth,
        });
        state.init = Some(Arc::new(data));
        Ok(())
    }

    fn push_segment(&self, data: Vec<u8>, duration: f64, start: u64, end: u64) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("HLS store lock was poisoned"))?;
        state.pending_segments.push_back(PendingSegment {
            duration,
            start,
            end,
            video: data,
        });
        publish_ready_segments(&mut state);
        self.ready.notify_all();
        Ok(())
    }

    fn push_audio(&self, frame: EncodedAudioFrame) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("HLS store lock was poisoned"))?;
        state.audio_watermark = state.audio_watermark.max(
            frame
                .timestamp
                .saturating_add(audio::ACCESS_UNIT_SAMPLES as u64),
        );
        state.audio_frames.push_back(frame);
        publish_ready_segments(&mut state);
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

    fn response(&self, path: &str, advertise_audio: bool) -> Result<Option<HttpBody>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("HLS store lock was poisoned"))?;
        match path {
            "/" => Ok(Some(HttpBody::text("cast is running\n"))),
            "/master.m3u8" => {
                self.playlist_requests.fetch_add(1, Ordering::Relaxed);
                Ok(state.metadata.as_ref().map(|metadata| HttpBody {
                    content_type: "application/vnd.apple.mpegurl",
                    data: Arc::new(
                        master_playlist(
                            &metadata.codec,
                            metadata.width,
                            metadata.height,
                            metadata.fps,
                            metadata.bandwidth,
                            state.audio_enabled && advertise_audio,
                        )
                        .into_bytes(),
                    ),
                }))
            }
            "/live.m3u8" => {
                self.playlist_requests.fetch_add(1, Ordering::Relaxed);
                Ok(Some(HttpBody {
                    content_type: "application/vnd.apple.mpegurl",
                    data: Arc::new(playlist(&state).into_bytes()),
                }))
            }
            "/audio.m3u8" if state.audio_enabled && advertise_audio => {
                self.playlist_requests.fetch_add(1, Ordering::Relaxed);
                Ok(Some(HttpBody {
                    content_type: "application/vnd.apple.mpegurl",
                    data: Arc::new(audio_playlist(&state).into_bytes()),
                }))
            }
            "/init.mp4" => {
                self.init_requests.fetch_add(1, Ordering::Relaxed);
                Ok(state.init.as_ref().map(|data| HttpBody {
                    content_type: "video/mp4",
                    data: Arc::clone(data),
                }))
            }
            path if path.starts_with("/audio-") && state.audio_enabled && advertise_audio => {
                let sequence = path
                    .strip_prefix("/audio-")
                    .and_then(|path| path.strip_suffix(".aac"))
                    .and_then(|value| value.parse::<u64>().ok());
                let response = sequence.and_then(|sequence| {
                    state
                        .segments
                        .iter()
                        .find(|segment| segment.sequence == sequence)
                        .and_then(|segment| segment.audio.as_ref())
                        .map(|data| HttpBody {
                            content_type: "audio/aac",
                            data: Arc::clone(data),
                        })
                });
                if response.is_some() {
                    self.segment_requests.fetch_add(1, Ordering::Relaxed);
                }
                Ok(response)
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
                            data: Arc::clone(&segment.video),
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

fn master_playlist(
    codec: &str,
    width: u32,
    height: u32,
    fps: u32,
    bandwidth: u32,
    with_audio: bool,
) -> String {
    let frame_rate = f64::from(fps);
    if with_audio {
        format!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"System Audio\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH={},CODECS=\"{codec},mp4a.40.2\",RESOLUTION={width}x{height},FRAME-RATE={frame_rate:.3},AUDIO=\"audio\"\nlive.m3u8\n",
            bandwidth.saturating_add(audio::BITRATE)
        )
    } else {
        format!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},CODECS=\"{codec}\",RESOLUTION={width}x{height},FRAME-RATE={frame_rate:.3}\nlive.m3u8\n"
        )
    }
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

fn audio_playlist(state: &HlsState) -> String {
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
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target_duration}\n#EXT-X-MEDIA-SEQUENCE:{first_sequence}\n"
    );
    for segment in state.segments.iter().skip(visible_start) {
        output.push_str(&format!(
            "#EXTINF:{:.3},\naudio-{}.aac\n",
            segment.duration, segment.sequence
        ));
    }
    output
}

fn publish_ready_segments(state: &mut HlsState) {
    while state.pending_segments.front().is_some_and(|segment| {
        !state.audio_enabled || state.audio_watermark >= video_to_audio_timestamp(segment.end)
    }) {
        let pending = state.pending_segments.pop_front().unwrap();
        let audio = state.audio_enabled.then(|| {
            let start = video_to_audio_timestamp(pending.start);
            let end = video_to_audio_timestamp(pending.end);
            while state.audio_frames.front().is_some_and(|frame| {
                frame
                    .timestamp
                    .saturating_add(audio::ACCESS_UNIT_SAMPLES as u64)
                    <= start
            }) {
                state.audio_frames.pop_front();
            }
            let timestamp = state
                .audio_frames
                .front()
                .map_or(start, |frame| frame.timestamp);
            let mut data = id3_timestamp(video_timestamp(timestamp));
            while state
                .audio_frames
                .front()
                .is_some_and(|frame| frame.timestamp < end)
            {
                data.extend_from_slice(&state.audio_frames.pop_front().unwrap().data);
            }
            Arc::new(data)
        });
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.saturating_add(1);
        let video_size = pending.video.len();
        let audio_size = audio.as_ref().map_or(0, |audio| audio.len());
        state.segments.push_back(Segment {
            sequence,
            duration: pending.duration,
            video: Arc::new(pending.video),
            audio,
        });
        log::debug!(
            "published HLS media segment {sequence}: {:.3}s, {video_size} video bytes, {audio_size} audio bytes",
            pending.duration
        );
        while state.segments.len() > RETAINED_SEGMENTS {
            state.segments.pop_front();
        }
    }
}

fn video_to_audio_timestamp(timestamp: u64) -> u64 {
    timestamp.saturating_mul(u64::from(audio::SAMPLE_RATE)) / TIMESCALE
}

fn video_timestamp(timestamp: u64) -> u64 {
    timestamp.saturating_mul(TIMESCALE) / u64::from(audio::SAMPLE_RATE)
}

fn id3_timestamp(timestamp: u64) -> Vec<u8> {
    const OWNER: &[u8] = b"com.apple.streaming.transportStreamTimestamp";
    let timestamp = timestamp & ((1_u64 << 33) - 1);
    let mut private = Vec::with_capacity(OWNER.len() + 9);
    private.extend_from_slice(OWNER);
    private.push(0);
    private.extend_from_slice(&timestamp.to_be_bytes());

    let mut frame = Vec::with_capacity(10 + private.len());
    frame.extend_from_slice(b"PRIV");
    frame.extend_from_slice(&synchsafe(private.len()));
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&private);

    let mut tag = Vec::with_capacity(10 + frame.len());
    tag.extend_from_slice(b"ID3\x04\0\0");
    tag.extend_from_slice(&synchsafe(frame.len()));
    tag.extend_from_slice(&frame);
    tag
}

fn synchsafe(value: usize) -> [u8; 4] {
    let value = value.min(0x0fff_ffff) as u32;
    [
        ((value >> 21) & 0x7f) as u8,
        ((value >> 14) & 0x7f) as u8,
        ((value >> 7) & 0x7f) as u8,
        (value & 0x7f) as u8,
    ]
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
    local_addr: SocketAddr,
    route_count: usize,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HttpServer {
    fn start(
        address: SocketAddr,
        routes: Arc<HashMap<String, HlsRoute>>,
        stop: Arc<AtomicBool>,
        failure: Arc<Mutex<Option<String>>>,
    ) -> Result<Self> {
        let listener = TcpListener::bind(address)
            .with_context(|| format!("could not bind live HTTP server to {address}"))?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let route_count = routes.len();
        log::debug!("HTTP server listening on {local_addr}");
        let server_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name(format!("cast-http-{}", local_addr.ip()))
            .spawn(move || {
                while !server_stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Err(error) = stream.set_nonblocking(false) {
                                eprintln!("Could not configure HTTP client socket: {error}");
                                continue;
                            }
                            let routes = Arc::clone(&routes);
                            thread::spawn(move || {
                                if let Err(error) = handle_http(stream, &routes) {
                                    eprintln!("HTTP request failed: {error:#}");
                                }
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20));
                        }
                        Err(error) => {
                            store_failure(
                                &failure,
                                format!("HTTP server on {local_addr} failed: {error}"),
                            );
                            server_stop.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            })
            .context("could not start live HTTP server thread")?;
        Ok(Self {
            local_addr,
            route_count,
            stop,
            thread: Some(thread),
        })
    }

    const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    const fn route_count(&self) -> usize {
        self.route_count
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

fn handle_http(mut stream: TcpStream, routes: &HashMap<String, HlsRoute>) -> Result<()> {
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

    let response = routed_response(routes, path)?;
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

fn routed_response(routes: &HashMap<String, HlsRoute>, path: &str) -> Result<Option<HttpBody>> {
    let Some(path) = path.strip_prefix('/') else {
        return Ok(None);
    };
    let (route, remainder) = path.split_once('/').unwrap_or((path, ""));
    let Some(route) = routes.get(route) else {
        return Ok(None);
    };
    let store_path = if remainder.is_empty() {
        "/".to_owned()
    } else {
        format!("/{remainder}")
    };
    route.response(&store_path)
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

fn store_failure(failure: &Mutex<Option<String>>, message: String) {
    if let Ok(mut failure) = failure.lock()
        && failure.is_none()
    {
        *failure = Some(message);
    }
}

const fn even(value: u32) -> u32 {
    value - value % 2
}

#[cfg_attr(target_os = "macos", link(name = "CoreMedia", kind = "framework"))]
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
        HlsRoute, HlsState, HlsStore, Segment, TIMESCALE, advance_timestamp, avc_codec_string,
        avcc_contains_nal_type, id3_timestamp, live_keyframe_interval, master_playlist, playlist,
        routed_response, validate_cast_hosts, validate_serve_only,
    };
    use crate::audio::{self, EncodedAudioFrame};
    use std::{
        collections::HashMap,
        collections::VecDeque,
        net::IpAddr,
        sync::{Arc, atomic::AtomicBool},
    };

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
            init: Some(Arc::new(vec![1])),
            segments: VecDeque::from([Segment {
                sequence: 7,
                duration: 1.0,
                video: Arc::new(vec![2]),
                audio: None,
            }]),
            next_sequence: 8,
            ..HlsState::default()
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
                video: Arc::new(vec![sequence as u8]),
                audio: None,
            })
            .collect();
        let state = HlsState {
            init: Some(Arc::new(vec![1])),
            segments,
            next_sequence: 10,
            ..HlsState::default()
        };

        let value = playlist(&state);
        assert!(value.contains("#EXT-X-MEDIA-SEQUENCE:2"));
        assert!(!value.contains("segment-1.m4s"));
        assert!(value.contains("segment-2.m4s"));
        assert!(value.contains("segment-9.m4s"));
    }

    #[test]
    fn master_playlist_describes_the_video_variant() {
        let value = master_playlist("avc1.42001F", 1280, 720, 30, 6_000_000, false);
        assert!(value.contains("BANDWIDTH=6000000"));
        assert!(value.contains("CODECS=\"avc1.42001F\""));
        assert!(value.contains("RESOLUTION=1280x720"));
        assert!(value.contains("FRAME-RATE=30.000"));
        assert!(value.ends_with("live.m3u8\n"));
    }

    #[test]
    fn multiplexed_routes_are_private_and_isolated() {
        let first = Arc::new(HlsStore::default());
        let second = Arc::new(HlsStore::default());
        first
            .set_init(vec![1, 2, 3], "avc1.42001F", 1280, 720, 30, 6_000_000)
            .unwrap();
        second
            .set_init(vec![4, 5, 6], "avc1.42001F", 1280, 720, 30, 6_000_000)
            .unwrap();
        let routes = HashMap::from([
            (
                "alpha".to_owned(),
                HlsRoute {
                    store: first,
                    audio: Arc::new(AtomicBool::new(false)),
                },
            ),
            (
                "bravo".to_owned(),
                HlsRoute {
                    store: second,
                    audio: Arc::new(AtomicBool::new(false)),
                },
            ),
        ]);

        assert_eq!(
            &*routed_response(&routes, "/alpha/init.mp4")
                .unwrap()
                .unwrap()
                .data,
            &[1, 2, 3]
        );
        assert_eq!(
            &*routed_response(&routes, "/bravo/init.mp4")
                .unwrap()
                .unwrap()
                .data,
            &[4, 5, 6]
        );
        assert!(
            routed_response(&routes, "/alphax/init.mp4")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn audio_master_and_packed_aac_timestamp_are_well_formed() {
        let value = master_playlist("avc1.42001F", 1280, 720, 30, 6_000_000, true);
        assert!(value.contains("#EXT-X-MEDIA:TYPE=AUDIO"));
        assert!(value.contains("CODECS=\"avc1.42001F,mp4a.40.2\""));
        assert!(value.contains("AUDIO=\"audio\""));

        let tag = id3_timestamp(90_000);
        assert_eq!(&tag[..6], b"ID3\x04\0\0");
        assert!(tag.windows(4).any(|window| window == b"PRIV"));
        assert!(
            tag.windows(b"com.apple.streaming.transportStreamTimestamp".len())
                .any(|window| window == b"com.apple.streaming.transportStreamTimestamp")
        );
        assert_eq!(&tag[tag.len() - 8..], &90_000_u64.to_be_bytes());
    }

    #[test]
    fn audio_segments_publish_in_lockstep_with_video() {
        let store = HlsStore::new(true);
        store
            .set_init(vec![1], "avc1.42001F", 1280, 720, 30, 6_000_000)
            .unwrap();
        store.push_segment(vec![2], 1.0, 0, TIMESCALE).unwrap();
        assert_eq!(store.state.lock().unwrap().segments.len(), 0);

        for timestamp in (0..audio::SAMPLE_RATE as u64).step_by(audio::ACCESS_UNIT_SAMPLES) {
            store
                .push_audio(EncodedAudioFrame {
                    timestamp,
                    data: vec![0xff, 0xf1],
                })
                .unwrap();
        }
        let state = store.state.lock().unwrap();
        assert_eq!(state.segments.len(), 1);
        let segment = state.segments.front().unwrap();
        assert_eq!(&*segment.video, &[2]);
        let audio = segment.audio.as_ref().unwrap();
        assert!(audio.starts_with(b"ID3"));
        assert!(audio.ends_with(&[0xff, 0xf1]));
    }

    #[test]
    fn group_validation_rejects_duplicates_and_multi_host_serve_only() {
        let first = "192.0.2.10".parse::<IpAddr>().unwrap();
        let second = "192.0.2.20".parse::<IpAddr>().unwrap();
        assert!(validate_cast_hosts(&[first, second]).is_ok());
        assert!(validate_cast_hosts(&[first, first]).is_err());
        assert!(validate_serve_only(&[first], true).is_ok());
        assert!(validate_serve_only(&[first, second], true).is_err());
        assert!(validate_serve_only(&[first, second], false).is_ok());
    }
}
