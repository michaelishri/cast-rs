use std::{
    collections::BTreeMap,
    net::IpAddr,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};

const CAST_SERVICE: &str = "_googlecast._tcp.local.";
const CAPABILITY_VIDEO_OUT: u32 = 1;
const CAPABILITY_AUDIO_OUT: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastService {
    pub name: String,
    pub model: String,
    pub capability: DeviceCapability,
    pub address: IpAddr,
    pub port: u16,
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
                let address = info
                    .get_addresses_v4()
                    .iter()
                    .copied()
                    .next()
                    .map(IpAddr::V4)
                    .or_else(|| info.get_addresses().iter().next().map(|ip| ip.to_ip_addr()));

                if let Some(address) = address {
                    let name = property(&info, "fn").unwrap_or_else(|| info.get_fullname().into());
                    let model = property(&info, "md").unwrap_or_else(|| "Unknown".into());
                    let capability = DeviceCapability::from_mdns(property(&info, "ca").as_deref());
                    found.insert(
                        (address, info.get_port()),
                        CastService {
                            name,
                            model,
                            capability,
                            address,
                            port: info.get_port(),
                        },
                    );
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let _ = daemon.stop_browse(CAST_SERVICE);
    let _ = daemon.shutdown();
    Ok(found.into_values().collect())
}

fn property(info: &mdns_sd::ResolvedService, key: &str) -> Option<String> {
    info.get_property_val_str(key).map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::DeviceCapability;

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
    fn handles_missing_or_unrecognised_capabilities() {
        assert_eq!(DeviceCapability::from_mdns(None), DeviceCapability::Unknown);
        assert_eq!(
            DeviceCapability::from_mdns(Some("not-a-number")),
            DeviceCapability::Unknown
        );
    }
}
