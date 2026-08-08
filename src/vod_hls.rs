use std::{
    fs::File,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    media::{self, MediaInfo, TranscodeTracks},
    network::http_url,
};

const MAX_REQUEST_HEADER_BYTES: usize = 16 * 1024;
const TRANSFER_BUFFER_BYTES: usize = 64 * 1024;
const READY_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TRANSCODED_HLS_PEAK_BANDWIDTH: u64 = 6_500_000;
const COPIED_HLS_PEAK_BANDWIDTH: u64 = 25_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HlsServerStats {
    pub requests: u64,
    pub playlists: u64,
    pub init_segments: u64,
    pub media_segments: u64,
    pub bytes_sent: u64,
    pub failures: u64,
}

#[derive(Default)]
struct HlsCounters {
    requests: AtomicU64,
    playlists: AtomicU64,
    init_segments: AtomicU64,
    media_segments: AtomicU64,
    bytes_sent: AtomicU64,
    failures: AtomicU64,
}

impl HlsCounters {
    fn snapshot(&self) -> HlsServerStats {
        HlsServerStats {
            requests: self.requests.load(Ordering::Relaxed),
            playlists: self.playlists.load(Ordering::Relaxed),
            init_segments: self.init_segments.load(Ordering::Relaxed),
            media_segments: self.media_segments.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
        }
    }
}

#[derive(Default)]
struct PreparationStatus {
    progress: f64,
    finished: bool,
    failure: Option<String>,
}

#[derive(Default)]
struct SharedPreparationStatus {
    status: Mutex<PreparationStatus>,
    changed: Condvar,
}

#[derive(Clone, Copy)]
struct HlsRendition {
    bandwidth: u64,
    width: u32,
    height: u32,
    frame_rate: Option<f64>,
    has_audio: bool,
}

pub struct IncrementalHlsPreparation {
    server: HlsDirectoryServer,
    directory: tempfile::TempDir,
    rendition: HlsRendition,
    cancel: Arc<AtomicBool>,
    status: Arc<SharedPreparationStatus>,
    worker: Option<JoinHandle<()>>,
}

impl IncrementalHlsPreparation {
    pub fn start(
        input_path: PathBuf,
        info: MediaInfo,
        tracks: TranscodeTracks,
        bind_address: SocketAddr,
        route: String,
        mut progress_callback: impl FnMut(f64) + Send + 'static,
    ) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("cast-hls-")
            .tempdir()
            .context("could not create an incremental media directory")?;
        let server = HlsDirectoryServer::start(bind_address, directory.path().to_owned(), route)?;
        let playlist_path = directory.path().join("index.m3u8");
        let segment_pattern = directory.path().join("segment-%06d.m4s");
        let (width, height) = if tracks.video {
            media::compatible_dimensions(info.video.width, info.video.height)
        } else {
            (info.video.width, info.video.height)
        };
        let rendition = HlsRendition {
            bandwidth: if tracks.video {
                TRANSCODED_HLS_PEAK_BANDWIDTH
            } else {
                COPIED_HLS_PEAK_BANDWIDTH
            },
            width,
            height,
            frame_rate: info.video.frame_rate,
            has_audio: info.audio.is_some(),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let status = Arc::new(SharedPreparationStatus::default());
        let worker_cancel = Arc::clone(&cancel);
        let worker_status = Arc::clone(&status);
        let worker = thread::Builder::new()
            .name("cast-hls-transcode".into())
            .spawn(move || {
                let result = media::transcode_to_hls_with_tracks(
                    &input_path,
                    &playlist_path,
                    &segment_pattern,
                    &info,
                    tracks,
                    &worker_cancel,
                    |progress| {
                        if let Ok(mut state) = worker_status.status.lock() {
                            state.progress = progress;
                            worker_status.changed.notify_all();
                        }
                        progress_callback(progress);
                    },
                );
                if let Ok(mut state) = worker_status.status.lock() {
                    state.finished = true;
                    if let Err(error) = result {
                        state.failure = Some(format!("{error:#}"));
                    }
                    worker_status.changed.notify_all();
                }
            })
            .context("could not start incremental media preparation")?;

        Ok(Self {
            server,
            directory,
            rendition,
            cancel,
            status,
            worker: Some(worker),
        })
    }

