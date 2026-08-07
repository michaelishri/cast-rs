use std::{
    fs::File,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::fs::FileExt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};

use crate::network::http_url;

const MAX_REQUEST_HEADER_BYTES: usize = 16 * 1024;
const TRANSFER_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaServerStats {
    pub requests: u64,
    pub full_responses: u64,
    pub range_responses: u64,
    pub bytes_sent: u64,
    pub failures: u64,
}

#[derive(Default)]
struct MediaServerCounters {
    requests: AtomicU64,
    file_requests: AtomicU64,
    full_responses: AtomicU64,
    range_responses: AtomicU64,
    bytes_sent: AtomicU64,
    failures: AtomicU64,
}

impl MediaServerCounters {
    fn snapshot(&self) -> MediaServerStats {
        MediaServerStats {
            requests: self.requests.load(Ordering::Relaxed),
            full_responses: self.full_responses.load(Ordering::Relaxed),
            range_responses: self.range_responses.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
        }
    }
}

struct ServedFile {
    file: File,
    length: u64,
    content_type: String,
    path: String,
    counters: MediaServerCounters,
}

pub struct MediaFileServer {
    address: SocketAddr,
    path: String,
    state: Arc<ServedFile>,
    stop: Arc<AtomicBool>,
    listener_thread: Option<JoinHandle<()>>,
    client_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl MediaFileServer {
    pub fn start(
        bind_address: SocketAddr,
        file: File,
        content_type: String,
        route: String,
    ) -> Result<Self> {
        if content_type.trim().is_empty()
            || content_type.contains('\r')
            || content_type.contains('\n')
        {
            bail!("media content type is empty or contains an invalid line break");
        }
        if route.is_empty()
            || !route
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            bail!("private media route contains invalid characters");
        }

        let length = file
            .metadata()
            .context("could not read local video metadata")?
            .len();
        let listener = TcpListener::bind(bind_address)
            .with_context(|| format!("could not bind local video server to {bind_address}"))?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let path = format!("/{route}/media");
        let state = Arc::new(ServedFile {
            file,
            length,
            content_type,
            path: path.clone(),
            counters: MediaServerCounters::default(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let client_threads = Arc::new(Mutex::new(Vec::<JoinHandle<()>>::new()));
        let listener_state = Arc::clone(&state);
        let listener_stop = Arc::clone(&stop);
        let listener_clients = Arc::clone(&client_threads);

        log::debug!("local video server listening on {address}");
        let listener_thread = thread::Builder::new()
            .name("caster-video-http".into())
            .spawn(move || {
                while !listener_stop.load(Ordering::SeqCst) {
                    reap_finished_clients(&listener_clients);
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            let state = Arc::clone(&listener_state);
                            match thread::Builder::new()
                                .name("caster-video-http-client".into())
                                .spawn(move || {
                                    if let Err(error) = handle_client(stream, peer, &state) {
                                        if is_expected_client_disconnect(&error) {
                                            log::debug!(
                                                "local video HTTP client {peer} closed its request: {error:#}"
                                            );
                                        } else {
                                            state
                                                .counters
                                                .failures
                                                .fetch_add(1, Ordering::Relaxed);
                                            log::warn!(
                                                "local video HTTP request from {peer} failed: {error:#}"
                                            );
                                        }
                                    }
                                })
                            {
                                Ok(handle) => match listener_clients.lock() {
                                    Ok(mut clients) => clients.push(handle),
                                    Err(_) => {
                                        log::error!("video HTTP client thread lock was poisoned");
                                        let _ = handle.join();
                                    }
                                },
                                Err(error) => {
                                    listener_state
                                        .counters
                                        .failures
                                        .fetch_add(1, Ordering::Relaxed);
                                    log::warn!(
                                        "could not start video HTTP client thread for {peer}: {error}"
                                    );
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
                            log::error!("local video HTTP listener failed: {error}");
                            break;
                        }
                    }
                }
            })
            .context("could not start local video HTTP server thread")?;

        Ok(Self {
            address,
            path,
            state,
            stop,
            listener_thread: Some(listener_thread),
            client_threads,
        })
    }

    #[cfg(test)]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn url(&self) -> String {
        http_url(self.address, &self.path)
    }

    pub fn stats(&self) -> MediaServerStats {
        self.state.counters.snapshot()
    }

    pub fn received_request(&self) -> bool {
        self.state.counters.file_requests.load(Ordering::Relaxed) > 0
    }
}

impl Drop for MediaFileServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.listener_thread.take()
            && thread.join().is_err()
        {
            log::warn!("local video HTTP listener thread panicked");
        }

        let clients = self
            .client_threads
            .lock()
            .map(|mut clients| clients.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for client in clients {
            if client.join().is_err() {
                log::warn!("local video HTTP client thread panicked");
            }
        }
    }
}

fn reap_finished_clients(client_threads: &Mutex<Vec<JoinHandle<()>>>) {
    let Ok(mut clients) = client_threads.lock() else {
        log::error!("video HTTP client thread lock was poisoned");
        return;
    };
    let mut index = 0;
    while index < clients.len() {
        if clients[index].is_finished() {
            let client = clients.swap_remove(index);
            if client.join().is_err() {
                log::warn!("local video HTTP client thread panicked");
            }
        } else {
            index += 1;
        }
    }
}

struct HttpRequest {
    method: String,
    path: String,
    range: Option<String>,
}

fn handle_client(mut stream: TcpStream, peer: SocketAddr, state: &ServedFile) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    state.counters.requests.fetch_add(1, Ordering::Relaxed);

    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            log::debug!("HTTP request from {peer} -> 400 ({error:#})");
            write_empty_response(&mut stream, "400 Bad Request", &[])?;
            return Ok(());
        }
    };
    let path = request.path.split('?').next().unwrap_or("/");
    if path != state.path {
        log::debug!("HTTP {} {path} from {peer} -> 404", request.method);
        write_empty_response(&mut stream, "404 Not Found", &[])?;
        return Ok(());
    }

    if request.method == "OPTIONS" {
        log::debug!("HTTP OPTIONS {path} from {peer} -> 204");
        write_empty_response(
            &mut stream,
            "204 No Content",
            &[("Allow", "GET, HEAD, OPTIONS")],
        )?;
        return Ok(());
    }
    if request.method != "GET" && request.method != "HEAD" {
        log::debug!("HTTP {} {path} from {peer} -> 405", request.method);
        write_empty_response(
            &mut stream,
            "405 Method Not Allowed",
            &[("Allow", "GET, HEAD, OPTIONS")],
        )?;
        return Ok(());
    }
    state.counters.file_requests.fetch_add(1, Ordering::Relaxed);

    let resolved = match resolve_range(request.range.as_deref(), state.length) {
        Ok(range) => range,
        Err(RangeError::Malformed(reason)) => {
            log::debug!(
                "HTTP {} {path} from {peer} range={:?} -> 400 ({reason})",
                request.method,
                request.range
            );
            write_empty_response(&mut stream, "400 Bad Request", &[])?;
            return Ok(());
        }
        Err(RangeError::Unsatisfiable) => {
            let content_range = format!("bytes */{}", state.length);
            log::debug!(
                "HTTP {} {path} from {peer} range={:?} -> 416",
                request.method,
                request.range
            );
            write_empty_response(
                &mut stream,
                "416 Range Not Satisfiable",
                &[("Content-Range", &content_range)],
            )?;
            return Ok(());
        }
    };

    let head_only = request.method == "HEAD";
    let (status, start, length, content_range) = match resolved {
        ResolvedRange::Full => ("200 OK", 0, state.length, None),
        ResolvedRange::Partial { start, end } => (
            "206 Partial Content",
            start,
            end - start + 1,
            Some(format!("bytes {start}-{end}/{}", state.length)),
        ),
    };
    write_file_headers(
        &mut stream,
        status,
        &state.content_type,
        length,
        content_range.as_deref(),
    )?;

    match resolved {
        ResolvedRange::Full => {
            state
                .counters
                .full_responses
                .fetch_add(1, Ordering::Relaxed);
        }
        ResolvedRange::Partial { .. } => {
            state
                .counters
                .range_responses
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    log::debug!(
        "HTTP {} {path} from {peer} range={:?} -> {status} ({length} bytes{})",
        request.method,
        request.range,
        if head_only { ", headers only" } else { "" }
    );
    if !head_only {
        write_file_range(
            &mut stream,
            &state.file,
            start,
            length,
            &state.counters.bytes_sent,
        )?;
    }
    stream.flush()?;
    Ok(())
}

fn is_expected_client_disconnect(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::ConnectionReset
            )
        })
    })
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut raw = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        if raw.len() == MAX_REQUEST_HEADER_BYTES {
            bail!("HTTP request headers exceed {MAX_REQUEST_HEADER_BYTES} bytes");
        }
        let remaining = MAX_REQUEST_HEADER_BYTES - raw.len();
        let max_read = remaining.min(chunk.len());
        let size = stream.read(&mut chunk[..max_read])?;
        if size == 0 {
            bail!("HTTP client closed before completing request headers");
        }
        raw.extend_from_slice(&chunk[..size]);
        if raw.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let request = std::str::from_utf8(&raw).context("HTTP request headers were not UTF-8")?;
    let mut lines = request.split("\r\n");
    let mut request_line = lines
        .next()
        .ok_or_else(|| anyhow!("HTTP request line is missing"))?
        .split_whitespace();
    let method = request_line
        .next()
        .ok_or_else(|| anyhow!("HTTP method is missing"))?;
    let path = request_line
        .next()
        .ok_or_else(|| anyhow!("HTTP request target is missing"))?;
    let version = request_line
        .next()
        .ok_or_else(|| anyhow!("HTTP version is missing"))?;
    if request_line.next().is_some() || !version.starts_with("HTTP/1.") {
        bail!("malformed HTTP request line");
    }

    let mut range = None;
    for line in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("malformed HTTP header"))?;
        if name.eq_ignore_ascii_case("range") {
            if range.is_some() {
                bail!("multiple Range headers are not supported");
            }
            range = Some(value.trim().to_owned());
        }
    }

    Ok(HttpRequest {
        method: method.to_ascii_uppercase(),
        path: path.to_owned(),
        range,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolvedRange {
    Full,
    Partial { start: u64, end: u64 },
}

#[derive(Debug, PartialEq, Eq)]
enum RangeError {
    Malformed(&'static str),
    Unsatisfiable,
}

fn resolve_range(header: Option<&str>, length: u64) -> Result<ResolvedRange, RangeError> {
    let Some(header) = header else {
        return Ok(ResolvedRange::Full);
    };
    let Some((unit, specification)) = header.split_once('=') else {
        return Err(RangeError::Malformed("byte range is missing '='"));
    };
    if !unit.eq_ignore_ascii_case("bytes") {
        return Err(RangeError::Malformed("only byte ranges are supported"));
    }
    if specification.contains(',') {
        log::debug!("ignoring multipart byte range request: {header}");
        return Ok(ResolvedRange::Full);
    }
    let (start, end) = specification
        .split_once('-')
        .ok_or(RangeError::Malformed("byte range is missing '-'"))?;
    if length == 0 {
        return Err(RangeError::Unsatisfiable);
    }

    if start.is_empty() {
        let suffix = parse_range_number(end)?;
        if suffix == 0 {
            return Err(RangeError::Unsatisfiable);
        }
        let start = length.saturating_sub(suffix);
        return Ok(ResolvedRange::Partial {
            start,
            end: length - 1,
        });
    }

    let start = parse_range_number(start)?;
    if start >= length {
        return Err(RangeError::Unsatisfiable);
    }
    let end = if end.is_empty() {
        length - 1
    } else {
        parse_range_number(end)?.min(length - 1)
    };
    if end < start {
        return Err(RangeError::Unsatisfiable);
    }
    Ok(ResolvedRange::Partial { start, end })
}

fn parse_range_number(value: &str) -> Result<u64, RangeError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RangeError::Malformed("byte range contains a non-number"));
    }
    value
        .parse()
        .map_err(|_| RangeError::Malformed("byte range number is too large"))
}

