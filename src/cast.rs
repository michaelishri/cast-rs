use std::{net::IpAddr, sync::mpsc, thread, time::Duration};

use anyhow::{Context, Result, anyhow};
use rust_cast::{
    CastDevice, ChannelMessage,
    channels::{
        heartbeat::HeartbeatResponse,
        media::{HlsSegmentFormat, LoadOptions, Media, MediaResponse, StreamType},
        receiver::CastDeviceApp,
    },
};

pub use rust_cast::channels::media::HlsVideoSegmentFormat;

struct MediaLoad {
    url: String,
    content_type: String,
    live: bool,
    hls_segment_format: Option<HlsSegmentFormat>,
    hls_video_segment_format: Option<HlsVideoSegmentFormat>,
}

pub fn cast_url(host: IpAddr, port: u16, url: &str, content_type: &str, live: bool) -> Result<()> {
    cast_url_with_options(host, port, url, content_type, live, None, None)
}

pub fn cast_url_with_hls_video_format(
    host: IpAddr,
    port: u16,
    url: &str,
    content_type: &str,
    live: bool,
    format: HlsVideoSegmentFormat,
) -> Result<()> {
    let segment_format = (format == HlsVideoSegmentFormat::Fmp4).then_some(HlsSegmentFormat::Fmp4);
    cast_url_with_options(
        host,
        port,
        url,
        content_type,
        live,
        segment_format,
        Some(format),
    )
}

pub fn cast_fmp4_hls(host: IpAddr, port: u16, url: &str) -> Result<()> {
    cast_url_with_options(
        host,
        port,
        url,
        "application/x-mpegURL",
        true,
        Some(HlsSegmentFormat::Fmp4),
        Some(HlsVideoSegmentFormat::Fmp4),
    )
}

fn cast_url_with_options(
    host: IpAddr,
    port: u16,
    url: &str,
    content_type: &str,
    live: bool,
    hls_segment_format: Option<HlsSegmentFormat>,
    hls_video_segment_format: Option<HlsVideoSegmentFormat>,
) -> Result<()> {
    let media_load = MediaLoad {
        url: url.to_owned(),
        content_type: content_type.to_owned(),
        live,
        hls_segment_format,
        hls_video_segment_format,
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("caster-cast-control".into())
        .spawn(move || {
            let result = cast_url_inner(host, port, &media_load, &sender);
            if let Err(error) = result {
                let message = format!("{error:#}");
                if sender.send(Err(error)).is_err() {
                    log::warn!("Cast control connection ended: {message}");
                }
            }
        })
        .context("could not start Cast control thread")?;

    receiver
        .recv_timeout(Duration::from_secs(20))
        .context("Cast receiver did not respond within 20 seconds")?
}

fn cast_url_inner(
    host: IpAddr,
    port: u16,
    media_load: &MediaLoad,
    ready: &mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    eprintln!("Connecting to Cast receiver at {host}:{port}...");
    let device = CastDevice::connect_without_host_verification(host.to_string(), port)
        .with_context(|| format!("could not connect to Cast device at {host}:{port}"))?;
    device
        .connection
        .connect("receiver-0")
        .context("could not initialize the Cast receiver channel")?;
    device
        .heartbeat
        .ping()
        .context("could not initialize the Cast heartbeat")?;
    eprintln!("Launching the Default Media Receiver...");
    let application = device
        .receiver
        .launch_app(&CastDeviceApp::DefaultMediaReceiver)
        .context("could not launch the Default Media Receiver")?;

    eprintln!("Opening the receiver media channel...");
    device
        .connection
        .connect(&application.transport_id)
        .context("could not connect to the receiver application")?;

    let media = Media {
        content_id: media_load.url.clone(),
        stream_type: if media_load.live {
            StreamType::Live
        } else {
            StreamType::Buffered
        },
        content_type: media_load.content_type.clone(),
        hls_segment_format: media_load.hls_segment_format,
        hls_video_segment_format: media_load.hls_video_segment_format,
        metadata: None,
        duration: media_load.live.then_some(-1.0),
    };

    log::debug!(
        "loading media: stream_type={}, duration={:?}, start_position={}, hls_segment_format={:?}, hls_video_segment_format={:?}",
        media.stream_type,
        media.duration,
        if media_load.live { "live-edge" } else { "0s" },
        media.hls_segment_format,
        media.hls_video_segment_format
    );

    eprintln!("Loading the live media URL...");
    let load_options = LoadOptions {
        current_time: (!media_load.live).then_some(0.0),
        autoplay: true,
    };
    let status = device
        .media
        .load_with_opts(
            application.transport_id,
            application.session_id,
            &media,
            load_options,
        )
        .context("Cast receiver rejected the media URL")?;

    log::debug!(
        "Cast LOAD response contained {} media status entries: {status:?}",
        status.entries.len()
    );
    println!("Cast receiver accepted {}", media_load.url);
    ready
        .send(Ok(()))
        .map_err(|_| anyhow!("caller stopped waiting for the Cast receiver"))?;

    monitor_receiver(&device)
}

fn monitor_receiver(device: &CastDevice<'_>) -> Result<()> {
    log::debug!("monitoring Cast receiver messages and heartbeats");
    loop {
        match device
            .receive()
            .context("could not receive the next Cast message")?
        {
            ChannelMessage::Heartbeat(HeartbeatResponse::Ping) => {
                log::trace!("receiver heartbeat PING; sending PONG");
                device
                    .heartbeat
                    .pong()
                    .context("could not answer Cast receiver heartbeat")?;
            }
            ChannelMessage::Heartbeat(HeartbeatResponse::Pong) => {
                log::trace!("receiver heartbeat PONG");
            }
            ChannelMessage::Heartbeat(message) => {
                log::debug!("unrecognized receiver heartbeat message: {message:?}");
            }
            ChannelMessage::Media(MediaResponse::Status(status)) => {
                if status.entries.is_empty() {
                    log::debug!("receiver media status has no active entries");
                }
                for entry in status.entries {
                    log::debug!(
                        "receiver media status: state={}, extended={:?}, idle_reason={:?}, current_time={:?}",
                        entry.player_state,
                        entry.extended_status,
                        entry.idle_reason,
                        entry.current_time
                    );
                }
            }
            ChannelMessage::Media(MediaResponse::Error(error)) => {
                log::error!(
                    "receiver media error: code={:?}, type={}",
                    error.detailed_error_code,
                    error.message_type
                );
            }
            ChannelMessage::Media(MediaResponse::LoadFailed(error)) => {
                log::error!(
                    "receiver failed to load the media (request_id={})",
                    error.request_id
                );
            }
            ChannelMessage::Media(MediaResponse::LoadCancelled(error)) => {
                log::warn!(
                    "receiver cancelled the media load (request_id={})",
                    error.request_id
                );
            }
            ChannelMessage::Media(message) => {
                log::debug!("receiver media message: {message:?}");
            }
            ChannelMessage::Connection(message) => {
                log::debug!("receiver connection message: {message:?}");
            }
            ChannelMessage::Receiver(message) => {
                log::trace!("receiver status message: {message:?}");
            }
            ChannelMessage::Raw(message) => {
                log::debug!("receiver raw Cast message: {message:?}");
            }
        }
    }
}