    pub fn url(&self) -> String {
        self.server.url()
    }

    pub fn received_request(&self) -> bool {
        self.server.stats().playlists > 0
    }

    pub fn stats(&self) -> HlsServerStats {
        self.server.stats()
    }

    pub fn progress(&self) -> f64 {
        self.status
            .status
            .lock()
            .map(|state| state.progress)
            .unwrap_or(0.0)
    }

    pub fn failure(&self) -> Option<String> {
        self.status
            .status
            .lock()
            .ok()
            .and_then(|state| state.failure.clone())
    }

    pub fn wait_until_playable(
        &self,
        start_at: f64,
        interrupted: &AtomicBool,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if interrupted.load(Ordering::SeqCst) {
                self.cancel();
                bail!("media preparation was cancelled");
            }
            if let Some(failure) = self.failure() {
                bail!("incremental media preparation failed: {failure}");
            }
            if playlist_is_playable(self.directory.path(), start_at)? {
                publish_master_playlist(self.directory.path(), self.rendition)?;
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for incremental media preparation at {:.0}%",
                    self.progress()
                );
            }

            let state = self
                .status
                .status
                .lock()
                .map_err(|_| anyhow!("incremental preparation status lock was poisoned"))?;
            let _ = self
                .status
                .changed
                .wait_timeout(state, READY_POLL_INTERVAL)
                .map_err(|_| anyhow!("incremental preparation status lock was poisoned"))?;
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn finish(&mut self) -> Result<()> {
        let Some(worker) = self.worker.take() else {
            return self
                .failure()
                .map_or(Ok(()), |failure| Err(anyhow!(failure)));
        };
        worker
            .join()
            .map_err(|_| anyhow!("incremental media preparation thread panicked"))?;
        self.failure()
            .map_or(Ok(()), |failure| Err(anyhow!(failure)))
    }
}

impl Drop for IncrementalHlsPreparation {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            log::warn!("incremental media preparation thread panicked during cleanup");
        }
    }
}

fn playlist_is_playable(directory: &Path, start_at: f64) -> Result<bool> {
    let init = directory.join("init.mp4");
    if !init.metadata().is_ok_and(|metadata| metadata.len() > 0) {
        return Ok(false);
    }
    let playlist = match std::fs::read_to_string(directory.join("index.m3u8")) {
        Ok(playlist) => playlist,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("could not inspect incremental HLS playlist"),
    };

    let mut duration = 0.0_f64;
    let mut segments = 0_usize;
    let mut pending_duration = None;
    for line in playlist.lines() {
        if let Some(value) = line.strip_prefix("#EXTINF:") {
            pending_duration = value
                .split(',')
                .next()
                .and_then(|duration| duration.parse::<f64>().ok());
        } else if !line.is_empty() && !line.starts_with('#') {
            let Some(segment_duration) = pending_duration.take() else {
                continue;
            };
            if !valid_hls_file_name(line, "segment-", ".m4s")
                || !directory
                    .join(line)
                    .metadata()
                    .is_ok_and(|metadata| metadata.len() > 0)
            {
                return Ok(false);
            }
            duration += segment_duration;
            segments += 1;
        }
    }
    Ok(segments > 0 && duration + 0.25 >= start_at.max(0.001))
}

fn publish_master_playlist(directory: &Path, rendition: HlsRendition) -> Result<()> {
    let init = std::fs::read(directory.join("init.mp4"))
        .context("could not inspect the incremental HLS initialization segment")?;
    let video_codec = avc_codec_string(&init).ok_or_else(|| {
        anyhow!("incremental HLS initialization segment has no AVC configuration")
    })?;
    let codecs = if rendition.has_audio {
        format!("{video_codec},mp4a.40.2")
    } else {
        video_codec
    };
    let mut attributes = format!(
        "BANDWIDTH={},CODECS=\"{codecs}\",RESOLUTION={}x{}",
        rendition.bandwidth, rendition.width, rendition.height
    );
    if let Some(frame_rate) = rendition
        .frame_rate
        .filter(|rate| rate.is_finite() && *rate > 0.0)
    {
        attributes.push_str(&format!(",FRAME-RATE={frame_rate:.3}"));
    }
    let master = format!("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-STREAM-INF:{attributes}\nindex.m3u8\n");
    let temporary_path = directory.join("master.m3u8.tmp");
    std::fs::write(&temporary_path, master.as_bytes())
        .context("could not write the incremental HLS master playlist")?;
    std::fs::rename(temporary_path, directory.join("master.m3u8"))
        .context("could not publish the incremental HLS master playlist")?;
    log::debug!(
        "published incremental HLS master playlist: {}",
        master.trim_end().replace('\n', " | ")
    );
    Ok(())
}

