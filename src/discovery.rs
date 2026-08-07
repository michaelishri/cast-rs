use std::{
    collections::BTreeMap,
    net::IpAddr,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};

const CAST_SERVICE: &str = "_googlecast._tcp.local.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastService {
    pub name: String,
    pub model: String,
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
                    found.insert(
                        (address, info.get_port()),
                        CastService {
                            name,
                            model,
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
