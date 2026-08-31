use std::{
    collections::BTreeMap,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde::Serialize;

const CAST_SERVICE: &str = "_googlecast._tcp.local.";
const CAPABILITY_VIDEO_OUT: u32 = 1;
const CAPABILITY_AUDIO_OUT: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceCapability {
    AudioOnly,
    Video,
    Unknown,
}

impl DeviceCapability {
    fn from_mdns(value: Option<&str>) -> Self {
        let Some(capabilities) = value.and_then(|value| value.parse::<u32>().ok()) else {
            return Self::Unknown;
        };

        if capabilities & CAPABILITY_VIDEO_OUT != 0 {
            Self::Video
        } else if capabilities & CAPABILITY_AUDIO_OUT != 0 {
            Self::AudioOnly
        } else {
            Self::Unknown
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::AudioOnly => "Audio only",
            Self::Video => "Audio + video",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CastService {
    pub name: String,
    pub model: String,
    pub capability: DeviceCapability,
    pub address: IpAddr,
    pub port: u16,
}

#[derive(Debug)]
pub enum DiscoveryEvent {
    Device(CastService),
    Finished,
    #[allow(dead_code)]
    Failed(String),
}

pub struct DiscoverySession {
    events: Receiver<DiscoveryEvent>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl DiscoverySession {
    pub fn start(timeout: Duration) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("could not start mDNS discovery")?;
        let receiver = daemon
            .browse(CAST_SERVICE)
            .context("could not browse for Google Cast devices")?;
        let (sender, events) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker = thread::Builder::new()
            .name("cast-device-discovery".to_owned())
            .spawn(move || {
                let deadline = Instant::now() + timeout;
                while !worker_cancel.load(Ordering::SeqCst) {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        break;
                    };
                    match receiver.recv_timeout(remaining.min(Duration::from_millis(100))) {
                        Ok(ServiceEvent::ServiceResolved(info)) => {
                            if let Some(service) = service_from_info(&info)
                                && sender.send(DiscoveryEvent::Device(service)).is_err()
                            {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => {}
                    }
                }
                let _ = daemon.stop_browse(CAST_SERVICE);
                let _ = daemon.shutdown();
                let _ = sender.send(DiscoveryEvent::Finished);
            })
            .context("could not start receiver discovery worker")?;
        Ok(Self {
            events,
            cancel,
            worker: Some(worker),
        })
    }

    pub fn try_recv(&self) -> std::result::Result<DiscoveryEvent, mpsc::TryRecvError> {
        self.events.try_recv()
    }

    pub fn stop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            log::warn!("receiver discovery worker panicked");
        }
    }
}

impl Drop for DiscoverySession {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn discover(timeout: Duration) -> Result<Vec<CastService>> {
    let daemon = ServiceDaemon::new().context("could not start mDNS discovery")?;
    let receiver = daemon
        .browse(CAST_SERVICE)
        .context("could not browse for Google Cast devices")?;
    let deadline = Instant::now() + timeout;
    let mut found = BTreeMap::new();

    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(service) = service_from_info(&info) {
                    found.insert((service.address, service.port), service);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let _ = daemon.stop_browse(CAST_SERVICE);
    let _ = daemon.shutdown();
    let mut services: Vec<_> = found.into_values().collect();
    sort_services(&mut services);
    Ok(services)
}

pub fn merge_service(services: &mut Vec<CastService>, service: CastService) {
    if let Some(existing) = services
        .iter_mut()
        .find(|existing| existing.address == service.address && existing.port == service.port)
    {
        *existing = service;
    } else {
        services.push(service);
    }
    sort_services(services);
}

fn sort_services(services: &mut [CastService]) {
    services.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.address.cmp(&right.address))
            .then_with(|| left.port.cmp(&right.port))
    });
}

fn service_from_info(info: &mdns_sd::ResolvedService) -> Option<CastService> {
    let address = info
        .get_addresses_v4()
        .iter()
        .copied()
        .next()
        .map(IpAddr::V4)
        .or_else(|| info.get_addresses().iter().next().map(|ip| ip.to_ip_addr()))?;
    Some(CastService {
        name: property(info, "fn").unwrap_or_else(|| info.get_fullname().into()),
        model: property(info, "md").unwrap_or_else(|| "Unknown".into()),
        capability: DeviceCapability::from_mdns(property(info, "ca").as_deref()),
        address,
        port: info.get_port(),
    })
}

fn property(info: &mdns_sd::ResolvedService, key: &str) -> Option<String> {
    info.get_property_val_str(key).map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::{CastService, DeviceCapability, merge_service};

    #[test]
    fn identifies_audio_only_and_video_receivers_from_mdns_capabilities() {
        assert_eq!(
            DeviceCapability::from_mdns(Some("4")),
            DeviceCapability::AudioOnly
        );
        assert_eq!(
            DeviceCapability::from_mdns(Some("5")),
            DeviceCapability::Video
        );
    }

    #[test]
    fn serializes_the_desktop_integration_contract() {
        let service = CastService {
            name: "Living Room".to_owned(),
            model: "Google TV".to_owned(),
            capability: DeviceCapability::Video,
            address: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8)),
            port: 8009,
        };
        assert_eq!(
            serde_json::to_value(service).unwrap(),
            serde_json::json!({
                "name": "Living Room",
                "model": "Google TV",
                "capability": "video",
                "address": "192.0.2.8",
                "port": 8009
            })
        );
    }

    #[test]
    fn handles_missing_or_unrecognised_capabilities() {
        assert_eq!(DeviceCapability::from_mdns(None), DeviceCapability::Unknown);
        assert_eq!(
            DeviceCapability::from_mdns(Some("not-a-number")),
            DeviceCapability::Unknown
        );
    }

    #[test]
    fn merges_by_address_and_port_and_sorts_by_display_name() {
        let service = |name: &str, last: u8| CastService {
            name: name.to_owned(),
            model: "Receiver".to_owned(),
            capability: DeviceCapability::Video,
            address: IpAddr::V4(Ipv4Addr::new(192, 0, 2, last)),
            port: 8009,
        };
        let mut services = Vec::new();
        merge_service(&mut services, service("Zulu", 2));
        merge_service(&mut services, service("alpha", 1));
        merge_service(&mut services, service("Bedroom", 2));
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].name, "alpha");
        assert_eq!(services[1].name, "Bedroom");
    }
}