fn avc_codec_string(init: &[u8]) -> Option<String> {
    let offset = init.windows(4).position(|window| window == b"avcC")?;
    let configuration = init.get(offset + 4..offset + 8)?;
    (configuration[0] == 1).then(|| {
        format!(
            "avc1.{:02X}{:02X}{:02X}",
            configuration[1], configuration[2], configuration[3]
        )
    })
}

struct HlsDirectoryState {
    directory: PathBuf,
    route: String,
    counters: HlsCounters,
}

struct HlsDirectoryServer {
    address: SocketAddr,
    state: Arc<HlsDirectoryState>,
    stop: Arc<AtomicBool>,
    listener_thread: Option<JoinHandle<()>>,
    client_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl HlsDirectoryServer {
    fn start(bind_address: SocketAddr, directory: PathBuf, route: String) -> Result<Self> {
        if route.is_empty()
            || !route
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            bail!("private HLS route contains invalid characters");
        }
        let listener = TcpListener::bind(bind_address)
            .with_context(|| format!("could not bind incremental HLS server to {bind_address}"))?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let state = Arc::new(HlsDirectoryState {
            directory,
            route,
            counters: HlsCounters::default(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let client_threads = Arc::new(Mutex::new(Vec::<JoinHandle<()>>::new()));
        let listener_state = Arc::clone(&state);
        let listener_stop = Arc::clone(&stop);
        let listener_clients = Arc::clone(&client_threads);

        log::debug!("incremental HLS server listening on {address}");
        let listener_thread = thread::Builder::new()
            .name("cast-http-hls".into())
            .spawn(move || {
                while !listener_stop.load(Ordering::SeqCst) {
                    reap_finished_clients(&listener_clients);
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            let state = Arc::clone(&listener_state);
                            match thread::Builder::new()
                                .name("cast-http-hls-client".into())
                                .spawn(move || {
                                    if let Err(error) = handle_client(stream, peer, &state) {
                                        state.counters.failures.fetch_add(1, Ordering::Relaxed);
                                        log::debug!(
                                            "incremental HLS request from {peer} failed: {error:#}"
                                        );
                                    }
                                }) {
                                Ok(handle) => {
                                    if let Ok(mut clients) = listener_clients.lock() {
                                        clients.push(handle);
                                    }
                                }
                                Err(error) => {
                                    listener_state
                                        .counters
                                        .failures
                                        .fetch_add(1, Ordering::Relaxed);
                                    log::warn!("could not start HLS client thread: {error}");
                                }
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20));
                        }
                        Err(error) => {
                            listener_state
                                .counters
                                .failures
                                .fetch_add(1, Ordering::Relaxed);
                            log::error!("incremental HLS listener failed: {error}");
                            break;
                        }
                    }
                }
            })
            .context("could not start incremental HLS server")?;

        Ok(Self {
            address,
            state,
            stop,
            listener_thread: Some(listener_thread),
            client_threads,
        })
    }

    fn url(&self) -> String {
        http_url(self.address, &format!("/{}/master.m3u8", self.state.route))
    }

    fn stats(&self) -> HlsServerStats {
        self.state.counters.snapshot()
    }

    #[cfg(test)]
    fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for HlsDirectoryServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.listener_thread.take() {
            let _ = thread.join();
        }
        let clients = self
            .client_threads
            .lock()
            .map(|mut clients| clients.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for client in clients {
            let _ = client.join();
        }
    }
}

