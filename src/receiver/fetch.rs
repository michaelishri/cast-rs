use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

const CHUNK_SIZE: usize = 256 * 1024;
const DOWNLOAD_CAP: u64 = 2 * 1024 * 1024 * 1024;

/// A reader over HTTP range requests against a sender-supplied origin. The
/// pinned FFmpeg libraries are built with `--disable-network`, so the player
/// reads media through this module's custom AVIO callbacks instead of
/// libavformat's own protocol stack.
pub struct RangeReader {
    url: String,
    agent: ureq::Agent,
    size: Option<u64>,
    range_support: bool,
    chunk: Vec<u8>,
    chunk_start: u64,
}

impl RangeReader {
    /// Probes the origin for range support and total size.
    pub fn open(url: &str) -> Result<Self> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .build();
        let mut reader = Self {
            url: url.to_owned(),
            agent,
            size: None,
            range_support: false,
            chunk: Vec::new(),
            chunk_start: 0,
        };
        reader.probe()?;
        Ok(reader)
    }

    pub fn size(&self) -> Option<u64> {
        self.size
    }

    pub fn supports_ranges(&self) -> bool {
        self.range_support
    }

    fn probe(&mut self) -> Result<()> {
        let response = self.agent.get(&self.url).set("Range", "bytes=0-0").call();
        match response {
            Ok(response) if response.status() == 206 => {
                self.range_support = true;
                self.size = response
                    .header("Content-Range")
                    .and_then(parse_total_from_content_range);
                Ok(())
            }
            Ok(response) => {
                self.range_support = false;
                if let Some(total) = response
                    .header("Content-Length")
                    .and_then(|value| value.parse().ok())
                {
                    self.size = Some(total);
                }
                Ok(())
            }
            Err(ureq::Error::Status(code, _)) => Err(anyhow!(
                "the sender's media origin answered HTTP {code}; it may have stopped serving the file"
            )),
            Err(error) => Err(anyhow!(error).context("could not reach the sender's media origin")),
        }
    }

    /// Reads up to `buffer.len()` bytes starting at `offset`, served from the
    /// cached chunk or a fresh ranged request.
    pub fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if let Some(size) = self.size
            && offset >= size
        {
            return Ok(0);
        }
        if offset < self.chunk_start || offset >= self.chunk_start + self.chunk.len() as u64 {
            self.fill_chunk(offset)?;
        }
        let start = (offset - self.chunk_start) as usize;
        let available = self.chunk.len() - start;
        let count = buffer.len().min(available);
        buffer[..count].copy_from_slice(&self.chunk[start..start + count]);
        Ok(count)
    }

    fn fill_chunk(&mut self, offset: u64) -> Result<()> {
        let request = if self.range_support {
            self.agent.get(&self.url).set(
                "Range",
                &format!("bytes={offset}-{}", offset + CHUNK_SIZE as u64 - 1),
            )
        } else {
            self.agent.get(&self.url)
        };
        let response = request.call().map_err(|error| {
            anyhow!(error).context(format!("could not fetch media from {}", self.url))
        })?;
        let mut body = response.into_reader();
        let mut chunk = Vec::with_capacity(if self.range_support {
            CHUNK_SIZE
        } else {
            1024 * 1024
        });
        if self.range_support {
            let mut remaining = CHUNK_SIZE;
            let mut buffer = [0_u8; 16 * 1024];
            while remaining > 0 {
                let want = buffer.len().min(remaining);
                let read = body
                    .read(&mut buffer[..want])
                    .context("media fetch failed")?;
                if read == 0 {
                    break;
                }
                chunk.extend_from_slice(&buffer[..read]);
                remaining -= read;
            }
        } else {
            // Without range support a GET returns the whole body; cap how much
            // we buffer to keep memory bounded while still allowing
            // short-file playback, and record the true size for AVSEEK_SIZE.
            let cap = self.size.unwrap_or(DOWNLOAD_CAP).min(DOWNLOAD_CAP);
            let mut taken = 0_usize;
            let mut buffer = [0_u8; 64 * 1024];
            while taken < cap as usize {
                let want = buffer.len().min(cap as usize - taken);
                let read = body
                    .read(&mut buffer[..want])
                    .context("media fetch failed")?;
                if read == 0 {
                    break;
                }
                chunk.extend_from_slice(&buffer[..read]);
                taken += read;
            }
            self.size = Some(chunk.len() as u64);
        }
        if chunk.is_empty() {
            return Err(anyhow!("the sender's media origin returned no data"));
        }
        self.chunk_start = offset;
        self.chunk = chunk;
        Ok(())
    }

    /// Streams the entire origin into a temp file for origins without range
    /// support, returning the local path.
    pub fn download_to_temp(&self) -> Result<std::path::PathBuf> {
        download_for_playback(&self.url, self.size)
    }
}