fn write_file_headers(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    content_length: u64,
    content_range: Option<&str>,
) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\nAccept-Ranges: bytes\r\n"
    )?;
    if let Some(content_range) = content_range {
        write!(stream, "Content-Range: {content_range}\r\n")?;
    }
    write_common_headers(stream)?;
    write!(stream, "Connection: close\r\n\r\n")?;
    Ok(())
}

fn write_empty_response(
    stream: &mut TcpStream,
    status: &str,
    extra_headers: &[(&str, &str)],
) -> Result<()> {
    write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\n")?;
    for (name, value) in extra_headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write_common_headers(stream)?;
    write!(stream, "Connection: close\r\n\r\n")?;
    stream.flush()?;
    Ok(())
}

fn write_common_headers(stream: &mut TcpStream) -> Result<()> {
    write!(
        stream,
        "Cache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, HEAD, OPTIONS\r\nAccess-Control-Allow-Headers: Range\r\nAccess-Control-Expose-Headers: Accept-Ranges, Content-Length, Content-Range\r\n"
    )?;
    Ok(())
}

fn write_file_range(
    stream: &mut TcpStream,
    file: &File,
    start: u64,
    length: u64,
    bytes_sent: &AtomicU64,
) -> Result<()> {
    let mut buffer = vec![0_u8; TRANSFER_BUFFER_BYTES];
    let mut offset = start;
    let mut remaining = length;
    while remaining > 0 {
        let requested = usize::try_from(remaining.min(TRANSFER_BUFFER_BYTES as u64)).unwrap();
        let size = file.read_at(&mut buffer[..requested], offset)?;
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "local video ended while serving a declared byte range",
            )
            .into());
        }
        stream.write_all(&buffer[..size])?;
        let size = u64::try_from(size).unwrap();
        bytes_sent.fetch_add(size, Ordering::Relaxed);
        offset += size;
        remaining -= size;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpStream,
        time::Instant,
    };

    use tempfile::NamedTempFile;

    use super::*;

    const CONTENT: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    struct TestServer {
        _file: NamedTempFile,
        server: MediaFileServer,
    }

    impl TestServer {
        fn start() -> Self {
            let mut file = NamedTempFile::new().unwrap();
            file.write_all(CONTENT).unwrap();
            file.flush().unwrap();
            let served = File::open(file.path()).unwrap();
            let server = MediaFileServer::start(
                SocketAddr::from(([127, 0, 0, 1], 0)),
                served,
                "video/mp4".to_owned(),
                "test-route".to_owned(),
            )
            .unwrap();
            Self {
                _file: file,
                server,
            }
        }

        fn request(&self, request: &str) -> Vec<u8> {
            raw_request(self.server.address(), request)
        }
    }

    fn raw_request(address: SocketAddr, request: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    }

    fn response_parts(response: &[u8]) -> (&str, &[u8]) {
        let boundary = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        (
            std::str::from_utf8(&response[..boundary]).unwrap(),
            &response[boundary + 4..],
        )
    }

    #[test]
    fn resolves_supported_ranges() {
        assert_eq!(resolve_range(None, 10), Ok(ResolvedRange::Full));
        assert_eq!(
            resolve_range(Some("bytes=2-5"), 10),
            Ok(ResolvedRange::Partial { start: 2, end: 5 })
        );
        assert_eq!(
            resolve_range(Some("Bytes=2-5"), 10),
            Ok(ResolvedRange::Partial { start: 2, end: 5 })
        );
        assert_eq!(
            resolve_range(Some("bytes=7-"), 10),
            Ok(ResolvedRange::Partial { start: 7, end: 9 })
        );
        assert_eq!(
            resolve_range(Some("bytes=-3"), 10),
            Ok(ResolvedRange::Partial { start: 7, end: 9 })
        );
        assert_eq!(
            resolve_range(Some("bytes=-30"), 10),
            Ok(ResolvedRange::Partial { start: 0, end: 9 })
        );
        assert_eq!(
            resolve_range(Some("bytes=2-30"), 10),
            Ok(ResolvedRange::Partial { start: 2, end: 9 })
        );
    }

    #[test]
    fn rejects_malformed_and_unsatisfiable_ranges() {
        assert_eq!(
            resolve_range(Some("items=0-1"), 10),
            Err(RangeError::Malformed("only byte ranges are supported"))
        );
        assert_eq!(
            resolve_range(Some("bytes=x-1"), 10),
            Err(RangeError::Malformed("byte range contains a non-number"))
        );
        assert_eq!(
            resolve_range(Some("bytes=10-"), 10),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(
            resolve_range(Some("bytes=5-2"), 10),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(
            resolve_range(Some("bytes=0-0"), 0),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(
            resolve_range(Some("bytes=-0"), 10),
            Err(RangeError::Unsatisfiable)
        );
    }

    #[test]
    fn classifies_receiver_disconnects_without_hiding_server_failures() {
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
        ] {
            assert!(is_expected_client_disconnect(&io::Error::from(kind).into()));
        }
        assert!(!is_expected_client_disconnect(
            &io::Error::from(io::ErrorKind::TimedOut).into()
        ));
    }

    #[test]
    fn serves_full_get_and_head_with_cors_and_ranges() {
        let test = TestServer::start();
        let response = test.request("GET /test-route/media HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let (headers, body) = response_parts(&response);
        assert!(
            headers.starts_with("HTTP/1.1 200 OK"),
            "unexpected response headers: {headers}"
        );
        assert!(headers.contains("Content-Type: video/mp4"));
        assert!(headers.contains(&format!("Content-Length: {}", CONTENT.len())));
        assert!(headers.contains("Accept-Ranges: bytes"));
        assert!(headers.contains("Access-Control-Allow-Origin: *"));
        assert_eq!(body, CONTENT);

        let response = test.request("HEAD /test-route/media HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let (headers, body) = response_parts(&response);
        assert!(headers.starts_with("HTTP/1.1 200 OK"));
        assert!(headers.contains(&format!("Content-Length: {}", CONTENT.len())));
        assert!(body.is_empty());
    }

    #[test]
    fn waits_for_request_headers_after_accepting_a_connection() {
        let test = TestServer::start();
        let mut stream = TcpStream::connect(test.server.address()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while test.server.stats().requests == 0 {
            assert!(
                Instant::now() < deadline,
                "server did not accept test client"
            );
            thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(20));

        stream
            .write_all(b"GET /test-route/media HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();

        let (headers, body) = response_parts(&response);
        assert!(headers.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(body, CONTENT);
    }

    #[test]
    fn serves_closed_open_and_suffix_ranges() {
        let test = TestServer::start();
        for (range, expected, content_range) in [
            ("bytes=2-5", b"2345".as_slice(), "bytes 2-5/36"),
            ("bytes=33-", b"xyz".as_slice(), "bytes 33-35/36"),
            ("bytes=-3", b"xyz".as_slice(), "bytes 33-35/36"),
        ] {
            let response = test.request(&format!(
                "GET /test-route/media HTTP/1.1\r\nHost: localhost\r\nRange: {range}\r\n\r\n"
            ));
            let (headers, body) = response_parts(&response);
            assert!(headers.starts_with("HTTP/1.1 206 Partial Content"));
            assert!(headers.contains(&format!("Content-Range: {content_range}")));
            assert_eq!(body, expected);
        }
    }

    #[test]
    fn returns_416_for_an_unsatisfiable_range() {
        let test = TestServer::start();
        let response = test.request(
            "GET /test-route/media HTTP/1.1\r\nHost: localhost\r\nRange: bytes=99-\r\n\r\n",
        );
        let (headers, body) = response_parts(&response);
        assert!(headers.starts_with("HTTP/1.1 416 Range Not Satisfiable"));
        assert!(headers.contains("Content-Range: bytes */36"));
        assert!(body.is_empty());
    }

    #[test]
    fn ignores_multipart_ranges_with_a_full_response() {
        let test = TestServer::start();
        let response = test.request(
            "GET /test-route/media HTTP/1.1\r\nHost: localhost\r\nRange: bytes=0-1,4-5\r\n\r\n",
        );
        let (headers, body) = response_parts(&response);
        assert!(headers.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(body, CONTENT);
    }

    #[test]
    fn isolates_the_private_route_and_rejects_unsupported_methods() {
        let test = TestServer::start();
        let response = test.request("GET /other/media HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(response_parts(&response).0.starts_with("HTTP/1.1 404"));
        assert!(!test.server.received_request());

        let response = test.request("POST /test-route/media HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let (headers, body) = response_parts(&response);
        assert!(headers.starts_with("HTTP/1.1 405 Method Not Allowed"));
        assert!(headers.contains("Allow: GET, HEAD, OPTIONS"));
        assert!(body.is_empty());
        assert!(!test.server.received_request());
    }

    #[test]
    fn answers_cors_preflight_without_exposing_file_bytes() {
        let test = TestServer::start();
        let response = test.request(
            "OPTIONS /test-route/media HTTP/1.1\r\nHost: localhost\r\nAccess-Control-Request-Method: GET\r\nAccess-Control-Request-Headers: Range\r\n\r\n",
        );
        let (headers, body) = response_parts(&response);
        assert!(headers.starts_with("HTTP/1.1 204 No Content"));
        assert!(headers.contains("Access-Control-Allow-Origin: *"));
        assert!(headers.contains("Access-Control-Allow-Headers: Range"));
        assert!(body.is_empty());
        assert!(!test.server.received_request());
    }

    #[test]
    fn serves_simultaneous_ranges_without_shared_cursor_races() {
        let test = TestServer::start();
        let address = test.server.address();
        let first = thread::spawn(move || {
            raw_request(
                address,
                "GET /test-route/media HTTP/1.1\r\nRange: bytes=0-9\r\n\r\n",
            )
        });
        let second = thread::spawn(move || {
            raw_request(
                address,
                "GET /test-route/media HTTP/1.1\r\nRange: bytes=26-35\r\n\r\n",
            )
        });
        assert_eq!(response_parts(&first.join().unwrap()).1, b"0123456789");
        assert_eq!(response_parts(&second.join().unwrap()).1, b"qrstuvwxyz");
    }

    #[test]
    fn streams_files_larger_than_the_transfer_buffer() {
        let content = (0..TRANSFER_BUFFER_BYTES * 3 + 17)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&content).unwrap();
        file.flush().unwrap();
        let served = File::open(file.path()).unwrap();
        let server = MediaFileServer::start(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            served,
            "video/mp4".to_owned(),
            "large-route".to_owned(),
        )
        .unwrap();
        let response = raw_request(
            server.address(),
            "GET /large-route/media HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        assert_eq!(response_parts(&response).1, content);
        assert_eq!(server.stats().bytes_sent, content.len() as u64);
    }

    #[test]
    fn reports_server_stats() {
        let test = TestServer::start();
        test.request("GET /test-route/media HTTP/1.1\r\nHost: localhost\r\n\r\n");
        test.request(
            "HEAD /test-route/media HTTP/1.1\r\nHost: localhost\r\nRange: bytes=0-3\r\n\r\n",
        );
        let stats = test.server.stats();
        assert_eq!(stats.requests, 2);
        assert_eq!(stats.full_responses, 1);
        assert_eq!(stats.range_responses, 1);
        assert_eq!(stats.bytes_sent, CONTENT.len() as u64);
        assert_eq!(stats.failures, 0);
        assert!(test.server.received_request());
        assert!(test.server.url().ends_with("/test-route/media"));
    }
}