fn reap_finished_clients(client_threads: &Mutex<Vec<JoinHandle<()>>>) {
    let Ok(mut clients) = client_threads.lock() else {
        return;
    };
    let mut index = 0;
    while index < clients.len() {
        if clients[index].is_finished() {
            let client = clients.swap_remove(index);
            let _ = client.join();
        } else {
            index += 1;
        }
    }
}

fn handle_client(mut stream: TcpStream, peer: SocketAddr, state: &HlsDirectoryState) -> Result<()> {
    // Accepted sockets can inherit O_NONBLOCK from the listener on macOS. Cast receivers
    // sometimes connect just before sending the request, so a nonblocking read would fail with
    // EAGAIN instead of waiting for the bounded read timeout below.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    state.counters.requests.fetch_add(1, Ordering::Relaxed);
    let (method, path) = read_request(&mut stream)?;
    let prefix = format!("/{}/", state.route);
    let file_name = path
        .split('?')
        .next()
        .and_then(|path| path.strip_prefix(&prefix));

    if method == "OPTIONS" {
        return write_empty_response(&mut stream, "204 No Content");
    }
    if method != "GET" && method != "HEAD" {
        return write_empty_response(&mut stream, "405 Method Not Allowed");
    }
    let Some(file_name) = file_name.filter(|name| allowed_hls_file_name(name)) else {
        log::debug!("HTTP {method} {path} from {peer} -> 404");
        return write_empty_response(&mut stream, "404 Not Found");
    };
    let content_type = match file_name {
        "master.m3u8" | "index.m3u8" => "application/vnd.apple.mpegurl",
        "init.mp4" => "video/mp4",
        _ => "video/iso.segment",
    };
    let file_path = state.directory.join(file_name);
    let mut file = match File::open(&file_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            log::debug!("HTTP {method} {path} from {peer} -> 404 (not published yet)");
            return write_empty_response(&mut stream, "404 Not Found");
        }
        Err(error) => return Err(error).context("could not open a prepared HLS object"),
    };
    let length = file.metadata()?.len();
    match file_name {
        "master.m3u8" | "index.m3u8" => {
            state.counters.playlists.fetch_add(1, Ordering::Relaxed);
        }
        "init.mp4" => {
            state.counters.init_segments.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            state
                .counters
                .media_segments
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, HEAD, OPTIONS\r\nConnection: close\r\n\r\n"
    )?;
    if method == "GET" {
        let mut buffer = [0_u8; TRANSFER_BUFFER_BYTES];
        loop {
            let size = file.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            stream.write_all(&buffer[..size])?;
            state
                .counters
                .bytes_sent
                .fetch_add(size as u64, Ordering::Relaxed);
        }
    }
    stream.flush()?;
    log::debug!("HTTP {method} {path} from {peer} -> 200 ({length} bytes)");
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> Result<(String, String)> {
    let mut raw = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
        if raw.len() == MAX_REQUEST_HEADER_BYTES {
            bail!("HTTP request headers are too large");
        }
        let available = (MAX_REQUEST_HEADER_BYTES - raw.len()).min(chunk.len());
        let size = stream.read(&mut chunk[..available])?;
        if size == 0 {
            bail!("HTTP client closed before sending complete headers");
        }
        raw.extend_from_slice(&chunk[..size]);
    }
    let request = std::str::from_utf8(&raw).context("HTTP request headers were not UTF-8")?;
    let mut parts = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| anyhow!("HTTP method is missing"))?
        .to_ascii_uppercase();
    let path = parts
        .next()
        .ok_or_else(|| anyhow!("HTTP path is missing"))?
        .to_owned();
    let version = parts
        .next()
        .ok_or_else(|| anyhow!("HTTP version is missing"))?;
    if parts.next().is_some() || !version.starts_with("HTTP/1.") {
        bail!("malformed HTTP request line");
    }
    Ok((method, path))
}

fn write_empty_response(stream: &mut TcpStream, status: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, HEAD, OPTIONS\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;
    Ok(())
}

fn allowed_hls_file_name(name: &str) -> bool {
    matches!(name, "master.m3u8" | "index.m3u8" | "init.mp4")
        || valid_hls_file_name(name, "segment-", ".m4s")
}