fn parse_total_from_content_range(value: &str) -> Option<u64> {
    // Content-Range: bytes 0-0/123456
    let total = value.rsplit('/').next()?.trim();
    if total == "*" {
        return None;
    }
    total.parse().ok()
}

/// Downloads the origin into a temp file and returns its path. Used when the
/// origin cannot serve range requests and the file is small enough.
pub fn download_for_playback(url: &str, size: Option<u64>) -> Result<std::path::PathBuf> {
    if let Some(size) = size
        && size > DOWNLOAD_CAP
    {
        return Err(anyhow!(
            "the media is {size} bytes; receivers need range-capable origins above {DOWNLOAD_CAP} bytes"
        ));
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(120))
        .build();
    let response = agent
        .get(url)
        .call()
        .map_err(|error| anyhow!(error).context("could not fetch media from the origin"))?;
    let mut file =
        tempfile::NamedTempFile::new().context("could not create a temporary media file")?;
    let mut body = response.into_reader();
    std::io::copy(&mut body, &mut file).context("could not download the sender's media")?;
    let path = file.path().to_path_buf();
    file.persist(&path)
        .map_err(|error| error.error)
        .context("could not keep the temporary media file")?;
    Ok(path)
}

/// Rejects URLs and content types the v1 receiver cannot play, with reasons
/// senders can surface.
pub fn classify_load(url: &str, content_type: &str, stream_type: &str) -> Result<()> {
    if stream_type.eq_ignore_ascii_case("LIVE") {
        return Err(anyhow!("live streams are not supported by this receiver"));
    }
    let lowered = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if lowered.ends_with(".m3u8")
        || content_type.contains("mpegurl")
        || content_type.contains("vnd.apple")
    {
        return Err(anyhow!("HLS playlists are not supported by this receiver"));
    }
    if lowered.ends_with(".mpd") || content_type.contains("dash+xml") {
        return Err(anyhow!("DASH manifests are not supported by this receiver"));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(anyhow!("only http(s) media URLs are supported"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_totals_parse() {
        assert_eq!(
            parse_total_from_content_range("bytes 0-0/12345"),
            Some(12345)
        );
        assert_eq!(parse_total_from_content_range("bytes 0-0/*"), None);
        assert_eq!(parse_total_from_content_range("garbage"), None);
    }

    #[test]
    fn unsupported_sources_are_classified_with_reasons() {
        let error =
            classify_load("http://a/movie.m3u8", "application/x-mpegURL", "BUFFERED").unwrap_err();
        assert!(error.to_string().contains("HLS"));
        let error =
            classify_load("http://a/live.m3u8", "application/x-mpegURL", "LIVE").unwrap_err();
        assert!(error.to_string().contains("live"));
        let error =
            classify_load("http://a/manifest.mpd", "application/dash+xml", "BUFFERED").unwrap_err();
        assert!(error.to_string().contains("DASH"));
        let error = classify_load("rtsp://a/movie", "video/mp4", "BUFFERED").unwrap_err();
        assert!(error.to_string().contains("http(s)"));
        assert!(classify_load("http://a/movie.mp4", "video/mp4", "BUFFERED").is_ok());
        assert!(classify_load("http://a/track?query#frag", "audio/mp4", "BUFFERED").is_ok());
        assert!(classify_load("https://a/movie.mp4", "video/mp4", "BUFFERED").is_ok());
    }

    #[test]
    fn missing_origin_fails_with_a_diagnosable_error() {
        let error = match RangeReader::open("http://127.0.0.1:1/missing.mp4") {
            Ok(_) => panic!("a closed port cannot serve media"),
            Err(error) => error,
        };
        assert!(error.to_string().to_lowercase().contains("could not"));
    }
}