fn valid_hls_file_name(name: &str, prefix: &str, suffix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|name| name.strip_suffix(suffix))
        .is_some_and(|number| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_ready_playlist(directory: &Path, duration: f64) {
        std::fs::write(directory.join("init.mp4"), b"init").unwrap();
        std::fs::write(directory.join("segment-000000.m4s"), b"segment").unwrap();
        std::fs::write(
            directory.join("index.m3u8"),
            format!(
                "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:{duration:.3},\nsegment-000000.m4s\n"
            ),
        )
        .unwrap();
    }

    fn request(address: SocketAddr, request: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    }

    #[test]
    fn waits_for_a_complete_segment_covering_the_start_position() {
        let directory = tempfile::tempdir().unwrap();
        assert!(!playlist_is_playable(directory.path(), 0.0).unwrap());
        write_ready_playlist(directory.path(), 2.0);
        assert!(playlist_is_playable(directory.path(), 0.0).unwrap());
        assert!(playlist_is_playable(directory.path(), 2.2).unwrap());
        assert!(!playlist_is_playable(directory.path(), 3.0).unwrap());
    }

    #[test]
    fn rejects_missing_and_unpublished_playlist_objects() {
        let directory = tempfile::tempdir().unwrap();
        write_ready_playlist(directory.path(), 2.0);
        std::fs::remove_file(directory.path().join("segment-000000.m4s")).unwrap();
        assert!(!playlist_is_playable(directory.path(), 0.0).unwrap());
        assert!(!allowed_hls_file_name("../index.m3u8"));
        assert!(!allowed_hls_file_name("segment-x.m4s"));
    }

    #[test]
    fn publishes_a_master_playlist_with_the_generated_codecs() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("init.mp4"),
            b"prefixavcC\x01\x4d\x40\x1fsuffix",
        )
        .unwrap();
        publish_master_playlist(
            directory.path(),
            HlsRendition {
                bandwidth: TRANSCODED_HLS_PEAK_BANDWIDTH,
                width: 480,
                height: 270,
                frame_rate: Some(30.0),
                has_audio: true,
            },
        )
        .unwrap();
        let master = std::fs::read_to_string(directory.path().join("master.m3u8")).unwrap();
        assert!(master.contains("CODECS=\"avc1.4D401F,mp4a.40.2\""));
        assert!(master.contains("RESOLUTION=480x270,FRAME-RATE=30.000"));
        assert!(master.ends_with("index.m3u8\n"));
    }

    #[test]
    fn serves_only_published_objects_under_the_private_route() {
        let directory = tempfile::tempdir().unwrap();
        write_ready_playlist(directory.path(), 2.0);
        let server = HlsDirectoryServer::start(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            directory.path().to_owned(),
            "private".to_owned(),
        )
        .unwrap();
        assert!(server.url().ends_with("/private/master.m3u8"));
        let response = request(
            server.address(),
            "GET /private/index.m3u8 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        assert!(String::from_utf8_lossy(&response).contains("#EXT-X-MAP:URI=\"init.mp4\""));
        let response = request(
            server.address(),
            "GET /other/index.m3u8 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        assert!(response.starts_with(b"HTTP/1.1 404 Not Found"));
        assert_eq!(server.stats().playlists, 1);
    }

    #[test]
    fn waits_for_request_bytes_on_an_inherited_nonblocking_socket() {
        let directory = tempfile::tempdir().unwrap();
        write_ready_playlist(directory.path(), 2.0);
        let state = Arc::new(HlsDirectoryState {
            directory: directory.path().to_owned(),
            route: "private".to_owned(),
            counters: HlsCounters::default(),
        });
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server_stream, peer) = listener.accept().unwrap();
        server_stream.set_nonblocking(true).unwrap();
        let handler_state = Arc::clone(&state);
        let handler = thread::spawn(move || handle_client(server_stream, peer, &handler_state));

        thread::sleep(Duration::from_millis(50));
        client
            .write_all(b"GET /private/segment-000000.m4s HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();

        handler.join().unwrap().unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        assert!(response.ends_with(b"segment"));
    }
}
